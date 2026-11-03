//! Call [Fil-C](https://fil-c.org/) code from safe Rust.
//!
//! Fil-C is a memory-safe implementation of C and C++. This crate loads the
//! Fil-C runtime into an ordinary (glibc-linked) Rust process, opens Fil-C
//! shared libraries, and calls their functions, passing data back and forth
//! without giving up Fil-C's guarantees: every pointer Rust hands to Fil-C
//! carries a real capability, and every access Rust makes to Fil-C memory is
//! bounds- and liveness-checked.
//!
//! ```no_run
//! use fil_rust::{Ptr, Runtime};
//! use std::ffi::c_char;
//!
//! # fn main() -> fil_rust::Result<()> {
//! let rt = Runtime::start_from_env()?;
//! let lib = rt.load("libdemo.so")?;
//! let n = rt.run(|fil| -> fil_rust::Result<usize> {
//!     let strlen = fil.libc().function::<fn(Ptr<c_char>) -> usize>(fil, "strlen")?;
//!     let s = fil.c_str("hello")?;
//!     strlen.call(fil, (s,))
//! })?;
//! assert_eq!(n, 5);
//! # let _ = lib;
//! # Ok(())
//! # }
//! ```
//!
//! # Model
//!
//! * [`Runtime`] is the process-wide Fil-C runtime. Rust code that touches
//!   Fil-C runs on the runtime's threads via [`Runtime::run`] or
//!   [`Runtime::spawn`], receiving a [`Fil`] token.
//! * [`FilPtr<'f, T>`] is a Fil-C pointer (address + capability), valid in the
//!   scope `'f` of a `Fil`. The garbage collector keeps it alive in that scope;
//!   all reads and writes go through checked copies.
//! * [`FilHandle`] keeps an object alive across scopes and threads;
//!   [`FilBuf`] is a Fil-C buffer that Rust owns exclusively and can access as
//!   a plain slice.
//! * [`Library`] is a loaded Fil-C library. [`Function<Sig>`] is one of its
//!   functions, typed by a Rust `fn` signature over [`FilType`]s.
//!   [`foreign!`] declares a whole interface at once.
//! * [`Callback`] turns a Rust closure into a Fil-C function pointer.
//! * C structs implement [`FilType`] with `#[derive(FilType)]`.
//!
//! # Flavors
//!
//! Fil-C comes in two flavors, which differ in the user libc that Fil-C code
//! links against:
//!
//! * [`Flavor::Musl`] (the `musl` feature, on by default): the "pizfix"
//!   binary distribution. Point [`Config::musl`] at the directory containing
//!   `pizfix/`.
//! * [`Flavor::Glibc`] (the `glibc` feature): the "optfil" distribution,
//!   normally installed at `/opt/fil`. Point [`Config::glibc`] at it. It
//!   exists only for x86-64; on aarch64, only the musl flavor is available.
//!
//! Both features can be enabled at once; the flavor is chosen when the
//! runtime starts, and a process holds one runtime.
//! [`Config::from_env`] reads `FILC_FLAVOR` and the matching root variable.
//! Either way the Rust program itself stays linked to the system's glibc: the
//! Fil-C runtime is loaded next to it, with small built-in stand-ins for the
//! distribution's "yolo" libc.
//!
//! # Threads and blocking
//!
//! Fil-C code only runs on Fil-C threads. [`Runtime::run`] and
//! [`Runtime::spawn`] hand jobs to a pool of executor threads, which grows on
//! demand up to [`Config::max_workers`], so jobs that wait on each other don't
//! starve. Threads that Fil-C code creates itself (`pthread_create`,
//! `std::thread`) work as usual and may call Rust [`Callback`]s.
//!
//! While Rust code holds a [`Fil`], its thread is *entered*: the garbage
//! collector waits for it at every handshake. So **don't block while
//! entered.** Waiting on a lock, channel, condition variable, `OnceLock`, or
//! another thread inside a job can deadlock the whole runtime if the thread
//! you wait for is itself waiting on the collector. Wrap blocking work in
//! [`Fil::exited`], which leaves the runtime for the duration of a closure;
//! long CPU-bound loops that don't call into Fil-C should call
//! [`Fil::pollcheck`] now and then. Calling Fil-C functions that block (a
//! Fil-C `pthread_mutex_lock`, `read`, `sleep`) is fine: Fil-C's own libc
//! exits the runtime around blocking system calls.
//!
//! Some blocking is less obvious. On glibc, the first use on a thread of a
//! `thread_local!` whose value needs dropping registers its destructor under
//! the dynamic loader's lock, which `dlopen` holds while library constructors
//! run; a constructor that creates threads waits for the collector, which
//! waits for every entered thread. So first touch such thread-locals outside
//! the runtime (or inside [`Fil::exited`]). The runtime handles the cases it
//! owns: symbol lookups and library loading run exited, and when the runtime
//! starts it wraps the panic hook installed at that point so that it runs
//! exited (a panic hook installed later replaces that wrapper).
//!
//! A child process made by `fork()` after the runtime started has none of
//! its threads, so [`Runtime::run`], [`Runtime::spawn`] and
//! [`JoinHandle::join`] panic there; values that came from the runtime are
//! leaked when dropped. Starting programs with `std::process::Command` is
//! fine (it uses `posix_spawn` or `exec`s right away).
//!
//! # Errors, panics, and exceptions
//!
//! Checks that Rust code makes (bounds, liveness, alignment, permissions on
//! a [`FilPtr`] access; a symbol that doesn't exist) return an [`Error`].
//! Safety violations that Fil-C code makes are *Fil-C panics*: the runtime
//! prints a diagnostic and ends the process, exactly as it would in a pure
//! Fil-C program. A call whose Rust signature doesn't match the callee (too
//! few arguments, a pointer passed as an integer) is caught by Fil-C's calling
//! convention checks the same way.
//!
//! A C++ exception that escapes a Fil-C function called from Rust becomes
//! [`Error::Exception`] (after destructors on the way run). A Rust panic
//! inside a job propagates to the caller of [`Runtime::run`] or
//! [`JoinHandle::join`]; a Rust panic inside a [`Callback`] can't unwind
//! through C frames, so it becomes a Fil-C panic.
//!
//! # Scopes
//!
//! A [`FilPtr`] is only valid in the scope of the [`Fil`] it came from, and
//! the borrow checker enforces it. It can't escape the closure:
//!
//! ```compile_fail
//! # fn f(rt: fil_rust::Runtime) {
//! let leaked = rt.run(|fil| fil.c_str("hi").unwrap());
//! # }
//! ```
//!
//! A `Fil` can't move to another thread:
//!
//! ```compile_fail
//! # fn f(rt: fil_rust::Runtime) {
//! rt.run(|fil| {
//!     std::thread::scope(|s| {
//!         s.spawn(|| fil.pollcheck());
//!     });
//! });
//! # }
//! ```
//!
//! And Fil-C memory is off limits while exited:
//!
//! ```compile_fail
//! # fn f(rt: fil_rust::Runtime) {
//! rt.run(|fil| {
//!     let p = fil.alloc::<i32>(&1).unwrap();
//!     fil.exited(|| p.read(fil));
//! });
//! # }
//! ```
//!
//! To keep an object across scopes or threads, root it with [`Fil::root`];
//! the [`FilHandle`] it returns is `Send + Sync`:
//!
//! ```no_run
//! # fn f(rt: fil_rust::Runtime) {
//! let greeting = rt.run(|fil| fil.root(fil.c_str("hi").unwrap()).unwrap());
//! std::thread::spawn(move || {
//!     let rt = fil_rust::Runtime::get().unwrap();
//!     rt.run(|fil| assert_eq!(greeting.get(fil).read_string(fil).unwrap(), "hi"));
//! });
//! # }
//! ```

