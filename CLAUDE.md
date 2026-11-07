# PDFQ

HTTP service that compiles Typst documents to PDF, sync or async (webhook). Stateless, shipped as a Docker image, safe against untrusted documents. Think "Gotenberg for Typst".

- API contract: `docs/api-spec.md` (source of truth for routes, fields, error kinds, limits).
- License: MIT OR Apache-2.0. Code, comments, docs and commit messages in English.

## Architecture (Cargo workspace)

| Crate | Status | Role |
|---|---|---|
| `crates/core` (`pdfq-core`) | done | Typst `World` (`PdfqWorld`), fonts, local packages + allowlist, compile, PDF export, diagnostics |
| `crates/cli` (`pdfq-cli`) | done | `pdfq-cli compile in.typ -o out.pdf` — dev/debug, no isolation |
| `crates/protocol` (`pdfq-protocol`) | done | `Job`/`WorkerMessage`/`JobError`/`Diagnostic`; frames = u32 LE length + postcard |
| `crates/worker` (`pdfq-worker`) | done | One job at a time over stdin/stdout, per-job soft rlimits, OOM allocator (exit 3) |
| `crates/server` (`pdfq-server`, binary `pdfq`) | sync + async API done | `pool`: worker supervisor; `config`: clap + `PDFQ_*`; `request`: multipart/JSON parsing; `validate`: paths, sizes, zip, image headers; `api`: `/compile`, `/jobs`, `/health`, `/ready`; `jobs`: in-memory TTL store (bounded count and bytes); `webhook`: signed delivery, retries, anti-SSRF; `error`: `JobError` to HTTP status |

Security model: every compilation runs in a separate worker process. The supervisor sets hard rlimits in `pre_exec` (`crates/server/src/spawn.rs`); before each job the worker lowers its own soft `RLIMIT_AS` (current VmSize + budget) and `RLIMIT_CPU` (used + budget), because both are cumulative over the process life (`crates/worker/src/limits.rs`). Death is classified in `pool::classify`: exit 3 = `memory_limit`, SIGXCPU = `timeout`/cpu, our SIGKILL after the wall timeout = `timeout`/wall. The server never links Typst.

Webhook SSRF guard: IP literals are checked in `Webhooks::validate` (reqwest skips the resolver for them), hostnames by `GuardedResolver` at each connection (no rebinding); no redirects, no proxy.

Hostile corpus: `tests/fixtures/hostile/`, exercised by `crates/server/tests/hostile.rs` (the test builds `pdfq-worker` itself, or uses `PDFQ_WORKER_BIN`).

## Core rules

- Typst crates are pinned with `=` in the root `Cargo.toml`. Never trust memory for the Typst API: read the sources in `~/.cargo/registry/src/*/typst-*-<version>/` before changing World/compile code.
- `PdfqWorld` reads only: project files (`Project`: disk root confined via canonicalize + prefix check, overlaid by in-memory files; disk wins) and `@preview` packages from local dirs (`Packages`). No network, ever.
- `sys.inputs` values are strings; JSON data goes in `sys.inputs.data`.
- Attachments: with any attachment, `PdfqWorld` compiles a synthetic `/.pdfq/main.typ` that `#include`s the user's entrypoint, then calls `#std.pdf.attach(...)` with data from the reserved `/.pdfq/attachments/<i>` (checked before the project). The document's show rules or comments cannot reach the attach calls. Names are passed as absolute paths, because the name in the PDF is the root-relative resolved path.
- Factur-X: `core/src/facturx.rs` rewrites krilla's metadata object in place and shifts the xref table. It assumes a classic xref table with one subsection and an unfiltered XMP stream, and fails (`worker_crashed`) rather than returning a PDF without the metadata. `core/tests/attach.rs` validates with veraPDF when installed (`PDFQ_REQUIRE_VERAPDF=1` makes it mandatory).
- Hints mentioning CLI flags (`--root`, `--package-path`) are filtered out of diagnostics.

## Typst 0.15.1 gotchas

- `PagedDocument` comes from `typst-layout` (not re-exported by `typst`).
- File ids: `RootedPath::new(VirtualRoot::Project | Package(spec), VirtualPath::new(p)?).intern()`; `FileId` derefs to `RootedPath`.
- `World::today` takes `Option<typst::foundations::Duration>`.
- `PdfStandard` is parsed through its serde names (`a-3b`, `ua-1`, …).
- No custom XMP in typst-pdf/krilla: Factur-X needs post-processing of the XMP stream.
- Typst memoizes closure calls and rejects `while true`: hostile fixtures must defeat both.
- Protocol: never use `skip_serializing_if` on protocol types; postcard is positional and skipped fields corrupt frames.

## Commands

Run the server locally: `cargo build -p pdfq-server -p pdfq-worker && PDFQ_TEMPLATE_DIR=examples ./target/debug/pdfq` (the worker binary is looked up next to `pdfq`, or `PDFQ_WORKER_BIN`).


```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -p pdfq-cli -- compile examples/invoice/main.typ --data examples/invoice/data.json -o out.pdf
```

`PR_SET_PDEATHSIG` fires when the *spawning thread* exits: spawn workers from Tokio worker threads, never from `spawn_blocking`.

`RLIMIT_AS` is not enforced on macOS, so memory cases of the hostile corpus only run on Linux: `./scripts/test-linux.sh` (Docker). `pre_exec` code must stay async-signal-safe (no allocation, no logging).

## Conventions

- One commit per milestone; stop for review after each.
- Stateless: no database. Async jobs live in memory and are lost on restart (documented).
- Every limit has a default and a `PDFQ_*` env var / CLI flag (clap).
- `unsafe` is denied workspace-wide (`forbid` in core, cli, protocol). Only `worker` (allocator, rlimits) and `server/src/spawn.rs` opt out, with `SAFETY` comments.
