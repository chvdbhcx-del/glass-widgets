//! Built-in vector glyphs. Each glyph is a few SVG-style paths in a 24x24 box (absolute M L H V C Q A Z only), painted with the
//! colour passed to `Cx::icon`. Stroke widths are in glyph units, so they scale with the icon size. Geometry is built lazily and cached.
use crate::gfx::*;
use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Glyph {
    Play,
    Pause,
    Next,
    Prev,
    Plug,
    Bolt,
    Cloud,
    Sun,
    PartlyCloudy,
    Rain,
    Snow,
    Thunder,
    Moon,
    Drop,
    Cpu,
    Ram,
    Gpu,
    Disk,
    NetUp,
    NetDown,
    Note,
    Check,
    CheckCircle,
    Circle,
    Plus,
    Trash,
    Bell,
    Power,
    Sleep,
    Lock,
    Restart,
    Wifi,
    Bluetooth,
    Volume,
    Brightness,
    Mic,
    ChevL,
    ChevR,
    ChevUp,
    ChevDown,
    X,
    Clock,
    Calendar,
    Laptop,
    Headphones,
    Keyboard,
    Mouse,
    Phone,
    Battery,
    Clipboard,
    Copy,
    Folder,
    Image,
    Star,
    Pin,
    Search,
    Location,
}

impl Glyph {
    pub const ALL: &'static [Glyph] = &[
        Glyph::Play, Glyph::Pause, Glyph::Next, Glyph::Prev, Glyph::Plug, Glyph::Bolt, Glyph::Cloud, Glyph::Sun, Glyph::PartlyCloudy, Glyph::Rain,
        Glyph::Snow, Glyph::Thunder, Glyph::Moon, Glyph::Drop, Glyph::Cpu, Glyph::Ram, Glyph::Gpu, Glyph::Disk, Glyph::NetUp, Glyph::NetDown,
        Glyph::Note, Glyph::Check, Glyph::CheckCircle, Glyph::Circle, Glyph::Plus, Glyph::Trash, Glyph::Bell, Glyph::Power, Glyph::Sleep, Glyph::Lock,
        Glyph::Restart, Glyph::Wifi, Glyph::Bluetooth, Glyph::Volume, Glyph::Brightness, Glyph::Mic, Glyph::ChevL, Glyph::ChevR, Glyph::ChevUp,
        Glyph::ChevDown, Glyph::X, Glyph::Clock, Glyph::Calendar, Glyph::Laptop, Glyph::Headphones, Glyph::Keyboard, Glyph::Mouse, Glyph::Phone,
        Glyph::Battery, Glyph::Clipboard, Glyph::Copy, Glyph::Folder, Glyph::Image, Glyph::Star, Glyph::Pin, Glyph::Search, Glyph::Location,
    ];
}

#[derive(Clone, Copy, Debug)]
pub enum Paint {
    Fill,
    /// stroke width in glyph units
    Stroke(f32),
    /// fill + a round-joined stroke (soft corners on solid shapes)
    Both(f32),
}
use Paint::*;

fn circ(cx: f32, cy: f32, r: f32) -> String {
    format!("M{} {} A{r} {r} 0 1 0 {} {} A{r} {r} 0 1 0 {} {} Z", cx - r, cy, cx + r, cy, cx - r, cy)
}
fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> String {
    let (x2, y2) = (x + w, y + h);
    format!("M{} {y} H{} A{r} {r} 0 0 1 {x2} {} V{} A{r} {r} 0 0 1 {} {y2} H{} A{r} {r} 0 0 1 {x} {} V{} A{r} {r} 0 0 1 {} {y} Z", x + r, x2 - r, y + r, y2 - r, x2 - r, x + r, y2 - r, y + r, x + r)
}
/// Solid cloud scaled by `k` about the glyph centre and shifted by (ox, oy).
fn cloud(ox: f32, oy: f32, k: f32) -> String {
    let p = |x: f32, y: f32| (12.0 + (x - 12.0) * k + ox, 12.0 + (y - 12.0) * k + oy);
    let (a, b, c, d) = (p(7.0, 18.0), p(6.6, 10.05), p(17.3, 9.2), p(17.0, 18.0));
    format!("M{} {} A{} {} 0 0 1 {} {} A{} {} 0 0 1 {} {} A{} {} 0 0 1 {} {} Z", a.0, a.1, 4.0 * k, 4.0 * k, b.0, b.1, 5.5 * k, 5.5 * k, c.0, c.1, 4.4 * k, 4.4 * k, d.0, d.1)
}
/// Sun rays: angles in degrees clockwise from "up", from radius r0 to r1 around (cx, cy).
fn rays(cx: f32, cy: f32, r0: f32, r1: f32, angles: &[f32]) -> String {
    let mut s = String::new();
    for a in angles {
        let (sn, cs) = a.to_radians().sin_cos();
        s += &format!("M{} {} L{} {} ", cx + r0 * sn, cy - r0 * cs, cx + r1 * sn, cy - r1 * cs);
    }
    s
}
const EIGHT: [f32; 8] = [0.0, 45.0, 90.0, 135.0, 180.0, 225.0, 270.0, 315.0];

