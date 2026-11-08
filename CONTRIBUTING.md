# Contributing to PDFQ

Thanks for your interest. Issues and pull requests are welcome. For anything beyond a small fix, open an issue first so we can agree on the approach.

## Ground rules

- **Security first.** PDFQ runs untrusted documents. A change must not let a document read files outside its project, reach the network, or escape the worker limits. If you find a way to do one of these, report it privately (see below) rather than in a public issue.
- **The API contract lives in [`docs/api-spec.md`](docs/api-spec.md).** Change it in the same pull request as the code.
- **Every limit has a default and a `PDFQ_*` variable / CLI flag.** No hard-coded resource limits, OS hardening included (e.g. `PDFQ_WORKER_OPEN_FILES`).
- **Stateless.** No database, no disk state: async jobs live in memory.
- Code, comments, docs and commit messages are in English.

## Setup

You need the Rust toolchain pinned in `rust-toolchain.toml` and Docker (for Linux-only tests and the image).

```sh
cargo test                       # unit and integration tests (builds pdfq-worker itself)
./scripts/test-linux.sh          # the same tests in Linux, where memory limits are enforced
cargo run -p pdfq-cli -- compile examples/invoice/main.typ --data examples/invoice/data.json -o invoice.pdf
cargo build -p pdfq-server -p pdfq-worker && PDFQ_TEMPLATE_DIR=examples ./target/debug/pdfq
```

Optional: install [veraPDF](https://verapdf.org) to run the PDF/A validation tests locally (they are skipped without it; `PDFQ_REQUIRE_VERAPDF=1` makes them mandatory, as in CI). `scripts/verapdf-docker.sh` runs it from its container if you link it as `verapdf` on your `PATH`.

## Before opening a pull request

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
./scripts/test-linux.sh
```

If you touch the `Dockerfile`, `packages/` or `scripts/`:

```sh
docker build -t pdfq:dev . && ./scripts/smoke-image.sh pdfq:dev
```

If you touch `.github/workflows/`: `docker run --rm -v "$PWD":/repo -w /repo rhysd/actionlint:1.7.12@sha256:b1934ee5f1c509618f2508e6eb47ee0d3520686341fec936f3b79331f9315667`.

## Code map

| Crate | Role |
|---|---|
| `crates/protocol` | Messages between server and worker (postcard frames). Postcard is positional: never use `skip_serializing_if`, and bump `PROTOCOL_VERSION` when a message changes. |
| `crates/core` | Typst `World` (project files, packages, fonts), compilation, PDF export and post-processing (attachments, Factur-X). `#![forbid(unsafe_code)]`. |
| `crates/worker` | One process per compilation slot: reads jobs on stdin, applies per-job limits, answers on stdout. |
| `crates/server` | HTTP API, request validation, worker pool, async jobs, webhooks, metrics. The server never links Typst. |
| `crates/cli` | Local compilation for debugging, without isolation. |
| `prototypes/` | Historical spikes (Typst API, process isolation) with their notes; outside the workspace. |

`CLAUDE.md` holds the detailed notes on the security model and on Typst API pitfalls.

## Tests worth knowing

- **Hostile corpus:** [`tests/fixtures/hostile/`](tests/fixtures/hostile/) holds documents that try to exhaust CPU, memory or pages, escape the project, or abuse image decoding. Add a fixture and a test in `crates/server/tests/hostile.rs` for any new attack you defend against. Typst memoizes function calls and rejects `while true`, so hostile documents must defeat both.
- **Memory tests run on Linux only** (`RLIMIT_AS` is not enforced on macOS).
- **Typst upgrades:** versions are pinned with `=`. Read the sources of the new version before changing `World` or compile code, update `TYPST_VERSION`, and run the whole suite, including veraPDF and the image smoke test (Factur-X post-processing depends on the exact PDF layout).

## Commits

One logical change per commit, with a message that explains why. Keep pull requests focused.

## Reporting a vulnerability

Do not open a public issue. Use GitHub's private vulnerability reporting on this repository ("Security" tab, "Report a vulnerability"), with a minimal document or request that reproduces the problem.

## License

By contributing, you agree that your contributions are licensed under the MIT license, like the rest of the project.
