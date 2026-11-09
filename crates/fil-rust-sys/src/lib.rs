//! Raw, `unsafe` bindings for hosting the Fil-C runtime inside a glibc process.
//!
//! This crate embeds the libraries `build.rs` builds for each enabled Fil-C
//! flavor: **compat** libraries with the sonames and version nodes of Fil-C's
//! yolo libc, the generated runtime **stub** (`libpizlo.so`), stand-ins for the
//! Fil-C libc, and the Fil-C-compiled **support** library. `fil-rust-rt` loads
//! them and fills in the stub; the `fil-rust` crate builds the safe API on top.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_int, c_void};
use std::io::Write;
use std::os::fd::FromRawFd;

/// Status codes returned by `fr_*` functions.
pub mod status {
    pub const OK: i32 = 0;
    pub const NULL_CAPABILITY: i32 = 1;
    pub const SPECIAL: i32 = 2;
    pub const FREED: i32 = 3;
    pub const OUT_OF_BOUNDS: i32 = 4;
    pub const READONLY: i32 = 5;
    pub const MISALIGNED: i32 = 6;
    pub const NOT_A_FUNCTION: i32 = 7;
    pub const EXCEPTION: i32 = 8;
    pub const BAD_SLOT: i32 = 9;
}

/// Object flags (`FILC_OBJECT_FLAG_*`).
pub mod flags {
    pub const GLOBAL: u32 = 1;
    pub const READONLY: u32 = 2;
    pub const FREE: u32 = 4;
    pub const MMAP: u32 = 8;
}

/// Special object types (`FILC_SPECIAL_TYPE_*`).
pub mod special {
    pub const NONE: u32 = 0;
    pub const FUNCTION: u32 = 1;
}

/// A Fil-C flight pointer: an address plus the capability (`lower`) that
/// grants access through it. Layout-compatible with the runtime's `filc_ptr`.
#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawPtr {
    pub ptr: *mut c_void,
    pub lower: *mut c_void,
}

impl RawPtr {
    /// The null pointer (no address, no capability).
    pub const NULL: RawPtr = RawPtr { ptr: std::ptr::null_mut(), lower: std::ptr::null_mut() };

    /// A pointer with the given address and no capability. It can be passed
    /// around but never dereferenced.
    pub const fn from_addr(addr: usize) -> RawPtr {
        RawPtr { ptr: addr as *mut c_void, lower: std::ptr::null_mut() }
    }
}

/// The runtime's per-thread state (`filc_thread`). Opaque.
#[repr(C)]
pub struct FilcThread {
    _private: [u8; 0],
}

/// What the runtime reports about a pointer's capability.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PtrInfo {
    pub lower: *mut c_void,
    pub upper: *mut c_void,
    pub flags: u32,
    pub special_type: u32,
}

impl Default for PtrInfo {
    fn default() -> Self {
        PtrInfo {
            lower: std::ptr::null_mut(),
            upper: std::ptr::null_mut(),
            flags: 0,
            special_type: 0,
        }
    }
}

/// Rust entry point for callbacks; returns the size of the return values written.
pub type CallbackFn = unsafe extern "C" fn(*mut FilcThread, u64, usize) -> usize;
/// A Fil-C distribution flavor, distinguished by which libc its Fil-C
/// binaries (and its runtime) were built against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// The musl-based "pizfix" distribution (`filc-<v>-linux-<arch>.tar.xz`).
    Musl,
    /// The glibc-based "/opt/fil" distribution (`optfil-<v>-linux-<arch>.tar.xz`).
    Glibc,
}

/// An embedded support library.
#[derive(Clone, Copy, Debug)]
pub struct SupportLib {
    /// The soname the library is built with; Fil-C objects bind to it by name.
    pub soname: &'static str,
    pub bytes: &'static [u8],
}

#[cfg(feature = "musl")]
static MUSL_COMPAT: [SupportLib; 1] = [SupportLib {
    soname: "libyoloc.so",
    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/compat_musl_libyoloc.so")),
}];

