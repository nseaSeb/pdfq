//! HTTP routes.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pdfq_protocol::{ErrorKind, Job, TYPST_VERSION};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::Config;
use crate::error::ApiError;
use crate::pool::Pool;
use crate::request::{self, JsonRequest};
use crate::schema::SchemaCache;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: Arc<Pool>,
    /// Requests admitted at once (`workers + queue_size`). Taken before the body
    /// is read, so a burst of large uploads cannot exhaust the server's memory.
    pub admission: Arc<Semaphore>,
    pub schemas: Arc<SchemaCache>,
}

impl AppState {
    /// Admits a request or fails with `queue_full` without reading its body.
    pub fn admit(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        self.admission
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::new(ErrorKind::QueueFull, "the server is busy, retry later"))
    }
}

pub fn router(state: AppState) -> Router {
    // Multipart framing adds overhead on top of the upload limit.
    let body_limit = usize::try_from(state.config.max_upload_size)
        .unwrap_or(usize::MAX)
        .saturating_add(state.config.max_data_size as usize)
        .saturating_add(1 << 20);
    Router::new()
        .route("/compile", post(compile))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

/// Parses a compile request, multipart or JSON.
pub async fn parse_job(
    state: &AppState,
    request: Request,
    extra: &mut (dyn FnMut(&str, String) -> Result<bool, ApiError> + Send),
) -> Result<Job, ApiError> {
    let content_type = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if content_type.starts_with("multipart/form-data") {
        let multipart = Multipart::from_request(request, state)
            .await
            .map_err(|e| ApiError::invalid(e.body_text()))?;
        request::from_multipart(multipart, &state.config, &state.schemas, extra).await
    } else if content_type.starts_with("application/json") {
        let Json(body) = Json::<JsonRequest>::from_request(request, state)
            .await
            .map_err(|e| {
                if e.status() == StatusCode::PAYLOAD_TOO_LARGE {
                    ApiError::too_large("the request body is too large")
                } else {
                    ApiError::invalid(e.body_text())
                }
            })?;
        request::from_json(body, &state.config, &state.schemas)
    } else {
        Err(ApiError::invalid(
            "expected a multipart/form-data or application/json body",
        ))
    }
}

async fn compile(State(state): State<AppState>, request: Request) -> Result<Response, ApiError> {
    let _permit = state.admit()?;
    let job = parse_job(&state, request, &mut |_, _| Ok(false)).await?;
    let start = Instant::now();
    let receiver = state.pool.submit(job)?;
    // Queue wait + compilation, with some slack for worker respawns. Dropping the
    // receiver cancels a job that is still queued.
    let deadline = state
        .config
        .queue_timeout
        .saturating_add(state.config.wall_timeout)
        .saturating_add(Duration::from_secs(5));
    let output = match tokio::time::timeout(deadline, receiver).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => {
            return Err(ApiError::new(
                ErrorKind::WorkerCrashed,
                "the worker pool stopped",
            ));
        }
        Err(_) => {
            return Err(ApiError::new(
                ErrorKind::QueueFull,
                "timed out waiting for a worker",
            ));
        }
    };

    let mut response = output.pdf.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/pdf"),
    );
    headers.insert("x-pdfq-pages", output.pages.into());
    headers.insert("x-pdfq-warnings", (output.warnings.len() as u64).into());
    headers.insert(
        "x-pdfq-duration-ms",
        (start.elapsed().as_millis() as u64).into(),
    );
    Ok(response)
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "typst_version": TYPST_VERSION,
    }))
}

async fn ready(State(state): State<AppState>) -> Response {
    let stats = state.pool.stats();
    let workers = stats
        .ready_workers
        .load(std::sync::atomic::Ordering::Relaxed);
    let queued = state.pool.queue_len();
    let ready = workers > 0 && queued < state.config.queue_size.max(1);
    let status = if ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = json!({ "ready": ready, "workers": workers, "queued": queued });
    (status, Json(body)).into_response()
}
