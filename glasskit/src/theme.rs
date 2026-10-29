//! `Theme`: every colour, radius and size a control of the kit uses, so one control set serves Pill (dark and light), the Settings window and
//! the glass widgets. The numbers are the CSS custom properties of `theme.css` / `settings.css` of the web UI that Pill replaces.
//!
//! * [`Theme::pill_dark`] / [`Theme::pill_light`]  `:root` and `:root[data-theme="light"]` of `theme.css`.
//! * [`Theme::settings_dark`]                      the Settings window's own tokens (`settings.css`: 44x24 switch, 8 px radii, #111013).
//! * [`Theme::glass`]                              white-on-glass, for the widgets' dark appearance.
//!
//! Controls take `&Theme` in `draw`; nothing is global, so two windows can be themed differently. Colours that depend on state
//! (hover, press) are separate fields so a control mixes them with its animation progress instead of picking one.
use crate::color::Color;
use crate::text::Style;

/// Sizes of the controls (logical px). Pill and Settings differ in a few of them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// cards, tiles
    pub radius: f32,
    pub radius_sm: f32,
    pub btn_h: f32,
    pub btn_h_sm: f32,
    pub btn_r: f32,
    pub btn_pad_x: f32,
    pub field_h: f32,
    pub field_r: f32,
    pub field_pad_x: f32,
    /// segmented control: track padding, segment height, segment corner radius, track corner radius, segment horizontal padding
    pub seg_pad: f32,
    pub seg_h: f32,
    pub seg_r: f32,
    pub seg_track_r: f32,
    pub seg_pad_x: f32,
    /// switch: width, height, knob diameter, knob inset, knob growth while pressed
    pub sw_w: f32,
    pub sw_h: f32,
    pub sw_knob: f32,
    pub sw_inset: f32,
    pub sw_press_grow: f32,
    /// slider: hit height, track thickness, thumb diameter
    pub sl_h: f32,
    pub sl_track: f32,
    pub sl_thumb: f32,
    /// checkbox / radio box size and checkbox corner radius
    pub box_size: f32,
    pub box_r: f32,
    /// chip / badge heights
    pub chip_h: f32,
    pub badge_h: f32,
}

/// Colours and metrics. All colours are straight RGBA.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    pub m: Metrics,
    // surfaces
    pub bg: Color,
    pub card: Color,
    pub card_hover: Color,
    pub raised: Color,
    pub raised2: Color,
    pub field: Color,
    pub hover: Color,
    pub border: Color,
    pub line: Color,
    pub track: Color,
    pub switch_off: Color,
    // control fills
    pub btn: Color,
    pub btn_hover: Color,
    pub seg_bg: Color,
    pub seg_on: Color,
    pub seg_hover_text: Color,
    // text
    pub text: Color,
    pub text2: Color,
    pub dim: Color,
    pub faint: Color,
    pub icon: Color,
    pub on_accent: Color,
    // accents
    pub accent: Color,
    pub accent_hover: Color,
    pub accent_soft: Color,
    pub slider_fill: Color,
    pub thumb: Color,
    pub green: Color,
    pub green_bg: Color,
    pub red: Color,
    pub red_bg: Color,
    pub amber: Color,
    pub amber_bg: Color,
    pub link: Color,
    // floating things
    pub shadow: Color,
    pub tip_bg: Color,
    pub tip_text: Color,
    pub scrollbar: Color,
    pub scrollbar_hover: Color,
    pub menu_bg: Color,
    pub menu_hover: Color,
}

const fn c(hex: u32) -> Color {
    Color::rgb8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}
const fn ca(hex: u32, a: f32) -> Color {
    Color::rgba(((hex >> 16) & 255) as f32 / 255.0, ((hex >> 8) & 255) as f32 / 255.0, (hex & 255) as f32 / 255.0, a)
}

impl Metrics {
    /// `theme.css`: 32 px buttons with 12 px radius, 36 px fields, 4 px padded segmented control, 40 x 24 switch with an 18 px knob.
    pub const PILL: Metrics = Metrics {
        radius: 14.0,
        radius_sm: 10.0,
        btn_h: 32.0,
        btn_h_sm: 28.0,
        btn_r: 12.0,
        btn_pad_x: 16.0,
        field_h: 36.0,
        field_r: 12.0,
        field_pad_x: 12.0,
        seg_pad: 4.0,
        seg_h: 24.0,
        seg_r: 8.0,
        seg_track_r: 12.0,
        seg_pad_x: 12.0,
        sw_w: 40.0,
        sw_h: 24.0,
        sw_knob: 18.0,
        sw_inset: 3.0,
        sw_press_grow: 4.0,
        sl_h: 16.0,
        sl_track: 4.0,
        sl_thumb: 12.0,
        box_size: 18.0,
        box_r: 5.0,
        chip_h: 24.0,
        badge_h: 16.0,
    };

