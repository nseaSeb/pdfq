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

    /// Loads every font into memory now instead of on first use. Workers call
    /// this at startup so font data is part of their memory baseline and no job
    /// pays for loading a font inside its own memory budget.
    /// Returns the number of fonts that loaded.
    pub fn preload(&self) -> usize {
        let book = self.store.book();
        (0..)
            .take_while(|&i| book.info(i).is_some())
            .filter(|&i| self.store.font(i).is_some())
            .count()
    }

    /// Number of font families.
    pub fn family_count(&self) -> usize {
        self.store.book().families().count()
    }

    pub(crate) fn store(&self) -> &FontStore {
        &self.store
    }
}
