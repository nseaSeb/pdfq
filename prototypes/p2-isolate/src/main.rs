//! Prototype 2: compile Typst in a separate, resource-limited worker process.
//!
//! `p2-isolate supervise <file.typ>...` spawns workers (`p2-isolate worker`),
//! sends each file as a job and classifies the outcome. Linux only for limits.
//!
//! Framing (both directions): u8 tag + u32 LE length + payload.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::{self, Read, Write};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use typst::diag::{FileError, FileResult};
use typst::foundations::{Bytes, Datetime, Duration as TDuration};
use typst::syntax::{FileId, RootedPath, Source, VirtualPath, VirtualRoot};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};
use typst_kit::fonts::FontStore;
use typst_layout::PagedDocument;

const EXIT_OOM: i32 = 3;
const MIB: u64 = 1024 * 1024;

const TAG_JOB: u8 = 0;
const TAG_READY: u8 = 1;
const TAG_OK: u8 = 2;
const TAG_COMPILE_ERROR: u8 = 3;
const TAG_TOO_MANY_PAGES: u8 = 4;
const TAG_PANIC: u8 = 5;

// ---------------------------------------------------------------------------
// Allocator: turn allocation failure (RLIMIT_AS hit) into a recognizable exit
// code instead of an abort that looks like any other crash.

struct OomExit;

unsafe impl GlobalAlloc for OomExit {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        check(unsafe { System.alloc(l) })
    }
    unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
        check(unsafe { System.alloc_zeroed(l) })
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        check(unsafe { System.realloc(p, l, n) })
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        unsafe { System.dealloc(p, l) }
    }
}

fn check(p: *mut u8) -> *mut u8 {
    if p.is_null() {
        // Async-signal-safe, does not allocate.
        unsafe { libc::_exit(EXIT_OOM) }
    }
    p
}

#[global_allocator]
static GLOBAL: OomExit = OomExit;

// ---------------------------------------------------------------------------
// Framing

fn write_frame(w: &mut impl Write, tag: u8, payload: &[u8]) -> io::Result<()> {
    w.write_all(&[tag])?;
    w.write_all(&(payload.len() as u32).to_le_bytes())?;
    w.write_all(payload)?;
    w.flush()
}

fn read_frame(r: &mut impl Read, max: usize) -> io::Result<(u8, Vec<u8>)> {
    let mut head = [0u8; 5];
    r.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut buf = vec![0; len];
    r.read_exact(&mut buf)?;
    Ok((head[0], buf))
}

// ---------------------------------------------------------------------------
// Limits helpers

fn set_soft(resource: libc::__rlimit_resource_t, value: u64) {
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        libc::getrlimit(resource, &mut lim);
        lim.rlim_cur = value.min(lim.rlim_max);
        libc::setrlimit(resource, &lim);
    }
}

fn vm_size() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            let line = s.lines().find(|l| l.starts_with("VmSize:"))?;
            line.split_whitespace().nth(1)?.parse::<u64>().ok()
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

fn cpu_used_secs() -> u64 {
    unsafe {
        let mut u: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut u);
        (u.ru_utime.tv_sec + u.ru_stime.tv_sec) as u64 + 1
    }
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Worker

struct JobWorld<'a> {
    library: &'a LazyHash<Library>,
    fonts: &'a FontStore,
    main: Source,
}

impl World for JobWorld<'_> {
    fn library(&self) -> &LazyHash<Library> {
        self.library
    }
    fn book(&self) -> &LazyHash<FontBook> {
        self.fonts.book()
    }
    fn main(&self) -> FileId {
        self.main.id()
    }
    fn source(&self, id: FileId) -> FileResult<Source> {
        if id == self.main.id() { Ok(self.main.clone()) } else { Err(FileError::AccessDenied) }
    }
    fn file(&self, _: FileId) -> FileResult<Bytes> {
        Err(FileError::AccessDenied)
    }
    fn font(&self, index: usize) -> Option<Font> {
        self.fonts.font(index)
    }
    fn today(&self, _: Option<TDuration>) -> Option<Datetime> {
        Datetime::from_ymd(2026, 1, 1)
    }
}

