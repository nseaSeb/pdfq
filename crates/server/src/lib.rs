//! PDFQ server. Typst never runs in this process: compilations are delegated to
//! supervised `pdfq-worker` processes (see [`pool`]).

pub mod api;
pub mod config;
pub mod error;
pub mod jobs;
pub mod metrics;
pub mod pool;
mod request;
mod schema;
mod spawn;
pub mod validate;
pub mod webhook;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use crate::api::AppState;
use crate::config::Config;
use crate::jobs::{JobStore, StoreLimits};
use crate::pool::{Pool, PoolConfig};
use crate::webhook::{WebhookConfig, Webhooks};

/// Worker pool settings derived from the server configuration.
pub fn pool_config(config: &Config, worker_bin: PathBuf) -> PoolConfig {
    let mut worker_args: Vec<OsString> = Vec::new();
    for path in &config.font_paths {
        worker_args.extend(["--font-path".into(), path.into()]);
    }
    for path in &config.package_paths {
        worker_args.extend(["--package-path".into(), path.into()]);
    }
    for package in &config.allowed_packages {
        worker_args.extend(["--allow-package".into(), package.into()]);
    }
    if let Some(dir) = &config.template_dir {
        worker_args.extend(["--template-dir".into(), dir.into()]);
    }
    PoolConfig {
        worker_bin,
        worker_args,
        workers: config
            .workers
            .unwrap_or_else(|| PoolConfig::default().workers),
        jobs_per_worker: config.jobs_per_worker,
        queue_size: config.queue_size,
        wall_timeout: config.wall_timeout,
        queue_timeout: config.queue_timeout,
        cpu_timeout: config.cpu_timeout,
        memory_limit: config.memory_limit,
        worker_open_files: config.worker_open_files,
        max_output_size: config.max_output_size,
    }
}

/// Starts the worker pool and returns the application state. Requires a Tokio runtime.
pub fn start(config: Config, worker_bin: PathBuf) -> AppState {
    let pool_config = pool_config(&config, worker_bin);
    let admission = Arc::new(tokio::sync::Semaphore::new(
        pool_config.workers.max(1) + pool_config.queue_size.max(1),
    ));
    let pool = Pool::start(pool_config);
    let jobs = Arc::new(JobStore::new(StoreLimits {
        ttl: config.job_ttl,
        max_jobs: config.max_jobs,
        max_bytes: config.max_stored_size,
    }));
    // Expired jobs are also purged on access; this frees memory when idle.
    let sweeper = Arc::downgrade(&jobs);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            tick.tick().await;
            let Some(jobs) = sweeper.upgrade() else { break };
            jobs.purge_expired();
        }
    });
    let webhooks = Webhooks::new(WebhookConfig {
        secret: config.webhook_secret.clone(),
        allowed_hosts: config.webhook_allowed_hosts.clone(),
        allow_private: config.webhook_allow_private,
        allow_http: config.webhook_allow_http,
        timeout: config.webhook_timeout,
        retries: config.webhook_retries,
        retry_delay: config.webhook_retry_delay,
    });
    AppState {
        config: Arc::new(config),
        pool: Arc::new(pool),
        admission,
        schemas: Default::default(),
        jobs,
        webhooks,
        rejected: Default::default(),
    }
}
