use pdfq_protocol::{Diagnostic, Severity};
use typst::World;
use typst::WorldExt;
use typst::diag::{self, SourceDiagnostic};
use typst::syntax::VirtualRoot;

/// Resolves a Typst diagnostic to file, line and column.
pub(crate) fn resolve(world: &dyn World, diag: &SourceDiagnostic) -> Diagnostic {
    let severity = match diag.severity {
        diag::Severity::Error => Severity::Error,
        diag::Severity::Warning => Severity::Warning,
    };
    // Hints written for the Typst CLI (e.g. `--root`) mean nothing to API users.
    let hints = diag
        .hints
        .iter()
        .map(|h| h.v.to_string())
        .filter(|h| !h.contains("--root") && !h.contains("--package-path"))
        .collect();
    let mut out = Diagnostic {
        severity,
        message: diag.message.to_string(),
        file: None,
        line: None,
        column: None,
        hints,
    };

    let Some(id) = diag.span.id() else { return out };
    let vpath = id.vpath().get_without_slash();
    out.file = Some(match id.root() {
        VirtualRoot::Project => vpath.to_string(),
        VirtualRoot::Package(spec) => format!("{spec}/{vpath}"),
    });
    if let (Ok(source), Some(range)) = (world.source(id), world.range(diag.span))
        && let Some((line, column)) = source.lines().byte_to_line_column(range.start)
    {
        out.line = Some(line + 1);
        out.column = Some(column + 1);
    }
    out
}
