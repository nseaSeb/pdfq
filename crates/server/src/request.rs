//! Parsing `POST /compile` and `POST /jobs` requests into a worker [`Job`].

use std::collections::BTreeMap;
use std::path::Path;

use axum::extract::Multipart;
use axum::extract::multipart::Field;
use axum::http::StatusCode;
use pdfq_protocol::{Date, Diagnostic, ErrorKind, Job, JobError, JobOptions, JobSource, Severity};
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::error::ApiError;
use crate::schema::SchemaCache;
use crate::validate::{Files, Limits, is_valid_template_name, normalize_path};

/// Fields described in the spec but not implemented yet.
const NOT_YET: &[&str] = &["attachment", "attach_data", "facturx", "facturx_profile"];

/// JSON body, for template mode without uploads.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JsonRequest {
    pub template: String,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(default)]
    pub input: BTreeMap<String, String>,
    #[serde(default)]
    pub pdf_standard: OneOrMany,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub strict: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

/// A request being assembled, whatever its encoding.
struct Builder<'a> {
    config: &'a Config,
    schemas: &'a SchemaCache,
    template: Option<String>,
    files: Files,
    assets: Files,
    archive: bool,
    file_parts: bool,
    entrypoint: Option<String>,
    /// `data` as received and parsed.
    data: Option<(String, Value)>,
    strict_set: bool,
    inputs: Vec<(String, String)>,
    data_size: u64,
    pdf_standards: Vec<String>,
    date: Option<Date>,
    strict: bool,
}

impl<'a> Builder<'a> {
    fn new(config: &'a Config, schemas: &'a SchemaCache) -> Self {
        let limits = Limits {
            max_file_size: config.max_file_size,
            max_upload_size: config.max_upload_size,
            max_files: config.max_files,
            max_image_pixels: config.max_image_pixels,
        };
        Self {
            config,
            schemas,
            strict_set: false,
            template: None,
            files: Files::new(limits),
            assets: Files::new(limits),
            archive: false,
            file_parts: false,
            entrypoint: None,
            data: None,
            inputs: Vec::new(),
            data_size: 0,
            pdf_standards: Vec::new(),
            date: None,
            strict: false,
        }
    }

    fn count_data(&mut self, len: usize) -> Result<(), ApiError> {
        self.data_size += len as u64;
        if self.data_size > self.config.max_data_size {
            return Err(ApiError::too_large(format!(
                "`data` and `input.*` exceed {} bytes",
                self.config.max_data_size
            )));
        }
        Ok(())
    }

    fn set_data(&mut self, json: String) -> Result<(), ApiError> {
        self.count_data(json.len())?;
        let value = serde_json::from_str::<Value>(&json)
            .map_err(|e| ApiError::invalid(format!("`data` is not valid JSON: {e}")))?;
        self.data = Some((json, value));
        Ok(())
    }

    fn set_data_value(&mut self, value: Value) -> Result<(), ApiError> {
        let json = value.to_string();
        self.count_data(json.len())?;
        self.data = Some((json, value));
        Ok(())
    }

    fn set_date(&mut self, value: &str) -> Result<(), ApiError> {
        if self.date.replace(parse_date(value)?).is_some() {
            return Err(ApiError::invalid("`date` given more than once"));
        }
        Ok(())
    }

    fn set_strict(&mut self, value: bool) -> Result<(), ApiError> {
        if std::mem::replace(&mut self.strict_set, true) {
            return Err(ApiError::invalid("`strict` given more than once"));
        }
        self.strict = value;
        Ok(())
    }

    fn add_input(&mut self, key: &str, value: String) -> Result<(), ApiError> {
        let valid = !key.is_empty()
            && key != "data"
            && key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !valid {
            return Err(ApiError::invalid(format!("invalid input name `{key}`")));
        }
        if self.inputs.iter().any(|(k, _)| k == key) {
            return Err(ApiError::invalid(format!(
                "`input.{key}` given more than once"
            )));
        }
        self.count_data(key.len() + value.len())?;
        self.inputs.push((key.to_string(), value));
        Ok(())
    }

