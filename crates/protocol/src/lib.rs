//! Messages exchanged between the PDFQ server and its worker processes.
//!
//! Each frame is a `u32` little-endian payload length followed by a
//! [postcard](https://docs.rs/postcard) payload. The server writes [`Job`]s to the
//! worker's stdin; the worker writes [`WorkerMessage`]s to its stdout.

#![forbid(unsafe_code)]

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change to the messages below.
pub const PROTOCOL_VERSION: u32 = 2;

/// The exact Typst version PDFQ is built against (pinned in the root `Cargo.toml`).
pub const TYPST_VERSION: &str = "0.15.1";

/// Template names: 1 to 64 lowercase letters, digits, `-` and `_`.
pub fn is_valid_template_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// Parses a byte size such as `10MiB`, `512KiB`, `1GiB` or `1048576`.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let number: u64 = number.parse().map_err(|_| format!("invalid size `{s}`"))?;
    let factor: u64 = match unit.trim() {
        "" | "B" => 1,
        "KiB" => 1 << 10,
        "MiB" => 1 << 20,
        "GiB" => 1 << 30,
        _ => {
            return Err(format!(
                "invalid size unit in `{s}` (use B, KiB, MiB or GiB)"
            ));
        }
    };
    number
        .checked_mul(factor)
        .ok_or_else(|| format!("size `{s}` is too large"))
}

/// Exit code of a worker whose allocation failed (memory limit reached).
pub const EXIT_OOM: i32 = 3;

/// A compilation request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub source: JobSource,
    /// `sys.inputs` entries.
    pub inputs: Vec<(String, String)>,
    pub options: JobOptions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum JobSource {
    /// Uploaded project files.
    Files {
        files: Vec<File>,
        entrypoint: String,
    },
    /// A template mounted on the worker side, plus assets mounted at `/assets/`.
    Template { name: String, assets: Vec<File> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct File {
    /// Path relative to the project root.
    pub path: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Limits use `0` for "no limit".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobOptions {
    pub max_pages: u32,
    pub max_file_size: u64,
    pub max_output_size: u64,
    /// Typst PDF standard names (`a-3b`, `ua-1`, …).
    pub pdf_standards: Vec<String>,
    pub date: Option<Date>,
    pub strict: bool,
    /// Files embedded in the PDF (`pdf.attach`).
    pub attachments: Vec<Attachment>,
    /// Writes the Factur-X XMP metadata. The XML itself is in `attachments`
    /// as [`FACTURX_FILENAME`], and the standards include PDF/A-3.
    pub facturx: Option<FacturxProfile>,
}

/// Project directory reserved for PDFQ's own files (attachments): never read
/// from the project, refused in uploads.
pub const RESERVED_DIR: &str = ".pdfq";

/// File name of the Factur-X XML inside the PDF (fixed by the standard).
pub const FACTURX_FILENAME: &str = "factur-x.xml";

/// A file embedded in the PDF.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    /// Name shown by PDF readers: 1 to 128 of `[A-Za-z0-9._ -]`, not starting with `.`.
    pub name: String,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
    /// `type/subtype`, see [`is_valid_mime_type`].
    pub mime_type: String,
    pub relationship: Relationship,
    pub description: String,
}

/// The PDF/A-3 `AFRelationship` of an attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relationship {
    Source,
    Data,
    Alternative,
    Supplement,
    Unspecified,
}

impl Relationship {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "source" => Self::Source,
            "data" => Self::Data,
            "alternative" => Self::Alternative,
            "supplement" => Self::Supplement,
            "unspecified" => Self::Unspecified,
            _ => return None,
        })
    }

    /// The Typst `pdf.attach` value; `None` for unspecified.
    pub fn typst_name(self) -> Option<&'static str> {
        match self {
            Self::Source => Some("source"),
            Self::Data => Some("data"),
            Self::Alternative => Some("alternative"),
            Self::Supplement => Some("supplement"),
            Self::Unspecified => None,
        }
    }
}

