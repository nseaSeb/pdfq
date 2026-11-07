//! Parsing `POST /compile` and `POST /jobs` requests into a worker [`Job`].

use std::collections::BTreeMap;
use std::path::Path;

use axum::extract::Multipart;
use axum::extract::multipart::Field;
use axum::http::StatusCode;
use pdfq_protocol::{
    Attachment, Date, Diagnostic, ErrorKind, FACTURX_FILENAME, FacturxProfile, Job, JobError,
    JobOptions, JobSource, Relationship, Severity, is_valid_attachment_name, is_valid_mime_type,
};
use serde::Deserialize;
use serde_json::Value;

use crate::config::Config;
use crate::error::ApiError;
use crate::schema::SchemaCache;
use crate::validate::{Files, Limits, is_valid_template_name, normalize_path};

/// Name of the attachment made from `data` with `attach_data`.
const DATA_FILENAME: &str = "data.json";

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
    #[serde(default)]
    pub callback_url: Option<String>,
    #[serde(default)]
    pub attach_data: bool,
    /// Factur-X CII XML.
    #[serde(default)]
    pub facturx: Option<String>,
    #[serde(default)]
    pub facturx_profile: Option<String>,
    /// Keyed by attachment name (`data.json`, `factur-x.xml`).
    #[serde(default)]
    pub attachment_relationship: BTreeMap<String, String>,
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
    callback_url: Option<String>,
    /// `data` as received and parsed.
    data: Option<(String, Value)>,
    strict_set: bool,
    inputs: Vec<(String, String)>,
    data_size: u64,
    pdf_standards: Vec<String>,
    date: Option<Date>,
    strict: bool,
    /// Uploaded attachments (named by their filename) and their MIME types.
    attachments: Files,
    mime_types: BTreeMap<String, String>,
    relationships: BTreeMap<String, Relationship>,
    attach_data: Option<bool>,
    facturx: Option<Vec<u8>>,
    facturx_profile: Option<String>,
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
            callback_url: None,
            template: None,
            files: Files::new(limits),
            assets: Files::new(limits),
            attachments: Files::new(limits),
            mime_types: BTreeMap::new(),
            relationships: BTreeMap::new(),
            attach_data: None,
            facturx: None,
            facturx_profile: None,
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

    /// Bytes still allowed for the whole request, all uploads together.
    fn remaining(&self) -> u64 {
        self.config.max_upload_size.saturating_sub(self.uploaded())
    }

    /// Uploaded bytes so far (files, assets, attachments, Factur-X XML).
    fn uploaded(&self) -> u64 {
        let facturx = self.facturx.as_ref().map_or(0, |x| x.len() as u64);
        self.files.total() + self.assets.total() + self.attachments.total() + facturx
    }

    fn add_attachment(
        &mut self,
        name: &str,
        content_type: Option<&str>,
        data: Vec<u8>,
    ) -> Result<(), ApiError> {
        if !is_valid_attachment_name(name) {
            return Err(ApiError::invalid(format!(
                "invalid attachment name `{name}`: use 1 to 128 letters, digits, `.`, `_`, `-` \
                 or spaces, not starting with `.`"
            )));
        }
        if name == DATA_FILENAME || name == FACTURX_FILENAME {
            return Err(ApiError::invalid(format!(
                "`{name}` is reserved: use `attach_data` or `facturx`"
            )));
        }
        let mime = mime_type(name, content_type)?;
        self.attachments.add(name, data)?;
        self.mime_types.insert(name.to_string(), mime);
        Ok(())
    }

    fn set_relationship(&mut self, name: &str, value: &str) -> Result<(), ApiError> {
        let relationship = Relationship::parse(value).ok_or_else(|| {
            ApiError::invalid(format!(
                "invalid relationship `{value}`: use source, data, alternative, supplement \
                 or unspecified"
            ))
        })?;
        if self
            .relationships
            .insert(name.to_string(), relationship)
            .is_some()
        {
            return Err(ApiError::invalid(format!(
                "`attachment_relationship.{name}` given more than once"
            )));
        }
        Ok(())
    }

    fn set_attach_data(&mut self, value: bool) -> Result<(), ApiError> {
        if self.attach_data.replace(value).is_some() {
            return Err(ApiError::invalid("`attach_data` given more than once"));
        }
        Ok(())
    }

    fn set_facturx(&mut self, xml: Vec<u8>) -> Result<(), ApiError> {
        if self.facturx.is_some() {
            return Err(ApiError::invalid("`facturx` given more than once"));
        }
        if xml.len() as u64 > self.config.max_file_size {
            return Err(ApiError::too_large(format!(
                "`facturx` exceeds {} bytes",
                self.config.max_file_size
            )));
        }
        if xml.len() as u64 > self.remaining() {
            return Err(ApiError::too_large(format!(
                "the request exceeds {} bytes",
                self.config.max_upload_size
            )));
        }
        // A sanity check, not a schema validation.
        let text =
            std::str::from_utf8(&xml).map_err(|_| ApiError::invalid("`facturx` is not UTF-8"))?;
        if !text.contains("CrossIndustryInvoice") {
            return Err(ApiError::invalid(
                "`facturx` is not a CII invoice (no CrossIndustryInvoice element)",
            ));
        }
        self.facturx = Some(xml);
        Ok(())
    }

    /// The attachments, the Factur-X profile and the final PDF standards.
    fn attachments(&mut self) -> Result<(Vec<Attachment>, Option<FacturxProfile>), ApiError> {
        let count = self.files.len()
            + self.assets.len()
            + self.attachments.len()
            + usize::from(self.facturx.is_some());
        if count > self.config.max_files {
            return Err(ApiError::too_large(format!(
                "too many files (limit {})",
                self.config.max_files
            )));
        }
        if self.uploaded() > self.config.max_upload_size {
            return Err(ApiError::too_large(format!(
                "the request exceeds {} bytes",
                self.config.max_upload_size
            )));
        }
        let files = self.attachments.take().into_vec();
        let mut relationships = std::mem::take(&mut self.relationships);
        let mut attachments: Vec<Attachment> = files
            .into_iter()
            .map(|f| Attachment {
                relationship: relationships
                    .remove(&f.path)
                    .unwrap_or(Relationship::Supplement),
                mime_type: self.mime_types.remove(&f.path).unwrap_or_default(),
                description: f.path.clone(),
                name: f.path,
                data: f.data,
            })
            .collect();

        if self.attach_data == Some(true) {
            let Some((json, _)) = &self.data else {
                return Err(ApiError::invalid("`attach_data` requires `data`"));
            };
            attachments.push(Attachment {
                name: DATA_FILENAME.into(),
                data: json.clone().into_bytes(),
                mime_type: "application/json".into(),
                relationship: relationships
                    .remove(DATA_FILENAME)
                    .unwrap_or(Relationship::Source),
                description: "Input data of the document".into(),
            });
        }

        let profile = match (self.facturx.take(), self.facturx_profile.take()) {
            (None, None) => None,
            (Some(_), None) => {
                return Err(ApiError::invalid("`facturx` requires `facturx_profile`"));
            }
            (None, Some(_)) => {
                return Err(ApiError::invalid("`facturx_profile` requires `facturx`"));
            }
            (Some(xml), Some(profile)) => {
                let profile = FacturxProfile::parse(&profile).ok_or_else(|| {
                    ApiError::invalid(format!(
                        "invalid facturx_profile `{profile}`: use minimum, basic-wl, basic, \
                         en16931 or extended"
                    ))
                })?;
                let relationship = relationships
                    .remove(FACTURX_FILENAME)
                    .unwrap_or(Relationship::Data);
                if !profile.allows_relationship(relationship) {
                    return Err(ApiError::invalid(
                        "invalid relationship for the Factur-X XML: data, source or alternative \
                         (only data for minimum and basic-wl)",
                    ));
                }
                // Factur-X is PDF/A-3: add it, and refuse any other PDF/A part.
                let archival: Vec<_> = self
                    .pdf_standards
                    .iter()
                    .filter(|s| s.starts_with("a-"))
                    .collect();
                if archival
                    .iter()
                    .any(|s| !matches!(s.as_str(), "a-3a" | "a-3b" | "a-3u"))
                {
                    return Err(ApiError::invalid("Factur-X requires PDF/A-3 (`a-3b`)"));
                }
                if archival.is_empty() {
                    self.pdf_standards.push("a-3b".into());
                }
                attachments.push(Attachment {
                    name: FACTURX_FILENAME.into(),
                    data: xml,
                    mime_type: "text/xml".into(),
                    relationship,
                    description: "Factur-X invoice".into(),
                });
                Some(profile)
            }
        };

        if let Some(name) = relationships.keys().next() {
            return Err(ApiError::invalid(format!(
                "`attachment_relationship.{name}` names no attachment"
            )));
        }
        Ok((attachments, profile))
    }

    fn set_once(slot: &mut Option<String>, name: &str, value: String) -> Result<(), ApiError> {
        if slot.replace(value).is_some() {
            return Err(ApiError::invalid(format!("`{name}` given more than once")));
        }
        Ok(())
    }

    fn finish(mut self) -> Result<Parsed, ApiError> {
        let callback_url = self.callback_url.take();
        let (attachments, facturx) = self.attachments()?;
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
        let job = Job {
            source,
            inputs,
            options: JobOptions {
                max_pages: self.config.max_pages,
                max_file_size: self.config.max_file_size,
                max_output_size: self.config.max_output_size,
                pdf_standards: self.pdf_standards,
                date: self.date,
                strict: self.strict,
                attachments,
                facturx,
            },
        };
        Ok(Parsed { job, callback_url })
    }
}

