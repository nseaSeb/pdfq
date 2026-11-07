//! `pdfq-cli compile input.typ -o out.pdf`: compiles a document with the same
//! core as the PDFQ server, without process isolation. For development only.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use pdfq_core::{
    CompileFailure, CompileOptions, Diagnostic, Fonts, PackageAllowlist, Packages, PdfqWorld,
    Project, WorldOptions, compile, parse_pdf_standard, parse_size,
};
use typst::foundations::Datetime;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Compile a Typst file to PDF.
    Compile(CompileArgs),
}

#[derive(Parser)]
struct CompileArgs {
    /// Main Typst file.
    input: PathBuf,
    /// Output PDF path.
    #[arg(short, long, default_value = "out.pdf")]
    output: PathBuf,
    /// Project root (defaults to the input's directory). Files outside it are not readable.
    #[arg(long)]
    root: Option<PathBuf>,
    /// JSON file exposed as `sys.inputs.data`.
    #[arg(long)]
    data: Option<PathBuf>,
    /// Extra `sys.inputs` entry, as key=value (repeatable).
    #[arg(long = "input", value_parser = parse_key_value)]
    inputs: Vec<(String, String)>,
    /// Additional font directory (repeatable).
    #[arg(long = "font-path", env = "PDFQ_FONT_PATHS", value_delimiter = ',')]
    font_paths: Vec<PathBuf>,
    /// Package directory laid out as <dir>/preview/<name>/<version> (repeatable).
    #[arg(
        long = "package-path",
        env = "PDFQ_PACKAGE_PATHS",
        value_delimiter = ','
    )]
    package_paths: Vec<PathBuf>,
    /// PDF standard, e.g. a-2b, a-3b, ua-1 (repeatable).
    #[arg(long = "pdf-standard")]
    pdf_standards: Vec<String>,
    /// Fixed date (YYYY-MM-DD) for `datetime.today()` and the PDF timestamp.
    #[arg(long, value_parser = parse_date)]
    date: Option<Datetime>,
    /// Maximum number of pages.
    #[arg(long, env = "PDFQ_MAX_PAGES", default_value_t = 500)]
    max_pages: usize,
    /// Maximum size of any single file read by the document (e.g. 10MiB).
    #[arg(long, env = "PDFQ_MAX_FILE_SIZE", default_value = "10MiB", value_parser = parse_size)]
    max_file_size: u64,
    /// Treat warnings as errors.
    #[arg(long)]
    strict: bool,
}

fn parse_key_value(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .ok_or_else(|| format!("expected key=value, got `{s}`"))
}

fn parse_date(s: &str) -> Result<Datetime, String> {
    let parts: Vec<_> = s.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        return Err("expected YYYY-MM-DD".into());
    };
    let invalid = || format!("invalid date `{s}`");
    let year = y.parse::<i32>().map_err(|_| invalid())?;
    let month = m.parse::<u8>().map_err(|_| invalid())?;
    let day = d.parse::<u8>().map_err(|_| invalid())?;
    Datetime::from_ymd(year, month, day).ok_or_else(|| format!("invalid date `{s}`"))
}

fn main() -> ExitCode {
    let Command::Compile(args) = Cli::parse().command;
    match run(args) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

fn run(args: CompileArgs) -> anyhow::Result<ExitCode> {
    let input = args
        .input
        .canonicalize()
        .with_context(|| format!("{}", args.input.display()))?;
    let root = match &args.root {
        Some(root) => root.canonicalize().context("invalid --root")?,
        None => input.parent().unwrap_or(Path::new(".")).to_path_buf(),
    };
    let entrypoint = input
        .strip_prefix(&root)
        .context("the input file must be inside --root")?
        .to_string_lossy()
        .replace('\\', "/");

    let mut inputs = args.inputs;
    if let Some(path) = &args.data {
        let text = std::fs::read_to_string(path).context("reading --data")?;
        serde_json::from_str::<serde_json::Value>(&text).context("--data is not valid JSON")?;
        inputs.push(("data".into(), text));
    }
    let pdf_standards = args
        .pdf_standards
        .iter()
        .map(|s| parse_pdf_standard(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(anyhow::Error::msg)?;

    let fonts = Fonts::load(&args.font_paths);
    let packages = Packages::new(&args.package_paths, PackageAllowlist::Installed);
    let project = Project::from_dir(&root)?;
    let options = WorldOptions {
        entrypoint,
        inputs,
        date: args.date,
        max_file_size: args.max_file_size,
    };
    let world = PdfqWorld::new(project, &fonts, &packages, options)?;

    let options = CompileOptions {
        max_pages: Some(args.max_pages),
        pdf_standards,
        strict: args.strict,
    };
    match compile(&world, &options) {
        Ok(compiled) => {
            print_diagnostics(&compiled.warnings);
            std::fs::write(&args.output, &compiled.pdf).context("writing output")?;
            eprintln!(
                "{} page(s), {} bytes -> {}",
                compiled.pages,
                compiled.pdf.len(),
                args.output.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(CompileFailure::Diagnostics(diags)) => {
            print_diagnostics(&diags);
            Ok(ExitCode::FAILURE)
        }
        Err(CompileFailure::PackageNotAllowed(spec)) => bail!("package not allowed: {spec}"),
        Err(CompileFailure::TooManyPages { pages, max }) => {
            bail!("too many pages: {pages} > {max}")
        }
        Err(CompileFailure::InvalidOptions(msg)) => bail!("invalid options: {msg}"),
    }
}

fn print_diagnostics(diags: &[Diagnostic]) {
    for d in diags {
        let severity = serde_json::to_value(d.severity).unwrap();
        let location = match (&d.file, d.line, d.column) {
            (Some(f), Some(l), Some(c)) => format!("{f}:{l}:{c}: "),
            (Some(f), ..) => format!("{f}: "),
            _ => String::new(),
        };
        eprintln!(
            "{}: {location}{}",
            severity.as_str().unwrap_or("error"),
            d.message
        );
        for hint in &d.hints {
            eprintln!("  hint: {hint}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::parse_date;

    #[test]
    fn rejects_out_of_range_dates() {
        assert!(parse_date("2026-11-07").is_ok());
        for bad in [
            "2026-259-01",
            "2026-01-257",
            "2026-13-01",
            "2026-02-30",
            "2026-1",
        ] {
            assert!(parse_date(bad).is_err(), "{bad}");
        }
    }
}
