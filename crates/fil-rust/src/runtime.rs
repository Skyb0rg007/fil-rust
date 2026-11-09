//! Starting the Fil-C runtime inside this (glibc) process, and running Rust
//! code on its threads.

use fil_rust_rt::shim as rt;
use std::cell::Cell;
use std::collections::VecDeque;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::Duration;

pub use fil_rust_rt::gc::{Collection, HeapStats};
use fil_rust_sys::{FilcThread, Flavor, RawPtr};

use crate::callback::Registry;
use crate::handle::HandleTable;
use crate::library::Library;
use crate::ptr::FilPtr;
use crate::types::{FilType, raw_add};
use crate::{Error, Result, error};

/// Whether the process loads the Fil-C libc. Only the musl flavor has one to
/// load: the glibc flavor's libc is the runtime's own, there from the start,
/// so this makes no difference there.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Libc {
    /// For the first library that imports from it, through [`Runtime::load`], which waits for any
    /// [`Runtime::run`] in progress. Until then, [`Runtime::libc`] has none of
    /// its symbols.
    #[default]
    Auto,
    /// Load it at startup.
    Always,
    /// Never load it.
    Never,
}

/// How to find and start a Fil-C runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    flavor: Flavor,
    root: PathBuf,
    workers: usize,
    max_workers: usize,
    args: Vec<String>,
    search_path: Vec<PathBuf>,
    libc: Libc,
    collection: Collection,
}

impl Config {
    /// A configuration for the distribution of `flavor` installed at `root`.
    ///
    /// For [`Flavor::Musl`], `root` is the directory containing `build/` and
    /// `pizfix/`. For [`Flavor::Glibc`], it is the `/opt/fil` directory
    /// (containing `bin/` and `lib/`).
    pub fn new(flavor: Flavor, root: impl Into<PathBuf>) -> Config {
        Config {
            flavor,
            root: root.into(),
            workers: std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(2, 8),
            max_workers: 256,
            args: vec![
                std::env::args().next().unwrap_or_else(|| "fil-rust".to_string()),
            ],
            search_path: Vec::new(),
            libc: Libc::default(),
            collection: Collection::default(),
        }
    }

    /// A configuration for the musl ("pizfix") distribution at `root`.
    pub fn musl(root: impl Into<PathBuf>) -> Config {
        Config::new(Flavor::Musl, root)
    }

    /// A configuration for the glibc ("/opt/fil") distribution at `root`.
    pub fn glibc(root: impl Into<PathBuf>) -> Config {
        Config::new(Flavor::Glibc, root)
    }

    /// Reads the configuration from the environment:
    ///
    /// * `FILC_FLAVOR`: `musl` (default) or `glibc`;
    /// * `FILC_MUSL_ROOT` (or `FILC_ROOT`) / `FILC_GLIBC_ROOT`: the
    ///   distribution root for the chosen flavor;
    /// * `FIL_RUST_WORKERS`: number of executor threads (optional);
    /// * `FIL_RUST_GC_STRESS`: collect at every nth safepoint and scribble
    ///   over swept memory, to find references the collector missed.
    pub fn from_env() -> Result<Config> {
        let flavor = match std::env::var("FILC_FLAVOR").as_deref() {
            Err(_) | Ok("musl") | Ok("") => Flavor::Musl,
            Ok("glibc") => Flavor::Glibc,
            Ok(other) => return Err(Error::Load(format!("unknown FILC_FLAVOR {other:?}"))),
        };
        Config::from_env_for(flavor)
    }

    /// Like [`Config::from_env`], but for a specific flavor.
    pub fn from_env_for(flavor: Flavor) -> Result<Config> {
        let vars: &[&str] = match flavor {
            Flavor::Musl => &["FILC_MUSL_ROOT", "FILC_ROOT"],
            Flavor::Glibc => &["FILC_GLIBC_ROOT"],
        };
        let root = vars
            .iter()
            .find_map(std::env::var_os)
            .ok_or_else(|| Error::Load(format!("set {} to the Fil-C distribution root", vars[0])))?;
        let mut config = Config::new(flavor, root);
        if let Some(n) = std::env::var("FIL_RUST_WORKERS").ok().and_then(|v| v.parse().ok()) {
            config = config.workers(n);
        }
        Ok(config)
    }

    /// When collections happen.
    pub fn collection(mut self, collection: Collection) -> Config {
        self.collection = collection;
        self
    }

    /// Whether to load the Fil-C libc.
    pub fn libc(mut self, libc: Libc) -> Config {
        self.libc = libc;
        self
    }

    /// Number of Fil-C threads started up front to execute [`Runtime::run`]
    /// jobs (at least 1).
    pub fn workers(mut self, workers: usize) -> Config {
        self.workers = workers.max(1);
        self.max_workers = self.max_workers.max(self.workers);
        self
    }

    /// Upper bound on executor threads. When every executor is busy (for
    /// example, blocked in [`Fil::exited`]) and jobs are waiting, the runtime
    /// starts more executors, up to this many. Default: 256.
    pub fn max_workers(mut self, max_workers: usize) -> Config {
        self.max_workers = max_workers.max(1);
        self.workers = self.workers.min(self.max_workers);
        self
    }

