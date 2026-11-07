//! The hostile corpus (`tests/fixtures/hostile/`): every document must fail
//! with the expected typed error, and the pool must keep serving afterwards.
//!
//! Memory cases only run on Linux: macOS does not enforce `RLIMIT_AS`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use pdfq_protocol::{ErrorKind, File, Job, JobOptions, JobSource};
use pdfq_server::pool::{Pool, PoolConfig};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

/// Builds `pdfq-worker` once (or uses `PDFQ_WORKER_BIN`).
fn worker_bin() -> PathBuf {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        if let Some(bin) = std::env::var_os("PDFQ_WORKER_BIN") {
            return bin.into();
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "-q", "-p", "pdfq-worker"])
            .current_dir(workspace())
            .status()
            .expect("failed to run cargo");
        assert!(status.success(), "building pdfq-worker failed");
        let target = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| workspace().join("target"));
        target.join("debug/pdfq-worker")
    })
    .clone()
}

fn config() -> PoolConfig {
    PoolConfig {
        worker_bin: worker_bin(),
        worker_args: Vec::new(),
        workers: 2,
        jobs_per_worker: 50,
        queue_size: 8,
        wall_timeout: Duration::from_secs(10),
        cpu_timeout: Duration::from_secs(3),
        memory_limit: 256 << 20,
        max_output_size: 100 << 20,
    }
}

fn options() -> JobOptions {
    JobOptions {
        max_pages: 500,
        max_file_size: 10 << 20,
        max_output_size: 100 << 20,
        ..JobOptions::default()
    }
}

