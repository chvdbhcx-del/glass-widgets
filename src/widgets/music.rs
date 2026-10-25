//! Music (Now Playing) from the Windows System Media Transport Controls.
//! S: the cover fills the card, a dark glass pill at the bottom with title / artist and a play-pause button.
//! M: cover on the left, title + artist, a 24-bar equalizer (pseudo animation, not audio analysis) and prev / play-pause / next.
//! L: full-width cover on top, title + artist, a progress bar with elapsed / remaining time and the three controls.
//! Idle (no session): a dim note and "Ничего не играет". No cover: a soft gradient with a note glyph.
//! Data: one long-lived worker thread polls the current SMTC session every 1 s (title, artist, status, timeline, thumbnail bytes decoded with WIC
//! to <= 512 px once per track change) and wakes the UI only when something visible changed. The UI thread turns the pixels into a D2D bitmap.
//! The equalizer, the progress bar / times (L) and a title marquee are the overlay (30 fps, only while playing and visible); everything else is static.
//! Settings: `preview_state` = "playing" (default) | "paused" | "idle" selects the sample state in previews (`--cards "music:m@0,0{\"preview_state\":\"idle\"}"`).
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSession as Session, GlobalSystemMediaTransportControlsSessionManager as Manager,
    GlobalSystemMediaTransportControlsSessionMediaProperties as Props, GlobalSystemMediaTransportControlsSessionPlaybackStatus as Status,
};
use windows::Storage::Streams::DataReader;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "music",
        name: ("Музыка", "Music"),
        desc: ("Что сейчас играет и управление воспроизведением", "Now playing with playback controls"),
        group: "music",
        icon: Glyph::Note,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::M,
        make: || Box::new(Music::new()),
    });
}

// ================================================================ pure helpers (unit tested)

/// "m:ss" or "h:mm:ss".
fn fmt_clock(ms: i64) -> String {
    let s = (ms.max(0) / 1000) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Position now = the reported one + the time since the app last updated it (only while playing), clamped to the duration.
fn adjust_position(pos_ms: i64, lag_ms: i64, playing: bool, dur_ms: i64) -> i64 {
    let p = pos_ms + if playing { lag_ms.clamp(0, 12 * 3600 * 1000) } else { 0 };
    if dur_ms > 0 {
        p.clamp(0, dur_ms)
    } else {
        p.max(0)
    }
}

/// Horizontal title offset (<= 0) for a text `over` px wider than its box at wall time `t` seconds:
/// hold, scroll to the end, hold, scroll back.
fn marquee_off(over: f32, t: f64) -> f32 {
    let d = (over / 32.0 + 0.8) as f64;
    let hold = 1.6;
    let cyc = 2.0 * (hold + d);
    let p = t.rem_euclid(cyc);
    let ease = |x: f64| (x * x * (3.0 - 2.0 * x)) as f32;
    -over
        * if p < hold {
            0.0
        } else if p < hold + d {
            ease((p - hold) / d)
        } else if p < 2.0 * hold + d {
            1.0
        } else {
            1.0 - ease((p - 2.0 * hold - d) / d)
        }
}

fn hsl_to_color(h: f32, s: f32, l: f32) -> Color {
    let (h, c) = (h.rem_euclid(360.0) / 60.0, (1.0 - (2.0 * l - 1.0).abs()) * s);
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    Color::rgba(r + m, g + m, b + m, 1.0)
}

/// Accent colour of a cover (premultiplied BGRA): the picture is averaged into 8x8 cells, cells are weighted by saturation and mid lightness
/// (like the WebView version), the hue is kept and saturation / lightness are clamped to a readable accent. Grey covers give PINK.
fn accent_from(bgra: &[u8], w: u32, h: u32) -> Color {
    let (w, h) = (w as usize, h as usize);
    if w == 0 || h == 0 || bgra.len() < w * h * 4 {
        return PINK;
    }
    let mut sum = [[0f64; 4]; 64];
    for y in 0..h {
        for x in 0..w {
            let (cell, i) = ((y * 8 / h) * 8 + x * 8 / w, (y * w + x) * 4);
            let c = &mut sum[cell];
            c[0] += bgra[i + 2] as f64;
            c[1] += bgra[i + 1] as f64;
            c[2] += bgra[i] as f64;
            c[3] += 1.0;
        }
    }
    let (mut r, mut g, mut b, mut wt) = (0f64, 0f64, 0f64, 0f64);
    for c in sum.iter().filter(|c| c[3] > 0.0) {
        let (cr, cg, cb) = (c[0] / c[3], c[1] / c[3], c[2] / c[3]);
        let (mx, mn) = (cr.max(cg).max(cb), cr.min(cg).min(cb));
        let sat = (mx - mn) / if mx > 0.0 { mx } else { 1.0 };
        let lum = (mx + mn) / 510.0;
        let k = 0.02 + sat * (1.0 - (2.0 * lum - 1.0).abs());
        r += cr * k;
        g += cg * k;
        b += cb * k;
        wt += k;
    }
    let (r, g, b) = ((r / wt / 255.0) as f32, (g / wt / 255.0) as f32, (b / wt / 255.0) as f32);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let d = mx - mn;
    if d < 0.05 {
        return PINK;
    }
    let hue = 60.0 * if mx == r { ((g - b) / d) % 6.0 } else if mx == g { (b - r) / d + 2.0 } else { (r - g) / d + 4.0 };
    let den = 1.0 - (mx + mn - 1.0).abs();
    let s = (d / if den > 0.0 { den } else { 1.0 }).clamp(0.55, 0.90);
    hsl_to_color(hue, s, 0.56)
}

fn hash_hue(s: &str) -> f32 {
    let mut h: u32 = 2166136261;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(16777619);
    }
    (h % 360) as f32
}