    /// The `argv` the Fil-C side sees.
    pub fn args<I, S>(mut self, args: I) -> Config
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        if self.args.is_empty() {
            self.args.push("fil-rust".to_string());
        }
        self
    }

    /// Adds a directory searched for the dependencies (`DT_NEEDED`) of
    /// libraries opened with [`Runtime::load`].
    ///
    /// A dependency found here is loaded first, and then satisfies the
    /// `DT_NEEDED` entry through its soname, so it must have one
    /// (`-Wl,-soname,libfoo.so`).
    pub fn search_path(mut self, dir: impl Into<PathBuf>) -> Config {
        self.search_path.push(dir.into());
        self
    }

    pub fn flavor(&self) -> Flavor {
        self.flavor
    }

    /// The number of executor threads started up front.
    pub fn worker_count(&self) -> usize {
        self.workers
    }

    /// The maximum number of executor threads.
    pub fn max_worker_count(&self) -> usize {
        self.max_workers
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory holding the runtime (libpizlo) and the Fil-C libc.
    pub fn lib_dir(&self) -> PathBuf {
        match self.flavor {
            Flavor::Musl => self.root.join("pizfix/lib"),
            Flavor::Glibc => self.root.join("lib"),
        }
    }

    pub(crate) fn search_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = self.search_path.clone();
        dirs.push(self.lib_dir());
        dirs
    }

    /// The Fil-C user libc objects, loaded at startup. The first one provides
    /// `__libc_start_main`.
    fn user_libs(&self) -> Vec<PathBuf> {
        let dir = self.lib_dir();
        let names: &[&str] = match self.flavor {
            Flavor::Musl => &["libc.so"],
            Flavor::Glibc => &["libc.so.6666", "libm.so.6666", "libdl.so.6662", "libpthread.so.6660"],
        };
        names.iter().map(|n| dir.join(n)).collect()
    }
}

type Job = Box<dyn for<'f> FnOnce(&Fil<'f>) + Send + 'static>;

/// How Fil-C code gets a thread to run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// On the threads that ask for it, and this many are doing so. There's no
    /// libc for them to call.
    Bare { running: usize },
    /// The libc is on its way, and `Runtime::run` waits for it.
    StartingLibc,
    /// On executors, which the libc knows about.
    Libc,
}

/// The executor pool's shared state.
struct Queue {
    jobs: VecDeque<Job>,
    /// Executors waiting for a job.
    idle: usize,
    /// Executors started or requested.
    total: usize,
    /// Executors requested from the spawner but not yet waiting for jobs.
    starting: usize,
}

pub(crate) struct Inner {
    pub(crate) config: Config,
    /// Where the Fil-C libc stands.
    mode: Mutex<Mode>,
    mode_cv: Condvar,
    /// The Fil-C libc, once it's loaded.
    libc: OnceLock<Library>,
    /// The empty library in its place until then.
    stand_in: Option<Library>,
    pub(crate) runtime_lib: Library,
    queue: Mutex<Queue>,
    queue_cv: Condvar,
    spawn_cv: Condvar,
    ready: Mutex<bool>,
    ready_cv: Condvar,
    pub(crate) handles: HandleTable,
    pub(crate) callbacks: Registry,
}

static INNER: OnceLock<&'static Inner> = OnceLock::new();
static START_LOCK: Mutex<()> = Mutex::new(());

/// Set in the child of a `fork`. The child has only the forking thread: the
/// executors are gone, and locks they held stay held, so the runtime can't
/// be used there.
static FORKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn mark_forked() {
    FORKED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process is the child of a `fork` made after the runtime
/// started.
pub(crate) fn in_forked_child() -> bool {
    FORKED.load(std::sync::atomic::Ordering::Relaxed)
}

fn refuse_in_forked_child() {
    if in_forked_child() {
        panic!("fil-rust: the Fil-C runtime can't be used in a child process made by fork() (only the parent has its threads)");
    }
}

thread_local! {
    /// The Fil-C thread state of the current OS thread, if it is a Fil-C
    /// thread currently running Rust code (an executor or a callback).
    static CURRENT: Cell<*mut FilcThread> = const { Cell::new(std::ptr::null_mut()) };
}

/// The Fil-C libc's handle if it's loaded on the Rust backend, or null.
pub(crate) fn loaded_libc_handle() -> *mut c_void {
    INNER.get().and_then(|inner| inner.libc.get()).map_or(std::ptr::null_mut(), Library::handle)
}

pub(crate) fn current_thread() -> *mut FilcThread {
    CURRENT.with(|c| c.get())
}

pub(crate) fn set_current_thread(thread: *mut FilcThread) -> *mut FilcThread {
    CURRENT.with(|c| c.replace(thread))
}

/// A handle to the process-wide Fil-C runtime.
///
/// The runtime can be started once per process, with one flavor. Rust code
/// that touches Fil-C runs on the runtime's own threads, through
/// [`Runtime::run`] (blocking) or [`Runtime::spawn`]; each job receives a
/// [`Fil`] token for the thread it runs on.
#[derive(Clone, Copy)]
pub struct Runtime {
    inner: &'static Inner,
}

impl std::fmt::Debug for Runtime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runtime").field("config", &self.inner.config).finish()
    }
}

impl Runtime {
    /// Starts the Fil-C runtime, or returns the running one if it was started
    /// with an equivalent configuration.
    ///
    /// Starting the runtime replaces the process's `environ` with a copy (the
    /// Fil-C runtime moves the environment out of the way of the process
    /// title), and it publishes the new array before filling it in. So don't
    /// read or change environment variables on other threads while this runs;
    /// read the configuration first, as [`Runtime::start_from_env`] does.
    pub fn start(config: Config) -> Result<Runtime> {
        let _guard = START_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(inner) = INNER.get() {
            let same = inner.config.flavor == config.flavor && inner.config.root == config.root;
            return if same { Ok(Runtime { inner }) } else { Err(Error::AlreadyStarted) };
        }
        let inner = boot(config)?;
        Ok(Runtime { inner })
    }

    /// Starts the runtime configured by [`Config::from_env`].
    pub fn start_from_env() -> Result<Runtime> {
        Runtime::start(Config::from_env()?)
    }

    /// The running runtime, if it has been started.
    pub fn get() -> Option<Runtime> {
        INNER.get().map(|&inner| Runtime { inner })
    }

