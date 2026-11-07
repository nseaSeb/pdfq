use pdfq_protocol::FacturxProfile;
use serde::Deserialize;
use serde::de::IntoDeserializer;
use serde::de::value::Error as DeError;
use typst::foundations::Datetime;
use typst::foundations::NativeElement;
use typst::introspection::Introspector;
use typst::pdf::AttachElem;
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
    /// Write the Factur-X XMP metadata (requires a PDF/A-3 standard).
    pub facturx: Option<FacturxProfile>,
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
    /// Post-processing of the exported PDF failed (a PDFQ bug, not the document's).
    Internal(String),
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
    let pdf_a3 = options
        .pdf_standards
        .iter()
        .any(|s| matches!(s, PdfStandard::A_3a | PdfStandard::A_3b | PdfStandard::A_3u));
    if options.facturx.is_some() && !pdf_a3 {
        return Err(CompileFailure::InvalidOptions(
            "Factur-X requires a PDF/A-3 standard".into(),
        ));
    }

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

    // The wrapper keeps the attach calls out of the document's reach; this
    // guard makes sure a PDF never ships without an expected attachment.
    let attached: Vec<String> = document
        .introspector()
        .query(&AttachElem::ELEM.select())
        .iter()
        .filter_map(|e| {
            e.to_packed::<AttachElem>()
                .map(|a| a.path.derived.to_string())
        })
        .collect();
    if let Some(missing) = world
        .attachment_names()
        .iter()
        .find(|name| !attached.contains(name))
    {
        let mut all = vec![Diagnostic {
            severity: Severity::Error,
            message: format!("attachment `{missing}` is missing from the document"),
            file: None,
            line: None,
            column: None,
            hints: Vec::new(),
        }];
        all.append(&mut warnings);
        return Err(CompileFailure::Diagnostics(all));
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
    let pdf = match options.facturx {
        Some(profile) => crate::facturx::add_metadata(&pdf, profile)
            .map_err(|e| CompileFailure::Internal(format!("Factur-X metadata: {e}")))?,
        None => pdf,
    };
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
