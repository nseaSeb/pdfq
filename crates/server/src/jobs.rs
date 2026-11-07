//! In-memory store of asynchronous jobs. Bounded in number and in stored
//! bytes; finished jobs expire after a TTL. Lost on restart (by design).

use bytes::Bytes;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use pdfq_protocol::{Diagnostic, ErrorKind, JobError};
use serde::Serialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WebhookState {
    Pending,
    Delivered,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebhookStatus {
    pub state: WebhookState,
    pub attempts: u32,
}

/// The JSON returned by `GET /jobs/{id}`.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: String,
    pub status: Status,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub pages: Option<u32>,
    pub warnings: Vec<Diagnostic>,
    pub error: Option<JobError>,
    pub webhook: Option<WebhookStatus>,
}

struct Entry {
    view: JobView,
    pdf: Option<Bytes>,
    created: Instant,
    expires: Option<Instant>,
}

#[derive(Debug, Clone, Copy)]
pub struct StoreLimits {
    pub ttl: Duration,
    pub max_jobs: usize,
    pub max_bytes: u64,
}

pub struct JobStore {
    limits: StoreLimits,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    jobs: HashMap<Uuid, Entry>,
    bytes: u64,
}

/// What `GET /jobs/{id}/pdf` can return.
pub enum Pdf {
    Ready(Bytes),
    NotFinished,
    Failed(JobError),
}

fn now_rfc3339() -> String {
    OffsetDateTime::from(SystemTime::now())
        .format(&Rfc3339)
        .unwrap_or_default()
}

impl JobStore {
    pub fn new(limits: StoreLimits) -> Self {
        Self {
            limits,
            inner: Mutex::default(),
        }
    }

    /// Registers a new queued job, evicting the oldest finished jobs if needed.
    pub fn create(&self, with_webhook: bool) -> Result<Uuid, JobError> {
        let mut inner = self.inner.lock().unwrap();
        let max_jobs = self.limits.max_jobs.max(1);
        if inner.jobs.len() >= max_jobs {
            self.purge(&mut inner);
            if !evict_oldest(&mut inner, |inner| inner.jobs.len() < max_jobs) {
                return Err(JobError::new(
                    ErrorKind::QueueFull,
                    "too many jobs in progress, retry later",
                ));
            }
        }
        let id = Uuid::new_v4();
        let view = JobView {
            id: id.to_string(),
            status: Status::Queued,
            created_at: now_rfc3339(),
            finished_at: None,
            pages: None,
            warnings: Vec::new(),
            error: None,
            webhook: with_webhook.then_some(WebhookStatus {
                state: WebhookState::Pending,
                attempts: 0,
            }),
        };
        inner.jobs.insert(
            id,
            Entry {
                view,
                pdf: None,
                created: Instant::now(),
                expires: None,
            },
        );
        Ok(id)
    }

