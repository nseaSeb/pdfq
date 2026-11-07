//! HTTP API tests: requests go through the router to real worker processes.

mod common;

use std::sync::OnceLock;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use pdfq_server::config::Config;
use serde_json::{Value, json};
use tower::ServiceExt;

/// Template directory shared by the tests.
fn templates() -> &'static std::path::Path {
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap();
        let invoice = dir.path().join("invoice");
        std::fs::create_dir(&invoice).unwrap();
        std::fs::write(
            invoice.join("main.typ"),
            "#let data = json(bytes(sys.inputs.at(\"data\", default: \"{}\")))\n\
             Invoice #data.number for #data.client\n\
             #if \"logo\" in data { image(data.logo, width: 1cm) }",
        )
        .unwrap();
        std::fs::write(
            invoice.join("schema.json"),
            r#"{"type":"object","required":["number","client"],
                "properties":{"number":{"type":"string"},"client":{"type":"string"}}}"#,
        )
        .unwrap();
        dir
    })
    .path()
}

fn app_with(config: Config) -> Router {
    let state = pdfq_server::start(config, common::worker_bin());
    pdfq_server::api::router(state)
}

fn config() -> Config {
    Config {
        workers: Some(2),
        template_dir: Some(templates().to_path_buf()),
        wall_timeout: Duration::from_secs(10),
        cpu_timeout: Duration::from_secs(5),
        ..Config::defaults()
    }
}

fn app() -> Router {
    app_with(config())
}

/// A multipart part: (field name, optional filename, content).
type Part<'a> = (&'a str, Option<&'a str>, &'a [u8]);

