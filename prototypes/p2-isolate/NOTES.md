# Prototype 2 notes (Linux container, typst 0.15.1)

Run: `./run-linux.sh fixtures/*.typ` (env: P2_WALL_S, P2_CPU_BUDGET_S, P2_MEM_BUDGET_MB, P2_MAX_PAGES, P2_JOBS_PER_WORKER).

| fixture | outcome | time |
|---|---|---|
| infinite-loop (CPU budget 5s) | Timeout(cpu) via SIGXCPU | 6.1 s |
| infinite-loop (CPU 30s, wall 2s) | Timeout(wall) via killpg SIGKILL | 2.0 s |
| exp-recursion | MemoryLimit (memo cache grows) | 1.1 s |
| doubling-array | MemoryLimit (exit code 3 from allocator wrapper) | 84 ms |
| deep-recursion | CompileError "maximum function call depth exceeded" | <1 ms |
| thousands-pages | TooManyPages (5001 > 500) | 65 ms |

A sane document compiled right after every hostile case, on a respawned worker when needed.

Findings:
- Worker baseline VM is ~175 MiB (embedded fonts, 64 MiB stack, runtime), so a fixed RLIMIT_AS would be misleading: the soft limit is re-armed per job as `VmSize + budget`.
- RLIMIT_CPU is cumulative per process: the soft limit is re-armed per job as `used + budget`, with a hard limit for the whole worker lifetime.
- A global allocator that calls `_exit(3)` on null gives a clean, typed `memory_limit`.
- Typst memoizes closure calls: naive exponential recursion finishes instantly. Hostile fixtures must use distinct arguments.
- Typst rejects `while true` statically ("condition is always true"), and caps call depth itself.
- Recycling after N jobs works by closing the worker's stdin.
- Env passed to the worker: RAYON_NUM_THREADS=1, MALLOC_ARENA_MAX=2 (limits reserved address space).
