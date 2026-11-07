//! Prometheus metrics, in the text exposition format (written by hand: a few
//! counters do not justify a dependency). Label values come only from fixed sets.

use std::fmt::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pdfq_protocol::{ErrorKind, JobError, TYPST_VERSION};

use crate::pool::PoolStats;

/// Upper bounds (seconds) of the job duration histogram.
const BUCKETS: [f64; 10] = [0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0];

/// Job outcomes and durations, recorded by the pool for sync and async jobs alike.
#[derive(Debug, Default)]
pub struct JobMetrics {
    succeeded: AtomicU64,
    failed: [AtomicU64; ErrorKind::ALL.len()],
    /// Cumulative counts per bucket, plus `+Inf` last.
    buckets: [AtomicU64; BUCKETS.len() + 1],
    duration_micros: AtomicU64,
}

impl JobMetrics {
    /// Records a job outcome. `elapsed` is `None` for jobs that never ran.
    pub fn record<T>(&self, result: &Result<T, JobError>, elapsed: Option<Duration>) {
        match result {
            Ok(_) => self.succeeded.fetch_add(1, Ordering::Relaxed),
            Err(err) => {
                // ALL is complete (tested in pdfq-protocol); never panic here.
                if let Some(counter) = self.failed.get(err.kind as usize) {
                    counter.fetch_add(1, Ordering::Relaxed);
                }
                0
            }
        };
        if let Some(elapsed) = elapsed {
            let seconds = elapsed.as_secs_f64();
            for (i, bound) in BUCKETS.iter().enumerate() {
                if seconds <= *bound {
                    self.buckets[i].fetch_add(1, Ordering::Relaxed);
                }
            }
            self.buckets[BUCKETS.len()].fetch_add(1, Ordering::Relaxed);
            let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
            self.duration_micros.fetch_add(micros, Ordering::Relaxed);
        }
    }
}

/// Values that do not live in [`PoolStats`].
pub struct Extra {
    pub queued: u64,
    pub admission_rejected: u64,
    pub async_jobs: u64,
    pub webhooks_delivered: u64,
    pub webhooks_failed: u64,
}

pub const CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

pub fn render(stats: &PoolStats, extra: &Extra) -> String {
    let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
    let jobs = &stats.jobs;
    let mut out = String::new();
    let info = format!(
        "{{version=\"{}\",typst_version=\"{TYPST_VERSION}\"}}",
        env!("CARGO_PKG_VERSION")
    );
    write_metric(
        &mut out,
        "pdfq_info",
        "gauge",
        "Build information.",
        &[(&info, 1)],
    );

    let mut outcomes = vec![("{outcome=\"succeeded\"}".to_string(), load(&jobs.succeeded))];
    for kind in ErrorKind::ALL {
        let value = jobs.failed.get(kind as usize).map_or(0, load);
        outcomes.push((format!("{{outcome=\"{}\"}}", kind.as_str()), value));
    }
    let outcomes: Vec<_> = outcomes.iter().map(|(l, v)| (l.as_str(), *v)).collect();
    write_metric(
        &mut out,
        "pdfq_jobs_total",
        "counter",
        "Compilation jobs by outcome (succeeded or error kind).",
        &outcomes,
    );

    let _ = writeln!(
        out,
        "# HELP pdfq_job_duration_seconds Time spent running jobs in a worker.\n\
         # TYPE pdfq_job_duration_seconds histogram"
    );
    for (i, bound) in BUCKETS.iter().enumerate() {
        let count = load(&jobs.buckets[i]);
        let _ = writeln!(
            out,
            "pdfq_job_duration_seconds_bucket{{le=\"{bound}\"}} {count}"
        );
    }
    let total = load(&jobs.buckets[BUCKETS.len()]);
    let sum = load(&jobs.duration_micros) as f64 / 1e6;
    let _ = writeln!(
        out,
        "pdfq_job_duration_seconds_bucket{{le=\"+Inf\"}} {total}"
    );
    let _ = writeln!(out, "pdfq_job_duration_seconds_sum {sum}");
    let _ = writeln!(out, "pdfq_job_duration_seconds_count {total}");

    let live = stats.ready_workers.load(Ordering::Relaxed) as u64;
    let busy = stats.busy_workers.load(Ordering::Relaxed) as u64;
    write_metric(
        &mut out,
        "pdfq_workers",
        "gauge",
        "Live workers by state.",
        &[
            ("{state=\"ready\"}", live.saturating_sub(busy)),
            ("{state=\"busy\"}", busy),
        ],
    );
    write_metric(
        &mut out,
        "pdfq_queue_length",
        "gauge",
        "Jobs waiting for a worker.",
        &[("", extra.queued)],
    );
    write_metric(
        &mut out,
        "pdfq_async_jobs",
        "gauge",
        "Async jobs held in memory.",
        &[("", extra.async_jobs)],
    );
    write_metric(
        &mut out,
        "pdfq_worker_spawns_total",
        "counter",
        "Worker processes started.",
        &[("", load(&stats.spawned))],
    );
    write_metric(
        &mut out,
        "pdfq_worker_exits_total",
        "counter",
        "Worker processes stopped, by reason.",
        &[
            ("{reason=\"recycled\"}", load(&stats.recycled)),
            ("{reason=\"wall_timeout\"}", load(&stats.killed_timeout)),
            ("{reason=\"cpu_limit\"}", load(&stats.died_cpu)),
            ("{reason=\"memory_limit\"}", load(&stats.died_memory)),
            ("{reason=\"crashed\"}", load(&stats.crashed)),
        ],
    );
    write_metric(
        &mut out,
        "pdfq_requests_rejected_total",
        "counter",
        "Requests refused before reading their body (server at capacity).",
        &[("", extra.admission_rejected)],
    );
    write_metric(
        &mut out,
        "pdfq_webhook_deliveries_total",
        "counter",
        "Webhook deliveries by result (after retries).",
        &[
            ("{result=\"delivered\"}", extra.webhooks_delivered),
            ("{result=\"failed\"}", extra.webhooks_failed),
        ],
    );
    out
}

fn write_metric(out: &mut String, name: &str, kind: &str, help: &str, samples: &[(&str, u64)]) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
    for (labels, value) in samples {
        let _ = writeln!(out, "{name}{labels} {value}");
    }
}
