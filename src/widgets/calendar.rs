//! Calendar: S month grid with today in a cream circle; M big day number + month grid; L big month grid (month paging with
//! the chevrons that appear on hover) + a "Сегодня" strip. Port of src/widgets/calendar.js/.css. There is no event source yet,
//! so the strip shows sample rows in previews and "Нет событий" live (the reminders widget will feed it later).
//! Everything is static: `Tick::Never`; midnight is handled by the framework (the layer is re-recorded when the day changes).
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;

#[derive(Default)]
struct Calendar {
    /// months shown relative to the current one (L only)
    offset: i32,
    hover: bool,
    ver: u64,
}

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "calendar",
        name: ("Календарь", "Calendar"),
        desc: ("Месяц, сегодняшнее число и напоминания дня", "The month, today and your reminders"),
        group: "calendar",
        icon: Glyph::Calendar,
        sizes: &[Size::S, Size::M, Size::L],
        default_size: Size::S,
        make: || Box::new(Calendar::default()),
    });
}

/// Metrics of one month block (logical px).
struct Metrics {
    head: f32,
    dow_h: f32,
    dow_size: f32,
    row: f32,
    num_size: f32,
    /// today circle radius
    circ: f32,
    /// vertical centre of the number inside its row
    num_cy: f32,
    title_size: f32,
}
const MS: Metrics = Metrics { head: 22.0, dow_h: 15.0, dow_size: 10.0, row: 17.0, num_size: 11.5, circ: 8.5, num_cy: 8.5, title_size: 12.0 };
const ML: Metrics = Metrics { head: 30.0, dow_h: 20.0, dow_size: 12.0, row: 28.0, num_size: 15.0, circ: 12.0, num_cy: 12.5, title_size: 13.0 };

fn view_month(now: &Now, offset: i32) -> (i32, u32) {
    let m = now.y * 12 + now.mo as i32 - 1 + offset;
    (m.div_euclid(12), m.rem_euclid(12) as u32 + 1)
}

/// Month title, weekday initials and the day grid in a block `w` wide whose top-left is (x, y).
fn month_block(cx: &Cx, x: f32, y: f32, w: f32, m: &Metrics, view: (i32, u32), title_w: f32) {
    let now = cx.now;
    let (vy, vm) = view;
    let mut title = month_name(cx.lang, vm).to_uppercase();
    if vy != now.y {
        title += &format!(" {vy}");
    }
    cx.text(Rect::new(x, y, title_w, 18.0), &title, Style::LABEL.size(m.title_size), accent());
    let sun_first = cx.lang == Lang::En;
    let initials = if sun_first { ["S", "M", "T", "W", "T", "F", "S"] } else { ["П", "В", "С", "Ч", "П", "С", "В"] };
    let cw = w / 7.0;
    let dow_y = y + m.head;
    for (i, d) in initials.iter().enumerate() {
        cx.text(Rect::new(x + i as f32 * cw, dow_y, cw, m.dow_h), d, Style::new(m.dow_size, 600).center(), DIM);
    }
    let lead = (tz::dow_mon0(vy, vm, 1) + if sun_first { 1 } else { 0 }) % 7;
    let grid_y = dow_y + m.dow_h;
    let current = (vy, vm) == (now.y, now.mo);
    for day in 1..=tz::days_in(vy, vm) {
        let idx = lead + day - 1;
        let (c, r) = (idx % 7, idx / 7);
        let (ccx, ccy) = (x + c as f32 * cw + cw / 2.0, grid_y + r as f32 * m.row + m.num_cy);
        let cell = Rect::new(ccx - cw / 2.0, ccy - m.row / 2.0, cw, m.row);
        let num = day.to_string();
        let st = Style::new(m.num_size, 500).center();
        if current && day == now.d {
            cx.circle((ccx, ccy), m.circ, TEXT);
            cx.text(cell, &num, st.weight(700), INK);
        } else {
            let past = (vy, vm, day) < (now.y, now.mo, now.d);
            cx.text(cell, &num, st, if past { DIM } else { TEXT });
        }
    }
}

