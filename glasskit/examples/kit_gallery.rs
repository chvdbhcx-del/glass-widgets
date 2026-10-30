//! Headless gallery of the kit: renders every control in every state to PNG files through the WARP software device, at scale 1 and 2, for the
//! dark and the light Pill theme (plus the Settings and glass themes), and two animated-state frame sequences (the segmented indicator and the toggle).
//!
//! ```text
//! cargo run -p glasskit --example kit_gallery -- --out target-agent/kit-images
//! cargo run -p glasskit --example kit_gallery -- --out some/dir --only controls
//! ```
//! `--only` takes `controls`, `sequences`, `text` (inputs), `scroll` (virtual list), `rich` (markdown) (default: all). Nothing here is part of the library; the strings are demo text.
use glasskit::controls::*;
use glasskit::edit::TextInput;
use glasskit::markdown;
use glasskit::richtext::view::{MarkdownLabels, MarkdownView};
use glasskit::scroll::{RowSource, RowState, ScrollList};
use glasskit::ease;
use glasskit::geom::Rect;
use glasskit::icons::Glyph;
use glasskit::input::{Buttons, InputEvent, Modifiers, MouseButton};
use glasskit::svgpath::{GeomCache, Svg};
use glasskit::testkit::Harness;
use glasskit::text::Style;
use glasskit::theme::Theme;
use glasskit::{Canvas, Color};
use std::path::PathBuf;
use std::rc::Rc;

fn mv(x: f32, y: f32) -> InputEvent {
    InputEvent::MouseMove { pos: (x, y), mods: Modifiers::NONE, buttons: Buttons::NONE }
}
fn down(x: f32, y: f32) -> InputEvent {
    InputEvent::MouseDown { pos: (x, y), button: MouseButton::Left, mods: Modifiers::NONE, clicks: 1 }
}
fn up(x: f32, y: f32) -> InputEvent {
    InputEvent::MouseUp { pos: (x, y), button: MouseButton::Left, mods: Modifiers::NONE }
}

/// Put a widget into a state for the picture and let its animations finish.
fn pose(w: &mut dyn Widget, rect: Rect, hover: bool, press: bool, focus: bool, enabled: bool) {
    let c = rect.center();
    w.set_enabled(enabled);
    if hover || press {
        w.event(&mv(c.0, c.1), rect);
    }
    if press {
        w.event(&down(c.0, c.1), rect);
    }
    if focus {
        w.set_focus(true, true);
    }
    for _ in 0..60 {
        w.step(1.0 / 60.0);
    }
}

struct Icons {
    svgs: Vec<Rc<Svg>>,
    cache: Rc<GeomCache>,
}

impl Icons {
    fn new() -> Icons {
        let inner = [
            r#"<path d="M3.5 11 12 3.8l8.5 7.2"/><path d="M5.5 9.6V20h4.4v-5.6h4.2V20h4.4V9.6"/>"#,
            r#"<rect x="5" y="4.5" width="14" height="17" rx="3"/><path d="M9 4.5V4a1.5 1.5 0 0 1 1.5-1.5h3A1.5 1.5 0 0 1 15 4v.5"/><path d="M9 11h.01M9 15.5h.01M12.5 11H15M12.5 15.5H15"/>"#,
            r#"<path d="M7.9 20A9 9 0 1 0 4 16.1L2 22z"/>"#,
            r#"<path d="M20 7h-9M14 17H5"/><circle cx="17" cy="17" r="3"/><circle cx="7" cy="7" r="3"/>"#,
            r#"<path d="M20 13c0 5-3.5 7.5-7.7 9a1 1 0 0 1-.7 0C7.5 20.500 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.500-1.200 6.200-2.700a1.200 1.200 0 0 1 1.500 0C14.500 3.800 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>"#,
        ];
        Icons { svgs: inner.iter().map(|s| Rc::new(Svg::icon(s, 1.7))).collect(), cache: Rc::new(GeomCache::new()) }
    }
    fn get(&self, i: usize) -> Icon {
        svg_icon(self.svgs[i % self.svgs.len()].clone(), self.cache.clone())
    }
}

