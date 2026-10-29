//! The accent colour taken from the desktop wallpaper (`accentMode` "wallpaper"): the picking algorithm lives in `glasskit::accent`
//! (re-exported here); this file adds the two bits that know about the app, decoding the wallpaper file through `sys` and the
//! on-disk cache next to config.json.
pub use glasskit::accent::*;

/// Accent of the wallpaper file at `path` (`None` when it cannot be decoded).
pub fn from_file(path: &str) -> Option<[u8; 3]> {
    pick_or_neutral(&crate::sys::decode_image_file(path, 64, 64)?.bgra)
}

fn cache_path() -> Option<std::path::PathBuf> {
    Some(crate::layout::config_path()?.with_file_name("accent.txt"))
}

/// The accent the live capture produced last time ("r g b"): used at start-up so the first frames already have the live colour
/// instead of the colour of the wallpaper file.
pub fn load_cache() -> Option<[u8; 3]> {
    let s = std::fs::read_to_string(cache_path()?).ok()?;
    let v: Vec<u8> = s.split_whitespace().filter_map(|p| p.parse().ok()).collect();
    (v.len() == 3).then(|| [v[0], v[1], v[2]])
}

/// Writes only when the stored colour differs (the live refresh calls this every few seconds with the same value).
pub fn save_cache(rgb: [u8; 3]) {
    if load_cache() == Some(rgb) {
        return;
    }
    if let Some(p) = cache_path() {
        let _ = std::fs::write(p, format!("{} {} {}", rgb[0], rgb[1], rgb[2]));
    }
}
