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
pub const PROTOCOL_VERSION: u32 = 1;

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
                ..JobOptions::default()
            },
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &job).unwrap();
        let back: Job = read_frame(&mut buf.as_slice(), 1024).unwrap().unwrap();
        assert_eq!(back.options.max_pages, 10);
        assert!(
            matches!(back.source, JobSource::Files { ref files, .. } if files[0].data == b"Hello")
        );
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
