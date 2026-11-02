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

mod call;
mod callback;
mod error;
mod handle;
mod library;
mod ptr;
mod runtime;
mod types;

pub use call::{DynCall, FilArg, FilFn, FilSig, Function};
pub use callback::Callback;
pub use error::{Error, Result};
pub use fil_rust_sys::Flavor;
pub use handle::{FilBuf, FilHandle};
pub use library::Library;
pub use ptr::{Capability, FilPtr, Opaque, Ptr};
pub use runtime::{Config, Fil, JoinHandle, Runtime};
pub use types::FilType;


/// Items used by the derive macro and [`foreign!`]. Not a stable API.
#[doc(hidden)]
pub mod __private {
    pub use crate::call::{Class, arg_slots, classify};
    pub use crate::types::{CLayout, Leaf, LeafKind, Word, c_layout, raw_add};
    pub use fil_rust_sys::RawPtr;
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
///     }
/// }
///
/// # fn main() -> fil_rust::Result<()> {
/// let rt = Runtime::start_from_env()?;
/// let demo = rt.run(|fil| Demo::load(fil, "libdemo.so"))?;
/// let sum = rt.run(|fil| demo.add(fil, 2, 3))?;
/// # Ok(())
/// # }
/// ```
///
/// The struct holds the [`Library`] and one [`Function`] per declaration,
/// all looked up when it is created (so a missing symbol is reported early).
/// It is `Send + Sync + Clone`. Each method takes the [`Fil`] followed by the
/// arguments as their Rust values ([`FilType::Value`]).
#[macro_export]
macro_rules! foreign {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$fmeta:meta])*
                fn $func:ident ( $($arg:ident : $aty:ty),* $(,)? ) $(-> $ret:ty)? ;
            )*
        }
    ) => {
        $(#[$meta])*
        #[derive(Clone, Debug)]
        $vis struct $name {
            library: $crate::Library,
            $( $func: $crate::Function<fn($($aty),*) -> $crate::foreign!(@ret $($ret)?)>, )*
        }

        #[allow(dead_code)]
        impl $name {
            /// Opens the library at `path` and looks up its functions.
            $vis fn load(fil: &$crate::Fil<'_>, path: impl ::std::convert::AsRef<::std::path::Path>) -> $crate::Result<Self> {
                Self::from_library(fil, fil.load(path)?)
            }

            /// Looks up the functions in an already loaded library.
            $vis fn from_library(fil: &$crate::Fil<'_>, library: $crate::Library) -> $crate::Result<Self> {
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
                ) -> $crate::Result<<$crate::foreign!(@ret $($ret)?) as $crate::FilType>::Value<'f>> {
                    self.$func.call(fil, ($($arg,)*))
                }
            )*
        }
    };
    (@ret) => { () };
    (@ret $ret:ty) => { $ret };
}