fn multipart(parts: &[Part]) -> Request<Body> {
    let boundary = "pdfq-test-boundary";
    let mut body = Vec::new();
    for (name, filename, data) in parts {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        let disposition = match filename {
            Some(f) => format!("form-data; name=\"{name}\"; filename=\"{f}\""),
            None => format!("form-data; name=\"{name}\""),
        };
        body.extend_from_slice(format!("Content-Disposition: {disposition}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::post("/compile")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

fn json_request(body: Value) -> Request<Body> {
    Request::post("/compile")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn send(app: Router, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, headers, body)
}

/// Sends a request expected to fail and returns the error object.
async fn error(app: Router, request: Request<Body>, status: StatusCode, kind: &str) -> Value {
    let (actual, _, body) = send(app, request).await;
    let body: Value = serde_json::from_slice(&body)
        .unwrap_or_else(|_| panic!("non-JSON error body: {}", String::from_utf8_lossy(&body)));
    assert_eq!(actual, status, "{body}");
    assert_eq!(body["error"]["kind"], kind, "{body}");
    body["error"].clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn compiles_uploaded_files() {
    let (status, headers, body) = send(
        app(),
        multipart(&[
            (
                "file",
                Some("main.typ"),
                b"#import \"lib/util.typ\": hi\n#hi #sys.inputs.who",
            ),
            ("file", Some("lib/util.typ"), b"#let hi = [Hello]"),
            ("input.who", None, b"world"),
            ("date", None, b"2026-01-02"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers["content-type"], "application/pdf");
    assert_eq!(headers["x-pdfq-pages"], "1");
    assert!(body.starts_with(b"%PDF-"));
}

#[tokio::test(flavor = "multi_thread")]
async fn compile_errors_carry_diagnostics() {
    let err = error(
        app(),
        multipart(&[("file", Some("main.typ"), b"= Title\n#let x = (1, 2\n")]),
        StatusCode::UNPROCESSABLE_ENTITY,
        "compile_error",
    )
    .await;
    let diag = &err["diagnostics"][0];
    assert_eq!(diag["message"], "unclosed delimiter");
    assert_eq!(
        (diag["file"].as_str(), diag["line"].as_u64()),
        (Some("main.typ"), Some(2))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn compiles_a_template_from_json() {
    let (status, _, body) = send(
        app(),
        json_request(json!({
            "template": "invoice",
            "data": { "number": "F-1", "client": "ACME" },
            "pdf_standard": "a-2b",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(String::from_utf8_lossy(&body).contains("pdfaid:part"));
}

#[tokio::test(flavor = "multi_thread")]
async fn template_assets_are_mounted_under_assets() {
    let png =
        std::fs::read(common::workspace().join("crates/core/tests/fixtures/pixel.png")).unwrap();
    let data = br#"{"number":"F-2","client":"ACME","logo":"/assets/logo.png"}"#;
    let (status, _, body) = send(
        app(),
        multipart(&[
            ("template", None, b"invoice"),
            ("data", None, data),
            ("asset", Some("logo.png"), &png),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test(flavor = "multi_thread")]
async fn data_is_checked_against_the_template_schema() {
    let err = error(
        app(),
        json_request(json!({ "template": "invoice", "data": { "number": 42 } })),
        StatusCode::BAD_REQUEST,
        "schema_violation",
    )
    .await;
    let messages = err["diagnostics"].to_string();
    assert!(
        messages.contains("/number") && messages.contains("client"),
        "{messages}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_template_is_not_found() {
    error(
        app(),
        json_request(json!({ "template": "nope" })),
        StatusCode::NOT_FOUND,
        "not_found",
    )
    .await;
    error(
        app(),
        json_request(json!({ "template": "../invoice" })),
        StatusCode::BAD_REQUEST,
        "invalid_input",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn compiles_a_zip_archive() {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("doc/main.typ", options).unwrap();
    std::io::Write::write_all(&mut zip, b"#include \"part.typ\"").unwrap();
    zip.start_file("doc/part.typ", options).unwrap();
    std::io::Write::write_all(&mut zip, b"From a zip").unwrap();
    let archive = zip.finish().unwrap().into_inner();
    let (status, _, body) = send(
        app(),
        multipart(&[
            ("archive", Some("doc.zip"), &archive),
            ("entrypoint", None, b"doc/main.typ"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_invalid_inputs() {
    let cases: Vec<(Vec<Part>, StatusCode, &str)> = vec![
        (
            vec![("file", Some("../main.typ"), b"x")],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![("file", Some("other.typ"), b"x")],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![("bogus", None, b"x")],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![("attachment", Some("a.json"), b"{}")],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![
                ("file", Some("main.typ"), b"x"),
                ("data", None, b"{not json"),
            ],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![
                ("file", Some("main.typ"), b"x"),
                ("date", None, b"2026-13-01"),
            ],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![
                ("file", Some("main.typ"), b"x"),
                ("template", None, b"invoice"),
            ],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (vec![], StatusCode::BAD_REQUEST, "invalid_input"),
        (
            vec![
                ("file", Some("main.typ"), b"x"),
                ("date", None, b"+999-01-01"),
            ],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![
                ("file", Some("main.typ"), b"x"),
                ("date", None, b"2026-01-01"),
                ("date", None, b"2026-01-02"),
            ],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![
                ("file", Some("main.typ"), b"x"),
                ("input.a", None, b"1"),
                ("input.a", None, b"2"),
            ],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
        (
            vec![
                ("file", Some("main.typ"), b"x"),
                ("archive", Some("a.zip"), b"PK"),
            ],
            StatusCode::BAD_REQUEST,
            "invalid_input",
        ),
    ];
    for (parts, status, kind) in cases {
        error(app(), multipart(&parts), status, kind).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn enforces_size_and_count_limits() {
    let small = Config {
        max_file_size: 100,
        max_files: 2,
        ..config()
    };
    let big = vec![b'x'; 200];
    error(
        app_with(small.clone()),
        multipart(&[("file", Some("main.typ"), &big)]),
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;
    error(
        app_with(small),
        multipart(&[
            ("file", Some("main.typ"), b"x"),
            ("file", Some("a.typ"), b"x"),
            ("file", Some("b.typ"), b"x"),
        ]),
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn image_bombs_are_refused_before_compiling() {
    let png =
        std::fs::read(common::workspace().join("tests/fixtures/hostile/image-bomb.png")).unwrap();
    let err = error(
        app(),
        multipart(&[
            ("file", Some("main.typ"), b"#image(\"bomb.png\")"),
            ("file", Some("bomb.png"), &png),
        ]),
        StatusCode::BAD_REQUEST,
        "invalid_input",
    )
    .await;
    assert!(err["message"].as_str().unwrap().contains("pixels"));
}

#[tokio::test(flavor = "multi_thread")]
async fn hostile_documents_get_typed_errors() {
    let app = app_with(Config {
        cpu_timeout: Duration::from_secs(2),
        ..config()
    });
    let infinite =
        std::fs::read(common::workspace().join("tests/fixtures/hostile/infinite-loop.typ"))
            .unwrap();
    let err = error(
        app.clone(),
        multipart(&[("file", Some("main.typ"), &infinite)]),
        StatusCode::UNPROCESSABLE_ENTITY,
        "timeout",
    )
    .await;
    assert_eq!(err["detail"], "cpu");
    let (status, _, _) = send(app, multipart(&[("file", Some("main.typ"), b"ok")])).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn requests_beyond_capacity_are_refused_before_reading_them() {
    let state = pdfq_server::start(
        Config {
            workers: Some(1),
            queue_size: 1,
            ..config()
        },
        common::worker_bin(),
    );
    // Capacity is workers + queue_size = 2: hold both slots.
    let _held = (state.admit().unwrap(), state.admit().unwrap());
    let app = pdfq_server::api::router(state);
    // An invalid body would be a 400 if it were read: 429 proves it was not.
    let request = Request::post("/compile")
        .header("content-type", "application/json")
        .body(Body::from("not json"))
        .unwrap();
    let (status, headers, _) = send(app, request).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(headers["retry-after"], "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn health_and_readiness() {
    let app = app();
    wait_ready(&app).await;
    let (status, _, body) = send(
        app.clone(),
        Request::get("/health").body(Body::empty()).unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["typst_version"], "0.15.1");

    // No worker can start: unhealthy.
    let broken = pdfq_server::start(config(), "/nonexistent/pdfq-worker".into());
    let request = Request::get("/health").body(Body::empty()).unwrap();
    let (status, _, _) = send(pdfq_server::api::router(broken), request).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

/// Waits until a worker is up (workers start asynchronously).
async fn wait_ready(app: &Router) {
    for _ in 0..200 {
        let request = Request::get("/ready").body(Body::empty()).unwrap();
        if send(app.clone(), request).await.0 == StatusCode::OK {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("never ready");
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

const CII: &[u8] = b"<?xml version=\"1.0\"?><rsm:CrossIndustryInvoice/>";

#[tokio::test(flavor = "multi_thread")]
async fn embeds_attachments_and_data() {
    let (status, _, body) = send(
        app(),
        multipart(&[
            ("file", Some("main.typ"), b"Hello"),
            ("attachment", Some("lines.csv"), b"a,b\n1,2"),
            ("attachment_relationship.lines.csv", None, b"data"),
            ("data", None, br#"{"n":1}"#),
            ("attach_data", None, b"true"),
            ("pdf_standard", None, b"a-3b"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(contains(&body, "(lines.csv)"));
    assert!(contains(&body, "(data.json)"));
}

#[tokio::test(flavor = "multi_thread")]
async fn writes_facturx() {
    let (status, _, body) = send(
        app(),
        multipart(&[
            ("file", Some("main.typ"), b"Invoice"),
            ("facturx", Some("invoice.xml"), CII),
            ("facturx_profile", None, b"en16931"),
        ]),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(contains(&body, "(factur-x.xml)"));
    assert!(
        contains(&body, "<pdfaid:part>3</pdfaid:part>"),
        "a-3b is implied"
    );
    assert!(contains(
        &body,
        "<fx:ConformanceLevel>EN 16931</fx:ConformanceLevel>"
    ));

    // JSON template mode takes the XML as a string.
    let (status, _, body) = send(
        app(),
        json_request(json!({
            "template": "invoice",
            "data": { "number": "F-1", "client": "ACME" },
            "facturx": String::from_utf8_lossy(CII),
            "facturx_profile": "minimum",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert!(contains(
        &body,
        "<fx:ConformanceLevel>MINIMUM</fx:ConformanceLevel>"
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_invalid_attachments() {
    let main: Part = ("file", Some("main.typ"), b"x");
    let profile: Part = ("facturx_profile", None, b"basic");
    let facturx: Part = ("facturx", Some("f.xml"), CII);
    let cases: Vec<Vec<Part>> = vec![
        vec![main, ("attachment", Some(".hidden"), b"x")],
        vec![main, ("attachment", Some("data.json"), b"{}")],
        vec![
            main,
            ("attachment", Some("a.txt"), b"x"),
            ("attachment", Some("a.txt"), b"y"),
        ],
        vec![main, ("attachment_relationship.nope.txt", None, b"data")],
        vec![
            main,
            ("attachment", Some("a.txt"), b"x"),
            ("attachment_relationship.a.txt", None, b"bogus"),
        ],
        vec![main, ("attach_data", None, b"true")],
        vec![main, facturx],
        vec![main, profile],
        vec![main, facturx, ("facturx_profile", None, b"gold")],
        vec![main, ("facturx", Some("f.xml"), b"<Invoice/>"), profile],
        vec![main, facturx, profile, ("pdf_standard", None, b"a-2b")],
        vec![
            main,
            facturx,
            ("facturx_profile", None, b"minimum"),
            ("attachment_relationship.factur-x.xml", None, b"alternative"),
        ],
        vec![main, ("file", Some(".pdfq/attachments/0"), b"x")],
    ];
    for parts in cases {
        let names: Vec<_> = parts.iter().map(|p| p.0).collect();
        let (status, _, body) = send(app(), multipart(&parts)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{names:?}: {}",
            String::from_utf8_lossy(&body)
        );
    }

    // Standards that forbid attachments are reported by the compiler.
    error(
        app(),
        multipart(&[
            main,
            ("attachment", Some("a.txt"), b"x"),
            ("pdf_standard", None, b"a-2b"),
        ]),
        StatusCode::UNPROCESSABLE_ENTITY,
        "compile_error",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn archive_shares_the_upload_budget() {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    zip.start_file("main.typ", zip::write::SimpleFileOptions::default())
        .unwrap();
    std::io::Write::write_all(&mut zip, &[b'a'; 600]).unwrap();
    let archive = zip.finish().unwrap().into_inner();
    let app = app_with(Config {
        max_upload_size: 1000,
        ..config()
    });
    error(
        app,
        multipart(&[
            ("attachment", Some("a.txt"), &[b'x'; 600]),
            ("archive", Some("p.zip"), &archive),
        ]),
        StatusCode::PAYLOAD_TOO_LARGE,
        "payload_too_large",
    )
    .await;
}

/// Parses the exposition format into (series, value), checking every line.
fn parse_metrics(text: &str) -> Vec<(String, f64)> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let (series, value) = l.rsplit_once(' ').unwrap_or_else(|| panic!("bad line {l}"));
            (
                series.to_string(),
                value.parse().unwrap_or_else(|_| panic!("bad value {l}")),
            )
        })
        .collect()
}

fn metric(metrics: &[(String, f64)], series: &str) -> f64 {
    metrics
        .iter()
        .find(|(s, _)| s == series)
        .unwrap_or_else(|| panic!("no {series}"))
        .1
}

#[tokio::test(flavor = "multi_thread")]
async fn exposes_prometheus_metrics() {
    let state = pdfq_server::start(config(), common::worker_bin());
    let app = pdfq_server::api::router(state.clone());
    let ok = multipart(&[("file", Some("main.typ"), b"Hello")]);
    assert_eq!(send(app.clone(), ok).await.0, StatusCode::OK);
    let broken = multipart(&[("file", Some("main.typ"), b"#let x = (")]);
    assert_eq!(
        send(app.clone(), broken).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );

    // Scraping bypasses admission: hold every slot first.
    let capacity = 2 + config().queue_size;
    let _held: Vec<_> = (0..capacity).map(|_| state.admit().unwrap()).collect();
    assert!(state.admit().is_err());

    let (status, headers, body) =
        send(app, Request::get("/metrics").body(Body::empty()).unwrap()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain; version=0.0.4")
    );
    let metrics = parse_metrics(&String::from_utf8(body).unwrap());
    assert_eq!(
        metric(&metrics, r#"pdfq_jobs_total{outcome="succeeded"}"#),
        1.0
    );
    assert_eq!(
        metric(&metrics, r#"pdfq_jobs_total{outcome="compile_error"}"#),
        1.0
    );
    assert_eq!(metric(&metrics, "pdfq_job_duration_seconds_count"), 2.0);
    assert_eq!(
        metric(&metrics, r#"pdfq_job_duration_seconds_bucket{le="+Inf"}"#),
        2.0
    );
    assert_eq!(metric(&metrics, "pdfq_requests_rejected_total"), 1.0);
    assert!(metric(&metrics, "pdfq_worker_spawns_total") >= 1.0);
}
