//! Core of PDFQ: a sandboxed Typst [`World`](typst::World), compilation and PDF export.
//!
//! This crate performs no process isolation itself. It is meant to run inside a
//! resource-limited worker process (see `pdfq-worker`).

#![forbid(unsafe_code)]

mod compile;
mod diagnostics;
mod facturx;
mod fonts;
mod packages;
mod project;
mod world;

pub use compile::{CompileFailure, CompileOptions, Compiled, compile, parse_pdf_standard};
pub use fonts::Fonts;
pub use packages::{PackageAllowlist, Packages};
pub use pdfq_protocol::parse_size;
pub use pdfq_protocol::{Diagnostic, Severity};
pub use project::{Project, ProjectError};
pub use world::{PdfqWorld, WorldOptions};

/// Drops Typst's memoization caches. Call between jobs in long-lived processes.
pub fn evict_cache() {
    typst::comemo::evict(0);
}

pub use pdfq_protocol::TYPST_VERSION;