fn caption(cv: &Canvas, th: &Theme, y: f32, h: f32, text: &str) {
    cv.text(Rect::new(12.0, y, 100.0, h), text, th.caption(), th.dim);
}

fn sub(cv: &Canvas, th: &Theme, x: f32, y: f32, w: f32, text: &str) {
    cv.text(Rect::new(x, y, w, 12.0), text, Style::new(10.0, 500).center(), th.faint);
}

const X0: f32 = 120.0;

/// Every control in every state; returns nothing, the caller sized the canvas with `controls_height`.
fn controls_sheet(cv: &Canvas, th: &Theme, ic: &Icons) {
    cv.fill_rect(Rect::new(0.0, 0.0, SHEET_W as f32, SHEET_H as f32), th.bg);
    let mut y = 12.0;

    // ---- buttons
    caption(cv, th, y, 32.0, "Buttons");
    let kinds = [(ButtonKind::Normal, "Cancel"), (ButtonKind::Primary, "Save"), (ButtonKind::Ghost, "Skip"), (ButtonKind::Danger, "Delete")];
    let mut x = X0;
    for (k, label) in kinds {
        let mut b = Button::new(label).kind(k);
        let r = Rect::new(x, y, 0.0, 0.0);
        let (w, h) = { let mut s = (0.0, 0.0); cv_size(cv, th, &b, &mut s); s };
        let r = Rect::new(r.x, r.y, w, h);
        pose(&mut b, r, false, false, false, true);
        b.draw(cv, th, r);
        x += w + 10.0;
    }
    let mut ib = Button::icon_only("add", glyph_icon(Glyph::Plus));
    pose(&mut ib, Rect::new(x, y + 2.0, 28.0, 28.0), false, false, false, true);
    ib.draw(cv, th, Rect::new(x, y + 2.0, 28.0, 28.0));
    let mut sb = Button::with_icon("Settings", ic.get(3)).kind(ButtonKind::Normal);
    let (sw, sh) = { let mut s = (0.0, 0.0); cv_size(cv, th, &sb, &mut s); s };
    let sr = Rect::new(x + 40.0, y, sw, sh);
    pose(&mut sb, sr, false, false, false, true);
    sb.draw(cv, th, sr);
    y += 44.0;
    caption(cv, th, y, 32.0, "  states");
    let mut x = X0;
    for (i, (name, hover, press, focus, enabled)) in [("hover", true, false, false, true), ("pressed", true, true, false, true), ("focus", false, false, true, true), ("disabled", false, false, false, false)].into_iter().enumerate() {
        for (j, k) in [ButtonKind::Normal, ButtonKind::Primary].into_iter().enumerate() {
            let mut b = Button::new(if j == 0 { "Normal" } else { "Primary" }).kind(k);
            let r = Rect::new(x, y, 76.0, 32.0);
            pose(&mut b, r, hover, press, focus, enabled);
            b.draw(cv, th, r);
            if j == 0 {
                sub(cv, th, x, y + 34.0, 76.0, name);
            }
            x += 82.0;
        }
        let _ = i;
        x += 0.0;
        if x > 560.0 {
            break;
        }
    }
    y += 62.0;

    // ---- toggles
    caption(cv, th, y, 24.0, "Toggle");
    let mut x = X0;
    for (name, on, hover, press, focus, enabled) in [
        ("off", false, false, false, false, true),
        ("on", true, false, false, false, true),
        ("hover", false, true, false, false, true),
        ("held", true, true, true, false, true),
        ("focus", true, false, false, true, true),
        ("disabled", true, false, false, false, false),
    ] {
        let mut t = Toggle::new(on).spring(th.m.sw_spring);
        let r = Rect::new(x, y, th.m.sw_w, th.m.sw_h);
        pose(&mut t, r, hover, press, focus, enabled);
        t.draw(cv, th, r);
        sub(cv, th, x - 6.0, y + 28.0, th.m.sw_w + 12.0, name);
        x += 70.0;
    }
    y += 54.0;

    // ---- sliders
    caption(cv, th, y, 16.0, "Slider");
    let mut x = X0;
    for (name, v, thin, hover, press, enabled) in [("0", 0.0, false, false, false, true), ("50", 50.0, false, false, false, true), ("100", 100.0, false, false, false, true), ("thin", 40.0, true, false, false, true), ("thin hover", 40.0, true, true, false, true), ("value", 65.0, false, false, true, true), ("disabled", 50.0, false, false, false, false)] {
        let mut s = Slider::new(0.0, 100.0, v).with_format(|v| format!("{v:.0}%"));
        if thin {
            s = s.thin();
        }
        let r = Rect::new(x, y + 14.0, 60.0, 16.0);
        s.event_themed(&mv(-100.0, -100.0), r, th);
        pose_slider(&mut s, r, hover, press, enabled, th);
        s.draw(cv, th, r);
        sub(cv, th, x, y + 34.0, 60.0, name);
        x += 68.0;
    }
    y += 62.0;

    // ---- segmented
    caption(cv, th, y, 24.0, "Segmented");
    let mut x = X0;
    for (sel, hover) in [(0usize, false), (1, false), (2, false), (0, true)] {
        let mut s = Segmented::new(&["Day", "Week", "Month"], sel);
        let (w, h) = { let mut z = (0.0, 0.0); cv_size(cv, th, &s, &mut z); z };
        let r = Rect::new(x, y, w, h);
        if hover {
            let t = s.item_rects(r)[2].center();
            s.event(&mv(t.0, t.1), r);
            for _ in 0..30 {
                s.step(1.0 / 60.0);
            }
        }
        s.draw(cv, th, r);
        x += w + 12.0;
    }
    y += 44.0;

    // ---- checkbox / radio
    caption(cv, th, y, 20.0, "Checkbox");
    let mut x = X0;
    for (name, st, hover, focus, enabled) in [("off", CheckState::Off, false, false, true), ("on", CheckState::On, false, false, true), ("mixed", CheckState::Mixed, false, false, true), ("hover", CheckState::Off, true, false, true), ("focus", CheckState::On, false, true, true), ("disabled", CheckState::On, false, false, false)] {
        let mut c = Checkbox::new("", st);
        let r = Rect::new(x, y, 20.0, 20.0);
        pose(&mut c, r, hover, false, focus, enabled);
        c.draw(cv, th, r);
        sub(cv, th, x - 14.0, y + 24.0, 48.0, name);
        x += 62.0;
    }
    let mut c = Checkbox::new("Remember me", CheckState::On);
    let r = Rect::new(x + 10.0, y, 130.0, 20.0);
    pose(&mut c, r, false, false, false, true);
    c.draw(cv, th, r);
    y += 46.0;
    caption(cv, th, y, 20.0, "Radio");
    let mut rg = RadioGroup::new(&["Light", "Dark", "Auto"], 1);
    let rr = Rect::new(X0, y - 4.0, 120.0, 84.0);
    pose(&mut rg, rr, false, false, false, true);
    rg.draw(cv, th, rr);
    let mut x = X0 + 150.0;
    for (name, on, hover) in [("off", false, false), ("on", true, false), ("hover", false, true)] {
        let mut r = Radio::new("", on);
        let rc = Rect::new(x, y, 20.0, 20.0);
        pose(&mut r, rc, hover, false, false, true);
        r.draw(cv, th, rc);
        sub(cv, th, x - 14.0, y + 24.0, 48.0, name);
        x += 62.0;
    }
    y += 92.0;

    // ---- badges
    caption(cv, th, y, 20.0, "Badge");
    let mut x = X0;
    for (t, k) in [("NEUTRAL", BadgeKind::Neutral), ("ACCENT", BadgeKind::Accent), ("FREE", BadgeKind::Green), ("ERROR", BadgeKind::Red), ("WARN", BadgeKind::Amber), ("PRO", BadgeKind::Pro)] {
        let b = Badge::new(t, k);
        let (w, h) = b.natural_size(cv, th);
        b.draw(cv, th, Rect::new(x, y + 2.0, w, h));
        x += w + 8.0;
    }
    let mut x = X0;
    for (t, k) in [("Neutral", BadgeKind::Neutral), ("Accent", BadgeKind::Accent), ("Connected", BadgeKind::Green)] {
        let b = Badge::new(t, k).pill();
        let (w, h) = b.natural_size(cv, th);
        b.draw(cv, th, Rect::new(x, y + 24.0, w, h));
        x += w + 8.0;
    }
    y += 62.0;

    // ---- progress
    caption(cv, th, y, 20.0, "Progress");
    let mut x = X0;
    for (name, v) in [("0 %", 0.0), ("35 %", 0.35), ("100 %", 1.0)] {
        let mut p = ProgressBar::determinate(v);
        p.step(1.0);
        p.draw(cv, th, Rect::new(x, y + 8.0, 90.0, 4.0));
        sub(cv, th, x, y + 18.0, 90.0, name);
        x += 104.0;
    }
    let mut p = ProgressBar::indeterminate();
    p.step(0.55);
    p.draw(cv, th, Rect::new(x, y + 8.0, 90.0, 4.0));
    sub(cv, th, x, y + 18.0, 90.0, "unknown");
    let mut x = X0;
    y += 40.0;
    caption(cv, th, y, 20.0, "Spinner");
    for t in [0.0, 0.3, 0.6] {
        let mut s = Spinner::new();
        s.step(t);
        s.draw(cv, th, Rect::new(x, y + 2.0, 18.0, 18.0));
        sub(cv, th, x - 14.0, y + 24.0, 46.0, &format!("{:.0} ms", t * 1000.0));
        x += 60.0;
    }
    y += 52.0;

    // ---- icons
    caption(cv, th, y, 24.0, "Icons");
    let mut x = X0;
    for i in 0..5 {
        let mut b = Button::icon_only("i", ic.get(i));
        let r = Rect::new(x, y, 28.0, 28.0);
        pose(&mut b, r, i == 1, false, false, true);
        b.draw(cv, th, r);
        x += 36.0;
    }
    for g in [Glyph::Search, Glyph::Copy, Glyph::Trash, Glyph::Bell] {
        let r = Rect::new(x, y + 6.0, 16.0, 16.0);
        cv.glyph(g, r, th.icon);
        x += 28.0;
    }
    y += 48.0;

    // ---- select (closed, placeholder, open)
    caption(cv, th, y, th.m.field_h, "Select");
    let mut s1 = Select::new(&["Automatic", "Dark", "Light"], Some(1));
    let r1 = Rect::new(X0, y, 170.0, th.m.field_h);
    let s2 = Select::new(&["A", "B"], None).with_placeholder("Choose a theme");
    let r2 = Rect::new(X0 + 190.0, y, 170.0, th.m.field_h);
    let mut s3 = Select::new(&["Automatic", "Dark", "Light"], Some(0));
    let r3 = Rect::new(X0 + 380.0, y, 150.0, th.m.field_h);
    s3.set_work_area(Rect::new(0.0, 0.0, SHEET_W as f32, SHEET_H as f32));
    s1.draw(cv, th, r1);
    s2.draw(cv, th, r2);
    pose(&mut s1, r1, true, false, false, true);
    s1.draw(cv, th, r1);
    s3.draw(cv, th, r3);
    s3.open(r3);
    s3.event(&mv(r3.x + 20.0, r3.bottom() + 44.0), r3);
    for _ in 0..40 {
        s3.step(1.0 / 60.0);
    }
    s3.draw(cv, th, r3);
    s3.draw_popup(cv, th);
    y += 160.0;

    // ---- tooltip
    caption(cv, th, y, 28.0, "Tooltip");
    let mut b = Button::icon_only("copy", glyph_icon(Glyph::Copy));
    let br = Rect::new(X0, y, 28.0, 28.0);
    pose(&mut b, br, true, false, false, true);
    b.draw(cv, th, br);
    let mut tip = Tooltip::new();
    tip.hover(1, br, "Copy to clipboard", Rect::new(0.0, 0.0, SHEET_W as f32, SHEET_H as f32));
    for _ in 0..60 {
        tip.step(1.0 / 60.0);
    }
    tip.draw(cv, th);
}

