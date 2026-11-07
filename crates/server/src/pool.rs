//! A pool of supervised worker processes behind a bounded queue.
//!
//! Each slot owns at most one worker process and runs one job at a time:
//! - the job is written to the worker's stdin and the result read from stdout,
//! - a wall-clock timeout kills the worker (`SIGKILL`),
//! - a dead worker is classified (`memory_limit`, CPU `timeout`, crash) and respawned,
//! - workers are recycled after a number of jobs.

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use pdfq_protocol::{
    EXIT_OOM, ErrorKind, Job, JobError, JobOutput, PROTOCOL_VERSION, WorkerMessage,
    read_frame_async, write_frame_async,
};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::time::timeout;

use crate::spawn::{self, HardLimits};

/// How long a worker may take to start (fonts are loaded at startup).
const READY_TIMEOUT: Duration = Duration::from_secs(60);
/// How long a recycled worker may take to exit after its stdin is closed.
const RETIRE_TIMEOUT: Duration = Duration::from_secs(5);
/// Delay before retrying a failed spawn.
const SPAWN_BACKOFF: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct PoolConfig {
    /// Path of the `pdfq-worker` binary.
    pub worker_bin: PathBuf,
    /// Extra worker arguments (font, package and template paths).
    pub worker_args: Vec<OsString>,
    /// Number of worker processes.
    pub workers: usize,
    /// Jobs a worker runs before being replaced.
    pub jobs_per_worker: u32,
    /// Jobs waiting for a worker before submissions are refused.
    pub queue_size: usize,
    /// Wall-clock limit per job.
    pub wall_timeout: Duration,
    /// Longest wait in the queue: older jobs fail with `queue_full` without running.
    pub queue_timeout: Duration,
    /// CPU time limit per job.
    pub cpu_timeout: Duration,
    /// Memory budget per job, above the worker's baseline.
    pub memory_limit: u64,
    /// Largest PDF a worker may return.
    pub max_output_size: u64,
}

impl Default for PoolConfig {
    /// Defaults matching `docs/api-spec.md`; the server overrides them from its configuration.
    fn default() -> Self {
        Self {
            worker_bin: PathBuf::from("pdfq-worker"),
            worker_args: Vec::new(),
            workers: std::thread::available_parallelism().map_or(2, |n| n.get()),
            jobs_per_worker: 100,
            queue_size: 64,
            wall_timeout: Duration::from_secs(30),
            queue_timeout: Duration::from_secs(30),
            cpu_timeout: Duration::from_secs(20),
            memory_limit: 512 << 20,
            max_output_size: 100 << 20,
        }
    }
}

/// Counters exposed for health checks and metrics.
#[derive(Debug, Default)]
pub struct PoolStats {
    pub ready_workers: AtomicUsize,
    pub busy_workers: AtomicUsize,
    pub spawned: AtomicU64,
    pub killed_timeout: AtomicU64,
    pub died_memory: AtomicU64,
    pub died_cpu: AtomicU64,
    pub crashed: AtomicU64,
    pub recycled: AtomicU64,
    /// Outcomes and durations of every job, sync or async.
    pub jobs: crate::metrics::JobMetrics,
}

pub type JobResult = Result<JobOutput, JobError>;

/// The error of a job whose result sender was dropped.
pub fn stopped() -> JobError {
    JobError::new(ErrorKind::WorkerCrashed, "the worker pool stopped")
}

/// Called when a worker picks the job up.
pub type OnStart = Box<dyn FnOnce() + Send>;

struct Task {
    job: Job,
    reply: oneshot::Sender<JobResult>,
    enqueued: Instant,
    on_start: Option<OnStart>,
}

/// Handle to the pool. Dropping it stops the workers once the queue drains.
pub struct Pool {
    queue: async_channel::Sender<Task>,
    stats: Arc<PoolStats>,
    max_output_size: u64,
}

