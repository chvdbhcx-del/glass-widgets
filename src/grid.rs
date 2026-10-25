//! Pure grid logic of the widget desk (no Windows, no Direct2D): cell occupancy, the nearest-free-cell spiral search, overlap
//! resolution for loads / resizes, snap maths, the bounding box and the gallery search filter. Unit-tested at the bottom.
use crate::widget::Size;

/// logical px between two neighbouring cells (170 cell + 16 gap) and the grid origin (20 px margin + 30 px top bar)
pub const PITCH: f32 = 186.0;
pub const ORIGIN: (f32, f32) = (20.0, 50.0);

/// A rectangle of grid cells (top-left cell + extent in cells).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    pub col: i32,
    pub row: i32,
    pub cw: i32,
    pub ch: i32,
}

impl Place {
    pub fn new(size: Size, col: i32, row: i32) -> Place {
        let (cw, ch) = size.cells();
        Place { col, row, cw: cw as i32, ch: ch as i32 }
    }
    pub fn overlaps(&self, o: &Place) -> bool {
        self.col < o.col + o.cw && o.col < self.col + self.cw && self.row < o.row + o.ch && o.row < self.row + self.ch
    }
}

/// Number of cell columns / rows the desk offers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dims {
    pub cols: i32,
    pub rows: i32,
}

/// Grid size for a work area of `w` x `h` logical px (origin margin on the left, 12 px kept free at the bottom).
pub fn dims(w: f32, h: f32) -> Dims {
    Dims { cols: (((w - 2.0 * ORIGIN.0 + 16.0) / PITCH).floor() as i32).max(2), rows: (((h - ORIGIN.1 - 12.0 + 16.0) / PITCH).floor() as i32).max(1) }
}

/// Top-left of a cell in logical px.
pub fn cell_xy(col: i32, row: i32) -> (f32, f32) {
    (ORIGIN.0 + col as f32 * PITCH, ORIGIN.1 + row as f32 * PITCH)
}

/// The cell whose top-left is nearest to a card top-left at (x, y) logical px (may be outside the grid: callers clamp / search).
pub fn snap_cell(x: f32, y: f32) -> (i32, i32) {
    (((x - ORIGIN.0) / PITCH).round() as i32, ((y - ORIGIN.1) / PITCH).round() as i32)
}

pub fn in_bounds(d: &Dims, p: &Place) -> bool {
    p.col >= 0 && p.row >= 0 && p.col + p.cw <= d.cols && p.row + p.ch <= d.rows
}

pub fn fits(d: &Dims, occ: &[Place], p: &Place) -> bool {
    in_bounds(d, p) && !occ.iter().any(|o| o.overlaps(p))
}

/// Nearest free spot for a card of `size` wanted at (col, row): rings of growing Chebyshev radius around the (clamped) wish,
/// the best squared Euclidean distance wins, ties go to the smaller row, then the smaller column. None = the grid is full.
pub fn find_free(d: &Dims, occ: &[Place], size: Size, col: i32, row: i32) -> Option<Place> {
    let probe = Place::new(size, 0, 0);
    let (c0, r0) = (col.clamp(0, (d.cols - probe.cw).max(0)), row.clamp(0, (d.rows - probe.ch).max(0)));
    let max_ring = d.cols.max(d.rows) + 2;
    let mut best: Option<(i32, i32, i32)> = None; // (dist2, row, col)
    for ring in 0..=max_ring {
        if let Some((d2, _, _)) = best {
            if d2 < ring * ring {
                break; // every cell of this ring is at least `ring` away
            }
        }
        for r in (r0 - ring)..=(r0 + ring) {
            for c in (c0 - ring)..=(c0 + ring) {
                if (c - c0).abs().max((r - r0).abs()) != ring {
                    continue;
                }
                let p = Place { col: c, row: r, ..probe };
                if !fits(d, occ, &p) {
                    continue;
                }
                let key = ((c - c0).pow(2) + (r - r0).pow(2), r, c);
                if best.is_none_or(|b| key < b) {
                    best = Some(key);
                }
            }
        }
    }
    best.map(|(_, r, c)| Place { col: c, row: r, ..probe })
}

