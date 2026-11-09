//! Starting the Fil-C runtime inside this (glibc) process, and running Rust
//! code on its threads.

use fil_rust_rt::shim as rt;
use std::cell::Cell;
use std::ffi::{CStr, c_char, c_void};
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

pub use fil_rust_rt::gc::{Collection, HeapStats};
use fil_rust_sys::{FilcThread, Flavor, RawPtr};

use crate::callback::Registry;
use crate::handle::HandleTable;
use crate::library::Library;
use crate::ptr::FilPtr;
use crate::types::{FilType, raw_add};
use crate::{Error, Result, error};

/// How to find and start a Fil-C runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    flavor: Flavor,
    root: PathBuf,
    args: Vec<String>,
    search_path: Vec<PathBuf>,
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
            args: vec![
                std::env::args().next().unwrap_or_else(|| "fil-rust".to_string()),
            ],
            search_path: Vec::new(),
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
        Ok(Config::new(flavor, root))
    }

    /// When collections happen.
    pub fn collection(mut self, collection: Collection) -> Config {
        self.collection = collection;
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

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The distribution's library directory, where libraries' dependencies
    /// are looked for last.
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

}

pub(crate) struct Inner {
    pub(crate) config: Config,
    /// The libc stand-in: the runtime's own libc for the glibc flavor, and a
    /// library with no symbols for musl.
    libc: Library,
    pub(crate) runtime_lib: Library,
    pub(crate) handles: HandleTable,
    pub(crate) callbacks: Registry,
}

static INNER: OnceLock<&'static Inner> = OnceLock::new();
static START_LOCK: Mutex<()> = Mutex::new(());

/// Set in the child of a `fork` made from Rust. The child has only the forking
/// thread: the others are still in the runtime's thread list, and locks they
/// held stay held, so the runtime can't be used there.
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
    /// The Fil-C thread state of the current OS thread, while it runs Rust
    /// code for Fil-C (in `Runtime::run` or a callback).
    static CURRENT: Cell<*mut FilcThread> = const { Cell::new(std::ptr::null_mut()) };
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
/// that touches Fil-C runs through [`Runtime::run`], on the calling thread, or
/// [`Runtime::spawn`], on a new one; each gets a [`Fil`] token for the thread
/// it runs on.
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
    /// with an equivalent configuration. Starting loads the runtime and the
    /// libc stand-ins; it starts no threads, and runs no Fil-C code.
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

    /// The libc, for calling its functions: for the glibc flavor, the
    /// runtime's own, which Rust shares; the musl flavor has none, and this
    /// has none of the symbols.
    pub fn libc(&self) -> &'static Library {
        &self.inner.libc
    }

    /// The Fil-C runtime library (`libpizlo`), for calling `z*` functions
    /// from `<stdfil.h>`.
    pub fn runtime_library(&self) -> &'static Library {
        &self.inner.runtime_lib
    }

    /// Runs `f` on this thread, as a Fil-C thread, and returns its result.
    ///
    /// The thread joins the runtime the first time, and stays in its thread
    /// list until it exits. Inside a job or a callback, `f` runs in a nested
    /// scope. Panics in `f` propagate to the caller.
    pub fn run<R, F>(&self, f: F) -> R
    where
        F: for<'f> FnOnce(&Fil<'f>) -> R,
    {
        let thread = current_thread();
        if !thread.is_null() {
            return unsafe { run_inline(self.inner, thread, f) };
        }
        refuse_in_forked_child();
        run_bare(self.inner, f)
    }

    /// Runs `f` on a new thread.
    pub fn spawn<R, F>(&self, f: F) -> JoinHandle<R>
    where
        F: for<'f> FnOnce(&Fil<'f>) -> R + Send + 'static,
        R: Send + 'static,
    {
        refuse_in_forked_child();
        let (tx, rx) = mpsc::sync_channel::<std::thread::Result<R>>(1);
        let runtime = *self;
        std::thread::Builder::new()
            .name("fil-c-job".into())
            .spawn(move || {
                let result = catch_unwind(AssertUnwindSafe(|| runtime.run(f)));
                let _ = tx.send(result);
            })
            .expect("fil-rust: spawning a thread");
        JoinHandle { rx }
    }

    /// Opens a Fil-C shared library (see [`Fil::load`]).
    pub fn load(&self, path: impl AsRef<Path>) -> Result<Library> {
        let path = path.as_ref().to_path_buf();
        self.run(move |fil| fil.load(&path))
    }

    /// Loads a library compiled into the program (see [`EmbeddedLibrary`]),
    /// running its constructors.
    ///
    /// [`EmbeddedLibrary`]: crate::EmbeddedLibrary
    pub fn load_embedded(&self, library: &crate::EmbeddedLibrary) -> Result<Library> {
        self.run(|fil| fil.load_embedded(library))
    }

    /// Flushes the Fil-C streams (`fflush(NULL)`). This also happens when the
    /// process exits normally.
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
            Err(_) => panic!("fil-rust: a job's thread went away"),
        }
    }

    /// Waits up to `timeout` for the job.
    pub fn join_timeout(self, timeout: Duration) -> std::result::Result<R, JoinHandle<R>> {
        refuse_in_forked_child();
        match outside_runtime(|| self.rx.recv_timeout(timeout)) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(panic)) => resume_unwind(panic),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(self),
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("fil-rust: a job's thread went away"),
        }
    }
}

impl Inner {
    fn new(config: Config, libc: Library, runtime_lib: Library) -> &'static Inner {
        Box::leak(Box::new(Inner { config, libc, runtime_lib, handles: HandleTable::new(), callbacks: Registry::new() }))
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
    struct Forget;
    impl Drop for Forget {
        fn drop(&mut self) {
            set_current_thread(std::ptr::null_mut());
        }
    }
    let _forget = Forget;
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
    fil_rust_rt::globals::set_ctor_args(config.args.iter().map(|arg| arg.clone().into_bytes()).collect());
    let installed = fil_rust_rt::install(flavor).map_err(Error::Load)?;
    fil_rust_rt::gc::set_collection(config.collection);
    crate::library::check_no_foreign_runtime(&config)?;
    let libc = Library::from_handle(installed.libc as *mut c_void, PathBuf::from("libc.so"));
    let runtime_lib = Library::from_handle(installed.runtime as *mut c_void, PathBuf::from("libpizlo.so"));
    let inner = Inner::new(config, libc, runtime_lib);
    let _ = INNER.set(inner);
    unsafe { libc::pthread_atfork(None, None, Some(mark_forked)) };
    install_panic_hook();
    unsafe { rt::fr_init(crate::callback::callback_entry) };
    Ok(inner)
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

    /// The libc: see [`Runtime::libc`].
    pub fn libc(&self) -> &'static Library {
        &self.inner.libc
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
    /// For the musl flavor, which has no libc, this fails if the library
    /// imports from one.
    pub fn load(&self, path: impl AsRef<Path>) -> Result<Library> {
        let path = path.as_ref().to_path_buf();
        let inner = self.inner;
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

    /// `fflush(NULL)` in the libc, if there is one.
    pub fn flush(&self) -> Result<()> {
        if !self.libc().has_symbol("fflush") {
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
