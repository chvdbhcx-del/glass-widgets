//! The accent colour taken from the desktop wallpaper (`accentMode` "wallpaper"): the wallpaper path comes from
//! `SystemParametersInfo(SPI_GETDESKWALLPAPER)`, the caller decodes it at <= 64 px (WIC) into premultiplied BGRA, `pick` finds the dominant saturated hue
//! (a 36 bin hue histogram weighted by saturation x brightness) and clamps saturation / lightness so the colour stays legible on dark glass
//! and on light glass (where the ink pass darkens it by 20 %). Grey / near-black / near-white wallpapers give `None` from `pick`; the callers turn that into `NEUTRAL` with `pick_or_neutral` (the accent follows a monochrome wallpaper too).
use std::time::SystemTime;
use windows::Win32::UI::WindowsAndMessaging::*;

const BINS: usize = 36;
/// pixels less saturated / darker than this carry no hue
const MIN_S: f32 = 0.15;
const MIN_V: f32 = 0.15;
/// share of "colourful weight" (mean of s x v over ALL pixels) below which the wallpaper counts as grey
const MIN_COLORFUL: f32 = 0.025;
/// output HSL range: the default accent (100, 190, 255) has lightness .70; a hue taken from the wallpaper sits on glass of the same hue, so it leans brighter
const S_RANGE: (f32, f32) = (0.55, 1.0);
const L_RANGE: (f32, f32) = (0.68, 0.80);
/// perceived brightness (gamma encoded Rec.709 luma) above which a hue (yellow, lime) is darkened: x0.8 on light glass must stay readable
const MAX_LUMA: f32 = 0.78;
/// how far the luma cap may darken a bright hue
const MIN_L: f32 = 0.42;

/// Accent from premultiplied BGRA pixels (`Decoded::bgra`); `None` for a wallpaper without a dominant hue.
pub fn pick(bgra: &[u8]) -> Option<[u8; 3]> {
    let mut hist = [0f32; BINS];
    let mut sum = [[0f32; 3]; BINS]; // weighted rgb per bin
    let mut n = 0usize;
    for p in bgra.chunks_exact(4) {
        n += 1;
        if p[3] < 128 {
            continue;
        }
        let (r, g, b) = (p[2] as f32 / 255.0, p[1] as f32 / 255.0, p[0] as f32 / 255.0);
        let (h, s, v) = rgb_to_hsv(r, g, b);
        if s < MIN_S || v < MIN_V {
            continue;
        }
        let (w, bin) = (s * v, ((h / 360.0 * BINS as f32) as usize).min(BINS - 1));
        hist[bin] += w;
        for (acc, c) in sum[bin].iter_mut().zip([r, g, b]) {
            *acc += c * w;
        }
    }
    if n == 0 || hist.iter().sum::<f32>() / (n as f32) < MIN_COLORFUL {
        return None;
    }
    // the peak: a bin with its two neighbours (circular), so a hue split over a bin border still wins
    let near = |i: usize| hist[(i + BINS - 1) % BINS] + hist[i] + hist[(i + 1) % BINS];
    let peak = (0..BINS).max_by(|&a, &b| near(a).total_cmp(&near(b)))?;
    let (mut rgb, mut w) = ([0f32; 3], 0f32);
    for i in [(peak + BINS - 1) % BINS, peak, (peak + 1) % BINS] {
        w += hist[i];
        for k in 0..3 {
            rgb[k] += sum[i][k];
        }
    }
    if w <= 0.0 {
        return None;
    }
    let (h, s, mut l) = rgb_to_hsl(rgb[0] / w, rgb[1] / w, rgb[2] / w);
    let s = s.clamp(S_RANGE.0, S_RANGE.1);
    l = l.clamp(L_RANGE.0, L_RANGE.1);
    let mut out = hsl_to_rgb(h, s, l);
    while luma(out) > MAX_LUMA && l > MIN_L {
        l -= 0.02;
        out = hsl_to_rgb(h, s, l);
    }
    Some([(out[0] * 255.0).round() as u8, (out[1] * 255.0).round() as u8, (out[2] * 255.0).round() as u8])
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// h in degrees 0..360, s and v in 0..1
fn rgb_to_hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let d = mx - mn;
    let h = if d <= 0.0 {
        0.0
    } else if mx == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if mx <= 0.0 { 0.0 } else { d / mx }, mx)
}

fn rgb_to_hsl(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (h, _, mx) = rgb_to_hsv(r, g, b);
    let mn = r.min(g).min(b);
    let l = (mx + mn) / 2.0;
    let d = mx - mn;
    (h, if d <= 0.0 { 0.0 } else { d / (1.0 - (2.0 * l - 1.0).abs()) }, l)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [f32; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0).rem_euclid(2.0) - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (h.rem_euclid(360.0) / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    [r + m, g + m, b + m]
}

// ---------------------------------------------------------------- the wallpaper

/// The current wallpaper file and its modification time (a slideshow rewrites the same transcoded file, so the time matters too).
pub fn wallpaper_key() -> Option<(String, Option<SystemTime>)> {
    let mut buf = [0u16; 520];
    unsafe { SystemParametersInfoW(SPI_GETDESKWALLPAPER, buf.len() as u32, Some(buf.as_mut_ptr() as *mut _), SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0)).ok()? };
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let path = String::from_utf16_lossy(&buf[..n]);
    (!path.is_empty()).then(|| {
        let m = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        (path, m)
    })
}