// ================================================================ data from SMTC (worker side)

/// Decoded cover + its accent colour, produced on the worker, turned into a bitmap on the UI thread.
struct Cover {
    dec: Decoded,
    accent: Color,
}

#[derive(Clone)]
struct Snap {
    has: bool,
    playing: bool,
    title: String,
    artist: String,
    /// position at `stamp`
    pos_ms: i64,
    dur_ms: i64,
    stamp: Instant,
    /// changes whenever the cover changes (new track or thumbnail arrived)
    cover_id: u64,
    cover: Option<Arc<Cover>>,
}

impl Snap {
    fn idle() -> Snap {
        Snap { has: false, playing: false, title: String::new(), artist: String::new(), pos_ms: 0, dur_ms: 0, stamp: Instant::now(), cover_id: 0, cover: None }
    }
    /// Is `b` visibly different from `a` (worth waking the UI)? A position only counts when it deviates from the extrapolation (seek).
    fn differs(a: &Snap, b: &Snap) -> bool {
        if a.has != b.has || a.playing != b.playing || a.title != b.title || a.artist != b.artist || a.dur_ms != b.dur_ms || a.cover_id != b.cover_id {
            return true;
        }
        let expect = a.pos_ms + if a.playing { b.stamp.saturating_duration_since(a.stamp).as_millis() as i64 } else { 0 };
        (expect - b.pos_ms).abs() > 1500
    }
}

#[derive(Clone, Copy)]
enum Cmd {
    Prev,
    Toggle,
    Next,
}

#[derive(Default)]
struct Tracker {
    key: String,
    cover: Option<Arc<Cover>>,
    cover_id: u64,
}