/// Factur-X profiles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FacturxProfile {
    Minimum,
    BasicWl,
    Basic,
    En16931,
    Extended,
}

impl FacturxProfile {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "minimum" => Self::Minimum,
            "basic-wl" => Self::BasicWl,
            "basic" => Self::Basic,
            "en16931" => Self::En16931,
            "extended" => Self::Extended,
            _ => return None,
        })
    }

    /// The `fx:ConformanceLevel` value.
    pub fn conformance_level(self) -> &'static str {
        match self {
            Self::Minimum => "MINIMUM",
            Self::BasicWl => "BASIC WL",
            Self::Basic => "BASIC",
            Self::En16931 => "EN 16931",
            Self::Extended => "EXTENDED",
        }
    }

    /// MINIMUM and BASIC WL are not full invoices: their XML can only be `data`.
    pub fn allows_relationship(self, relationship: Relationship) -> bool {
        match self {
            Self::Minimum | Self::BasicWl => relationship == Relationship::Data,
            _ => matches!(
                relationship,
                Relationship::Data | Relationship::Source | Relationship::Alternative
            ),
        }
    }
}

/// Attachment names: safe in PDF readers and file systems once extracted.
pub fn is_valid_attachment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b' '))
}

/// A `type/subtype` MIME type without parameters, as accepted by krilla
/// (letters, digits, `-`, `+`, `.`).
pub fn is_valid_mime_type(mime: &str) -> bool {
    let token = |s: &str| {
        !s.is_empty()
            && s.len() <= 127
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-+.".contains(&b))
    };
    matches!(mime.split_once('/'), Some((t, sub)) if token(t) && token(sub))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Date {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

/// Messages sent by a worker.
#[derive(Debug, Serialize, Deserialize)]
pub enum WorkerMessage {
    /// Sent once at startup, after fonts are loaded.
    Ready(Ready),
    Done(JobOutput),
    Failed(JobError),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Ready {
    pub protocol_version: u32,
    pub typst_version: String,
    pub font_families: u32,
    /// Address space used after initialization, in bytes (0 if unknown).
    pub baseline_memory: u64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct JobOutput {
    #[serde(with = "serde_bytes")]
    pub pdf: Vec<u8>,
    pub pages: u32,
    pub warnings: Vec<Diagnostic>,
}

/// A typed failure, shared by the worker protocol and the HTTP API.
///
/// No `skip_serializing_if` here or below: postcard is positional, and skipped
/// fields would corrupt the frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobError {
    pub kind: ErrorKind,
    pub message: String,
    pub detail: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

impl JobError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            detail: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    InvalidInput,
    SchemaViolation,
    NotFound,
    PayloadTooLarge,
    CompileError,
    PackageNotAllowed,
    TooManyPages,
    OutputTooLarge,
    Timeout,
    MemoryLimit,
    QueueFull,
    WorkerCrashed,
    /// HTTP only: the async job has not finished yet.
    NotFinished,
    /// A PDFQ bug (e.g. PDF post-processing failed). The worker stays alive.
    Internal,
}

impl ErrorKind {
    /// Every kind, in declaration order (`kind as usize` indexes it).
    pub const ALL: [ErrorKind; 14] = [
        Self::InvalidInput,
        Self::SchemaViolation,
        Self::NotFound,
        Self::PayloadTooLarge,
        Self::CompileError,
        Self::PackageNotAllowed,
        Self::TooManyPages,
        Self::OutputTooLarge,
        Self::Timeout,
        Self::MemoryLimit,
        Self::QueueFull,
        Self::WorkerCrashed,
        Self::NotFinished,
        Self::Internal,
    ];

    /// The serialized (snake_case) name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidInput => "invalid_input",
            Self::SchemaViolation => "schema_violation",
            Self::NotFound => "not_found",
            Self::PayloadTooLarge => "payload_too_large",
            Self::CompileError => "compile_error",
            Self::PackageNotAllowed => "package_not_allowed",
            Self::TooManyPages => "too_many_pages",
            Self::OutputTooLarge => "output_too_large",
            Self::Timeout => "timeout",
            Self::MemoryLimit => "memory_limit",
            Self::QueueFull => "queue_full",
            Self::WorkerCrashed => "worker_crashed",
            Self::NotFinished => "not_finished",
            Self::Internal => "internal",
        }
    }
}

/// A Typst diagnostic resolved to a file, line and column (1-based).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub hints: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

/// Serializes `msg` and returns its length header and body.
/// The body is one serialized copy of the message (a PDF is copied once).
fn encode<T: Serialize>(msg: &T) -> io::Result<([u8; 4], Vec<u8>)> {
    let body = postcard::to_allocvec(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "frame too large"))?;
    Ok((len.to_le_bytes(), body))
}

