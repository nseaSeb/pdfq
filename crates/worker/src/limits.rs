//! Per-job resource limits, applied by the worker to itself.
//!
//! The supervisor sets generous *hard* limits before exec. Before each job the
//! worker lowers its *soft* limits relative to its current state, because both
//! counters are cumulative over the life of the process:
//! - `RLIMIT_AS`: current address space + memory budget (Linux; macOS does not
//!   enforce it),
//! - `RLIMIT_CPU`: CPU time used so far + CPU budget (exceeding it raises `SIGXCPU`).

#![allow(unsafe_code)]

macro_rules! set_soft {
    ($resource:expr, $value:expr) => {{
        // SAFETY: plain libc calls on a stack-allocated struct.
        unsafe {
            let mut lim: libc::rlimit = std::mem::zeroed();
            if libc::getrlimit($resource, &mut lim) == 0 {
                let value = $value as libc::rlim_t;
                lim.rlim_cur = if lim.rlim_max == libc::RLIM_INFINITY {
                    value
                } else {
                    value.min(lim.rlim_max)
                };
                libc::setrlimit($resource, &lim);
            }
        }
    }};
}

/// Arms the soft limits for the next job.
pub fn arm(memory_budget: u64, cpu_budget_secs: u64) {
    if memory_budget > 0
        && let Some(vm) = address_space()
    {
        set_soft!(libc::RLIMIT_AS, vm.saturating_add(memory_budget));
    }
    if cpu_budget_secs > 0 {
        set_soft!(libc::RLIMIT_CPU, cpu_used_secs() + cpu_budget_secs);
    }
}

/// Lifts the soft memory limit back to the hard limit (between jobs).
pub fn disarm_memory() {
    set_soft!(libc::RLIMIT_AS, libc::RLIM_INFINITY);
}

/// Current virtual address space of this process, in bytes (Linux only).
pub fn address_space() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|l| l.starts_with("VmSize:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib * 1024)
}

/// CPU time (user + system) consumed so far, rounded up to whole seconds, plus
/// one second of margin so a job never starts with a nearly exhausted budget.
fn cpu_used_secs() -> u64 {
    // SAFETY: getrusage writes into a zeroed, stack-allocated struct.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let micros = |t: libc::timeval| t.tv_sec as u64 * 1_000_000 + t.tv_usec as u64;
    let total = micros(usage.ru_utime) + micros(usage.ru_stime);
    total.div_ceil(1_000_000) + 1
}
