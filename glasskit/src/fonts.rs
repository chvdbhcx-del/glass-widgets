//! Bundled typefaces for DirectWrite. An app describes its fonts once as a `FontSet` (the font files embedded in the exe, which family the
//! text uses, an optional GDI file for EDIT controls); the set builds ONE in-memory font collection per process on first use (DirectWrite
//! reads the files from memory, Windows 10 1703+). When that is not available, or the set has no files, `collection` gives `None` and
//! callers keep the system fonts (Segoe UI).
use std::sync::OnceLock;
use windows::core::{Interface, IUnknown};
use windows::Win32::Graphics::DirectWrite::*;

fn never() -> bool {
    false
}

/// A set of font files and the rules for using them. Make it a `static` (it caches the collection); every `fn` is called each time it is
/// needed, so a family picked from an environment variable stays switchable.
pub struct FontSet {
    files: &'static [&'static [u8]],
    family: fn() -> &'static str,
    gdi_file: fn() -> &'static [u8],
    system_only: fn() -> bool,
    bolder_small: fn() -> bool,
    collection: OnceLock<Option<IDWriteFontCollection>>,
    gdi: OnceLock<()>,
}

impl FontSet {
    /// `files`: every font file to register (all families of the set); `family`: the family name text is set in; `gdi_file`: the regular
    /// weight file `install_gdi` makes visible to GDI controls.
    pub const fn new(files: &'static [&'static [u8]], family: fn() -> &'static str, gdi_file: fn() -> &'static [u8]) -> FontSet {
        FontSet { files, family, gdi_file, system_only: never, bolder_small: never, collection: OnceLock::new(), gdi: OnceLock::new() }
    }

    /// No bundled fonts: text uses the system fonts.
    pub const fn system() -> FontSet {
        fn none() -> &'static [u8] {
            &[]
        }
        fn name() -> &'static str {
            "Segoe UI"
        }
        FontSet::new(&[], name, none)
    }

    /// `f()` true = skip the bundled fonts and use the system ones (a developer switch).
    pub const fn system_only_when(mut self, f: fn() -> bool) -> FontSet {
        self.system_only = f;
        self
    }

    /// `f()` true = the family is lighter than Segoe: small text gets one more weight step to hold its contrast.
    pub const fn bolder_small_when(mut self, f: fn() -> bool) -> FontSet {
        self.bolder_small = f;
        self
    }

    /// Family used for all text.
    pub fn family(&self) -> &'static str {
        (self.family)()
    }

    /// Does small text need the extra weight step (see `bolder_small_when`)?
    pub fn bolder_small(&self) -> bool {
        (self.bolder_small)()
    }

    fn build(&self, dw: &IDWriteFactory) -> windows::core::Result<IDWriteFontCollection> {
        unsafe {
            let f5: IDWriteFactory5 = dw.cast()?;
            let loader = f5.CreateInMemoryFontFileLoader()?;
            f5.RegisterFontFileLoader(&loader)?;
            let set = f5.CreateFontSetBuilder()?;
            for data in self.files.iter().copied() {
                let file = loader.CreateInMemoryFontFileReference(dw, data.as_ptr().cast(), data.len() as u32, None::<&IUnknown>)?;
                set.AddFontFile(&file)?;
            }
            // the factory keeps the loader alive through the registration; the data is 'static
            f5.CreateFontCollectionFromFontSet(&set.CreateFontSet()?)?.cast()
        }
    }

    /// The collection that holds the set's fonts, or `None` (use the system collection). Built once per process: DirectWrite's shared factory is one object.
    pub fn collection(&self, dw: &IDWriteFactory) -> Option<IDWriteFontCollection> {
        self.collection
            .get_or_init(|| {
                if self.files.is_empty() || (self.system_only)() {
                    return None;
                }
                self.build(dw).map_err(|e| eprintln!("glasskit: bundled font unavailable ({e})")).ok()
            })
            .clone()
    }

    /// Makes the `gdi_file` font usable by GDI (an EDIT control) for this process only.
    pub fn install_gdi(&self) {
        use windows::Win32::Graphics::Gdi::AddFontMemResourceEx;
        self.gdi.get_or_init(|| {
            let d = (self.gdi_file)();
            if d.is_empty() {
                return;
            }
            let mut n = 0u32;
            unsafe {
                AddFontMemResourceEx(d.as_ptr().cast(), d.len() as u32, None, &mut n);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_without_files_is_the_system_fonts() {
        static SYS: FontSet = FontSet::system();
        assert_eq!(SYS.family(), "Segoe UI");
        assert!(!SYS.bolder_small());
        SYS.install_gdi(); // nothing to register, nothing happens
    }

    #[test]
    fn the_family_and_the_switches_are_read_on_every_call() {
        fn family() -> &'static str {
            "Test Sans"
        }
        fn gdi() -> &'static [u8] {
            &[]
        }
        fn yes() -> bool {
            true
        }
        static SET: FontSet = FontSet::new(&[b"not a font"], family, gdi).bolder_small_when(yes).system_only_when(yes);
        assert_eq!(SET.family(), "Test Sans");
        assert!(SET.bolder_small());
    }
}