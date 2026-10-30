//! SVG path data, a tiny SVG element reader and a Direct2D painter for them: the vector half of Pill's icons and pet art.
//!
//! * [`Path`]       neutral outline (`Vec<Seg>`, absolute coordinates, lines and cubics only) parsed from path data by [`Path::parse`].
//!   All of `M m L l H h V v C c S s Q q T t A a Z z` are understood, with implicit repeats and the compact number forms icon tools write
//!   (`1.5.5`, `10-5`, `1e-2`, arc flags without separators such as `a1 1 0 011 1`). Quadratics are promoted to cubics and arcs are converted
//!   to cubics (SVG implementation notes F.6, radii scaled up when too small), so every later stage handles two curve kinds only.
//! * [`Affine`]     2x3 matrix with the SVG `transform` attribute grammar ([`Affine::parse`]).
//! * [`build_geometry`] / [`GeomCache`]  `ID2D1PathGeometry` of a path under a transform, cached by (source, transform, fill mode).
//! * [`Svg`]        a reader for the SVG subset icons and small illustrations use, and [`Svg::draw`] to paint it into a rectangle.
//!
//! ```ignore
//! let cache = GeomCache::new();
//! // Pill's icons.js keeps INNER markup; the wrapper attributes come separately
//! let icon = Svg::icon(r#"<path d="M20 6 9 17l-5-5"/>"#, 1.7);
//! icon.draw(&cv, Rect::new(8.0, 8.0, 18.0, 18.0), theme.text, &cache);
//! // a standalone document
//! let art = Svg::parse(r#"<svg viewBox="0 0 48 48"><circle cx="24" cy="24" r="20" fill="#ff7aa2"/></svg>"#);
//! art.draw(&cv, dest, theme.text, &cache);
//! ```
//!
//! # Supported SVG subset
//! Elements `svg` (root; a nested one is treated as `g`), `g`, `path`, `circle`, `ellipse`, `rect` (`rx`/`ry`), `line`, `polyline`, `polygon`.
//! Attributes `transform`, `fill`, `stroke`, `stroke-width`, `stroke-linecap`, `stroke-linejoin`, `stroke-miterlimit`, `stroke-dasharray`,
//! `stroke-dashoffset`, `fill-rule` (`nonzero`/`evenodd`), `opacity`, `fill-opacity`, `stroke-opacity`, `display="none"`, and a `style="a:b;c:d"`
//! declaration list of those same properties. Colours: `#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`, `rgb()`, `rgba()`, `white`, `black`, `red`,
//! `blue`, `gray`/`grey`, `none`, `transparent`, `currentColor` (the `tint` given to [`Svg::draw`]). Paint, stroke and fill-rule inherit from
//! `g`; `opacity` and `transform` do not, and group opacity is a real group (children composite first). The root `viewBox` is fitted
//! `xMidYMid meet` into the destination and clipped to it.
//!
//! # Not supported (skipped, and listed by [`Svg::warnings`])
//! CSS (`<style>`, `class` selectors; the `class` attribute is ignored), gradients and patterns (`url(...)` paints draw nothing), `defs`, `use`,
//! `symbol`, `clipPath`, `mask`, `filter`, `text`, `image`, percentage lengths, `preserveAspectRatio` other than the default. Unknown elements
//! are skipped together with their children.
//!
//! Stroke widths are in user units like SVG: the baked transform's uniform scale (`sqrt|det|`) is applied, so a 1.7 wide line in a 24 box
//! drawn into 48 px is 3.4 px wide. A non-uniform transform strokes with the geometric mean scale (an approximation Direct2D shares: a
//! stroke style cannot be sheared).
use crate::canvas::Canvas;
use crate::color::Color;
use crate::geom::Rect;
use crate::gfx::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use windows::core::{Interface, Result};
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;

type P = (f32, f32);

// ---------------------------------------------------------------- errors, scanner

/// Where and why path data (or another attribute) could not be read. `offset` is a byte offset into the string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub offset: usize,
    pub msg: &'static str,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at byte {}", self.msg, self.offset)
    }
}

impl std::error::Error for ParseError {}

/// Byte scanner shared by path data, transform lists, point lists and viewBoxes.
struct Sc<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Sc<'a> {
    fn new(s: &'a str) -> Sc<'a> {
        Sc { b: s.as_bytes(), i: 0 }
    }
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }
    /// Skip whitespace and commas.
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n' | b',')) {
            self.i += 1;
        }
    }
    fn at_end(&mut self) -> bool {
        self.ws();
        self.i >= self.b.len()
    }
    fn digits(&mut self) -> usize {
        let s = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - s
    }
    /// A number: sign, digits, one fraction, optional exponent. `1.5.5` is two numbers, `10-5` is two numbers.
    fn number(&mut self) -> Option<f32> {
        self.ws();
        let s = self.i;
        if matches!(self.peek(), Some(b'+' | b'-')) {
            self.i += 1;
        }
        let mut n = self.digits();
        if self.peek() == Some(b'.') {
            self.i += 1;
            n += self.digits();
        }
        if n == 0 {
            self.i = s;
            return None;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let save = self.i;
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                self.i = save; // "1e" followed by a letter: the e belongs to something else
            }
        }
        let v = std::str::from_utf8(&self.b[s..self.i]).ok()?.parse::<f64>().ok()?;
        v.is_finite().then_some(v as f32)
    }
    /// An arc flag: exactly one `0` or `1`, with no separator needed after it.
    fn flag(&mut self) -> Option<bool> {
        self.ws();
        let v = match self.peek() {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return None,
        };
        self.i += 1;
        Some(v)
    }
    fn numbers(&mut self) -> Vec<f32> {
        let mut v = Vec::new();
        while let Some(n) = self.number() {
            v.push(n);
        }
        v
    }
}

// ---------------------------------------------------------------- affine

/// 2x3 affine matrix: `x' = a x + c y + e`, `y' = b x + d y + f` (the SVG `matrix(a b c d e f)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Affine {
    pub a: f32,
    pub b: f32,
    pub c: f32,
    pub d: f32,
    pub e: f32,
    pub f: f32,
}

impl Default for Affine {
    fn default() -> Affine {
        Affine::IDENTITY
    }
}

impl Affine {
    pub const IDENTITY: Affine = Affine { a: 1.0, b: 0.0, c: 0.0, d: 1.0, e: 0.0, f: 0.0 };

    pub fn translate(x: f32, y: f32) -> Affine {
        Affine { e: x, f: y, ..Affine::IDENTITY }
    }
    pub fn scale(x: f32, y: f32) -> Affine {
        Affine { a: x, d: y, ..Affine::IDENTITY }
    }
    /// Rotation by `deg` degrees (clockwise on screen, y down) around the origin.
    pub fn rotate(deg: f32) -> Affine {
        let (s, c) = deg.to_radians().sin_cos();
        Affine { a: c, b: s, c: -s, d: c, e: 0.0, f: 0.0 }
    }

    /// Matrix product `self * o`: the result applies `o` FIRST, then `self` (so `parent.mul(&child)` maps child coordinates to the parent's parent).
    pub fn mul(&self, o: &Affine) -> Affine {
        Affine {
            a: self.a * o.a + self.c * o.b,
            b: self.b * o.a + self.d * o.b,
            c: self.a * o.c + self.c * o.d,
            d: self.b * o.c + self.d * o.d,
            e: self.a * o.e + self.c * o.f + self.e,
            f: self.b * o.e + self.d * o.f + self.f,
        }
    }

    pub fn apply(&self, p: P) -> P {
        (self.a * p.0 + self.c * p.1 + self.e, self.b * p.0 + self.d * p.1 + self.f)
    }

    /// Uniform scale factor `sqrt|det|`: what a stroke width is multiplied by.
    pub fn scale_factor(&self) -> f32 {
        (self.a * self.d - self.b * self.c).abs().sqrt()
    }

    fn bits(&self) -> [u32; 6] {
        [self.a.to_bits(), self.b.to_bits(), self.c.to_bits(), self.d.to_bits(), self.e.to_bits(), self.f.to_bits()]
    }

    /// The SVG `transform` attribute: a list of `matrix`, `translate`, `scale`, `rotate(a [cx cy])`, `skewX`, `skewY`, separated by spaces or
    /// commas; the leftmost is the outermost. An empty string is the identity.
    pub fn parse(s: &str) -> std::result::Result<Affine, ParseError> {
        let mut sc = Sc::new(s);
        let mut m = Affine::IDENTITY;
        loop {
            sc.ws();
            if sc.i >= sc.b.len() {
                return Ok(m);
            }
            let start = sc.i;
            while matches!(sc.peek(), Some(b'a'..=b'z' | b'A'..=b'Z')) {
                sc.i += 1;
            }
            let name = &s[start..sc.i];
            sc.ws();
            if sc.peek() != Some(b'(') {
                return Err(ParseError { offset: sc.i, msg: "expected '(' after transform name" });
            }
            sc.i += 1;
            let args = sc.numbers();
            sc.ws();
            if sc.peek() != Some(b')') {
                return Err(ParseError { offset: sc.i, msg: "unterminated transform arguments" });
            }
            sc.i += 1;
            let t = match (name, args.as_slice()) {
                ("matrix", &[a, b, c, d, e, f]) => Affine { a, b, c, d, e, f },
                ("translate", &[x]) => Affine::translate(x, 0.0),
                ("translate", &[x, y]) => Affine::translate(x, y),
                ("scale", &[s]) => Affine::scale(s, s),
                ("scale", &[x, y]) => Affine::scale(x, y),
                ("rotate", &[a]) => Affine::rotate(a),
                ("rotate", &[a, cx, cy]) => Affine::translate(cx, cy).mul(&Affine::rotate(a)).mul(&Affine::translate(-cx, -cy)),
                ("skewX", &[a]) => Affine { c: a.to_radians().tan(), ..Affine::IDENTITY },
                ("skewY", &[a]) => Affine { b: a.to_radians().tan(), ..Affine::IDENTITY },
                _ => return Err(ParseError { offset: start, msg: "unknown transform or wrong argument count" }),
            };
            m = m.mul(&t);
        }
    }
}

// ---------------------------------------------------------------- path

/// One absolute path segment. Quadratics and arcs have been converted to cubics.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    /// control 1, control 2, end point
    Cubic(P, P, P),
    /// back to the start of the sub-path
    Close,
}

/// A neutral outline: a list of [`Seg`]s in user units.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    pub segs: Vec<Seg>,
}

struct PathParser {
    out: Vec<Seg>,
    cur: P,
    start: P,
    last_c: Option<P>,
    last_q: Option<P>,
    need_move: bool,
}

impl PathParser {
    fn draw(&mut self, s: Seg) {
        if self.need_move {
            self.out.push(Seg::Move(self.cur.0, self.cur.1));
            self.need_move = false;
        }
        self.out.push(s);
    }
    fn line(&mut self, p: P) {
        self.draw(Seg::Line(p.0, p.1));
        self.cur = p;
        self.last_c = None;
        self.last_q = None;
    }
    fn cubic(&mut self, c1: P, c2: P, p: P) {
        self.draw(Seg::Cubic(c1, c2, p));
        self.cur = p;
        self.last_c = Some(c2);
        self.last_q = None;
    }
    fn quad(&mut self, q: P, p: P) {
        let c = self.cur;
        self.draw(Seg::Cubic((c.0 + 2.0 / 3.0 * (q.0 - c.0), c.1 + 2.0 / 3.0 * (q.1 - c.1)), (p.0 + 2.0 / 3.0 * (q.0 - p.0), p.1 + 2.0 / 3.0 * (q.1 - p.1)), p));
        self.cur = p;
        self.last_c = None;
        self.last_q = Some(q);
    }
}