fn cv_size(cv: &Canvas, th: &Theme, w: &dyn Widget, out: &mut (f32, f32)) {
    *out = w.natural_size(cv, th);
}

fn pose_slider(s: &mut Slider, r: Rect, hover: bool, press: bool, enabled: bool, th: &Theme) {
    s.set_enabled(enabled);
    if hover || press {
        let (_, ty) = (0.0, r.cy());
        s.event_themed(&mv(r.cx(), ty), r, th);
    }
    if press {
        // hold the thumb at 65 %
        s.event_themed(&down(r.x + 6.0 + (r.w - 12.0) * 0.65, r.cy()), r, th);
    }
    for _ in 0..60 {
        s.step(1.0 / 60.0);
    }
}

const SHEET_H: u32 = 860;
const SHEET_W: u32 = 760;

fn save(h: &Harness, img: &glasskit::testkit::Image, dir: &std::path::Path, name: &str) {
    let p = dir.join(name);
    match img.save_png(h, &p) {
        Ok(()) => println!("{}", p.display()),
        Err(e) => eprintln!("could not write {}: {e}", p.display()),
    }
}

fn main() {
    let mut out = PathBuf::from("kit-images");
    let mut only: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--out" => out = PathBuf::from(args.next().expect("--out DIR")),
            "--only" => only = args.next(),
            _ => {}
        }
    }
    let want = |s: &str| only.as_deref().map_or(true, |o| o == s);
    // the gallery always shows the animated look, whatever the machine's accessibility setting
    ease::force_reduced(Some(false));
    let h = Harness::new();
    let ic = Icons::new();
    let themes: Vec<(&str, Theme, Color)> = vec![
        ("pill-dark", Theme::pill_dark(), Color::rgb8(0, 0, 0)),
        ("pill-light", Theme::pill_light(), Color::rgb8(246, 246, 246)),
        ("settings", Theme::settings_dark(), Color::rgb8(0x11, 0x10, 0x13)),
        ("glass", Theme::glass(), Color::rgb8(58, 84, 130)),
    ];
    if want("controls") {
        for (name, th, bg) in &themes {
            for scale in [1.0f32, 2.0] {
                let img = h.render(SHEET_W, SHEET_H, scale, *bg, |cv| controls_sheet(cv, th, &ic));
                save(&h, &img, &out, &format!("controls_{name}@{scale:.0}x.png"));
            }
        }
    }
    if want("sequences") {
        sequences(&h, &out, &themes);
    }
    for (name, th, bg) in themes.iter().take(3) {
        for scale in [1.0f32, 2.0] {
            if want("text") {
                let img = h.render(520, 420, scale, *bg, |cv| text_sheet(cv, th));
                save(&h, &img, &out, &format!("text_{name}@{scale:.0}x.png"));
            }
            if want("scroll") {
                let img = h.render(520, 360, scale, *bg, |cv| scroll_sheet(cv, th));
                save(&h, &img, &out, &format!("scroll_{name}@{scale:.0}x.png"));
            }
            if want("rich") {
                let img = h.render(520, 640, scale, *bg, |cv| rich_sheet(cv, th));
                save(&h, &img, &out, &format!("markdown_{name}@{scale:.0}x.png"));
            }
        }
    }
}

