#!/usr/bin/env bash
# Runs the test suite inside a Linux container. Needed on macOS: RLIMIT_AS (memory
# limit) is only enforced on Linux, so the memory cases of the hostile corpus are
# skipped elsewhere. Extra arguments are passed to `cargo test`.
set -euo pipefail
cd "$(dirname "$0")/.."
exec docker run --rm \
  -v "$PWD":/work -w /work \
  -v pdfq-cargo-registry:/usr/local/cargo/registry \
  -v pdfq-linux-target:/work/target \
  rust:1-bookworm cargo test --workspace "$@"
