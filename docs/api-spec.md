# PDFQ HTTP API — v1 specification (draft)

PDFQ compiles Typst documents to PDF. It is stateless: async jobs live in memory and are lost on restart.
Compatibility is tied to the Typst version: images are tagged `pdfq:<version>-typst<typst-version>` (e.g. `pdfq:0.1.0-typst0.15.1`), and `/health` reports both.

## Endpoints

| Method | Path | Purpose |
|---|---|---|
| `POST` | `/compile` | Compile synchronously, respond with the PDF |
| `POST` | `/jobs` | Queue a compilation, respond `202` |
| `GET` | `/jobs/{id}` | Job status (JSON) |
| `GET` | `/jobs/{id}/pdf` | Job result |
| `GET` | `/health` | Liveness + versions |
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
| `attachment` (repeatable) | — | File embedded in the PDF as-is (any format) |
| `attachment_relationship.<filename>` | `supplement` | `source`, `data`, `alternative`, `supplement`, `unspecified` |
| `attach_data` | `false` | Embed `data` as `data.json` (relationship `source`) |
| `facturx` | — | Factur-X CII XML. Requires `facturx_profile` |
| `facturx_profile` | — | `minimum`, `basic-wl`, `basic`, `en16931`, `extended`. Forces `pdf_standard=a-3b`, embeds `factur-x.xml`, writes the Factur-X XMP metadata |
| `pdf_standard` | none | Values accepted by Typst 0.15.1 (`a-2b`, `a-3b`, `ua-1`, …). With attachments only `a-3*` (or none) is allowed |
| `date` | today (UTC) | `YYYY-MM-DD`: fixes `datetime.today()` and the PDF creation date (reproducible output) |
| `strict` | `false` | Any Typst warning (e.g. unknown font) fails the job |
| `callback_url` | — | `/jobs` only, see *Webhook* |

Typst snippet to read the data: `#let data = json(bytes(sys.inputs.at("data", default: "{}")))`.

## Responses

**`POST /compile` success:** `200 application/pdf`, headers `X-Pdfq-Pages`, `X-Pdfq-Warnings` (count), `X-Pdfq-Duration-Ms`.

**`POST /jobs`:** `202 {"job_id": "<uuid>", "status_url": "/jobs/<uuid>"}`.

**`GET /jobs/{id}`:**
```json
{ "id": "…", "status": "queued|running|succeeded|failed",
  "created_at": "…", "finished_at": "…", "pages": 3,
  "warnings": [ { "message": "…", "file": "main.typ", "line": 4, "column": 2 } ],
  "error": null }
```
**`GET /jobs/{id}/pdf`:** `200` PDF, `409` not finished, `422` job failed (error body), `404` unknown or expired.

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
| `queue_full` | 429 | Queue full, `Retry-After` set |
| `worker_crashed` | 500 | Unexpected worker failure |

Document-caused failures are 4xx on purpose: they are deterministic and must not trigger automatic retries.

## Webhook

On completion, `POST callback_url` with the PDF (`application/pdf`) on success, or the error JSON on failure.
Headers: `X-Pdfq-Job-Id`, `X-Pdfq-Status`, `X-Pdfq-Timestamp`, `X-Pdfq-Signature: sha256=<hex HMAC-SHA256(secret, timestamp + "." + body)>`.
Up to 3 retries (1 s, 4 s, 16 s) on network errors and 5xx. No redirects. HTTPS required; private, loopback and link-local addresses refused; optional domain allowlist.

## Templates

```
/templates/<name>/
  main.typ        # entrypoint
  schema.json     # optional JSON Schema for `data`, checked before compiling
  logo.png, …     # template files, readable from main.typ
```
Fonts: embedded set + `/fonts` volume (no per-request fonts). Packages: preloaded `@preview` set + `/packages` volume, no download at runtime.

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
| `PDFQ_MEMORY_LIMIT` | `512MiB` | Memory per job, above the worker baseline |
| `PDFQ_QUEUE_SIZE` | `64` | Queued jobs before `429` |
| `PDFQ_JOB_TTL` | `1h` | Async job retention after completion |

Limits are server-wide in v1: a request cannot raise or lower them.

## Future (not in v1)

Asset cache (`PUT /assets`, reference by sha256), PNG/SVG output, Factur-X XML generation from `data`, per-request fonts.
