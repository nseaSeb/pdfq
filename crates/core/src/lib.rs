//! Core of PDFQ: a sandboxed Typst [`World`](typst::World), compilation and PDF export.
//!
//! This crate performs no process isolation itself. It is meant to run inside a
//! resource-limited worker process (see `pdfq-worker`).

#![forbid(unsafe_code)]

mod compile;
mod diagnostics;
mod fonts;
mod packages;
mod project;
mod size;
mod world;

pub use compile::{CompileFailure, CompileOptions, Compiled, compile, parse_pdf_standard};
pub use fonts::Fonts;
pub use packages::{PackageAllowlist, Packages};
pub use pdfq_protocol::{Diagnostic, Severity};
pub use project::{Project, ProjectError};
pub use size::parse_size;
pub use world::{PdfqWorld, WorldOptions};

/// Drops Typst's memoization caches. Call between jobs in long-lived processes.
pub fn evict_cache() {
    typst::comemo::evict(0);
}

/// The exact Typst version PDFQ is built against.
pub const TYPST_VERSION: &str = "0.15.1";