fn check_len(header: [u8; 4], max: usize) -> io::Result<usize> {
    let len = u32::from_le_bytes(header) as usize;
    if len > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame of {len} bytes exceeds the {max} bytes limit"),
        ));
    }
    Ok(len)
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    postcard::from_bytes(body).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Writes one frame and flushes.
pub fn write_frame<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let (header, body) = encode(msg)?;
    w.write_all(&header)?;
    w.write_all(&body)?;
    w.flush()
}

/// Reads one frame. Returns `Ok(None)` on end of stream before a frame starts.
/// Frames larger than `max` bytes are rejected before allocating.
pub fn read_frame<R: Read, T: DeserializeOwned>(r: &mut R, max: usize) -> io::Result<Option<T>> {
    let mut header = [0u8; 4];
    // End of stream is clean only before the first header byte.
    loop {
        match r.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    r.read_exact(&mut header[1..])?;
    let mut body = vec![0u8; check_len(header, max)?];
    r.read_exact(&mut body)?;
    decode(&body).map(Some)
}

#[cfg(feature = "tokio")]
mod asynchronous {
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::*;

    /// Async counterpart of [`write_frame`].
    pub async fn write_frame_async<W: AsyncWrite + Unpin, T: Serialize>(
        w: &mut W,
        msg: &T,
    ) -> io::Result<()> {
        let (header, body) = encode(msg)?;
        w.write_all(&header).await?;
        w.write_all(&body).await?;
        w.flush().await
    }

    /// Async counterpart of [`read_frame`].
    pub async fn read_frame_async<R: AsyncRead + Unpin, T: DeserializeOwned>(
        r: &mut R,
        max: usize,
    ) -> io::Result<Option<T>> {
        let mut header = [0u8; 4];
        // End of stream is clean only before the first header byte.
        if r.read(&mut header[..1]).await? == 0 {
            return Ok(None);
        }
        r.read_exact(&mut header[1..]).await?;
        let mut body = vec![0u8; check_len(header, max)?];
        r.read_exact(&mut body).await?;
        decode(&body).map(Some)
    }
}

#[cfg(feature = "tokio")]
pub use asynchronous::{read_frame_async, write_frame_async};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_job() {
        let job = Job {
            source: JobSource::Files {
                files: vec![File {
                    path: "main.typ".into(),
                    data: b"Hello".to_vec(),
                }],
                entrypoint: "main.typ".into(),
            },
            inputs: vec![("data".into(), "{}".into())],
            options: JobOptions {
                max_pages: 10,
                attachments: vec![Attachment {
                    name: "factur-x.xml".into(),
                    data: b"<x/>".to_vec(),
                    mime_type: "text/xml".into(),
                    relationship: Relationship::Unspecified,
                    description: String::new(),
                }],
                facturx: Some(FacturxProfile::BasicWl),
                strict: true,
                ..JobOptions::default()
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &job).unwrap();
        let back: Job = read_frame(&mut buf.as_slice(), 1024).unwrap().unwrap();
        assert_eq!(back.options.max_pages, 10);
        assert_eq!(back.options.attachments, job.options.attachments);
        assert_eq!(back.options.facturx, Some(FacturxProfile::BasicWl));
        assert!(back.options.strict);
        assert!(
            matches!(back.source, JobSource::Files { ref files, .. } if files[0].data == b"Hello")
        );
    }

    #[test]
    fn all_lists_every_error_kind_in_order() {
        // Adding a variant breaks this match until it is handled here and in ALL.
        let listed = |k: ErrorKind| match k {
            ErrorKind::InvalidInput
            | ErrorKind::SchemaViolation
            | ErrorKind::NotFound
            | ErrorKind::PayloadTooLarge
            | ErrorKind::CompileError
            | ErrorKind::PackageNotAllowed
            | ErrorKind::TooManyPages
            | ErrorKind::OutputTooLarge
            | ErrorKind::Timeout
            | ErrorKind::MemoryLimit
            | ErrorKind::QueueFull
            | ErrorKind::WorkerCrashed
            | ErrorKind::NotFinished
            | ErrorKind::Internal => ErrorKind::ALL.get(k as usize) == Some(&k),
        };
        assert!(ErrorKind::ALL.iter().all(|k| listed(*k)));
        assert!(listed(ErrorKind::Internal), "the last variant closes ALL");
        assert_eq!(ErrorKind::ALL.len(), ErrorKind::Internal as usize + 1);
    }

    #[test]
    fn error_kind_names_match_serde() {
        for kind in ErrorKind::ALL {
            let json = serde_json::to_value(kind).unwrap();
            assert_eq!(json, kind.as_str());
        }
    }

    #[test]
    fn validates_attachment_names_and_mime_types() {
        assert!(is_valid_attachment_name("factur-x.xml"));
        assert!(is_valid_attachment_name("Report 2026.pdf"));
        for bad in ["", ".hidden", "a/b", "a\"b", &"x".repeat(129)] {
            assert!(!is_valid_attachment_name(bad), "{bad}");
        }
        assert!(is_valid_mime_type("application/vnd.ms-excel"));
        for bad in [
            "text",
            "text/",
            "text/plain; charset=utf-8",
            "a/b/c",
            "application/x_foo",
        ] {
            assert!(!is_valid_mime_type(bad), "{bad}");
        }
    }

    #[test]
    fn rejects_oversized_frames_and_reports_eof() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &"x".repeat(100)).unwrap();
        assert!(read_frame::<_, String>(&mut buf.as_slice(), 10).is_err());
        assert!(
            read_frame::<_, String>(&mut [].as_slice(), 10)
                .unwrap()
                .is_none()
        );
        // A stream cut inside the header is an error, not a clean shutdown.
        assert!(read_frame::<_, String>(&mut [5u8, 0].as_slice(), 10).is_err());
    }

    #[test]
    fn round_trips_a_failure() {
        let err = JobError {
            diagnostics: vec![Diagnostic {
                severity: Severity::Error,
                message: "boom".into(),
                file: None,
                line: Some(2),
                column: None,
                hints: Vec::new(),
            }],
            ..JobError::new(ErrorKind::CompileError, "compilation failed")
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &WorkerMessage::Failed(err.clone())).unwrap();
        let back: WorkerMessage = read_frame(&mut buf.as_slice(), 1024).unwrap().unwrap();
        assert!(matches!(back, WorkerMessage::Failed(e) if e == err));
    }

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert_eq!(parse_size("10MiB"), Ok(10 << 20));
        assert_eq!(parse_size("512 KiB"), Ok(512 << 10));
        assert_eq!(parse_size("1GiB"), Ok(1 << 30));
        for bad in ["", "MiB", "10MB", "-1", "99999999999999999999GiB"] {
            assert!(parse_size(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn validates_template_names() {
        assert!(is_valid_template_name("invoice_v2-fr"));
        for bad in ["", "Invoice", "../x", "a/b", &"a".repeat(65)] {
            assert!(!is_valid_template_name(bad), "{bad}");
        }
    }

    #[test]
    fn error_kinds_serialize_in_snake_case() {
        let json = serde_json::to_string(&ErrorKind::PackageNotAllowed).unwrap();
        assert_eq!(json, "\"package_not_allowed\"");
    }
}
