//! HTTP mapping of [`JobError`].

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use pdfq_protocol::{ErrorKind, JobError};
use serde::Serialize;

/// A [`JobError`] returned to an HTTP client as `{"error": {...}}`.
#[derive(Debug)]
pub struct ApiError(pub JobError);

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self(JobError::new(kind, message))
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::InvalidInput, message)
    }

    pub fn too_large(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::PayloadTooLarge, message)
    }
}

impl From<JobError> for ApiError {
    fn from(err: JobError) -> Self {
        Self(err)
    }
}

/// HTTP status of each error kind. Document-caused failures are 4xx on purpose:
/// they are deterministic and must not trigger automatic retries.
pub fn status(kind: ErrorKind) -> StatusCode {
    match kind {
        ErrorKind::InvalidInput | ErrorKind::SchemaViolation => StatusCode::BAD_REQUEST,
        ErrorKind::NotFound => StatusCode::NOT_FOUND,
        ErrorKind::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ErrorKind::CompileError
        | ErrorKind::PackageNotAllowed
        | ErrorKind::TooManyPages
        | ErrorKind::OutputTooLarge
        | ErrorKind::Timeout
        | ErrorKind::MemoryLimit => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorKind::QueueFull => StatusCode::TOO_MANY_REQUESTS,
        ErrorKind::WorkerCrashed => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[derive(Serialize)]
struct Body<'a> {
    error: &'a JobError,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (status(self.0.kind), Json(Body { error: &self.0 })).into_response();
        if self.0.kind == ErrorKind::QueueFull {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}