fn shapes(g: Glyph) -> Vec<(String, Paint)> {
    use Glyph::*;
    let s = |t: &str, p: Paint| (t.to_string(), p);
    match g {
        Play => vec![s("M8 5.3 L8 18.7 L19 12 Z", Both(1.6))],
        Pause => vec![(rrect(6.5, 5.0, 3.8, 14.0, 1.3), Fill), (rrect(13.7, 5.0, 3.8, 14.0, 1.3), Fill)],
        Next => vec![s("M5.5 5.8 L5.5 18.2 L15 12 Z", Both(1.4)), (rrect(16.7, 5.8, 2.6, 12.4, 1.1), Fill)],
        Prev => vec![s("M18.5 5.8 L18.5 18.2 L9 12 Z", Both(1.4)), (rrect(4.7, 5.8, 2.6, 12.4, 1.1), Fill)],
        Plug => vec![s("M9 3 V8 M15 3 V8", Stroke(1.8)), s("M6.5 8 H17.5 V11.5 A5.5 5.5 0 0 1 12 17 A5.5 5.5 0 0 1 6.5 11.5 Z", Stroke(1.8)), s("M12 17 V21", Stroke(1.8))],
        Bolt => vec![s("M13.5 2.8 L5.5 13.5 H11 L10.2 21.2 L18.5 10.3 H13 Z", Both(1.2))],
        Cloud => vec![(cloud(0.0, 1.0, 1.0), Both(1.0))],
        Sun => vec![(circ(12.0, 12.0, 4.6), Fill), (rays(12.0, 12.0, 7.4, 9.8, &EIGHT), Stroke(1.9))],
        Brightness => vec![(circ(12.0, 12.0, 3.8), Stroke(1.7)), (rays(12.0, 12.0, 6.8, 9.4, &EIGHT), Stroke(1.7))],
        PartlyCloudy => vec![(circ(8.4, 8.2, 3.1), Fill), (rays(8.4, 8.2, 5.3, 7.1, &[225.0, 270.0, 315.0, 0.0, 45.0]), Stroke(1.6)), (cloud(3.2, 3.4, 0.8), Both(1.0))],
        Rain => vec![(cloud(0.0, -2.2, 0.82), Both(1.0)), s("M8.5 17.2 L7.3 20.6 M12.5 17.2 L11.3 20.6 M16.5 17.2 L15.3 20.6", Stroke(1.7))],
        Snow => vec![(cloud(0.0, -2.2, 0.82), Both(1.0)), (circ(8.5, 18.3, 1.0), Fill), (circ(12.5, 20.4, 1.0), Fill), (circ(16.5, 18.3, 1.0), Fill)],
        Thunder => vec![(cloud(0.0, -2.2, 0.82), Both(1.0)), s("M12.9 15.9 L9.5 20 H12.2 L11.3 23 L15 18.6 H12.4 Z", Both(0.8))],
        Moon => vec![s("M20.5 14.2 A8.5 8.5 0 1 1 9.8 3.5 A6.7 6.7 0 0 0 20.5 14.2 Z", Both(1.2))],
        Sleep => vec![s("M20.5 14.2 A8.5 8.5 0 1 1 9.8 3.5 A6.7 6.7 0 0 0 20.5 14.2 Z", Stroke(1.7))],
        Drop => vec![s("M12 3.5 C12 3.5 5.5 10.5 5.5 14.8 A6.5 6.5 0 0 0 18.5 14.8 C18.5 10.5 12 3.5 12 3.5 Z", Both(1.2))],
        Cpu => vec![
            (rrect(6.5, 6.5, 11.0, 11.0, 2.0), Stroke(1.6)),
            (rrect(10.0, 10.0, 4.0, 4.0, 0.8), Fill),
            s("M9.5 3.5 V6.5 M14.5 3.5 V6.5 M9.5 17.5 V20.5 M14.5 17.5 V20.5 M3.5 9.5 H6.5 M3.5 14.5 H6.5 M17.5 9.5 H20.5 M17.5 14.5 H20.5", Stroke(1.6)),
        ],
        Ram => vec![
            (rrect(2.5, 6.5, 19.0, 10.0, 1.8), Stroke(1.6)),
            (rrect(5.3, 9.3, 3.2, 4.4, 0.6), Fill),
            (rrect(10.4, 9.3, 3.2, 4.4, 0.6), Fill),
            (rrect(15.5, 9.3, 3.2, 4.4, 0.6), Fill),
            s("M6 16.5 V19.5 M10 16.5 V19.5 M14 16.5 V19.5 M18 16.5 V19.5", Stroke(1.5)),
        ],
        Gpu => vec![
            (rrect(4.0, 6.5, 17.5, 10.5, 1.8), Stroke(1.6)),
            (circ(9.6, 11.75, 2.7), Stroke(1.4)),
            (circ(16.0, 11.75, 2.7), Stroke(1.4)),
            s("M2.5 6.5 V15 M7 17 V19.5 M10.5 17 V19.5 M14 17 V19.5", Stroke(1.6)),
        ],
        Disk => vec![(rrect(3.0, 6.5, 18.0, 11.0, 2.6), Stroke(1.7)), s("M3 13.5 H21", Stroke(1.6)), (circ(17.0, 15.6, 0.95), Fill), (circ(13.8, 15.6, 0.95), Fill)],
        NetUp => vec![s("M12 20 V5 M6 11 L12 5 L18 11", Stroke(2.0))],
        NetDown => vec![s("M12 4 V19 M6 13 L12 19 L18 13", Stroke(2.0))],
        Note => vec![s("M9.5 17.5 V6 L18.5 4 V15.5", Stroke(1.8)), (circ(7.2, 17.5, 2.5), Fill), (circ(16.2, 15.5, 2.5), Fill)],
        Check => vec![s("M5.5 12.5 L10 17 L18.5 7.5", Stroke(2.2))],
        CheckCircle => vec![(circ(12.0, 12.0, 9.0), Stroke(1.8)), s("M8 12.5 L11 15.5 L16.5 9", Stroke(2.0))],
        Circle => vec![(circ(12.0, 12.0, 8.6), Stroke(1.8))],
        Plus => vec![s("M12 5 V19 M5 12 H19", Stroke(2.0))],
        Trash => vec![s("M4.5 7 H19.5 M9.5 7 V4.5 H14.5 V7 M6.5 7 L7.5 19.5 H16.5 L17.5 7 M10.2 10.5 V16 M13.8 10.5 V16", Stroke(1.7))],
        Bell => vec![s("M12 3.5 A6 6 0 0 0 6 9.5 V13.5 L4.5 16.5 H19.5 L18 13.5 V9.5 A6 6 0 0 0 12 3.5 Z", Stroke(1.7)), s("M10 19.5 A2.2 2.2 0 0 0 14 19.5", Stroke(1.7))],
        Power => vec![s("M7.2 6.9 A8 8 0 1 0 16.8 6.9", Stroke(1.9)), s("M12 3.5 V12", Stroke(1.9))],
        Lock => vec![
            (rrect(5.0, 10.5, 14.0, 10.0, 2.5), Stroke(1.7)),
            s("M8 10.5 V8 A4 4 0 0 1 16 8 V10.5", Stroke(1.7)),
            (circ(12.0, 15.0, 1.3), Fill),
            s("M12 15 V17.5", Stroke(1.6)),
        ],
        Restart => vec![s("M19 12 A7 7 0 1 1 16.5 6.6", Stroke(1.9)), s("M17.4 2.9 V7.2 H13.1", Stroke(1.9))],
        Wifi => vec![
            s("M2.8 9.2 A13 13 0 0 1 21.2 9.2", Stroke(1.8)),
            s("M5.8 12.6 A8.7 8.7 0 0 1 18.2 12.6", Stroke(1.8)),
            s("M8.8 16 A4.4 4.4 0 0 1 15.2 16", Stroke(1.8)),
            (circ(12.0, 19.3, 1.2), Fill),
        ],
        Bluetooth => vec![s("M6.5 7.5 L17.5 16.5 L12 21 V3 L17.5 7.5 L6.5 16.5", Stroke(1.8))],
        Volume => vec![s("M4 9.5 H7.5 L12.5 5.5 V18.5 L7.5 14.5 H4 Z", Both(1.4)), s("M15.5 9.2 A4 4 0 0 1 15.5 14.8", Stroke(1.7)), s("M18.2 6.5 A7.5 7.5 0 0 1 18.2 17.5", Stroke(1.7))],
        Mic => vec![(rrect(9.0, 3.0, 6.0, 11.0, 3.0), Stroke(1.7)), s("M5.5 11 A6.5 6.5 0 0 0 18.5 11", Stroke(1.7)), s("M12 17.5 V21 M9 21 H15", Stroke(1.7))],
        ChevL => vec![s("M14.5 5.5 L8 12 L14.5 18.5", Stroke(2.0))],
        ChevR => vec![s("M9.5 5.5 L16 12 L9.5 18.5", Stroke(2.0))],
        ChevUp => vec![s("M5.5 14.5 L12 8 L18.5 14.5", Stroke(2.0))],
        ChevDown => vec![s("M5.5 9.5 L12 16 L18.5 9.5", Stroke(2.0))],
        X => vec![s("M6.5 6.5 L17.5 17.5 M17.5 6.5 L6.5 17.5", Stroke(2.0))],
        Clock => vec![(circ(12.0, 12.0, 9.0), Stroke(1.7)), s("M12 7 V12 L15.5 14", Stroke(1.7))],
        Calendar => vec![(rrect(3.5, 5.0, 17.0, 15.5, 3.0), Stroke(1.7)), s("M3.5 10 H20.5 M8 3 V6.5 M16 3 V6.5", Stroke(1.7))],
        Laptop => vec![(rrect(5.0, 5.5, 14.0, 9.5, 1.6), Stroke(1.7)), s("M2.5 18.5 H21.5", Stroke(1.8))],
        Headphones => vec![s("M4.5 15 V12 A7.5 7.5 0 0 1 19.5 12 V15", Stroke(1.8)), (rrect(3.5, 14.0, 4.0, 6.5, 1.6), Fill), (rrect(16.5, 14.0, 4.0, 6.5, 1.6), Fill)],
        Keyboard => vec![
            (rrect(2.5, 6.5, 19.0, 11.0, 2.2), Stroke(1.6)),
            s("M6.5 10.5 H6.6 M10 10.5 H10.1 M13.5 10.5 H13.6 M17 10.5 H17.1", Stroke(2.0)),
            s("M8 14.2 H16", Stroke(1.7)),
        ],
        Mouse => vec![(rrect(7.0, 3.5, 10.0, 17.0, 5.0), Stroke(1.7)), s("M12 3.5 V9.5", Stroke(1.7))],
        Phone => vec![(rrect(7.0, 2.5, 10.0, 19.0, 2.5), Stroke(1.7)), s("M10.5 18.5 H13.5", Stroke(1.7))],
        Battery => vec![(rrect(2.5, 7.0, 17.0, 10.0, 2.5), Stroke(1.7)), s("M21.5 10.5 V13.5", Stroke(1.8)), (rrect(5.0, 9.5, 8.0, 5.0, 1.0), Fill)],
        Clipboard => vec![(rrect(5.0, 5.0, 14.0, 16.5, 2.5), Stroke(1.7)), (rrect(9.0, 2.8, 6.0, 4.0, 1.5), Stroke(1.6)), s("M8.5 12 H15.5 M8.5 15.5 H13", Stroke(1.6))],
        Copy => vec![(rrect(8.5, 8.5, 11.0, 11.0, 2.2), Stroke(1.7)), s("M15.5 8.5 V6.5 A2 2 0 0 0 13.5 4.5 H6.5 A2 2 0 0 0 4.5 6.5 V13.5 A2 2 0 0 0 6.5 15.5 H8.5", Stroke(1.7))],
        Folder => vec![s("M3.5 7.5 A2 2 0 0 1 5.5 5.5 H9.5 L11.5 8 H18.5 A2 2 0 0 1 20.5 10 V17.5 A2 2 0 0 1 18.5 19.5 H5.5 A2 2 0 0 1 3.5 17.5 Z", Stroke(1.7))],
        Image => vec![(rrect(3.5, 4.5, 17.0, 15.0, 2.5), Stroke(1.7)), (circ(9.0, 10.0, 1.6), Fill), s("M4 18 L9.5 13 L13 16 L15.5 13.5 L20 18", Stroke(1.7))],
        Star => {
            let mut d = String::new();
            for i in 0..10 {
                let r = if i % 2 == 0 { 9.6 } else { 4.2 };
                let a = (-90.0 + i as f32 * 36.0).to_radians();
                d += &format!("{}{} {} ", if i == 0 { "M" } else { "L" }, 12.0 + r * a.cos(), 12.8 + r * a.sin());
            }
            d += "Z";
            vec![(d, Both(1.3))]
        }
        Pin => vec![s("M12 21 C12 21 5.5 14.5 5.5 10 A6.5 6.5 0 0 1 18.5 10 C18.5 14.5 12 21 12 21 Z", Stroke(1.7)), (circ(12.0, 10.0, 2.3), Fill)],
        Search => vec![(circ(10.5, 10.5, 6.3), Stroke(1.8)), s("M15.2 15.2 L20.3 20.3", Stroke(2.0))],
        Location => vec![s("M20 4 L4 11 L11 13 L13 20 Z", Both(1.4))],
    }
}