impl Path {
    pub fn new() -> Path {
        Path::default()
    }

    pub fn is_empty(&self) -> bool {
        self.segs.is_empty()
    }

    /// Parse path data strictly: the first problem is an error carrying its byte offset. Data must start with `M`/`m`; an empty string is an
    /// empty path. Use [`Path::parse_prefix`] to keep what precedes an error the way SVG renderers do.
    pub fn parse(d: &str) -> std::result::Result<Path, ParseError> {
        match Path::parse_inner(d) {
            (p, None) => Ok(p),
            (_, Some(e)) => Err(e),
        }
    }

    /// Everything before the first error (what browsers draw for broken path data) and the error, if any.
    pub fn parse_prefix(d: &str) -> (Path, Option<ParseError>) {
        Path::parse_inner(d)
    }

    fn parse_inner(d: &str) -> (Path, Option<ParseError>) {
        let mut sc = Sc::new(d);
        let mut st = PathParser { out: Vec::new(), cur: (0.0, 0.0), start: (0.0, 0.0), last_c: None, last_q: None, need_move: false };
        let mut cmd: Option<u8> = None;
        let mut first = true;
        let err = loop {
            if sc.at_end() {
                break None;
            }
            let c = sc.peek().unwrap_or(0);
            if c.is_ascii_alphabetic() {
                if !b"MmLlHhVvCcSsQqTtAaZz".contains(&c) {
                    break Some(ParseError { offset: sc.i, msg: "unknown path command" });
                }
                if first && !matches!(c, b'M' | b'm') {
                    break Some(ParseError { offset: sc.i, msg: "path data must start with a moveto" });
                }
                sc.i += 1;
                cmd = Some(c);
                if matches!(c, b'Z' | b'z') {
                    if !st.out.is_empty() && !matches!(st.out.last(), Some(Seg::Close)) {
                        st.out.push(Seg::Close);
                    }
                    st.cur = st.start;
                    st.last_c = None;
                    st.last_q = None;
                    st.need_move = true;
                    continue;
                }
            } else if matches!(cmd, None | Some(b'Z' | b'z')) {
                break Some(ParseError { offset: sc.i, msg: "expected a path command" });
            }
            first = false;
            let c = cmd.unwrap_or(b'M');
            let rel = c.is_ascii_lowercase();
            let off = sc.i;
            let bad = ParseError { offset: off, msg: "missing or malformed number" };
            let base = if rel { st.cur } else { (0.0, 0.0) };
            let r = (|| -> Option<()> {
                match c.to_ascii_uppercase() {
                    b'M' => {
                        let p = (sc.number()? + base.0, sc.number()? + base.1);
                        st.out.push(Seg::Move(p.0, p.1));
                        st.cur = p;
                        st.start = p;
                        st.last_c = None;
                        st.last_q = None;
                        st.need_move = false;
                        cmd = Some(if rel { b'l' } else { b'L' }); // further pairs are linetos
                    }
                    b'L' => {
                        let p = (sc.number()? + base.0, sc.number()? + base.1);
                        st.line(p);
                    }
                    b'H' => {
                        let x = sc.number()? + base.0;
                        st.line((x, st.cur.1));
                    }
                    b'V' => {
                        let y = sc.number()? + base.1;
                        st.line((st.cur.0, y));
                    }
                    b'C' => {
                        let c1 = (sc.number()? + base.0, sc.number()? + base.1);
                        let c2 = (sc.number()? + base.0, sc.number()? + base.1);
                        let p = (sc.number()? + base.0, sc.number()? + base.1);
                        st.cubic(c1, c2, p);
                    }
                    b'S' => {
                        let c2 = (sc.number()? + base.0, sc.number()? + base.1);
                        let p = (sc.number()? + base.0, sc.number()? + base.1);
                        let c1 = st.last_c.map_or(st.cur, |l| (2.0 * st.cur.0 - l.0, 2.0 * st.cur.1 - l.1));
                        st.cubic(c1, c2, p);
                    }
                    b'Q' => {
                        let q = (sc.number()? + base.0, sc.number()? + base.1);
                        let p = (sc.number()? + base.0, sc.number()? + base.1);
                        st.quad(q, p);
                    }
                    b'T' => {
                        let p = (sc.number()? + base.0, sc.number()? + base.1);
                        let q = st.last_q.map_or(st.cur, |l| (2.0 * st.cur.0 - l.0, 2.0 * st.cur.1 - l.1));
                        st.quad(q, p);
                    }
                    b'A' => {
                        let (rx, ry, rot) = (sc.number()?, sc.number()?, sc.number()?);
                        let (large, sweep) = (sc.flag()?, sc.flag()?);
                        let p = (sc.number()? + base.0, sc.number()? + base.1);
                        let from = st.cur;
                        let mut segs = Vec::new();
                        arc_to_segs(from, rx, ry, rot, large, sweep, p, &mut segs);
                        if segs.is_empty() {
                            // same end point: the arc is omitted, but the current point stays valid
                            st.last_c = None;
                            st.last_q = None;
                        }
                        for s in segs {
                            match s {
                                Seg::Line(x, y) => st.line((x, y)),
                                Seg::Cubic(a, b, e) => st.cubic(a, b, e),
                                _ => {}
                            }
                        }
                        st.last_c = None;
                    }
                    _ => unreachable!(),
                }
                Some(())
            })();
            if r.is_none() {
                break Some(bad);
            }
        };
        (Path { segs: st.out }, err)
    }

    /// The path with every point mapped through `m`.
    pub fn transformed(&self, m: &Affine) -> Path {
        let f = |p: P| m.apply(p);
        Path {
            segs: self
                .segs
                .iter()
                .map(|s| match *s {
                    Seg::Move(x, y) => {
                        let p = f((x, y));
                        Seg::Move(p.0, p.1)
                    }
                    Seg::Line(x, y) => {
                        let p = f((x, y));
                        Seg::Line(p.0, p.1)
                    }
                    Seg::Cubic(a, b, c) => Seg::Cubic(f(a), f(b), f(c)),
                    Seg::Close => Seg::Close,
                })
                .collect(),
        }
    }

    /// Exact bounding box of the outline (curve extrema, not control points). An empty path has a zero rectangle at the origin.
    pub fn bounds(&self) -> Rect {
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        let mut add = |p: P| {
            x0 = x0.min(p.0);
            y0 = y0.min(p.1);
            x1 = x1.max(p.0);
            y1 = y1.max(p.1);
        };
        let (mut cur, mut start) = ((0.0, 0.0), (0.0, 0.0));
        for s in &self.segs {
            match *s {
                Seg::Move(x, y) => {
                    cur = (x, y);
                    start = cur;
                    add(cur);
                }
                Seg::Line(x, y) => {
                    add(cur);
                    cur = (x, y);
                    add(cur);
                }
                Seg::Cubic(c1, c2, p) => {
                    add(cur);
                    add(p);
                    for axis in 0..2 {
                        let g = |q: P| if axis == 0 { q.0 } else { q.1 };
                        for t in cubic_extrema(g(cur), g(c1), g(c2), g(p)) {
                            add(cubic_at(cur, c1, c2, p, t));
                        }
                    }
                    cur = p;
                }
                Seg::Close => cur = start,
            }
        }
        if x1 < x0 {
            return Rect::default();
        }
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}

/// Point of the cubic at parameter `t`.
fn cubic_at(p0: P, p1: P, p2: P, p3: P, t: f32) -> P {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1)
}

/// Parameters in (0, 1) where one coordinate of a cubic has a zero derivative.
fn cubic_extrema(p0: f32, p1: f32, p2: f32, p3: f32) -> Vec<f32> {
    // B'(t) / 3 = a t^2 + b t + c
    let a = -p0 + 3.0 * p1 - 3.0 * p2 + p3;
    let b = 2.0 * (p0 - 2.0 * p1 + p2);
    let c = p1 - p0;
    let mut r = Vec::new();
    if a.abs() < 1e-9 {
        if b.abs() > 1e-9 {
            r.push(-c / b);
        }
    } else {
        let disc = b * b - 4.0 * a * c;
        if disc >= 0.0 {
            let s = disc.sqrt();
            r.push((-b + s) / (2.0 * a));
            r.push((-b - s) / (2.0 * a));
        }
    }
    r.retain(|t| *t > 0.0 && *t < 1.0);
    r
}