/// Where a card of `size` whose top-left is at (x, y) physical px (UI scale `s`) would land: the free cell nearest to its snapped cell.
pub fn target_for(d: &Dims, occ: &[Place], size: Size, x: i32, y: i32, s: f32) -> Option<Place> {
    let (col, row) = snap_cell(x as f32 / s, y as f32 / s);
    find_free(d, occ, size, col, row)
}

/// First free cell in row-major order (what a click on a gallery preview does).
pub fn first_free(d: &Dims, occ: &[Place], size: Size) -> Option<Place> {
    find_free(d, occ, size, 0, 0)
}

/// Resize in place: keep the cell when the new size fits there, else the nearest free spot (None = no room).
pub fn resize(d: &Dims, occ_without_me: &[Place], size: Size, col: i32, row: i32) -> Option<Place> {
    let p = Place::new(size, col, row);
    if fits(d, occ_without_me, &p) {
        Some(p)
    } else {
        find_free(d, occ_without_me, size, col, row)
    }
}

/// Load-time resolution: items are placed in order, each where it wishes when that is free, else at the nearest free spot.
/// Result[i] = None means card i has no room at all (it stays in the file but is not drawn).
pub fn resolve_all(d: &Dims, items: &[(Size, i32, i32)]) -> Vec<Option<Place>> {
    let mut occ: Vec<Place> = Vec::new();
    items
        .iter()
        .map(|&(size, col, row)| {
            let p = resize(d, &occ, size, col, row)?;
            occ.push(p);
            Some(p)
        })
        .collect()
}

/// Bounding box (l, t, r, b) of rectangles given as (l, t, r, b).
pub fn bbox(rects: &[(i32, i32, i32, i32)]) -> Option<(i32, i32, i32, i32)> {
    rects.iter().copied().reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))
}

/// Gallery search: every whitespace separated token of `q` must occur (case-insensitive, any language) in one of the strings.
pub fn matches(q: &str, hay: &[&str]) -> bool {
    let joined = hay.join(" ").to_lowercase();
    q.to_lowercase().split_whitespace().all(|t| joined.contains(t))
}

#[cfg(test)]
mod tests {
    use super::*;

    const D: Dims = Dims { cols: 6, rows: 4 };

    #[test]
    fn dims_of_common_monitors() {
        assert_eq!(dims(1920.0, 1032.0), Dims { cols: 10, rows: 5 });
        assert_eq!(dims(100.0, 100.0), Dims { cols: 2, rows: 1 });
    }

    #[test]
    fn snap_rounds_to_the_nearest_cell() {
        assert_eq!(snap_cell(20.0, 50.0), (0, 0));
        assert_eq!(snap_cell(20.0 + 92.0, 50.0), (0, 0));
        assert_eq!(snap_cell(20.0 + 94.0, 50.0 + 3.0 * PITCH + 10.0), (1, 3));
        assert_eq!(snap_cell(-400.0, -400.0), (-2, -2));
        assert_eq!(cell_xy(2, 1), (20.0 + 372.0, 50.0 + 186.0));
    }

    #[test]
    fn spiral_finds_the_nearest_free_cell() {
        let occ = [Place::new(Size::M, 0, 0), Place::new(Size::S, 2, 0)];
        // wish (0,0): everything near is taken; nearest free S is (0,1) / (1,1) -> (0,1) wins the tie by column
        assert_eq!(find_free(&D, &occ, Size::S, 0, 0), Some(Place::new(Size::S, 0, 1)));
        // a free wish is returned as it is
        assert_eq!(find_free(&D, &occ, Size::S, 4, 2), Some(Place::new(Size::S, 4, 2)));
        // L (2x2) wished on top of the occupied corner goes to the nearest 2x2 hole
        let l = find_free(&D, &occ, Size::L, 0, 0).unwrap();
        assert!(fits(&D, &occ, &l));
        assert_eq!((l.col, l.row), (0, 1));
        // out of grid wishes are clamped first
        assert_eq!(find_free(&D, &[], Size::M, 99, 99), Some(Place::new(Size::M, 4, 3)));
        assert_eq!(find_free(&D, &[], Size::S, -5, -5), Some(Place::new(Size::S, 0, 0)));
    }