    /// `settings.css`: 32 px buttons with 8 px radius, 3 px padded 26 px segments, 44 x 24 switch with a 20 px knob, 14 px slider thumb.
    pub const SETTINGS: Metrics = Metrics {
        radius: 12.0,
        radius_sm: 8.0,
        btn_h: 32.0,
        btn_h_sm: 28.0,
        btn_r: 8.0,
        btn_pad_x: 16.0,
        field_h: 32.0,
        field_r: 8.0,
        field_pad_x: 12.0,
        seg_pad: 3.0,
        seg_h: 26.0,
        seg_r: 6.0,
        seg_track_r: 8.0,
        seg_pad_x: 14.0,
        sw_w: 44.0,
        sw_h: 24.0,
        sw_knob: 20.0,
        sw_inset: 2.0,
        sw_press_grow: 4.0,
        sl_h: 16.0,
        sl_track: 4.0,
        sl_thumb: 14.0,
        box_size: 18.0,
        box_r: 5.0,
        chip_h: 30.0,
        badge_h: 16.0,
    };
}

impl Theme {
    /// Pill, dark (`:root` of `theme.css`: pure black body, periwinkle accent).
    pub fn pill_dark() -> Theme {
        let accent = c(0x8f96ff);
        Theme {
            dark: true,
            m: Metrics::PILL,
            bg: c(0x000000),
            card: c(0x0f0f0f),
            card_hover: c(0x181818),
            raised: c(0x1c1c1c),
            raised2: c(0x292929),
            field: c(0x0f0f0f),
            hover: ca(0xffffff, 0.07),
            border: ca(0xffffff, 0.10),
            line: ca(0xffffff, 0.07),
            track: c(0x333333),
            switch_off: c(0x2a2a2a),
            btn: c(0x0f0f0f),
            btn_hover: c(0x181818),
            seg_bg: c(0x0f0f0f),
            seg_on: c(0x292929),
            seg_hover_text: c(0xffffff),
            text: c(0xffffff),
            text2: c(0xababab),
            dim: c(0x7c7c7c),
            faint: c(0x616161),
            icon: c(0x878787),
            on_accent: c(0x0b0b14),
            accent,
            accent_hover: brighten(accent, 1.08),
            accent_soft: ca(0x8f96ff, 0.16),
            slider_fill: accent,
            thumb: c(0xffffff),
            green: c(0x58ddab),
            green_bg: c(0x002016),
            red: c(0xff453a),
            red_bg: ca(0xff453a, 0.16),
            amber: c(0xffb340),
            amber_bg: ca(0xffb340, 0.16),
            link: c(0xabbaf3),
            shadow: Color::rgba(0.0, 0.0, 0.0, 0.45),
            tip_bg: c(0x262626),
            tip_text: c(0xffffff),
            scrollbar: c(0x3a3a3a),
            scrollbar_hover: c(0x555555),
            menu_bg: c(0x161616),
            menu_hover: c(0x262626),
        }
    }

    /// Pill, light (`:root[data-theme="light"]`).
    pub fn pill_light() -> Theme {
        let accent = c(0x5b63e6);
        Theme {
            dark: false,
            m: Metrics::PILL,
            bg: c(0xf6f6f6),
            card: c(0xe9e9e9),
            card_hover: c(0xe0e0e0),
            raised: c(0xdedede),
            raised2: c(0xd2d2d2),
            field: c(0xe9e9e9),
            hover: ca(0x000000, 0.06),
            border: ca(0x000000, 0.12),
            line: ca(0x000000, 0.08),
            track: c(0xc9c9c9),
            switch_off: c(0xc9c9c9),
            btn: c(0xe9e9e9),
            btn_hover: c(0xe0e0e0),
            seg_bg: c(0xe9e9e9),
            seg_on: c(0xd2d2d2),
            seg_hover_text: c(0x161616),
            text: c(0x161616),
            text2: c(0x555555),
            dim: c(0x7a7a7a),
            faint: c(0x8e8e8e),
            icon: c(0x6c6c6c),
            on_accent: c(0xffffff),
            accent,
            accent_hover: brighten(accent, 1.08),
            accent_soft: ca(0x5b63e6, 0.14),
            slider_fill: accent,
            thumb: c(0xffffff),
            green: c(0x11805a),
            green_bg: c(0xcdeedf),
            red: c(0xd92d20),
            red_bg: ca(0xd92d20, 0.12),
            amber: c(0xb26a00),
            amber_bg: ca(0xb26a00, 0.14),
            link: c(0x4048c9),
            shadow: Color::rgba(0.0, 0.0, 0.0, 0.22),
            tip_bg: c(0x2b2b2b),
            tip_text: c(0xffffff),
            scrollbar: c(0xbdbdbd),
            scrollbar_hover: c(0x9a9a9a),
            menu_bg: c(0xf2f2f2),
            menu_hover: c(0xe2e2e2),
        }
    }