    fn set_once(slot: &mut Option<String>, name: &str, value: String) -> Result<(), ApiError> {
        if slot.replace(value).is_some() {
            return Err(ApiError::invalid(format!("`{name}` given more than once")));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Job, ApiError> {
        let sources = [self.template.is_some(), self.file_parts, self.archive];
        match sources.iter().filter(|&&s| s).count() {
            0 => {
                return Err(ApiError::invalid(
                    "missing source: give `template`, `file` or `archive`",
                ));
            }
            1 => {}
            _ => {
                return Err(ApiError::invalid(
                    "give only one of `template`, `file` or `archive`",
                ));
            }
        }

        let source = if let Some(name) = self.template.take() {
            if self.entrypoint.is_some() {
                return Err(ApiError::invalid(
                    "`entrypoint` is not allowed with `template`",
                ));
            }
            let dir = template_dir(self.config, &name)?;
            check_schema(self.schemas, &dir, self.data.as_ref().map(|(_, v)| v))?;
            JobSource::Template {
                name,
                assets: self.assets.into_vec(),
            }
        } else {
            if !self.assets.is_empty() {
                return Err(ApiError::invalid("`asset` is only allowed with `template`"));
            }
            let entrypoint = normalize_path(self.entrypoint.as_deref().unwrap_or("main.typ"))?;
            if !entrypoint.ends_with(".typ") {
                return Err(ApiError::invalid("`entrypoint` must be a .typ file"));
            }
            if !self.files.contains(&entrypoint) {
                return Err(ApiError::invalid(format!(
                    "entrypoint `{entrypoint}` is missing"
                )));
            }
            JobSource::Files {
                files: self.files.into_vec(),
                entrypoint,
            }
        };

        let mut inputs = self.inputs;
        if let Some((json, _)) = self.data {
            inputs.push(("data".into(), json));
        }
        Ok(Job {
            source,
            inputs,
            options: JobOptions {
                max_pages: self.config.max_pages,
                max_file_size: self.config.max_file_size,
                max_output_size: self.config.max_output_size,
                pdf_standards: self.pdf_standards,
                date: self.date,
                strict: self.strict,
            },
        })
    }
}

fn template_dir(config: &Config, name: &str) -> Result<std::path::PathBuf, ApiError> {
    if !is_valid_template_name(name) {
        return Err(ApiError::invalid(format!("invalid template name `{name}`")));
    }
    let not_found = || ApiError::new(ErrorKind::NotFound, format!("template `{name}` not found"));
    let dir = config
        .template_dir
        .as_ref()
        .ok_or_else(not_found)?
        .join(name);
    if !dir.join("main.typ").is_file() {
        return Err(not_found());
    }
    Ok(dir)
}

/// Validates `data` against the template's optional `schema.json`.
fn check_schema(schemas: &SchemaCache, dir: &Path, data: Option<&Value>) -> Result<(), ApiError> {
    let Some(validator) = schemas.get(dir)? else {
        return Ok(());
    };
    let empty = Value::Object(Default::default());
    let instance = data.unwrap_or(&empty);
    let diagnostics: Vec<_> = validator
        .iter_errors(instance)
        .take(20)
        .map(|e| {
            let pointer = e.instance_path().to_string();
            let pointer = if pointer.is_empty() {
                "/".to_string()
            } else {
                pointer
            };
            Diagnostic {
                severity: Severity::Error,
                message: format!("{pointer}: {e}"),
                file: None,
                line: None,
                column: None,
                hints: Vec::new(),
            }
        })
        .collect();
    if diagnostics.is_empty() {
        return Ok(());
    }
    Err(ApiError(JobError {
        diagnostics,
        ..JobError::new(
            ErrorKind::SchemaViolation,
            "`data` does not match the template schema",
        )
    }))
}

fn parse_date(value: &str) -> Result<Date, ApiError> {
    let value = value.trim();
    let invalid = || ApiError::invalid(format!("invalid date `{value}`, expected YYYY-MM-DD"));
    let mut parts = value.split('-');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(invalid());
    };
    let digits = |s: &str, len: usize| s.len() == len && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(y, 4) || !digits(m, 2) || !digits(d, 2) {
        return Err(invalid());
    }
    let year: i32 = y.parse().map_err(|_| invalid())?;
    let month: u8 = m.parse().map_err(|_| invalid())?;
    let day: u8 = d.parse().map_err(|_| invalid())?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(invalid());
    }
    Ok(Date { year, month, day })
}

fn parse_bool(name: &str, value: &str) -> Result<bool, ApiError> {
    match value {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(ApiError::invalid(format!("`{name}` must be true or false"))),
    }
}

/// Reads a multipart field with a size cap, without buffering beyond it.
async fn read_capped(mut field: Field<'_>, cap: u64, name: &str) -> Result<Vec<u8>, ApiError> {
    let mut data = Vec::new();
    while let Some(chunk) = field.chunk().await.map_err(multipart_error)? {
        if (data.len() + chunk.len()) as u64 > cap {
            return Err(ApiError::too_large(format!("`{name}` exceeds {cap} bytes")));
        }
        data.extend_from_slice(&chunk);
    }
    Ok(data)
}

