//! Attachments and Factur-X metadata.

use std::process::Command;
use std::sync::LazyLock;

use pdfq_core::{
    CompileFailure, CompileOptions, Compiled, Fonts, Packages, PdfqWorld, Project, WorldOptions,
    compile, parse_pdf_standard,
};
use pdfq_protocol::{Attachment, FacturxProfile, Relationship};

static FONTS: LazyLock<Fonts> = LazyLock::new(Fonts::embedded);

fn attachment(name: &str, data: &str, relationship: Relationship) -> Attachment {
    Attachment {
        name: name.into(),
        data: data.as_bytes().to_vec(),
        mime_type: "text/plain".into(),
        relationship,
        description: format!("The \"{name}\" file \\ with escapes\n"),
    }
}

fn run(
    files: &[(&str, &str)],
    entrypoint: &str,
    attachments: Vec<Attachment>,
    standards: &[&str],
    facturx: Option<FacturxProfile>,
) -> Result<Compiled, CompileFailure> {
    let mut project = Project::default();
    for (path, text) in files {
        project.add_file(path, text.as_bytes().to_vec()).unwrap();
    }
    let packages = Packages::default();
    let world = WorldOptions {
        entrypoint: entrypoint.into(),
        attachments,
        ..WorldOptions::default()
    };
    let world = PdfqWorld::new(project, &FONTS, &packages, world).unwrap();
    let options = CompileOptions {
        pdf_standards: standards
            .iter()
            .map(|s| parse_pdf_standard(s).unwrap())
            .collect(),
        facturx,
        ..CompileOptions::default()
    };
    compile(&world, &options)
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

/// Validates with veraPDF when installed (required when `PDFQ_REQUIRE_VERAPDF=1`).
fn verapdf(pdf: &[u8], flavour: &str) {
    let required = std::env::var("PDFQ_REQUIRE_VERAPDF").is_ok_and(|v| v == "1");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("out.pdf");
    std::fs::write(&path, pdf).unwrap();
    let output = match Command::new("verapdf")
        .args(["--flavour", flavour, "--format", "text"])
        .arg(&path)
        .output()
    {
        Ok(output) => output,
        Err(_) if !required => {
            eprintln!("veraPDF not installed, skipping validation");
            return;
        }
        Err(e) => panic!("veraPDF is required: {e}"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.starts_with("PASS"), "veraPDF: {stdout}");
}

#[test]
fn attaches_files_with_root_relative_names() {
    let main = "#let pdf = none\n#let read = none\nHello // trailing comment";
    let out = run(
        &[("src/main.typ", main)],
        "src/main.typ",
        vec![
            attachment("data.csv", "a,b", Relationship::Data),
            attachment("notes.txt", "hi", Relationship::Unspecified),
        ],
        &["a-3b"],
        None,
    )
    .unwrap();
    assert!(
        contains(&out.pdf, "(data.csv)"),
        "attachment named by its root path"
    );
    assert!(contains(&out.pdf, "(notes.txt)"));
    assert!(!contains(&out.pdf, "src/data.csv"));
    assert!(
        contains(&out.pdf, "/AFRelationship /Data") || contains(&out.pdf, "/AFRelationship/Data")
    );
    verapdf(&out.pdf, "3b");
}

#[test]
fn reserved_files_cannot_be_shadowed() {
    let main = "#assert.eq(read(\"/.pdfq/attachments/0\"), \"GOOD\")";
    run(
        &[("main.typ", main), (".pdfq/attachments/0", "EVIL")],
        "main.typ",
        vec![attachment("a.txt", "GOOD", Relationship::Supplement)],
        &[],
        None,
    )
    .unwrap();
}

#[test]
fn standards_without_attachments_report_an_error() {
    let result = run(
        &[("main.typ", "Hello")],
        "main.typ",
        vec![attachment("a.txt", "x", Relationship::Supplement)],
        &["a-2b"],
        None,
    );
    let Err(CompileFailure::Diagnostics(d)) = result else {
        panic!("expected diagnostics, got {result:?}")
    };
    assert!(d[0].message.contains("attached file"), "{d:?}");
}

#[test]
fn writes_facturx_metadata() {
    let xml = "<?xml version=\"1.0\"?><rsm:CrossIndustryInvoice/>";
    let out = run(
        &[(
            "main.typ",
            "#set document(title: \"Invoice endobj endstream\")\nInvoice",
        )],
        "main.typ",
        vec![Attachment {
            name: "factur-x.xml".into(),
            data: xml.as_bytes().to_vec(),
            mime_type: "text/xml".into(),
            relationship: Relationship::Alternative,
            description: "Factur-X invoice".into(),
        }],
        &["a-3b"],
        Some(FacturxProfile::En16931),
    )
    .unwrap();
    assert!(contains(
        &out.pdf,
        "<fx:ConformanceLevel>EN 16931</fx:ConformanceLevel>"
    ));
    assert!(contains(
        &out.pdf,
        "<pdfaSchema:prefix>fx</pdfaSchema:prefix>"
    ));
    check_xref(&out.pdf);
    verapdf(&out.pdf, "3b");
}

#[test]
fn facturx_requires_pdf_a3() {
    let result = run(
        &[("main.typ", "x")],
        "main.typ",
        vec![],
        &[],
        Some(FacturxProfile::Basic),
    );
    assert!(
        matches!(result, Err(CompileFailure::InvalidOptions(_))),
        "{result:?}"
    );
}

/// Every in-use xref entry points at its object, and startxref at the table.
fn check_xref(pdf: &[u8]) {
    let text = String::from_utf8_lossy(pdf);
    let startxref: usize = text
        .rsplit("startxref")
        .next()
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    assert!(
        pdf[startxref..].starts_with(b"xref"),
        "startxref points at the table"
    );
    // Byte offsets: decode from the table on (binary streams come before it).
    let table = String::from_utf8_lossy(&pdf[startxref..]);
    let mut lines = table.lines().skip(1);
    let count: usize = lines
        .next()
        .unwrap()
        .split(' ')
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    for (i, line) in lines.take(count).enumerate() {
        if line.ends_with('n') || line.trim_end().ends_with('n') {
            let offset: usize = line[..10].parse().unwrap();
            assert!(
                pdf[offset..].starts_with(format!("{i} 0 obj").as_bytes()),
                "object {i} at {offset}"
            );
        }
    }
}

#[test]
fn the_document_cannot_reach_the_attach_calls() {
    // Show rules discarding or repeating the body, an unclosed trailing comment.
    for main in [
        "#show: _ => [Replaced]\nHello",
        "#show: body => { body; body }\nHello",
        "Hello /* unclosed",
    ] {
        let out = run(
            &[("main.typ", main)],
            "main.typ",
            vec![attachment("a.txt", "x", Relationship::Supplement)],
            &["a-3b"],
            None,
        )
        .unwrap_or_else(|e| panic!("{main}: {e:?}"));
        assert!(contains(&out.pdf, "(a.txt)"), "{main}");
    }
}

#[test]
fn set_rules_of_the_document_still_apply() {
    let main = "#set document(title: \"Wrapped\")\n#set page(width: 10cm, height: 10cm)\nHi";
    let out = run(
        &[("main.typ", main)],
        "main.typ",
        vec![attachment("a.txt", "x", Relationship::Supplement)],
        &["a-3b"],
        None,
    )
    .unwrap();
    assert!(contains(&out.pdf, "Wrapped"));
    assert!(contains(&out.pdf, "283.46"), "page set rule applied");
}

#[test]
fn attachments_do_not_add_pages() {
    let main = "Hello\n#pagebreak()\nWorld\n#pagebreak()";
    let pages = |attachments| {
        run(&[("main.typ", main)], "main.typ", attachments, &[], None)
            .unwrap()
            .pages
    };
    assert_eq!(
        pages(vec![attachment("a.txt", "x", Relationship::Supplement)]),
        pages(vec![])
    );
}

#[test]
fn pdf_a4f_allows_attachments() {
    run(
        &[("main.typ", "Hello")],
        "main.typ",
        vec![attachment("a.txt", "x", Relationship::Supplement)],
        &["a-4f"],
        None,
    )
    .unwrap();
}
