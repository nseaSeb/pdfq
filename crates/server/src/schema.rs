//! Compiled `schema.json` validators, cached per template and refreshed when
//! the file's modification time changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use pdfq_protocol::ErrorKind;
use serde_json::Value;

use crate::error::ApiError;

type Entry = (SystemTime, Arc<jsonschema::Validator>);

#[derive(Default)]
pub struct SchemaCache {
    entries: Mutex<HashMap<PathBuf, Entry>>,
}

impl SchemaCache {
    /// The validator for the template in `dir`, if it has a `schema.json`.
    pub fn get(&self, dir: &Path) -> Result<Option<Arc<jsonschema::Validator>>, ApiError> {
        let path = dir.join("schema.json");
        let Some(modified) = std::fs::metadata(&path)
            .ok()
            .and_then(|m| m.modified().ok())
        else {
            self.entries.lock().unwrap().remove(&path);
            return Ok(None);
        };
        if let Some((cached_at, validator)) = self.entries.lock().unwrap().get(&path)
            && *cached_at == modified
        {
            return Ok(Some(validator.clone()));
        }
        let validator = Arc::new(compile(&path)?);
        self.entries
            .lock()
            .unwrap()
            .insert(path, (modified, validator.clone()));
        Ok(Some(validator))
    }
}

fn compile(path: &Path) -> Result<jsonschema::Validator, ApiError> {
    let broken = |e: String| {
        tracing::error!(path = %path.display(), "invalid template schema: {e}");
        ApiError::new(
            ErrorKind::WorkerCrashed,
            "the template's schema.json is invalid",
        )
    };
    let schema: Value = std::fs::read(path)
        .map_err(|e| e.to_string())
        .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()))
        .map_err(broken)?;
    jsonschema::validator_for(&schema).map_err(|e| broken(e.to_string()))
}
