//! Input validation done by the server before a job reaches a worker: paths,
//! sizes, file counts, zip archives and image headers. The worker's own limits
//! remain the last line of defense.

use pdfq_protocol::RESERVED_DIR;
use std::collections::HashSet;
use std::io::{Cursor, Read};

use pdfq_protocol::File;

use crate::error::ApiError;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_file_size: u64,
    pub max_upload_size: u64,
    pub max_files: usize,
    pub max_image_pixels: u64,
}

/// Accumulates uploaded files while enforcing the limits.
pub struct Files {
    limits: Limits,
    files: Vec<File>,
    paths: HashSet<String>,
    total: u64,
}

impl Files {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            files: Vec::new(),
            paths: HashSet::new(),
            total: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn contains(&self, path: &str) -> bool {
        self.paths.contains(path)
    }

    pub fn into_vec(self) -> Vec<File> {
        self.files
    }

    /// Moves the files out, leaving an empty set with the same limits.
    pub fn take(&mut self) -> Files {
        std::mem::replace(self, Files::new(self.limits))
    }

    /// Caps the bytes this set may still accept (shared request budget).
    pub fn limit_remaining(&mut self, budget: u64) {
        self.limits.max_upload_size = self.total.saturating_add(budget);
    }

    /// Bytes accepted so far.
    pub fn total(&self) -> u64 {
        self.total
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Bytes still allowed for the whole request.
    pub fn remaining(&self) -> u64 {
        self.limits.max_upload_size.saturating_sub(self.total)
    }

    pub fn add(&mut self, path: &str, data: Vec<u8>) -> Result<(), ApiError> {
        let path = normalize_path(path)?;
        if self.files.len() >= self.limits.max_files {
            return Err(ApiError::too_large(format!(
                "too many files (limit {})",
                self.limits.max_files
            )));
        }
        let size = data.len() as u64;
        if size > self.limits.max_file_size {
            return Err(ApiError::too_large(format!(
                "`{path}` is {size} bytes, the limit is {} bytes",
                self.limits.max_file_size
            )));
        }
        if size > self.remaining() {
            return Err(ApiError::too_large(format!(
                "the request exceeds {} bytes",
                self.limits.max_upload_size
            )));
        }
        check_image(&path, &data, self.limits.max_image_pixels)?;
        if !self.paths.insert(path.clone()) {
            return Err(ApiError::invalid(format!("duplicate file `{path}`")));
        }
        self.total += size;
        self.files.push(File { path, data });
        Ok(())
    }

    /// Extracts a zip archive. Sizes are enforced on the bytes actually
    /// decompressed, never on the sizes declared in the archive.
    pub fn add_zip(&mut self, archive: &[u8]) -> Result<(), ApiError> {
        let invalid =
            |e: zip::result::ZipError| ApiError::invalid(format!("invalid zip archive: {e}"));
        let mut zip = zip::ZipArchive::new(Cursor::new(archive)).map_err(invalid)?;
        // Directories count too, but an archive with this many entries is never legitimate.
        if zip.len() > self.limits.max_files.saturating_mul(4) + 64 {
            return Err(ApiError::too_large(format!(
                "the archive has {} entries (limit {} files)",
                zip.len(),
                self.limits.max_files
            )));
        }
        for index in 0..zip.len() {
            let mut entry = zip.by_index(index).map_err(invalid)?;
            if entry.is_dir() {
                continue;
            }
            let name = entry.name().map_err(invalid)?.to_string();
            if entry.is_symlink() {
                return Err(ApiError::invalid(format!(
                    "symbolic link `{name}` in the archive"
                )));
            }
            let cap = self.limits.max_file_size.min(self.remaining());
            let mut data = Vec::new();
            (&mut entry)
                .take(cap + 1)
                .read_to_end(&mut data)
                .map_err(|e| ApiError::invalid(format!("invalid zip entry `{name}`: {e}")))?;
            if data.len() as u64 > cap {
                return Err(ApiError::too_large(format!(
                    "`{name}` decompresses beyond the size limits"
                )));
            }
            self.add(&name, data)?;
        }
        Ok(())
    }
}

/// Normalizes a relative path: `/`-separated, no `..`, no absolute paths.
pub fn normalize_path(path: &str) -> Result<String, ApiError> {
    let invalid = || ApiError::invalid(format!("invalid file path `{path}`"));
    if path.is_empty() || path.len() > 255 || path.contains(['\\', '\0']) || path.starts_with('/') {
        return Err(invalid());
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." => {}
            "" | ".." => return Err(invalid()),
            s if s.chars().any(char::is_control) => return Err(invalid()),
            s => segments.push(s),
        }
    }
    if segments.is_empty() {
        return Err(invalid());
    }
    if segments[0] == RESERVED_DIR {
        return Err(ApiError::invalid(format!(
            "invalid file path `{path}`: `{RESERVED_DIR}/` is reserved"
        )));
    }
    Ok(segments.join("/"))
}

