//! The server and workers are non-dumpable on Linux: a process running as the
//! same user (such as a compromised worker) cannot read their environment or
//! memory through `/proc`.
//!
//! Starting the pool makes this test process non-dumpable, so these tests live
//! in their own test binary.

#![cfg(target_os = "linux")]

mod common;

use std::io::{BufReader, ErrorKind};
use std::process::{Command, Stdio};

use common::worker_bin;
use pdfq_protocol::{WorkerMessage, read_frame};

/// `CAP_SYS_PTRACE` (root under `sudo` or `--privileged`) bypasses the dumpable
/// check: the reads would succeed and prove nothing.
fn has_cap_sys_ptrace() -> bool {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    let cap_eff = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:"))
        .expect("CapEff in /proc/self/status");
    let caps = u64::from_str_radix(cap_eff.trim(), 16).unwrap();
    caps & (1 << 19) != 0
}

fn skip() -> bool {
    let skip = has_cap_sys_ptrace();
    if skip {
        eprintln!("skipped: CAP_SYS_PTRACE bypasses the dumpable check");
    }
    skip
}

/// Whether another process of the same user can read `/proc/<pid>/environ`.
fn environ_readable_by_child(pid: u32) -> bool {
    Command::new("head")
        .args(["-c1", &format!("/proc/{pid}/environ")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run head")
        .success()
}

#[tokio::test(flavor = "multi_thread")]
async fn server_is_not_dumpable_once_started() {
    if skip() {
        return;
    }
    let pid = std::process::id();
    // Control: same reader, same file, before the pool starts.
    assert!(
        environ_readable_by_child(pid),
        "the test process should be readable before start"
    );
    let config = pdfq_server::config::Config {
        workers: Some(1),
        ..pdfq_server::config::Config::defaults()
    };
    let state = pdfq_server::start(config, worker_bin()).expect("start");
    assert!(
        !environ_readable_by_child(pid),
        "/proc/{pid}/environ is readable after start"
    );
    drop(state);
}

#[test]
fn worker_is_not_dumpable() {
    if skip() {
        return;
    }
    // Control: an ordinary (dumpable) child of the same user is readable.
    let mut control = Command::new("sleep")
        .arg("30")
        .spawn()
        .expect("spawn sleep");
    let readable = std::fs::read(format!("/proc/{}/environ", control.id()));
    control.kill().unwrap();
    control.wait().unwrap();
    readable.expect("a dumpable process should expose its environ");

    let mut child = Command::new(worker_bin())
        .env("PDFQ_TEST_SECRET", "s3cret")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn pdfq-worker");
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let ready: Option<WorkerMessage> = read_frame(&mut stdout, 1 << 20).expect("ready frame");
    assert!(matches!(ready, Some(WorkerMessage::Ready(_))), "{ready:?}");

    let pid = child.id();
    for file in ["environ", "mem"] {
        let err = std::fs::read(format!("/proc/{pid}/{file}"))
            .expect_err(&format!("/proc/{pid}/{file} is readable"));
        assert_eq!(err.kind(), ErrorKind::PermissionDenied, "{file}: {err}");
    }

    drop(child.stdin.take()); // EOF: the worker exits.
    assert!(child.wait().unwrap().success());
}
