//! Spawning a worker process with OS resource limits.

#![allow(unsafe_code)]

use std::io;

use tokio::process::Command;

/// Hard limits applied to a worker before exec.
#[derive(Debug, Clone, Copy)]
pub struct HardLimits {
    /// Address space ceiling in bytes (the worker lowers its soft limit per job).
    pub address_space: u64,
    /// CPU seconds over the whole life of the worker.
    pub cpu_secs: u64,
    /// Open file descriptors (the worker reads files whole, then closes them).
    pub open_files: u64,
}

/// Makes the current process non-dumpable (Linux), so that processes running as
/// the same user, such as a compromised worker, cannot read its memory or
/// environment through `/proc` nor ptrace it. No-op on other systems.
///
/// Called by [`crate::pool::Pool::start`] before the first worker is spawned;
/// idempotent. `execve` resets the flag, so workers must set it themselves
/// (the image also installs `pdfq-worker` execute-only, which closes the window
/// before the worker's `main`).
pub(crate) fn set_non_dumpable() -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl with integer arguments only; no memory is passed.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Installs the hard limits and isolation settings, applied in the child
/// between `fork` and `exec`.
pub fn apply(cmd: &mut Command, limits: HardLimits) {
    let address_space = limits.address_space as libc::rlim_t;
    let cpu = limits.cpu_secs as libc::rlim_t;
    let open_files = limits.open_files as libc::rlim_t;
    // SAFETY: the closure runs after fork in a multithreaded process, so it only
    // makes async-signal-safe calls (setsid, prctl, setrlimit) on values computed
    // beforehand. It does not allocate, lock or log.
    unsafe {
        cmd.pre_exec(move || {
            libc::setsid();
            #[cfg(target_os = "linux")]
            {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
            }
            let set = |resource, value: libc::rlim_t| {
                let lim = libc::rlimit {
                    rlim_cur: value,
                    rlim_max: value,
                };
                if libc::setrlimit(resource, &lim) == 0 {
                    Ok(())
                } else {
                    Err(io::Error::last_os_error())
                }
            };
            // Not enforced on macOS, and refused by Darwin when below current usage.
            #[cfg(target_os = "linux")]
            set(libc::RLIMIT_AS, address_space)?;
            #[cfg(not(target_os = "linux"))]
            let _ = address_space;
            set(libc::RLIMIT_CPU, cpu)?;
            set(libc::RLIMIT_NOFILE, open_files)?;
            set(libc::RLIMIT_FSIZE, 0)?;
            set(libc::RLIMIT_CORE, 0)?;
            Ok(())
        });
    }
}
