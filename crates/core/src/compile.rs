use serde::Deserialize;
use serde::de::IntoDeserializer;
use serde::de::value::Error as DeError;
use typst::foundations::Datetime;
use typst_layout::PagedDocument;
use typst_pdf::{PdfOptions, PdfStandard, PdfStandards, Timestamp};

use pdfq_protocol::{Diagnostic, Severity};

use crate::diagnostics::resolve;
use crate::world::PdfqWorld;

/// Output settings of a compilation.
#[derive(Debug, Clone, Default)]
pub struct CompileOptions {
    /// Fail with [`CompileFailure::TooManyPages`] above this count (checked before export).
    pub max_pages: Option<usize>,
    /// PDF standards to conform to (e.g. PDF/A-3b).
    pub pdf_standards: Vec<PdfStandard>,
    /// Treat warnings as errors.
    pub strict: bool,
}

/// A successful compilation.
#[derive(Debug)]
pub struct Compiled {
    pub pdf: Vec<u8>,
    pub pages: usize,
    pub warnings: Vec<Diagnostic>,
}

/// Why a compilation produced no PDF.
#[derive(Debug)]
pub enum CompileFailure {
    /// Typst errors (or warnings in strict mode).
    Diagnostics(Vec<Diagnostic>),
    /// The document imported a package outside the allowlist.
    PackageNotAllowed(String),
    /// The laid-out document exceeds the page limit.
    TooManyPages { pages: usize, max: usize },
    /// The options are invalid (e.g. incompatible PDF standards).
    InvalidOptions(String),
}

/// Parses a PDF standard name as accepted by Typst (`a-2b`, `a-3b`, `ua-1`, `1.7`, …).
pub fn parse_pdf_standard(name: &str) -> Result<PdfStandard, String> {
    PdfStandard::deserialize(IntoDeserializer::<DeError>::into_deserializer(name))
        .map_err(|_| format!("unknown PDF standard `{name}`"))
}

/// Compiles the world's main file to PDF.
pub fn compile(world: &PdfqWorld, options: &CompileOptions) -> Result<Compiled, CompileFailure> {
    let standards = PdfStandards::new(&options.pdf_standards)
        .map_err(|e| CompileFailure::InvalidOptions(e.message().to_string()))?;

    let result = typst::compile::<PagedDocument>(world);
    let mut warnings: Vec<_> = result.warnings.iter().map(|d| resolve(world, d)).collect();

    let document = match result.output {
        Ok(document) => document,
        Err(errors) => {
            if let Some(spec) = world.denied_package() {
                return Err(CompileFailure::PackageNotAllowed(spec));
            }
            let mut all: Vec<_> = errors.iter().map(|d| resolve(world, d)).collect();
            all.append(&mut warnings);
            return Err(CompileFailure::Diagnostics(all));
        }
    };

    if options.strict && !warnings.is_empty() {
        for w in &mut warnings {
            w.severity = Severity::Error;
        }
        return Err(CompileFailure::Diagnostics(warnings));
    }

    let pages = document.pages().len();
    if let Some(max) = options.max_pages
        && pages > max
    {
        return Err(CompileFailure::TooManyPages { pages, max });
    }

    let pdf_options = PdfOptions {
        timestamp: Some(timestamp(world.date())),
        standards,
        ..PdfOptions::default()
    };
    let pdf = typst_pdf::pdf(&document, &pdf_options).map_err(|errors| {
        let mut all: Vec<_> = errors.iter().map(|d| resolve(world, d)).collect();
        all.extend(warnings.iter().cloned());
        CompileFailure::Diagnostics(all)
    })?;
    Ok(Compiled {
        pdf,
        pages,
        warnings,
    })
}

fn timestamp(date: Option<Datetime>) -> Timestamp {
    let datetime = date.unwrap_or_else(|| {
        let now = time::OffsetDateTime::now_utc();
        let now = now.replace_nanosecond(0).unwrap_or(now);
        Datetime::Datetime(time::PrimitiveDateTime::new(now.date(), now.time()))
    });
    Timestamp::new_utc(datetime)
}
