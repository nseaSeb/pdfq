#!/usr/bin/env bash
# Benchmark: cost of PDFQ's HTTP layer and process isolation versus compiling
# Typst in-process with pdfq-cli. Method and results: docs/benchmark.md.
#
# Usage: scripts/bench.sh [--no-build]
# Needs: cargo, curl, jq, hyperfine, oha. Keep the machine otherwise idle.
# Tunables (env): BENCH_PORT (3996), BENCH_RUNS (20, hyperfine runs),
# BENCH_SEQ (200, sequential requests), BENCH_LOAD (500, requests per
# concurrency level), BENCH_SAMPLES (50, requests sampled for the
# X-Pdfq-Duration-Ms header), BENCH_DOCS ("hello invoice report").
set -euo pipefail

cd "$(dirname "$0")/.."
ROOT=$PWD
PORT=${BENCH_PORT:-3996}
RUNS=${BENCH_RUNS:-20}
SEQ=${BENCH_SEQ:-200}
LOAD=${BENCH_LOAD:-500}
SAMPLES=${BENCH_SAMPLES:-50}
read -r -a DOCS <<<"${BENCH_DOCS:-hello invoice report}"
URL="http://127.0.0.1:$PORT"
BIN=$ROOT/target/release

for tool in cargo curl jq hyperfine oha; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 1; }
done

if [[ "${1:-}" != "--no-build" ]]; then
  cargo build --release -p pdfq-server -p pdfq-worker -p pdfq-cli
fi

WORK=$(mktemp -d)
SERVER_PID=
cleanup() {
  if [[ -n "$SERVER_PID" ]]; then
    kill "$SERVER_PID" 2>/dev/null || true
    wait "$SERVER_PID" 2>/dev/null || true
  fi
  rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

# Same embedded fonts on both sides.
unset PDFQ_FONT_PATHS PDFQ_PACKAGE_PATHS

# Documents. hello: tiny multipart upload. invoice: the mounted template with
# JSON data. report: 20 generated pages (text and a table per page).
mkdir -p "$WORK/hello" "$WORK/report"
echo '= Hello' >"$WORK/hello/main.typ"
cat >"$WORK/report/main.typ" <<'TYP'
#set page(numbering: "1")
#for i in range(1, 21) [
  = Section #i
  #lorem(250)
  #table(columns: 4, ..range(24).map(j => [#(i * j)]))
  #pagebreak(weak: true)
]
TYP
jq -c --argjson data "$(cat examples/invoice/data.json)" -n \
  '{template: "invoice", data: $data}' >"$WORK/invoice.json"
# One body per line with a different invoice number, to check whether
# repeated identical requests benefit from caching.
for i in $(seq 1 "$SEQ"); do
  jq -c --arg n "BENCH-$i" '.data.number = $n' "$WORK/invoice.json"
done >"$WORK/invoice-varied.jsonl"

cpus=$(getconf _NPROCESSORS_ONLN)

echo "== Machine"
if [[ "$(uname)" == Darwin ]]; then
  echo "cpu:   $(sysctl -n machdep.cpu.brand_string), $cpus cores"
  echo "ram:   $(($(sysctl -n hw.memsize) / 1073741824)) GiB"
  echo "os:    macOS $(sw_vers -productVersion) (RLIMIT_AS not enforced: memory limit inactive)"
else
  echo "cpu:   $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | xargs), $cpus cores"
  echo "ram:   $(awk '/MemTotal/ {printf "%d GiB", $2 / 1048576}' /proc/meminfo)"
  echo "os:    $(uname -sr)"
fi
echo "rustc: $(rustc --version)"
echo "tools: $(hyperfine --version), $(oha --version)"

echo
echo "== A: pdfq-cli compile (in-process, new process per run: includes start-up and font loading)"
cli_args() {
  case $1 in
    hello) echo "$WORK/hello/main.typ" ;;
    invoice) echo "examples/invoice/main.typ --data examples/invoice/data.json" ;;
    report) echo "$WORK/report/main.typ" ;;
  esac
}
echo "report: $("$BIN/pdfq-cli" compile "$WORK/report/main.typ" -o "$WORK/out.pdf" 2>&1 | cut -d, -f1)"
printf '%-8s %8s %8s %8s\n' doc mean_ms p50_ms stddev
for doc in "${DOCS[@]}"; do
  hyperfine --warmup 3 --runs "$RUNS" --style none --export-json "$WORK/a-$doc.json" \
    "$BIN/pdfq-cli compile $(cli_args "$doc") -o $WORK/out.pdf" >/dev/null
  jq -r --arg d "$doc" '.results[0] | (.summary.time_wall_clock // .) # hyperfine 2.x // 1.x
    | [$d, .mean * 1000, .median * 1000, .stddev * 1000]
    | "\(.[0]) \(.[1]) \(.[2]) \(.[3])"' "$WORK/a-$doc.json" |
    xargs printf '%-8s %8.1f %8.1f %8.1f\n'
done