    pub(crate) fn inner(self) -> &'static Inner {
        self.inner
    }

    pub fn config(&self) -> &'static Config {
        &self.inner.config
    }

    pub fn flavor(&self) -> Flavor {
        self.inner.config.flavor
    }

    /// The Fil-C user libc, for calling its functions. With [`Libc::Auto`],
    /// this loads it, unless this is inside [`Runtime::run`]. Then it has none
    /// of the symbols.
    pub fn libc(&self) -> &'static Library {
        if self.inner.config.libc == Libc::Auto && self.inner.is_bare() && current_thread().is_null() {
            let _ = self.inner.start_libc();
        }
        self.inner.libc()
    }

    /// The Fil-C runtime library (`libpizlo`), for calling `z*` functions
    /// from `<stdfil.h>`.
    pub fn runtime_library(&self) -> &'static Library {
        &self.inner.runtime_lib
    }

    /// Runs `f` on a Fil-C thread and returns its result.
    ///
    /// If called from a Fil-C thread that is already running Rust code (inside
    /// a job or a callback), `f` runs right there in a nested scope; otherwise
    /// it runs on this thread, or on one of the executors while this thread
    /// waits once the Fil-C libc is loaded. Panics in `f` propagate to the
    /// caller.
    pub fn run<R, F>(&self, f: F) -> R
    where
        F: for<'f> FnOnce(&Fil<'f>) -> R + Send,
        R: Send,
    {
        let thread = current_thread();
        if !thread.is_null() {
            return unsafe { run_inline(self.inner, thread, f) };
        }
        refuse_in_forked_child();
        if self.inner.begin_bare_run() {
            return run_bare(self.inner, f);
        }
        let (tx, rx) = mpsc::sync_channel::<std::thread::Result<R>>(1);
        let job: Box<dyn for<'f> FnOnce(&Fil<'f>) + Send + '_> = Box::new(move |fil| {
            let result = catch_unwind(AssertUnwindSafe(|| f(fil)));
            let _ = tx.send(result);
        });
        // SAFETY: we block below until the job has run to completion (the job
        // always sends, even if `f` panics), so nothing it borrows can be
        // dropped while it is still in use.
        let job: Job = unsafe { std::mem::transmute(job) };
        self.inner.push_job(job);
        match rx.recv() {
            Ok(Ok(value)) => value,
            Ok(Err(panic)) => resume_unwind(panic),
            Err(_) => panic!("fil-rust: executor dropped a job"),
        }
    }

    /// Runs `f` in the background: on a new thread, or on an executor once the
    /// Fil-C libc is loaded.
    pub fn spawn<R, F>(&self, f: F) -> JoinHandle<R>
    where
        F: for<'f> FnOnce(&Fil<'f>) -> R + Send + 'static,
        R: Send + 'static,
    {
        refuse_in_forked_child();
        let (tx, rx) = mpsc::sync_channel::<std::thread::Result<R>>(1);
        if self.inner.is_bare() {
            let runtime = *self;
            std::thread::Builder::new()
                .name("fil-c-job".into())
                .spawn(move || {
                    let result = catch_unwind(AssertUnwindSafe(|| runtime.run(f)));
                    let _ = tx.send(result);
                })
                .expect("fil-rust: spawning a thread");
            return JoinHandle { rx };
        }
        self.inner.push_job(Box::new(move |fil| {
            let result = catch_unwind(AssertUnwindSafe(|| f(fil)));
            let _ = tx.send(result);
        }));
        JoinHandle { rx }
    }

    /// Opens a Fil-C shared library (see [`Fil::load`]), loading the Fil-C libc
    /// first if it needs that and [`Libc::Auto`] allows it.
    pub fn load(&self, path: impl AsRef<Path>) -> Result<Library> {
        let path = path.as_ref().to_path_buf();
        if self.inner.is_bare() {
            let data = std::fs::read(&path).map_err(|e| Error::Load(format!("reading {}: {e}", path.display())))?;
            self.prepare_for(&path.display().to_string(), &data)?;
        }
        self.run(move |fil| fil.load(&path))
    }

    /// Makes sure the libc is there, if a library needs it.
    fn prepare_for(&self, name: &str, data: &[u8]) -> Result<()> {
        if let Some(symbol) = crate::library::libc_import(self.inner, data) {
            if self.inner.config.libc == Libc::Never {
                return Err(Error::Unsupported(format!(
                    "{name} needs {symbol} from the Fil-C libc, which this runtime is configured not to load"
                )));
            }
            self.inner.start_libc()?;
        }
        Ok(())
    }

    /// Loads a library compiled into the program (see [`EmbeddedLibrary`]),
    /// running its constructors.
    ///
    /// [`EmbeddedLibrary`]: crate::EmbeddedLibrary
    pub fn load_embedded(&self, library: &crate::EmbeddedLibrary) -> Result<Library> {
        if self.inner.is_bare()
            && let Some(bytes) = library.bytes(self.inner.config.flavor)
        {
            self.prepare_for(&library.soname(), bytes)?;
        }
        self.run(|fil| fil.load_embedded(library))
    }

    /// Flushes the Fil-C libc's stdio buffers (`fflush(NULL)`). This also
    /// happens automatically when the process exits normally.
    pub fn flush(&self) -> Result<()> {
        self.run(|fil| fil.flush())
    }

    /// Collects garbage now, and returns when that's done. See
    /// [`Fil::collect_garbage`].
    pub fn collect(&self) -> Result<()> {
        self.run(|fil| fil.collect_garbage())
    }

    /// Changes when collections happen.
    pub fn set_collection(&self, collection: Collection) -> Result<()> {
        fil_rust_rt::gc::set_collection(collection);
        Ok(())
    }

    /// The state of the heap.
    pub fn heap_stats(&self) -> Result<HeapStats> {
        Ok(fil_rust_rt::gc::stats())
    }

}

