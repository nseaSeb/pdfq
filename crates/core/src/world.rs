use std::collections::HashMap;
use std::sync::Mutex;

use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Dict, Duration, IntoValue, Repr};
use typst::syntax::package::PackageSpec;
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

use pdfq_protocol::{Attachment, RESERVED_DIR};

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
    /// Files embedded in the PDF. They are attached by code appended to the
    /// entrypoint and served from the reserved [`RESERVED_DIR`].
    pub attachments: Vec<Attachment>,
}

impl Default for WorldOptions {
    fn default() -> Self {
        Self {
            entrypoint: "main.typ".into(),
            inputs: Vec::new(),
            date: None,
            max_file_size: 10 * 1024 * 1024,
            attachments: Vec::new(),
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
    /// Attachment data, served at `/.pdfq/attachments/<index>`.
    attachments: Vec<Bytes>,
    attachment_names: Vec<String>,
    /// With attachments, the compiled main file is a synthetic wrapper (see
    /// [`wrapper`]) and this is its code; `main` is then the wrapper's id.
    wrapper: Option<String>,
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
        let entry_path = entry.get_without_slash().to_string();
        let wrapper =
            (!options.attachments.is_empty()).then(|| wrapper(&entry_path, &options.attachments));
        let main = match wrapper {
            Some(_) => RootedPath::new(
                VirtualRoot::Project,
                VirtualPath::new(WRAPPER).expect("valid"),
            )
            .intern(),
            None => RootedPath::new(VirtualRoot::Project, entry).intern(),
        };
        let attachment_names = options.attachments.iter().map(|a| a.name.clone()).collect();
        let attachments = options
            .attachments
            .into_iter()
            .map(|a| Bytes::new(a.data))
            .collect();
        Ok(Self {
            attachments,
            attachment_names,
            wrapper,
            library: LazyHash::new(Library::builder().with_inputs(inputs).build()),
            fonts,
            packages,
            project,
            main,
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

    /// Names of the attachments the wrapper adds.
    pub(crate) fn attachment_names(&self) -> &[String] {
        &self.attachment_names
    }

    pub(crate) fn date(&self) -> Option<Datetime> {
        self.date
    }

    fn read(&self, id: FileId) -> FileResult<Bytes> {
        match id.root() {
            VirtualRoot::Project => {
                let key = id.vpath().get_without_slash();
                // Checked before the project, so no project file can shadow it.
                if let Some(rest) = key
                    .strip_prefix(RESERVED_DIR)
                    .and_then(|r| r.strip_prefix('/'))
                {
                    return rest
                        .strip_prefix("attachments/")
                        .and_then(|i| i.parse::<usize>().ok())
                        .and_then(|i| self.attachments.get(i).cloned())
                        .ok_or_else(|| FileError::NotFound(key.into()));
                }
                self.project.read(id.vpath(), self.max_file_size)
            }
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
        let text = match &self.wrapper {
            Some(wrapper) if id == self.main => wrapper.clone(),
            _ => {
                let bytes = self.read(id)?;
                let text = std::str::from_utf8(&bytes).map_err(|_| FileError::InvalidUtf8)?;
                text.strip_prefix('\u{feff}').unwrap_or(text).to_string()
            }
        };
        let source = Source::new(id, text);
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

/// Path of the synthetic main file, in the reserved directory.
const WRAPPER: &str = ".pdfq/main.typ";

/// The synthetic main file used with attachments: it includes the user's
/// entrypoint, then attaches the files. Nothing in the user's document (show
/// rules, unclosed comments, a body rendered twice) can reach the attach calls.
/// `std.` guards against a document shadowing `pdf` or `read`.
fn wrapper(entrypoint: &str, attachments: &[Attachment]) -> String {
    let literal = |s: &str| s.repr().to_string();
    let mut code = format!("#include {}\n", literal(&format!("/{entrypoint}")));
    for (i, a) in attachments.iter().enumerate() {
        // An absolute path: the name in the PDF is the root-relative path.
        code.push_str(&format!(
            "#std.pdf.attach({}, std.read(\"/{RESERVED_DIR}/attachments/{i}\", encoding: none), \
             mime-type: {}, description: {}",
            literal(&format!("/{}", a.name)),
            literal(&a.mime_type),
            literal(&a.description),
        ));
        if let Some(relationship) = a.relationship.typst_name() {
            code.push_str(&format!(", relationship: {}", literal(relationship)));
        }
        code.push_str(")\n");
    }
    code
}