    /// Forgets a job that could not be queued.
    pub fn remove(&self, id: Uuid) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(entry) = inner.jobs.remove(&id) {
            inner.bytes -= entry.pdf.map_or(0, |p| p.len() as u64);
        }
    }

    pub fn set_running(&self, id: Uuid) {
        if let Some(entry) = self.inner.lock().unwrap().jobs.get_mut(&id)
            && entry.view.status == Status::Queued
        {
            entry.view.status = Status::Running;
        }
    }

    /// Stores the PDF. When it cannot be stored the job fails instead, and the
    /// error is returned so the webhook reports the same outcome.
    pub fn succeed(
        &self,
        id: Uuid,
        pdf: Bytes,
        pages: u32,
        warnings: Vec<Diagnostic>,
    ) -> Result<(), JobError> {
        let mut inner = self.inner.lock().unwrap();
        let size = pdf.len() as u64;
        let max = self.limits.max_bytes;
        let stored = if size > max {
            Err(JobError::new(
                ErrorKind::OutputTooLarge,
                "the PDF is larger than PDFQ_MAX_STORED_SIZE",
            ))
        } else {
            if inner.bytes + size > max {
                self.purge(&mut inner);
                evict_oldest(&mut inner, |inner| inner.bytes + size <= max);
            }
            if inner.bytes + size <= max {
                Ok(())
            } else {
                // Only unfinished or undelivered jobs are left: transient.
                Err(JobError::new(
                    ErrorKind::QueueFull,
                    "the result store is full, resubmit later",
                ))
            }
        };
        let Some(entry) = inner.jobs.get_mut(&id) else {
            return stored;
        };
        match &stored {
            Ok(()) => {
                entry.view.status = Status::Succeeded;
                entry.view.pages = Some(pages);
                entry.view.warnings = warnings;
                entry.pdf = Some(pdf);
            }
            Err(error) => {
                entry.view.status = Status::Failed;
                entry.view.error = Some(error.clone());
            }
        }
        self.finish(entry);
        if stored.is_ok() {
            inner.bytes += size;
        }
        stored
    }

    pub fn fail(&self, id: Uuid, error: JobError) {
        if let Some(entry) = self.inner.lock().unwrap().jobs.get_mut(&id) {
            entry.view.status = Status::Failed;
            entry.view.error = Some(error);
            self.finish(entry);
        }
    }

    /// Records the webhook outcome; the job's TTL starts now.
    pub fn set_webhook(&self, id: Uuid, state: WebhookState, attempts: u32) {
        if let Some(entry) = self.inner.lock().unwrap().jobs.get_mut(&id) {
            entry.view.webhook = Some(WebhookStatus { state, attempts });
            entry.expires = Some(Instant::now() + self.limits.ttl);
        }
    }

    pub fn view(&self, id: Uuid) -> Option<JobView> {
        let inner = self.inner.lock().unwrap();
        live(&inner, id).map(|e| e.view.clone())
    }

    pub fn pdf(&self, id: Uuid) -> Option<Pdf> {
        let inner = self.inner.lock().unwrap();
        let entry = live(&inner, id)?;
        Some(match (&entry.pdf, &entry.view.error) {
            (Some(pdf), _) => Pdf::Ready(pdf.clone()),
            (None, Some(error)) => Pdf::Failed(error.clone()),
            (None, None) => Pdf::NotFinished,
        })
    }

    /// Drops expired jobs. Called periodically and when the store is full.
    pub fn purge_expired(&self) {
        let mut inner = self.inner.lock().unwrap();
        self.purge(&mut inner);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().jobs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Marks a job finished. The TTL starts unless a webhook is still to be
    /// delivered: until then the job is neither evicted nor expired, so its PDF
    /// (shared with the delivery) stays within `max_bytes`.
    fn finish(&self, entry: &mut Entry) {
        entry.view.finished_at = Some(now_rfc3339());
        let pending = entry
            .view
            .webhook
            .as_ref()
            .is_some_and(|w| w.state == WebhookState::Pending);
        if !pending {
            entry.expires = Some(Instant::now() + self.limits.ttl);
        }
    }

    fn purge(&self, inner: &mut Inner) {
        let now = Instant::now();
        let mut freed = 0;
        inner.jobs.retain(|_, e| {
            let keep = e.expires.is_none_or(|t| t > now);
            if !keep {
                freed += e.pdf.as_ref().map_or(0, |p| p.len() as u64);
            }
            keep
        });
        inner.bytes -= freed;
    }
}

/// A job that is present and not expired (the sweeper may not have run yet).
fn live(inner: &Inner, id: Uuid) -> Option<&Entry> {
    let entry = inner.jobs.get(&id)?;
    entry
        .expires
        .is_none_or(|t| t > Instant::now())
        .then_some(entry)
}

