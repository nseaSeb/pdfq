//! Async jobs and webhooks: requests go through the router to real worker processes,
//! webhooks to a local receiver.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use pdfq_server::config::Config;
use serde_json::Value;
use tokio::sync::mpsc;
use tower::ServiceExt;

const SECRET: &str = "test-secret";
const HELLO: &[u8] = b"Hello";
const BROKEN: &[u8] = b"#let x = (";

fn config() -> Config {
    Config {
        workers: Some(1),
        wall_timeout: Duration::from_secs(10),
        cpu_timeout: Duration::from_secs(5),
        webhook_secret: Some(SECRET.into()),
        webhook_allow_private: true,
        webhook_allow_http: true,
        webhook_retry_delay: Duration::from_millis(10),
        ..Config::defaults()
    }
}

fn app(config: Config) -> Router {
    pdfq_server::api::router(pdfq_server::start(config, common::worker_bin()).unwrap())
}

/// A `POST /jobs` with `main.typ` and an optional callback.
fn submit(source: &[u8], callback: Option<&str>) -> Request<Body> {
    let boundary = "pdfq-test-boundary";
    let mut body = Vec::new();
    let mut part = |disposition: &str, data: &[u8]| {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(format!("Content-Disposition: {disposition}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(b"\r\n");
    };
    part(r#"form-data; name="file"; filename="main.typ""#, source);
    if let Some(url) = callback {
        part(r#"form-data; name="callback_url""#, url.as_bytes());
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    Request::post("/jobs")
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(Body::from(body))
        .unwrap()
}

async fn send(app: &Router, request: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body.to_vec())
}

async fn get(app: &Router, path: &str) -> (StatusCode, Vec<u8>) {
    let (status, _, body) = send(app, Request::get(path).body(Body::empty()).unwrap()).await;
    (status, body)
}

fn json(body: &[u8]) -> Value {
    serde_json::from_slice(body)
        .unwrap_or_else(|_| panic!("non-JSON body: {}", String::from_utf8_lossy(body)))
}

/// Submits a job and returns its id.
async fn create(app: &Router, request: Request<Body>) -> String {
    let (status, headers, body) = send(app, request).await;
    assert_eq!(
        status,
        StatusCode::ACCEPTED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let body = json(&body);
    let id = body["job_id"].as_str().unwrap().to_owned();
    assert_eq!(body["status_url"], format!("/jobs/{id}"));
    assert_eq!(headers["location"], format!("/jobs/{id}").as_str());
    id
}

/// Polls the job until it is finished (and its webhook settled) and returns its view.
async fn wait_done(app: &Router, id: &str) -> Value {
    for _ in 0..400 {
        let (status, body) = get(app, &format!("/jobs/{id}")).await;
        assert_eq!(status, StatusCode::OK);
        let view = json(&body);
        let finished = view["status"] == "succeeded" || view["status"] == "failed";
        if finished && view["webhook"]["state"] != "pending" {
            return view;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job {id} never finished");
}

struct Hook {
    headers: HeaderMap,
    body: Bytes,
}

/// A local webhook receiver that fails the first `failures` calls with a 500.
async fn receiver(failures: u32) -> (String, mpsc::UnboundedReceiver<Hook>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicU32::new(0));
    let handler = move |headers: HeaderMap, body: Bytes| {
        let (tx, calls) = (tx.clone(), calls.clone());
        async move {
            if calls.fetch_add(1, Ordering::SeqCst) < failures {
                return StatusCode::INTERNAL_SERVER_ERROR;
            }
            let _ = tx.send(Hook { headers, body });
            StatusCode::NO_CONTENT
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let router = Router::new().route("/hook", axum::routing::post(handler));
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (url, rx)
}

fn check_signature(hook: &Hook) {
    let header = |name: &str| hook.headers[name].to_str().unwrap().to_owned();
    let timestamp: u64 = header("x-pdfq-timestamp").parse().unwrap();
    let expected = pdfq_server::webhook::sign(SECRET, timestamp, &hook.body);
    assert_eq!(header("x-pdfq-signature"), format!("sha256={expected}"));
}

#[tokio::test(flavor = "multi_thread")]
async fn job_succeeds_and_delivers_a_signed_webhook() {
    let app = app(config());
    let (url, mut hooks) = receiver(0).await;
    let id = create(&app, submit(HELLO, Some(&url))).await;

    let view = wait_done(&app, &id).await;
    assert_eq!(view["status"], "succeeded", "{view}");
    assert_eq!(view["pages"], 1);
    assert_eq!(view["webhook"]["state"], "delivered");
    assert_eq!(view["webhook"]["attempts"], 1);
    assert!(view["finished_at"].is_string());

    let hook = hooks.recv().await.unwrap();
    check_signature(&hook);
    assert_eq!(hook.headers["x-pdfq-job-id"], id.as_str());
    assert_eq!(hook.headers["x-pdfq-status"], "succeeded");
    assert_eq!(hook.headers["content-type"], "application/pdf");
    assert!(hook.body.starts_with(b"%PDF-"));

    let (status, pdf) = get(&app, &format!("/jobs/{id}/pdf")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pdf, hook.body.as_ref());
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_is_retried_on_server_errors() {
    let app = app(config());
    let (url, mut hooks) = receiver(2).await;
    let id = create(&app, submit(HELLO, Some(&url))).await;

    let view = wait_done(&app, &id).await;
    assert_eq!(view["webhook"]["state"], "delivered", "{view}");
    assert_eq!(view["webhook"]["attempts"], 3);
    check_signature(&hooks.recv().await.unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn undeliverable_webhook_is_reported() {
    let app = app(Config {
        webhook_retries: 1,
        ..config()
    });
    let (url, _hooks) = receiver(u32::MAX).await;
    let id = create(&app, submit(HELLO, Some(&url))).await;

    let view = wait_done(&app, &id).await;
    assert_eq!(view["status"], "succeeded", "{view}");
    assert_eq!(view["webhook"]["state"], "failed");
    assert_eq!(view["webhook"]["attempts"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_job_reports_its_error() {
    let app = app(config());
    let (url, mut hooks) = receiver(0).await;
    let id = create(&app, submit(BROKEN, Some(&url))).await;

    let view = wait_done(&app, &id).await;
    assert_eq!(view["status"], "failed", "{view}");
    assert_eq!(view["error"]["kind"], "compile_error");

    let (status, body) = get(&app, &format!("/jobs/{id}/pdf")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json(&body)["error"]["kind"], "compile_error");

    let hook = hooks.recv().await.unwrap();
    check_signature(&hook);
    assert_eq!(hook.headers["x-pdfq-status"], "failed");
    assert_eq!(hook.headers["content-type"], "application/json");
    assert_eq!(json(&hook.body)["error"]["kind"], "compile_error");
}

#[tokio::test(flavor = "multi_thread")]
async fn job_without_callback() {
    let app = app(config());
    let id = create(&app, submit(HELLO, None)).await;
    let view = wait_done(&app, &id).await;
    assert_eq!(view["status"], "succeeded", "{view}");
    assert!(view["webhook"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn unfinished_job_pdf_is_a_conflict() {
    let app = app(config());
    let looping =
        std::fs::read(common::workspace().join("tests/fixtures/hostile/infinite-loop.typ"))
            .unwrap();
    let id = create(&app, submit(&looping, None)).await;
    let (status, body) = get(&app, &format!("/jobs/{id}/pdf")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json(&body)["error"]["kind"], "not_finished");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_jobs_are_not_found() {
    let app = app(config());
    for path in [
        "/jobs/not-a-uuid",
        "/jobs/00000000-0000-4000-8000-000000000000",
        "/jobs/00000000-0000-4000-8000-000000000000/pdf",
    ] {
        let (status, body) = get(&app, path).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}");
        assert_eq!(json(&body)["error"]["kind"], "not_found");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn finished_jobs_expire() {
    let app = app(Config {
        job_ttl: Duration::from_millis(300),
        ..config()
    });
    let id = create(&app, submit(HELLO, None)).await;
    wait_done(&app, &id).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        get(&app, &format!("/jobs/{id}")).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn job_store_is_bounded() {
    let app = app(Config {
        max_jobs: 1,
        ..config()
    });
    let looping =
        std::fs::read(common::workspace().join("tests/fixtures/hostile/infinite-loop.typ"))
            .unwrap();
    create(&app, submit(&looping, None)).await;
    let (status, headers, body) = send(&app, submit(HELLO, None)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(json(&body)["error"]["kind"], "queue_full");
    assert_eq!(headers["retry-after"], "1");
}

#[tokio::test(flavor = "multi_thread")]
async fn callback_urls_are_checked() {
    let cases = [
        (config(), "not a url"),
        (config(), "ftp://example.com/hook"),
        (config(), "https://user:pass@example.com/hook"),
        // Private addresses are refused by default.
        (
            Config {
                webhook_allow_private: false,
                ..config()
            },
            "https://127.0.0.1/hook",
        ),
        (
            Config {
                webhook_allow_private: false,
                ..config()
            },
            "https://[::1]/hook",
        ),
        // http is refused by default.
        (
            Config {
                webhook_allow_http: false,
                ..config()
            },
            "http://example.com/hook",
        ),
        (
            Config {
                webhook_allowed_hosts: vec!["*.example.com".into()],
                ..config()
            },
            "https://evil.test/hook",
        ),
        // Webhooks are disabled without a secret.
        (
            Config {
                webhook_secret: None,
                ..config()
            },
            "https://example.com/hook",
        ),
    ];
    for (config, url) in cases {
        let app = app(config);
        let (status, _, body) = send(&app, submit(HELLO, Some(url))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{url}");
        assert_eq!(json(&body)["error"]["kind"], "invalid_input", "{url}");
    }

    // `callback_url` is refused on the synchronous endpoint.
    let app = app(config());
    let mut request = submit(HELLO, Some("https://example.com/hook"));
    *request.uri_mut() = "/compile".parse().unwrap();
    let (status, _, _) = send(&app, request).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test(flavor = "multi_thread")]
async fn hostnames_resolving_to_private_addresses_are_not_called() {
    let app = app(Config {
        webhook_allow_private: false,
        webhook_retries: 0,
        ..config()
    });
    let (url, mut hooks) = receiver(0).await;
    // Passes validation (not an IP literal), but the resolver refuses loopback.
    let url = url.replace("127.0.0.1", "localhost");
    let id = create(&app, submit(HELLO, Some(&url))).await;

    let view = wait_done(&app, &id).await;
    assert_eq!(view["webhook"]["state"], "failed", "{view}");
    assert!(hooks.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_reports_what_the_store_recorded() {
    let app = app(Config {
        max_stored_size: 100,
        ..config()
    });
    let (url, mut hooks) = receiver(0).await;
    let id = create(&app, submit(HELLO, Some(&url))).await;

    let view = wait_done(&app, &id).await;
    assert_eq!(view["status"], "failed", "{view}");
    assert_eq!(view["error"]["kind"], "output_too_large");

    let hook = hooks.recv().await.unwrap();
    assert_eq!(hook.headers["x-pdfq-status"], "failed");
    assert_eq!(json(&hook.body)["error"]["kind"], "output_too_large");
}

#[tokio::test(flavor = "multi_thread")]
async fn failed_job_pdf_is_always_unprocessable() {
    // A job that waits too long for a worker fails with `queue_full`, but fetching
    // its PDF must not answer 429: retrying the GET cannot help.
    let app = app(Config {
        queue_timeout: Duration::ZERO,
        wall_timeout: Duration::from_secs(1),
        ..config()
    });
    let looping =
        std::fs::read(common::workspace().join("tests/fixtures/hostile/infinite-loop.typ"))
            .unwrap();
    create(&app, submit(&looping, None)).await;
    let id = create(&app, submit(HELLO, None)).await;
    let view = wait_done(&app, &id).await;
    assert_eq!(view["error"]["kind"], "queue_full", "{view}");
    let (status, body) = get(&app, &format!("/jobs/{id}/pdf")).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(json(&body)["error"]["kind"], "queue_full");
}
