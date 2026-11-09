# Benchmark: cost of HTTP and process isolation

What does PDFQ add on top of compiling a Typst document directly? This page measures the HTTP layer plus the worker-process isolation against an in-process compile using the same `pdfq-core` and Typst 0.15.1. Reproduce it with `scripts/bench.sh`.

## Method

All binaries are release builds (`cargo build --release -p pdfq-server -p pdfq-worker -p pdfq-cli`). They run natively on the host, with no Docker. Both sides use only the embedded fonts.

- **A, direct**: `pdfq-cli compile` timed with `hyperfine` (3 warm-up runs, 20 runs). It compiles in-process with no HTTP and no isolation. Each run is a new process, so it also pays for process start and font loading.
- **B, PDFQ with warm workers**: `pdfq` on `127.0.0.1:3996` with the defaults (one worker per CPU, `PDFQ_JOBS_PER_WORKER=100`). Sync `POST /compile` is driven by `oha` over keep-alive connections, with 20 warm-up requests, then:
  - 200 sequential requests (concurrency 1) for latency;
  - 500 requests at concurrency 8 (the CPU count) and at 16 for throughput.
  Every response is checked to be `200`.
- **C, fresh worker per job**: same as B with `PDFQ_JOBS_PER_WORKER=1`. Each worker exits after one job and the supervisor spawns a replacement. That replacement process loads and preloads fonts before it can take a job.

Documents:

| Name | Request | Output |
|---|---|---|
| `hello` | multipart upload of `= Hello` | 1 page |
| `invoice` | JSON body: `examples/invoice` template + `examples/invoice/data.json` | 1 page |
| `report` | multipart upload: a loop of 20 sections (`lorem(250)` + a 24-cell table) | 20 pages |

### Warm compile time

`pdfq-cli` has no repeat option, so there is no direct "warm in-process" number. The closest figure is the server's `X-Pdfq-Duration-Ms` header. The server measures it from job submission to result, so it is the worker's compile time plus the pipe round-trip and postcard framing. It has millisecond resolution. The script samples it over 50 sequential `curl` requests and compares it with curl's total time for the same requests.

### Typst memoization

A warm worker could make repeated identical documents look faster through Typst's memoization (comemo). The worker clears these caches after every job with `comemo::evict(0)` (`crates/worker/src/main.rs`, `pdfq_core::evict_cache`), so no cache carries over from one request to the next. `oha` cannot vary a multipart body, but it can send a different body per request from a file (`-Z`). The script uses this to compare 200 identical invoice requests with 200 requests that each have a different invoice number.

## Machine

- Apple M1 (4 performance + 4 efficiency cores), 16 GiB RAM, macOS 27.0.1, on battery.
- rustc 1.99.1 (Homebrew), hyperfine 2.0.0, oha 1.16.0.
- Typst 0.15.1, PDFQ 0.1.0 (commit `4453ecc`).
- macOS does not enforce `RLIMIT_AS`, so the per-job memory limit is inactive. The CPU limit and the wall timeout are active. This should not change timings much.

**The machine was not quiet.** Other workloads (Elixir builds, clang, OrbStack) kept the load average around 20 for the whole session, and they could not be stopped. The script was run three times. The tables below come from the fastest run (run 2). The other two runs were up to 60% slower in absolute terms. The ratios between A, B and C point the same way in every run, but their size varies (see the run-to-run variation section). Treat the absolute values as indicative, and the ratios as orders of magnitude. For citable numbers, rerun the script on an idle machine.

## Results (run 2)

### A: direct compile, `pdfq-cli` (new process per run)

| Document | mean ms | p50 ms | stddev ms |
|---|---:|---:|---:|
| hello | 8.8 | 8.8 | 0.5 |
| invoice | 11.4 | 11.5 | 0.3 |
| report (20 pages) | 34.1 | 34.2 | 1.8 |

### B: PDFQ, warm workers (8 workers, `PDFQ_JOBS_PER_WORKER=100`)

| Document | concurrency | p50 ms | p95 ms | p99 ms | req/s |
|---|---:|---:|---:|---:|---:|
| hello | 1 | 1.2 | 1.5 | 2.6 | 827 |
| hello | 8 | 1.5 | 2.4 | 2.8 | 4898 |
| hello | 16 | 2.8 | 6.7 | 13.8 | 4756 |
| invoice | 1 | 3.4 | 4.0 | 4.5 | 286 |
| invoice | 8 | 5.5 | 10.9 | 16.1 | 1285 |
| invoice | 16 | 11.5 | 18.3 | 20.5 | 1322 |
| report | 1 | 36.5 | 46.8 | 57.0 | 26.6 |
| report | 8 | 62.0 | 95.8 | 113.2 | 123 |
| report | 16 | 148.5 | 224.0 | 256.8 | 102 |

Server-side time (`X-Pdfq-Duration-Ms`) against the full HTTP request time, both p50 over 50 sequential requests:

| Document | header p50 ms | curl total p50 ms |
|---|---:|---:|
| hello | 0 (under 1) | 1.6 |
| invoice | 2 | 3.4 |
| report | 31 | 32.4 |