/// The result of [`Runtime::spawn`].
pub struct JoinHandle<R> {
    rx: mpsc::Receiver<std::thread::Result<R>>,
}

impl<R> JoinHandle<R> {
    /// Waits for the job and returns its result, propagating a panic.
    ///
    /// Inside a job or callback, the thread leaves the runtime while it
    /// waits.
    pub fn join(self) -> R {
        refuse_in_forked_child();
        match outside_runtime(|| self.rx.recv()) {
            Ok(Ok(value)) => value,
            Ok(Err(panic)) => resume_unwind(panic),
            Err(_) => panic!("fil-rust: executor dropped a job"),
        }
    }

    /// Waits up to `timeout` for the job.
    pub fn join_timeout(self, timeout: Duration) -> std::result::Result<R, JoinHandle<R>> {
        refuse_in_forked_child();
        match outside_runtime(|| self.rx.recv_timeout(timeout)) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(panic)) => resume_unwind(panic),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("fil-rust: executor dropped a job"),
        }
    }
}

impl Inner {
    fn new(config: Config, libc: Option<Library>, stand_in: Option<Library>, runtime_lib: Library) -> &'static Inner {
        let mode = if libc.is_some() { Mode::Libc } else { Mode::Bare { running: 0 } };
        let cell = OnceLock::new();
        if let Some(libc) = libc {
            let _ = cell.set(libc);
        }
        Box::leak(Box::new(Inner {
            config,
            mode: Mutex::new(mode),
            mode_cv: Condvar::new(),
            libc: cell,
            stand_in,
            runtime_lib,
            queue: Mutex::new(Queue { jobs: VecDeque::new(), idle: 0, total: 0, starting: 0 }),
            queue_cv: Condvar::new(),
            spawn_cv: Condvar::new(),
            ready: Mutex::new(false),
            ready_cv: Condvar::new(),
            handles: HandleTable::new(),
            callbacks: Registry::new(),
        }))
    }

    fn lock_mode(&self) -> std::sync::MutexGuard<'_, Mode> {
        self.mode.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(crate) fn libc(&self) -> &Library {
        self.libc.get().or(self.stand_in.as_ref()).expect("a libc or a stand-in for it")
    }

    /// Whether there's no libc, for now.
    pub(crate) fn is_bare(&self) -> bool {
        matches!(*self.lock_mode(), Mode::Bare { .. } | Mode::StartingLibc)
    }

    /// Whether the real libc is in the process.
    pub(crate) fn has_libc(&self) -> bool {
        self.libc.get().is_some()
    }

    /// Lets a thread that isn't running Fil-C yet run it right there, if that's
    /// how things are done. The caller has to call [`Inner::end_bare_run`].
    fn begin_bare_run(&self) -> bool {
        let mut mode = self.lock_mode();
        loop {
            match *mode {
                Mode::Bare { running } => {
                    *mode = Mode::Bare { running: running + 1 };
                    return true;
                }
                Mode::StartingLibc => mode = self.mode_cv.wait(mode).unwrap_or_else(|e| e.into_inner()),
                Mode::Libc => return false,
            }
        }
    }

    fn end_bare_run(&self) {
        let mut mode = self.lock_mode();
        if let Mode::Bare { running } = *mode {
            *mode = Mode::Bare { running: running - 1 };
            self.mode_cv.notify_all();
        }
    }

    /// Moves the process to the libc and its executors, once no thread is
    /// running Fil-C code without them.
    fn start_libc(&'static self) -> Result<()> {
        if self.config.flavor == Flavor::Glibc {
            return Ok(());
        }
        if !current_thread().is_null() {
            return Err(Error::Unsupported(
                "the Fil-C libc can't start from inside Runtime::run, which has to finish first".into(),
            ));
        }
        {
            let mut mode = self.lock_mode();
            loop {
                match *mode {
                    Mode::Libc => return Ok(()),
                    Mode::StartingLibc => mode = self.mode_cv.wait(mode).unwrap_or_else(|e| e.into_inner()),
                    Mode::Bare { running: 0 } => break,
                    Mode::Bare { .. } => mode = self.mode_cv.wait(mode).unwrap_or_else(|e| e.into_inner()),
                }
            }
            *mode = Mode::StartingLibc;
        }
        let result = unsafe { load_and_start_libc(self) };
        *self.lock_mode() = if result.is_ok() { Mode::Libc } else { Mode::Bare { running: 0 } };
        self.mode_cv.notify_all();
        result
    }

    fn push_job(&self, job: Job) {
        // `spawn` may be called from inside a job.
        let mut queue = lock_anywhere(&self.queue);
        queue.jobs.push_back(job);
        // Grow the pool if every executor is busy, so that jobs that block
        // can't starve the others (or deadlock waiting on them).
        if queue.jobs.len() > queue.idle + queue.starting && queue.total < self.config.max_workers {
            queue.total += 1;
            queue.starting += 1;
            self.spawn_cv.notify_one();
        }
        drop(queue);
        self.queue_cv.notify_one();
    }

    fn next_job(&self, first: bool) -> Job {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if first {
            queue.starting -= 1;
        }
        loop {
            if let Some(job) = queue.jobs.pop_front() {
                return job;
            }
            queue.idle += 1;
            queue = self.queue_cv.wait(queue).unwrap_or_else(|e| e.into_inner());
            queue.idle -= 1;
        }
    }

    /// Blocks until executors have been requested; returns how many.
    fn wait_for_spawn_requests(&self, already_spawned: &mut usize) -> usize {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        while queue.total == *already_spawned {
            queue = self.spawn_cv.wait(queue).unwrap_or_else(|e| e.into_inner());
        }
        let n = queue.total - *already_spawned;
        *already_spawned = queue.total;
        n
    }

    fn spawn_failed(&self, count: usize) {
        let mut queue = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        queue.total -= count;
        queue.starting -= count;
    }

    fn mark_ready(&self) {
        *self.ready.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready_cv.notify_all();
    }

    fn wait_until_ready(&self) -> Result<()> {
        let mut ready = self.ready.lock().unwrap_or_else(|e| e.into_inner());
        while !*ready {
            let (guard, timeout) = self
                .ready_cv
                .wait_timeout(ready, Duration::from_secs(60))
                .unwrap_or_else(|e| e.into_inner());
            ready = guard;
            if timeout.timed_out() && !*ready {
                return Err(Error::Load("the Fil-C runtime did not start".into()));
            }
        }
        Ok(())
    }
}