/// Parse the absolute-only path syntax into a Direct2D path geometry (figures are FILLED so Fill works on open paths too).
fn build(f: &ID2D1Factory1, d: &str) -> Result<ID2D1PathGeometry> {
    unsafe {
        let g = f.CreatePathGeometry()?;
        let sink = g.Open()?;
        let t: Vec<&str> = d.split_whitespace().collect();
        let (mut i, mut open) = (0usize, false);
        let mut cmd = 'M';
        let (mut cx, mut cy) = (0.0f32, 0.0f32);
        let num = |i: &mut usize| -> f32 {
            let v = t.get(*i).and_then(|s| s.parse::<f32>().ok()).unwrap_or(0.0);
            *i += 1;
            v
        };
        while i < t.len() {
            let tok = t[i];
            if let Some(c) = tok.chars().next().filter(|c| c.is_ascii_alphabetic()) {
                cmd = c; // `glue` made every command letter a standalone token
                i += 1;
                if cmd == 'Z' {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    continue;
                }
            }
            match cmd {
                'M' => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                    }
                    (cx, cy) = (num(&mut i), num(&mut i));
                    sink.BeginFigure(pt(cx, cy), D2D1_FIGURE_BEGIN_FILLED);
                    open = true;
                    cmd = 'L';
                }
                'L' => {
                    (cx, cy) = (num(&mut i), num(&mut i));
                    sink.AddLine(pt(cx, cy));
                }
                'H' => {
                    cx = num(&mut i);
                    sink.AddLine(pt(cx, cy));
                }
                'V' => {
                    cy = num(&mut i);
                    sink.AddLine(pt(cx, cy));
                }
                'C' => {
                    let (a, b, c, d2, e, f2) = (num(&mut i), num(&mut i), num(&mut i), num(&mut i), num(&mut i), num(&mut i));
                    sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: pt(a, b), point2: pt(c, d2), point3: pt(e, f2) });
                    (cx, cy) = (e, f2);
                }
                'Q' => {
                    let (a, b, c, d2) = (num(&mut i), num(&mut i), num(&mut i), num(&mut i));
                    sink.AddQuadraticBezier(&D2D1_QUADRATIC_BEZIER_SEGMENT { point1: pt(a, b), point2: pt(c, d2) });
                    (cx, cy) = (c, d2);
                }
                'A' => {
                    let (rx, ry, rot, large, sweep, x, y) = (num(&mut i), num(&mut i), num(&mut i), num(&mut i), num(&mut i), num(&mut i), num(&mut i));
                    sink.AddArc(&D2D1_ARC_SEGMENT {
                        point: pt(x, y),
                        size: D2D_SIZE_F { width: rx, height: ry },
                        rotationAngle: rot,
                        sweepDirection: if sweep != 0.0 { D2D1_SWEEP_DIRECTION_CLOCKWISE } else { D2D1_SWEEP_DIRECTION_COUNTER_CLOCKWISE },
                        arcSize: if large != 0.0 { D2D1_ARC_SIZE_LARGE } else { D2D1_ARC_SIZE_SMALL },
                    });
                    (cx, cy) = (x, y);
                }
                _ => i += 1,
            }
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
        sink.Close()?;
        Ok(g.cast()?)
    }
}

/// Make "M12 3" style tokens standalone: "M 12 3".
fn glue(d: &str) -> String {
    let mut o = String::with_capacity(d.len() + 16);
    for ch in d.chars() {
        if ch.is_ascii_alphabetic() {
            o.push(' ');
            o.push(ch);
            o.push(' ');
        } else {
            o.push(ch);
        }
    }
    o
}
pub fn geometries(f: &ID2D1Factory1, g: Glyph) -> Result<Vec<(ID2D1PathGeometry, Paint)>> {
    shapes(g).into_iter().map(|(d, p)| Ok((build(f, &glue(&d))?, p))).collect()
}