start_server() {
  PDFQ_TEMPLATE_DIR=examples PDFQ_BIND=127.0.0.1:$PORT RUST_LOG=warn \
    "$@" "$BIN/pdfq" >"$WORK/server.log" 2>&1 &
  SERVER_PID=$!
  for _ in $(seq 1 100); do
    curl -fs "$URL/ready" >/dev/null 2>&1 && return 0
    sleep 0.1
  done
  echo "server did not become ready:" >&2
  cat "$WORK/server.log" >&2
  exit 1
}

stop_server() {
  kill "$SERVER_PID"
  wait "$SERVER_PID" 2>/dev/null || true
  SERVER_PID=
}

# Request arguments shared by oha and curl.
req_args() {
  case $1 in
    hello) echo "-F file=@$WORK/hello/main.typ" ;;
    report) echo "-F file=@$WORK/report/main.typ" ;;
    invoice) echo "-H content-type:application/json -d @$WORK/invoice.json" ;;
  esac
}

oha_run() { # doc concurrency requests [extra oha args...]
  local doc=$1 c=$2 n=$3
  shift 3
  local body
  if [[ "$doc" == invoice ]]; then
    body=(-T application/json -D "$WORK/invoice.json")
  else
    body=(-F "file=@$WORK/$doc/main.typ")
  fi
  oha --no-tui --output-format json -m POST -c "$c" -n "$n" "${body[@]}" "$@" "$URL/compile" >"$WORK/oha.json" 2>"$WORK/oha.err"
  check_ok "$doc c=$c" "$n"
}

lat() { # label
  printf '%-22s ' "$1"
  jq -r '[.latencyPercentiles.p50 * 1000, .latencyPercentiles.p95 * 1000,
    .latencyPercentiles.p99 * 1000, .summary.requestsPerSec] | map(tostring) | join(" ")' \
    "$WORK/oha.json" | xargs printf '%8.1f %8.1f %8.1f %9.1f\n'
}

check_ok() { # label requests
  local ok
  ok=$(jq '.statusCodeDistribution["200"] // 0' "$WORK/oha.json")
  if [[ "$ok" != "$2" ]]; then
    echo "non-200 responses for $1:" >&2
    jq -c '.statusCodeDistribution, .errorDistribution' "$WORK/oha.json" >&2
    cat "$WORK/oha.err" >&2
    exit 1
  fi
}

bench_server() { # label env...
  local label=$1
  shift
  echo
  echo "== $label"
  start_server env "$@"
  printf '%-22s %8s %8s %8s %9s\n' "doc / concurrency" p50_ms p95_ms p99_ms req_s
  for doc in "${DOCS[@]}"; do
    oha_run "$doc" 2 20 # warm-up
    oha_run "$doc" 1 "$SEQ"
    lat "$doc c=1"
    oha_run "$doc" "$cpus" "$LOAD"
    lat "$doc c=$cpus"
    oha_run "$doc" $((2 * cpus)) "$LOAD"
    lat "$doc c=$((2 * cpus))"
  done
}

bench_server "B: pdfq server, warm workers (PDFQ_JOBS_PER_WORKER=100, $cpus workers)"

echo
echo "-- B: X-Pdfq-Duration-Ms (server-side, submit to result) vs curl total, sequential, $SAMPLES requests"
printf '%-8s %14s %14s\n' doc header_p50_ms curl_p50_ms
for doc in "${DOCS[@]}"; do
  : >"$WORK/samples"
  for _ in $(seq 1 "$SAMPLES"); do
    # shellcheck disable=SC2046 # word splitting of the request arguments is intended
    curl -fs -o /dev/null -D "$WORK/headers" -w '%{time_total}\n' $(req_args "$doc") "$URL/compile" >"$WORK/t"
    h=$(tr -d '\r' <"$WORK/headers" | awk -F': ' 'tolower($1) == "x-pdfq-duration-ms" {print $2}')
    echo "$h $(cat "$WORK/t")" >>"$WORK/samples"
  done
  hp50=$(cut -d' ' -f1 "$WORK/samples" | sort -n | awk '{a[NR]=$1} END {print a[int((NR + 1) / 2)]}')
  cp50=$(cut -d' ' -f2 "$WORK/samples" | sort -n | awk '{a[NR]=$1} END {printf "%.1f", a[int((NR + 1) / 2)] * 1000}')
  printf '%-8s %14s %14s\n' "$doc" "$hp50" "$cp50"
done
echo "(header: measured in the server from job submission to result, so it"
echo " includes the pipe round-trip to the worker; curl: full HTTP request)"

if [[ " ${DOCS[*]} " == *" invoice "* ]]; then
  echo
  echo "-- B: invoice, identical vs varied bodies (sequential, $SEQ requests)"
  printf '%-22s %8s %8s %8s %9s\n' "body" p50_ms p95_ms p99_ms req_s
  oha_run invoice 1 "$SEQ"
  lat "identical"
  oha --no-tui --output-format json -m POST -c 1 -n "$SEQ" -T application/json \
    -Z "$WORK/invoice-varied.jsonl" "$URL/compile" >"$WORK/oha.json" 2>"$WORK/oha.err"
  check_ok "invoice varied" "$SEQ"
  lat "varied number"
fi
stop_server

bench_server "C: pdfq server, fresh worker per job (PDFQ_JOBS_PER_WORKER=1, $cpus workers)" PDFQ_JOBS_PER_WORKER=1
stop_server
