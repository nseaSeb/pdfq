# Changelog

All notable changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/). Image tags carry the Typst version: `<version>-typst<typst-version>`.

## [Unreleased]

First version, built against Typst 0.15.1.

### Added

- `POST /compile`: Typst to PDF from uploaded files, a zip archive, or a mounted template with JSON `data` (checked against an optional `schema.json`), `input.*` strings and `asset` files.
- Options: PDF standards (`pdf_standard`), fixed date (`date`) for reproducible output, `strict` mode turning warnings into errors.
- Async jobs: `POST /jobs`, `GET /jobs/{id}`, `GET /jobs/{id}/pdf`, kept in memory with a TTL and bounded by count and size.
- Webhooks signed with HMAC-SHA256, retried with backoff, with SSRF protection (HTTPS only, public addresses only, checked again at connection time).
- Attachments (`attachment`, `attach_data`) and Factur-X invoices (`facturx`, `facturx_profile`): PDF/A-3b with Factur-X XMP metadata.
- Process isolation: one worker process per compilation slot with hard rlimits, per-job memory and CPU budgets, wall-clock timeout with `SIGKILL`, worker recycling. Typed errors (`timeout`, `memory_limit`, `too_many_pages`, …).
- Input validation: sizes, file counts, zip bombs, image dimensions, paths, package allowlist.
- `/health`, `/ready`, Prometheus `/metrics`.
- Docker image: distroless, non-root, `HEALTHCHECK`, embedded fonts plus `/fonts`, preloaded `@preview` packages (configurable in `packages/packages.txt`) plus `/packages`, templates in `/templates`.
- `pdfq-cli` for local compilation.
- CI: formatting, clippy, tests on Linux amd64/arm64 and macOS, veraPDF validation, image build and smoke test.
