#!/usr/bin/env bash
# Build and run the prototype inside Linux (rlimits are not enforced on macOS).
set -euo pipefail
cd "$(dirname "$0")"
docker run --rm -v "$PWD":/w -w /w \
  -v pdfq-p2-registry:/usr/local/cargo/registry -v pdfq-p2-target:/w/target \
  -e P2_WALL_S -e P2_CPU_BUDGET_S -e P2_MEM_BUDGET_MB -e P2_MAX_PAGES -e P2_JOBS_PER_WORKER \
  rust:1-bookworm bash -c 'cargo build --release -q && ./target/release/p2-isolate supervise "$@"' _ "$@"