fn worker_main() {
    // Run everything on one big-stack thread, created before the memory
    // baseline is measured, so its stack is part of the baseline.
    let handle = std::thread::Builder::new()
        .stack_size(64 * MIB as usize)
        .spawn(worker_loop)
        .unwrap();
    let _ = handle.join();
}

fn worker_loop() {
    let mem_budget = env_u64("P2_MEM_BUDGET_MB", 256) * MIB;
    let cpu_budget = env_u64("P2_CPU_BUDGET_S", 5);
    let max_pages = env_u64("P2_MAX_PAGES", 500) as usize;

    let mut fonts = FontStore::new();
    fonts.extend(typst_kit::fonts::embedded());
    let library = LazyHash::new(Library::default());
    let main_id = RootedPath::new(VirtualRoot::Project, VirtualPath::new("main.typ").unwrap()).intern();

    let mut stdin = io::stdin().lock();
    let mut stdout = io::stdout().lock();
    let ready = format!("baseline_vm={}MiB fonts={}", vm_size() / MIB, fonts.book().families().count());
    write_frame(&mut stdout, TAG_READY, ready.as_bytes()).unwrap();

    while let Ok((_, src)) = read_frame(&mut stdin, 16 * MIB as usize) {
        // Arm per-job limits relative to the current state of the process.
        set_soft(libc::RLIMIT_AS, vm_size() + mem_budget);
        set_soft(libc::RLIMIT_CPU, cpu_used_secs() + cpu_budget);

        let text = String::from_utf8_lossy(&src).into_owned();
        let world = JobWorld { library: &library, fonts: &fonts, main: Source::new(main_id, text) };
        let (tag, payload) = match catch_unwind(AssertUnwindSafe(|| compile(&world, max_pages))) {
            Ok(r) => r,
            Err(_) => (TAG_PANIC, b"typst panicked".to_vec()),
        };
        drop(world);
        if write_frame(&mut stdout, tag, &payload).is_err() {
            return;
        }
        typst::comemo::evict(0);
    }
}

fn compile(world: &JobWorld, max_pages: usize) -> (u8, Vec<u8>) {
    let result = typst::compile::<PagedDocument>(world);
    let doc = match result.output {
        Ok(doc) => doc,
        Err(errors) => {
            let msg: Vec<_> = errors.iter().map(|e| e.message.to_string()).collect();
            return (TAG_COMPILE_ERROR, msg.join("; ").into_bytes());
        }
    };
    let pages = doc.pages().len();
    if pages > max_pages {
        return (TAG_TOO_MANY_PAGES, format!("{pages} pages > {max_pages}").into_bytes());
    }
    match typst_pdf::pdf(&doc, &typst_pdf::PdfOptions::default()) {
        Ok(pdf) => (TAG_OK, pdf),
        Err(errors) => {
            let msg: Vec<_> = errors.iter().map(|e| e.message.to_string()).collect();
            (TAG_COMPILE_ERROR, msg.join("; ").into_bytes())
        }
    }
}

// ---------------------------------------------------------------------------
// Supervisor