impl Pool {
    /// Starts the worker slots. Must be called inside a Tokio runtime.
    pub fn start(config: PoolConfig) -> Self {
        let (queue, receiver) = async_channel::bounded(config.queue_size.max(1));
        let stats = Arc::new(PoolStats::default());
        let max_output_size = config.max_output_size;
        let config = Arc::new(config);
        for slot in 0..config.workers.max(1) {
            tokio::spawn(run_slot(
                slot,
                config.clone(),
                receiver.clone(),
                stats.clone(),
            ));
        }
        Self {
            queue,
            stats,
            max_output_size,
        }
    }

    /// Queues a job. Fails immediately with `queue_full` when the queue is full.
    /// Dropping the returned receiver before a worker picks the job up cancels it.
    pub fn submit(&self, job: Job) -> Result<oneshot::Receiver<JobResult>, JobError> {
        self.submit_with(job, None)
    }

    /// Like [`Self::submit`], calling `on_start` when a worker picks the job up.
    pub fn submit_with(
        &self,
        mut job: Job,
        on_start: Option<OnStart>,
    ) -> Result<oneshot::Receiver<JobResult>, JobError> {
        // The pool's output limit also sizes the response frame: never let a job exceed it.
        let pool_max = self.max_output_size;
        let job_max = job.options.max_output_size;
        job.options.max_output_size = if job_max == 0 {
            pool_max
        } else {
            job_max.min(pool_max)
        };
        let (reply, receiver) = oneshot::channel();
        self.queue
            .try_send(Task {
                job,
                reply,
                enqueued: Instant::now(),
                on_start,
            })
            .map_err(|_| {
                let error = JobError::new(
                    ErrorKind::QueueFull,
                    "the compilation queue is full, retry later",
                );
                self.stats.jobs.record::<()>(&Err(error.clone()), None);
                error
            })?;
        Ok(receiver)
    }

    /// Queues a job and waits for its result.
    pub async fn compile(&self, job: Job) -> JobResult {
        let receiver = self.submit(job)?;
        receiver.await.unwrap_or_else(|_| Err(stopped()))
    }

    /// Jobs waiting for a worker.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    pub fn stats(&self) -> &PoolStats {
        &self.stats
    }
}