#[cfg(feature = "glibc")]
static GLIBC_COMPAT: [SupportLib; 3] = [
    SupportLib {
        soname: "ld-fil1-x86_64.so",
        bytes: include_bytes!(concat!(env!("OUT_DIR"), "/compat_glibc_ld.so")),
    },
    SupportLib {
        soname: "libyolocimpl.so",
        bytes: include_bytes!(concat!(env!("OUT_DIR"), "/compat_glibc_libyolocimpl.so")),
    },
    SupportLib {
        soname: "libyolomimpl.so",
        bytes: include_bytes!(concat!(env!("OUT_DIR"), "/compat_glibc_libyolomimpl.so")),
    },
];

impl Flavor {
    /// Compat libraries to load (in order, `RTLD_GLOBAL`) before any compiled
    /// library, so their sonames and version nodes satisfy its `DT_NEEDED` and
    /// `VERNEED` entries. `None` if this flavor's support wasn't built.
    pub fn compat_libs(self) -> Option<&'static [SupportLib]> {
        match self {
            #[cfg(feature = "musl")]
            Flavor::Musl => Some(&MUSL_COMPAT),
            #[cfg(feature = "glibc")]
            Flavor::Glibc => Some(&GLIBC_COMPAT),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// The generated `libpizlo.so` that stands in for the real runtime when
    /// the Rust runtime runs this flavor: a table of jump slots, filled in by
    /// `fil-rust-rt`, under every symbol compiled code can bind to.
    pub fn runtime_stub(self) -> Option<SupportLib> {
        match self {
            #[cfg(feature = "musl")]
            Flavor::Musl => Some(SupportLib {
                soname: "libpizlo.so",
                bytes: include_bytes!(concat!(env!("OUT_DIR"), "/stub_musl.so")),
            }),
            #[cfg(feature = "glibc")]
            Flavor::Glibc => Some(SupportLib {
                soname: "libpizlo.so",
                bytes: include_bytes!(concat!(env!("OUT_DIR"), "/stub_glibc.so")),
            }),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Libraries under the sonames of the Fil-C libc, which take its place
    /// when the process runs without it. glibc's have a jump slot under every
    /// symbol and version the real ones export, for the Rust runtime to fill
    /// in; musl's has only the soname.
    pub fn libc_stand_ins(self) -> &'static [SupportLib] {
        match self {
            #[cfg(feature = "musl")]
            Flavor::Musl => &[SupportLib {
                soname: "libc.so",
                bytes: include_bytes!(concat!(env!("OUT_DIR"), "/name_only_libc.so")),
            }],
            #[cfg(feature = "glibc")]
            Flavor::Glibc => &[
                SupportLib {
                    soname: "libc.so.6666",
                    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/stand_in_libc_6666.so")),
                },
                SupportLib {
                    soname: "libm.so.6666",
                    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/stand_in_libm_6666.so")),
                },
                SupportLib {
                    soname: "libdl.so.6662",
                    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/stand_in_libdl_6662.so")),
                },
                SupportLib {
                    soname: "libpthread.so.6660",
                    bytes: include_bytes!(concat!(env!("OUT_DIR"), "/stand_in_libpthread_6660.so")),
                },
            ],
            #[allow(unreachable_patterns)]
            _ => &[],
        }
    }

    /// The Fil-C-compiled part of the runtime: musl's linked from the
    /// distribution's `libpizlo.a`, glibc's compiled from the vendored source.
    /// `None` if the musl distribution has no archive.
    pub fn support_library(self) -> Option<SupportLib> {
        let bytes: &'static [u8] = match self {
            #[cfg(feature = "musl")]
            Flavor::Musl => include_bytes!(concat!(env!("OUT_DIR"), "/support_musl.so")),
            #[cfg(feature = "glibc")]
            Flavor::Glibc => include_bytes!(concat!(env!("OUT_DIR"), "/support_glibc.so")),
            #[allow(unreachable_patterns)]
            _ => &[],
        };
        (!bytes.is_empty()).then_some(SupportLib { soname: "libfilrust_support.so", bytes })
    }

    /// Name of the yolo-libc soname(s) of the real distribution. If one of
    /// these is mapped from a file instead of our compat stand-in, the second
    /// libc would corrupt the process.
    pub fn real_yolo_libc_names(self) -> &'static [&'static str] {
        match self {
            #[cfg(target_arch = "x86_64")]
            Flavor::Musl => &["libyoloc.so", "ld-fil1-x86_64.so"],
            #[cfg(target_arch = "aarch64")]
            Flavor::Musl => &["libyoloc.so", "ld-fil1-aarch64.so"],
            Flavor::Glibc => &["libyolocimpl.so", "libyolomimpl.so", "ld-fil1-x86_64.so"],
        }
    }
}

