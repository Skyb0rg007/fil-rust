use std::fmt;

use fil_rust_sys::status;

/// Errors reported by fil-rust.
///
/// Memory errors (`OutOfBounds`, `Freed`, ...) are what Fil-C itself would
/// report as a safety panic; fil-rust reports them to Rust instead, so a bad
/// pointer handed back by Fil-C code never takes the process down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Loading the runtime or a library failed.
    Load(String),
    /// The requested flavor or feature isn't available in this build.
    Unsupported(String),
    /// The runtime was already started with a different configuration.
    AlreadyStarted,
    /// The pointer carries no capability (null, or an integer cast to a pointer).
    NullCapability,
    /// The object the pointer refers to has been freed.
    Freed,
    /// The access falls outside the object the pointer's capability allows.
    OutOfBounds,
    /// The object is read-only (for example, a string literal).
    ReadOnly,
    /// A pointer-sized access wasn't pointer-aligned.
    Misaligned,
    /// The pointer refers to a special runtime object (a function, a thread,
    /// ...) whose payload can't be accessed as data.
    SpecialObject,
    /// The pointer doesn't point to a callable function.
    NotAFunction,
    /// A Fil-C (C++) exception propagated out of the called function.
    Exception,
    /// The library has no such symbol.
    MissingSymbol(String),
    /// A string passed to Fil-C contained an interior NUL byte.
    InteriorNul,
    /// A string read from Fil-C memory was not valid UTF-8.
    Utf8,
    /// The callee returned less data than its declared return type needs.
    ReturnTooSmall { expected: usize, actual: usize },
    /// A type can't be passed by value through the Fil-C calling convention.
    UnsupportedAbi(String),
    /// Too many live handles.
    HandleTableFull,
}

impl Error {
    pub(crate) fn from_status(code: i32) -> Error {
        match code {
            status::NULL_CAPABILITY => Error::NullCapability,
            status::SPECIAL => Error::SpecialObject,
            status::FREED => Error::Freed,
            status::OUT_OF_BOUNDS => Error::OutOfBounds,
            status::READONLY => Error::ReadOnly,
            status::MISALIGNED => Error::Misaligned,
            status::NOT_A_FUNCTION => Error::NotAFunction,
            status::EXCEPTION => Error::Exception,
            status::BAD_SLOT => Error::UnsupportedAbi("calling-convention slot out of range".into()),
            other => Error::Load(format!("unexpected shim status {other}")),
        }
    }
}

pub(crate) fn check(code: i32) -> Result<()> {
    if code == status::OK { Ok(()) } else { Err(Error::from_status(code)) }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Load(msg) => write!(f, "load error: {msg}"),
            Error::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            Error::AlreadyStarted => {
                write!(f, "the Fil-C runtime was already started with a different configuration")
            }
            Error::NullCapability => write!(f, "pointer has no capability"),
            Error::Freed => write!(f, "object has been freed"),
            Error::OutOfBounds => write!(f, "access out of bounds"),
            Error::ReadOnly => write!(f, "object is read-only"),
            Error::Misaligned => write!(f, "misaligned pointer access"),
            Error::SpecialObject => write!(f, "pointer refers to a special runtime object"),
            Error::NotAFunction => write!(f, "pointer is not a function"),
            Error::Exception => write!(f, "a Fil-C exception escaped the call"),
            Error::MissingSymbol(name) => write!(f, "symbol not found: {name}"),
            Error::InteriorNul => write!(f, "string contains an interior NUL byte"),
            Error::Utf8 => write!(f, "string is not valid UTF-8"),
            Error::ReturnTooSmall { expected, actual } => {
                write!(f, "callee returned {actual} bytes, expected at least {expected}")
            }
            Error::UnsupportedAbi(msg) => write!(f, "unsupported calling convention use: {msg}"),
            Error::HandleTableFull => write!(f, "handle table is full"),
        }
    }
}

impl std::error::Error for Error {}

/// `Result` with [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
