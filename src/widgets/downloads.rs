//! Downloads: the files your browser is still writing into the Downloads folder, with their growing size and speed. S = total speed and
//! the fastest file, M = up to 3 files as rows.
//! Read-only and narrow on purpose: only the user's Downloads folder (the known-folder path, no subfolders, no links), at most 200
//! entries per scan, only names that look like unfinished downloads (`.crdownload`, `.part`, `.download`, `.opdownload`, `.partial`, and
//! a `.tmp` only while it keeps growing); file CONTENTS are never opened, only the size is read. The speed comes from the size
//! difference between two scans (2 s apart, a worker does the scan while the card is visible). A browser does not tell the total size,
//! so there is no percentage.
use super::sysmon::{fmt_rate_parts, fmt_size};
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "downloads",
        name: ("Загрузки", "Downloads"),
        desc: ("Скачиваемые сейчас файлы: размер и скорость", "Files being downloaded now: size and speed"),
        group: "system",
        icon: Glyph::NetDown,
        sizes: &[Size::S, Size::M],
        default_size: Size::S,
        make: || Box::new(Downloads::default()),
    });
}

const MAX_ENTRIES: usize = 200;
const SHOWN: usize = 3;

// ================================================================ pure rules

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// a browser's own partial-file suffix: always an unfinished download
    Partial,
    /// `.tmp`: only counts while its size keeps growing
    Tmp,
}

/// The name a browser shows for an unfinished file ("setup.exe.crdownload" -> "setup.exe") and what kind of marker it carries.
fn partial_name(file: &str) -> Option<(String, Kind)> {
    let (stem, ext) = file.rsplit_once('.')?;
    let kind = match ext.to_ascii_lowercase().as_str() {
        "crdownload" | "part" | "download" | "opdownload" | "partial" => Kind::Partial,
        "tmp" => Kind::Tmp,
        _ => return None,
    };
    let stem = stem.trim();
    (!stem.is_empty() && !stem.starts_with('.')).then(|| (stem.to_string(), kind))
}

/// One candidate file of a scan.
#[derive(Clone, Debug, PartialEq)]
struct Entry {
    /// the real file name (the key between two scans)
    file: String,
    size: u64,
}

/// A file shown on the card.
#[derive(Clone, Debug, PartialEq)]
struct Item {
    file: String,
    name: String,
    size: u64,
    /// bytes per second (smoothed)
    speed: f64,
}

/// Bytes per second from two sizes `dt` seconds apart; None for a shrunk file or a too short interval.
fn speed_bps(prev: u64, now: u64, dt: f64) -> Option<f64> {
    (dt >= 0.2 && now >= prev).then(|| (now - prev) as f64 / dt)
}

/// Half of the new reading, half of the old one: the number does not jump with every burst of the browser.
fn smooth(old: f64, new: f64) -> f64 {
    0.5 * old + 0.5 * new
}

/// The card's list after a scan `dt` seconds after the previous one: `.tmp` files only when they grew, speeds from the previous
/// list, fastest first (then by name).
fn update(prev: &[Item], scan: &[Entry], dt: f64) -> Vec<Item> {
    let mut out: Vec<Item> = scan
        .iter()
        .filter_map(|e| {
            let (name, kind) = partial_name(&e.file)?;
            let old = prev.iter().find(|p| p.file == e.file);
            if kind == Kind::Tmp && !old.is_some_and(|o| e.size > o.size) {
                return None;
            }
            let speed = match old {
                Some(o) => speed_bps(o.size, e.size, dt).map_or(o.speed, |s| smooth(o.speed, s)),
                None => 0.0,
            };
            Some(Item { file: e.file.clone(), name, size: e.size, speed })
        })
        .collect();
    out.sort_by(|a, b| b.speed.total_cmp(&a.speed).then_with(|| a.name.cmp(&b.name)));
    out
}

// ================================================================ the scan (worker thread)

fn downloads_dir() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Downloads, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Downloads, KF_FLAG_DEFAULT, None) {
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const _));
            if let Some(s) = s.filter(|s| !s.is_empty()) {
                return Some(PathBuf::from(s));
            }
        }
    }
    Some(PathBuf::from(std::env::var_os("USERPROFILE")?).join("Downloads"))
}

/// The candidate files of the Downloads folder: regular files only (no folders, no links), at most 200 entries looked at.
/// None = the folder cannot be read.
fn scan() -> Option<(Instant, Vec<Entry>)> {
    let rd = std::fs::read_dir(downloads_dir()?).ok()?;
    let at = Instant::now();
    let mut out = Vec::new();
    for e in rd.flatten().take(MAX_ENTRIES) {
        if !e.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        let file = e.file_name().to_string_lossy().into_owned();
        if partial_name(&file).is_some() {
            if let Ok(m) = e.metadata() {
                out.push(Entry { file, size: m.len() });
            }
        }
    }
    Some((at, out))
}