#[derive(Debug)]
enum Outcome {
    Pdf { bytes: usize },
    CompileError(String),
    TooManyPages(String),
    Timeout(&'static str),
    MemoryLimit,
    Crashed(String),
}

struct Worker {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<io::Result<(u8, Vec<u8>)>>,
    jobs: u64,
}

struct Config {
    wall: Duration,
    jobs_per_worker: u64,
    hard_as: u64,
    hard_cpu: u64,
}

impl Worker {
    fn spawn(cfg: &Config) -> io::Result<(Worker, String)> {
        let hard_as = cfg.hard_as as libc::rlim_t;
        let hard_cpu = cfg.hard_cpu as libc::rlim_t;
        let mut cmd = Command::new(std::env::current_exe()?);
        cmd.arg("worker")
            .env_clear()
            .env("RAYON_NUM_THREADS", "1")
            .env("MALLOC_ARENA_MAX", "2")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for key in ["P2_MEM_BUDGET_MB", "P2_CPU_BUDGET_S", "P2_MAX_PAGES"] {
            if let Ok(v) = std::env::var(key) {
                cmd.env(key, v);
            }
        }
        // Runs after fork in the child: async-signal-safe calls only.
        unsafe {
            cmd.pre_exec(move || {
                libc::setsid();
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
                let set = |r, v| libc::setrlimit(r, &libc::rlimit { rlim_cur: v, rlim_max: v });
                set(libc::RLIMIT_AS, hard_as);
                set(libc::RLIMIT_CPU, hard_cpu);
                set(libc::RLIMIT_NOFILE, 32);
                set(libc::RLIMIT_FSIZE, 0);
                set(libc::RLIMIT_CORE, 0);
                Ok(())
            });
        }
        let mut child = cmd.spawn()?;
        let stdin = child.stdin.take().unwrap();
        let mut stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let r = read_frame(&mut stdout, 256 * MIB as usize);
                let stop = r.is_err();
                if tx.send(r).is_err() || stop {
                    break;
                }
            }
        });
        let ready = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok((TAG_READY, p))) => String::from_utf8_lossy(&p).into_owned(),
            other => return Err(io::Error::other(format!("no ready frame: {other:?}"))),
        };
        Ok((Worker { child, stdin, rx, jobs: 0 }, ready))
    }

    /// Returns the outcome and whether the worker is still usable.
    fn run(&mut self, src: &[u8], cfg: &Config) -> (Outcome, bool) {
        self.jobs += 1;
        if write_frame(&mut self.stdin, TAG_JOB, src).is_err() {
            return (self.reap(), false);
        }
        match self.rx.recv_timeout(cfg.wall) {
            Ok(Ok((tag, p))) => {
                let text = || String::from_utf8_lossy(&p).into_owned();
                let outcome = match tag {
                    TAG_OK => Outcome::Pdf { bytes: p.len() },
                    TAG_COMPILE_ERROR => Outcome::CompileError(text()),
                    TAG_TOO_MANY_PAGES => Outcome::TooManyPages(text()),
                    TAG_PANIC => return (Outcome::Crashed(text()), false),
                    t => Outcome::Crashed(format!("unexpected tag {t}")),
                };
                (outcome, true)
            }
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => (self.reap(), false),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                unsafe { libc::killpg(self.child.id() as i32, libc::SIGKILL) };
                let _ = self.child.wait();
                (Outcome::Timeout("wall"), false)
            }
        }
    }

    fn reap(&mut self) -> Outcome {
        let status = match self.child.wait() {
            Ok(s) => s,
            Err(e) => return Outcome::Crashed(e.to_string()),
        };
        match (status.code(), status.signal()) {
            (Some(EXIT_OOM), _) => Outcome::MemoryLimit,
            (_, Some(libc::SIGXCPU)) => Outcome::Timeout("cpu"),
            (_, Some(libc::SIGKILL)) => Outcome::Timeout("cpu-hard"),
            _ => Outcome::Crashed(format!("{status}")),
        }
    }
}

fn supervise(files: &[String]) {
    let cfg = Config {
        wall: Duration::from_secs(env_u64("P2_WALL_S", 10)),
        jobs_per_worker: env_u64("P2_JOBS_PER_WORKER", 50),
        hard_as: env_u64("P2_HARD_AS_MB", 4096) * MIB,
        hard_cpu: env_u64("P2_HARD_CPU_S", 600),
    };
    let mut worker: Option<Worker> = None;
    let mut spawned = 0;
    for file in files {
        let src = std::fs::read(file).expect("read input");
        if worker.as_ref().is_some_and(|w| w.jobs >= cfg.jobs_per_worker) {
            drop(worker.take()); // closes stdin, worker exits on EOF
        }
        if worker.is_none() {
            let (w, ready) = Worker::spawn(&cfg).expect("spawn worker");
            spawned += 1;
            println!("  [spawn #{spawned} pid={} {ready}]", w.child.id());
            worker = Some(w);
        }
        let start = Instant::now();
        let (outcome, alive) = worker.as_mut().unwrap().run(&src, &cfg);
        let mut line = format!("{outcome:?}");
        line.truncate(110);
        println!("{file:<36} {:>6} ms  {line}", start.elapsed().as_millis());
        if !alive {
            worker = None;
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("worker") => worker_main(),
        Some("supervise") => supervise(&args[1..]),
        _ => eprintln!("usage: p2-isolate supervise <file.typ>..."),
    }
}