/// Runs `f` with a fresh `Fil` on `thread`, entering the runtime first if the
/// thread is currently exited.
unsafe fn run_inline<R, F>(inner: &'static Inner, thread: *mut FilcThread, f: F) -> R
where
    F: for<'f> FnOnce(&Fil<'f>) -> R,
{
    let was_entered = unsafe { rt::fr_is_entered(thread) } != 0;
    if !was_entered {
        unsafe { rt::fr_enter(thread) };
    }
    struct Leave(*mut FilcThread, bool);
    impl Drop for Leave {
        fn drop(&mut self) {
            if self.1 {
                unsafe { rt::fr_exit(self.0) };
            }
        }
    }
    let _leave = Leave(thread, !was_entered);
    in_native_frame(thread, || {
        let fil = unsafe { Fil::new(inner, thread) };
        f(&fil)
    })
}

/// Runs `f` on the calling thread, which isn't running Fil-C code yet.
fn run_bare<R, F>(inner: &'static Inner, f: F) -> R
where
    F: for<'f> FnOnce(&Fil<'f>) -> R,
{
    struct Forget(&'static Inner);
    impl Drop for Forget {
        fn drop(&mut self) {
            set_current_thread(std::ptr::null_mut());
            self.0.end_bare_run();
        }
    }
    let _forget = Forget(inner);
    let thread = fil_rust_rt::thread::attach_current().cast::<FilcThread>();
    set_current_thread(thread);
    unsafe {
        run_inline(inner, thread, |fil| {
            inner.handles.flush_pending(fil);
            f(fil)
        })
    }
}

/// Runs `f` inside a new native frame on `thread` (which must be entered).
/// Objects tracked while `f` runs are released afterwards. A panic in `f` is
/// carried across the C frame and resumed here.
pub(crate) fn in_native_frame<R, F: FnOnce() -> R>(thread: *mut FilcThread, f: F) -> R {
    struct Payload<F, R> {
        f: Option<F>,
        result: Option<std::thread::Result<R>>,
    }

    unsafe extern "C" fn trampoline<F: FnOnce() -> R, R>(arg: *mut c_void) {
        let payload = unsafe { &mut *(arg as *mut Payload<F, R>) };
        let f = payload.f.take().expect("trampoline called once");
        payload.result = Some(catch_unwind(AssertUnwindSafe(f)));
    }

    let mut payload = Payload { f: Some(f), result: None };
    unsafe {
        rt::fr_with_native_frame(
            thread,
            trampoline::<F, R>,
            (&mut payload as *mut Payload<F, R>).cast(),
        );
    }
    match payload.result.expect("native frame body ran") {
        Ok(value) => value,
        Err(panic) => resume_unwind(panic),
    }
}

unsafe extern "C" fn thread_main_entry(thread: *mut FilcThread, is_main: c_int) {
    let inner = *INNER.get().expect("runtime initialized before executors start");
    // Name the thread for debuggers and `ps`.
    let name: &CStr = if is_main != 0 { c"fil-c-main" } else { c"fil-c-worker" };
    unsafe { libc::prctl(libc::PR_SET_NAME, name.as_ptr()) };
    // On the main thread too: it calls into Fil-C (to start executors), and
    // the code that leaves the runtime before blocking (`outside_runtime`,
    // `lock_anywhere`) finds the thread to exit here. Without it, the main
    // thread would look up `pthread_create` with dlsym while entered, and
    // block a collection behind the loader lock whenever a library
    // constructor creating threads holds it.
    set_current_thread(thread);
    if is_main != 0 {
        // The main Fil-C thread starts the executors, then stays around to
        // start more on demand (only a Fil-C thread can create Fil-C threads).
        let mut spawned = {
            let mut queue = inner.queue.lock().unwrap_or_else(|e| e.into_inner());
            queue.total = inner.config.workers;
            queue.starting = inner.config.workers;
            queue.total
        };
        let mut request = spawned;
        loop {
            in_native_frame(thread, || {
                let fil = unsafe { Fil::new(inner, thread) };
                if let Err(e) = spawn_workers(&fil, request) {
                    eprintln!("fil-rust: failed to start Fil-C executor threads: {e}");
                    inner.spawn_failed(request);
                    spawned -= request;
                }
            });
            inner.mark_ready();
            unsafe { rt::fr_exit(thread) };
            request = inner.wait_for_spawn_requests(&mut spawned);
            unsafe { rt::fr_enter(thread) };
        }
    }
    let mut first = true;
    loop {
        unsafe { rt::fr_exit(thread) };
        let job = inner.next_job(first);
        first = false;
        unsafe { rt::fr_enter(thread) };
        in_native_frame(thread, || {
            let fil = unsafe { Fil::new(inner, thread) };
            inner.handles.flush_pending(&fil);
            job(&fil);
        });
    }
}

/// Starts `count` executor threads with the Fil-C libc's `pthread_create`.
fn spawn_workers(fil: &Fil<'_>, count: usize) -> Result<()> {
    let pthread_create = fil
        .libc()
        .function::<fn(crate::Ptr, crate::Ptr, crate::Ptr, crate::Ptr) -> i32>(fil, "pthread_create")?;
    let worker = FilPtr::from_raw(unsafe { rt::fr_worker_function() });
    for _ in 0..count {
        let tid = fil.alloc_bytes(16)?;
        let rc = pthread_create.call(fil, (tid.cast(), FilPtr::null(), worker, FilPtr::null()))?;
        if rc != 0 {
            return Err(Error::Load(format!("pthread_create failed with error {rc}")));
        }
    }
    Ok(())
}

extern "C" fn flush_at_exit() {
    // Fil-C's own exit() has already flushed if we are on a Fil-C thread.
    if !current_thread().is_null() {
        return;
    }
    if let Some(rt) = Runtime::get() {
        let _ = rt.spawn(|fil| fil.flush()).join_timeout(Duration::from_secs(5));
    }
}

/// Loads the Fil-C libc into the process, and starts its
/// `main`. Nothing may be running Fil-C code. What's already loaded stays, and
/// libraries keep binding to the stand-in by name, finding the real one's
/// symbols in the global scope.
unsafe fn load_and_start_libc(inner: &'static Inner) -> Result<()> {
    use fil_rust_sys::{dlopen_path, dlsym_checked};

    let config = &inner.config;
    fil_rust_rt::globals::defer_ctors();
    let mut handles = Vec::new();
    for path in config.user_libs() {
        handles.push(unsafe { dlopen_path(&path, libc::RTLD_NOW | libc::RTLD_GLOBAL) }.map_err(Error::Load)?);
    }
    let libc_start_main: fil_rust_rt::abi::GetterFn = unsafe {
        std::mem::transmute(dlsym_checked(handles[0], "pizlonated___libc_start_main\0").map_err(Error::Load)?)
    };
    let main: fil_rust_rt::abi::GetterFn = unsafe { std::mem::transmute(rt::fr_main_getter as *const ()) };
    let _ = inner.libc.set(Library::from_handle(handles[0], config.user_libs()[0].clone()));
    let args: Vec<Vec<u8>> = config.args.iter().map(|arg| arg.clone().into_bytes()).collect();
    std::thread::Builder::new()
        .name("fil-c-main".into())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(move || unsafe {
            let args: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
            fil_rust_rt::start::start_program(&args, libc_start_main, main)
        })
        .map_err(|e| Error::Load(format!("spawning the Fil-C main thread: {e}")))?;
    inner.wait_until_ready()?;
    unsafe { libc::atexit(flush_at_exit) };
    Ok(())
}

/// Library constructors run on this stack, so it's as big as a real `main`'s.
const MAIN_STACK_SIZE: usize = 8 << 20;

/// Starts the runtime, which is only a matter of loading it: threads attach
/// when they first run Fil-C code.
fn boot(config: Config) -> Result<&'static Inner> {
    let flavor = config.flavor;
    // Nothing is loaded from the distribution yet, but libraries find their
    // dependencies there: a wrong root should fail here, not at the first load.
    let marker = config.lib_dir().join("libpizlo.so");
    if !marker.is_file() {
        return Err(Error::Load(format!(
            "{} isn't a Fil-C {flavor:?} distribution: there's no {}",
            config.root.display(),
            marker.display()
        )));
    }
    if flavor == Flavor::Glibc {
        fil_rust_rt::globals::set_ctor_args(config.args.iter().map(|arg| arg.clone().into_bytes()).collect());
    }
    let installed = fil_rust_rt::install(flavor).map_err(Error::Load)?;
    fil_rust_rt::gc::set_collection(config.collection);
    crate::library::check_no_foreign_runtime(&config, false)?;
    let stand_in = Library::from_handle(installed.libc as *mut c_void, PathBuf::from("libc.so"));
    let runtime_lib = Library::from_handle(installed.runtime as *mut c_void, PathBuf::from("libpizlo.so"));
    let always = config.libc == Libc::Always;
    let inner = Inner::new(config, None, Some(stand_in), runtime_lib);
    let _ = INNER.set(inner);
    unsafe { libc::pthread_atfork(None, None, Some(mark_forked)) };
    install_panic_hook();
    unsafe { rt::fr_init(thread_main_entry, crate::callback::callback_entry) };
    if always {
        inner.start_libc()?;
    }
    Ok(inner)
}