/// A parsed request: the job and, for async jobs, the callback URL.
pub struct Parsed {
    pub job: Job,
    pub callback_url: Option<String>,
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

/// The MIME type of an attachment: the part's `Content-Type` (without
/// parameters) unless absent or generic, else guessed from the extension.
fn mime_type(name: &str, content_type: Option<&str>) -> Result<String, ApiError> {
    if let Some(given) = content_type {
        let given = given
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        if !is_valid_mime_type(&given) {
            return Err(ApiError::invalid(format!(
                "invalid content type `{given}` for attachment `{name}`"
            )));
        }
        if given != "application/octet-stream" {
            return Ok(given);
        }
    }
    let extension = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase());
    let guessed = match extension.as_deref() {
        Some("xml") => "text/xml",
        Some("json") => "application/json",
        Some("csv") => "text/csv",
        Some("txt") => "text/plain",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("zip") => "application/zip",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        _ => "application/octet-stream",
    };
    Ok(guessed.to_string())
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
) -> Result<Parsed, ApiError> {
    let mut b = Builder::new(config, schemas);
    let text_cap = config.max_data_size;
    while let Some(field) = multipart.next_field().await.map_err(multipart_error)? {
        let name = field.name().unwrap_or_default().to_string();
        match name.as_str() {
            "file" | "asset" => {
                let path = field.file_name().map(str::to_string).ok_or_else(|| {
                    ApiError::invalid(format!("`{name}` part without a filename"))
                })?;
                let cap = config.max_file_size.min(b.remaining());
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
                let budget = b.remaining();
                let data = read_capped(field, budget.max(1), "archive").await?;
                b.archive = true;
                // Extraction shares the request budget with the other uploads.
                let mut files = b.files.take();
                files.limit_remaining(budget);
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
            "attachment" => {
                let filename = field
                    .file_name()
                    .map(str::to_string)
                    .ok_or_else(|| ApiError::invalid("`attachment` part without a filename"))?;
                let content_type = field.content_type().map(str::to_string);
                let cap = config.max_file_size.min(b.remaining());
                let data = read_capped(field, cap.max(1), &filename).await?;
                b.add_attachment(&filename, content_type.as_deref(), data)?;
            }
            _ if name.starts_with("attachment_relationship.") => {
                let value = read_text(field, 32, &name).await?;
                b.set_relationship(&name["attachment_relationship.".len()..], value.trim())?;
            }
            "attach_data" => b.set_attach_data(parse_bool(
                "attach_data",
                read_text(field, 8, &name).await?.trim(),
            )?)?,
            "facturx" => {
                let cap = config.max_file_size.min(b.remaining());
                let xml = read_capped(field, cap.max(1), "facturx").await?;
                b.set_facturx(xml)?;
            }
            "facturx_profile" => {
                let value = read_text(field, 32, &name).await?;
                Builder::set_once(&mut b.facturx_profile, "facturx_profile", value)?;
            }
            "callback_url" => {
                let value = read_text(field, crate::webhook::MAX_URL_LEN as u64, &name).await?;
                Builder::set_once(&mut b.callback_url, "callback_url", value)?;
            }
            _ => return Err(ApiError::invalid(format!("unknown field `{name}`"))),
        }
    }
    b.finish()
}

/// Builds a job from a JSON body (template mode).
pub fn from_json(
    body: JsonRequest,
    config: &Config,
    schemas: &SchemaCache,
) -> Result<Parsed, ApiError> {
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
    b.callback_url = body.callback_url;
    b.attach_data = Some(body.attach_data);
    if let Some(xml) = body.facturx {
        b.set_facturx(xml.into_bytes())?;
    }
    b.facturx_profile = body.facturx_profile;
    for (name, value) in &body.attachment_relationship {
        b.set_relationship(name, value)?;
    }
    b.finish()
}