/// Evicts evictable jobs (finished, webhook settled), oldest first, until `done`
/// returns true. Returns whether `done` was reached. One scan and one sort.
fn evict_oldest(inner: &mut Inner, done: impl Fn(&Inner) -> bool) -> bool {
    let mut candidates: Vec<(Instant, Uuid)> = inner
        .jobs
        .iter()
        .filter(|(_, e)| e.expires.is_some())
        .map(|(id, e)| (e.created, *id))
        .collect();
    candidates.sort_unstable();
    for (_, id) in candidates {
        if done(inner) {
            return true;
        }
        let entry = inner.jobs.remove(&id).expect("present");
        inner.bytes -= entry.pdf.map_or(0, |p| p.len() as u64);
    }
    done(inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(max_jobs: usize, max_bytes: u64, ttl: Duration) -> JobStore {
        JobStore::new(StoreLimits {
            ttl,
            max_jobs,
            max_bytes,
        })
    }

    #[test]
    fn tracks_the_lifecycle() {
        let s = store(10, 1000, Duration::from_secs(60));
        let id = s.create(false).unwrap();
        assert_eq!(s.view(id).unwrap().status, Status::Queued);
        assert!(matches!(s.pdf(id), Some(Pdf::NotFinished)));
        s.set_running(id);
        assert_eq!(s.view(id).unwrap().status, Status::Running);
        s.succeed(id, Bytes::from(vec![1, 2, 3]), 1, Vec::new())
            .unwrap();
        let view = s.view(id).unwrap();
        assert_eq!((view.status, view.pages), (Status::Succeeded, Some(1)));
        assert!(matches!(s.pdf(id), Some(Pdf::Ready(p)) if *p == vec![1, 2, 3]));
    }

    #[test]
    fn finished_jobs_expire() {
        let s = store(10, 1000, Duration::from_millis(20));
        let id = s.create(false).unwrap();
        s.fail(id, JobError::new(ErrorKind::CompileError, "x"));
        assert!(s.view(id).is_some());
        std::thread::sleep(Duration::from_millis(40));
        assert!(s.view(id).is_none());
    }

    #[test]
    fn evicts_finished_jobs_but_never_pending_ones() {
        let s = store(2, 1000, Duration::from_secs(60));
        let a = s.create(false).unwrap();
        let _b = s.create(false).unwrap();
        assert_eq!(s.create(false).unwrap_err().kind, ErrorKind::QueueFull);
        s.succeed(a, Bytes::from(vec![0; 10]), 1, Vec::new())
            .unwrap();
        s.create(false).unwrap();
        assert!(
            s.view(a).is_none(),
            "the finished job should have been evicted"
        );
    }

    #[test]
    fn bounds_stored_bytes() {
        let s = store(10, 100, Duration::from_secs(60));
        let a = s.create(false).unwrap();
        s.succeed(a, Bytes::from(vec![0; 80]), 1, Vec::new())
            .unwrap();
        let b = s.create(false).unwrap();
        s.succeed(b, Bytes::from(vec![0; 80]), 1, Vec::new())
            .unwrap();
        assert!(s.view(a).is_none(), "oldest result evicted to make room");
        assert_eq!(s.view(b).unwrap().status, Status::Succeeded);
        let c = s.create(false).unwrap();
        let err = s.succeed(c, Bytes::from(vec![0; 200]), 1, Vec::new());
        assert_eq!(err.unwrap_err().kind, ErrorKind::OutputTooLarge);
        assert_eq!(s.view(c).unwrap().status, Status::Failed);
        assert!(s.view(b).is_some(), "an oversized PDF evicts nothing");
    }

    #[test]
    fn jobs_with_a_pending_webhook_are_kept() {
        let s = store(1, 100, Duration::from_millis(20));
        let a = s.create(true).unwrap();
        s.succeed(a, Bytes::from(vec![0; 80]), 1, Vec::new())
            .unwrap();
        std::thread::sleep(Duration::from_millis(40));
        assert!(s.view(a).is_some(), "no expiry before delivery");
        assert_eq!(s.create(false).unwrap_err().kind, ErrorKind::QueueFull);
        s.set_webhook(a, WebhookState::Delivered, 1);
        assert_eq!(
            s.view(a).unwrap().webhook.unwrap().state,
            WebhookState::Delivered
        );
        s.create(false).unwrap();
        assert!(s.view(a).is_none(), "evictable once delivered");
    }

    #[test]
    fn full_store_of_undelivered_results_is_transient() {
        let s = store(10, 100, Duration::from_secs(60));
        let a = s.create(true).unwrap();
        s.succeed(a, Bytes::from(vec![0; 80]), 1, Vec::new())
            .unwrap();
        let b = s.create(false).unwrap();
        let err = s.succeed(b, Bytes::from(vec![0; 80]), 1, Vec::new());
        assert_eq!(err.unwrap_err().kind, ErrorKind::QueueFull);
    }
}