fn load_cover(props: &Props) -> Option<Cover> {
    let stream = props.Thumbnail().ok()?.OpenReadAsync().ok()?.get().ok()?;
    let size = stream.Size().ok()? as u32;
    if size == 0 || size > 8 * 1024 * 1024 {
        return None;
    }
    let reader = DataReader::CreateDataReader(&stream).ok()?;
    reader.LoadAsync(size).ok()?.get().ok()?;
    let mut buf = vec![0u8; size as usize];
    reader.ReadBytes(&mut buf).ok()?;
    let dec = decode_image_bytes(&buf, 512, 512)?;
    let accent = accent_from(&dec.bgra, dec.w, dec.h);
    Some(Cover { dec, accent })
}

/// (position, duration) in ms from the timeline; the position is advanced by the time since the app updated it.
fn timeline(s: &Session, playing: bool) -> Option<(i64, i64)> {
    let tl = s.GetTimelineProperties().ok()?;
    let ms = |t: windows::Foundation::TimeSpan| t.Duration / 10_000;
    let (start, end, pos) = (ms(tl.StartTime().ok()?), ms(tl.EndTime().ok()?), ms(tl.Position().ok()?));
    let dur = (end - start).max(0);
    // LastUpdatedTime: 100 ns ticks since 1601-01-01 UTC
    let lag = tl
        .LastUpdatedTime()
        .ok()
        .map(|t| t.UniversalTime)
        .filter(|t| *t > 0)
        .and_then(|t| {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?;
            Some((now.as_nanos() as i64 / 100 + 116_444_736_000_000_000 - t) / 10_000)
        })
        .unwrap_or(0);
    Some((adjust_position((pos - start).max(0), lag, playing, dur), dur))
}

fn read(mgr: &Manager, tr: &mut Tracker) -> windows::core::Result<Snap> {
    let s = mgr.GetCurrentSession()?;
    let props = s.TryGetMediaPropertiesAsync()?.get()?;
    let playing = s.GetPlaybackInfo()?.PlaybackStatus()? == Status::Playing;
    let title = props.Title()?.to_string();
    let artist = props.Artist()?.to_string();
    let key = format!("{title}\u{1}{artist}");
    if tr.key != key {
        tr.key = key;
        tr.cover = None;
        tr.cover_id += 1;
    }
    if tr.cover.is_none() {
        if let Some(c) = load_cover(&props) {
            tr.cover = Some(Arc::new(c));
            tr.cover_id += 1;
        }
    }
    let (pos_ms, dur_ms) = timeline(&s, playing).unwrap_or((0, 0));
    Ok(Snap { has: true, playing, title, artist, pos_ms, dur_ms, stamp: Instant::now(), cover_id: tr.cover_id, cover: tr.cover.clone() })
}

fn control(mgr: &Manager, cmd: Cmd) {
    if let Ok(s) = mgr.GetCurrentSession() {
        let _ = match cmd {
            Cmd::Prev => s.TrySkipPreviousAsync().and_then(|o| o.get()),
            Cmd::Toggle => s.TryTogglePlayPauseAsync().and_then(|o| o.get()),
            Cmd::Next => s.TrySkipNextAsync().and_then(|o| o.get()),
        };
    }
}

/// Long-lived poller: ends by itself when the widget (and so the command sender) is dropped.
fn worker(rx: Receiver<Cmd>, shared: Arc<Mutex<Snap>>, waker: Waker) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut mgr: Option<Manager> = None;
    let mut tr = Tracker::default();
    loop {
        let cmd = match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        if mgr.is_none() {
            mgr = Manager::RequestAsync().ok().and_then(|o| o.get().ok());
        }
        if let (Some(c), Some(m)) = (cmd, &mgr) {
            control(m, c);
            std::thread::sleep(Duration::from_millis(150)); // let the player apply it before re-reading
        }
        let snap = mgr.as_ref().and_then(|m| read(m, &mut tr).ok()).unwrap_or_else(Snap::idle);
        let mut g = shared.lock().unwrap_or_else(|e| e.into_inner());
        if Snap::differs(&g, &snap) {
            *g = snap;
            drop(g);
            waker.wake();
        }
    }
}

// ================================================================ layout