async fn read_text(field: Field<'_>, cap: u64, name: &str) -> Result<String, ApiError> {
    let data = read_capped(field, cap, name).await?;
    String::from_utf8(data).map_err(|_| ApiError::invalid(format!("`{name}` is not UTF-8")))
}

fn multipart_error(err: axum::extract::multipart::MultipartError) -> ApiError {
    if err.status() == StatusCode::PAYLOAD_TOO_LARGE {
        ApiError::too_large("the request body is too large")
    } else {
        ApiError::invalid(format!("invalid multipart body: {}", err.body_text()))
    }
}

/// Builds a job from a `multipart/form-data` body.
pub async fn from_multipart(
    mut multipart: Multipart,
    config: &Config,
    schemas: &SchemaCache,
    extra: &mut (dyn FnMut(&str, String) -> Result<bool, ApiError> + Send),
) -> Result<Job, ApiError> {
    let mut b = Builder::new(config, schemas);
    let text_cap = config.max_data_size;
    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" | "asset" => {
                let path = field.file_name().map(str::to_string).ok_or_else(|| {
                    ApiError::invalid(format!("`{name}` part without a filename"))
                })?;
                let cap = config
                    .max_file_size
                    .min(b.files.remaining().min(b.assets.remaining()));
                let data = read_capped(field, cap.max(1), &path).await?;
                b.file_parts |= name == "file";
                let target = if name == "file" {
                    &mut b.files
                } else {
                    &mut b.assets
                };
                target.add(&path, data)?;
            }
            "archive" => {
                if b.archive {
                    return Err(ApiError::invalid("`archive` given more than once"));
                }
                let data = read_capped(field, config.max_upload_size, "archive").await?;
                b.archive = true;
                let mut files = std::mem::replace(
                    &mut b.files,
                    Files::new(Limits {
                        max_file_size: 0,
                        max_upload_size: 0,
                        max_files: 0,
                        max_image_pixels: 0,
                    }),
                );
                // Zip extraction is CPU-bound: keep it off the async runtime.
                files = tokio::task::spawn_blocking(move || files.add_zip(&data).map(|()| files))
                    .await
                    .map_err(|_| ApiError::invalid("invalid zip archive"))??;
                b.files = files;
            }
            "template" => {
                let value = read_text(field, 256, &name).await?;
                Builder::set_once(&mut b.template, "template", value)?;
            }
            "entrypoint" => {
                let value = read_text(field, 256, &name).await?;
                Builder::set_once(&mut b.entrypoint, "entrypoint", value)?;
            }
            "data" => {
                if b.data.is_some() {
                    return Err(ApiError::invalid("`data` given more than once"));
                }
                let value = read_text(field, text_cap, &name).await?;
                b.set_data(value)?;
            }
            "pdf_standard" => {
                let value = read_text(field, 64, &name).await?;
                b.pdf_standards.push(value);
            }
            "date" => b.set_date(&read_text(field, 32, &name).await?)?,
            "strict" => b.set_strict(parse_bool(
                "strict",
                read_text(field, 8, &name).await?.trim(),
            )?)?,
            _ if name.starts_with("input.") => {
                let value = read_text(field, text_cap, &name).await?;
                b.add_input(&name["input.".len()..], value)?;
            }
            _ if NOT_YET.contains(&name.as_str())
                || name.starts_with("attachment_relationship.") =>
            {
                return Err(ApiError::invalid(format!("`{name}` is not supported yet")));
            }
            _ => {
                let value = read_text(field, 4096, &name).await?;
                if !extra(&name, value)? {
                    return Err(ApiError::invalid(format!("unknown field `{name}`")));
                }
            }
        }
    }
    b.finish()
}

/// Builds a job from a JSON body (template mode).
pub fn from_json(
    body: JsonRequest,
    config: &Config,
    schemas: &SchemaCache,
) -> Result<Job, ApiError> {
    let mut b = Builder::new(config, schemas);
    b.template = Some(body.template);
    if let Some(data) = body.data {
        b.set_data_value(data)?;
    }
    for (key, value) in body.input {
        b.add_input(&key, value)?;
    }
    b.pdf_standards = match body.pdf_standard {
        OneOrMany::None => Vec::new(),
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    };
    if let Some(date) = body.date {
        b.set_date(&date)?;
    }
    b.strict = body.strict;
    b.finish()
}