// ================================================================ widget

#[derive(Default)]
struct Downloads {
    items: Vec<Item>,
    /// when the scan behind `items` was taken
    at: Option<Instant>,
    /// the folder could not be read
    unreadable: bool,
    job: Option<Pending<Option<(Instant, Vec<Entry>)>>>,
    last: Option<Instant>,
    ver: u64,
    started: bool,
}

fn sample() -> Vec<Item> {
    const MB: u64 = 1 << 20;
    let it = |file: &str, name: &str, size: u64, speed: f64| Item { file: file.into(), name: name.into(), size, speed };
    vec![
        it("ubuntu-24.04-desktop-amd64.iso.crdownload", "ubuntu-24.04-desktop-amd64.iso", 1480 * MB, 11.8 * MB as f64),
        it("setup-1.2.exe.part", "setup-1.2.exe", 62 * MB, 2.4 * MB as f64),
        it("Отчёт за квартал.pdf.crdownload", "Отчёт за квартал.pdf", 3 * MB, 0.4 * MB as f64),
    ]
}

fn speed_text(cx: &Cx, bps: f64) -> String {
    let (n, u) = fmt_rate_parts(bps, cx.lang);
    format!("{n} {u}")
}

impl Downloads {
    fn init(&mut self, cx: &Cx) {
        if self.started {
            return;
        }
        self.started = true;
        self.ver += 1;
        if cx.preview {
            self.items = sample();
        }
    }
}