/// Geometry per size (card px); the same numbers feed draw, overlay and hit testing.
struct Lay {
    cover: Rect,
    pill: Option<Rect>,
    title: Rect,
    /// title font size
    tsz: f32,
    artist: Rect,
    eq: Option<Rect>,
    prog: Option<Rect>,
    times: Option<(Rect, Rect)>,
    prev: Option<Rect>,
    pp: Rect,
    next: Option<Rect>,
}

fn lay(size: Size) -> Lay {
    match size {
        Size::S => Lay {
            cover: Rect::new(0.0, 0.0, 170.0, 170.0),
            pill: Some(Rect::new(8.0, 118.0, 154.0, 44.0)),
            title: Rect::new(22.0, 122.0, 96.0, 18.0),
            tsz: 13.0,
            artist: Rect::new(22.0, 140.0, 96.0, 15.0),
            eq: None,
            prog: None,
            times: None,
            prev: None,
            pp: Rect::new(124.0, 125.0, 30.0, 30.0),
            next: None,
        },
        Size::M => Lay {
            cover: Rect::new(16.0, 16.0, 138.0, 138.0),
            pill: None,
            title: Rect::new(170.0, 14.0, 170.0, 22.0),
            tsz: 15.0,
            artist: Rect::new(170.0, 36.0, 170.0, 18.0),
            eq: Some(Rect::new(170.0, 64.0, 170.0, 34.0)),
            prog: None,
            times: None,
            prev: Some(Rect::new(178.0, 114.0, 40.0, 40.0)),
            pp: Rect::new(234.0, 113.0, 42.0, 42.0),
            next: Some(Rect::new(292.0, 114.0, 40.0, 40.0)),
        },
        Size::L => Lay {
            cover: Rect::new(0.0, 0.0, 356.0, 200.0),
            pill: None,
            title: Rect::new(16.0, 209.0, 324.0, 24.0),
            tsz: 17.0,
            artist: Rect::new(16.0, 233.0, 324.0, 18.0),
            eq: None,
            prog: Some(Rect::new(16.0, 266.0, 324.0, 5.0)),
            times: Some((Rect::new(16.0, 275.0, 120.0, 14.0), Rect::new(220.0, 275.0, 120.0, 14.0))),
            prev: Some(Rect::new(98.0, 298.0, 40.0, 40.0)),
            pp: Rect::new(155.0, 295.0, 46.0, 46.0),
            next: Some(Rect::new(218.0, 298.0, 40.0, 40.0)),
        },
    }
}

// ================================================================ widget

struct Music {
    started: bool,
    preview: bool,
    ver: u64,
    shared: Arc<Mutex<Snap>>,
    tx: Option<Sender<Cmd>>,
    has: bool,
    playing: bool,
    title: String,
    artist: String,
    pos_ms: i64,
    pos_at: Instant,
    dur_ms: i64,
    cover_id: u64,
    cover: Option<Bitmap>,
    accent: Color,
    hue: f32,
    /// natural width of the title (for the marquee decision)
    title_w: f32,
    /// hovered control: 0 none, 1 prev, 2 play-pause, 3 next
    hov: u8,
    /// equalizer bars: (angular speed rad/s, phase, amplitude)
    bars: [(f64, f64, f64); 24],
}

