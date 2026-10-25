//! Photos: a slideshow of the images of one folder, "cover" fit, filling the whole card (rounded like the glass), no padding.
//! Settings: `cfg.folder` (default `%USERPROFILE%\Pictures`, scanned non-recursively for jpg/jpeg/png/bmp/webp/gif), `cfg.interval_s`
//! (seconds per photo, default 30, 5..3600). The order is a shuffle seeded by the day. Click = next photo, right click = choose the folder
//! (`prompt_text`). The file name + a bottom gradient appear only on hover (overlay). The 1.2 s crossfade is an overlay too (30 fps only
//! while it runs; `Tick::Poll` otherwise). Photos are decoded on workers (WIC, <= 720 px) and at most TWO bitmaps are alive (current + next).
//! Preview: generated gradient "landscapes" (picked by the day), no I/O. Test hooks (previews only): `cfg.preview_hover` = true,
//! `cfg.preview_fade` = 0..1 (a frozen crossfade).
use crate::gfx::rr;
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::collections::HashSet;
use std::path::Path;
use std::time::Instant;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::{D2D1_BITMAP_BRUSH_PROPERTIES1, D2D1_BRUSH_PROPERTIES, D2D1_EXTEND_MODE_CLAMP, D2D1_INTERPOLATION_MODE_LINEAR};

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "photos",
        name: ("Фото", "Photos"),
        desc: ("Слайд-шоу из ваших фотографий", "A slideshow of your photos"),
        group: "photos",
        icon: Glyph::Image,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Photos::default()),
    });
}

const FADE_MS: f32 = 1200.0;
const MAX_PX: u32 = 720;

// ================================================================ pure logic (unit-tested)

fn is_image(name: &str) -> bool {
    let ext = Path::new(name).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("jpg" | "jpeg" | "png" | "bmp" | "webp" | "gif"))
}

/// Deterministic Fisher-Yates (xorshift64*) so a given day always gives the same order.
fn shuffle<T>(v: &mut [T], seed: u64) {
    let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for i in (1..v.len()).rev() {
        s ^= s >> 12;
        s ^= s << 25;
        s ^= s >> 27;
        let r = s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 33;
        v.swap(i, (r % (i as u64 + 1)) as usize);
    }
}

/// Worker: image files directly inside `dir`, sorted then shuffled by the day.
fn scan_folder(dir: &str, day: u32) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<String> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path().to_string_lossy().into_owned())
        .filter(|p| is_image(p))
        .take(5000)
        .collect();
    v.sort();
    shuffle(&mut v, day as u64);
    v
}

fn default_folder() -> String {
    std::env::var("USERPROFILE").map(|p| format!(r"{p}\Pictures")).unwrap_or_default()
}

fn interval_secs(v: Option<f64>) -> u64 {
    v.filter(|x| x.is_finite()).map(|x| x.round().clamp(5.0, 3600.0) as u64).unwrap_or(30)
}

