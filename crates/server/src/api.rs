//! HTTP routes.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use pdfq_protocol::{ErrorKind, JobError, TYPST_VERSION};
use serde_json::json;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::config::Config;
use crate::error::ApiError;
use crate::jobs::{JobStore, Pdf, WebhookState};
use crate::pool::{self, JobResult, Pool};
use crate::request::{self, JsonRequest, Parsed};
use crate::schema::SchemaCache;
use crate::webhook::{Delivery, Webhooks};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub pool: Arc<Pool>,
    /// Requests admitted at once (`workers + queue_size`). Taken before the body
    /// is read, so a burst of large uploads cannot exhaust the server's memory.
    pub admission: Arc<Semaphore>,
    pub schemas: Arc<SchemaCache>,
    pub jobs: Arc<JobStore>,
    pub webhooks: Webhooks,
    /// Requests refused by [`AppState::admit`].
    pub rejected: Arc<AtomicU64>,
}

impl AppState {
    /// Admits a request or fails with `queue_full` without reading its body.
    pub fn admit(&self) -> Result<OwnedSemaphorePermit, ApiError> {
        self.admission.clone().try_acquire_owned().map_err(|_| {
            self.rejected.fetch_add(1, Ordering::Relaxed);
            ApiError::new(ErrorKind::QueueFull, "the server is busy, retry later")
        })
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
        .route("/jobs", post(create_job))
        .route("/jobs/{id}", get(job_status))
        .route("/jobs/{id}/pdf", get(job_pdf))
        .route("/metrics", get(metrics))
        .route("/health", get(health))
        .route("/ready", get(ready))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(state)
}

/// Parses a compile request, multipart or JSON.
pub async fn parse_job(state: &AppState, request: Request) -> Result<Parsed, ApiError> {
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
        request::from_multipart(multipart, &state.config, &state.schemas).await
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
    let Parsed { job, callback_url } = parse_job(&state, request).await?;
    if callback_url.is_some() {
        return Err(ApiError::invalid(
            "`callback_url` is only allowed on POST /jobs",
        ));
    }
    let start = Instant::now();
    let receiver = state.pool.submit(job)?;
    let output = wait_result(&state.config, receiver).await?;

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

async fn create_job(State(state): State<AppState>, request: Request) -> Result<Response, ApiError> {
    let parsed = {
        // Admission only bounds request parsing here; the queue bounds the jobs.
        let _permit = state.admit()?;
        parse_job(&state, request).await?
    };
    let callback = parsed
        .callback_url
        .as_deref()
        .map(|u| state.webhooks.validate(u))
        .transpose()?;

    let id = state.jobs.create(callback.is_some())?;
    let jobs = state.jobs.clone();
    let on_start = Box::new(move || jobs.set_running(id));
    let receiver = match state.pool.submit_with(parsed.job, Some(on_start)) {
        Ok(receiver) => receiver,
        Err(err) => {
            state.jobs.remove(id);
            return Err(err.into());
        }
    };

    let (jobs, webhooks, config) = (
        state.jobs.clone(),
        state.webhooks.clone(),
        state.config.clone(),
    );
    tokio::spawn(async move {
        // The webhook reports what the store recorded (a PDF may not fit in it).
        let stored = match wait_result(&config, receiver).await {
            Ok(output) => {
                let pdf = Bytes::from(output.pdf);
                jobs.succeed(id, pdf.clone(), output.pages, output.warnings)
                    .map(|()| pdf)
            }
            Err(error) => {
                jobs.fail(id, error.clone());
                Err(error)
            }
        };
        let (status, content_type, body) = match stored {
            Ok(pdf) => ("succeeded", "application/pdf", pdf),
            Err(error) => {
                let body = serde_json::to_vec(&json!({ "error": error })).unwrap_or_default();
                ("failed", "application/json", Bytes::from(body))
            }
        };
        let delivery = callback.map(|url| {
            let delivery = Delivery {
                job_id: id.to_string(),
                status,
                content_type,
                body,
            };
            (url, delivery)
        });
        if let Some((url, delivery)) = delivery {
            let report = webhooks.deliver(url, delivery).await;
            let state = if report.delivered {
                WebhookState::Delivered
            } else {
                WebhookState::Failed
            };
            jobs.set_webhook(id, state, report.attempts);
        }
    });

    let location = format!("/jobs/{id}");
    let body = json!({ "job_id": id.to_string(), "status_url": location });
    let mut response = (StatusCode::ACCEPTED, Json(body)).into_response();
    if let Ok(value) = HeaderValue::from_str(&location) {
        response.headers_mut().insert(header::LOCATION, value);
    }
    Ok(response)
}

/// Waits for a job: queue wait plus compilation, with slack for worker respawns.
/// Dropping the receiver on timeout cancels a job that is still queued.
async fn wait_result(config: &Config, receiver: oneshot::Receiver<JobResult>) -> JobResult {
    let deadline = config
        .queue_timeout
        .saturating_add(config.wall_timeout)
        .saturating_add(Duration::from_secs(5));
    match tokio::time::timeout(deadline, receiver).await {
        Ok(result) => result.unwrap_or_else(|_| Err(pool::stopped())),
        Err(_) => Err(JobError::new(
            ErrorKind::QueueFull,
            "timed out waiting for a worker",
        )),
    }
}

fn job_id(raw: &str) -> Result<uuid::Uuid, ApiError> {
    uuid::Uuid::parse_str(raw).map_err(|_| not_found())
}

fn not_found() -> ApiError {
    ApiError::new(ErrorKind::NotFound, "unknown or expired job")
}

async fn job_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<crate::jobs::JobView>, ApiError> {
    state
        .jobs
        .view(job_id(&id)?)
        .map(Json)
        .ok_or_else(not_found)
}

async fn job_pdf(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    match state.jobs.pdf(job_id(&id)?).ok_or_else(not_found)? {
        Pdf::Ready(pdf) => {
            let mut response = Body::from(pdf).into_response();
            response.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/pdf"),
            );
            Ok(response)
        }
        Pdf::NotFinished => Err(ApiError::new(
            ErrorKind::NotFinished,
            "the job has not finished yet",
        )),
        // Always 422, whatever the job's error: retrying this request cannot help.
        Pdf::Failed(error) => Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": error })),
        )
            .into_response()),
    }
}

/// Never goes through admission: scraping works while the server is saturated.
async fn metrics(State(state): State<AppState>) -> Response {
    let (delivered, failed) = state.webhooks.counts();
    let body = crate::metrics::render(
        state.pool.stats(),
        &crate::metrics::Extra {
            queued: state.pool.queue_len() as u64,
            admission_rejected: state.rejected.load(Ordering::Relaxed),
            async_jobs: state.jobs.len() as u64,
            webhooks_delivered: delivered,
            webhooks_failed: failed,
        },
    );
    ([(header::CONTENT_TYPE, crate::metrics::CONTENT_TYPE)], body).into_response()
}

/// Liveness: 503 while no worker process is alive (startup, or workers that
/// cannot start). Busy workers count as alive: a full queue is not unhealthy.
async fn health(State(state): State<AppState>) -> Response {
    let workers = state.pool.stats().ready_workers.load(Ordering::Relaxed);
    let (status, text) = if workers > 0 {
        (StatusCode::OK, "ok")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no_worker")
    };
    let body = json!({
        "status": text,
        "workers": workers,
        "version": env!("CARGO_PKG_VERSION"),
        "typst_version": TYPST_VERSION,
    });
    (status, Json(body)).into_response()
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