    /// The Settings window (`settings.css`).
    pub fn settings_dark() -> Theme {
        let accent = c(0x90a0ff);
        Theme {
            dark: true,
            m: Metrics::SETTINGS,
            bg: c(0x111013),
            card: c(0x1a191c),
            card_hover: c(0x201f22),
            raised: c(0x2b2a2d),
            raised2: c(0x4b4a4d),
            field: c(0x101010),
            hover: ca(0xffffff, 0.07),
            border: c(0x26252a),
            line: c(0x272629),
            track: c(0x3d3c3f),
            switch_off: c(0x3d3c3f),
            btn: c(0x2d2c2f),
            btn_hover: c(0x39383c),
            seg_bg: c(0x2b2a2d),
            seg_on: c(0x4b4a4d),
            seg_hover_text: c(0xffffff),
            text: c(0xffffff),
            text2: c(0xcfcfd2),
            dim: c(0x8e8c93),
            faint: c(0x6f6d72),
            icon: c(0xa0a0a8),
            on_accent: c(0x0b0b14),
            accent,
            accent_hover: c(0xa3b0ff),
            accent_soft: c(0x32354b),
            slider_fill: c(0x929eff),
            thumb: c(0x8fa1ff),
            green: c(0x86d3bc),
            green_bg: c(0x26443d),
            red: c(0xff6b6b),
            red_bg: ca(0xff6b6b, 0.14),
            amber: c(0xc8b46a),
            amber_bg: ca(0xc8b46a, 0.14),
            link: c(0xabbaf3),
            shadow: Color::rgba(0.0, 0.0, 0.0, 0.5),
            tip_bg: c(0x39383c),
            tip_text: c(0xffffff),
            scrollbar: c(0x3d3c3f),
            scrollbar_hover: c(0x57565a),
            menu_bg: c(0x1b1a1d),
            menu_hover: c(0x2b2a2d),
        }
    }

    /// White-on-glass for the widgets' dark appearance: translucent fills over a blurred backdrop, the system-blue accent.
    pub fn glass() -> Theme {
        let accent = crate::color::ACCENT;
        let w = |a: f32| Color::rgba(1.0, 1.0, 1.0, a);
        Theme {
            dark: true,
            m: Metrics::PILL,
            bg: Color::rgba(0.0, 0.0, 0.0, 0.0),
            card: w(0.10),
            card_hover: w(0.16),
            raised: w(0.14),
            raised2: w(0.22),
            field: w(0.10),
            hover: w(0.10),
            border: w(0.14),
            line: w(0.14),
            track: w(0.18),
            switch_off: w(0.20),
            btn: w(0.16),
            btn_hover: w(0.24),
            seg_bg: w(0.12),
            seg_on: w(0.26),
            seg_hover_text: w(0.96),
            text: crate::color::TEXT,
            text2: crate::color::SUBTLE,
            dim: crate::color::DIM,
            faint: crate::color::FAINT,
            icon: crate::color::DIM,
            on_accent: crate::color::INK,
            accent,
            accent_hover: brighten(accent, 1.08),
            accent_soft: accent.alpha(0.22),
            slider_fill: accent,
            thumb: crate::color::WHITE,
            green: crate::color::GREEN,
            green_bg: crate::color::GREEN.alpha(0.2),
            red: crate::color::RED,
            red_bg: crate::color::RED.alpha(0.2),
            amber: crate::color::ORANGE,
            amber_bg: crate::color::ORANGE.alpha(0.2),
            link: accent,
            shadow: Color::rgba(0.0, 0.0, 0.0, 0.35),
            tip_bg: Color::rgba(0.1, 0.1, 0.12, 0.92),
            tip_text: crate::color::WHITE,
            scrollbar: w(0.30),
            scrollbar_hover: w(0.45),
            menu_bg: Color::rgba(0.12, 0.12, 0.14, 0.94),
            menu_hover: w(0.14),
        }
    }