Identical against varied invoice bodies (sequential, 200 requests):

| Body | p50 ms | p95 ms | p99 ms | req/s |
|---|---:|---:|---:|---:|
| identical | 2.6 | 3.0 | 3.9 | 369 |
| different invoice number per request | 2.8 | 3.2 | 4.2 | 351 |

### C: PDFQ, fresh worker per job (`PDFQ_JOBS_PER_WORKER=1`)

| Document | concurrency | p50 ms | p95 ms | p99 ms | req/s |
|---|---:|---:|---:|---:|---:|
| hello | 1 | 2.4 | 3.3 | 3.5 | 399 |
| hello | 8 | 15.7 | 22.8 | 27.7 | 494 |
| hello | 16 | 34.0 | 45.4 | 50.2 | 455 |
| invoice | 1 | 7.1 | 11.6 | 15.6 | 131 |
| invoice | 8 | 23.1 | 35.8 | 43.6 | 332 |
| invoice | 16 | 50.0 | 76.4 | 88.0 | 307 |
| report | 1 | 53.1 | 66.6 | 74.7 | 18.4 |
| report | 8 | 94.7 | 132.3 | 161.0 | 81 |
| report | 16 | 196.2 | 278.6 | 334.4 | 78 |

### Run-to-run variation

Values from runs 1, 2 and 3, under varying background load:

| Metric | run 1 | run 2 | run 3 |
|---|---:|---:|---:|
| A hello p50 ms | 12.7 | 8.8 | 10.7 |
| B hello c=1 p50 ms | 1.7 | 1.2 | 1.9 |
| A report p50 ms | 52.5 | 34.2 | 48.8 |
| B report c=1 p50 ms | 41.6 | 36.5 | 54.0 |
| B invoice c=8 req/s | 1187 | 1285 | 835 |
| C invoice c=8 req/s | 360 | 332 | 379 |
| B hello c=8 req/s | 3091 | 4898 | 4959 |
| C hello c=8 req/s | 445 | 494 | 395 |

## Interpretation

- **With warm workers, HTTP and isolation add about 1 to 1.5 ms per request.** Curl's total time minus the server-side time is 1.4 to 1.6 ms. That includes a new TCP connection per curl call. With keep-alive (`oha`), a whole `hello` request takes 1.2 ms at p50, so the fixed cost is at most that. It does not grow with the document, so it matters for tiny documents and disappears for large ones (32.4 ms against 31 ms for the report).
- **A warm PDFQ server answers faster than running `pdfq-cli` once per document:** 1.2 ms against 8.8 ms for hello, and 3.4 ms against 11.5 ms for the invoice. Most of the CLI time is process start and font loading, which a warm worker pays once. For the 20-page report, compile time dominates and A and B are within noise of each other (the order flips between runs). Throughput scales with the workers until the 4 performance cores are busy. At concurrency 16, requests queue, so throughput holds but latency doubles.
- **`PDFQ_JOBS_PER_WORKER=1` costs a process spawn and font preload per job.** At concurrency 1 this hardly shows, because idle workers were spawned ahead of time (hello 2.4 ms, invoice 7.1 ms). Under load, spawning becomes the bottleneck: throughput is capped around 400 to 500 req/s on this machine, which is a worker start every 15 to 20 ms per slot. Across the three runs, that is about 7 to 12x less than warm workers for `hello` and 2 to 4x less for the invoice, while latency grows to 15 to 50 ms. For a heavy document like the report, compile time dominates and the gap shrinks to about 1.5x.
- **No memoization bias:** identical and varied invoice bodies give the same latency, as expected since the worker evicts Typst's caches after every job.

## Caveats

- One machine, a laptop on battery, with sustained background load (see the Machine section). Absolute numbers will differ elsewhere. Compare ratios, not milliseconds.
- macOS: process spawn is slower than on Linux, which probably makes C look worse than it would in the Linux image. `RLIMIT_AS` is not enforced, so the memory limit was inactive. Linux sets more limits in `pre_exec`, but they cost little.
- The M1 has 4 performance and 4 efficiency cores. The concurrency 8 and 16 figures mix both kinds.
- Client and server share the CPU, so `oha` takes some of the CPU time at high concurrency.
- No Linux figure: the Docker VM (OrbStack) was itself busy during the session, so a measurement inside it would not mean much. To get one, run the script in a Linux environment or point `oha` at the image with the same requests.
- `X-Pdfq-Duration-Ms` has millisecond resolution and includes the pipe round-trip to the worker, so it is an upper bound on the warm compile time.
- The documents are small. Documents with large images or many packages shift the balance further toward compile time.

## Reproducing

```sh
scripts/bench.sh            # builds release binaries, then runs A, B and C
scripts/bench.sh --no-build
BENCH_DOCS="hello invoice" BENCH_SEQ=500 scripts/bench.sh --no-build
```

The script needs `cargo`, `curl`, `jq`, `hyperfine` and `oha` (`brew install hyperfine oha` or `cargo install hyperfine oha`). It starts the server on `127.0.0.1:3996` (`BENCH_PORT`) and stops it on exit. Close other workloads first.
