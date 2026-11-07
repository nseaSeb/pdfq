# PDFQ

HTTP service that compiles Typst documents to PDF, sync or async (webhook). Stateless, shipped as a Docker image, safe against untrusted documents. Think "Gotenberg for Typst".

- API contract: `docs/api-spec.md` (source of truth for routes, fields, error kinds, limits).
- License: MIT OR Apache-2.0. Code, comments, docs and commit messages in English.

## Architecture (Cargo workspace)

| Crate | Status | Role |
|---|---|---|
| `crates/core` (`pdfq-core`) | done | Typst `World` (`PdfqWorld`), fonts, local packages + allowlist, compile, PDF export, diagnostics |
| `crates/cli` (`pdfq-cli`) | done | `pdfq-cli compile in.typ -o out.pdf` — dev/debug, no isolation |
| `crates/protocol` | planned | Worker frames: u8 tag + u32 LE length + postcard payload |
| `crates/worker` | planned | Minimal binary, one job at a time over stdin/stdout |
| `crates/server` (binary `pdfq`) | planned | axum + tokio, worker pool supervisor, bounded queue, async job store |

Security model: every compilation runs in a separate worker process with rlimits (see `prototypes/p2-isolate/NOTES.md`). The server never links Typst.

## Core rules

- Typst crates are pinned with `=` in the root `Cargo.toml`. Never trust memory for the Typst API: read the sources in `~/.cargo/registry/src/*/typst-*-<version>/` before changing World/compile code.
- `PdfqWorld` reads only: project files (`Project`: disk root confined via canonicalize + prefix check, overlaid by in-memory files; disk wins) and `@preview` packages from local dirs (`Packages`). No network, ever.
- `sys.inputs` values are strings; JSON data goes in `sys.inputs.data`.
- Hints mentioning CLI flags (`--root`, `--package-path`) are filtered out of diagnostics.

## Typst 0.15.1 gotchas

- `PagedDocument` comes from `typst-layout` (not re-exported by `typst`).
- File ids: `RootedPath::new(VirtualRoot::Project | Package(spec), VirtualPath::new(p)?).intern()`; `FileId` derefs to `RootedPath`.
- `World::today` takes `Option<typst::foundations::Duration>`.
- `PdfStandard` is parsed through its serde names (`a-3b`, `ua-1`, …).
- No custom XMP in typst-pdf/krilla: Factur-X needs post-processing of the XMP stream.
- Typst memoizes closure calls and rejects `while true`: hostile fixtures must defeat both.

## Commands

```sh
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo test
cargo run -p pdfq-cli -- compile examples/invoice/main.typ --data examples/invoice/data.json -o out.pdf
```

rlimits (`RLIMIT_AS` in particular) are not enforced on macOS: isolation tests must run on Linux (CI or Docker, see `prototypes/p2-isolate/run-linux.sh`).

## Conventions

- One commit per milestone; stop for review after each.
- Stateless: no database. Async jobs live in memory and are lost on restart (documented).
- Every limit has a default and a `PDFQ_*` env var / CLI flag (clap).
- `unsafe` is forbidden workspace-wide; only the worker/server spawn code may opt out, with a comment.