impl Calendar {
    /// L header chevrons (prev, next) and the clickable month name.
    fn nav_rects() -> (Rect, Rect, Rect) {
        (Rect::new(292.0, 14.0, 24.0, 24.0), Rect::new(316.0, 14.0, 24.0, 24.0), Rect::new(16.0, 14.0, 200.0, 24.0))
    }
}

impl Widget for Calendar {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        let now = cx.now;
        match cx.size {
            Size::S => month_block(cx, 16.0, 16.0, 138.0, &MS, (now.y, now.mo), 138.0),
            Size::M => {
                let left = Rect::new(16.0, 16.0, 142.6, 138.0);
                let wd = weekday_name(cx.lang, now.dow()).to_uppercase();
                cx.text(Rect::new(left.x, left.y, left.w, 16.0), &wd, Style::LABEL.ellipsis(), accent());
                cx.text(Rect::new(left.x - 2.0, left.y + 17.0, left.w + 8.0, 76.0), &now.d.to_string(), Style::new(76.0, 300).mono().track(-2.3), TEXT);
                let line = if cx.preview { cx.t("Сегодня 2 напоминания", "2 reminders today") } else { cx.t("Нет событий", "No events") };
                cx.text(Rect::new(left.x, left.bottom() - 16.0, left.w, 16.0), line, Style::new(12.0, 400), DIM);
                month_block(cx, 172.6, 16.0, 167.4, &MS, (now.y, now.mo), 167.4);
            }
            Size::L => {
                let view = view_month(&now, self.offset);
                month_block(cx, 16.0, 16.0, 324.0, &ML, view, 220.0);
                if self.hover {
                    let (p, n, _) = Self::nav_rects();
                    cx.icon(Glyph::ChevL, p.inset(5.0), DIM);
                    cx.icon(Glyph::ChevR, n.inset(5.0), DIM);
                }
                // "Today" strip, anchored to the bottom: separator, header, three rows of 22
                cx.rect(Rect::new(16.0, 248.0, 324.0, 0.6), FILL);
                cx.text(Rect::new(16.0, 255.0, 324.0, 15.0), &cx.t("СЕГОДНЯ", "TODAY"), Style::LABEL, accent());
                if cx.preview {
                    let rows = [("Позвонить маме", "18:30", true), ("Купить билеты", "", false), ("Отправить отчёт", "10:00", false)];
                    for (i, (t, tm, over)) in rows.iter().enumerate() {
                        let y = 274.0 + i as f32 * 22.0;
                        cx.circle_stroke((21.5, y + 11.0), 4.9, 1.5, FAINT);
                        cx.text(Rect::new(35.0, y, 240.0, 22.0), t, Style::BODY.ellipsis(), TEXT);
                        cx.text(Rect::new(280.0, y, 60.0, 22.0), tm, Style::CAPTION.mono().right(), if *over { RED } else { DIM });
                    }
                } else {
                    cx.text(Rect::new(16.0, 274.0, 324.0, 22.0), cx.t("Нет событий", "No events"), Style::new(12.0, 400), FAINT);
                }
            }
        }
        Ok(())
    }

    fn tick(&mut self, _cx: &Cx) -> Tick {
        Tick::Never
    }

    fn mouse(&mut self, ev: &MouseEv, cx: &mut Cx) -> bool {
        if cx.size != Size::L {
            return false;
        }
        let mut changed = false;
        let over = ev.kind != MouseKind::Leave;
        if over != self.hover {
            self.hover = over;
            changed = true;
        }
        let (p, n, name) = Self::nav_rects();
        let step = if ev.clicked(p) {
            -1
        } else if ev.clicked(n) {
            1
        } else if ev.clicked(name) && self.offset != 0 {
            -self.offset
        } else {
            0
        };
        if step != 0 {
            self.offset += step;
            changed = true;
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