/// Convert an SVG elliptical arc to line / cubic segments appended to `out` (nothing when the end points coincide; a line when a radius is
/// zero). Radii too small for the chord are scaled up (F.6.6).
#[allow(clippy::too_many_arguments)]
fn arc_to_segs(from: P, rx: f32, ry: f32, rot_deg: f32, large: bool, sweep: bool, to: P, out: &mut Vec<Seg>) {
    if from == to {
        return;
    }
    let (mut rx, mut ry) = (rx.abs() as f64, ry.abs() as f64);
    if rx == 0.0 || ry == 0.0 {
        out.push(Seg::Line(to.0, to.1));
        return;
    }
    let phi = (rot_deg as f64).to_radians();
    let (sp, cp) = phi.sin_cos();
    let (x0, y0, x1, y1) = (from.0 as f64, from.1 as f64, to.0 as f64, to.1 as f64);
    let (dx, dy) = ((x0 - x1) / 2.0, (y0 - y1) / 2.0);
    let (x1p, y1p) = (cp * dx + sp * dy, -sp * dx + cp * dy);
    let lam = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if lam > 1.0 {
        let s = lam.sqrt();
        rx *= s;
        ry *= s;
    }
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let coef = if den == 0.0 { 0.0 } else { (num / den).max(0.0).sqrt() } * if large == sweep { -1.0 } else { 1.0 };
    let (cxp, cyp) = (coef * rx * y1p / ry, -coef * ry * x1p / rx);
    let (cx, cy) = (cp * cxp - sp * cyp + (x0 + x1) / 2.0, sp * cxp + cp * cyp + (y0 + y1) / 2.0);
    let ang = |ux: f64, uy: f64, vx: f64, vy: f64| (ux * vy - uy * vx).atan2(ux * vx + uy * vy);
    let th1 = ang(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut dth = ang((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if !sweep && dth > 0.0 {
        dth -= std::f64::consts::TAU;
    } else if sweep && dth < 0.0 {
        dth += std::f64::consts::TAU;
    }
    let n = ((dth.abs() / std::f64::consts::FRAC_PI_2) - 1e-9).ceil().max(1.0) as usize;
    let delta = dth / n as f64;
    let t = 4.0 / 3.0 * (delta / 4.0).tan();
    let pt = |th: f64| {
        let (s, c) = th.sin_cos();
        (cx + rx * c * cp - ry * s * sp, cy + rx * c * sp + ry * s * cp)
    };
    let dpt = |th: f64| {
        let (s, c) = th.sin_cos();
        (-rx * s * cp - ry * c * sp, -rx * s * sp + ry * c * cp)
    };
    for k in 0..n {
        let (a, b) = (th1 + delta * k as f64, th1 + delta * (k + 1) as f64);
        let (pa, pb) = (pt(a), pt(b));
        let (da, db) = (dpt(a), dpt(b));
        let end = if k + 1 == n { (to.0, to.1) } else { (pb.0 as f32, pb.1 as f32) };
        out.push(Seg::Cubic(((pa.0 + t * da.0) as f32, (pa.1 + t * da.1) as f32), ((pb.0 - t * db.0) as f32, (pb.1 - t * db.1) as f32), end));
    }
}

// ---------------------------------------------------------------- Direct2D geometry

/// The Direct2D geometry of `path` under `m`. `filled` = the figures can be filled (they are begun `FILLED`; a stroke of the same geometry
/// still treats open sub-paths as open). Non-zero winding fill; see [`build_geometry_rule`] for even-odd.
pub fn build_geometry(gfx: &Gfx, path: &Path, m: &Affine, filled: bool) -> Result<ID2D1PathGeometry> {
    build_geometry_rule(gfx, path, m, filled, false)
}

/// [`build_geometry`] with the fill rule: `evenodd` false = non-zero winding.
pub fn build_geometry_rule(gfx: &Gfx, path: &Path, m: &Affine, filled: bool, evenodd: bool) -> Result<ID2D1PathGeometry> {
    unsafe {
        let g = gfx.d2d.CreatePathGeometry()?;
        let sink = g.Open()?;
        sink.SetFillMode(if evenodd { D2D1_FILL_MODE_ALTERNATE } else { D2D1_FILL_MODE_WINDING });
        let begin = if filled { D2D1_FIGURE_BEGIN_FILLED } else { D2D1_FIGURE_BEGIN_HOLLOW };
        let f = |p: P| {
            let q = m.apply(p);
            pt(q.0, q.1)
        };
        let (mut open, mut cur, mut start) = (false, (0.0f32, 0.0f32), (0.0f32, 0.0f32));
        for s in &path.segs {
            match *s {
                Seg::Move(x, y) => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_OPEN);
                        open = false;
                    }
                    cur = (x, y);
                    start = cur;
                }
                Seg::Line(x, y) => {
                    if !open {
                        sink.BeginFigure(f(cur), begin);
                        open = true;
                    }
                    sink.AddLine(f((x, y)));
                    cur = (x, y);
                }
                Seg::Cubic(a, b, c) => {
                    if !open {
                        sink.BeginFigure(f(cur), begin);
                        open = true;
                    }
                    sink.AddBezier(&D2D1_BEZIER_SEGMENT { point1: f(a), point2: f(b), point3: f(c) });
                    cur = c;
                }
                Seg::Close => {
                    if open {
                        sink.EndFigure(D2D1_FIGURE_END_CLOSED);
                        open = false;
                    }
                    cur = start;
                }
            }
        }
        if open {
            sink.EndFigure(D2D1_FIGURE_END_OPEN);
        }
        sink.Close()?;
        Ok(g.into())
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Src {
    /// path data given as text
    Text(String),
    /// a shape of an [`Svg`]: (svg id, shape index)
    Node(u64, u32),
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct GKey {
    src: Src,
    m: [u32; 6],
    filled: bool,
    evenodd: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct SKey {
    cap: u8,
    join: u8,
    miter: u32,
    dash: Vec<u32>,
    offset: u32,
}

/// Bounded cache of built geometries (keyed by source, transform bits and fill mode) and of stroke styles. Per Direct2D device; not `Send`.
/// When it holds more than [`GeomCache::LIMIT`] geometries it starts over (a miss only costs one rebuild).
#[derive(Default)]
pub struct GeomCache {
    geoms: RefCell<HashMap<GKey, ID2D1PathGeometry>>,
    strokes: RefCell<HashMap<SKey, ID2D1StrokeStyle>>,
}

impl GeomCache {
    pub const LIMIT: usize = 512;

    pub fn new() -> GeomCache {
        GeomCache::default()
    }

    /// Number of cached geometries.
    pub fn len(&self) -> usize {
        self.geoms.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn clear(&self) {
        self.geoms.borrow_mut().clear();
        self.strokes.borrow_mut().clear();
    }

    /// Geometry of path data `d` under `m` (cached). Broken data draws what precedes the error, like browsers do.
    pub fn get(&self, gfx: &Gfx, d: &str, m: &Affine, filled: bool) -> Result<ID2D1PathGeometry> {
        self.get_rule(gfx, d, m, filled, false)
    }

    pub fn get_rule(&self, gfx: &Gfx, d: &str, m: &Affine, filled: bool, evenodd: bool) -> Result<ID2D1PathGeometry> {
        let key = GKey { src: Src::Text(d.to_string()), m: m.bits(), filled, evenodd };
        self.lookup(key, || build_geometry_rule(gfx, &Path::parse_prefix(d).0, m, filled, evenodd))
    }

    fn node(&self, gfx: &Gfx, id: u64, idx: u32, path: &Path, m: &Affine, evenodd: bool) -> Result<ID2D1PathGeometry> {
        let key = GKey { src: Src::Node(id, idx), m: m.bits(), filled: true, evenodd };
        self.lookup(key, || build_geometry_rule(gfx, path, m, true, evenodd))
    }

    fn lookup(&self, key: GKey, build: impl FnOnce() -> Result<ID2D1PathGeometry>) -> Result<ID2D1PathGeometry> {
        if let Some(g) = self.geoms.borrow().get(&key) {
            return Ok(g.clone());
        }
        let g = build()?;
        let mut m = self.geoms.borrow_mut();
        if m.len() >= Self::LIMIT {
            m.clear();
        }
        m.insert(key, g.clone());
        Ok(g)
    }

    fn stroke_style(&self, gfx: &Gfx, st: &SvgStyle, width: f32) -> Result<ID2D1StrokeStyle> {
        // Direct2D dash lengths are multiples of the stroke width, SVG's are user units: divide
        let dash: Vec<f32> = if width > 0.0 { st.dash.iter().map(|d| d / width).collect() } else { Vec::new() };
        let key = SKey { cap: st.cap as u8, join: st.join as u8, miter: st.miter.to_bits(), dash: dash.iter().map(|d| d.to_bits()).collect(), offset: (st.dash_offset / width.max(1e-6)).to_bits() };
        if let Some(s) = self.strokes.borrow().get(&key) {
            return Ok(s.clone());
        }
        let cap = match st.cap {
            Cap::Butt => D2D1_CAP_STYLE_FLAT,
            Cap::Round => D2D1_CAP_STYLE_ROUND,
            Cap::Square => D2D1_CAP_STYLE_SQUARE,
        };
        let sp = D2D1_STROKE_STYLE_PROPERTIES1 {
            startCap: cap,
            endCap: cap,
            dashCap: cap,
            lineJoin: match st.join {
                Join::Miter => D2D1_LINE_JOIN_MITER_OR_BEVEL,
                Join::Round => D2D1_LINE_JOIN_ROUND,
                Join::Bevel => D2D1_LINE_JOIN_BEVEL,
            },
            miterLimit: st.miter.max(1.0),
            dashStyle: if dash.is_empty() { D2D1_DASH_STYLE_SOLID } else { D2D1_DASH_STYLE_CUSTOM },
            dashOffset: st.dash_offset / width.max(1e-6),
            transformType: D2D1_STROKE_TRANSFORM_TYPE_NORMAL,
        };
        let s: ID2D1StrokeStyle = unsafe { gfx.d2d.CreateStrokeStyle(&sp, if dash.is_empty() { None } else { Some(&dash) })?.cast()? };
        let mut m = self.strokes.borrow_mut();
        if m.len() >= 64 {
            m.clear();
        }
        m.insert(key, s.clone());
        Ok(s)
    }
}

// ---------------------------------------------------------------- style

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    Butt = 0,
    Round = 1,
    Square = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    Miter = 0,
    Round = 1,
    Bevel = 2,
}

/// A fill or stroke paint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Paint {
    None,
    /// `currentColor`: the tint passed to [`Svg::draw`]
    Current,
    Color(Color),
}

/// The inherited presentation properties of an element (and the defaults a fragment is read with).
#[derive(Clone, Debug, PartialEq)]
pub struct SvgStyle {
    pub fill: Paint,
    pub stroke: Paint,
    pub stroke_width: f32,
    pub cap: Cap,
    pub join: Join,
    pub miter: f32,
    pub dash: Vec<f32>,
    pub dash_offset: f32,
    pub fill_opacity: f32,
    pub stroke_opacity: f32,
    pub evenodd: bool,
}

impl Default for SvgStyle {
    /// SVG's initial values: black fill, no stroke, width 1, butt caps, miter joins.
    fn default() -> SvgStyle {
        SvgStyle {
            fill: Paint::Color(Color::rgb8(0, 0, 0)),
            stroke: Paint::None,
            stroke_width: 1.0,
            cap: Cap::Butt,
            join: Join::Miter,
            miter: 4.0,
            dash: Vec::new(),
            dash_offset: 0.0,
            fill_opacity: 1.0,
            stroke_opacity: 1.0,
            evenodd: false,
        }
    }
}

impl SvgStyle {
    /// The wrapper of Pill's line icons: no fill, `currentColor` stroke of width `stroke_width`, round caps and joins.
    pub fn line_icon(stroke_width: f32) -> SvgStyle {
        SvgStyle { fill: Paint::None, stroke: Paint::Current, stroke_width, cap: Cap::Round, join: Join::Round, ..SvgStyle::default() }
    }
}

fn parse_color(s: &str) -> Option<Paint> {
    let s = s.trim();
    let l = s.to_ascii_lowercase();
    let c = |r: u8, g: u8, b: u8, a: f32| Paint::Color(Color::rgb8(r, g, b).alpha(a));
    match l.as_str() {
        "none" => return Some(Paint::None),
        "currentcolor" => return Some(Paint::Current),
        "transparent" => return Some(c(0, 0, 0, 0.0)),
        "white" => return Some(c(255, 255, 255, 1.0)),
        "black" => return Some(c(0, 0, 0, 1.0)),
        "red" => return Some(c(255, 0, 0, 1.0)),
        "blue" => return Some(c(0, 0, 255, 1.0)),
        "gray" | "grey" => return Some(c(128, 128, 128, 1.0)),
        _ => {}
    }
    if let Some(h) = l.strip_prefix('#') {
        let v = u32::from_str_radix(h, 16).ok()?;
        let n = |i: u32, bits: u32| ((v >> i) & ((1 << bits) - 1)) as u8;
        return match h.len() {
            3 => Some(c(n(8, 4) * 17, n(4, 4) * 17, n(0, 4) * 17, 1.0)),
            4 => Some(c(n(12, 4) * 17, n(8, 4) * 17, n(4, 4) * 17, n(0, 4) as f32 * 17.0 / 255.0)),
            6 => Some(c(n(16, 8), n(8, 8), n(0, 8), 1.0)),
            8 => Some(c(n(24, 8), n(16, 8), n(8, 8), n(0, 8) as f32 / 255.0)),
            _ => None,
        };
    }
    let inner = l.strip_prefix("rgba(").or_else(|| l.strip_prefix("rgb("))?.strip_suffix(')')?;
    let v: Vec<f32> = inner.split(|ch: char| ch == ',' || ch.is_whitespace() || ch == '/').filter(|x| !x.is_empty()).map(|x| x.trim_end_matches('%').parse::<f32>().ok()).collect::<Option<_>>()?;
    match v.as_slice() {
        &[r, g, b] => Some(c(r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8, 1.0)),
        &[r, g, b, a] => Some(c(r.clamp(0.0, 255.0) as u8, g.clamp(0.0, 255.0) as u8, b.clamp(0.0, 255.0) as u8, a.clamp(0.0, 1.0))),
        _ => None,
    }
}

fn num_attr(v: &str) -> Option<f32> {
    let t = v.trim();
    let t = t.strip_suffix("px").unwrap_or(t);
    t.parse::<f32>().ok().filter(|x| x.is_finite())
}

/// Apply one presentation property to `st`; false when the property is not one of ours or its value is not understood.
fn set_prop(st: &mut SvgStyle, name: &str, v: &str, warn: &mut Vec<String>) -> bool {
    let v = v.trim();
    match name {
        "fill" | "stroke" => {
            let p = if v.starts_with("url(") {
                warn.push(format!("{name}: gradients and patterns are not supported ({v})"));
                Some(Paint::None)
            } else {
                parse_color(v)
            };
            match p {
                Some(p) => {
                    if name == "fill" {
                        st.fill = p
                    } else {
                        st.stroke = p
                    }
                }
                None => warn.push(format!("{name}: unknown colour {v:?}")),
            }
        }
        "stroke-width" => match num_attr(v) {
            Some(w) if w >= 0.0 => st.stroke_width = w,
            _ => warn.push(format!("stroke-width: bad value {v:?}")),
        },
        "stroke-linecap" => match v {
            "butt" => st.cap = Cap::Butt,
            "round" => st.cap = Cap::Round,
            "square" => st.cap = Cap::Square,
            _ => warn.push(format!("stroke-linecap: bad value {v:?}")),
        },
        "stroke-linejoin" => match v {
            "miter" | "miter-clip" | "arcs" => st.join = Join::Miter,
            "round" => st.join = Join::Round,
            "bevel" => st.join = Join::Bevel,
            _ => warn.push(format!("stroke-linejoin: bad value {v:?}")),
        },
        "stroke-miterlimit" => {
            if let Some(m) = num_attr(v) {
                st.miter = m.max(1.0)
            }
        }
        "stroke-dasharray" => {
            if v == "none" {
                st.dash.clear();
            } else {
                let mut d = Sc::new(v).numbers();
                if d.iter().any(|x| *x < 0.0) || d.iter().sum::<f32>() <= 0.0 {
                    d.clear();
                } else if d.len() % 2 == 1 {
                    d.extend_from_within(..); // an odd list repeats to even
                }
                st.dash = d;
            }
        }
        "stroke-dashoffset" => {
            if let Some(o) = num_attr(v) {
                st.dash_offset = o
            }
        }
        "fill-opacity" => {
            if let Some(o) = num_attr(v) {
                st.fill_opacity = o.clamp(0.0, 1.0)
            }
        }
        "stroke-opacity" => {
            if let Some(o) = num_attr(v) {
                st.stroke_opacity = o.clamp(0.0, 1.0)
            }
        }
        "fill-rule" => st.evenodd = v == "evenodd",
        _ => return false,
    }
    true
}

/// Apply the presentation attributes and the `style` declaration list of an element to `st`.
/// Returns (opacity, display:none).
fn cascade(st: &mut SvgStyle, attrs: &[(String, String)], warn: &mut Vec<String>) -> (f32, bool) {
    let mut opacity = 1.0;
    let mut hidden = false;
    let mut apply = |st: &mut SvgStyle, name: &str, v: &str, warn: &mut Vec<String>| match name {
        "opacity" => {
            if let Some(o) = num_attr(v) {
                opacity = o.clamp(0.0, 1.0)
            }
        }
        "display" => hidden = v.trim() == "none",
        _ => {
            set_prop(st, name, v, warn);
        }
    };
    for (k, v) in attrs {
        apply(st, k, v, warn);
    }
    for (k, v) in attrs {
        if k == "style" {
            for decl in v.split(';') {
                if let Some((n, val)) = decl.split_once(':') {
                    apply(st, n.trim(), val, warn);
                }
            }
        }
    }
    (opacity, hidden)
}

// ---------------------------------------------------------------- xml-ish tokenizer

#[derive(Debug)]
struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
    close: bool,
    self_close: bool,
}

impl Tag {
    fn attr(&self, n: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str())
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

/// Tolerant tag scanner: skips text, comments, `<?...?>` and `<!...>`; never fails (a broken tag ends the scan).
fn tokenize(src: &str) -> Vec<Tag> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(p) = src[i..].find('<') {
        i += p;
        if src[i..].starts_with("<!--") {
            match src[i..].find("-->") {
                Some(e) => i += e + 3,
                None => break,
            }
            continue;
        }
        if src[i..].starts_with("<?") || src[i..].starts_with("<!") {
            match src[i..].find('>') {
                Some(e) => i += e + 1,
                None => break,
            }
            continue;
        }
        i += 1;
        let close = b.get(i) == Some(&b'/');
        if close {
            i += 1;
        }
        let ns = i;
        while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>') {
            i += 1;
        }
        let name = src[ns..i].to_string();
        let mut attrs = Vec::new();
        let mut self_close = false;
        loop {
            while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
                i += 1;
            }
            match b.get(i) {
                None => return out,
                Some(b'>') => {
                    i += 1;
                    break;
                }
                Some(b'/') => {
                    self_close = true;
                    i += 1;
                }
                Some(_) => {
                    let ks = i;
                    while i < b.len() && !matches!(b[i], b'=' | b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>') {
                        i += 1;
                    }
                    let k = src[ks..i].to_string();
                    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
                        i += 1;
                    }
                    let mut v = String::new();
                    if b.get(i) == Some(&b'=') {
                        i += 1;
                        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n') {
                            i += 1;
                        }
                        match b.get(i) {
                            Some(&q) if q == b'"' || q == b'\'' => {
                                i += 1;
                                let vs = i;
                                while i < b.len() && b[i] != q {
                                    i += 1;
                                }
                                v = unescape(&src[vs..i.min(b.len())]);
                                i += 1;
                            }
                            _ => {
                                let vs = i;
                                while i < b.len() && !matches!(b[i], b' ' | b'\t' | b'\r' | b'\n' | b'/' | b'>') {
                                    i += 1;
                                }
                                v = unescape(&src[vs..i]);
                            }
                        }
                    }
                    if k.is_empty() {
                        i += 1; // stray character: do not loop forever
                    } else {
                        attrs.push((k, v));
                    }
                }
            }
        }
        out.push(Tag { name, attrs, close, self_close });
    }
    out
}

