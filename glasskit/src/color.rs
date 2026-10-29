//! Straight (non-premultiplied) RGBA colour, the white-with-alpha text palette and the process-wide accent colour.
//! `Color::d2d` converts to the Direct2D colour; `accent()` / `set_accent` hold the accent that headers and highlights follow
//! (the default blue, or a colour taken from the wallpaper, see `accent.rs`).
use crate::gfx::col;
use std::sync::atomic::Ordering;
use windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F;
/// Straight (non-premultiplied) RGBA, 0..1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}
impl Color {
    pub const fn rgba(r: f32, g: f32, b: f32, a: f32) -> Color {
        Color { r, g, b, a }
    }
    pub const fn rgb8(r: u8, g: u8, b: u8) -> Color {
        Color { r: r as f32 / 255.0, g: g as f32 / 255.0, b: b as f32 / 255.0, a: 1.0 }
    }
    /// same colour, alpha replaced
    pub const fn alpha(self, a: f32) -> Color {
        Color { a, ..self }
    }
    /// same colour, alpha multiplied
    pub fn fade(self, k: f32) -> Color {
        Color { a: self.a * k, ..self }
    }
    pub fn mix(self, o: Color, t: f32) -> Color {
        let l = |a: f32, b: f32| a + (b - a) * t;
        Color { r: l(self.r, o.r), g: l(self.g, o.g), b: l(self.b, o.b), a: l(self.a, o.a) }
    }
    pub fn d2d(self) -> D2D1_COLOR_F {
        col(self.r, self.g, self.b, self.a)
    }
}
// palette (white text with alpha steps, like the Apple references)
pub const TEXT: Color = Color::rgba(1.0, 1.0, 1.0, 0.96);
pub const DIM: Color = Color::rgba(1.0, 1.0, 1.0, 0.70);
/// secondary text that must stay readable on bright glass: titles of cards, units next to a big number
pub const SUBTLE: Color = Color::rgba(1.0, 1.0, 1.0, 0.80);
pub const FAINT: Color = Color::rgba(1.0, 1.0, 1.0, 0.50);
/// subtle fills, ring tracks, bar tracks, separators
pub const FILL: Color = Color::rgba(1.0, 1.0, 1.0, 0.18);
pub const HAIR: Color = Color::rgba(1.0, 1.0, 1.0, 0.14);
pub const RED: Color = Color::rgb8(255, 69, 58);
pub const GREEN: Color = Color::rgb8(48, 209, 88);
pub const BLUE: Color = Color::rgb8(10, 132, 255);
pub const ORANGE: Color = Color::rgb8(255, 159, 10);
pub const YELLOW: Color = Color::rgb8(255, 214, 10);
pub const PINK: Color = Color::rgb8(255, 55, 95);
pub const TEAL: Color = Color::rgb8(64, 200, 224);
pub const PURPLE: Color = Color::rgb8(191, 90, 242);
/// The DEFAULT accent (`accentMode` "blue"). Widgets must call `accent()` instead: it follows the wallpaper in `accentMode` "wallpaper".
pub const ACCENT: Color = Color::rgb8(100, 190, 255); // lighter than BLUE: stays readable on grey, orange and pink glass
const ACCENT_RGB: [u8; 3] = [100, 190, 255];
static ACCENT_NOW: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(pack_rgb(ACCENT_RGB));
const fn pack_rgb(c: [u8; 3]) -> u32 {
    (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32
}
/// The accent colour of headers, links and highlights (calendar month, reminders title): `ACCENT`, or the colour taken from the wallpaper.
pub fn accent() -> Color {
    let v = ACCENT_NOW.load(Ordering::Relaxed);
    Color::rgb8((v >> 16) as u8, (v >> 8) as u8, v as u8)
}
/// Set the accent (`None` = the default blue); true when it changed (the cached card content has to be recorded again then).
pub fn set_accent(rgb: Option<[u8; 3]>) -> bool {
    ACCENT_NOW.swap(pack_rgb(rgb.unwrap_or(ACCENT_RGB)), Ordering::Relaxed) != pack_rgb(rgb.unwrap_or(ACCENT_RGB))
}
/// dark text on a light/cream fill (today circle, buttons)
pub const INK: Color = Color::rgba(20.0 / 255.0, 20.0 / 255.0, 24.0 / 255.0, 0.92);
pub const BLACK: Color = Color::rgba(0.0, 0.0, 0.0, 1.0);
pub const WHITE: Color = Color::rgba(1.0, 1.0, 1.0, 1.0);