    /// The same theme with another accent (the wallpaper accent, a user choice). Derived colours follow.
    pub fn with_accent(mut self, accent: Color) -> Theme {
        self.accent = accent;
        self.accent_hover = brighten(accent, 1.08);
        self.accent_soft = accent.alpha(self.accent_soft.a);
        self.slider_fill = accent;
        self
    }

    // ------------------------------------------------------------ type scale (`--fs-*`, `--fw-*`)

    /// 10 px caps label (`--fs-2xs`, semibold, tracked).
    pub fn label_caps(&self) -> Style {
        Style::new(10.0, 600).track(0.6)
    }
    /// 11 px caption / meta
    pub fn caption(&self) -> Style {
        Style::new(11.0, 500)
    }
    /// 12 px chrome label (small buttons, tabs, segments)
    pub fn small(&self, weight: u16) -> Style {
        Style::new(12.0, weight)
    }
    /// 13 px body (rows, inputs)
    pub fn body(&self) -> Style {
        Style::new(13.0, 400)
    }
    /// 14 px card titles
    pub fn base(&self) -> Style {
        Style::new(14.0, 600)
    }
    /// 16 px track title
    pub fn large(&self) -> Style {
        Style::new(16.0, 600)
    }
}

/// `filter: brightness(k)`: channels scaled, clamped.
pub fn brighten(c: Color, k: f32) -> Color {
    Color::rgba((c.r * k).min(1.0), (c.g * k).min(1.0), (c.b * k).min(1.0), c.a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lum(c: Color) -> f32 {
        0.2126 * c.r + 0.7152 * c.g + 0.0722 * c.b
    }

    /// WCAG contrast ratio of two opaque colours.
    fn contrast(a: Color, b: Color) -> f32 {
        let f = |c: f32| if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) };
        let l = |c: Color| 0.2126 * f(c.r) + 0.7152 * f(c.g) + 0.0722 * f(c.b);
        let (x, y) = (l(a), l(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    #[test]
    fn dark_and_light_are_opposites() {
        let (d, l) = (Theme::pill_dark(), Theme::pill_light());
        assert!(d.dark && !l.dark);
        assert!(lum(d.bg) < 0.05 && lum(l.bg) > 0.9);
        assert!(lum(d.text) > lum(d.dim) && lum(l.text) < lum(l.dim));
    }

    #[test]
    fn text_and_primary_buttons_are_readable() {
        for t in [Theme::pill_dark(), Theme::pill_light(), Theme::settings_dark()] {
            assert!(contrast(t.text, t.bg) >= 7.0, "body text on the surface");
            assert!(contrast(t.text, t.card) >= 7.0, "text on a card");
            assert!(contrast(t.on_accent, t.accent) >= 4.5, "primary button label");
            assert!(contrast(t.dim, t.bg) >= 3.0, "captions");
        }
    }

    #[test]
    fn tokens_match_the_css() {
        let d = Theme::pill_dark();
        assert_eq!(d.accent, Color::rgb8(0x8f, 0x96, 0xff));
        assert_eq!((d.m.btn_h, d.m.btn_r, d.m.sw_w, d.m.sw_knob), (32.0, 12.0, 40.0, 18.0));
        let s = Theme::settings_dark();
        assert_eq!((s.m.sw_w, s.m.sw_knob, s.m.btn_r, s.m.seg_h), (44.0, 20.0, 8.0, 26.0));
        assert_eq!(s.btn, Color::rgb8(0x2d, 0x2c, 0x2f));
        assert!((d.hover.a - 0.07).abs() < 1e-6);
        assert_eq!(Theme::pill_light().accent, Color::rgb8(0x5b, 0x63, 0xe6));
    }

    #[test]
    fn accent_override_derives_hover_and_soft() {
        let t = Theme::pill_dark().with_accent(Color::rgb8(255, 0, 0));
        assert_eq!(t.accent, Color::rgb8(255, 0, 0));
        assert_eq!(t.slider_fill, t.accent);
        assert!((t.accent_soft.a - 0.16).abs() < 1e-6 && t.accent_soft.r == 1.0);
        assert!(brighten(Color::rgb8(100, 100, 100), 1.08).r > 100.0 / 255.0);
        assert_eq!(brighten(Color::rgb8(250, 250, 250), 1.5).r, 1.0);
    }
}