// ---------------------------------------------------------------- svg document

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
struct Shape {
    idx: u32,
    path: Path,
    transform: Affine,
    style: SvgStyle,
    opacity: f32,
}

#[derive(Clone, Debug)]
enum Node {
    Group { transform: Affine, opacity: f32, children: Vec<Node> },
    Shape(Shape),
}

/// A parsed SVG (the supported subset, see the module docs). Cheap to draw many times: paths are parsed once and geometries come from a
/// [`GeomCache`].
#[derive(Clone, Debug)]
pub struct Svg {
    id: u64,
    vb: (f32, f32, f32, f32),
    size: (f32, f32),
    root: Vec<Node>,
    shapes: u32,
    warnings: Vec<String>,
}

struct Frame {
    style: SvgStyle,
    transform: Affine,
    opacity: f32,
    children: Vec<Node>,
}

struct Builder {
    frames: Vec<Frame>,
    skip: u32,
    shapes: u32,
    warnings: Vec<String>,
    root_seen: bool,
    vb: Option<(f32, f32, f32, f32)>,
    size: Option<(f32, f32)>,
}

fn ellipse_path(cx: f32, cy: f32, rx: f32, ry: f32) -> Path {
    let mut s = vec![Seg::Move(cx + rx, cy)];
    arc_to_segs((cx + rx, cy), rx, ry, 0.0, false, true, (cx - rx, cy), &mut s);
    arc_to_segs((cx - rx, cy), rx, ry, 0.0, false, true, (cx + rx, cy), &mut s);
    s.push(Seg::Close);
    Path { segs: s }
}

fn rect_path(x: f32, y: f32, w: f32, h: f32, rx: f32, ry: f32) -> Path {
    let (rx, ry) = (rx.min(w / 2.0), ry.min(h / 2.0));
    if rx <= 0.0 || ry <= 0.0 {
        return Path { segs: vec![Seg::Move(x, y), Seg::Line(x + w, y), Seg::Line(x + w, y + h), Seg::Line(x, y + h), Seg::Close] };
    }
    let mut s = vec![Seg::Move(x + rx, y), Seg::Line(x + w - rx, y)];
    arc_to_segs((x + w - rx, y), rx, ry, 0.0, false, true, (x + w, y + ry), &mut s);
    s.push(Seg::Line(x + w, y + h - ry));
    arc_to_segs((x + w, y + h - ry), rx, ry, 0.0, false, true, (x + w - rx, y + h), &mut s);
    s.push(Seg::Line(x + rx, y + h));
    arc_to_segs((x + rx, y + h), rx, ry, 0.0, false, true, (x, y + h - ry), &mut s);
    s.push(Seg::Line(x, y + ry));
    arc_to_segs((x, y + ry), rx, ry, 0.0, false, true, (x + rx, y), &mut s);
    s.push(Seg::Close);
    Path { segs: s }
}

fn poly_path(points: &str, close: bool) -> Path {
    let v = Sc::new(points).numbers();
    let mut segs = Vec::new();
    for (i, p) in v.chunks_exact(2).enumerate() {
        segs.push(if i == 0 { Seg::Move(p[0], p[1]) } else { Seg::Line(p[0], p[1]) });
    }
    if close && !segs.is_empty() {
        segs.push(Seg::Close);
    }
    Path { segs }
}

impl Builder {
    fn new(base: SvgStyle) -> Builder {
        Builder {
            frames: vec![Frame { style: base, transform: Affine::IDENTITY, opacity: 1.0, children: Vec::new() }],
            skip: 0,
            shapes: 0,
            warnings: Vec::new(),
            root_seen: false,
            vb: None,
            size: None,
        }
    }

    fn feed(&mut self, tag: Tag) {
        if self.skip > 0 {
            if tag.close {
                self.skip -= 1;
            } else if !tag.self_close {
                self.skip += 1;
            }
            return;
        }
        if tag.close {
            if self.frames.len() > 1 {
                let f = self.frames.pop().unwrap_or_else(|| unreachable!());
                let node = Node::Group { transform: f.transform, opacity: f.opacity, children: f.children };
                if let Some(top) = self.frames.last_mut() {
                    top.children.push(node);
                }
            }
            return;
        }
        match tag.name.as_str() {
            "svg" | "g" => {
                let mut style = self.frames.last().map_or_else(SvgStyle::default, |f| f.style.clone());
                let (opacity, hidden) = cascade(&mut style, &tag.attrs, &mut self.warnings);
                if tag.name == "svg" && !self.root_seen {
                    self.root_seen = true;
                    self.read_root(&tag);
                } else if tag.name == "svg" {
                    self.warnings.push("nested <svg> treated as <g>".into());
                }
                let transform = self.transform_of(&tag);
                if hidden {
                    if !tag.self_close {
                        self.skip = 1;
                    }
                    return;
                }
                if tag.self_close {
                    return;
                }
                self.frames.push(Frame { style, transform, opacity, children: Vec::new() });
            }
            "path" | "circle" | "ellipse" | "rect" | "line" | "polyline" | "polygon" => {
                self.shape(&tag);
                if !tag.self_close {
                    self.skip = 1; // children of a shape are ignored
                }
            }
            other => {
                self.warnings.push(format!("<{other}> is not supported and was skipped"));
                if !tag.self_close {
                    self.skip = 1;
                }
            }
        }
    }

