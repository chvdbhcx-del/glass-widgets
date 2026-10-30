//! Blue-noise threshold tile for the final 8-bit conversion (void and cluster, Ulichney 1993), built once at start-up and deterministic: the same
//! pixel always gets the same threshold, so a static card never shimmers and a cached layer stays valid.
//!
//! `Gfx::quantize` adds `value(rank)` (+-0.375 of one 8-bit level, uniformly distributed over the tile) to the float image before D2D rounds it
//! to 8 bit: an unbiased dither, exact values (0, 255, every flat colour that is a whole number of levels) pass through unchanged.
use std::sync::OnceLock;

/// Tile edge in px (the noise repeats seamlessly: the distances wrap around).
pub const N: usize = 64;
const SIGMA: f32 = 1.5;
const R: i32 = 5;

/// Rank (0..N*N-1) of every pixel of the tile, row-major: a blue-noise permutation.
pub fn ranks() -> &'static [u16] {
    static T: OnceLock<Vec<u16>> = OnceLock::new();
    T.get_or_init(build)
}

/// Dither offset of a rank in LEVELS of the 8-bit target, uniform in (-0.375, 0.375): the margin (storing a bright value in half-float costs
/// up to 1/8 level, downward) keeps `round(v + offset)` equal to `v` for a whole level value: no specks of alpha 1 over a fully
/// transparent area, no change of exact colours.
pub fn offset(rank: u16) -> f32 {
    ((rank as f32 + 0.5) / (N * N) as f32 - 0.5) * 0.75
}

fn build() -> Vec<u16> {
    let n = N * N;
    // gaussian energy kernel on the torus
    let mut kern = Vec::new();
    for dy in -R..=R {
        for dx in -R..=R {
            kern.push((dx, dy, (-((dx * dx + dy * dy) as f32) / (2.0 * SIGMA * SIGMA)).exp()));
        }
    }
    let mut energy = vec![0f32; n];
    let splat = |energy: &mut Vec<f32>, p: usize, sign: f32| {
        let (px, py) = ((p % N) as i32, (p / N) as i32);
        for &(dx, dy, w) in &kern {
            let (x, y) = ((px + dx).rem_euclid(N as i32) as usize, (py + dy).rem_euclid(N as i32) as usize);
            energy[y * N + x] += sign * w;
        }
    };
    let mut bits = vec![false; n];
    // initial pattern: 10 % ones from a fixed LCG
    let mut s: u32 = 0x1234_5678;
    let mut placed = 0;
    while placed < n / 10 {
        s = s.wrapping_mul(1664525).wrapping_add(1013904223);
        let p = (s >> 8) as usize % n;
        if !bits[p] {
            bits[p] = true;
            splat(&mut energy, p, 1.0);
            placed += 1;
        }
    }
    let tightest = |bits: &[bool], energy: &[f32]| (0..n).filter(|&i| bits[i]).max_by(|&a, &b| energy[a].total_cmp(&energy[b])).unwrap_or(0);
    let largest_void = |bits: &[bool], energy: &[f32]| (0..n).filter(|&i| !bits[i]).min_by(|&a, &b| energy[a].total_cmp(&energy[b])).unwrap_or(0);
    // relax: move the tightest one into the largest void until it would go back where it was
    for _ in 0..n {
        let c = tightest(&bits, &energy);
        bits[c] = false;
        splat(&mut energy, c, -1.0);
        let v = largest_void(&bits, &energy);
        bits[v] = true;
        splat(&mut energy, v, 1.0);
        if v == c {
            break;
        }
    }
    let mut rank = vec![0u16; n];
    // phase 1: remove ones, the tightest first, ranks ones-1 .. 0
    let (mut b1, mut e1) = (bits.clone(), energy.clone());
    for r in (0..placed).rev() {
        let c = tightest(&b1, &e1);
        b1[c] = false;
        splat(&mut e1, c, -1.0);
        rank[c] = r as u16;
    }
    // phase 2: insert ones into the largest void, ranks ones .. n-1
    for r in placed..n {
        let v = largest_void(&bits, &energy);
        bits[v] = true;
        splat(&mut energy, v, 1.0);
        rank[v] = r as u16;
    }
    rank
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranks_are_a_permutation_and_offsets_stay_inside_half_a_level() {
        let r = ranks();
        assert_eq!(r.len(), N * N);
        let mut seen = vec![false; N * N];
        for &v in r {
            assert!(!seen[v as usize]);
            seen[v as usize] = true;
        }
        let (lo, hi) = (offset(0), offset((N * N - 1) as u16));
        assert!(lo > -0.375 && hi < 0.375 && lo < -0.37 && hi > 0.37, "{lo} {hi}");
        let mean: f32 = (0..N * N).map(|i| offset(i as u16)).sum::<f32>() / (N * N) as f32;
        assert!(mean.abs() < 1e-3, "unbiased: {mean}");
    }

    #[test]
    fn noise_is_blue_not_clumped() {
        // a blue-noise tile has little low-frequency energy: the 8x8 block means of the thresholds stay close to the global mean
        let r = ranks();
        let mean = (N * N) as f32 / 2.0;
        let mut worst = 0f32;
        for by in 0..N / 8 {
            for bx in 0..N / 8 {
                let mut s = 0f32;
                for y in 0..8 {
                    for x in 0..8 {
                        s += r[(by * 8 + y) * N + bx * 8 + x] as f32;
                    }
                }
                worst = worst.max((s / 64.0 - mean).abs() / mean);
            }
        }
        assert!(worst < 0.2, "8x8 block mean deviates by {worst}");
    }
}
