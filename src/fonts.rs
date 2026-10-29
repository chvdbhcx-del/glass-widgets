//! Bundled typefaces (SIL OFL 1.1: `assets/fonts/Medeo-OFL.txt`, `OFL.txt` for Nunito), five static weights each, subset to Latin + Cyrillic, embedded in the exe.
//! DirectWrite reads them from memory (Windows 10 1703+); when that is not available the callers keep the system Segoe UI.
//! Default Medeo Sans; `MSW_FONT=nunito` picks Nunito, `MSW_FONT=system` skips the bundled fonts.
//! The machinery (`FontSet`: the collection, GDI registration) is `glasskit::fonts`; this file only says which files and which family.
use glasskit::fonts::FontSet;

/// Family used for all text.
pub fn family() -> &'static str {
    if std::env::var("MSW_FONT").is_ok_and(|v| v == "nunito") {
        "Nunito"
    } else {
        "Medeo Sans"
    }
}

/// Medeo Sans (Light, Regular, Medium, SemiBold, Bold), then Nunito (the same five).
static FILES: [&[u8]; 10] = [
    include_bytes!("../assets/fonts/MedeoSans-Light.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-Regular.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-Medium.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-Bold.ttf"),
    include_bytes!("../assets/fonts/Nunito-Light.ttf"),
    include_bytes!("../assets/fonts/Nunito-Regular.ttf"),
    include_bytes!("../assets/fonts/Nunito-Medium.ttf"),
    include_bytes!("../assets/fonts/Nunito-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Nunito-Bold.ttf"),
];

/// Regular weight of the chosen family, for GDI (the prompt's EDIT control).
fn gdi_file() -> &'static [u8] {
    FILES[if family() == "Nunito" { 6 } else { 1 }]
}

fn system_only() -> bool {
    std::env::var("MSW_FONT").is_ok_and(|v| v == "system")
}

/// Nunito is lighter than Medeo / Segoe: its small text gets one more weight step.
fn nunito() -> bool {
    family() == "Nunito"
}

/// The app's font set, handed to `Res::new`.
pub static SET: FontSet = FontSet::new(&FILES, family, gdi_file).system_only_when(system_only).bolder_small_when(nunito);

/// The collection that holds the bundled fonts, or `None` (use the system collection).
pub fn collection(dw: &windows::Win32::Graphics::DirectWrite::IDWriteFactory) -> Option<windows::Win32::Graphics::DirectWrite::IDWriteFontCollection> {
    SET.collection(dw)
}

/// Makes the Regular weight usable by GDI (the prompt's EDIT control) for this process only.
pub fn install_gdi() {
    SET.install_gdi();
}