    fn transform_of(&mut self, tag: &Tag) -> Affine {
        match tag.attr("transform").map(Affine::parse) {
            Some(Ok(m)) => m,
            Some(Err(e)) => {
                self.warnings.push(format!("transform {:?}: {e}", tag.attr("transform").unwrap_or("")));
                Affine::IDENTITY
            }
            None => Affine::IDENTITY,
        }
    }

    fn read_root(&mut self, tag: &Tag) {
        if let Some(v) = tag.attr("viewBox").or_else(|| tag.attr("viewbox")) {
            let n = Sc::new(v).numbers();
            if let [x, y, w, h] = n[..] {
                if w > 0.0 && h > 0.0 {
                    self.vb = Some((x, y, w, h));
                }
            }
        }
        let (w, h) = (tag.attr("width").and_then(num_attr), tag.attr("height").and_then(num_attr));
        if let (Some(w), Some(h)) = (w, h) {
            self.size = Some((w, h));
        }
        if tag.attr("preserveAspectRatio").is_some_and(|p| p.trim() != "xMidYMid meet" && p.trim() != "xMidYMid") {
            self.warnings.push("preserveAspectRatio other than xMidYMid meet is ignored".into());
        }
    }

    fn shape(&mut self, tag: &Tag) {
        let mut style = self.frames.last().map_or_else(SvgStyle::default, |f| f.style.clone());
        let (opacity, hidden) = cascade(&mut style, &tag.attrs, &mut self.warnings);
        if hidden {
            return;
        }
        let f = |n: &str| tag.attr(n).and_then(num_attr);
        let (path, line_only) = match tag.name.as_str() {
            "path" => {
                let d = tag.attr("d").unwrap_or("");
                let (p, e) = Path::parse_prefix(d);
                if let Some(e) = e {
                    self.warnings.push(format!("path data: {e}"));
                }
                (p, false)
            }
            "circle" => {
                let r = f("r").unwrap_or(0.0);
                (if r > 0.0 { ellipse_path(f("cx").unwrap_or(0.0), f("cy").unwrap_or(0.0), r, r) } else { Path::new() }, false)
            }
            "ellipse" => {
                let (rx, ry) = (f("rx").unwrap_or(0.0), f("ry").unwrap_or(0.0));
                (if rx > 0.0 && ry > 0.0 { ellipse_path(f("cx").unwrap_or(0.0), f("cy").unwrap_or(0.0), rx, ry) } else { Path::new() }, false)
            }
            "rect" => {
                let (w, h) = (f("width").unwrap_or(0.0), f("height").unwrap_or(0.0));
                let (rx, ry) = match (f("rx"), f("ry")) {
                    (Some(a), Some(b)) => (a, b),
                    (Some(a), None) | (None, Some(a)) => (a, a),
                    _ => (0.0, 0.0),
                };
                (if w > 0.0 && h > 0.0 { rect_path(f("x").unwrap_or(0.0), f("y").unwrap_or(0.0), w, h, rx.max(0.0), ry.max(0.0)) } else { Path::new() }, false)
            }
            "line" => (Path { segs: vec![Seg::Move(f("x1").unwrap_or(0.0), f("y1").unwrap_or(0.0)), Seg::Line(f("x2").unwrap_or(0.0), f("y2").unwrap_or(0.0))] }, true),
            "polyline" => (poly_path(tag.attr("points").unwrap_or(""), false), false),
            _ => (poly_path(tag.attr("points").unwrap_or(""), true), false),
        };
        if path.is_empty() {
            return;
        }
        let _ = line_only; // a two-point path has no area: the fill draws nothing, no special case needed
        let transform = self.transform_of(tag);
        let idx = self.shapes;
        self.shapes += 1;
        if let Some(top) = self.frames.last_mut() {
            top.children.push(Node::Shape(Shape { idx, path, transform, style, opacity }));
        }
    }

    fn finish(mut self) -> Svg {
        while self.frames.len() > 1 {
            // unclosed groups: close them
            self.feed(Tag { name: String::new(), attrs: Vec::new(), close: true, self_close: false });
        }
        let root = self.frames.pop().map(|f| f.children).unwrap_or_default();
        let bounds = || {
            let mut acc: Option<Rect> = None;
            fn walk(n: &[Node], m: &Affine, acc: &mut Option<Rect>) {
                for x in n {
                    match x {
                        Node::Group { transform, children, .. } => walk(children, &m.mul(transform), acc),
                        Node::Shape(s) => {
                            let r = s.path.transformed(&m.mul(&s.transform)).bounds();
                            *acc = Some(match *acc {
                                None => r,
                                Some(a) => {
                                    let (x0, y0) = (a.x.min(r.x), a.y.min(r.y));
                                    Rect::new(x0, y0, a.right().max(r.right()) - x0, a.bottom().max(r.bottom()) - y0)
                                }
                            });
                        }
                    }
                }
            }
            walk(&root, &Affine::IDENTITY, &mut acc);
            acc.filter(|r| r.w > 0.0 && r.h > 0.0).map_or((0.0, 0.0, 1.0, 1.0), |r| (r.x, r.y, r.w, r.h))
        };
        let vb = match (self.vb, self.size) {
            (Some(v), _) => v,
            (None, Some((w, h))) => (0.0, 0.0, w, h),
            (None, None) => bounds(),
        };
        let size = self.size.unwrap_or((vb.2, vb.3));
        Svg { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), vb, size, root, shapes: self.shapes, warnings: self.warnings }
    }
}

impl Svg {
    /// Read a standalone document (`<svg viewBox=...>...</svg>`) or a bare element list. Never fails: what is not understood is skipped and
    /// recorded in [`Svg::warnings`]. Without a `viewBox` the `width`/`height`, else the content bounds, define the box.
    pub fn parse(markup: &str) -> Svg {
        let mut b = Builder::new(SvgStyle::default());
        for t in tokenize(markup) {
            b.feed(t);
        }
        b.finish()
    }

    /// Read INNER markup (no `<svg>` wrapper), the way `icons.js` / `pets*.js` store art: `view_box` = (x, y, w, h) and `defaults` are the
    /// wrapper's presentation attributes (see [`SvgStyle::line_icon`]).
    pub fn from_fragment(inner: &str, view_box: (f32, f32, f32, f32), defaults: SvgStyle) -> Svg {
        let mut b = Builder::new(defaults);
        b.vb = Some(view_box);
        b.root_seen = true; // a nested <svg> in a fragment is not a root
        for t in tokenize(inner) {
            b.feed(t);
        }
        b.finish()
    }

    /// A Pill line icon: inner markup in a 24 x 24 box, no fill, `currentColor` stroke of `stroke_width` (1.7 in the web UI), round caps and joins.
    pub fn icon(inner: &str, stroke_width: f32) -> Svg {
        Svg::from_fragment(inner, (0.0, 0.0, 24.0, 24.0), SvgStyle::line_icon(stroke_width))
    }

    /// The `viewBox` (x, y, w, h) the art is fitted from.
    pub fn view_box(&self) -> (f32, f32, f32, f32) {
        self.vb
    }

    /// Natural size: the root's `width` x `height`, else the viewBox size.
    pub fn size(&self) -> (f32, f32) {
        self.size
    }

    /// What was skipped or not understood while reading.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Number of drawable shapes.
    pub fn shape_count(&self) -> usize {
        self.shapes as usize
    }

    /// The matrix that fits the viewBox into `dest` (`xMidYMid meet`).
    pub fn fit(&self, dest: Rect) -> Affine {
        let (vx, vy, vw, vh) = self.vb;
        let s = (dest.w / vw).min(dest.h / vh);
        let (ox, oy) = (dest.x + (dest.w - vw * s) / 2.0, dest.y + (dest.h - vh * s) / 2.0);
        Affine::translate(ox, oy).mul(&Affine::scale(s, s)).mul(&Affine::translate(-vx, -vy))
    }

    /// Paint into `dest` (logical px): the viewBox is fitted `xMidYMid meet` and the drawing is clipped to `dest`. `tint` is `currentColor`.
    pub fn draw(&self, cv: &Canvas, dest: Rect, tint: Color, cache: &GeomCache) {
        if dest.w <= 0.0 || dest.h <= 0.0 || self.vb.2 <= 0.0 || self.vb.3 <= 0.0 {
            return;
        }
        let fit = self.fit(dest);
        cv.with_clip(dest, |cv| self.draw_nodes(cv, &self.root, &fit, tint, cache));
    }

    fn draw_nodes(&self, cv: &Canvas, nodes: &[Node], parent: &Affine, tint: Color, cache: &GeomCache) {
        for n in nodes {
            match n {
                Node::Group { transform, opacity, children } => {
                    let m = parent.mul(transform);
                    cv.with_opacity(*opacity, |cv| self.draw_nodes(cv, children, &m, tint, cache));
                }
                Node::Shape(s) => {
                    let m = parent.mul(&s.transform);
                    cv.with_opacity(s.opacity, |cv| self.draw_shape(cv, s, &m, tint, cache));
                }
            }
        }
    }

