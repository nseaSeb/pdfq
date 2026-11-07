use serde::Serialize;
use typst::World;
use typst::WorldExt;
use typst::diag::{self, SourceDiagnostic};
use typst::syntax::VirtualRoot;

/// A Typst diagnostic resolved to a file, line and column (1-based).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Diagnostic {
    pub severity: Severity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    pub hints: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    Warning,
}

impl Diagnostic {
    pub(crate) fn error(message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            message: message.into(),
            file: None,
            line: None,
            column: None,
            hints: Vec::new(),
        }
    }

    pub(crate) fn resolve(world: &dyn World, diag: &SourceDiagnostic) -> Self {
        let severity = match diag.severity {
            diag::Severity::Error => Severity::Error,
            diag::Severity::Warning => Severity::Warning,
        };
        let mut out = Self::error(diag.message.to_string());
        out.severity = severity;
        // Hints written for the Typst CLI (e.g. `--root`) mean nothing to API users.
        out.hints = diag
            .hints
            .iter()
            .map(|h| h.v.to_string())
            .filter(|h| !h.contains("--root") && !h.contains("--package-path"))
            .collect();

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
}
