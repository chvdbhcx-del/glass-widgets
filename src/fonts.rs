//! Bundled typefaces (SIL OFL 1.1: `assets/fonts/Medeo-OFL.txt`, `OFL.txt` for Nunito), five static weights each, subset to Latin + Cyrillic, embedded in the exe.
//! DirectWrite reads them from memory (Windows 10 1703+); when that is not available the callers keep the system Segoe UI.
//! Default Medeo Sans; `MSW_FONT=nunito` picks Nunito, `MSW_FONT=system` skips the bundled fonts.
use std::sync::OnceLock;
use windows::core::{Interface, IUnknown};
use windows::Win32::Graphics::DirectWrite::*;

/// Family used for all text.
pub fn family() -> &'static str {
    if std::env::var("MSW_FONT").is_ok_and(|v| v == "nunito") {
        "Nunito"
    } else {
        "Medeo Sans"
    }
}

static MEDEO: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/MedeoSans-Light.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-Regular.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-Medium.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-SemiBold.ttf"),
    include_bytes!("../assets/fonts/MedeoSans-Bold.ttf"),
];

static NUNITO: [&[u8]; 5] = [
    include_bytes!("../assets/fonts/Nunito-Light.ttf"),
    include_bytes!("../assets/fonts/Nunito-Regular.ttf"),
    include_bytes!("../assets/fonts/Nunito-Medium.ttf"),
    include_bytes!("../assets/fonts/Nunito-SemiBold.ttf"),
    include_bytes!("../assets/fonts/Nunito-Bold.ttf"),
];

fn build(dw: &IDWriteFactory) -> windows::core::Result<IDWriteFontCollection> {
    unsafe {
        let f5: IDWriteFactory5 = dw.cast()?;
        let loader = f5.CreateInMemoryFontFileLoader()?;
        f5.RegisterFontFileLoader(&loader)?;
        let set = f5.CreateFontSetBuilder()?;
        for data in MEDEO.iter().chain(NUNITO.iter()).copied() {
            let file = loader.CreateInMemoryFontFileReference(dw, data.as_ptr().cast(), data.len() as u32, None::<&IUnknown>)?;
            set.AddFontFile(&file)?;
        }
        // the factory keeps the loader alive through the registration; the data is 'static
        f5.CreateFontCollectionFromFontSet(&set.CreateFontSet()?)?.cast()
    }
}

/// The collection that holds Nunito, or `None` (use the system collection). Built once per process: DirectWrite's shared factory is one object.
pub fn collection(dw: &IDWriteFactory) -> Option<IDWriteFontCollection> {
    static C: OnceLock<Option<IDWriteFontCollection>> = OnceLock::new();
    C.get_or_init(|| {
        if std::env::var("MSW_FONT").is_ok_and(|v| v == "system") {
            return None;
        }
        build(dw).map_err(|e| eprintln!("mswidgets: bundled font unavailable ({e})")).ok()
    })
    .clone()
}

/// Makes Nunito Regular usable by GDI (the prompt's EDIT control) for this process only.
pub fn install_gdi() {
    use windows::Win32::Graphics::Gdi::AddFontMemResourceEx;
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let d = if family() == "Nunito" { NUNITO[1] } else { MEDEO[1] };
        let mut n = 0u32;
        unsafe {
            AddFontMemResourceEx(d.as_ptr().cast(), d.len() as u32, None, &mut n);
        }
    });
}
