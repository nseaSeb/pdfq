use std::fs;
use std::sync::LazyLock;

use pdfq_core::{
    CompileFailure, CompileOptions, Compiled, Fonts, PackageAllowlist, Packages, PdfqWorld,
    Project, WorldOptions, compile, parse_pdf_standard,
};
use typst::foundations::Datetime;

static FONTS: LazyLock<Fonts> = LazyLock::new(Fonts::embedded);

fn project(files: &[(&str, &str)]) -> Project {
    let mut p = Project::default();
    for (path, text) in files {
        p.add_file(path, text.as_bytes().to_vec()).unwrap();
    }
    p
}

fn run_with(
    project: Project,
    packages: &Packages,
    world: WorldOptions,
    options: CompileOptions,
) -> Result<Compiled, CompileFailure> {
    let world = PdfqWorld::new(project, &FONTS, packages, world).unwrap();
    compile(&world, &options)
}

fn run(main: &str) -> Result<Compiled, CompileFailure> {
    run_with(
        project(&[("main.typ", main)]),
        &Packages::default(),
        WorldOptions::default(),
        CompileOptions::default(),
    )
}

fn diagnostics(result: Result<Compiled, CompileFailure>) -> Vec<pdfq_core::Diagnostic> {
    match result {
        Err(CompileFailure::Diagnostics(d)) => d,
        other => panic!("expected diagnostics, got {other:?}"),
    }
}

#[test]
fn compiles_with_sys_inputs_data() {
    let world = WorldOptions {
        inputs: vec![("data".into(), r#"{"client":"ACME"}"#.into())],
        ..WorldOptions::default()
    };
    let main = r#"#let data = json(bytes(sys.inputs.data))
#assert.eq(data.client, "ACME")
Client: #data.client"#;
    let out = run_with(
        project(&[("main.typ", main)]),
        &Packages::default(),
        world,
        CompileOptions::default(),
    )
    .unwrap();
    assert!(out.pdf.starts_with(b"%PDF-"));
    assert_eq!(out.pages, 1);
    assert!(out.warnings.is_empty());
}

#[test]
fn reports_syntax_errors_with_position() {
    let diags = diagnostics(run("= Title\n#let x = (1, 2\n"));
    let first = &diags[0];
    assert_eq!(first.message, "unclosed delimiter");
    assert_eq!(first.file.as_deref(), Some("main.typ"));
    assert_eq!((first.line, first.column), (Some(2), Some(10)));
}

#[test]
fn reads_project_files_and_images() {
    let png = include_bytes!("fixtures/pixel.png");
    let mut p = project(&[
        (
            "main.typ",
            "#import \"lib/util.typ\": greet\n#greet()\n#image(\"img/pixel.png\")",
        ),
        ("lib/util.typ", "#let greet() = [Hello]"),
    ]);
    p.add_file("img/pixel.png", png.to_vec()).unwrap();
    run_with(
        p,
        &Packages::default(),
        WorldOptions::default(),
        CompileOptions::default(),
    )
    .unwrap();
}

#[test]
fn refuses_paths_escaping_the_project() {
    let diags = diagnostics(run("#read(\"../../../etc/passwd\")"));
    assert!(diags[0].message.contains("escape"), "{diags:?}");
    assert!(diags[0].hints.iter().all(|h| !h.contains("--root")));
}

#[test]
fn refuses_packages_outside_the_allowlist() {
    let only_none = Packages::new::<&str>(&[], PackageAllowlist::Only(Default::default()));
    // Default allowlist: what is installed. Nothing is installed here.
    for packages in [&only_none, &Packages::default()] {
        assert_package_denied(packages);
    }
}

fn assert_package_denied(packages: &Packages) {
    for import in ["@preview/cetz:0.4.0", "@local/mine:0.1.0"] {
        let main = format!("#import \"{import}\": *");
        let result = run_with(
            project(&[("main.typ", &main)]),
            packages,
            WorldOptions::default(),
            CompileOptions::default(),
        );
        assert!(matches!(result, Err(CompileFailure::PackageNotAllowed(ref s)) if s == import));
    }
}

#[test]
fn loads_installed_packages() {
    let dir = tempfile::tempdir().unwrap();
    let pkg = dir.path().join("preview/hello/0.1.0");
    fs::create_dir_all(&pkg).unwrap();
    fs::write(
        pkg.join("typst.toml"),
        "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nentrypoint = \"lib.typ\"\n",
    )
    .unwrap();
    fs::write(pkg.join("lib.typ"), "#let hi = [Hi from a package]").unwrap();
    let only = PackageAllowlist::Only(["@preview/hello:0.1.0".to_string()].into());
    let packages = Packages::new(&[dir.path()], only);
    let main = "#import \"@preview/hello:0.1.0\": hi\n#hi";
    run_with(
        project(&[("main.typ", main)]),
        &packages,
        WorldOptions::default(),
        CompileOptions::default(),
    )
    .unwrap();
}

#[test]
fn enforces_page_limit() {
    let options = CompileOptions {
        max_pages: Some(10),
        ..CompileOptions::default()
    };
    let result = run_with(
        project(&[("main.typ", "#for i in range(20) [#i #pagebreak()]")]),
        &Packages::default(),
        WorldOptions::default(),
        options,
    );
    assert!(matches!(
        result,
        Err(CompileFailure::TooManyPages { pages: 21, max: 10 })
    ));
}

#[test]
fn strict_mode_fails_on_unknown_font() {
    let main = "#set text(font: \"No Such Font\")\nHello";
    let lenient = run(main).unwrap();
    assert!(
        lenient
            .warnings
            .iter()
            .any(|w| w.message.contains("unknown font"))
    );

    let options = CompileOptions {
        strict: true,
        ..CompileOptions::default()
    };
    let result = run_with(
        project(&[("main.typ", main)]),
        &Packages::default(),
        WorldOptions::default(),
        options,
    );
    assert!(!diagnostics(result).is_empty());
}

#[test]
fn fixed_date_gives_reproducible_output() {
    let world = || WorldOptions {
        date: Datetime::from_ymd(2026, 3, 1),
        ..WorldOptions::default()
    };
    let main = "#datetime.today().display()";
    let compile_once = || {
        run_with(
            project(&[("main.typ", main)]),
            &Packages::default(),
            world(),
            CompileOptions::default(),
        )
        .unwrap()
        .pdf
    };
    assert_eq!(compile_once(), compile_once());
}

#[test]
fn supports_pdf_a() {
    assert!(parse_pdf_standard("nope").is_err());
    let options = CompileOptions {
        pdf_standards: vec![parse_pdf_standard("a-3b").unwrap()],
        ..CompileOptions::default()
    };
    let out = run_with(
        project(&[("main.typ", "#set document(title: [Invoice])\nHello")]),
        &Packages::default(),
        WorldOptions::default(),
        options,
    )
    .unwrap();
    assert!(String::from_utf8_lossy(&out.pdf).contains("pdfaid:part"));
}

#[test]
fn missing_entrypoint_is_rejected() {
    let world = WorldOptions {
        entrypoint: "nope.typ".into(),
        ..WorldOptions::default()
    };
    let p = project(&[("main.typ", "x")]);
    assert!(PdfqWorld::new(p, &FONTS, &Packages::default(), world).is_err());
}

#[test]
fn typst_version_constant_matches_the_pin() {
    let manifest = include_str!("../../../Cargo.toml");
    let pin = format!("typst = \"={}\"", pdfq_core::TYPST_VERSION);
    assert!(
        manifest.contains(&pin),
        "update TYPST_VERSION in pdfq-protocol"
    );
}