    #[test]
    fn spiral_prefers_euclid_over_ring_order() {
        // wish (2,1) is blocked; (2,0)/(1,1)/(3,1)/(2,2) are at distance 1, a diagonal at sqrt2 must lose to them
        let occ = [Place::new(Size::S, 2, 1)];
        let p = find_free(&D, &occ, Size::S, 2, 1).unwrap();
        assert_eq!((p.col, p.row), (2, 0)); // distance 1, smallest row
    }

    #[test]
    fn drop_target_follows_the_pointer_and_avoids_cards() {
        let occ = [Place::new(Size::S, 1, 1)];
        // a card dragged to px (20 + 186, 50 + 186) at scale 1 wants cell (1,1): taken, the nearest free one is (1,0)
        let p = target_for(&D, &occ, Size::S, 206, 236, 1.0).unwrap();
        assert_eq!((p.col, p.row), (1, 0));
        // scale 1.5: the same cell is at 1.5x the pixels
        let p = target_for(&D, &[], Size::S, 309, 354, 1.5).unwrap();
        assert_eq!((p.col, p.row), (1, 1));
        // dragged far off the bottom right: the last cell that fits
        assert_eq!(target_for(&D, &[], Size::L, 9000, 9000, 1.0), Some(Place::new(Size::L, 4, 2)));
    }

    #[test]
    fn full_grid_has_no_free_cell() {
        let occ: Vec<Place> = (0..4).flat_map(|r| (0..6).map(move |c| Place::new(Size::S, c, r))).collect();
        assert_eq!(find_free(&D, &occ, Size::S, 3, 2), None);
        assert_eq!(first_free(&D, &occ, Size::S), None);
    }

    #[test]
    fn resize_keeps_cell_or_reflows() {
        let me = Place::new(Size::S, 0, 0);
        let other = Place::new(Size::S, 1, 0);
        let occ = [other]; // everything except the card itself
        assert_eq!(resize(&D, &[], Size::S, me.col, me.row), Some(me));
        // M at (0,0) would hit the neighbour at (1,0): nearest free M is (0,1)
        assert_eq!(resize(&D, &occ, Size::M, 0, 0), Some(Place::new(Size::M, 0, 1)));
        // L next to the right edge shifts left
        assert_eq!(resize(&D, &[], Size::L, 5, 0), Some(Place::new(Size::L, 4, 0)));
        // no room at all
        let full: Vec<Place> = (0..4).flat_map(|r| (0..6).map(move |c| Place::new(Size::S, c, r))).collect();
        assert_eq!(resize(&D, &full, Size::M, 1, 1), None);
    }

    #[test]
    fn load_resolves_overlaps_in_order_and_drops_the_homeless() {
        let r = resolve_all(&D, &[(Size::M, 0, 0), (Size::S, 1, 0), (Size::S, 9, 9)]);
        assert_eq!(r[0], Some(Place::new(Size::M, 0, 0)));
        let second = r[1].unwrap();
        assert!(!second.overlaps(&r[0].unwrap()));
        assert_eq!((second.col, second.row), (2, 0)); // (2,0) and (1,1) are both at distance 1: the smaller row wins
        let tiny = Dims { cols: 2, rows: 1 };
        let r = resolve_all(&tiny, &[(Size::M, 0, 0), (Size::S, 0, 0)]);
        assert_eq!(r[1], None);
    }

    #[test]
    fn bbox_unions() {
        assert_eq!(bbox(&[]), None);
        assert_eq!(bbox(&[(10, 20, 30, 40), (5, 25, 35, 38)]), Some((5, 20, 35, 40)));
    }

    #[test]
    fn search_is_case_insensitive_and_multi_token() {
        let hay = ["Часы", "Clock", "Аналоговые часы", "Analog clock"];
        assert!(matches("", &hay));
        assert!(matches("ЧАС", &hay));
        assert!(matches("analog CLOCK", &hay));
        assert!(matches("часы clock", &hay));
        assert!(!matches("погода", &hay));
        assert!(!matches("clock weather", &hay));
    }
}
