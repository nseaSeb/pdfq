use std::path::Path;

use typst_kit::fonts::{FontStore, embedded, scan};

/// Fonts available to every compilation: Typst's embedded fonts plus the
/// fonts found in the given directories. Loaded once per worker.
pub struct Fonts {
    store: FontStore,
}

impl Fonts {
    /// Typst's embedded fonts only.
    pub fn embedded() -> Self {
        let mut store = FontStore::new();
        store.extend(embedded());
        Self { store }
    }

    /// Embedded fonts plus every font found (recursively) in `dirs`.
    /// Missing directories are skipped.
    pub fn load<P: AsRef<Path>>(dirs: &[P]) -> Self {
        let mut fonts = Self::embedded();
        for dir in dirs {
            let dir = dir.as_ref();
            if dir.is_dir() {
                fonts.store.extend(scan(dir));
            }
        }
        fonts
    }

    /// Number of font families.
    pub fn family_count(&self) -> usize {
        self.store.book().families().count()
    }

    pub(crate) fn store(&self) -> &FontStore {
        &self.store
    }
}