/// Crossfade progress 0..1 from elapsed milliseconds (smoothstep).
fn ease(elapsed_ms: f32) -> f32 {
    let t = (elapsed_ms / FADE_MS).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// First index at or after `from` (wrapping) that is not in `bad`.
fn next_good(n: usize, from: usize, bad: &HashSet<usize>) -> Option<usize> {
    (0..n).map(|k| (from + k) % n.max(1)).find(|i| !bad.contains(i)).filter(|_| n > 0)
}

fn stem(path: &str) -> String {
    Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or(path).to_string()
}

const SAMPLE_PALETTES: usize = 4;
const SAMPLE_NAMES: [&str; SAMPLE_PALETTES] = ["Закат у озера", "Морской бриз", "Утро в лесу", "Сумерки в горах"];

/// Which generated sample the preview shows for a given day key, and the one after it (for the crossfade test).
fn sample_pair(day_key: u32) -> (usize, usize) {
    let a = day_key as usize % SAMPLE_PALETTES;
    (a, (a + 1) % SAMPLE_PALETTES)
}

/// A generated "landscape": sky gradient, a sun with a glow and three layers of hills. Premultiplied (opaque) BGRA.
fn sample_photo(pal: usize, w: u32, h: u32) -> Decoded {
    // (sky top, sky bottom, sun, [hill colours far..near])
    type P = ([f32; 3], [f32; 3], [f32; 3], [[f32; 3]; 3]);
    let p: P = match pal % SAMPLE_PALETTES {
        0 => ([0.25, 0.20, 0.45], [1.00, 0.62, 0.35], [1.00, 0.93, 0.70], [[0.55, 0.30, 0.42], [0.35, 0.18, 0.36], [0.18, 0.10, 0.24]]),
        1 => ([0.10, 0.35, 0.70], [0.65, 0.88, 0.98], [1.00, 1.00, 0.92], [[0.30, 0.55, 0.72], [0.15, 0.40, 0.58], [0.07, 0.25, 0.42]]),
        2 => ([0.20, 0.42, 0.40], [0.98, 0.90, 0.62], [1.00, 0.97, 0.80], [[0.36, 0.58, 0.40], [0.20, 0.42, 0.30], [0.08, 0.26, 0.20]]),
        _ => ([0.10, 0.08, 0.28], [0.80, 0.45, 0.62], [0.98, 0.85, 0.85], [[0.42, 0.28, 0.52], [0.26, 0.17, 0.40], [0.12, 0.09, 0.25]]),
    };
    let (wf, hf) = (w as f32, h as f32);
    let (sx, sy, sr) = (wf * 0.68, hf * 0.50, hf * 0.09);
    let mut bgra = vec![0u8; (w * h * 4) as usize];
    let mix = |a: [f32; 3], b: [f32; 3], t: f32| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t];
    for y in 0..h {
        for x in 0..w {
            let (fx, fy) = (x as f32 / wf, y as f32 / hf);
            let mut c = mix(p.0, p.1, (fy / 0.75).min(1.0).powf(1.3));
            let d = ((x as f32 - sx).powi(2) + (y as f32 - sy).powi(2)).sqrt();
            let glow = (-(d / (sr * 4.0)).powi(2)).exp() * 0.55;
            c = mix(c, p.2, glow);
            if d < sr {
                c = mix(c, p.2, ((sr - d) / 2.0).clamp(0.0, 1.0));
            }
            // hills: three sine layers, nearer = lower and darker
            for (k, hill) in p.3.iter().enumerate() {
                let kf = k as f32;
                let base = 0.60 + 0.12 * kf;
                let top = base + 0.05 * (fx * (3.0 + kf * 1.7) * std::f32::consts::TAU + kf * 2.0).sin() + 0.025 * (fx * 11.0 + kf).sin();
                if fy > top {
                    let shade = ((fy - top) * 2.2).min(0.35);
                    c = mix(*hill, [0.0, 0.0, 0.0], shade * 0.5);
                }
            }
            // soft vignette
            let v = 1.0 - 0.18 * (((fx - 0.5).powi(2) + (fy - 0.5).powi(2)) * 2.0);
            let o = ((y * w + x) * 4) as usize;
            bgra[o] = (c[2] * v * 255.0).clamp(0.0, 255.0) as u8;
            bgra[o + 1] = (c[1] * v * 255.0).clamp(0.0, 255.0) as u8;
            bgra[o + 2] = (c[0] * v * 255.0).clamp(0.0, 255.0) as u8;
            bgra[o + 3] = 255;
        }
    }
    Decoded { w, h, bgra }
}

// ================================================================ drawing helpers

/// Cover-fit `bmp` into `r` with rounded corners and an opacity (the core `bitmap_round` has no opacity).
fn paint_cover(cx: &Cx, r: Rect, bmp: &Bitmap, rad: f32, opacity: f32) {
    unsafe {
        let sz = bmp.GetPixelSize();
        let (bw, bh) = (sz.width.max(1) as f32, sz.height.max(1) as f32);
        let k = (r.w / bw).max(r.h / bh);
        let bp = D2D1_BITMAP_BRUSH_PROPERTIES1 { extendModeX: D2D1_EXTEND_MODE_CLAMP, extendModeY: D2D1_EXTEND_MODE_CLAMP, interpolationMode: D2D1_INTERPOLATION_MODE_LINEAR };
        let props = D2D1_BRUSH_PROPERTIES { opacity, transform: Matrix3x2 { M11: k, M12: 0.0, M21: 0.0, M22: k, M31: r.cx() - bw * k / 2.0, M32: r.cy() - bh * k / 2.0 } };
        let Ok(b) = cx.dc.CreateBitmapBrush(bmp, Some(&bp), Some(&props)) else { return };
        cx.dc.FillRoundedRectangle(&rr(r.x, r.y, r.w, r.h, rad), &b);
    }
}