impl Widget for Downloads {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.init(cx);
        let c = cx.content();
        cx.text(Rect::new(c.x, c.y - 1.0, c.w - 24.0, 20.0), cx.t("Загрузки", "Downloads"), Style::TITLE.ellipsis(), accent());
        let Some(top) = self.items.first() else {
            let msg = if self.unreadable { cx.t("Папка недоступна", "Folder unavailable") } else { cx.t("Нет активных загрузок", "No active downloads") };
            cx.icon(Glyph::NetDown, Rect::new(c.cx() - 18.0, c.y + 40.0, 36.0, 36.0), FAINT);
            cx.text(Rect::new(c.x, c.y + 84.0, c.w, 20.0), msg, Style::BODY.center().ellipsis(), DIM);
            return Ok(());
        };
        let total: f64 = self.items.iter().map(|i| i.speed).sum();
        match cx.size {
            Size::M => {
                let (n, u) = fmt_rate_parts(total, cx.lang);
                cx.text(Rect::new(c.x + 80.0, c.y - 1.0, c.w - 80.0, 20.0), &format!("{n} {u}"), Style::new(12.0, 500).mono().right(), TEXT);
                for (i, it) in self.items.iter().take(SHOWN).enumerate() {
                    let row = Rect::new(c.x, c.y + 24.0 + i as f32 * 38.0, c.w, 36.0);
                    cx.text(Rect::new(row.x, row.y, row.w - 86.0, 18.0), &it.name, Style::BODY.ellipsis(), TEXT);
                    cx.text(Rect::new(row.right() - 84.0, row.y, 84.0, 18.0), &speed_text(cx, it.speed), Style::new(12.0, 500).mono().right().ellipsis(), accent());
                    cx.text(Rect::new(row.x, row.y + 18.0, row.w, 16.0), &fmt_size(it.size, cx.lang), Style::CAPTION.ellipsis(), DIM);
                }
                if self.items.len() > SHOWN {
                    cx.text(Rect::new(c.right() - 60.0, c.bottom() - 12.0, 60.0, 12.0), &format!("+{}", self.items.len() - SHOWN), Style::CAPTION.right().mono(), FAINT);
                }
            }
            _ => {
                let (n, u) = fmt_rate_parts(total, cx.lang);
                let big = Style::new(34.0, 300).mono();
                cx.text(Rect::new(c.x, c.y + 20.0, c.w, 42.0), &n, big, TEXT);
                let nw = cx.measure(&n, big, 0.0).0;
                cx.text(Rect::new(c.x + nw + 5.0, c.y + 40.0, (c.w - nw - 5.0).max(10.0), 16.0), u, Style::CAPTION.ellipsis(), DIM);
                let k = self.items.len() as u32;
                cx.text(Rect::new(c.x, c.y + 64.0, c.w, 16.0), &format!("{k} {}", plural(k, cx.t("файл", "file"), cx.t("файла", "files"), cx.t("файлов", "files"))), Style::CAPTION, DIM);
                cx.text(Rect::new(c.x, c.y + 88.0, c.w, 18.0), &top.name, Style::BODY.ellipsis(), TEXT);
                cx.text(Rect::new(c.x, c.y + 108.0, c.w, 16.0), &fmt_size(top.size, cx.lang), Style::CAPTION.ellipsis(), DIM);
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.init(cx);
        if cx.preview {
            return Tick::Never;
        }
        if let Some(j) = &self.job {
            match j.take() {
                Some(r) => {
                    self.job = None;
                    let (items, at, unreadable) = match r {
                        Some((at, scan)) => (update(&self.items, &scan, self.at.map_or(0.0, |p| at.saturating_duration_since(p).as_secs_f64())), Some(at), false),
                        None => (Vec::new(), None, true),
                    };
                    if items != self.items || unreadable != self.unreadable {
                        self.ver += 1;
                    }
                    (self.items, self.at, self.unreadable) = (items, at, unreadable);
                }
                None if !j.busy() => self.job = None,
                None => {}
            }
        }
        if self.job.is_none() && self.last.map_or(true, |t| t.elapsed() >= Duration::from_millis(1900)) {
            self.last = Some(Instant::now());
            self.job = Some(cx.spawn(scan));
        }
        Tick::Poll(Duration::from_secs(2))
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(file: &str, size: u64) -> Entry {
        Entry { file: file.into(), size }
    }
    fn item(file: &str, size: u64, speed: f64) -> Item {
        Item { file: file.into(), name: partial_name(file).unwrap().0, size, speed }
    }

    #[test]
    fn file_name_rules() {
        assert_eq!(partial_name("setup.exe.crdownload"), Some(("setup.exe".into(), Kind::Partial)));
        assert_eq!(partial_name("Movie.MKV.PART"), Some(("Movie.MKV".into(), Kind::Partial)));
        assert_eq!(partial_name("a.zip.download"), Some(("a.zip".into(), Kind::Partial)));
        assert_eq!(partial_name("a.zip.opdownload"), Some(("a.zip".into(), Kind::Partial)));
        assert_eq!(partial_name("a.zip.partial"), Some(("a.zip".into(), Kind::Partial)));
        assert_eq!(partial_name("Unconfirmed 123456.crdownload"), Some(("Unconfirmed 123456".into(), Kind::Partial)));
        assert_eq!(partial_name("x.TMP"), Some(("x".into(), Kind::Tmp)));
        for no in ["done.zip", "notes.txt", "crdownload", ".crdownload", ".part", " .tmp", "archive.parts", "a.crdownload.bak", ""] {
            assert_eq!(partial_name(no), None, "{no:?}");
        }
    }

    #[test]
    fn speed_math() {
        assert_eq!(speed_bps(1_000, 3_000, 2.0), Some(1_000.0));
        assert_eq!(speed_bps(5_000, 5_000, 2.0), Some(0.0)); // stalled
        assert_eq!(speed_bps(5_000, 4_000, 2.0), None); // a restarted file
        assert_eq!(speed_bps(0, 1_000, 0.0), None); // no time passed
        assert_eq!(smooth(100.0, 300.0), 200.0);
    }

    #[test]
    fn list_after_scans() {
        // first scan: a browser file is listed at once, a .tmp is not (nothing to compare)
        let first = update(&[], &[e("a.iso.crdownload", 1_000), e("t.tmp", 50), e("done.zip", 9)], 0.0);
        assert_eq!(first, vec![item("a.iso.crdownload", 1_000, 0.0)]);
        // 2 s later: a grew by 4000 B (2000 B/s, smoothed with 0 -> 1000), the .tmp grew and shows up, a new part file starts
        let second = update(&first, &[e("a.iso.crdownload", 5_000), e("t.tmp", 50), e("b.part", 10)], 2.0);
        assert_eq!(second, vec![item("a.iso.crdownload", 5_000, 1_000.0), item("b.part", 10, 0.0)]); // t.tmp is new to the previous list: it needs one more scan to prove it grows
        let third = update(&second, &[e("t.tmp", 100), e("a.iso.crdownload", 5_000)], 2.0);
        // t.tmp has no earlier entry in `second`, so it waits one more scan; a stalled a decays towards 0
        assert_eq!(third, vec![item("a.iso.crdownload", 5_000, 500.0)]);
        let grew = update(&[item("t.tmp", 50, 0.0)], &[e("t.tmp", 90)], 2.0);
        assert_eq!(grew.len(), 1);
        assert_eq!(grew[0].speed, 10.0); // 40 B over 2 s = 20 B/s, smoothed with 0
        // a finished file leaves the list
        assert!(update(&third, &[], 2.0).is_empty());
    }

    #[test]
    fn fastest_first() {
        let prev = vec![item("a.part", 0, 0.0), item("b.part", 0, 0.0), item("c.part", 0, 0.0)];
        let l = update(&prev, &[e("a.part", 100), e("b.part", 900), e("c.part", 100)], 1.0);
        assert_eq!(l.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(), vec!["b", "a", "c"]); // ties by name
    }

    /// Lists the real Downloads folder: `cargo test downloads_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn downloads_live() {
        println!("{:?}", downloads_dir());
        println!("{:?}", scan().map(|(_, v)| v));
    }
}
