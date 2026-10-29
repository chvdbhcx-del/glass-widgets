//! Plain geometry value types in logical pixels: a point and a rectangle with the slicing helpers (`inset`, `cut_*`, `row`, `col`) used to lay
//! out UI by hand. No GPU, no Win32: everything here is pure and unit-testable.
pub type Pt = (f32, f32);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}
impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub fn center(&self) -> Pt {
        (self.cx(), self.cy())
    }
    pub fn contains(&self, px: f32, py: f32) -> bool {
        px >= self.x && px < self.x + self.w && py >= self.y && py < self.y + self.h
    }
    pub fn inset(&self, d: f32) -> Rect {
        Rect::new(self.x + d, self.y + d, (self.w - 2.0 * d).max(0.0), (self.h - 2.0 * d).max(0.0))
    }
    pub fn inset_xy(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    pub fn offset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }
    /// same centre, new size
    pub fn centered(&self, w: f32, h: f32) -> Rect {
        Rect::new(self.cx() - w / 2.0, self.cy() - h / 2.0, w, h)
    }
    /// (top slice of height h, the rest)
    pub fn cut_top(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.y, self.w, h), Rect::new(self.x, self.y + h, self.w, self.h - h))
    }
    /// (the rest, bottom slice of height h)
    pub fn cut_bottom(&self, h: f32) -> (Rect, Rect) {
        let h = h.min(self.h);
        (Rect::new(self.x, self.y, self.w, self.h - h), Rect::new(self.x, self.bottom() - h, self.w, h))
    }
    /// (left slice of width w, the rest)
    pub fn cut_left(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.x, self.y, w, self.h), Rect::new(self.x + w, self.y, self.w - w, self.h))
    }
    /// (the rest, right slice of width w)
    pub fn cut_right(&self, w: f32) -> (Rect, Rect) {
        let w = w.min(self.w);
        (Rect::new(self.x, self.y, self.w - w, self.h), Rect::new(self.right() - w, self.y, w, self.h))
    }
    /// i-th of n equal rows (gap between rows)
    pub fn row(&self, i: u32, n: u32, gap: f32) -> Rect {
        let h = (self.h - gap * (n as f32 - 1.0)) / n as f32;
        Rect::new(self.x, self.y + i as f32 * (h + gap), self.w, h)
    }
    /// i-th of n equal columns (gap between columns)
    pub fn col(&self, i: u32, n: u32, gap: f32) -> Rect {
        let w = (self.w - gap * (n as f32 - 1.0)) / n as f32;
        Rect::new(self.x + i as f32 * (w + gap), self.y, w, self.h)
    }
}