async fn run_slot(
    slot: usize,
    config: Arc<PoolConfig>,
    queue: async_channel::Receiver<Task>,
    stats: Arc<PoolStats>,
) {
    let mut worker: Option<Worker> = None;
    loop {
        // Keep a warm worker ready before taking the next job.
        if worker.is_none() {
            match Worker::spawn(&config, slot).await {
                Ok(w) => {
                    stats.spawned.fetch_add(1, Ordering::Relaxed);
                    stats.ready_workers.fetch_add(1, Ordering::Relaxed);
                    worker = Some(w);
                }
                Err(err) => {
                    tracing::error!(slot, error = %err, "failed to start a worker");
                    if queue.is_closed() && queue.is_empty() {
                        break; // The pool was dropped.
                    }
                    // With no worker running anywhere, queued jobs would wait forever.
                    if stats.ready_workers.load(Ordering::Relaxed) == 0 {
                        while let Ok(task) = queue.try_recv() {
                            let result = Err(JobError::new(
                                ErrorKind::WorkerCrashed,
                                "no worker is available",
                            ));
                            stats.jobs.record(&result, None);
                            let _ = task.reply.send(result);
                        }
                    }
                    tokio::time::sleep(SPAWN_BACKOFF).await;
                    continue;
                }
            }
        }
        let Ok(task) = queue.recv().await else { break };
        if task.reply.is_closed() {
            continue; // The client went away while the job was queued.
        }
        if task.enqueued.elapsed() > config.queue_timeout {
            let result = Err(JobError::new(
                ErrorKind::QueueFull,
                "timed out waiting for a worker, retry later",
            ));
            stats.jobs.record(&result, None);
            let _ = task.reply.send(result);
            continue;
        }

        let mut task = task;
        if let Some(on_start) = task.on_start.take() {
            on_start();
        }
        let w = worker.as_mut().expect("worker spawned above");
        stats.busy_workers.fetch_add(1, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let (result, alive) = w.run(&task.job, &config, &stats).await;
        stats.busy_workers.fetch_sub(1, Ordering::Relaxed);
        stats.jobs.record(&result, Some(started.elapsed()));
        let _ = task.reply.send(result);

        w.jobs += 1;
        if !alive || w.jobs >= config.jobs_per_worker {
            stats.ready_workers.fetch_sub(1, Ordering::Relaxed);
            let w = worker.take().expect("worker present");
            if alive {
                stats.recycled.fetch_add(1, Ordering::Relaxed);
                w.retire().await;
            }
        }
    }
    if let Some(w) = worker {
        w.retire().await;
    }
}

struct Worker {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: ChildStdout,
    jobs: u32,
    max_frame: usize,
}

impl Worker {
    async fn spawn(config: &PoolConfig, slot: usize) -> std::io::Result<Self> {
        let mut cmd = Command::new(&config.worker_bin);
        cmd.args(&config.worker_args)
            .arg("--memory-budget")
            .arg(config.memory_limit.to_string())
            .arg("--cpu-budget")
            .arg(config.cpu_timeout.as_secs().max(1).to_string())
            .env_clear()
            // Fewer threads and malloc arenas: less reserved address space.
            .env("RAYON_NUM_THREADS", "1")
            .env("MALLOC_ARENA_MAX", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // Generous ceiling over the worker's life: per-job soft limits do the real work,
        // and the soft limit must never be clamped to the hard one (SIGKILL, not SIGXCPU).
        let per_job = config.cpu_timeout.as_secs().max(1) + 2;
        let lifetime_cpu = per_job * 2 * u64::from(config.jobs_per_worker.max(1)) + 120;
        spawn::apply(
            &mut cmd,
            HardLimits {
                address_space: config.memory_limit.saturating_add(4 << 30),
                cpu_secs: lifetime_cpu,
            },
        );

        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take();
        let mut stdout = child.stdout.take().expect("piped stdout");
        if let Some(stderr) = child.stderr.take() {
            let pid = child.id();
            tokio::spawn(async move {
                forward_stderr(stderr, slot, pid).await;
            });
        }

        let ready = timeout(READY_TIMEOUT, read_frame_async(&mut stdout, 1 << 20)).await;
        let ready = match ready {
            Ok(Ok(Some(WorkerMessage::Ready(ready)))) => ready,
            other => {
                let _ = child.kill().await;
                return Err(std::io::Error::other(format!(
                    "no ready message: {other:?}"
                )));
            }
        };
        if ready.protocol_version != PROTOCOL_VERSION {
            let _ = child.kill().await;
            return Err(std::io::Error::other(format!(
                "worker speaks protocol {}, expected {PROTOCOL_VERSION}",
                ready.protocol_version
            )));
        }
        tracing::debug!(
            slot,
            pid = child.id(),
            fonts = ready.font_families,
            baseline_mib = ready.baseline_memory >> 20,
            "worker ready"
        );
        Ok(Self {
            child,
            stdin,
            stdout,
            jobs: 0,
            max_frame: usize::try_from(config.max_output_size)
                .unwrap_or(usize::MAX)
                .saturating_add(16 << 20),
        })
    }

    /// Runs one job. Returns the result and whether the worker can be reused.
    async fn run(
        &mut self,
        job: &Job,
        config: &PoolConfig,
        stats: &PoolStats,
    ) -> (JobResult, bool) {
        let Some(stdin) = self.stdin.as_mut() else {
            return (Err(self.reap(stats).await), false);
        };
        if write_frame_async(stdin, job).await.is_err() {
            return (Err(self.reap(stats).await), false);
        }
        match timeout(
            config.wall_timeout,
            read_frame_async(&mut self.stdout, self.max_frame),
        )
        .await
        {
            Ok(Ok(Some(WorkerMessage::Done(output)))) => (Ok(output), true),
            Ok(Ok(Some(WorkerMessage::Failed(err)))) => {
                let alive = err.kind != ErrorKind::WorkerCrashed;
                if !alive {
                    stats.crashed.fetch_add(1, Ordering::Relaxed);
                    let _ = self.child.kill().await;
                }
                (Err(err), alive)
            }
            Ok(Ok(Some(WorkerMessage::Ready(_)))) | Ok(Err(_)) => {
                // Protocol violation or unreadable frame: do not trust this worker.
                let _ = self.child.kill().await;
                stats.crashed.fetch_add(1, Ordering::Relaxed);
                (
                    Err(JobError::new(
                        ErrorKind::WorkerCrashed,
                        "invalid worker response",
                    )),
                    false,
                )
            }
            Ok(Ok(None)) => (Err(self.reap(stats).await), false),
            Err(_elapsed) => {
                let _ = self.child.kill().await;
                stats.killed_timeout.fetch_add(1, Ordering::Relaxed);
                let limit = config.wall_timeout.as_secs_f64();
                let err = JobError::new(
                    ErrorKind::Timeout,
                    format!("compilation exceeded the {limit}s time limit"),
                )
                .with_detail("wall");
                (Err(err), false)
            }
        }
    }

    /// Waits for a dead worker and explains why it died.
    async fn reap(&mut self, stats: &PoolStats) -> JobError {
        let status = match timeout(RETIRE_TIMEOUT, self.child.wait()).await {
            Ok(Ok(status)) => status,
            _ => {
                let _ = self.child.kill().await;
                stats.crashed.fetch_add(1, Ordering::Relaxed);
                return JobError::new(ErrorKind::WorkerCrashed, "the worker stopped responding");
            }
        };
        let err = classify(status);
        let counter = match err.kind {
            ErrorKind::MemoryLimit => &stats.died_memory,
            ErrorKind::Timeout => &stats.died_cpu,
            _ => &stats.crashed,
        };
        counter.fetch_add(1, Ordering::Relaxed);
        err
    }

    /// Closes stdin so the worker exits on its own, then kills it if needed.
    async fn retire(mut self) {
        drop(self.stdin.take());
        if timeout(RETIRE_TIMEOUT, self.child.wait()).await.is_err() {
            let _ = self.child.kill().await;
        }
    }
}

/// Logs worker stderr line by line, truncating long lines, without ever
/// buffering more than `MAX_LINE` bytes in the server.
async fn forward_stderr(mut stderr: tokio::process::ChildStderr, slot: usize, pid: Option<u32>) {
    const MAX_LINE: usize = 4096;
    let mut line = Vec::with_capacity(256);
    let mut chunk = [0u8; 4096];
    let mut truncated = false;
    let emit = |line: &mut Vec<u8>, truncated: &mut bool| {
        let suffix = if *truncated { " [truncated]" } else { "" };
        tracing::warn!(
            slot,
            pid,
            "worker: {}{suffix}",
            String::from_utf8_lossy(line)
        );
        line.clear();
        *truncated = false;
    };
    while let Ok(n) = stderr.read(&mut chunk).await {
        if n == 0 {
            break;
        }
        for &byte in &chunk[..n] {
            if byte == b'\n' {
                emit(&mut line, &mut truncated);
            } else if line.len() < MAX_LINE {
                line.push(byte);
            } else {
                truncated = true;
            }
        }
    }
    if !line.is_empty() {
        emit(&mut line, &mut truncated);
    }
}

fn classify(status: ExitStatus) -> JobError {
    if status.code() == Some(EXIT_OOM) {
        return JobError::new(
            ErrorKind::MemoryLimit,
            "compilation exceeded the memory limit",
        );
    }
    if status.signal() == Some(libc::SIGXCPU) {
        return JobError::new(
            ErrorKind::Timeout,
            "compilation exceeded the CPU time limit",
        )
        .with_detail("cpu");
    }
    JobError::new(
        ErrorKind::WorkerCrashed,
        format!("the worker died unexpectedly ({status})"),
    )
}