/// A faint inner rim so the photo still reads as a glass card.
fn rim(cx: &Cx, rad: f32) {
    cx.rrect_stroke(cx.bounds().inset(0.5), rad - 0.5, 1.0, WHITE.alpha(0.22));
}

// ================================================================ widget

struct Photo {
    bmp: Bitmap,
    name: String,
}

#[derive(Default)]
struct Photos {
    started: bool,
    folder: String,
    interval: u64,
    /// bumped when the folder changes, so results of jobs from the old folder are ignored
    epoch: u32,
    files: Vec<String>,
    bad: HashSet<usize>,
    scan: Option<Pending<(u32, Vec<String>)>>,
    scanned: bool,
    load: Option<Pending<(u32, usize, Option<Decoded>)>>,
    cur: Option<Photo>,
    cur_idx: Option<usize>,
    next: Option<(usize, Photo)>,
    fade: Option<Instant>,
    due: Option<Instant>,
    want_next: bool,
    hover: bool,
    prompt: Option<Pending<Option<String>>>,
    /// a folder chosen in `tick` (which cannot write the config): flushed from `mouse` / `draw`
    cfg_folder: Option<String>,
    ver: u64,
}

fn poll<T>(slot: &mut Option<Pending<T>>) -> Option<T> {
    let p = slot.as_ref()?;
    let busy = p.busy();
    match p.take() {
        Some(v) => {
            *slot = None;
            Some(v)
        }
        None => {
            if !busy {
                *slot = None;
            }
            None
        }
    }
}

impl Photos {
    fn sync_cfg(&mut self, cx: &mut Cx) -> bool {
        match self.cfg_folder.take() {
            Some(f) => {
                cx.cfg_set("folder", json!(f));
                true
            }
            None => false,
        }
    }

    fn start_scan(&mut self, cx: &Cx) {
        self.epoch += 1;
        let (epoch, dir, day) = (self.epoch, self.folder.clone(), cx.now.day_key());
        self.files.clear();
        self.bad.clear();
        self.cur = None;
        self.cur_idx = None;
        self.next = None;
        self.fade = None;
        self.load = None;
        self.want_next = false;
        self.scanned = false;
        self.scan = Some(cx.spawn(move || (epoch, scan_folder(&dir, day))));
        self.ver += 1;
    }

    /// Index of the file to decode next: the first one while nothing is shown, else the one after the current photo.
    fn want_load(&self) -> Option<usize> {
        if self.load.is_some() || self.files.is_empty() {
            return None;
        }
        match (self.cur_idx, &self.next) {
            (None, _) if self.cur.is_none() => next_good(self.files.len(), 0, &self.bad),
            (Some(c), None) => next_good(self.files.len(), c + 1, &self.bad).filter(|&i| i != c),
            _ => None,
        }
    }

    fn empty_state(&self, cx: &Cx) {
        let loading = self.scan.is_some() || (!self.files.is_empty() && self.bad.len() < self.files.len());
        let c = cx.bounds();
        cx.icon(Glyph::Image, c.centered(40.0, 40.0).offset(0.0, -14.0), FAINT);
        let msg = if loading { cx.t("Загрузка…", "Loading…") } else { cx.t("Нет фото в папке", "No photos in the folder") };
        cx.text(Rect::new(c.x + 12.0, c.cy() + 10.0, c.w - 24.0, 20.0), msg, Style::BODY.center().ellipsis(), DIM);
        if !loading && cx.size != Size::S {
            cx.text(Rect::new(c.x + 12.0, c.cy() + 30.0, c.w - 24.0, 16.0), cx.t("Правый клик — выбрать папку", "Right click to pick a folder"), Style::CAPTION.center().ellipsis(), FAINT);
        }
    }
}

