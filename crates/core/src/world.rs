use std::collections::HashMap;
use std::sync::Mutex;

use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Dict, Duration, IntoValue};
use typst::syntax::package::PackageSpec;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

use crate::fonts::Fonts;
use crate::packages::Packages;
use crate::project::{Project, ProjectError};

/// Per-compilation settings of a [`PdfqWorld`].
#[derive(Debug, Clone)]
pub struct WorldOptions {
    /// Path of the main file inside the project.
    pub entrypoint: String,
    /// Exposed to the document as `sys.inputs` (strings only, like the Typst CLI).
    pub inputs: Vec<(String, String)>,
    /// Fixed date for `datetime.today()`. `None` uses the current UTC date.
    pub date: Option<Datetime>,
    /// Maximum size of any single file read by the document.
    pub max_file_size: u64,
}

impl Default for WorldOptions {
    fn default() -> Self {
        Self {
            entrypoint: "main.typ".into(),
            inputs: Vec::new(),
            date: None,
            max_file_size: 10 * 1024 * 1024,
        }
    }
}

/// The Typst world of one compilation. It can only read the project files and
/// allowed, locally installed packages: no network, nothing else on disk.
pub struct PdfqWorld<'a> {
    library: LazyHash<Library>,
    fonts: &'a Fonts,
    packages: &'a Packages,
    project: Project,
    main: FileId,
    date: Option<Datetime>,
    max_file_size: u64,
    sources: Mutex<HashMap<FileId, Source>>,
    denied_package: Mutex<Option<PackageSpec>>,
}

impl<'a> PdfqWorld<'a> {
    pub fn new(
        project: Project,
        fonts: &'a Fonts,
        packages: &'a Packages,
        options: WorldOptions,
    ) -> Result<Self, ProjectError> {
        let entry = VirtualPath::new(&options.entrypoint)
            .map_err(|_| ProjectError::InvalidPath(options.entrypoint.clone()))?;
        if !project.contains(entry.get_without_slash()) {
            return Err(ProjectError::InvalidPath(options.entrypoint));
        }
        let mut inputs = Dict::new();
        for (key, value) in options.inputs {
            inputs.insert(key.into(), value.into_value());
        }
        Ok(Self {
            library: LazyHash::new(Library::builder().with_inputs(inputs).build()),
            fonts,
            packages,
            project,
            main: RootedPath::new(VirtualRoot::Project, entry).intern(),
            date: options.date,
            max_file_size: options.max_file_size,
            sources: Mutex::new(HashMap::new()),
            denied_package: Mutex::new(None),
        })
    }

    /// The first package import refused by the allowlist, if any.
    pub fn denied_package(&self) -> Option<String> {
        self.denied_package
            .lock()
            .unwrap()
            .as_ref()
            .map(ToString::to_string)
    }

    pub(crate) fn date(&self) -> Option<Datetime> {
        self.date
    }

    fn read(&self, id: FileId) -> FileResult<Bytes> {
        match id.root() {
            VirtualRoot::Project => self.project.read(id.vpath(), self.max_file_size),
            VirtualRoot::Package(spec) => {
                if !self.packages.is_allowed(spec) {
                    self.denied_package
                        .lock()
                        .unwrap()
                        .get_or_insert_with(|| spec.clone());
                    return Err(FileError::AccessDenied);
                }
                self.packages.read(spec, id.vpath(), self.max_file_size)
            }
        }
    }
}

impl World for PdfqWorld<'_> {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.store().book()
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        if let Some(source) = self.sources.lock().unwrap().get(&id) {
            return Ok(source.clone());
        }
        let bytes = self.read(id)?;
        let text = std::str::from_utf8(&bytes).map_err(|_| FileError::InvalidUtf8)?;
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let source = Source::new(id, text.to_string());
        self.sources.lock().unwrap().insert(id, source.clone());
        Ok(source)
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        self.read(id)
    }

    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.store().font(index)
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        if let Some(date) = self.date {
            return Some(date);
        }
        let now = time::OffsetDateTime::now_utc() + offset.map_or(time::Duration::ZERO, Into::into);
        Some(Datetime::Date(now.date()))
    }
}