impl Music {
    fn new() -> Music {
        let mut seed: u32 = 0x9e37_79b9;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            (seed >> 8) as f64 / (1u32 << 24) as f64
        };
        let mut bars = [(0.0, 0.0, 0.0); 24];
        for b in bars.iter_mut() {
            let period = 0.45 + rnd() * 0.7; // like the WebView version: 0.45 .. 1.15 s
            *b = (std::f64::consts::TAU / period, rnd() * std::f64::consts::TAU, 0.3 + rnd() * 0.7);
        }
        let shared = Arc::new(Mutex::new(Snap::idle()));
        Music {
            started: false,
            preview: false,
            ver: 0,
            shared,
            tx: None,
            has: false,
            playing: false,
            title: String::new(),
            artist: String::new(),
            pos_ms: 0,
            pos_at: Instant::now(),
            dur_ms: 0,
            cover_id: 0,
            cover: None,
            accent: PINK,
            hue: 330.0,
            title_w: 0.0,
            hov: 0,
            bars,
        }
    }

    fn marquee(&self, l: &Lay) -> bool {
        self.title_w > l.title.w + 0.5
    }

    fn pos_now(&self) -> i64 {
        let p = if self.playing && !self.preview { self.pos_ms + self.pos_at.elapsed().as_millis() as i64 } else { self.pos_ms };
        if self.dur_ms > 0 {
            p.clamp(0, self.dur_ms)
        } else {
            p.max(0)
        }
    }

    fn title_style(l: &Lay) -> Style {
        Style::new(l.tsz, 600)
    }

    fn set_title(&mut self, cx: &Cx, title: &str) {
        self.title = title.to_string();
        self.hue = hash_hue(&format!("{}{}", self.title, self.artist));
        self.title_w = cx.measure(&self.title, Self::title_style(&lay(cx.size)), 0.0).0;
    }

    /// Pull the worker's latest snapshot into the UI state; bump the version when something visible changed.
    fn sync(&mut self, cx: &Cx) {
        let s = self.shared.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let mut changed = false;
        if s.has != self.has || s.playing != self.playing || s.dur_ms != self.dur_ms {
            self.has = s.has;
            self.playing = s.playing;
            self.dur_ms = s.dur_ms;
            changed = true;
        }
        let name = if s.has && s.title.is_empty() { cx.t("Без названия", "Untitled").to_string() } else { s.title.clone() };
        if name != self.title || s.artist != self.artist {
            self.artist = s.artist.clone();
            self.set_title(cx, &name);
            changed = true;
        }
        self.pos_ms = s.pos_ms;
        self.pos_at = s.stamp;
        if s.cover_id != self.cover_id {
            self.cover_id = s.cover_id;
            match s.cover.as_ref().and_then(|c| cx.image(&c.dec).ok().map(|b| (b, c.accent))) {
                Some((b, a)) => {
                    self.cover = Some(b);
                    self.accent = a;
                }
                None => {
                    self.cover = None;
                    self.accent = hsl_to_color(self.hue, 0.7, 0.58);
                }
            }
            changed = true;
        } else if self.cover.is_none() {
            let a = hsl_to_color(self.hue, 0.7, 0.58);
            if a != self.accent {
                self.accent = a;
                changed = true;
            }
        }
        if changed {
            self.ver += 1;
        }
    }

    fn start_preview(&mut self, cx: &Cx) {
        self.preview = true;
        match cx.cfg_str("preview_state").unwrap_or("playing") {
            "idle" => {}
            st => {
                self.has = true;
                self.playing = st != "paused";
                self.artist = cx.t("Исполнитель", "Artist").to_string();
                self.set_title(cx, cx.cfg_str("preview_title").unwrap_or(cx.t("Название трека", "Track title")));
                self.pos_ms = 84_000;
                self.dur_ms = 216_000;
                if st == "nocover" {
                    self.accent = hsl_to_color(self.hue, 0.7, 0.58);
                } else {
                    let dec = sample_cover();
                    self.accent = accent_from(&dec.bgra, dec.w, dec.h);
                    self.cover = cx.image(&dec).ok();
                }
            }
        }
        self.ver += 1;
    }

    // ------------------------------------------------ drawing pieces

    /// Cover (or the no-cover gradient + note) into `r` with corner radius `rad`.
    fn paint_cover(&self, cx: &Cx, r: Rect, rad: f32) {
        if let Some(b) = &self.cover {
            cx.bitmap_round(r, b, Fit::Cover, rad);
        } else {
            cx.gradient_rrect(r, rad, hsl_to_color(self.hue, 0.55, 0.42), hsl_to_color(self.hue + 40.0, 0.6, 0.26), 150.0);
            let g = (r.w.min(r.h) * 0.36).min(72.0);
            cx.icon(Glyph::Note, Rect::new(r.cx() - g / 2.0, r.cy() - g / 2.0, g, g), WHITE.alpha(0.45));
        }
    }

    fn draw_cover(&self, cx: &Cx, l: &Lay) {
        match cx.size {
            Size::M => self.paint_cover(cx, l.cover, 18.0),
            _ => cx.with_clip_round(cx.bounds(), cx.size.radius(), |c| self.paint_cover(c, l.cover, 0.0)),
        }
    }

    fn small_btn(&self, cx: &Cx, r: Rect, g: Glyph, id: u8) {
        if self.hov == id {
            cx.circle(r.center(), r.w / 2.0, FILL);
        }
        cx.icon(g, r.centered(22.0, 22.0), TEXT);
    }

    fn controls(&self, cx: &Cx, l: &Lay) {
        if let Some(p) = l.prev {
            self.small_btn(cx, p, Glyph::Prev, 1);
        }
        if let Some(n) = l.next {
            self.small_btn(cx, n, Glyph::Next, 3);
        }
        let c = if self.hov == 2 { self.accent.mix(WHITE, 0.18) } else { self.accent };
        cx.circle(l.pp.center(), l.pp.w / 2.0, c);
        let g = l.pp.w * 0.54;
        let dx = if self.playing { 0.0 } else { 0.8 }; // optical centring of the triangle
        cx.icon(if self.playing { Glyph::Pause } else { Glyph::Play }, Rect::new(l.pp.cx() - g / 2.0 + dx, l.pp.cy() - g / 2.0, g, g), WHITE);
    }

    /// Equalizer: a frozen resting state (`live` = false, drawn in `draw`) or the animated one (overlay).
    fn eq_bars(&self, cx: &Cx, r: Rect, live: bool) {
        let n = self.bars.len() as f32;
        let bw = 4.0;
        let gap = (r.w - n * bw) / (n - 1.0);
        let t = cx.now.secs_f() as f64;
        for (i, &(w, ph, amp)) in self.bars.iter().enumerate() {
            let k = if live { (0.5 + 0.5 * (t * w + ph).sin()) * 0.75 + (0.5 + 0.5 * (t * w * 0.37 + ph * 1.7).sin()) * 0.25 } else { 0.16 };
            let h = 4.0 + (r.h - 4.0) * (k * amp) as f32;
            let x = r.x + i as f32 * (bw + gap);
            cx.pill(Rect::new(x, r.cy() - h / 2.0, bw, h), self.accent.fade(if live { 0.95 } else { 0.5 }));
        }
    }

    fn progress(&self, cx: &Cx, l: &Lay) {
        let (Some(r), Some((a, b))) = (l.prog, l.times) else { return };
        let pos = self.pos_now();
        cx.bar(r, if self.dur_ms > 0 { pos as f32 / self.dur_ms as f32 } else { 0.0 }, TEXT.fade(0.9));
        if self.dur_ms > 0 {
            cx.text(a, &fmt_clock(pos), Style::CAPTION.mono(), TEXT.fade(0.74));
            cx.text(b, &format!("−{}", fmt_clock(self.dur_ms - pos)), Style::CAPTION.mono().right(), TEXT.fade(0.74));
        }
    }

    fn title_text(&self, cx: &Cx, l: &Lay, off: f32) {
        let st = Self::title_style(l);
        if off == 0.0 {
            cx.text(l.title, &self.title, st.ellipsis(), TEXT);
        } else {
            cx.clip(l.title);
            cx.text(Rect::new(l.title.x + off, l.title.y, self.title_w + 4.0, l.title.h), &self.title, st, TEXT);
            cx.pop_clip();
        }
    }
}