impl Widget for Photos {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        if self.sync_cfg(cx) {
            cx.waker().wake(); // let the framework persist the config now
        }
        let rad = cx.size.radius();
        match &self.cur {
            Some(p) => {
                paint_cover(cx, cx.bounds(), &p.bmp, rad, 1.0);
                rim(cx, rad);
            }
            None => self.empty_state(cx),
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        let rad = cx.size.radius();
        let (w, h) = (cx.w, cx.h);
        let preview_fade = if cx.preview { cx.cfg_f64("preview_fade").map(|v| v as f32) } else { None };
        let prog = match (self.fade, preview_fade) {
            (_, Some(p)) => Some(p.clamp(0.0, 1.0)),
            (Some(t0), _) => Some(ease(t0.elapsed().as_secs_f32() * 1000.0)),
            _ => None,
        };
        let mut shown = self.cur.as_ref().map(|p| p.name.clone());
        if let (Some(a), Some((_, nx))) = (prog, &self.next) {
            paint_cover(cx, cx.bounds(), &nx.bmp, rad, a);
            if a > 0.5 {
                shown = Some(nx.name.clone());
            }
            rim(cx, rad);
        }
        let hover = self.hover || (cx.preview && cx.cfg_bool("preview_hover") == Some(true));
        if hover && self.cur.is_some() {
            let gh = if cx.size == Size::S { 70.0 } else { 84.0 };
            cx.gradient_rrect(Rect::new(0.0, h - gh, w, gh), rad, BLACK.alpha(0.0), BLACK.alpha(0.62), 180.0);
            if let Some(n) = shown {
                cx.text(Rect::new(16.0, h - 16.0 - 20.0, w - 32.0, 20.0), &n, Style::new(13.0, 600).ellipsis(), WHITE);
            }
            rim(cx, rad);
        }
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            self.interval = interval_secs(cx.cfg_f64("interval_s"));
            if cx.preview {
                let (a, b) = sample_pair(cx.now.day_key());
                let mk = |i: usize| cx.image(&sample_photo(i, 640, 400)).ok().map(|bmp| Photo { bmp, name: SAMPLE_NAMES[i].to_string() });
                self.cur = mk(a);
                self.next = mk(b).map(|p| (1, p));
                self.ver += 1;
                return Tick::Never;
            }
            self.folder = cx.cfg_str("folder").map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).unwrap_or_else(default_folder);
            self.start_scan(cx);
        }
        if cx.preview {
            return Tick::Never;
        }
        self.interval = interval_secs(cx.cfg_f64("interval_s"));

        // a folder typed into the prompt
        if let Some(Some(f)) = poll(&mut self.prompt) {
            let f = f.trim().trim_matches('"').to_string();
            if !f.is_empty() && f != self.folder {
                self.folder = f.clone();
                self.cfg_folder = Some(f);
                self.start_scan(cx);
            }
        }
        if let Some((epoch, files)) = poll(&mut self.scan) {
            if epoch == self.epoch {
                self.files = files;
                self.scanned = true;
                self.ver += 1;
            }
        }
        if let Some((epoch, i, d)) = poll(&mut self.load) {
            if epoch == self.epoch {
                match d.and_then(|d| cx.image(&d).ok()) {
                    Some(bmp) => {
                        let photo = Photo { bmp, name: stem(self.files.get(i).map(String::as_str).unwrap_or("")) };
                        if self.cur.is_none() {
                            self.cur = Some(photo);
                            self.cur_idx = Some(i);
                            self.due = Some(Instant::now() + std::time::Duration::from_secs(self.interval));
                        } else {
                            self.next = Some((i, photo));
                        }
                    }
                    None => {
                        self.bad.insert(i);
                    }
                }
                self.ver += 1;
            }
        }

        // crossfade: start when due (or on click) and the next photo is decoded; finish after 1.2 s
        let now = Instant::now();
        if self.files.len() < 2 {
            self.want_next = false; // nothing to advance to
        }
        if self.fade.is_none() && self.cur.is_some() && self.next.is_some() && (self.want_next || self.due.is_some_and(|d| now >= d)) {
            self.fade = Some(now);
            self.want_next = false;
        }
        if self.fade.is_some_and(|t0| t0.elapsed().as_millis() as f32 >= FADE_MS) {
            if let Some((i, p)) = self.next.take() {
                self.cur = Some(p);
                self.cur_idx = Some(i);
            }
            self.fade = None;
            self.due = Some(now + std::time::Duration::from_secs(self.interval));
            self.ver += 1;
        }

        // decode the next photo ahead of time (at most cur + next are alive, one decode in flight)
        if let Some(i) = self.want_load() {
            if let Some(path) = self.files.get(i).cloned() {
                let epoch = self.epoch;
                self.load = Some(cx.spawn(move || (epoch, i, decode_image_file(&path, MAX_PX, MAX_PX))));
            }
        }

        if self.fade.is_some() {
            Tick::Fps(30)
        } else if self.hover {
            Tick::Fps(4) // keeps the hover caption painted over a re-recorded photo
        } else if self.cur.is_some() && self.files.len() > 1 {
            let wait = self.due.map(|d| d.saturating_duration_since(now)).unwrap_or_default();
            Tick::Poll(wait.max(std::time::Duration::from_millis(250)).min(std::time::Duration::from_secs(60)))
        } else {
            Tick::Never
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        self.sync_cfg(cx);
        let hover = ev.kind != MouseKind::Leave;
        let changed = hover != self.hover;
        self.hover = hover;
        if ev.clicked(cx.bounds()) {
            self.want_next = true;
            cx.waker().wake();
        } else if ev.right_clicked(cx.bounds()) && self.prompt.is_none() {
            self.prompt = Some(cx.prompt_text(cx.t("Папка с фото", "Photos folder"), &self.folder, false));
        }
        changed
    }

    fn version(&self) -> u64 {
        self.ver
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_extensions() {
        for n in ["a.jpg", "B.JPEG", "c.png", "d.Bmp", "e.webp", "f.gif", r"C:\x\y.PNG"] {
            assert!(is_image(n), "{n}");
        }
        for n in ["a.txt", "b", "c.jpg.bak", "d.heic", ".png"] {
            assert!(!is_image(n), "{n}");
        }
    }

    #[test]
    fn shuffle_is_deterministic_permutation() {
        let base: Vec<u32> = (0..50).collect();
        let (mut a, mut b, mut c) = (base.clone(), base.clone(), base.clone());
        shuffle(&mut a, 20261024);
        shuffle(&mut b, 20261024);
        shuffle(&mut c, 20261025);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_ne!(a, base);
        let mut s = a.clone();
        s.sort();
        assert_eq!(s, base);
        let mut one = [1];
        shuffle(&mut one, 7);
        let mut none: [u8; 0] = [];
        shuffle(&mut none, 7);
    }

    #[test]
    fn interval_and_ease() {
        assert_eq!(interval_secs(None), 30);
        assert_eq!(interval_secs(Some(1.0)), 5);
        assert_eq!(interval_secs(Some(99999.0)), 3600);
        assert_eq!(interval_secs(Some(45.4)), 45);
        assert_eq!(interval_secs(Some(f64::NAN)), 30);
        assert_eq!(ease(0.0), 0.0);
        assert_eq!(ease(5000.0), 1.0);
        assert!((ease(600.0) - 0.5).abs() < 1e-6);
        assert!(ease(300.0) < ease(900.0));
    }

    #[test]
    fn next_good_skips_bad_and_wraps() {
        let bad: HashSet<usize> = [1, 2].into_iter().collect();
        assert_eq!(next_good(4, 1, &bad), Some(3));
        assert_eq!(next_good(4, 3, &bad), Some(3));
        assert_eq!(next_good(4, 4, &bad), Some(0));
        let all: HashSet<usize> = (0..3).collect();
        assert_eq!(next_good(3, 0, &all), None);
        assert_eq!(next_good(0, 0, &HashSet::new()), None);
    }

    #[test]
    fn sample_pickers() {
        for d in 20261001..20261100 {
            let (a, b) = sample_pair(d);
            assert!(a < SAMPLE_PALETTES && b < SAMPLE_PALETTES && a != b);
            assert_eq!(sample_pair(d), (a, b));
        }
        let img = sample_photo(2, 64, 40);
        assert_eq!((img.w, img.h, img.bgra.len()), (64, 40, 64 * 40 * 4));
        assert!(img.bgra.chunks_exact(4).all(|p| p[3] == 255));
        assert_ne!(sample_photo(0, 32, 20).bgra, sample_photo(1, 32, 20).bgra);
        assert_eq!(stem(r"C:\Pics\Закат у озера.jpg"), "Закат у озера");
    }

    #[test]
    fn missing_folder_is_empty() {
        assert!(scan_folder(r"C:\definitely\not\here", 20261024).is_empty());
    }
}