fn job(files: Vec<(&str, Vec<u8>)>) -> Job {
    Job {
        source: JobSource::Files {
            files: files
                .into_iter()
                .map(|(path, data)| File {
                    path: path.into(),
                    data,
                })
                .collect(),
            entrypoint: "main.typ".into(),
        },
        inputs: Vec::new(),
        options: options(),
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(workspace().join("tests/fixtures/hostile").join(name)).unwrap()
}

fn hostile(name: &str) -> Job {
    let mut files = vec![("main.typ", fixture(&format!("{name}.typ")))];
    if name == "image-bomb" {
        files.push(("image-bomb.png", fixture("image-bomb.png")));
    }
    job(files)
}

fn sane() -> Job {
    job(vec![("main.typ", b"= Hello\nStill alive.".to_vec())])
}

/// Runs a hostile document, checks the error kind, then checks the pool recovers.
async fn expect(pool: &Pool, job: Job, kind: ErrorKind) -> pdfq_protocol::JobError {
    let err = pool
        .compile(job)
        .await
        .expect_err("hostile document compiled");
    assert_eq!(err.kind, kind, "{err:?}");
    let out = pool.compile(sane()).await.expect("pool did not recover");
    assert!(out.pdf.starts_with(b"%PDF-"));
    err
}

#[tokio::test(flavor = "multi_thread")]
async fn sane_document_compiles() {
    let pool = Pool::start(config());
    let out = pool.compile(sane()).await.unwrap();
    assert_eq!(out.pages, 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn infinite_loop_hits_the_cpu_limit() {
    let pool = Pool::start(PoolConfig {
        cpu_timeout: Duration::from_secs(2),
        ..config()
    });
    let err = expect(&pool, hostile("infinite-loop"), ErrorKind::Timeout).await;
    assert_eq!(err.detail.as_deref(), Some("cpu"));
}

#[tokio::test(flavor = "multi_thread")]
async fn infinite_loop_hits_the_wall_clock_limit() {
    let pool = Pool::start(PoolConfig {
        cpu_timeout: Duration::from_secs(60),
        wall_timeout: Duration::from_secs(2),
        ..config()
    });
    let start = Instant::now();
    let err = expect(&pool, hostile("infinite-loop"), ErrorKind::Timeout).await;
    assert_eq!(err.detail.as_deref(), Some("wall"));
    assert!(start.elapsed() < Duration::from_secs(8));
    assert_eq!(pool.stats().killed_timeout.load(Ordering::Relaxed), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn deep_recursion_is_a_compile_error() {
    let pool = Pool::start(config());
    let err = expect(&pool, hostile("deep-recursion"), ErrorKind::CompileError).await;
    assert!(err.diagnostics[0].message.contains("call depth"), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn thousands_of_pages_hit_the_page_limit() {
    let pool = Pool::start(config());
    expect(&pool, hostile("thousands-pages"), ErrorKind::TooManyPages).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn path_traversal_is_refused() {
    let pool = Pool::start(config());
    let err = expect(&pool, hostile("path-traversal"), ErrorKind::CompileError).await;
    assert!(err.diagnostics[0].message.contains("escape"), "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn forbidden_package_is_refused() {
    let pool = Pool::start(config());
    expect(
        &pool,
        hostile("forbidden-package"),
        ErrorKind::PackageNotAllowed,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn template_symlink_escape_is_refused() {
    let templates = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), "secret").unwrap();
    let dir = templates.path().join("invoice");
    std::fs::create_dir(&dir).unwrap();
    std::fs::write(dir.join("main.typ"), "#read(\"secret.txt\")").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret.txt"), dir.join("secret.txt")).unwrap();

    let pool = Pool::start(PoolConfig {
        worker_args: vec!["--template-dir".into(), templates.path().into()],
        ..config()
    });
    let template = |name: &str| Job {
        source: JobSource::Template {
            name: name.into(),
            assets: Vec::new(),
        },
        inputs: Vec::new(),
        options: options(),
    };
    expect(&pool, template("invoice"), ErrorKind::CompileError).await;
    expect(&pool, template("../invoice"), ErrorKind::InvalidInput).await;
    expect(&pool, template("missing"), ErrorKind::NotFound).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pool_output_limit_applies_to_every_job() {
    let pool = Pool::start(PoolConfig {
        max_output_size: 100,
        ..config()
    });
    let mut job = sane();
    job.options.max_output_size = 0; // "No limit" from the caller is capped by the pool.
    let err = pool.compile(job).await.expect_err("job succeeded");
    assert_eq!(err.kind, ErrorKind::OutputTooLarge, "{err:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn asset_paths_cannot_leave_assets() {
    let templates = tempfile::tempdir().unwrap();
    std::fs::create_dir(templates.path().join("t")).unwrap();
    std::fs::write(templates.path().join("t/main.typ"), "ok").unwrap();
    let pool = Pool::start(PoolConfig {
        worker_args: vec!["--template-dir".into(), templates.path().into()],
        ..config()
    });
    let job = Job {
        source: JobSource::Template {
            name: "t".into(),
            assets: vec![File {
                path: "../lib.typ".into(),
                data: b"x".to_vec(),
            }],
        },
        inputs: Vec::new(),
        options: options(),
    };
    expect(&pool, job, ErrorKind::InvalidInput).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_worker_binary_fails_queued_jobs() {
    let pool = Pool::start(PoolConfig {
        worker_bin: "/nonexistent/pdfq-worker".into(),
        ..config()
    });
    let result = tokio::time::timeout(Duration::from_secs(10), pool.compile(sane())).await;
    let err = result.expect("job hung").unwrap_err();
    assert_eq!(err.kind, ErrorKind::WorkerCrashed);
}

#[tokio::test(flavor = "multi_thread")]
async fn output_size_is_limited() {
    let pool = Pool::start(config());
    let mut big = sane();
    big.options.max_output_size = 100;
    expect(&pool, big, ErrorKind::OutputTooLarge).await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn doubling_array_hits_the_memory_limit() {
    let pool = Pool::start(config());
    expect(&pool, hostile("doubling-array"), ErrorKind::MemoryLimit).await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn exponential_recursion_is_stopped() {
    let pool = Pool::start(config());
    let err = pool.compile(hostile("exp-recursion")).await.unwrap_err();
    // Memoization grows with every distinct call: depending on the budgets, the
    // CPU or the memory limit is reached first.
    assert!(
        matches!(err.kind, ErrorKind::MemoryLimit | ErrorKind::Timeout),
        "{err:?}"
    );
    pool.compile(sane()).await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn image_bomb_is_stopped() {
    let pool = Pool::start(config());
    // The decoder allocates the full 40 GB pixel buffer from the header. Header
    // pre-validation (server side) will turn this into `invalid_input` earlier.
    expect(&pool, hostile("image-bomb"), ErrorKind::MemoryLimit).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hostile_job_does_not_block_other_workers() {
    let pool = Pool::start(PoolConfig {
        wall_timeout: Duration::from_secs(10),
        ..config()
    });
    let slow = pool.submit(hostile("infinite-loop")).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let start = Instant::now();
    pool.compile(sane()).await.unwrap();
    assert!(
        // Generous bound: tests run in parallel and saturate the CPU.
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(slow.await.unwrap().unwrap_err().kind, ErrorKind::Timeout);
}

#[tokio::test(flavor = "multi_thread")]
async fn full_queue_is_refused() {
    let pool = Pool::start(PoolConfig {
        workers: 1,
        queue_size: 1,
        wall_timeout: Duration::from_secs(3),
        ..config()
    });
    // Wait for the worker to be up so the first job is taken off the queue.
    pool.compile(sane()).await.unwrap();
    let _running = pool.submit(hostile("infinite-loop")).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _queued = pool.submit(sane()).unwrap();
    let err = pool.submit(sane()).unwrap_err();
    assert_eq!(err.kind, ErrorKind::QueueFull);
}

#[tokio::test(flavor = "multi_thread")]
async fn workers_are_recycled() {
    let pool = Pool::start(PoolConfig {
        workers: 1,
        jobs_per_worker: 2,
        ..config()
    });
    for _ in 0..5 {
        pool.compile(sane()).await.unwrap();
    }
    let stats = pool.stats();
    assert_eq!(stats.recycled.load(Ordering::Relaxed), 2);
    assert!(stats.spawned.load(Ordering::Relaxed) >= 3);
}
