//! Disks: S the system drive as a ring (used share) with the free space in the middle; M up to 3 drives as rows with a used bar;
//! L up to 6 drives with bars. Fixed drives plus removable ones that have media; network / optical drives are skipped.
//! Data: `GetLogicalDrives` / `GetDriveTypeW` / `GetDiskFreeSpaceExW` / `GetVolumeInformationW` on a worker thread every 30 s
//! (a sleeping HDD or a flaky card reader may block, so never on the UI thread). Everything is static between refreshes: `draw` + `version()`.
//! Colours (share used): green, amber from 80 %, red from 90 %.
use super::sysmon::{fmt_size, fmt_size_parts, level_color};
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::time::Instant;
use windows::core::PCWSTR;
use windows::Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives, GetVolumeInformationW};
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "disks",
        name: ("Диски", "Disks"),
        desc: ("Свободное место на дисках", "Free space on your drives"),
        group: "system",
        icon: Glyph::Disk,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::S,
        make: || Box::new(Disks::default()),
    });
}

#[derive(Clone, Debug, PartialEq)]
struct Drive {
    letter: char,
    label: String,
    total: u64,
    free: u64,
    removable: bool,
}
impl Drive {
    fn used_pct(&self) -> f32 {
        if self.total == 0 {
            0.0
        } else {
            (self.total.saturating_sub(self.free) as f64 / self.total as f64 * 100.0) as f32
        }
    }
    fn color(&self) -> Color {
        level_color(self.used_pct(), 80.0, 90.0)
    }
    /// "Windows (C:)" / "Локальный диск (D:)"
    fn name(&self, lang: Lang) -> String {
        let l = if self.label.is_empty() { lang.pick(if self.removable { "Съёмный диск" } else { "Локальный диск" }, if self.removable { "Removable Disk" } else { "Local Disk" }) } else { &self.label };
        format!("{l} ({}:)", self.letter)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Lists the drives (blocking calls: worker thread only). System drive first, then by letter.
fn enumerate() -> Vec<Drive> {
    let mut out = Vec::new();
    unsafe {
        let mut win = [0u16; 260];
        let n = GetWindowsDirectoryW(Some(&mut win)) as usize;
        let sys = win.get(..n).and_then(|w| char::decode_utf16(w.iter().copied()).next()).and_then(|c| c.ok()).map(|c| c.to_ascii_uppercase());
        let mask = GetLogicalDrives();
        for i in 0..26u32 {
            if mask & (1 << i) == 0 {
                continue;
            }
            let letter = (b'A' + i as u8) as char;
            let root = wide(&format!("{letter}:\\"));
            let root = PCWSTR(root.as_ptr());
            let kind = GetDriveTypeW(root);
            if kind != 3 && !(kind == 2 && letter > 'B') {
                continue; // DRIVE_FIXED, or DRIVE_REMOVABLE that is not a floppy letter
            }
            let (mut free, mut total) = (0u64, 0u64);
            if GetDiskFreeSpaceExW(root, None, Some(&mut total), Some(&mut free)).is_err() || total == 0 {
                continue; // no media
            }
            let mut buf = [0u16; 64];
            let label = if GetVolumeInformationW(root, Some(&mut buf), None, None, None, None).is_ok() {
                let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
                String::from_utf16_lossy(&buf[..end])
            } else {
                String::new()
            };
            out.push(Drive { letter, label, total, free, removable: kind == 2 });
        }
        out.sort_by_key(|d| (Some(d.letter) != sys, d.letter));
    }
    out
}

fn sample() -> Vec<Drive> {
    const GB: u64 = 1 << 30;
    const TB: u64 = 1 << 40;
    let d = |letter, label: &str, total, free, removable| Drive { letter, label: label.into(), total, free, removable };
    vec![
        d('C', "Windows", 931 * GB, 412 * GB, false),
        d('D', "Games", 2 * TB - 180 * GB, 340 * GB, false),
        d('E', "Backup", 4 * TB - 360 * GB, 3 * TB + 100 * GB, false),
        d('F', "Projects", 465 * GB, 41 * GB, false),
        d('G', "Media", 3 * TB - 270 * GB, 1 * TB + 200 * GB, false),
        d('H', "USB-STICK", 58 * GB, 31 * GB, true),
    ]
}

#[derive(Default)]
struct Disks {
    drives: Vec<Drive>,
    loaded: bool,
    job: Option<Pending<Vec<Drive>>>,
    last: Option<Instant>,
    ver: u64,
}

impl Disks {
    fn ensure_preview(&mut self, cx: &Cx) {
        if cx.preview && !self.loaded {
            self.loaded = true;
            self.drives = sample();
            self.ver += 1;
        }
    }
}

fn disk_icon_color(d: &Drive) -> Color {
    if d.removable {
        TEAL
    } else {
        DIM
    }
}

impl Widget for Disks {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        self.ensure_preview(cx);
        let lang = cx.lang;
        let free_word = cx.t("свободно", "free");
        let c = cx.content();
        if self.drives.is_empty() {
            let (head, rest) = c.cut_top(18.0);
            if cx.size == Size::L {
                cx.text(head, &cx.t("ДИСКИ", "DISKS"), Style::LABEL, ACCENT);
            } else {
                cx.text(head, cx.t("Диски", "Disks"), Style::TITLE, DIM);
            }
            cx.icon(Glyph::Disk, rest.centered(36.0, 36.0), FAINT);
            cx.text(rest.cut_bottom(18.0).1, if self.loaded { cx.t("Нет дисков", "No drives") } else { cx.t("Загрузка…", "Loading…") }, Style::CAPTION.center(), DIM);
            return Ok(());
        }
        match cx.size {
            Size::S => {
                let d = &self.drives[0];
                cx.text(Rect::new(c.x, c.y, c.w, 18.0), &d.name(lang), Style::TITLE.ellipsis(), DIM);
                let center = (c.cx(), c.y + 22.0 + 50.0);
                cx.ring(center, 45.0, 9.0, d.used_pct() / 100.0, d.color());
                let (n, u) = fmt_size_parts(d.free, lang);
                cx.text(Rect::new(center.0 - 40.0, center.1 - 26.0, 80.0, 34.0), &n, Style::new(30.0, 300).mono().center().ellipsis(), TEXT);
                cx.text(Rect::new(center.0 - 40.0, center.1 + 7.0, 80.0, 16.0), &format!("{u} {free_word}"), Style::CAPTION.center().ellipsis(), DIM);
            }
            Size::M => {
                let n = self.drives.len().min(3);
                let row_h = 46.0;
                let top = c.y + (c.h - row_h * n as f32) / 2.0;
                for (i, d) in self.drives.iter().take(n).enumerate() {
                    let r = Rect::new(c.x, top + i as f32 * row_h, c.w, row_h);
                    drive_row(cx, r, d, false);
                }
            }
            Size::L => {
                cx.text(Rect::new(c.x, c.y, c.w, 18.0), &cx.t("ДИСКИ", "DISKS"), Style::LABEL, ACCENT);
                let top = c.y + 28.0;
                for (i, d) in self.drives.iter().take(6).enumerate() {
                    let r = Rect::new(c.x, top + i as f32 * 49.0, c.w, 49.0);
                    if i > 0 {
                        cx.rect(Rect::new(r.x, r.y, r.w, 0.6), FILL);
                    }
                    drive_row(cx, r, d, true);
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if cx.preview {
            self.ensure_preview(cx);
            return Tick::Never;
        }
        if let Some(j) = &self.job {
            match j.take() {
                Some(list) => {
                    self.job = None;
                    if !self.loaded || list != self.drives {
                        self.drives = list;
                        self.loaded = true;
                        self.ver += 1;
                    }
                }
                None if !j.busy() => self.job = None, // the worker died, try again below
                None => {}
            }
        }
        if self.job.is_none() && self.last.map_or(true, |t| t.elapsed() >= Duration::from_secs(29)) {
            self.last = Some(Instant::now());
            self.job = Some(cx.spawn(enumerate));
        }
        Tick::Poll(Duration::from_secs(30))
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

/// One drive row (M compact / L with a caption line): icon, name, used bar, free-of-total text.
fn drive_row(cx: &Cx, r: Rect, d: &Drive, tall: bool) {
    let lang = cx.lang;
    let free = format!("{} {} {} {}", fmt_size(d.free, lang), cx.t("свободно", "free"), cx.t("из", "of"), fmt_size(d.total, lang));
    let ix = if tall { 24.0 } else { 20.0 };
    cx.icon(Glyph::Disk, Rect::new(r.x, r.y + if tall { 8.0 } else { 3.0 }, ix - 4.0, ix - 4.0).inset(-1.0), disk_icon_color(d));
    let x = r.x + ix + 8.0;
    let w = r.right() - x;
    if tall {
        cx.text(Rect::new(x, r.y + 5.0, w - 50.0, 18.0), &d.name(lang), Style::BODY.ellipsis(), TEXT);
        cx.text(Rect::new(r.right() - 50.0, r.y + 5.0, 50.0, 18.0), &format!("{}%", d.used_pct().round() as i32), Style::MONO.right(), DIM);
        cx.bar(Rect::new(x, r.y + 27.0, w, 6.0), d.used_pct() / 100.0, d.color());
        cx.text(Rect::new(x, r.y + 33.0, w, 14.0), &free, Style::CAPTION.ellipsis(), DIM);
    } else {
        cx.text(Rect::new(x, r.y, w * 0.5, 18.0), &d.name(lang), Style::BODY.ellipsis(), TEXT);
        cx.text(Rect::new(x + w * 0.5, r.y, w * 0.5, 18.0), &free, Style::CAPTION.right().ellipsis(), DIM);
        cx.bar(Rect::new(x, r.y + 23.0, w, 6.0), d.used_pct() / 100.0, d.color());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn used_share_and_names() {
        let d = Drive { letter: 'C', label: String::new(), total: 1000, free: 250, removable: false };
        assert_eq!(d.used_pct(), 75.0);
        assert_eq!(d.name(Lang::En), "Local Disk (C:)");
        assert_eq!(Drive { total: 0, ..d.clone() }.used_pct(), 0.0);
        assert_eq!(Drive { label: "Data".into(), letter: 'D', ..d.clone() }.name(Lang::Ru), "Data (D:)");
        assert_eq!(Drive { free: 50, ..d.clone() }.color(), RED);
        assert_eq!(Drive { free: 150, ..d }.color(), ORANGE);
    }

    /// `cargo test disks_live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn disks_live() {
        let t = Instant::now();
        let v = enumerate();
        println!("{} drives in {:?}", v.len(), t.elapsed());
        for d in &v {
            println!("  {} free {} of {} ({:.0}% used){}", d.name(Lang::En), fmt_size(d.free, Lang::En), fmt_size(d.total, Lang::En), d.used_pct(), if d.removable { " removable" } else { "" });
        }
        assert!(!v.is_empty());
    }
}
