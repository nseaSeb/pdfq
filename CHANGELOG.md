# Changelog

All notable changes are documented here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/). Image tags carry the Typst version: `<version>-typst<typst-version>`.

## [Unreleased]

### Added

- Benchmark of the cost of the HTTP layer and process isolation versus an in-process compile: `scripts/bench.sh` and results in [docs/benchmark.md](docs/benchmark.md).

## [0.1.0] - 2026-11-09

First version, built against Typst 0.15.1.

### Added

- `POST /compile`: Typst to PDF from uploaded files, a zip archive, or a mounted template with JSON `data` (checked against an optional `schema.json`), `input.*` strings and `asset` files.
- Options: PDF standards (`pdf_standard`), fixed date (`date`) for reproducible output, `strict` mode turning warnings into errors.
- Async jobs: `POST /jobs`, `GET /jobs/{id}`, `GET /jobs/{id}/pdf`, kept in memory with a TTL and bounded by count and size.
- Webhooks signed with HMAC-SHA256 (secret from `PDFQ_WEBHOOK_SECRET`, environment only), retried with backoff, with SSRF protection (HTTPS only, public addresses only, checked again at connection time).
- Attachments (`attachment`, `attach_data`) and Factur-X invoices (`facturx`, `facturx_profile`): PDF/A-3b with Factur-X XMP metadata.
- Process isolation: one worker process per compilation slot with hard rlimits, per-job memory and CPU budgets, wall-clock timeout with `SIGKILL`, worker recycling, server and workers non-dumpable on Linux (binaries execute-only in the image). Typed errors (`timeout`, `memory_limit`, `too_many_pages`, …).
- Input validation: sizes, file counts, zip bombs, image dimensions, paths, package allowlist.
- `/health`, `/ready`, Prometheus `/metrics`.
- Docker image: distroless, non-root, `HEALTHCHECK`, embedded fonts plus `/fonts`, preloaded `@preview` packages (configurable in `packages/packages.txt`) plus `/packages`, templates in `/templates`.
- `pdfq-cli` for local compilation.
- CI: formatting, clippy, tests on Linux amd64/arm64 and macOS, veraPDF validation, image build and smoke test.
- Release workflow: multi-arch images (linux/amd64, linux/arm64) on `ghcr.io/nseaseb/pdfq`, smoke-tested before push, with a build provenance attestation.
- Documentation: threat model and its limits, architecture and job lifecycle diagrams, OpenAPI 3.1 description (`docs/openapi.yaml`) and a Bruno collection (`examples/bruno/`).
- Client examples in Python, Node.js and Livebook (Elixir), with webhook receivers that verify the signature (`examples/clients/`).

[Unreleased]: https://github.com/nseaSeb/pdfq/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nseaSeb/pdfq/releases/tag/v0.1.0