/// The accent of a wallpaper without a dominant hue (grey, black, white): a neutral light tone, so the accent still follows the wallpaper
/// (white-ish titles on a monochrome desktop) instead of falling back to the blue of `accentMode` "blue".
pub const NEUTRAL: [u8; 3] = [220, 223, 230];

/// `pick`, but a wallpaper that has pixels and no hue gives [`NEUTRAL`]; `None` only for no pixels at all (nothing captured / undecodable).
pub fn pick_or_neutral(bgra: &[u8]) -> Option<[u8; 3]> {
    if bgra.len() < 4 {
        return None;
    }
    pick(bgra).or(Some(NEUTRAL))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// n pixels of one colour as BGRA
    fn solid(rgb: [u8; 3], n: usize) -> Vec<u8> {
        (0..n).flat_map(|_| [rgb[2], rgb[1], rgb[0], 255]).collect()
    }
    fn hsl(c: [u8; 3]) -> (f32, f32, f32) {
        rgb_to_hsl(c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0)
    }

    #[test]
    fn grey_dark_and_empty_give_nothing() {
        assert_eq!(pick(&[]), None);
        assert_eq!(pick(&solid([128, 128, 128], 400)), None);
        assert_eq!(pick(&solid([10, 12, 14], 400)), None); // near black
        assert_eq!(pick(&solid([250, 250, 252], 400)), None); // near white
        assert_eq!(pick(&solid([120, 128, 124], 400)), None); // a hint of colour is not an accent
    }

    #[test]
    fn dominant_hue_wins_over_area_of_grey() {
        // 70 % grey, 30 % red: the red wins
        let mut px = solid([120, 120, 120], 70);
        px.extend(solid([220, 30, 40], 30));
        let c = pick(&px).unwrap();
        assert!(c[0] > c[1] && c[0] > c[2], "{c:?}");
        let h = hsl(c).0;
        assert!(h < 12.0 || h > 348.0, "{h}");
        // pure blue
        let c = pick(&solid([20, 60, 230], 100)).unwrap();
        assert!(c[2] > c[0] && c[2] > c[1], "{c:?}");
    }

    #[test]
    fn saturation_weights_the_vote() {
        // many washed out greens (s .2) lose against fewer vivid oranges
        let mut px = solid([140, 180, 140], 60);
        px.extend(solid([255, 120, 20], 40));
        let c = pick(&px).unwrap();
        assert!(c[0] > c[2] && c[0] >= c[1], "{c:?}");
        let h = hsl(c).0;
        assert!((10.0..45.0).contains(&h), "{h}");
    }

    #[test]
    fn transparent_pixels_do_not_vote() {
        let mut px: Vec<u8> = (0..100).flat_map(|_| [0, 0, 255, 0]).collect(); // red, alpha 0
        px.extend(solid([30, 200, 60], 20));
        let c = pick(&px).unwrap();
        assert!(c[1] > c[0] && c[1] > c[2], "{c:?}");
    }

    #[test]
    fn output_is_clamped_to_a_legible_range() {
        // very dark saturated blue, very light pastel, a vivid yellow, a vivid green: all land in the legible band
        for rgb in [[10u8, 20, 90], [250, 180, 200], [255, 235, 0], [0, 255, 0], [200, 0, 255], [0, 120, 200]] {
            let c = pick(&solid(rgb, 200)).unwrap();
            let (_, s, l) = hsl(c);
            assert!(s >= S_RANGE.0 - 0.02 && l >= MIN_L - 0.02 && l <= L_RANGE.1 + 0.02, "{rgb:?} -> {c:?} s {s} l {l}");
            assert!(luma([c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0]) <= MAX_LUMA + 0.02, "{rgb:?} -> {c:?}");
        }
        // the default accent itself is inside the band: a pure sky blue wallpaper comes out as about the default
        let (h, _, _) = hsl(pick(&solid([100, 190, 255], 100)).unwrap());
        assert!((h - 205.0).abs() < 6.0, "{h}");
    }

    #[test]
    fn hue_across_the_bin_border_and_conversions() {
        // reds just below and above 0 degrees vote for the same peak
        let mut px = solid([255, 20, 0], 30);
        px.extend(solid([255, 0, 20], 30));
        px.extend(solid([20, 255, 0], 40)); // one green group of 40 loses against the 60 reds
        let c = pick(&px).unwrap();
        assert!(c[0] > c[1], "{c:?}");
        // hsl <-> rgb round trip
        for rgb in [[0.2f32, 0.5, 0.9], [0.9, 0.3, 0.1], [0.4, 0.8, 0.2]] {
            let (h, s, l) = rgb_to_hsl(rgb[0], rgb[1], rgb[2]);
            let back = hsl_to_rgb(h, s, l);
            assert!((0..3).all(|i| (back[i] - rgb[i]).abs() < 1e-4), "{rgb:?} {back:?}");
        }
    }

    #[test]
    fn a_wallpaper_without_hue_gets_the_neutral_accent() {
        assert_eq!(pick(&solid([120, 120, 122], 100)), None);
        assert_eq!(pick_or_neutral(&solid([120, 120, 122], 100)), Some(NEUTRAL));
        assert_eq!(pick_or_neutral(&solid([0, 0, 0], 100)), Some(NEUTRAL));
        assert_eq!(pick_or_neutral(&[]), None); // nothing captured: no opinion
        assert_ne!(pick_or_neutral(&solid([255, 90, 20], 100)), Some(NEUTRAL)); // a colourful one keeps its hue
    }
}