/// Refuses images whose header announces more than `max_pixels`. Every file is
/// sniffed, whatever its extension: Typst detects image formats from the bytes.
fn check_image(path: &str, data: &[u8], max_pixels: u64) -> Result<(), ApiError> {
    // Not an image, or an unreadable header: left to the compiler.
    let Ok(size) = imagesize::blob_size(data) else {
        return Ok(());
    };
    let pixels = size.width as u64 * size.height as u64;
    if pixels > max_pixels {
        return Err(ApiError::invalid(format!(
            "image `{path}` is {}x{} pixels, the limit is {max_pixels} pixels",
            size.width, size.height
        )));
    }
    Ok(())
}

pub use pdfq_protocol::is_valid_template_name;

#[cfg(test)]
mod tests {
    use std::io::Write;

    use pdfq_protocol::ErrorKind;

    use super::*;

    fn limits() -> Limits {
        Limits {
            max_file_size: 1000,
            max_upload_size: 3000,
            max_files: 5,
            max_image_pixels: 100,
        }
    }

    #[test]
    fn normalizes_paths() {
        assert_eq!(normalize_path("./img/logo.png").unwrap(), "img/logo.png");
        for bad in [
            "",
            "/etc/passwd",
            "../x",
            "a/../../x",
            "a//b",
            "a\\b",
            "a/\u{7}b",
        ] {
            assert!(normalize_path(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn enforces_counts_and_sizes() {
        let mut files = Files::new(limits());
        assert_eq!(
            files.add("big", vec![0; 1001]).unwrap_err().0.kind,
            ErrorKind::PayloadTooLarge
        );
        files.add("a", vec![0; 1000]).unwrap();
        assert_eq!(
            files.add("./a", vec![]).unwrap_err().0.kind,
            ErrorKind::InvalidInput
        );
        files.add("b", vec![0; 1000]).unwrap();
        files.add("c", vec![0; 1000]).unwrap();
        assert_eq!(
            files.add("d", vec![0; 1]).unwrap_err().0.kind,
            ErrorKind::PayloadTooLarge
        );
    }

    #[test]
    fn refuses_image_bombs() {
        let png = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../tests/fixtures/hostile/image-bomb.png"
        ))
        .unwrap();
        let mut files = Files::new(Limits {
            max_file_size: 1 << 20,
            ..limits()
        });
        let err = files.add("bomb.png", png.clone()).unwrap_err();
        assert_eq!(err.0.kind, ErrorKind::InvalidInput);
        assert!(err.0.message.contains("100000x100000"), "{}", err.0.message);
        // Renaming the file does not bypass the check.
        assert!(files.add("bomb.bin", png).is_err());
    }

    fn zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, data) in entries {
            out.start_file(*name, options).unwrap();
            out.write_all(data).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    #[test]
    fn extracts_zip_archives() {
        let mut files = Files::new(limits());
        files
            .add_zip(&zip(&[("main.typ", b"Hello"), ("lib/a.typ", b"x")]))
            .unwrap();
        assert!(files.contains("main.typ") && files.contains("lib/a.typ"));
    }

    #[test]
    fn refuses_zip_bombs_and_traversal() {
        let bomb = vec![0u8; 10 << 20];
        let err = Files::new(limits())
            .add_zip(&zip(&[("bomb.bin", &bomb)]))
            .unwrap_err();
        assert_eq!(err.0.kind, ErrorKind::PayloadTooLarge);

        let err = Files::new(limits())
            .add_zip(&zip(&[("../evil.typ", b"x")]))
            .unwrap_err();
        assert_eq!(err.0.kind, ErrorKind::InvalidInput);

        let many: Vec<(String, &[u8])> = (0..10).map(|i| (format!("f{i}"), &b"x"[..])).collect();
        let many: Vec<(&str, &[u8])> = many.iter().map(|(n, d)| (n.as_str(), *d)).collect();
        let err = Files::new(limits()).add_zip(&zip(&many)).unwrap_err();
        assert_eq!(err.0.kind, ErrorKind::PayloadTooLarge);
    }
}