fn refuse_libc_import(inner: &Inner, name: &str, data: &[u8]) -> Result<()> {
    match crate::library::libc_import(inner, data) {
        Some(symbol) => Err(Error::Unsupported(format!(
            "{name} needs {symbol} from the Fil-C libc, which isn't loaded; \
             load the library with Runtime::load, outside Runtime::run"
        ))),
        None => Ok(()),
    }
}

/// Panics in jobs and callbacks happen on entered threads, and the panic hook
/// blocks: it writes to stderr, and on glibc the thread-locals it touches first
/// register their destructors under the dynamic loader's lock (held while
/// library constructors run, which may wait for this thread). So the hook runs
/// outside the runtime.
fn install_panic_hook() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| outside_runtime(|| hook(info))));
}

/// Proof that the current thread is a Fil-C thread inside the runtime, plus
/// the scope `'f` that keeps the [`FilPtr`]s it hands out alive.
///
/// You get a `Fil` from [`Runtime::run`], [`Runtime::spawn`], or as the first
/// argument of a [`Callback`](crate::Callback). It can't leave that closure,
/// and it can't move to another thread.
pub struct Fil<'f> {
    pub(crate) inner: &'static Inner,
    pub(crate) thread: *mut FilcThread,
    _brand: Brand<'f>,
}

/// Makes `'f` invariant and `Fil` `!Send + !Sync`.
type Brand<'f> = PhantomData<(fn(&'f ()) -> &'f (), *mut ())>;