/// Text inputs in their states.
fn text_sheet(cv: &Canvas, th: &Theme) {
    cv.fill_rect(Rect::new(0.0, 0.0, 520.0, 420.0), th.bg);
    let mut y = 14.0;
    let mut field = |cv: &Canvas, label: &str, h: f32, setup: &dyn Fn(&mut TextInput)| {
        let mut t = TextInput::new(h > 50.0, th);
        t.reduced = Some(false);
        setup(&mut t);
        let r = Rect::new(150.0, y, 340.0, h);
        t.update(r, cv);
        for _ in 0..30 {
            t.step(1.0 / 60.0);
        }
        t.set_blink_phase(0.0);
        cv.text(Rect::new(14.0, y, 130.0, th.m.field_h), label, th.caption(), th.dim);
        t.draw(cv, r, th);
        y += h + 14.0;
    };
    let fh = th.m.field_h;
    field(cv, "empty + placeholder", fh, &|t| t.placeholder = "Search settings".into());
    field(cv, "text", fh, &|t| t.set_text("Привет, мир! Hello"));
    field(cv, "focused, caret", fh, &|t| {
        t.set_text("Focused field");
        t.set_focused(true);
    });
    field(cv, "selection", fh, &|t| {
        t.set_text("Select the middle word");
        t.set_focused(true);
        t.set_selection(4, 14);
    });
    field(cv, "password", fh, &|t| {
        t.password = true;
        t.set_text("hunter2");
    });
    field(cv, "disabled", fh, &|t| {
        t.enabled = false;
        t.set_text("Not editable");
    });
    field(cv, "multi-line", 96.0, &|t| {
        t.set_text("A multi-line field wraps long text to its width and keeps the caret in view.\nSecond paragraph with emoji \u{1F44D} and Кириллица.");
        t.set_focused(true);
        t.set_selection(6, 22);
    });
}