// The calling-convention encoding (struct classification, argument slots)
// follows Fil-C's per-architecture lowering (see call.rs).
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
compile_error!("fil-rust supports only x86-64 and aarch64 Linux");

mod call;
mod callback;
mod error;
mod handle;
mod library;
mod ptr;
mod runtime;
mod types;

pub use call::{DynArgs, DynCall, FilArg, FilFn, FilSig, Function};
pub use callback::Callback;
pub use error::{Error, Result};
pub use fil_rust_sys::Flavor;
pub use handle::{FilBuf, FilHandle};
pub use library::Library;
pub use ptr::{Capability, FilPtr, Opaque, Ptr};
pub use runtime::{Config, Fil, JoinHandle, Runtime};
pub use types::{FilType, Leaf, LeafKind, Place, Word};

#[cfg(feature = "derive")]
pub use fil_rust_macros::FilType;

// Lets the derive macro's `::fil_rust` paths work inside this crate.
extern crate self as fil_rust;

/// Items used by the derive macro and [`foreign!`]. Not a stable API.
#[doc(hidden)]
pub mod __private {
    pub use crate::call::{Class, classify};
    pub use crate::types::{CLayout, Leaf, LeafKind, c_layout};
}

/// Declares a typed interface to a Fil-C library.
///
/// ```no_run
/// use fil_rust::{Ptr, Runtime};
/// use std::ffi::c_char;
///
/// fil_rust::foreign! {
///     /// The demo library.
///     pub struct Demo {
///         fn add(a: i32, b: i32) -> i32;
///         fn greet(name: Ptr<c_char>);
///         /// `int counter;`
///         static counter: i32;
///     }
/// }
///
/// # fn main() -> fil_rust::Result<()> {
/// let rt = Runtime::start_from_env()?;
/// let demo = rt.run(|fil| Demo::load(fil, "libdemo.so"))?;
/// let sum = rt.run(|fil| demo.add(fil, 2, 3))?;
/// rt.run(|fil| demo.counter(fil).write(fil, &sum))?;
/// # Ok(())
/// # }
/// ```
///
/// The struct holds the [`Library`] and one [`Function`] per `fn`
/// declaration, all looked up when it is created (so a missing symbol is
/// reported early). A `static name: T;` declaration becomes a method
/// returning a [`FilPtr<'f, T>`] to the global variable, which is checked to
/// exist (and to be data) at the same time.
/// It is `Send + Sync + Clone`. Each method takes the [`Fil`] followed by the
/// arguments as their Rust values ([`FilType::Value`]).
#[macro_export]
macro_rules! foreign {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident { $($body:tt)* }
    ) => {
        $crate::foreign!(@parse [$(#[$meta])*] [$vis] $name [] [] $($body)*);
    };

    // Sorts the items into functions and globals.
    (@parse $m:tt $v:tt $name:ident [$($fns:tt)*] $statics:tt
        $(#[$fmeta:meta])* fn $func:ident ( $($arg:ident : $aty:ty),* $(,)? ) $(-> $ret:ty)? ;
        $($rest:tt)*
    ) => {
        $crate::foreign!(@parse $m $v $name
            [$($fns)* { [$(#[$fmeta])*] $func [$($arg : $aty),*] [$crate::foreign!(@ret $($ret)?)] }]
            $statics $($rest)*);
    };
    // `static mut` means the same as `static`: writes are checked anyway.
    (@parse $m:tt $v:tt $name:ident $fns:tt $statics:tt
        $(#[$smeta:meta])* static mut $global:ident : $gty:ty ;
        $($rest:tt)*
    ) => {
        $crate::foreign!(@parse $m $v $name $fns $statics
            $(#[$smeta])* static $global : $gty ; $($rest)*);
    };
    (@parse $m:tt $v:tt $name:ident $fns:tt [$($statics:tt)*]
        $(#[$smeta:meta])* static $global:ident : $gty:ty ;
        $($rest:tt)*
    ) => {
        $crate::foreign!(@parse $m $v $name $fns
            [$($statics)* { [$(#[$smeta])*] $global [$gty] }]
            $($rest)*);
    };

    (@parse [$(#[$meta:meta])*] [$vis:vis] $name:ident
        [$({ [$(#[$fmeta:meta])*] $func:ident [$($arg:ident : $aty:ty),*] [$ret:ty] })*]
        [$({ [$(#[$smeta:meta])*] $global:ident [$gty:ty] })*]
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug)]
        $vis struct $name {
            library: $crate::Library,
            $( $func: $crate::Function<fn($($aty),*) -> $ret>, )*
        }

        #[allow(dead_code, clippy::too_many_arguments)]
        impl $name {
            /// Opens the library at `path` and looks up its functions and
            /// globals.
            $vis fn load(fil: &$crate::Fil<'_>, path: impl ::std::convert::AsRef<::std::path::Path>) -> $crate::Result<Self> {
                Self::from_library(fil, fil.load(path)?)
            }

            /// Looks up the functions and globals in an already loaded
            /// library.
            $vis fn from_library(fil: &$crate::Fil<'_>, library: $crate::Library) -> $crate::Result<Self> {
                $( library.global::<$gty>(fil, stringify!($global))?; )*
                Ok($name {
                    $( $func: library.function(fil, stringify!($func))?, )*
                    library,
                })
            }

            /// The underlying library.
            $vis fn library(&self) -> &$crate::Library {
                &self.library
            }

            $(
                $(#[$fmeta])*
                $vis fn $func<'f>(
                    &self,
                    fil: &$crate::Fil<'f>,
                    $($arg: <$aty as $crate::FilType>::Value<'f>),*
                ) -> $crate::Result<<$ret as $crate::FilType>::Value<'f>> {
                    self.$func.call(fil, ($($arg,)*))
                }
            )*

            $(
                $(#[$smeta])*
                $vis fn $global<'f>(&self, fil: &$crate::Fil<'f>) -> $crate::FilPtr<'f, $gty> {
                    self.library
                        .global::<$gty>(fil, stringify!($global))
                        .expect(concat!("global ", stringify!($global), " was found when the library was loaded"))
                }
            )*
        }
    };

    (@ret) => { () };
    (@ret $ret:ty) => { $ret };
}