/// Chunk size for copies between Rust and Fil-C memory; the runtime gets a
/// chance to reach a safepoint between chunks.
const COPY_CHUNK: usize = 64 * 1024;

impl<'f> Fil<'f> {
    pub(crate) unsafe fn new(inner: &'static Inner, thread: *mut FilcThread) -> Fil<'f> {
        Fil { inner, thread, _brand: PhantomData }
    }

    /// The runtime this thread belongs to.
    pub fn runtime(&self) -> Runtime {
        Runtime { inner: self.inner }
    }

    /// Runs `f` in a nested scope. Pointers created inside are released when
    /// it returns, which bounds memory use in long loops.
    pub fn scope<R>(&self, f: impl for<'g> FnOnce(&Fil<'g>) -> R) -> R {
        let inner = self.inner;
        let thread = self.thread;
        in_native_frame(thread, || {
            let fil = unsafe { Fil::new(inner, thread) };
            f(&fil)
        })
    }

    /// Leaves the runtime while running `f`, so that blocking or
    /// long-running Rust code doesn't hold up the garbage collector.
    ///
    /// `f` must be `Send`, so it can't capture this `Fil` or any `FilPtr`:
    /// Fil-C memory is off limits while exited. Pointers from the enclosing
    /// scope stay alive.
    pub fn exited<R>(&self, f: impl FnOnce() -> R + Send) -> R {
        unsafe { rt::fr_exit(self.thread) };
        let result = catch_unwind(AssertUnwindSafe(f));
        unsafe { rt::fr_enter(self.thread) };
        match result {
            Ok(value) => value,
            Err(panic) => resume_unwind(panic),
        }
    }

    /// Lets the runtime reach a safepoint (garbage collection handshakes,
    /// deferred signals). Call it in long-running loops that don't otherwise
    /// call into Fil-C.
    pub fn pollcheck(&self) {
        unsafe { rt::fr_pollcheck(self.thread) };
    }

