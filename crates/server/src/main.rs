//! `pdfq`: the PDFQ HTTP server.

use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use pdfq_server::config::Config;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "Typst to PDF compilation server")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    config: Config,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server (default).
    Serve,
    /// Exit 0 if the local server answers /health (for container health checks).
    Healthcheck,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Serve) {
        Command::Serve => serve(cli.config),
        Command::Healthcheck => healthcheck(cli.config.bind),
    }
}

fn serve(config: Config) -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("PDFQ_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let worker_bin = config.worker_bin.clone().unwrap_or_else(default_worker_bin);
    if !worker_bin.is_file() {
        tracing::error!(path = %worker_bin.display(), "pdfq-worker not found (set PDFQ_WORKER_BIN)");
        return ExitCode::FAILURE;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build();
    let Ok(runtime) = runtime else {
        return ExitCode::FAILURE;
    };
    runtime.block_on(async move {
        let bind = config.bind;
        let state = pdfq_server::start(config, worker_bin);
        let app = pdfq_server::api::router(state);
        let listener = match tokio::net::TcpListener::bind(bind).await {
            Ok(listener) => listener,
            Err(err) => {
                tracing::error!(%bind, "cannot listen: {err}");
                return ExitCode::FAILURE;
            }
        };
        tracing::info!(%bind, "pdfq listening");
        match axum::serve(listener, app)
            .with_graceful_shutdown(shutdown())
            .await
        {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                tracing::error!("server error: {err}");
                ExitCode::FAILURE
            }
        }
    })
}

fn default_worker_bin() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("pdfq-worker")))
        .unwrap_or_else(|| PathBuf::from("pdfq-worker"))
}

async fn shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = ctrl_c => {}
        _ = term.recv() => {}
    }
    tracing::info!("shutting down");
}

fn healthcheck(bind: SocketAddr) -> ExitCode {
    // Connect where the server listens; loopback when it listens on all interfaces.
    let addr = match bind.ip() {
        ip if ip.is_unspecified() && ip.is_ipv6() => {
            SocketAddr::from((Ipv6Addr::LOCALHOST, bind.port()))
        }
        ip if ip.is_unspecified() => SocketAddr::from((Ipv4Addr::LOCALHOST, bind.port())),
        _ => bind,
    };
    let check = || -> std::io::Result<bool> {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")?;
        let mut head = [0u8; 12];
        stream.read_exact(&mut head)?;
        Ok(head.starts_with(b"HTTP/1.1 200"))
    };
    match check() {
        Ok(true) => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
