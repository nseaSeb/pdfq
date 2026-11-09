#!/usr/bin/env bash
# Smoke test of a built image, under its real rlimits, user and Docker profile.
# Usage: scripts/smoke-image.sh IMAGE
set -euo pipefail
image=${1:?usage: smoke-image.sh IMAGE}
cd "$(dirname "$0")/.."
name="pdfq-smoke-$$"
fixtures=tests/fixtures/hostile

docker run -d --name "$name" -p 127.0.0.1::3000 \
  -e PDFQ_WALL_TIMEOUT=10s -e PDFQ_CPU_TIMEOUT=5s -e PDFQ_MEMORY_LIMIT=256MiB \
  "$image" >/dev/null
passed=0
cleanup() {
  [[ "$passed" == 1 ]] || docker logs "$name" 2>&1 | tail -20 >&2
  docker rm -f "$name" >/dev/null
  rm -f /tmp/pdfq-smoke-$$.*
}
trap cleanup EXIT
url="http://$(docker port "$name" 3000/tcp | head -1)"

fail() { echo "FAIL: $*" >&2; exit 1; }

# The image's own HEALTHCHECK must report healthy.
for _ in $(seq 60); do
  health=$(docker inspect -f '{{.State.Health.Status}}' "$name")
  [[ "$health" == healthy ]] && break
  sleep 1
done
[[ "$health" == healthy ]] || fail "container is $health"
curl -fsS "$url/health" | grep -q '"typst_version"' || fail "/health"
echo "ok: healthy"

# Binaries are execute-only for nonroot (no dumpable window at exec).
for bin in pdfq pdfq-worker; do
  docker cp "$name:/usr/local/bin/$bin" - | tar -tvf - | grep -q '^-rwx--x--x' || fail "$bin mode"
done
echo "ok: execute-only binaries"

# POST /compile with main.typ (and extra curl args); prints "status kind".
compile() {
  local main=$1; shift
  curl -sS -o /tmp/pdfq-smoke-$$.out -w '%{http_code}' -F "file=@$main;filename=main.typ" "$@" \
    "$url/compile"
}
expect_pdf() {
  local status
  status=$(compile "$@")
  [[ "$status" == 200 ]] || fail "$1: HTTP $status $(cat /tmp/pdfq-smoke-$$.out)"
  head -c 5 /tmp/pdfq-smoke-$$.out | grep -q '%PDF-' || fail "$1: not a PDF"
}
expect_error() {
  local main=$1 kind=$2 status
  status=$(compile "$main")
  [[ "$status" == 422 ]] || fail "$main: HTTP $status, expected 422"
  grep -q "\"kind\":\"$kind\"" /tmp/pdfq-smoke-$$.out || fail "$main: $(cat /tmp/pdfq-smoke-$$.out)"
}

printf 'Hello from the image' >/tmp/pdfq-smoke-$$.typ
expect_pdf /tmp/pdfq-smoke-$$.typ && echo "ok: compile"
expect_pdf packages/smoke.typ && echo "ok: preloaded packages"

printf '<?xml version="1.0"?><rsm:CrossIndustryInvoice/>' >/tmp/pdfq-smoke-$$.xml
expect_pdf /tmp/pdfq-smoke-$$.typ -F "facturx=@/tmp/pdfq-smoke-$$.xml" -F facturx_profile=en16931
grep -q 'fx:ConformanceLevel' /tmp/pdfq-smoke-$$.out || fail "no Factur-X metadata"
echo "ok: Factur-X"

expect_error "$fixtures/doubling-array.typ" memory_limit && echo "ok: memory limit"
expect_error "$fixtures/infinite-loop.typ" timeout && echo "ok: timeout"

curl -fsS "$url/metrics" | grep -q '^pdfq_jobs_total{outcome="memory_limit"} 1$' || fail "/metrics"
echo "ok: metrics"
passed=1
