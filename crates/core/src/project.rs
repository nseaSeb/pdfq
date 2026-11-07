use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use typst::diag::{FileError, FileResult};
use typst::foundations::Bytes;
use typst::syntax::VirtualPath;

/// The files of a project: an optional directory on disk (a mounted template)
/// overlaid with in-memory files (uploads).
///
/// Disk files win over memory files, so uploads can never shadow template files.
/// Disk access is confined to the canonical root: `..` and symlinks that
/// resolve outside of it are refused.
#[derive(Default)]
pub struct Project {
    disk: Option<PathBuf>,
    memory: HashMap<String, Bytes>,
}

#[derive(Debug)]
pub enum ProjectError {
    /// The path is absolute on the host, escapes the root, or is otherwise invalid.
    InvalidPath(String),
    /// Two in-memory files map to the same normalized path.
    Duplicate(String),
    /// The disk root does not exist or cannot be resolved.
    InvalidRoot(PathBuf),
}

impl fmt::Display for ProjectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::InvalidPath(p) => write!(f, "invalid file path `{p}`"),
            Self::Duplicate(p) => write!(f, "duplicate file path `{p}`"),
            Self::InvalidRoot(p) => write!(f, "invalid project root `{}`", p.display()),
        }
    }
}

impl std::error::Error for ProjectError {}

impl Project {
    /// A project backed by a directory on disk.
    pub fn from_dir(root: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let root = root.as_ref();
        let canonical =
            fs::canonicalize(root).map_err(|_| ProjectError::InvalidRoot(root.to_path_buf()))?;
        if !canonical.is_dir() {
            return Err(ProjectError::InvalidRoot(root.to_path_buf()));
        }
        Ok(Self {
            disk: Some(canonical),
            memory: HashMap::new(),
        })
    }

    /// Adds an in-memory file. `path` is relative to the project root
    /// (`main.typ`, `img/logo.png`); it must not escape the root.
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) -> Result<(), ProjectError> {
        let key = normalize(path)?;
        if self.memory.contains_key(&key) {
            return Err(ProjectError::Duplicate(key));
        }
        self.memory.insert(key, Bytes::new(data));
        Ok(())
    }

    /// Whether a file exists at `path`, on disk or in memory.
    pub fn contains(&self, path: &str) -> bool {
        let Ok(vpath) = VirtualPath::new(path) else {
            return false;
        };
        let key = vpath.get_without_slash();
        self.memory.contains_key(key)
            || self
                .disk
                .as_ref()
                .is_some_and(|root| resolve_confined(root, key).is_ok_and(|p| p.is_file()))
    }

    pub(crate) fn read(&self, vpath: &VirtualPath, max_size: u64) -> FileResult<Bytes> {
        let key = vpath.get_without_slash();
        if let Some(root) = &self.disk {
            match read_confined(root, key, max_size) {
                Err(FileError::NotFound(_)) => {}
                other => return other,
            }
        }
        let data = self
            .memory
            .get(key)
            .ok_or_else(|| FileError::NotFound(PathBuf::from(key)))?;
        if data.len() as u64 > max_size {
            return Err(too_large());
        }
        Ok(data.clone())
    }
}

/// Normalizes an in-memory path into the key used for lookups.
fn normalize(path: &str) -> Result<String, ProjectError> {
    let invalid = || ProjectError::InvalidPath(path.to_string());
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return Err(invalid());
    }
    let vpath = VirtualPath::new(path).map_err(|_| invalid())?;
    let key = vpath.get_without_slash();
    if key.is_empty() {
        return Err(invalid());
    }
    Ok(key.to_string())
}

fn too_large() -> FileError {
    FileError::Other(Some("file exceeds the size limit".into()))
}

/// Resolves `rel` below `root` (canonical), refusing anything outside of it.
fn resolve_confined(root: &Path, rel: &str) -> FileResult<PathBuf> {
    let canonical =
        fs::canonicalize(root.join(rel)).map_err(|e| FileError::from_io(e, Path::new(rel)))?;
    if !canonical.starts_with(root) {
        return Err(FileError::AccessDenied);
    }
    Ok(canonical)
}

/// Reads `rel` below `root` (canonical), refusing anything that resolves outside of it.
pub(crate) fn read_confined(root: &Path, rel: &str, max_size: u64) -> FileResult<Bytes> {
    let canonical = resolve_confined(root, rel)?;
    let meta = fs::metadata(&canonical).map_err(|e| FileError::from_io(e, Path::new(rel)))?;
    if meta.is_dir() {
        return Err(FileError::IsDirectory);
    }
    if !meta.is_file() {
        return Err(FileError::AccessDenied);
    }
    if meta.len() > max_size {
        return Err(too_large());
    }
    let data = fs::read(&canonical).map_err(|e| FileError::from_io(e, Path::new(rel)))?;
    Ok(Bytes::new(data))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths() {
        let mut p = Project::default();
        for bad in ["../x.typ", "a/../../x.typ", "", "a\\b.typ", "/"] {
            assert!(p.add_file(bad, b"x".to_vec()).is_err(), "{bad} accepted");
        }
    }

    #[test]
    fn normalizes_and_detects_duplicates() {
        let mut p = Project::default();
        p.add_file("./img/logo.png", b"x".to_vec()).unwrap();
        assert!(p.contains("img/logo.png"));
        assert!(p.contains("/img/logo.png"));
        assert!(matches!(
            p.add_file("img/./logo.png", b"y".to_vec()),
            Err(ProjectError::Duplicate(_))
        ));
    }

    #[test]
    fn disk_files_win_over_memory_files() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("logo.png"), b"disk").unwrap();
        let mut p = Project::from_dir(dir.path()).unwrap();
        p.add_file("logo.png", b"memory".to_vec()).unwrap();
        let data = p
            .read(&VirtualPath::new("logo.png").unwrap(), u64::MAX)
            .unwrap();
        assert_eq!(data.as_slice(), b"disk");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlink_escaping_root() {
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        let root = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret.txt"),
            root.path().join("link.txt"),
        )
        .unwrap();
        let p = Project::from_dir(root.path()).unwrap();
        let err = p
            .read(&VirtualPath::new("link.txt").unwrap(), u64::MAX)
            .unwrap_err();
        assert!(matches!(err, FileError::AccessDenied), "{err:?}");
    }

    #[test]
    fn enforces_size_limit_on_memory_files() {
        let mut p = Project::default();
        p.add_file("big.bin", vec![0u8; 100]).unwrap();
        assert!(p.read(&VirtualPath::new("big.bin").unwrap(), 10).is_err());
        assert!(p.read(&VirtualPath::new("big.bin").unwrap(), 100).is_ok());
    }

    #[test]
    fn enforces_size_limit() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.bin"), vec![0u8; 100]).unwrap();
        let p = Project::from_dir(dir.path()).unwrap();
        assert!(p.read(&VirtualPath::new("big.bin").unwrap(), 10).is_err());
    }
}