/// 256x256 gradient "cover" for previews (deterministic, no I/O).
fn sample_cover() -> Decoded {
    let n = 256usize;
    let mut v = Vec::with_capacity(n * n * 4);
    let (c0, c1, c2) = ([255.0f32, 95.0, 109.0], [200.0f32, 80.0, 192.0], [65.0f32, 88.0, 208.0]);
    for y in 0..n {
        for x in 0..n {
            let t = (x + y) as f32 / (2.0 * (n - 1) as f32);
            let (a, b, k) = if t < 0.5 { (c0, c1, t * 2.0) } else { (c1, c2, (t - 0.5) * 2.0) };
            let (dx, dy) = (x as f32 / n as f32 - 0.3, y as f32 / n as f32 - 0.25);
            let glow = (-(dx * dx + dy * dy) * 9.0).exp() * 0.28;
            let mut px = [0u8; 4];
            for i in 0..3 {
                let c = a[i] + (b[i] - a[i]) * k;
                px[2 - i] = (c + (255.0 - c) * glow) as u8;
            }
            px[3] = 255;
            v.extend_from_slice(&px);
        }
    }
    Decoded { w: n as u32, h: n as u32, bgra: v }
}

impl Widget for Music {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        if !self.has {
            let b = cx.bounds();
            let g = match cx.size {
                Size::S => 34.0,
                Size::M => 40.0,
                Size::L => 56.0,
            };
            cx.icon(Glyph::Note, Rect::new(b.cx() - g / 2.0, b.cy() - g / 2.0 - 12.0, g, g), FAINT);
            cx.text(Rect::new(b.x + 8.0, b.cy() + g / 2.0 - 6.0, b.w - 16.0, 22.0), cx.t("Ничего не играет", "Not Playing"), Style::new(13.0, 500).center().ellipsis(), DIM);
            return Ok(());
        }
        let l = lay(cx.size);
        self.draw_cover(cx, &l);
        if let Some(p) = l.pill {
            cx.rrect(p, p.h / 2.0, BLACK.alpha(0.36));
            cx.rrect_stroke(p, p.h / 2.0, 0.8, WHITE.alpha(0.2));
        }
        let art = if l.pill.is_some() { TEXT.fade(0.75) } else { TEXT.fade(0.74) };
        if !(self.playing && self.marquee(&l)) {
            self.title_text(cx, &l, 0.0);
        }
        cx.text(l.artist, &self.artist, Style::new(if cx.size == Size::S { 11.0 } else { 13.0 }, 400).ellipsis(), art);
        if !self.playing {
            if let Some(eq) = l.eq {
                self.eq_bars(cx, eq, false);
            }
            self.progress(cx, &l);
        }
        self.controls(cx, &l);
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        if !self.has || !self.playing {
            return Ok(());
        }
        let l = lay(cx.size);
        if self.marquee(&l) {
            let off = if self.preview { 0.0 } else { marquee_off(self.title_w - l.title.w, cx.now.secs_f() as f64) };
            self.title_text(cx, &l, off);
        }
        if let Some(eq) = l.eq {
            self.eq_bars(cx, eq, true);
        }
        self.progress(cx, &l);
        Ok(())
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        if !self.started {
            self.started = true;
            if cx.preview {
                self.start_preview(cx);
            } else {
                let (tx, rx) = channel();
                self.tx = Some(tx);
                let (shared, waker) = (self.shared.clone(), cx.waker());
                if std::thread::Builder::new().name("music-smtc".into()).spawn(move || worker(rx, shared, waker)).is_err() {
                    self.tx = None;
                }
            }
        }
        if cx.preview {
            return Tick::Never;
        }
        self.sync(cx);
        if !(self.has && self.playing) {
            return Tick::Never;
        }
        let fast = self.marquee(&lay(cx.size));
        match cx.size {
            Size::M => Tick::Fps(30),
            Size::S if !fast => Tick::Never,
            Size::L if !fast => Tick::Second,
            _ => Tick::Fps(30),
        }
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if !self.has {
            return false;
        }
        let l = lay(cx.size);
        let pad = |r: Rect| r.inset(-4.0);
        let hit: [(u8, Option<Rect>); 3] = [(1, l.prev), (2, Some(pad(l.pp))), (3, l.next)];
        let over = hit.iter().find(|(_, r)| r.is_some_and(|r| ev.inside(r))).map_or(0, |(id, _)| *id);
        let changed = over != self.hov;
        self.hov = over;
        if ev.kind == MouseKind::Up && ev.button == Button::Left {
            let cmd = match over {
                1 => Some(Cmd::Prev),
                2 => Some(Cmd::Toggle),
                3 => Some(Cmd::Next),
                _ => None,
            };
            if let (Some(c), Some(tx)) = (cmd, &self.tx) {
                let _ = tx.send(c);
            }
        }
        if changed {
            self.ver += 1;
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
    fn clock_format() {
        assert_eq!(fmt_clock(0), "0:00");
        assert_eq!(fmt_clock(84_000), "1:24");
        assert_eq!(fmt_clock(216_999), "3:36");
        assert_eq!(fmt_clock(3_725_000), "1:02:05");
        assert_eq!(fmt_clock(-5), "0:00");
    }

    #[test]
    fn position_adjust() {
        assert_eq!(adjust_position(10_000, 2_500, true, 100_000), 12_500);
        assert_eq!(adjust_position(10_000, 2_500, false, 100_000), 10_000);
        assert_eq!(adjust_position(99_000, 5_000, true, 100_000), 100_000);
        assert_eq!(adjust_position(5_000, -300, true, 0), 5_000);
    }

    #[test]
    fn marquee_holds_and_scrolls() {
        assert_eq!(marquee_off(50.0, 0.0), 0.0);
        let d = 50.0 / 32.0 + 0.8;
        assert!((marquee_off(50.0, 1.6 + d + 0.1) + 50.0).abs() < 1e-3); // held at the end
        let mid = marquee_off(50.0, 1.6 + d / 2.0);
        assert!(mid < -10.0 && mid > -40.0);
    }

    #[test]
    fn accent_sampler() {
        let px = |b: u8, g: u8, r: u8| [b, g, r, 255];
        let img = |c: [u8; 4]| -> Vec<u8> { (0..16 * 16).flat_map(|_| c).collect() };
        let red = accent_from(&img(px(20, 30, 220)), 16, 16);
        assert!(red.r > red.g && red.r > red.b);
        let blue = accent_from(&img(px(220, 60, 20)), 16, 16);
        assert!(blue.b > blue.r && blue.b > blue.g);
        assert_eq!(accent_from(&img(px(128, 128, 128)), 16, 16), PINK);
        assert_eq!(accent_from(&[], 0, 0), PINK);
    }

    #[test]
    fn snap_diff_ignores_normal_progress() {
        let a = Snap { has: true, playing: true, title: "t".into(), pos_ms: 10_000, dur_ms: 200_000, ..Snap::idle() };
        let mut b = a.clone();
        b.stamp = a.stamp + Duration::from_secs(1);
        b.pos_ms = 11_000;
        assert!(!Snap::differs(&a, &b));
        b.pos_ms = 60_000; // seek
        assert!(Snap::differs(&a, &b));
        let mut c = a.clone();
        c.playing = false;
        assert!(Snap::differs(&a, &c));
    }

    #[test]
    fn hsl_primary() {
        let c = hsl_to_color(0.0, 1.0, 0.5);
        assert!((c.r - 1.0).abs() < 1e-3 && c.g < 1e-3 && c.b < 1e-3);
        let c = hsl_to_color(240.0, 1.0, 0.5);
        assert!(c.b > 0.99 && c.r < 1e-3);
    }
}
