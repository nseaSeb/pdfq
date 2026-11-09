//! Workers are non-dumpable on Linux: a process running as the same user (such
//! as another, compromised worker) cannot read their environment or memory
//! through `/proc`.

#![cfg(target_os = "linux")]

mod common;

use std::io::{BufReader, ErrorKind};
use std::process::{Command, Stdio};

use common::worker_bin;
use pdfq_protocol::{WorkerMessage, read_frame};

#[test]
fn worker_is_not_dumpable() {
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
