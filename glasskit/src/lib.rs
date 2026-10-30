//! glasskit: the reusable native-Windows UI toolkit behind Glass Widgets (`mswidgets`), extracted so other apps (Pill) can share it.
//!
//! * `gfx`        the D3D11 + Direct2D 1.1 + DirectWrite + WIC device (`Gfx`, WARP fallback) and small value helpers
//! * `geom`       `Rect` / `Pt`;  `color` `Color`, the palette and the accent;  `text` `Style` and `Res` (the text / glyph / gradient cache)
//! * `fonts`      `FontSet`, bundled fonts for DirectWrite;  `icons` the vector glyphs and a registry for your own;  `lang` `Lang`
//! * `motion`     pure animation state machines (springs, ramps, glides, morphs) and `HoverAnim` / `GlideAnim` on top of them
//! * `textfield`  the text-field model (caret, selection, word wrap);  `menu` the context-menu model
//! * `jobs`       background jobs (`spawn`, `Pending`, `Waker`, the worker pool)
//! * `monitors`, `accent`, `tz`  display enumeration and choice, the wallpaper accent, calendar arithmetic and world time zones
//!
//! Everything but `gfx`, `text::Res`, `icons::geometries`, `fonts`, `jobs` and the Win32 calls of `monitors` / `accent` is pure and unit-tested
//! without a GPU. The crate has no dependency on the widgets app.
pub mod accent;
pub mod canvas;
pub mod color;
pub mod controls;
pub mod ease;
pub mod edit;
pub mod fonts;
pub mod geom;
pub mod gfx;
pub mod icons;
pub mod input;
pub mod jobs;
pub mod lang;
pub mod markdown;
pub mod menu;
pub mod monitors;
pub mod motion;
pub mod richtext;
pub mod scroll;
pub mod svgpath;
pub mod testkit;
pub mod text;
pub mod textfield;
pub mod theme;
pub mod tz;

pub use canvas::Canvas;
pub use color::Color;
pub use geom::{Pt, Rect};
pub use gfx::Gfx;
pub use icons::Glyph;
pub use jobs::{Pending, Waker};
pub use lang::Lang;
pub use text::{Align, Res, Style, VAlign};