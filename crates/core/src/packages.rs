use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use typst::diag::{FileError, FileResult, PackageError};
use typst::foundations::Bytes;
use typst::syntax::VirtualPath;
use typst::syntax::package::PackageSpec;

use crate::project::read_confined;

/// Which packages documents may import.
#[derive(Debug, Clone, Default)]
pub enum PackageAllowlist {
    /// Any package present in the package directories.
    #[default]
    Installed,
    /// Only these specs, written like `@preview/cetz:0.4.0`.
    Only(HashSet<String>),
}

/// Locally installed packages, laid out as `<dir>/<namespace>/<name>/<version>/`.
/// Nothing is ever downloaded.
#[derive(Default)]
pub struct Packages {
    dirs: Vec<PathBuf>,
    allowlist: PackageAllowlist,
}

/// The only namespace PDFQ serves.
const NAMESPACE: &str = "preview";

impl Packages {
    /// Package directories are searched in order; missing ones are skipped.
    pub fn new<P: AsRef<Path>>(dirs: &[P], allowlist: PackageAllowlist) -> Self {
        let dirs = dirs
            .iter()
            .filter_map(|d| fs::canonicalize(d).ok())
            .collect();
        Self { dirs, allowlist }
    }

    /// Whether `spec` may be imported: it must be a `@preview` package, installed
    /// locally, and in the allowlist. A package that is not installed is reported
    /// as not allowed: from the document's point of view it is unavailable either way.
    pub fn is_allowed(&self, spec: &PackageSpec) -> bool {
        spec.namespace == NAMESPACE
            && self.is_installed(spec)
            && match &self.allowlist {
                PackageAllowlist::Installed => true,
                PackageAllowlist::Only(list) => list.contains(&spec.to_string()),
            }
    }

    fn is_installed(&self, spec: &PackageSpec) -> bool {
        let rel = spec_dir(spec);
        self.dirs.iter().any(|dir| dir.join(&rel).is_dir())
    }

    /// Reads a file of an allowed, installed package. The caller must check
    /// [`Self::is_allowed`] first.
    pub(crate) fn read(
        &self,
        spec: &PackageSpec,
        vpath: &VirtualPath,
        max_size: u64,
    ) -> FileResult<Bytes> {
        let rel = spec_dir(spec);
        for dir in &self.dirs {
            let Ok(root) = fs::canonicalize(dir.join(&rel)) else {
                continue;
            };
            if !root.starts_with(dir) {
                return Err(FileError::AccessDenied);
            }
            return read_confined(&root, vpath.get_without_slash(), max_size);
        }
        Err(FileError::Package(PackageError::NotFound(spec.clone())))
    }
}

fn spec_dir(spec: &PackageSpec) -> PathBuf {
    Path::new(spec.namespace.as_str())
        .join(spec.name.as_str())
        .join(spec.version.to_string())
}
