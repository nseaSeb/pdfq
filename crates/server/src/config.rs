//! Server configuration: every setting has a default and a `PDFQ_*` env var.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Args;

#[derive(Debug, Clone, Args)]
pub struct Config {
    /// Address to listen on.
    #[arg(long, env = "PDFQ_BIND", default_value = "0.0.0.0:3000")]
    pub bind: SocketAddr,

    /// Number of worker processes (default: number of CPUs).
    #[arg(long, env = "PDFQ_WORKERS")]
    pub workers: Option<usize>,
    /// Jobs a worker runs before being replaced.
    #[arg(long, env = "PDFQ_JOBS_PER_WORKER", default_value_t = 100)]
    pub jobs_per_worker: u32,
    /// Queued jobs before requests are refused with 429.
    #[arg(long, env = "PDFQ_QUEUE_SIZE", default_value_t = 64)]
    pub queue_size: usize,
    /// Path of the pdfq-worker binary (default: next to this binary).
    #[arg(long, env = "PDFQ_WORKER_BIN")]
    pub worker_bin: Option<PathBuf>,

    /// Wall-clock compilation time per job.
    #[arg(long, env = "PDFQ_WALL_TIMEOUT", default_value = "30s", value_parser = parse_duration)]
    pub wall_timeout: Duration,
    /// CPU time per job.
    #[arg(long, env = "PDFQ_CPU_TIMEOUT", default_value = "20s", value_parser = parse_duration)]
    pub cpu_timeout: Duration,
    /// How long a synchronous request may wait for a free worker.
    #[arg(long, env = "PDFQ_QUEUE_TIMEOUT", default_value = "30s", value_parser = parse_duration)]
    pub queue_timeout: Duration,
    /// Memory per job, above the worker baseline.
    #[arg(long, env = "PDFQ_MEMORY_LIMIT", default_value = "512MiB", value_parser = parse_size)]
    pub memory_limit: u64,

    /// Maximum size of each file (after unzip).
    #[arg(long, env = "PDFQ_MAX_FILE_SIZE", default_value = "10MiB", value_parser = parse_size)]
    pub max_file_size: u64,
    /// Maximum size of a whole request (after unzip).
    #[arg(long, env = "PDFQ_MAX_UPLOAD_SIZE", default_value = "50MiB", value_parser = parse_size)]
    pub max_upload_size: u64,
    /// Maximum number of files per request (after unzip).
    #[arg(long, env = "PDFQ_MAX_FILES", default_value_t = 200)]
    pub max_files: usize,
    /// Maximum size of `data` plus `input.*` fields.
    #[arg(long, env = "PDFQ_MAX_DATA_SIZE", default_value = "1MiB", value_parser = parse_size)]
    pub max_data_size: u64,
    /// Maximum width x height of an image, read from its header.
    #[arg(long, env = "PDFQ_MAX_IMAGE_PIXELS", default_value_t = 50_000_000)]
    pub max_image_pixels: u64,
    /// Maximum number of pages.
    #[arg(long, env = "PDFQ_MAX_PAGES", default_value_t = 500)]
    pub max_pages: u32,
    /// Maximum size of the generated PDF.
    #[arg(long, env = "PDFQ_MAX_OUTPUT_SIZE", default_value = "100MiB", value_parser = parse_size)]
    pub max_output_size: u64,

    /// Font directories (comma-separated), in addition to the embedded fonts.
    #[arg(long, env = "PDFQ_FONT_PATHS", value_delimiter = ',')]
    pub font_paths: Vec<PathBuf>,
    /// Package directories laid out as <dir>/preview/<name>/<version> (comma-separated).
    #[arg(long, env = "PDFQ_PACKAGE_PATHS", value_delimiter = ',')]
    pub package_paths: Vec<PathBuf>,
    /// Allowed packages like @preview/cetz:0.4.0 (comma-separated). Default: all installed.
    #[arg(long, env = "PDFQ_ALLOWED_PACKAGES", value_delimiter = ',')]
    pub allowed_packages: Vec<String>,
    /// Directory holding one sub-directory per template.
    #[arg(long, env = "PDFQ_TEMPLATE_DIR")]
    pub template_dir: Option<PathBuf>,
}

impl Config {
    /// Defaults, as if no flag or env var was given (`PDFQ_*` variables are ignored).
    pub fn defaults() -> Self {
        use clap::{CommandFactory, FromArgMatches};
        #[derive(clap::Parser)]
        struct Wrapper {
            #[command(flatten)]
            config: Config,
        }
        let command = Wrapper::command().mut_args(|arg| arg.env(None));
        let matches = command.get_matches_from(["pdfq"]);
        Wrapper::from_arg_matches(&matches)
            .expect("defaults are valid")
            .config
    }
}

fn too_long(s: &str) -> String {
    format!("duration `{s}` is too long")
}

/// Parses `500ms`, `30s`, `5m`, `1h` or a number of seconds.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (number, unit) = s.split_at(split);
    let n: u64 = number
        .parse()
        .map_err(|_| format!("invalid duration `{s}`"))?;
    match unit {
        "ms" => Ok(Duration::from_millis(n)),
        "" | "s" => Ok(Duration::from_secs(n)),
        "m" => n
            .checked_mul(60)
            .map(Duration::from_secs)
            .ok_or_else(|| too_long(s)),
        "h" => n
            .checked_mul(3600)
            .map(Duration::from_secs)
            .ok_or_else(|| too_long(s)),
        _ => Err(format!(
            "invalid duration unit in `{s}` (use ms, s, m or h)"
        )),
    }
}

pub use pdfq_protocol::parse_size;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_durations_and_sizes() {
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("2m"), Ok(Duration::from_secs(120)));
        assert_eq!(parse_duration("250ms"), Ok(Duration::from_millis(250)));
        assert!(parse_duration("3d").is_err());
        assert!(parse_duration("999999999999999999h").is_err());
        assert_eq!(parse_size("10MiB"), Ok(10 << 20));
        assert!(parse_size("10MB").is_err());
    }

    #[test]
    fn defaults_match_the_spec() {
        let c = Config::defaults();
        assert_eq!(c.max_pages, 500);
        assert_eq!(c.max_upload_size, 50 << 20);
        assert_eq!(c.wall_timeout, Duration::from_secs(30));
    }
}
