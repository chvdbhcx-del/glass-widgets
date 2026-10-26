//! Clock: S analog face; M analog + big digital time + date; L analog + up to 3 world clocks.
//! Port of src/widgets/clock.js/.css. The dial (ticks, numerals, text) is static; the three hands + the red cap are the overlay
//! (30 steps per second like the CSS `steps(1800)`), so the glass layer is re-rendered only once a minute (digital time).
//! Settings: `cfg.zones = [{"tz": "tokyo", "name": "Токио"}, ...]` (up to 3, ids from tz.rs; `name` optional).
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;

#[derive(Default)]
struct Clock {
    min: u64,
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "clock",
        name: ("Часы", "Clock"),
        desc: ("Аналоговые часы, время и мировые часы", "Analog clock, digital time and world clocks"),
        group: "clock",
        icon: Glyph::Clock,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::S,
        make: || Box::new(Clock::default()),
    });
}

/// (centre, diameter, 12 numerals?) of the dial in card px.
fn face(cx: &Cx) -> (Pt, f32, bool) {
    match cx.size {
        Size::S | Size::M => ((85.0, 85.0), 138.0, false),
        Size::L => ((178.0, 99.0), 166.0, true),
    }
}

impl Widget for Clock {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let ((mx, my), d, twelve) = face(cx);
        let u = d / 100.0;
        for i in 0..60 {
            let big = i % 5 == 0;
            let (sx, sy) = (i as f32 * 6.0).to_radians().sin_cos();
            let (r1, r2) = (47.0 * u, (if big { 42.5 } else { 44.8 }) * u);
            let (x1, y1, x2, y2) = (mx + sx * r1, my - sy * r1, mx + sx * r2, my - sy * r2);
            cx.line((x1, y1), (x2, y2), (if big { 1.1 } else { 0.55 }) * u, if big { DIM } else { FAINT });
        }
        let (nr, nsz) = if twelve { (34.0, 8.6) } else { (33.0, 11.0) };
        for h in 1..=12 {
            if !twelve && h % 3 != 0 {
                continue;
            }
            let (s, c) = (h as f32 * 30.0).to_radians().sin_cos();
            let (x, y) = (mx + nr * u * s, my - nr * u * c);
            cx.text(Rect::new(x - 20.0, y - 12.0, 40.0, 24.0), &h.to_string(), Style::new(nsz * u, 300).center(), TEXT);
        }
        match cx.size {
            Size::S => {}
            Size::M => {
                let n = cx.now;
                let x = 170.0;
                let w = 170.0;
                let mut y = 85.0 - 42.0;
                let wd = weekday_name(cx.lang, n.dow()).to_uppercase();
                cx.text(Rect::new(x, y, w, 16.0), &wd, Style::LABEL, RED);
                y += 16.0;
                cx.text(Rect::new(x, y, w, 50.0), &format!("{:02}:{:02}", n.h, n.mi), Style::new(46.0, 300).mono().track(-0.9), TEXT);
                y += 50.0;
                cx.text(Rect::new(x, y, w, 18.0), &format!("{} {}", n.d, month_genitive(cx.lang, n.mo)), Style::new(14.0, 500), DIM);
            }
            Size::L => zones(cx),
        }
        Ok(())
    }

    fn overlay(&mut self, cx: &mut Cx) -> Result<()> {
        let ((mx, my), d, _) = face(cx);
        let u = d / 100.0;
        let n = cx.now;
        let sec = n.sec as f32 + (n.ms as f32 * 0.03).floor() / 30.0; // 30 steps per second
        let min = n.mi as f32 + sec / 60.0;
        let hour = (n.h % 12) as f32 + min / 60.0;
        let hand = |deg: f32, tail: f32, tip: f32, wd: f32, c: Color| {
            let (s, k) = deg.to_radians().sin_cos();
            cx.line((mx - s * tail * u, my + k * tail * u), (mx + s * tip * u, my - k * tip * u), wd * u, c);
        };
        hand(hour * 30.0, 5.0, 23.0, 3.2, TEXT);
        hand(min * 6.0, 6.0, 37.0, 2.4, TEXT);
        hand(sec * 6.0, 12.0, 41.0, 1.0, RED);
        cx.circle((mx, my), 3.0 * u, RED);
        cx.circle_stroke((mx, my), 3.0 * u - 0.55, 1.1, Color::rgba(0.0, 0.0, 0.0, 0.30));
        Ok(())
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if ev.clicked(cx.bounds()) {
            cx.open_card_target("clock");
        }
        false
    }

    fn tick(&mut self, cx: &Cx) -> Tick {
        self.min = cx.now.minute_key();
        if cx.preview {
            Tick::Never
        } else {
            Tick::Fps(30)
        }
    }

    fn version(&self) -> u64 {
        self.min
    }
}

/// L: up to three world clocks under the dial: city + "Today, +3 hrs" on the left, the time (28 light) on the right.
fn zones(cx: &Cx) {
    let list: Vec<(&'static tz::Zone, Option<String>)> = match cx.cfg.get("zones").and_then(|z| z.as_array()) {
        Some(a) => a.iter().filter_map(|z| Some((tz::find(z.get("tz")?.as_str()?)?, z.get("name").and_then(|n| n.as_str()).map(str::to_string)))).take(3).collect(),
        None => ["moscow", "london", "new_york"].iter().filter_map(|i| tz::find(i)).map(|z| (z, None)).collect(),
    };
    let utc = cx.utc_secs();
    let local = tz::civil_from_unix(utc, cx.utc_offset_min());
    let n = list.len() as f32;
    for (k, (z, name)) in list.iter().enumerate() {
        let t = z.at(utc);
        let top = 340.0 - 44.0 * (n - k as f32);
        let row = Rect::new(16.0, top, 324.0, 44.0);
        cx.rect(Rect::new(row.x, row.y, row.w, 1.0), FILL);
        let diff = t.offset_min - local.offset_min;
        let a = diff.abs();
        let hrs = if a % 60 != 0 { format!("{}:{:02}", a / 60, a % 60) } else { format!("{}", a / 60) };
        let day = match t.day_number() - local.day_number() {
            0 => cx.t("Сегодня", "Today"),
            d if d > 0 => cx.t("Завтра", "Tomorrow"),
            _ => cx.t("Вчера", "Yesterday"),
        };
        let off = format!("{}, {}{} {}", day, if diff < 0 { '−' } else { '+' }, hrs, cx.t("ч", "HRS"));
        let city = name.clone().unwrap_or_else(|| cx.t(z.ru, z.en).to_string());
        cx.text(Rect::new(row.x, row.y + 5.0, 200.0, 20.0), &city, Style::new(15.0, 600).ellipsis(), TEXT);
        cx.text(Rect::new(row.x, row.y + 24.0, 200.0, 15.0), &off, Style::new(11.5, 400).ellipsis(), DIM);
        cx.text(Rect::new(row.right() - 110.0, row.y, 110.0, 44.0), &format!("{:02}:{:02}", t.h, t.mi), Style::new(28.0, 300).mono().right(), TEXT);
    }
}