    fn draw_shape(&self, cv: &Canvas, s: &Shape, m: &Affine, tint: Color, cache: &GeomCache) {
        let paint = |p: Paint, k: f32| match p {
            Paint::None => None,
            Paint::Current => Some(tint.fade(k)),
            Paint::Color(c) => Some(c.fade(k)),
        };
        let fill = paint(s.style.fill, s.style.fill_opacity).filter(|c| c.a > 0.0);
        let width = s.style.stroke_width * m.scale_factor();
        let stroke = paint(s.style.stroke, s.style.stroke_opacity).filter(|c| c.a > 0.0 && width > 0.0);
        if fill.is_none() && stroke.is_none() {
            return;
        }
        let Ok(g) = cache.node(cv.gfx, self.id, s.idx, &s.path, m, s.style.evenodd) else { return };
        let Ok(geom) = g.cast::<ID2D1Geometry>() else { return };
        cv.geometry(&geom, fill, None);
        if let Some(c) = stroke {
            if let Ok(ss) = cache.stroke_style(cv.gfx, &s.style, s.style.stroke_width) {
                unsafe {
                    cv.res.brush().SetColor(&c.d2d());
                    cv.dc.DrawGeometry(&geom, cv.res.brush(), width, &ss);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Harness;

    const ICONS: &str = include_str!("svgpath_testdata/icons.txt");
    const PET_PATHS: &str = include_str!("svgpath_testdata/pet_paths.txt");

    /// (name, inner markup) of every icon of icons.js
    fn icons() -> Vec<(String, String)> {
        ICONS
            .lines()
            .filter_map(|l| {
                let (name, rest) = l.trim().split_once(':')?;
                let a = rest.find('\'')?;
                let b = rest.rfind('\'')?;
                Some((name.trim().to_string(), rest[a + 1..b].to_string()))
            })
            .collect()
    }

    /// every `d="..."` value of a markup string
    fn ds(markup: &str) -> Vec<String> {
        let mut v = Vec::new();
        let mut rest = markup;
        while let Some(i) = rest.find(" d=\"") {
            rest = &rest[i + 4..];
            let e = rest.find('"').unwrap_or(rest.len());
            v.push(rest[..e].to_string());
            rest = &rest[e..];
        }
        v
    }

    fn near(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    fn pts(p: &Path) -> Vec<Seg> {
        p.segs.clone()
    }

    // ------------------------------------------------ numbers and commands

    #[test]
    fn numbers_in_every_compact_form() {
        let mut s = Sc::new("1.5.5 10-5 -.5e-1,+3 1e2 7e");
        assert_eq!(s.number(), Some(1.5));
        assert_eq!(s.number(), Some(0.5));
        assert_eq!(s.number(), Some(10.0));
        assert_eq!(s.number(), Some(-5.0));
        assert_eq!(s.number(), Some(-0.05));
        assert_eq!(s.number(), Some(3.0));
        assert_eq!(s.number(), Some(100.0));
        assert_eq!(s.number(), Some(7.0), "a trailing 'e' with no digits is not an exponent");
        assert_eq!(s.number(), None);
        assert_eq!(Sc::new("abc").number(), None);
        assert_eq!(Sc::new("-").number(), None);
        assert_eq!(Sc::new(".").number(), None);
    }

    #[test]
    fn relative_and_absolute_agree() {
        let a = Path::parse("M10 10L20 10L20 20L10 20Z").unwrap();
        let b = Path::parse("m10 10l10 0l0 10l-10 0z").unwrap();
        assert_eq!(a, b);
        let c = Path::parse("M10 10H20V20H10z").unwrap();
        assert_eq!(a, c);
    }

    #[test]
    fn implicit_repeats_and_moveto_pairs() {
        // pairs after M are linetos; repeated C takes more triples
        let p = Path::parse("M0 0 10 0 10 10").unwrap();
        assert_eq!(pts(&p), vec![Seg::Move(0.0, 0.0), Seg::Line(10.0, 0.0), Seg::Line(10.0, 10.0)]);
        let p = Path::parse("m1 1 2 0 0 2").unwrap();
        assert_eq!(pts(&p), vec![Seg::Move(1.0, 1.0), Seg::Line(3.0, 1.0), Seg::Line(3.0, 3.0)]);
        let p = Path::parse("M0 0C1 1 2 2 3 3 4 4 5 5 6 6").unwrap();
        assert_eq!(p.segs.len(), 3);
    }

    #[test]
    fn smooth_curves_reflect_the_previous_control_point() {
        let p = Path::parse("M0 0C0 10 10 10 10 0S20 -10 20 0").unwrap();
        match p.segs[2] {
            Seg::Cubic(c1, _, e) => {
                assert_eq!(c1, (10.0, -10.0), "reflection of (10,10) through (10,0)");
                assert_eq!(e, (20.0, 0.0));
            }
            _ => panic!(),
        }
        // S after a line has no reflection: the first control point is the current point
        let p = Path::parse("M0 0L5 5S9 9 10 10").unwrap();
        match p.segs[2] {
            Seg::Cubic(c1, _, _) => assert_eq!(c1, (5.0, 5.0)),
            _ => panic!(),
        }
        let q = Path::parse("M0 0Q5 10 10 0T20 0").unwrap();
        match q.segs[2] {
            Seg::Cubic(_, c2, e) => {
                assert_eq!(e, (20.0, 0.0));
                // the quadratic's control point is (15,-10): c2 = e + 2/3 (q - e)
                assert!(near(c2.0, 20.0 + 2.0 / 3.0 * (15.0 - 20.0), 1e-4) && near(c2.1, 2.0 / 3.0 * -10.0, 1e-4));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn quadratic_promotion_is_exact() {
        let p = Path::parse("M0 0Q10 20 20 0").unwrap();
        let Seg::Cubic(c1, c2, e) = p.segs[1] else { panic!() };
        // midpoint of the quadratic is (10, 10)
        let m = cubic_at((0.0, 0.0), c1, c2, e, 0.5);
        assert!(near(m.0, 10.0, 1e-4) && near(m.1, 10.0, 1e-4), "{m:?}");
    }

    #[test]
    fn close_returns_to_the_start_and_a_following_draw_restarts_there() {
        let p = Path::parse("M5 5h10v10zl3 0").unwrap();
        assert_eq!(p.segs, vec![Seg::Move(5.0, 5.0), Seg::Line(15.0, 5.0), Seg::Line(15.0, 15.0), Seg::Close, Seg::Move(5.0, 5.0), Seg::Line(8.0, 5.0)]);
        // a double z adds one close
        assert_eq!(Path::parse("M0 0h1zz").unwrap().segs.len(), 3);
    }

    #[test]
    fn errors_carry_the_offset_and_the_prefix_survives() {
        assert_eq!(Path::parse("").unwrap().segs.len(), 0);
        assert_eq!(Path::parse("L 1 2").unwrap_err().offset, 0, "must start with a moveto");
        let e = Path::parse("M0 0 L 10").unwrap_err();
        assert_eq!(e.msg, "missing or malformed number");
        assert_eq!(e.offset, 6, "points at the first number of the broken command: {e}");
        assert_eq!(Path::parse("M0 0 X1").unwrap_err().offset, 5);
        assert!(Path::parse("M0 0 z 5 5").is_err(), "numbers after z need a command");
        assert!(Path::parse("M0 0 A1 1 0 2 0 5 5").is_err(), "flag must be 0 or 1");
        let (p, e) = Path::parse_prefix("M0 0 L10 0 L10");
        assert_eq!(p.segs.len(), 2);
        assert!(e.is_some());
        // never panics on junk
        for junk in ["M", "M1", "MM", "M1,2,", "M0 0 C1", "é", "M\u{0}", "M0 0 a", "M1e999 0", ",,,", "z", "M0 0 Q1 1"] {
            let _ = Path::parse_prefix(junk);
        }
        assert!(Path::parse("M1e999 0").is_err(), "non-finite numbers are rejected");
    }

    #[test]
    fn compact_arc_flags_without_separators() {
        // flags "0" and "1" glued to each other and to the next number: a1 1 0 011 1
        let a = Path::parse("M0 0a1 1 0 011 1").unwrap();
        let b = Path::parse("M0 0a1 1 0 0 1 1 1").unwrap();
        assert_eq!(a, b);
        assert!(Path::parse("M20 13c0 5-3.5 7.5-7.7 9a1 1 0 0 1-.7 0").unwrap().segs.len() >= 3);
        let c = Path::parse("M0 0a1 1 0 1 0 1 1 1 1 0 0 1 1 1").unwrap(); // implicit repeat of A
        assert!(c.segs.len() > 2);
    }

    #[test]
    fn the_real_world_path_from_the_task_parses() {
        let d = "M20 13c0 5-3.5 7.5-7.7 9a1 1 0 0 1-.7 0C7.5 20.500 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.500-1.200 6.200-2.700a1.200 1.200 0 0 1 1.500 0C14.500 3.800 17 5 19 5a1 1 0 0 1 1 1z";
        let p = Path::parse(d).unwrap();
        let b = p.bounds();
        assert!(b.x >= 3.99 && b.right() <= 20.01 && b.y >= 1.5 && b.bottom() <= 22.1, "{b:?}");
        assert!(matches!(p.segs.last(), Some(Seg::Close)));
    }

    // ------------------------------------------------ arcs

    fn arc_points(p: &Path) -> Vec<P> {
        let mut out = Vec::new();
        let mut cur = (0.0, 0.0);
        for s in &p.segs {
            match *s {
                Seg::Move(x, y) | Seg::Line(x, y) => cur = (x, y),
                Seg::Cubic(a, b, c) => {
                    for k in 0..=8 {
                        out.push(cubic_at(cur, a, b, c, k as f32 / 8.0));
                    }
                    cur = c;
                }
                Seg::Close => {}
            }
        }
        out
    }

    #[test]
    fn arcs_stay_on_the_circle() {
        // quarter circle of radius 10 around the origin, clockwise (sweep 1)
        let p = Path::parse("M10 0A10 10 0 0 1 0 10").unwrap();
        assert_eq!(p.segs.len(), 2);
        for q in arc_points(&p) {
            assert!(near((q.0 * q.0 + q.1 * q.1).sqrt(), 10.0, 0.02), "{q:?}");
            assert!(q.0 >= -0.01 && q.1 >= -0.01, "clockwise on screen goes through the lower right: {q:?}");
        }
        // the same chord, other sweep: through the upper left, centre (10, 10)
        let p = Path::parse("M10 0A10 10 0 0 0 0 10").unwrap();
        for q in arc_points(&p) {
            assert!(near(((q.0 - 10.0).powi(2) + (q.1 - 10.0).powi(2)).sqrt(), 10.0, 0.02), "{q:?}");
        }
        // large arc: three quarters of the circle around (10, 10), clockwise through the right and the bottom, ending exactly on the end point
        let p = Path::parse("M10 0A10 10 0 1 1 0 10").unwrap();
        assert!(p.segs.len() >= 4);
        let b = p.bounds();
        assert!(near(b.x, 0.0, 0.02) && near(b.y, 0.0, 0.02) && near(b.w, 20.0, 0.02) && near(b.h, 20.0, 0.02), "{b:?}");
        assert!(matches!(p.segs.last(), Some(Seg::Cubic(_, _, e)) if *e == (0.0, 10.0)));
    }

    #[test]
    fn arc_edge_cases() {
        // radii too small for the chord are scaled up: a half circle of radius 5
        let p = Path::parse("M0 0A1 1 0 0 1 10 0").unwrap();
        let b = p.bounds();
        assert!(near(b.h, 5.0, 0.02) && near(b.w, 10.0, 0.02), "{b:?}");
        // zero radius = straight line
        assert_eq!(Path::parse("M0 0A0 5 0 0 1 10 0").unwrap().segs[1], Seg::Line(10.0, 0.0));
        // same end point: skipped
        assert_eq!(Path::parse("M3 3A5 5 0 0 1 3 3").unwrap().segs.len(), 1);
        // negative radii use the absolute value
        assert_eq!(Path::parse("M10 0A-10 -10 0 0 1 0 10").unwrap(), Path::parse("M10 0A10 10 0 0 1 0 10").unwrap());
        // rotated ellipse: x radius 10 along 45 degrees; the full ellipse fits in its rotated box
        let p = Path::parse("M-5 0A10 4 45 1 1 5 0").unwrap();
        assert!(p.bounds().w > 5.0);
    }

    // ------------------------------------------------ bounds, affine

    #[test]
    fn bounds_are_exact_for_curves() {
        let p = Path::parse("M0 0C0 10 10 10 10 0").unwrap();
        let b = p.bounds();
        assert!(near(b.h, 7.5, 1e-3), "extremum at t = .5 is 7.5, not the control point 10: {b:?}");
        assert!(near(b.w, 10.0, 1e-4));
        let c = Path::parse("M10 10A10 10 0 1 1 10.001 10").unwrap().bounds();
        assert!(c.w > 19.0 && c.h > 19.0, "a full circle: {c:?}");
        assert_eq!(Path::new().bounds(), Rect::default());
        // control point outside on one side only
        let q = Path::parse("M0 0C5 -20 15 -20 20 0").unwrap().bounds();
        assert!(near(q.y, -15.0, 1e-3) && near(q.bottom(), 0.0, 1e-4), "{q:?}");
    }

    #[test]
    fn transform_grammar() {
        let m = Affine::parse("translate(10 20) scale(2)").unwrap();
        assert_eq!(m.apply((1.0, 1.0)), (12.0, 22.0), "scale first, then translate (leftmost is outermost)");
        let r = Affine::parse("rotate(90)").unwrap().apply((1.0, 0.0));
        assert!(near(r.0, 0.0, 1e-6) && near(r.1, 1.0, 1e-6), "{r:?}: clockwise on screen");
        let rc = Affine::parse("rotate(90, 5, 5)").unwrap().apply((10.0, 5.0));
        assert!(near(rc.0, 5.0, 1e-5) && near(rc.1, 10.0, 1e-5), "{rc:?}");
        assert_eq!(Affine::parse("matrix(1 0 0 1 3 4)").unwrap().apply((0.0, 0.0)), (3.0, 4.0));
        assert_eq!(Affine::parse("translate(5)").unwrap().apply((0.0, 0.0)), (5.0, 0.0));
        assert_eq!(Affine::parse("scale(2 3)").unwrap().apply((1.0, 1.0)), (2.0, 3.0));
        let sk = Affine::parse("skewX(45)").unwrap().apply((0.0, 2.0));
        assert!(near(sk.0, 2.0, 1e-5));
        assert!(near(Affine::parse("skewY(45)").unwrap().apply((2.0, 0.0)).1, 2.0, 1e-5));
        assert_eq!(Affine::parse("").unwrap(), Affine::IDENTITY);
        assert_eq!(Affine::parse("translate(-6 7) scale(.65)").unwrap().apply((10.0, 0.0)), (-6.0 + 6.5, 7.0));
        assert_eq!(Affine::parse("translate(3,-5)").unwrap().apply((0.0, 0.0)), (3.0, -5.0));
        assert!(Affine::parse("translate(1").is_err());
        assert!(Affine::parse("shear(1)").is_err());
        assert!(Affine::parse("scale(1 2 3)").is_err());
        // mul order: self applied after o
        let a = Affine::translate(10.0, 0.0);
        let b = Affine::scale(2.0, 2.0);
        assert_eq!(a.mul(&b).apply((1.0, 1.0)), (12.0, 2.0));
        assert!(near(Affine::scale(2.0, 8.0).scale_factor(), 4.0, 1e-5));
        let t = Path::parse("M0 0L10 0").unwrap().transformed(&Affine::scale(2.0, 2.0));
        assert_eq!(t.segs[1], Seg::Line(20.0, 0.0));
    }

    // ------------------------------------------------ real art

    #[test]
    fn every_icon_path_parses_and_stays_inside_its_24_box() {
        let icons = icons();
        assert!(icons.len() > 60, "{}", icons.len());
        let mut n = 0;
        for (name, markup) in &icons {
            for d in ds(markup) {
                let p = Path::parse(&d).unwrap_or_else(|e| panic!("icon {name}: {e}: {d}"));
                let b = p.bounds();
                assert!(b.x >= -0.05 && b.y >= -0.05 && b.right() <= 24.05 && b.bottom() <= 24.05, "icon {name}: {b:?}");
                n += 1;
            }
        }
        assert!(n > 80, "{n} paths");
    }

    #[test]
    fn every_icon_reads_as_a_svg_without_warnings() {
        for (name, markup) in icons() {
            let s = Svg::icon(&markup, 1.7);
            assert!(s.warnings().is_empty(), "{name}: {:?}", s.warnings());
            assert!(s.shape_count() >= 1, "{name}");
            assert_eq!(s.view_box(), (0.0, 0.0, 24.0, 24.0));
        }
    }

    #[test]
    fn every_pet_path_parses() {
        let mut n = 0;
        for line in PET_PATHS.lines() {
            let d = line.trim().trim_matches('"');
            if d.is_empty() {
                continue;
            }
            let p = Path::parse(d).unwrap_or_else(|e| panic!("{e}: {d}"));
            let b = p.bounds();
            assert!(b.w.is_finite() && b.h.is_finite());
            // art is drawn around a local origin or inside the 48 box: never wildly outside
            assert!(b.x > -30.0 && b.y > -30.0 && b.right() < 130.0 && b.bottom() < 130.0, "{b:?}: {d}");
            n += 1;
        }
        assert!(n > 90, "{n}");
    }

    #[test]
    fn pet_style_fragments_read_with_groups_transforms_and_fills() {
        // constructs copied from pets-chars.js: circle without cx, fractional rx, nested groups with transform, per-element fills
        let m = r##"<g class="exb"><circle class="exc" r="5.6"/><rect x="-.9" y="-3.3" width="1.8" height="4.5" rx=".9" fill="#2a1d08"/><circle cy="2.7" r="1.05" fill="#2a1d08"/></g><g class="s s2"><path d="M0-3L.8-.8L3 0L.8.8L0 3L-.8.8L-3 0L-.8-.8Z" fill="#fff3b0" transform="translate(-6 7) scale(.65)"/></g>"##;
        let s = Svg::from_fragment(m, (0.0, 0.0, 48.0, 48.0), SvgStyle::default());
        assert!(s.warnings().is_empty(), "{:?}", s.warnings());
        assert_eq!(s.shape_count(), 4);
        let Node::Group { children, .. } = &s.root[0] else { panic!() };
        let Node::Shape(rect) = &children[1] else { panic!() };
        assert_eq!(rect.style.fill, Paint::Color(Color::rgb8(0x2a, 0x1d, 0x08)));
        let b = rect.path.bounds();
        assert!(near(b.x, -0.9, 1e-3) && near(b.w, 1.8, 1e-3));
        let Node::Group { children, .. } = &s.root[1] else { panic!() };
        let Node::Shape(star) = &children[0] else { panic!() };
        assert_eq!(star.transform.apply((0.0, 0.0)), (-6.0, 7.0));
    }

    // ------------------------------------------------ svg reader

    #[test]
    fn reader_inherits_style_and_skips_the_unknown() {
        let s = Svg::parse(
            r##"<?xml version="1.0"?><!-- c --><svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 10 20" width="40" height="80">
              <defs><linearGradient id="g"><stop offset="0"/></linearGradient></defs>
              <g fill="#f00" stroke="blue" stroke-width="2" opacity=".5" transform="translate(1 1)">
                <path d="M0 0h5" stroke-linecap="square" fill-opacity=".25"/>
                <circle cx="5" cy="5" r="2" style="fill:none;stroke-dasharray:2 1 1"/>
                <text x="1">hi<tspan>x</tspan></text>
                <rect width="4" height="4" display="none"/>
                <polygon points="0,0 4,0 4,4"/>
                <polyline points="0 0 1 1"/>
                <line x1="0" y1="0" x2="3" y2="3"/>
                <ellipse cx="1" cy="1" rx="2" ry="1"/>
              </g>
              <path d="M0 0L1 1" fill="url(#g)"/>
            </svg>"##,
        );
        assert_eq!(s.view_box(), (0.0, 0.0, 10.0, 20.0));
        assert_eq!(s.size(), (40.0, 80.0));
        assert_eq!(s.shape_count(), 7, "path, circle, polygon, polyline, line, ellipse, gradient path; text and hidden rect skipped");
        assert!(s.warnings().iter().any(|w| w.contains("<defs>")) && s.warnings().iter().any(|w| w.contains("<text>")) && s.warnings().iter().any(|w| w.contains("gradients")), "{:?}", s.warnings());
        let Node::Group { children: root, .. } = &s.root[0] else { panic!("the root svg is a group") };
        let Node::Group { opacity, transform, children } = &root[0] else { panic!() };
        assert_eq!((*opacity, transform.e), (0.5, 1.0));
        let Node::Shape(p) = &children[0] else { panic!() };
        assert_eq!((p.style.cap, p.style.stroke_width, p.style.fill_opacity), (Cap::Square, 2.0, 0.25));
        assert_eq!(p.style.stroke, Paint::Color(Color::rgb8(0, 0, 255)));
        let Node::Shape(c) = &children[1] else { panic!() };
        assert_eq!((c.style.fill, c.style.dash.clone()), (Paint::None, vec![2.0, 1.0, 1.0, 2.0, 1.0, 1.0]), "an odd dash list repeats");
        assert_eq!(c.opacity, 1.0, "opacity is not inherited by the child, the group carries it");
        assert_eq!(s.warnings().len(), 3);
    }

    #[test]
    fn viewbox_falls_back_to_size_then_to_content() {
        assert_eq!(Svg::parse(r#"<svg width="30" height="20"><rect width="1" height="1"/></svg>"#).view_box(), (0.0, 0.0, 30.0, 20.0));
        let s = Svg::parse(r#"<rect x="2" y="3" width="10" height="4"/>"#);
        assert_eq!(s.view_box(), (2.0, 3.0, 10.0, 4.0));
        assert_eq!(Svg::parse("").view_box(), (0.0, 0.0, 1.0, 1.0));
        // broken markup never panics
        for junk in ["<", "<svg", "<svg viewBox=", "<g><g><path d='M'/>", "</g></g>", "<path d=\"M0 0 L", "<a b='c", "<<<>>>"] {
            let _ = Svg::parse(junk);
        }
    }

    #[test]
    fn colours() {
        let c = |s: &str| parse_color(s);
        assert_eq!(c("#fff"), Some(Paint::Color(Color::rgb8(255, 255, 255))));
        assert_eq!(c("#ff7aa2"), Some(Paint::Color(Color::rgb8(255, 122, 162))));
        assert_eq!(c("#ff000080"), Some(Paint::Color(Color::rgb8(255, 0, 0).alpha(128.0 / 255.0))));
        assert_eq!(c("rgb(1, 2, 3)"), Some(Paint::Color(Color::rgb8(1, 2, 3))));
        assert_eq!(c("rgba(255,0,0,.5)"), Some(Paint::Color(Color::rgb8(255, 0, 0).alpha(0.5))));
        assert_eq!(c("currentColor"), Some(Paint::Current));
        assert_eq!(c("NONE"), Some(Paint::None));
        assert_eq!(c("transparent"), Some(Paint::Color(Color::rgb8(0, 0, 0).alpha(0.0))));
        assert_eq!(c("#12"), None);
        assert_eq!(c("papayawhip"), None);
    }

    #[test]
    fn element_shapes() {
        let s = Svg::parse(r#"<svg viewBox="0 0 20 20"><rect x="2" y="2" width="10" height="6" rx="2"/><circle cx="10" cy="10" r="3"/></svg>"#);
        let Node::Group { children, .. } = &s.root[0] else { panic!() };
        let Node::Shape(r) = &children[0] else { panic!() };
        let b = r.path.bounds();
        assert!(near(b.x, 2.0, 1e-4) && near(b.right(), 12.0, 1e-4) && near(b.bottom(), 8.0, 1e-3), "{b:?}");
        let Node::Shape(c) = &children[1] else { panic!() };
        let b = c.path.bounds();
        assert!(near(b.w, 6.0, 1e-3) && near(b.cx(), 10.0, 1e-3), "{b:?}");
        // rx larger than half the width is clamped
        let p = rect_path(0.0, 0.0, 10.0, 10.0, 50.0, 50.0).bounds();
        assert!(near(p.w, 10.0, 1e-3));
    }

    // ------------------------------------------------ Direct2D

    fn bounds_of(g: &ID2D1PathGeometry) -> Rect {
        let b = unsafe { g.GetBounds(None).unwrap() };
        Rect::new(b.left, b.top, b.right - b.left, b.bottom - b.top)
    }

    #[test]
    fn geometry_matches_the_path_bounds_under_a_transform() {
        let h = Harness::new();
        let p = Path::parse("M2 3C2 10 12 10 12 3L12 20Z").unwrap();
        let m = Affine::translate(5.0, 1.0).mul(&Affine::scale(2.0, 2.0));
        let g = build_geometry(&h.gfx, &p, &m, true).unwrap();
        let (gb, pb) = (bounds_of(&g), p.transformed(&m).bounds());
        assert!(near(gb.x, pb.x, 0.05) && near(gb.y, pb.y, 0.05) && near(gb.right(), pb.right(), 0.05) && near(gb.bottom(), pb.bottom(), 0.05), "{gb:?} vs {pb:?}");
    }

    #[test]
    fn the_cache_reuses_by_source_transform_and_mode() {
        let h = Harness::new();
        let c = GeomCache::new();
        assert!(c.is_empty());
        let m = Affine::IDENTITY;
        c.get(&h.gfx, "M0 0h5v5z", &m, true).unwrap();
        c.get(&h.gfx, "M0 0h5v5z", &m, true).unwrap();
        assert_eq!(c.len(), 1);
        c.get(&h.gfx, "M0 0h5v5z", &Affine::scale(2.0, 2.0), true).unwrap();
        c.get(&h.gfx, "M0 0h5v5z", &m, false).unwrap();
        c.get_rule(&h.gfx, "M0 0h5v5z", &m, true, true).unwrap();
        assert_eq!(c.len(), 4);
        // broken data still gives the valid prefix
        let g = c.get(&h.gfx, "M0 0h5v5 L", &m, true).unwrap();
        assert!(bounds_of(&g).w > 4.9);
        for i in 0..GeomCache::LIMIT + 5 {
            c.get(&h.gfx, &format!("M0 0h{}", i + 1), &m, false).unwrap();
        }
        assert!(c.len() <= GeomCache::LIMIT, "bounded: {}", c.len());
        c.clear();
        assert!(c.is_empty());
    }

    const BLACK: Color = Color::rgba(0.0, 0.0, 0.0, 1.0);

    #[test]
    fn an_icon_draws_inside_its_rect_only_at_both_scales() {
        let h = Harness::new();
        let cache = GeomCache::new();
        // "plus" with a stroke that would reach the edge of the box if it were not clipped
        let icon = Svg::icon(r#"<path d="M12 0v24M0 12h24"/>"#, 4.0);
        for scale in [1.0f32, 2.0] {
            let dest = Rect::new(10.0, 6.0, 20.0, 20.0);
            let img = h.render(40, 32, scale, BLACK, |cv| icon.draw(cv, dest, Color::rgb8(255, 255, 255), &cache));
            let lit = |p: [u8; 4]| p[0] > 8;
            let (x0, y0, x1, y1) = img.bounds(lit).expect("something was drawn");
            assert_eq!((x0, y0, x1, y1), ((10.0 * scale) as u32, (6.0 * scale) as u32, (30.0 * scale) as u32, (26.0 * scale) as u32), "scale {scale}: the clip to dest is the only limit");
            // the middle of the vertical bar is lit, a corner of the box is not
            assert!(img.at(20.0, 10.0)[0] > 240);
            assert_eq!(img.at(12.0, 8.0)[0], 0);
            // stroke width follows the fit: 4 user units * 20/24 = 3.33 px wide
            let row = (0..img.w).filter(|&x| img.px(x, (8.0 * scale) as u32)[0] > 128).count() as f32 / scale;
            assert!(near(row, 3.33, 1.0), "{row}");
        }
    }

    #[test]
    fn a_stroked_quarter_circle_lies_on_its_radius() {
        let h = Harness::new();
        let cache = GeomCache::new();
        let s = Svg::parse(r##"<svg viewBox="0 0 40 40"><path d="M30 20A10 10 0 0 1 20 30" fill="none" stroke="#fff" stroke-width="2"/></svg>"##);
        let img = h.render(40, 40, 2.0, BLACK, |cv| s.draw(cv, Rect::new(0.0, 0.0, 40.0, 40.0), Color::rgb8(255, 255, 255), &cache));
        let (cx, cy) = (20.0, 20.0);
        let a = std::f32::consts::FRAC_PI_4;
        let on = img.at(cx + 10.0 * a.cos(), cy + 10.0 * a.sin());
        assert!(on[0] > 200, "{on:?}");
        for r in [6.0f32, 14.0] {
            assert_eq!(img.at(cx + r * a.cos(), cy + r * a.sin())[0], 0, "radius {r}");
        }
        // the other quadrants stay empty
        assert_eq!(img.at(cx - 10.0 * a.cos(), cy - 10.0 * a.sin())[0], 0);
        assert_eq!(img.at(cx - 10.0 * a.cos(), cy + 10.0 * a.sin())[0], 0);
        // fill none: the inside of the arc is empty
        assert_eq!(img.at(24.0, 24.0)[0], 0);
    }

    #[test]
    fn transforms_opacity_and_current_colour() {
        let h = Harness::new();
        let cache = GeomCache::new();
        let doc = r##"<svg viewBox="0 0 40 20">
            <rect width="10" height="10" fill="currentColor" transform="translate(20 5)"/>
            <g opacity=".5"><rect x="0" y="0" width="10" height="10" fill="#fff"/><rect x="5" y="0" width="10" height="10" fill="#fff"/></g>
            <rect x="0" y="12" width="6" height="6" fill="#00f" fill-opacity=".5"/>
        </svg>"##;
        let s = Svg::parse(doc);
        let img = h.render(40, 20, 1.0, BLACK, |cv| s.draw(cv, Rect::new(0.0, 0.0, 40.0, 20.0), Color::rgb8(255, 0, 0), &cache));
        assert_eq!(img.at(25.0, 8.0), [255, 0, 0, 255], "translated and tinted with currentColor");
        assert_eq!(img.at(15.0, 8.0)[0], 0, "not at the untranslated place");
        // group opacity: the two overlapping rects composite FIRST, so the overlap is not brighter than the rest
        let (a, b) = (img.at(2.0, 5.0)[0] as i32, img.at(7.0, 5.0)[0] as i32);
        assert!(near(a as f32, 128.0, 3.0) && (a - b).abs() <= 1, "{a} {b}");
        let blue = img.at(3.0, 15.0);
        assert!(near(blue[2] as f32, 128.0, 3.0) && blue[0] == 0, "{blue:?}");
    }

    #[test]
    fn even_odd_leaves_the_hole() {
        let h = Harness::new();
        let cache = GeomCache::new();
        let d = r#"d="M0 0h20v20h-20zM5 5h10v10h-10z""#;
        let nz = Svg::parse(&format!(r##"<svg viewBox="0 0 20 20"><path {d} fill="#fff"/></svg>"##));
        let eo = Svg::parse(&format!(r##"<svg viewBox="0 0 20 20"><path {d} fill="#fff" fill-rule="evenodd"/></svg>"##));
        let r = Rect::new(0.0, 0.0, 20.0, 20.0);
        let a = h.render(20, 20, 1.0, BLACK, |cv| nz.draw(cv, r, Color::rgb8(255, 255, 255), &cache));
        let b = h.render(20, 20, 1.0, BLACK, |cv| eo.draw(cv, r, Color::rgb8(255, 255, 255), &cache));
        assert_eq!(a.at(10.0, 10.0)[0], 255, "same winding fills the hole");
        assert_eq!(b.at(10.0, 10.0)[0], 0, "even-odd keeps it empty");
        assert_eq!(b.at(2.0, 2.0)[0], 255);
    }

    #[test]
    fn dots_with_round_caps_and_dashes_draw() {
        let h = Harness::new();
        let cache = GeomCache::new();
        // the "grip" icon: 0.01 long lines with round caps are dots
        let grip = Svg::icon(r#"<path d="M9 5h.01M15 5h.01M9 12h.01M15 12h.01M9 19h.01M15 19h.01" stroke-width="3"/>"#, 1.7);
        let img = h.render(24, 24, 2.0, BLACK, |cv| grip.draw(cv, Rect::new(0.0, 0.0, 24.0, 24.0), Color::rgb8(255, 255, 255), &cache));
        assert!(img.at(9.0, 5.0)[0] > 200 && img.at(15.0, 19.0)[0] > 200);
        assert_eq!(img.at(12.0, 12.0)[0], 0);
        // dashes: 4 on, 4 off along a 40 px line
        let dash = Svg::parse(r##"<svg viewBox="0 0 40 6"><path d="M0 3H40" stroke="#fff" stroke-width="2" stroke-dasharray="4 4"/></svg>"##);
        let img = h.render(40, 6, 1.0, BLACK, |cv| dash.draw(cv, Rect::new(0.0, 0.0, 40.0, 6.0), Color::rgb8(255, 255, 255), &cache));
        assert!(img.at(2.0, 3.0)[0] > 200 && img.at(6.0, 3.0)[0] < 60 && img.at(10.0, 3.0)[0] > 200);
    }

    #[test]
    fn viewbox_is_fitted_and_centred() {
        let h = Harness::new();
        let cache = GeomCache::new();
        // a 2:1 art box into a square: letterboxed, centred vertically
        let s = Svg::parse(r##"<svg viewBox="0 0 20 10"><rect width="20" height="10" fill="#fff"/></svg>"##);
        let dest = Rect::new(4.0, 4.0, 40.0, 40.0);
        assert_eq!(s.fit(dest).apply((0.0, 0.0)), (4.0, 14.0));
        let img = h.render(48, 48, 1.0, BLACK, |cv| s.draw(cv, dest, Color::rgb8(255, 255, 255), &cache));
        assert_eq!(img.bounds(|p| p[0] > 8), Some((4, 14, 44, 34)));
        // empty / degenerate destinations draw nothing and do not panic
        let img = h.render(8, 8, 1.0, BLACK, |cv| s.draw(cv, Rect::new(0.0, 0.0, 0.0, 5.0), Color::rgb8(255, 255, 255), &cache));
        assert!(img.bounds(|p| p[0] > 0).is_none());
    }

    #[test]
    fn every_icon_renders_something_visible() {
        let h = Harness::new();
        let cache = GeomCache::new();
        for (name, markup) in icons() {
            let s = Svg::icon(&markup, 1.7);
            let img = h.render(24, 24, 1.0, BLACK, |cv| s.draw(cv, Rect::new(0.0, 0.0, 24.0, 24.0), Color::rgb8(255, 255, 255), &cache));
            assert!(img.count(None, |p| p[0] > 40) > 4, "icon {name} drew nothing");
        }
    }
}
