//! Shared test helpers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Builds `pdfq-worker` once (or uses `PDFQ_WORKER_BIN`).
pub fn worker_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(bin) = std::env::var_os("PDFQ_WORKER_BIN") {
            return bin.into();
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-q", "-p", "pdfq-worker"])
            .current_dir(workspace())
            .status()
            .expect("failed to run cargo");
        assert!(status.success(), "building pdfq-worker failed");
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace().join("target"));
        target.join("debug/pdfq-worker")
    })
    .clone()
}