/// The last `dlerror()` message.
pub fn dlerror_string() -> String {
    unsafe {
        let msg = libc::dlerror();
        if msg.is_null() {
            "unknown dynamic loader error".to_string()
        } else {
            CStr::from_ptr(msg).to_string_lossy().into_owned()
        }
    }
}

/// `dlsym` that reports a readable error. `name` must be NUL-terminated.
pub unsafe fn dlsym_checked(handle: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    debug_assert!(name.ends_with('\0'));
    unsafe {
        libc::dlerror();
        let sym = libc::dlsym(handle, name.as_ptr().cast());
        if sym.is_null() {
            Err(format!("symbol {} not found: {}", name.trim_end_matches('\0'), dlerror_string()))
        } else {
            Ok(sym)
        }
    }
}

/// `dlopen` by path.
pub unsafe fn dlopen_path(path: &std::path::Path, flags: c_int) -> Result<*mut c_void, String> {
    use std::os::unix::ffi::OsStrExt;
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("path contains NUL: {}", path.display()))?;
    unsafe {
        let handle = libc::dlopen(c.as_ptr(), flags);
        if handle.is_null() {
            Err(format!("dlopen({}) failed: {}", path.display(), dlerror_string()))
        } else {
            Ok(handle)
        }
    }
}

/// Writes `bytes` to an anonymous memory file named `label` and returns its
/// path under `/proc/self/fd`. The file descriptor is kept open for the life
/// of the process, so the path stays valid.
pub fn memfd_path(label: &str, bytes: &[u8]) -> Result<std::path::PathBuf, String> {
    let name = CString::new(label).map_err(|_| "label contains NUL".to_string())?;
    let fd = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("memfd_create failed: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: the descriptor was just created and nothing else owns it.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes).map_err(|e| format!("writing {label}: {e}"))?;
    // Keep the descriptor open: the loader maps the file by path.
    std::mem::forget(file);
    Ok(std::path::PathBuf::from(format!("/proc/self/fd/{fd}")))
}

/// Loads an embedded shared object: writes it to an anonymous memory file
/// ([`memfd_path`]) and `dlopen`s it through `/proc/self/fd`.
pub unsafe fn dlopen_bytes(label: &str, bytes: &[u8], flags: c_int) -> Result<*mut c_void, String> {
    let path = memfd_path(label, bytes)?;
    unsafe { dlopen_path(&path, flags) }
}

/// Paths of every object currently mapped by the dynamic loader.
pub fn loaded_objects() -> Vec<String> {
    unsafe extern "C" fn collect(
        info: *mut libc::dl_phdr_info,
        _size: usize,
        data: *mut c_void,
    ) -> c_int {
        unsafe {
            let names = &mut *(data as *mut Vec<String>);
            let name = (*info).dlpi_name;
            if !name.is_null() {
                names.push(CStr::from_ptr(name).to_string_lossy().into_owned());
            }
        }
        0
    }
    let mut names: Vec<String> = Vec::new();
    unsafe {
        libc::dl_iterate_phdr(Some(collect), (&mut names as *mut Vec<String>).cast());
    }
    names
}
