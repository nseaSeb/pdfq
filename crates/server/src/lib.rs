//! PDFQ server. Typst never runs in this process: compilations are delegated to
//! supervised `pdfq-worker` processes (see [`pool`]).

pub mod api;
pub mod config;
pub mod error;
pub mod pool;
mod request;
mod schema;
mod spawn;
pub mod validate;

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

use crate::api::AppState;
use crate::config::Config;
use crate::pool::{Pool, PoolConfig};

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
    AppState {
        config: Arc::new(config),
        pool: Arc::new(pool),
        admission,
        schemas: Default::default(),
    }
}