struct Rows;

impl RowSource for Rows {
    fn len(&self) -> usize {
        2000
    }
    fn height(&self, i: usize, _w: f32) -> f32 {
        if i % 5 == 3 { 64.0 } else { 40.0 }
    }
    fn draw_row(&self, cv: &Canvas, i: usize, r: Rect, st: RowState) {
        let c = if st.selected { Color::rgba(0.56, 0.59, 1.0, 0.25) } else if st.hover { Color::rgba(1.0, 1.0, 1.0, 0.07) } else { Color::rgba(1.0, 1.0, 1.0, 0.0) };
        cv.fill_rr(r.inset_xy(4.0, 2.0), 10.0, c);
        cv.text(r.inset_xy(14.0, 0.0), &format!("Row {i}{}", if i % 5 == 3 { " (tall)" } else { "" }), Style::new(13.0, 400), Color::rgba(0.5, 0.5, 0.55, 1.0));
    }
}

/// A virtualised list scrolled to the middle with its scrollbar showing and one row selected.
fn scroll_sheet(cv: &Canvas, th: &Theme) {
    cv.fill_rect(Rect::new(0.0, 0.0, 520.0, 360.0), th.bg);
    let mut l = ScrollList::new(40.0);
    let view = Rect::new(20.0, 20.0, 480.0, 320.0);
    let src = Rows;
    l.update(&src, view);
    l.scroll_to_index(&src, 1000, false, glasskit::scroll::Align::Start);
    l.set_selected(Some(1002));
    l.update(&src, view);
    l.bar.show();
    for _ in 0..10 {
        l.step(1.0 / 60.0, &src, view);
    }
    cv.stroke_rr(view, 12.0, 1.0, th.border);
    cv.with_clip_rr(view, 12.0, |cv| l.draw(cv, &src, view, th));
}

