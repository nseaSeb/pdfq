//! PDFQ worker: reads [`Job`] frames on stdin, compiles them one at a time and
//! writes [`WorkerMessage`] frames on stdout. Logs go to stderr.
//!
//! It is spawned and supervised by the PDFQ server, which applies hard rlimits
//! before exec, enforces a wall-clock timeout and recycles workers. A failed
//! allocation exits with [`EXIT_OOM`] so the supervisor can report `memory_limit`.

#![allow(unsafe_code)] // Global allocator and rlimits (see `limits`).

mod limits;

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, BufReader, BufWriter};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};

use clap::Parser;
use pdfq_core::{
    CompileFailure, CompileOptions, Fonts, PackageAllowlist, Packages, PdfqWorld, Project,
    WorldOptions, compile, parse_pdf_standard,
};
use pdfq_protocol::{
    EXIT_OOM, ErrorKind, Job, JobError, JobOutput, JobSource, PROTOCOL_VERSION, Ready,
    WorkerMessage, read_frame, write_frame,
};
use typst::foundations::Datetime;

/// Turns allocation failure (`RLIMIT_AS` reached) into a recognizable exit code.
struct OomExit;

// SAFETY: forwards to the system allocator; only adds a null check.
unsafe impl GlobalAlloc for OomExit {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        oom_check(unsafe { System.alloc(layout) })
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        oom_check(unsafe { System.alloc_zeroed(layout) })
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        oom_check(unsafe { System.realloc(ptr, layout, new_size) })
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

fn oom_check(ptr: *mut u8) -> *mut u8 {
    if ptr.is_null() {
        // SAFETY: `_exit` is async-signal-safe and does not allocate.
        unsafe { libc::_exit(EXIT_OOM) }
    }
    ptr
}

#[global_allocator]
static GLOBAL: OomExit = OomExit;

/// Stack of the compile thread: large enough for Typst's own depth limits to
/// trigger before a stack overflow.
const STACK_SIZE: usize = 64 * 1024 * 1024;
/// Upper bound for an incoming job frame (uploads are capped by the server).
const MAX_JOB_FRAME: usize = 1 << 30;

#[derive(Parser, Clone)]
#[command(version, about)]
struct Args {
    /// Font directory (repeatable).
    #[arg(long = "font-path")]
    font_paths: Vec<PathBuf>,
    /// Package directory laid out as <dir>/preview/<name>/<version> (repeatable).
    #[arg(long = "package-path")]
    package_paths: Vec<PathBuf>,
    /// Allowed package, like @preview/cetz:0.4.0 (repeatable). Default: any installed package.
    #[arg(long = "allow-package")]
    allowed_packages: Vec<String>,
    /// Directory holding one sub-directory per template.
    #[arg(long)]
    template_dir: Option<PathBuf>,
    /// Memory budget per job in bytes, above the worker's baseline (0 = no limit).
    #[arg(long, default_value_t = 0)]
    memory_budget: u64,
    /// CPU budget per job in seconds (0 = no limit).
    #[arg(long, default_value_t = 0)]
    cpu_budget: u64,
}

fn main() {
    let args = Args::parse();
    let handle = std::thread::Builder::new()
        .name("pdfq-worker".into())
        .stack_size(STACK_SIZE)
        .spawn(move || run(&args))
        .expect("failed to spawn the worker thread");
    let code = handle.join().unwrap_or(1);
    std::process::exit(code);
}

fn run(args: &Args) -> i32 {
    let fonts = Fonts::load(&args.font_paths);
    fonts.preload();
    let allowlist = if args.allowed_packages.is_empty() {
        PackageAllowlist::Installed
    } else {
        PackageAllowlist::Only(args.allowed_packages.iter().cloned().collect())
    };
    let packages = Packages::new(&args.package_paths, allowlist);

    let mut input = BufReader::new(io::stdin().lock());
    let mut output = BufWriter::new(io::stdout().lock());
    let ready = Ready {
        protocol_version: PROTOCOL_VERSION,
        typst_version: pdfq_core::TYPST_VERSION.into(),
        font_families: fonts.family_count() as u32,
        baseline_memory: limits::address_space().unwrap_or(0),
    };
    if write_frame(&mut output, &WorkerMessage::Ready(ready)).is_err() {
        return 1;
    }

    loop {
        let job: Job = match read_frame(&mut input, MAX_JOB_FRAME) {
            Ok(Some(job)) => job,
            Ok(None) => return 0, // Supervisor closed stdin: recycle.
            Err(err) => {
                eprintln!("pdfq-worker: invalid job frame: {err}");
                return 1;
            }
        };

        limits::arm(args.memory_budget, args.cpu_budget);
        let outcome = catch_unwind(AssertUnwindSafe(|| execute(job, args, &fonts, &packages)));
        // The memory budget bounds compilation, not the encoding of the response.
        limits::disarm_memory();
        let (message, healthy) = match outcome {
            Ok(message) => (message, true),
            Err(_) => (
                WorkerMessage::Failed(JobError::new(
                    ErrorKind::WorkerCrashed,
                    "the compiler panicked",
                )),
                false,
            ),
        };
        let sent = write_frame(&mut output, &message);
        drop(message);
        pdfq_core::evict_cache();
        if sent.is_err() || !healthy {
            return 1;
        }
    }
}

fn execute(job: Job, args: &Args, fonts: &Fonts, packages: &Packages) -> WorkerMessage {
    match try_execute(job, args, fonts, packages) {
        Ok(output) => WorkerMessage::Done(output),
        Err(err) => WorkerMessage::Failed(err),
    }
}

fn invalid(message: impl Into<String>) -> JobError {
    JobError::new(ErrorKind::InvalidInput, message)
}

fn try_execute(
    job: Job,
    args: &Args,
    fonts: &Fonts,
    packages: &Packages,
) -> Result<JobOutput, JobError> {
    let (project, entrypoint) = match job.source {
        JobSource::Files { files, entrypoint } => {
            let mut project = Project::default();
            for file in files {
                project
                    .add_file(&file.path, file.data)
                    .map_err(|e| invalid(e.to_string()))?;
            }
            (project, entrypoint)
        }
        JobSource::Template { name, assets } => {
            let mut project = open_template(args.template_dir.as_deref(), &name)?;
            for file in assets {
                // Assets must stay under /assets: no `..`, no absolute paths.
                if file.path.starts_with('/') || file.path.split(['/', '\\']).any(|s| s == "..") {
                    return Err(invalid(format!("invalid asset path `{}`", file.path)));
                }
                project
                    .add_file(&format!("assets/{}", file.path), file.data)
                    .map_err(|e| invalid(e.to_string()))?;
            }
            (project, "main.typ".to_string())
        }
    };

    let options = job.options;
    let date = match options.date {
        Some(d) => Some(
            Datetime::from_ymd(d.year, d.month, d.day).ok_or_else(|| invalid("invalid date"))?,
        ),
        None => None,
    };
    let world_options = WorldOptions {
        entrypoint: entrypoint.clone(),
        inputs: job.inputs,
        date,
        max_file_size: limit(options.max_file_size).unwrap_or(u64::MAX),
    };
    let world = PdfqWorld::new(project, fonts, packages, world_options)
        .map_err(|_| invalid(format!("entrypoint `{entrypoint}` not found")))?;

    let pdf_standards = options
        .pdf_standards
        .iter()
        .map(|s| parse_pdf_standard(s))
        .collect::<Result<Vec<_>, _>>()
        .map_err(invalid)?;
    let compile_options = CompileOptions {
        max_pages: limit(u64::from(options.max_pages)).map(|n| n as usize),
        pdf_standards,
        strict: options.strict,
    };

    let compiled = compile(&world, &compile_options).map_err(|failure| match failure {
        CompileFailure::Diagnostics(diagnostics) => JobError {
            diagnostics,
            ..JobError::new(ErrorKind::CompileError, "compilation failed")
        },
        CompileFailure::PackageNotAllowed(spec) => JobError::new(
            ErrorKind::PackageNotAllowed,
            format!("package {spec} is not available on this server"),
        ),
        CompileFailure::TooManyPages { pages, max } => JobError::new(
            ErrorKind::TooManyPages,
            format!("document has {pages} pages, the limit is {max}"),
        ),
        CompileFailure::InvalidOptions(message) => invalid(message),
    })?;

    if options.max_output_size > 0 && compiled.pdf.len() as u64 > options.max_output_size {
        return Err(JobError::new(
            ErrorKind::OutputTooLarge,
            format!(
                "PDF is {} bytes, the limit is {} bytes",
                compiled.pdf.len(),
                options.max_output_size
            ),
        ));
    }
    Ok(JobOutput {
        pdf: compiled.pdf,
        pages: compiled.pages as u32,
        warnings: compiled.warnings,
    })
}

/// Protocol limits use 0 for "no limit".
fn limit(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

fn open_template(dir: Option<&Path>, name: &str) -> Result<Project, JobError> {
    let valid = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !valid {
        return Err(invalid(format!("invalid template name `{name}`")));
    }
    let not_found = || JobError::new(ErrorKind::NotFound, format!("template `{name}` not found"));
    let dir = dir.ok_or_else(not_found)?;
    Project::from_dir(dir.join(name)).map_err(|_| not_found())
}
