//! Developer-only widget `_dev` (hidden from the sheet and the gallery): a gallery of every drawing primitive of `Cx`.
//!   --cards "_dev:l@0,0"                      primitives (text styles, rings, bars, sparkline, clip, bitmap, rotation)
//!   --cards '_dev:l@0,0{"mode":"icons"}'      all built-in glyphs in `Glyph::ALL` order, 8 per row
use crate::registry::{Registry, WidgetDef};
use crate::widget::*;

struct Dev;

pub fn register(r: &mut Registry) {
    r.register(WidgetDef {
        type_id: "_dev",
        name: ("Примитивы", "Primitives"),
        desc: ("Отладочная галерея примитивов", "Debug gallery of drawing primitives"),
        group: "dev",
        icon: Glyph::Star,
        sizes: &[Size::L],
        default_size: Size::L,
        make: || Box::new(Dev),
    });
}

impl Widget for Dev {
    fn draw(&mut self, cx: &mut Cx) -> Result<()> {
        if cx.cfg_str("mode") == Some("icons") {
            let r = cx.content();
            for (i, g) in Glyph::ALL.iter().enumerate() {
                let (c, row) = (i % 8, i / 8);
                let cell = Rect::new(r.x + c as f32 * 40.5, r.y + row as f32 * 44.0, 40.5, 44.0);
                cx.icon(*g, cell.centered(26.0, 26.0), TEXT);
                cx.text(Rect::new(cell.x, cell.y + 30.0, cell.w, 12.0), &i.to_string(), Style::new(8.0, 400).center(), FAINT);
            }
            return Ok(());
        }
        // text styles
        let mut y = 14.0;
        for (st, s, c) in [(Style::TITLE, "TITLE 13 semibold Привет", TEXT), (Style::BODY, "BODY 13 regular Привет", TEXT), (Style::CAPTION, "CAPTION 11 regular Привет", DIM), (Style::LABEL, "LABEL 12 BOLD ПЯТНИЦА", accent())] {
            cx.text(Rect::new(16.0, y, 324.0, 18.0), s, st, c);
            y += 18.0;
        }
        cx.text(Rect::new(16.0, y, 160.0, 48.0), "12:34", Style::BIG.mono(), TEXT);
        cx.text(Rect::new(150.0, y, 190.0, 48.0), "Long text that must be cut with an ellipsis here", Style::BODY.ellipsis(), DIM);
        y += 52.0;
        // rings
        for (i, (f, c, g)) in [(0.72, GREEN, Glyph::Laptop), (0.15, RED, Glyph::Phone), (1.0, ORANGE, Glyph::Headphones), (0.4, BLUE, Glyph::Mouse)].iter().enumerate() {
            let ctr = (16.0 + 26.0 + i as f32 * 62.0, y + 28.0);
            cx.ring(ctr, 22.0, 5.0, *f, *c);
            cx.icon(*g, Rect::new(ctr.0 - 9.0, ctr.1 - 9.0, 18.0, 18.0), TEXT);
        }
        cx.arc((300.0, y + 28.0), 20.0, 4.0, 200.0, -140.0, TEAL);
        y += 62.0;
        // bars + sparkline
        cx.bar(Rect::new(16.0, y, 150.0, 6.0), 0.62, GREEN);
        cx.bar(Rect::new(16.0, y + 12.0, 150.0, 6.0), 0.18, RED);
        cx.sparkline(Rect::new(184.0, y - 4.0, 156.0, 34.0), &[3.0, 5.0, 4.0, 8.0, 6.0, 9.0, 7.0, 12.0, 10.0, 14.0, 9.0, 11.0], BLUE);
        y += 36.0;
        // clip + gradient + generated bitmap + rotation
        let tile = Rect::new(16.0, y, 70.0, 70.0);
        cx.with_clip_round(tile, 16.0, |cx| {
            cx.gradient_rrect(tile, 0.0, ORANGE, PINK, 135.0);
            cx.circle((tile.right(), tile.bottom()), 40.0, WHITE.alpha(0.35));
        });
        let px: Vec<u8> = (0..64 * 32).flat_map(|i| if ((i % 64) / 8 + (i / 64) / 8) % 2 == 0 { [200u8, 120, 40, 255] } else { [90, 60, 20, 255] }).collect();
        if let Ok(bmp) = cx.gfx.from_bgra(64, 32, &px) {
            cx.bitmap_round(Rect::new(98.0, y, 70.0, 70.0), &bmp, Fit::Cover, 12.0);
            cx.bitmap(Rect::new(180.0, y, 70.0, 34.0), &bmp, Fit::Contain);
        }
        cx.rotated((295.0, y + 35.0), 25.0, |cx| cx.rrect(Rect::new(265.0, y + 15.0, 60.0, 40.0), 10.0, TEAL.alpha(0.8)));
        cx.circle((190.0, y + 52.0), 8.0, YELLOW);
        cx.pill(Rect::new(206.0, y + 44.0, 44.0, 16.0), FILL);
        Ok(())
    }
    fn tick(&mut self, _cx: &Cx) -> Tick {
        Tick::Never
    }
}