    /// The Fil-C user libc.
    pub fn libc(&self) -> &'static Library {
        self.inner.libc()
    }

    /// The Fil-C runtime library (`libpizlo`).
    pub fn runtime_library(&self) -> &'static Library {
        &self.inner.runtime_lib
    }

    /// Opens a Fil-C shared library.
    ///
    /// Dependencies that the dynamic loader wouldn't find on its own are looked
    /// up in the configured search path and the distribution's `lib` directory.
    ///
    /// Without the Fil-C libc, this fails if the library imports from it, as
    /// this thread may hold pointers across the change: see [`Runtime::load`].
    pub fn load(&self, path: impl AsRef<Path>) -> Result<Library> {
        let path = path.as_ref().to_path_buf();
        let inner = self.inner;
        if inner.is_bare() {
            let data = std::fs::read(&path).map_err(|e| Error::Load(format!("reading {}: {e}", path.display())))?;
            refuse_libc_import(inner, &path.display().to_string(), &data)?;
        }
        // Library constructors enter the runtime themselves, so the loader
        // must run while this thread is exited.
        self.exited(move || crate::library::open(inner, &path))
    }

    /// Loads a library compiled into the program (see [`EmbeddedLibrary`])
    /// from an anonymous memory file, with the same checks as
    /// [`Fil::load`]. Loading the same library again returns it again.
    ///
    /// [`EmbeddedLibrary`]: crate::EmbeddedLibrary
    pub fn load_embedded(&self, library: &crate::EmbeddedLibrary) -> Result<Library> {
        let inner = self.inner;
        let library = *library;
        if inner.is_bare()
            && let Some(bytes) = library.bytes(inner.config.flavor)
        {
            refuse_libc_import(inner, &library.soname(), bytes)?;
        }
        self.exited(move || crate::library::open_embedded(inner, &library))
    }

    /// Allocates `len` zeroed bytes of Fil-C memory.
    pub fn alloc_bytes(&self, len: usize) -> Result<FilPtr<'f, u8>> {
        Ok(FilPtr::from_raw(self.alloc_raw(len, 16)?))
    }

    /// Allocates memory for `count` zeroed elements of `T`.
    pub fn alloc_zeroed<T: FilType>(&self, count: usize) -> Result<FilPtr<'f, T>> {
        let size = count.checked_mul(T::SIZE).ok_or(Error::OutOfBounds)?;
        Ok(FilPtr::from_raw(self.alloc_raw(size, T::ALIGN)?))
    }

    /// Allocates a `T` initialized to `value`.
    pub fn alloc<T: FilType>(&self, value: &T::Value<'f>) -> Result<FilPtr<'f, T>> {
        let ptr = self.alloc_zeroed::<T>(1)?;
        ptr.write(self, value)?;
        Ok(ptr)
    }

    /// Allocates an array initialized from `values`.
    pub fn alloc_array<T: FilType>(&self, values: &[T::Value<'f>]) -> Result<FilPtr<'f, T>> {
        let ptr = self.alloc_zeroed::<T>(values.len())?;
        ptr.write_array(self, values)?;
        Ok(ptr)
    }

    /// Allocates a copy of `bytes`.
    pub fn bytes(&self, bytes: &[u8]) -> Result<FilPtr<'f, u8>> {
        let ptr = self.alloc_bytes(bytes.len())?;
        ptr.write_bytes(self, bytes)?;
        Ok(ptr)
    }

    /// Allocates a NUL-terminated copy of `s`.
    pub fn c_str(&self, s: &str) -> Result<FilPtr<'f, c_char>> {
        if s.as_bytes().contains(&0) {
            return Err(Error::InteriorNul);
        }
        let ptr = self.alloc_bytes(s.len() + 1)?;
        ptr.write_bytes(self, s.as_bytes())?;
        Ok(ptr.cast())
    }

    /// Allocates a copy of `s` (including its NUL terminator).
    pub fn c_string(&self, s: &CStr) -> Result<FilPtr<'f, c_char>> {
        Ok(self.bytes(s.to_bytes_with_nul())?.cast())
    }

    /// Collects garbage on this thread and returns when that's done, unless
    /// the [`Collection`] is [`Collection::Never`]. Every thread has to reach a
    /// safepoint first, so one that's entered and never calls into Fil-C or
    /// [`Fil::pollcheck`] holds this up.
    pub fn collect_garbage(&self) -> Result<()> {
        unsafe { fil_rust_rt::gc::collect(self.thread.cast()) };
        Ok(())
    }

    /// `fflush(NULL)` in the Fil-C libc, if there is one.
    pub fn flush(&self) -> Result<()> {
        if !self.inner.has_libc() {
            return Ok(());
        }
        let fflush = self.libc().function::<fn(crate::Ptr) -> i32>(self, "fflush")?;
        fflush.call(self, (FilPtr::null(),))?;
        Ok(())
    }

    /// The Fil-C runtime's version number (`zversion()`).
    pub fn version(&self) -> Result<u32> {
        let zversion = self.runtime_library().function::<fn() -> u32>(self, "zversion")?;
        zversion.call(self, ())
    }

    // ---- raw memory operations, shared by the typed API ----

    pub(crate) fn alloc_raw(&self, size: usize, align: usize) -> Result<RawPtr> {
        if size > isize::MAX as usize / 2 || !align.is_power_of_two() {
            return Err(Error::OutOfBounds);
        }
        Ok(unsafe { rt::fr_allocate(self.thread, size, align) })
    }

    pub(crate) fn copy_out_raw(&self, at: RawPtr, buf: &mut [u8]) -> Result<()> {
        error::check(unsafe { rt::fr_check(at, buf.len(), 0) })?;
        let mut done = 0;
        while done < buf.len() {
            let n = (buf.len() - done).min(COPY_CHUNK);
            error::check(unsafe {
                rt::fr_copy_out(self.thread, raw_add(at, done), buf[done..].as_mut_ptr().cast(), n)
            })?;
            done += n;
            if done < buf.len() {
                self.pollcheck();
            }
        }
        Ok(())
    }

    pub(crate) fn copy_in_raw(&self, at: RawPtr, bytes: &[u8]) -> Result<()> {
        error::check(unsafe { rt::fr_check(at, bytes.len(), 1) })?;
        let mut done = 0;
        while done < bytes.len() {
            let n = (bytes.len() - done).min(COPY_CHUNK);
            error::check(unsafe {
                rt::fr_copy_in(self.thread, raw_add(at, done), bytes[done..].as_ptr().cast(), n)
            })?;
            done += n;
            if done < bytes.len() {
                self.pollcheck();
            }
        }
        Ok(())
    }

    pub(crate) fn memmove_raw(&self, dst: RawPtr, src: RawPtr, len: usize) -> Result<()> {
        error::check(unsafe { rt::fr_memmove(self.thread, dst, src, len) })
    }

    pub(crate) fn strlen_raw(&self, at: RawPtr) -> Result<usize> {
        let mut len = 0usize;
        error::check(unsafe { rt::fr_strlen(at, &mut len) })?;
        Ok(len)
    }

    pub(crate) fn load_ptr_raw(&self, at: RawPtr) -> Result<RawPtr> {
        let mut out = RawPtr::NULL;
        error::check(unsafe { rt::fr_load_ptr(self.thread, at, &mut out) })?;
        Ok(out)
    }

    pub(crate) fn store_ptr_raw(&self, at: RawPtr, value: RawPtr) -> Result<()> {
        error::check(unsafe { rt::fr_store_ptr(self.thread, at, value) })
    }

    /// Acquires `mutex` without blocking the garbage collector: if it's
    /// contended, the thread leaves the runtime while it waits.
    pub(crate) fn lock<'m, T>(&self, mutex: &'m Mutex<T>) -> std::sync::MutexGuard<'m, T> {
        match mutex.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                unsafe { rt::fr_exit(self.thread) };
                let guard = mutex.lock().unwrap_or_else(|e| e.into_inner());
                unsafe { rt::fr_enter(self.thread) };
                guard
            }
        }
    }
}

/// Runs blocking code `f`. If this is a Fil-C thread inside the runtime,
/// it leaves the runtime while `f` runs (a thread must never block while
/// entered; see [`lock_anywhere`]).
pub(crate) fn outside_runtime<R>(f: impl FnOnce() -> R) -> R {
    let thread = current_thread();
    if thread.is_null() || unsafe { rt::fr_is_entered(thread) } == 0 {
        return f();
    }
    unsafe { rt::fr_exit(thread) };
    let result = catch_unwind(AssertUnwindSafe(f));
    unsafe { rt::fr_enter(thread) };
    match result {
        Ok(value) => value,
        Err(panic) => resume_unwind(panic),
    }
}

/// Acquires `mutex` from any thread (for `Drop` impls, which have no `Fil`).
///
/// A thread inside the runtime must never block while entered: the garbage
/// collector's handshakes would wait for it, and lock holders that re-enter
/// the runtime could deadlock with it. So on such a thread, a contended lock
/// is waited for outside the runtime.
pub(crate) fn lock_anywhere<'m, T>(mutex: &'m Mutex<T>) -> std::sync::MutexGuard<'m, T> {
    match mutex.try_lock() {
        Ok(guard) => guard,
        Err(std::sync::TryLockError::Poisoned(e)) => e.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => {
            let thread = current_thread();
            let entered = !thread.is_null() && unsafe { rt::fr_is_entered(thread) } != 0;
            if entered {
                unsafe { rt::fr_exit(thread) };
            }
            let guard = mutex.lock().unwrap_or_else(|e| e.into_inner());
            if entered {
                unsafe { rt::fr_enter(thread) };
            }
            guard
        }
    }
}
