# PDFQ HTTP API — v1 specification (draft)

PDFQ compiles Typst documents to PDF. It has no database: async jobs live in memory and are lost on restart.
Threat model and its limits: see the [README](../README.md#threat-model).
Compatibility is tied to the Typst version: tag built images `pdfq:<version>-typst<typst-version>` (e.g. `pdfq:0.1.0-typst0.15.1`); `/health` reports both. No image is published yet: build it from the `Dockerfile`.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/compile` | Compile synchronously, respond with the PDF |
| `POST` | `/jobs` | Queue a compilation, respond `202` |
| `GET` | `/jobs/{id}` | Job status (JSON) |
| `GET` | `/jobs/{id}/pdf` | Job result |
| `GET` | `/health` | Liveness + versions: `503` while no worker process is alive |
| `GET` | `/ready` | At least one worker ready and queue not full |
| `GET` | `/metrics` | Prometheus metrics |

## Request (`POST /compile`, `POST /jobs`)

Body: `multipart/form-data`, or `application/json` for template mode without uploads (same field names; `data` is then a JSON value instead of a string).

**Source — exactly one of:**

| Field | Description |
|---|---|
| `template` | Name of a mounted template (`^[a-z0-9_-]+$`), see *Templates* |
| `file` (repeatable) | Source files and assets. The part's filename is its path in the project (`main.typ`, `img/logo.png`) |
| `archive` | A zip of the project |

**Options:**

| Field | Default | Description |
|---|---|---|
| `entrypoint` | `main.typ` | Main file (not allowed with `template`) |
| `data` | — | JSON document, exposed as `sys.inputs.data` (string) |
| `input.<key>` | — | String exposed as `sys.inputs.<key>` |
| `asset` (repeatable) | — | Template mode only: file mounted at `/assets/<filename>`. Cannot shadow template files |
| `attachment` (repeatable) | — | File embedded in the PDF as-is (any format). The filename is its name in the PDF: 1–128 letters, digits, `.`, `_`, `-`, spaces, not starting with `.`. MIME type from the part's `Content-Type`, or guessed from the extension. Counts toward the file limits. Image headers are checked like any upload (the document can read attachments) |
| `attachment_relationship.<filename>` | `supplement` | `source`, `data`, `alternative`, `supplement`, `unspecified` (PDF/A-3 `AFRelationship`). Also applies to `data.json` and `factur-x.xml` |
| `attach_data` | `false` | Embed `data` as `data.json` (relationship `source`). Counted in `PDFQ_MAX_DATA_SIZE`, not in the file limits |
| `facturx` | — | Factur-X CII XML (file part, or string in JSON). Requires `facturx_profile`. Counts toward the file limits. Only sanity-checked (a `CrossIndustryInvoice` document): not validated against the schema, and `facturx_profile` is not checked against the profile the XML declares |
| `facturx_profile` | — | `minimum`, `basic-wl`, `basic`, `en16931`, `extended`. Embeds `factur-x.xml` (relationship `data` by default; `source` or `alternative` allowed except for `minimum` and `basic-wl`), adds `pdf_standard=a-3b` unless another `a-3*` is given (other PDF/A parts are refused), writes the Factur-X XMP metadata |
| `pdf_standard` (repeatable) | none | Values accepted by Typst 0.15.1 (`a-2b`, `a-3b`, `ua-1`, …). Attachments are refused by PDF/A-1, PDF/A-2 and `a-4` (`compile_error`); `a-3*`, `a-4f` and `a-4e` allow them |
| `date` | today (UTC) | `YYYY-MM-DD`: fixes `datetime.today()` and the PDF creation date (reproducible output) |
| `strict` | `false` | Any Typst warning (e.g. unknown font) fails the job |
| `callback_url` | — | `/jobs` only, see *Webhook* |

Paths of uploaded files come from the part's filename. With curl: `-F 'file=@logo.png;filename=img/logo.png'`. The `.pdfq/` directory is reserved: uploads under it are refused.

Factur-X with curl: `-F file=@main.typ -F facturx=@invoice.xml -F facturx_profile=en16931`.

Typst snippet to read the data: `#let data = json(bytes(sys.inputs.at("data", default: "{}")))`.

## Responses

**`POST /compile` success:** `200 application/pdf`, headers `X-Pdfq-Pages`, `X-Pdfq-Warnings` (count), `X-Pdfq-Duration-Ms`.

**`POST /jobs`:** `202 {"job_id": "<uuid>", "status_url": "/jobs/<uuid>"}`, with a `Location` header. Input errors are reported synchronously (same codes as `/compile`); `429` when the job store is full. An accepted job that waits longer than `PDFQ_QUEUE_TIMEOUT` for a worker ends `failed` with `queue_full`: resubmit it.

**`GET /jobs/{id}`:**
```json
{ "id": "…", "status": "queued|running|succeeded|failed",
  "created_at": "…", "finished_at": "…", "pages": 3,
  "warnings": [ { "message": "…", "file": "main.typ", "line": 4, "column": 2 } ],
  "error": null,
  "webhook": { "state": "pending|delivered|failed", "attempts": 1 } }
```
**`GET /jobs/{id}/pdf`:** `200` PDF, `409` not finished, `422` job failed (error body, whatever its `kind`), `404` unknown or expired. `webhook` is `null` without `callback_url`.

**Errors** (every endpoint):
```json
{ "error": { "kind": "compile_error", "message": "…",
  "diagnostics": [ { "severity": "error", "message": "unclosed delimiter",
                     "file": "main.typ", "line": 2, "column": 10, "hints": [] } ] } }
```

| `kind` | HTTP | Meaning |
|---|---|---|
| `invalid_input` | 400 | Bad multipart, path, JSON, image, option combination |
| `schema_violation` | 400 | `data` fails the template's `schema.json` (diagnostics carry the JSON pointer) |
| `not_found` | 404 | Unknown template or job |
| `payload_too_large` | 413 | Input limits exceeded |
| `compile_error` | 422 | Typst errors (or warnings with `strict`) |
| `package_not_allowed` | 422 | Import outside the allowlist |
| `too_many_pages` | 422 | Page limit exceeded |
| `output_too_large` | 422 | PDF size limit exceeded |
| `timeout` | 422 | CPU or wall-clock limit (`detail`: `cpu` / `wall`) |
| `memory_limit` | 422 | Memory limit |
| `not_finished` | 409 | `GET /jobs/{id}/pdf` before the job finished |
| `queue_full` | 429 | Queue or job store full, `Retry-After` set |
| `worker_crashed` | 500 | Unexpected worker failure |
| `internal` | 500 | PDFQ bug (e.g. Factur-X metadata could not be written) |

Document-caused failures are 4xx on purpose: they are deterministic and must not trigger automatic retries.

## Webhook

Disabled unless `PDFQ_WEBHOOK_SECRET` is set (`callback_url` is then refused with `400`). The secret is read from the environment only (there is no command-line flag), so it never shows in the process command line.

On completion, `POST callback_url` with the PDF (`application/pdf`) on success, or the error JSON (`{"error": {…}}`, `application/json`) on failure.
Headers: `X-Pdfq-Job-Id`, `X-Pdfq-Status` (`succeeded`/`failed`), `X-Pdfq-Timestamp` (Unix seconds), `X-Pdfq-Signature: sha256=<hex HMAC-SHA256(secret, timestamp + "." + body)>`. Receivers should check the signature and reject old timestamps.

Any 2xx is a delivery. Network errors, 5xx and 429 are retried (`PDFQ_WEBHOOK_RETRIES` times, delay ×4 from `PDFQ_WEBHOOK_RETRY_DELAY`: 1 s, 4 s, 16 s by default); other statuses are final. The outcome is shown in `GET /jobs/{id}`. The webhook reports the stored outcome: a PDF that does not fit in the job store is reported as `failed`. A job's TTL starts once its webhook is settled; until then it is never evicted.

Anti-SSRF: HTTPS only, no credentials in the URL, no redirects, no proxy. Private, loopback, link-local and reserved addresses are refused, both as IP literals and after DNS resolution (each connection is checked, so DNS rebinding does not help).

| Variable | Default | |
|---|---|---|
| `PDFQ_WEBHOOK_SECRET` | — | HMAC key; enables webhooks. Environment only |
| `PDFQ_WEBHOOK_ALLOWED_HOSTS` | any | Comma-separated hosts, `*.example.com` matches subdomains |
| `PDFQ_WEBHOOK_ALLOW_PRIVATE` | `false` | Allow private and loopback targets (internal networks) |
| `PDFQ_WEBHOOK_ALLOW_HTTP` | `false` | Allow plain `http` |
| `PDFQ_WEBHOOK_TIMEOUT` | `10s` | Per attempt |
| `PDFQ_WEBHOOK_RETRIES` | `3` | Retries after the first attempt |
| `PDFQ_WEBHOOK_RETRY_DELAY` | `1s` | First retry delay |

## Metrics

`GET /metrics` (Prometheus text format, never refused for capacity):

| Metric | Type | Labels |
|---|---|---|
| `pdfq_info` | gauge | `version`, `typst_version` |
| `pdfq_jobs_total` | counter | `outcome`: `succeeded` or an error `kind` |
| `pdfq_job_duration_seconds` | histogram | time in a worker |
| `pdfq_workers` | gauge | `state`: `ready`, `busy` |
| `pdfq_queue_length` | gauge | |
| `pdfq_worker_spawns_total` | counter | |
| `pdfq_worker_exits_total` | counter | `reason`: `recycled`, `wall_timeout`, `cpu_limit`, `memory_limit`, `crashed` |
| `pdfq_requests_rejected_total` | counter | refused before reading the body |
| `pdfq_async_jobs` | gauge | jobs held in memory |
| `pdfq_webhook_deliveries_total` | counter | `result`: `delivered`, `failed` |

## Docker image

Distroless, non-root, `HEALTHCHECK` on `/health`. Volumes: `/templates`, `/fonts` (added to the embedded fonts), `/packages` (`@preview` packages as `/packages/preview/<name>/<version>`, added to the preloaded set listed in `packages/packages.txt`). Port `3000`. The server runs as UID 65532 (`nonroot`): mounted volumes must be readable by it.

## Templates

```
/templates/<name>/
  main.typ        # entrypoint
  schema.json     # optional JSON Schema for `data`, checked before compiling
  logo.png, …     # template files, readable from main.typ
```
Fonts: embedded set + `/fonts` volume (no per-request fonts). Packages: preloaded `@preview` set + `/packages` volume, no download at runtime. Every package found is importable unless `PDFQ_ALLOWED_PACKAGES` restricts the list.

## Limits

Every limit has a default and can be overridden with an environment variable (or the matching CLI flag, e.g. `--max-pages`). Sizes accept `KiB`/`MiB`/`GiB`, durations accept `s`/`m`/`h`.

| Variable | Default | Applies to |
|---|---|---|
| `PDFQ_MAX_FILE_SIZE` | `10MiB` | Each uploaded file (after unzip) |
| `PDFQ_MAX_UPLOAD_SIZE` | `50MiB` | Whole request (after unzip) |
| `PDFQ_MAX_FILES` | `200` | Files per request (after unzip) |
| `PDFQ_MAX_DATA_SIZE` | `1MiB` | `data` field |
| `PDFQ_MAX_IMAGE_PIXELS` | `50000000` | Width × height read from image headers |
| `PDFQ_MAX_PAGES` | `500` | Pages after layout |
| `PDFQ_MAX_OUTPUT_SIZE` | `100MiB` | Generated PDF |
| `PDFQ_WALL_TIMEOUT` | `30s` | Wall-clock compilation time per job |
| `PDFQ_CPU_TIMEOUT` | `20s` | CPU time per job |
| `PDFQ_QUEUE_TIMEOUT` | `30s` | Time a job may wait for a free worker; older jobs fail with `queue_full` without running |
| `PDFQ_MEMORY_LIMIT` | `512MiB` | Memory per job, above the worker baseline |
| `PDFQ_WORKER_OPEN_FILES` | `64` | Open files per worker process |
| `PDFQ_QUEUE_SIZE` | `64` | Queued jobs before `429`. At most `workers + queue size` requests are read at once; extra ones get `429` before their body is read |
| `PDFQ_JOB_TTL` | `1h` | Async job retention after completion |
| `PDFQ_MAX_JOBS` | `1000` | Async jobs kept in memory; the oldest finished ones are evicted first, then `POST /jobs` gets `429` |
| `PDFQ_MAX_STORED_SIZE` | `1GiB` | Total size of stored PDFs, including those awaiting webhook delivery. The oldest settled jobs are evicted beyond it; a larger PDF fails with `output_too_large`, and if only undelivered results remain the job fails with `queue_full` |

Limits are server-wide in v1: a request cannot raise or lower them.

## Future (not in v1)

Asset cache (`PUT /assets`, reference by sha256), PNG/SVG output, Factur-X XML generation from `data`, per-request fonts.