const SAMPLE: &str = "# Native Pill\n\nThe **command bar** answers in *markdown*: `inline code`, [a link](https://example.com) and ~~strike~~.\n\n## Lists\n\n- first item\n- second item\n  - nested one\n  - nested two\n- third\n\n1. one\n2. two\n\n> A quote with a **bold** word.\n\n```rust\nfn main() {\n    println!(\"привет\");\n}\n```\n\n---\n\nПоддерживается кириллица and emoji \u{1F680}.";

fn rich_sheet(cv: &Canvas, th: &Theme) {
    cv.fill_rect(Rect::new(0.0, 0.0, 520.0, 640.0), th.bg);
    let labels = MarkdownLabels { copy: "Copy", copied: "Copied" };
    match MarkdownView::new(cv, markdown::parse(SAMPLE), 480.0, th, &labels) {
        Ok(mut v) => {
            v.step(0.0);
            v.draw(cv, (20.0, 16.0), th);
        }
        Err(e) => eprintln!("markdown view: {e}"),
    }
}


/// Frame sequences at t = 0, 40, 80, ... ms: the segmented indicator sliding with its overshoot and the toggle knob springing over.
fn sequences(h: &Harness, out: &std::path::Path, themes: &[(&str, Theme, Color)]) {
    let ts: Vec<f32> = (0..=10).map(|i| i as f32 * 0.04).collect();
    for (name, th, bg) in themes.iter().take(2) {
        for scale in [1.0f32, 2.0] {
            // segmented: Day -> Month
            let proto = Segmented::new(&["Day", "Week", "Month"], 0);
            let mut size = (0.0, 0.0);
            h.render(4, 4, 1.0, th.bg, |cv| size = proto.natural_size(cv, th));
            let (fw, fh) = (size.0.ceil() as u32 + 16, size.1.ceil() as u32 + 22);
            let img = h.render_sequence(fw, fh, scale, *bg, &ts, 4, |cv, t| {
                cv.fill_rect(Rect::new(0.0, 0.0, fw as f32, fh as f32), th.bg);
                let mut s = Segmented::new(&["Day", "Week", "Month"], 0);
                let r = Rect::new(8.0, 4.0, size.0, size.1);
                s.draw(cv, th, r); // measure + first layout
                s.select(2, r, true);
                for _ in 0..(t * 1000.0).round() as u32 {
                    s.step(0.001);
                }
                s.draw(cv, th, r);
                cv.text(Rect::new(0.0, fh as f32 - 14.0, fw as f32, 12.0), &format!("{:.0} ms", t * 1000.0), Style::new(10.0, 500).center(), th.faint);
            });
            save(h, &img, out, &format!("seq_segmented_{name}@{scale:.0}x.png"));

            // toggle: off -> on, then held
            let tw = th.m.sw_w.ceil() as u32 + 24;
            let img = h.render_sequence(tw, 48, scale, *bg, &ts, 4, |cv, t| {
                cv.fill_rect(Rect::new(0.0, 0.0, tw as f32, 48.0), th.bg);
                let mut tg = Toggle::new(false).spring(th.m.sw_spring);
                let r = Rect::new(12.0, 6.0, th.m.sw_w, th.m.sw_h);
                tg.event(&mv(r.cx(), r.cy()), r);
                tg.event(&down(r.cx(), r.cy()), r);
                tg.event(&up(r.cx(), r.cy()), r);
                for _ in 0..(t * 1000.0).round() as u32 {
                    tg.step(0.001);
                }
                tg.draw(cv, th, r);
                cv.text(Rect::new(0.0, 34.0, tw as f32, 12.0), &format!("{:.0} ms", t * 1000.0), Style::new(10.0, 500).center(), th.faint);
            });
            save(h, &img, out, &format!("seq_toggle_{name}@{scale:.0}x.png"));
        }
    }
}
