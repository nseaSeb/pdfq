//! Prototype 1: compile a single Typst file to PDF with typst 0.15.1.
//! Usage: p1-compile <input.typ> [-o out.pdf] [--data data.json]

use std::path::PathBuf;

use typst::diag::{FileError, FileResult, Severity, SourceDiagnostic};
use typst::foundations::{Bytes, Datetime, Dict, IntoValue};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World, WorldExt};
use typst_kit::fonts::FontStore;
use typst_layout::PagedDocument;

struct ProtoWorld {
    library: LazyHash<Library>,
    fonts: FontStore,
    main: Source,
}

impl World for ProtoWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }
    fn main(&self) -> FileId {
        self.main.id()
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() {
            Ok(self.main.clone())
        } else {
            Err(FileError::AccessDenied)
        }
    }
    fn file(&self, _id: FileId) -> FileResult<Bytes> {
        Err(FileError::AccessDenied)
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }
    fn today(&self, _offset: Option<typst::foundations::Duration>) -> Option<Datetime> {
        Datetime::from_ymd(2026, 1, 1)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input = None;
    let mut output = PathBuf::from("out.pdf");
    let mut data = None;
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => output = it.next().expect("-o needs a path").into(),
            "--data" => data = Some(std::fs::read_to_string(it.next().expect("--data needs a path")).unwrap()),
            _ => input = Some(PathBuf::from(a)),
        }
    }
    let input = input.expect("usage: p1-compile <input.typ> [-o out.pdf] [--data data.json]");
    let text = std::fs::read_to_string(&input).unwrap();

    let mut inputs = Dict::new();
    if let Some(d) = data {
        inputs.insert("data".into(), d.into_value());
    }

    let mut fonts = FontStore::new();
    fonts.extend(typst_kit::fonts::embedded());

    let name = input.file_name().unwrap().to_string_lossy();
    let id = RootedPath::new(VirtualRoot::Project, VirtualPath::new(&*name).unwrap()).intern();
    let world = ProtoWorld {
        library: LazyHash::new(Library::builder().with_inputs(inputs).build()),
        fonts,
        main: Source::new(id, text),
    };

    let result = typst::compile::<PagedDocument>(&world);
    print_diags(&world, &result.warnings);
    let doc = match result.output {
        Ok(doc) => doc,
        Err(errors) => {
            print_diags(&world, &errors);
            std::process::exit(1);
        }
    };
    let pdf = typst_pdf::pdf(&doc, &typst_pdf::PdfOptions::default()).unwrap_or_else(|errors| {
        print_diags(&world, &errors);
        std::process::exit(1);
    });
    std::fs::write(&output, &pdf).unwrap();
    eprintln!("ok: {} pages, {} bytes -> {}", doc.pages().len(), pdf.len(), output.display());
}

fn print_diags(world: &ProtoWorld, diags: &[SourceDiagnostic]) {
    for d in diags {
        let sev = match d.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        let loc = d
            .span
            .id()
            .and_then(|id| {
                let src = world.source(id).ok()?;
                let range = world.range(d.span)?;
                let (line, col) = src.lines().byte_to_line_column(range.start)?;
                Some(format!("{:?}:{}:{}", id.get().vpath(), line + 1, col + 1))
            })
            .unwrap_or_else(|| "<unknown>".into());
        eprintln!("{sev}: {loc}: {}", d.message);
        for h in &d.hints {
            eprintln!("  hint: {}", h.v);
        }
    }
}
