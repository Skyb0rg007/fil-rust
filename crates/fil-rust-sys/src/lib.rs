//! Raw, `unsafe` bindings for hosting the Fil-C runtime inside a glibc process.
//!
//! This crate embeds two kinds of C support libraries, built by `build.rs`
//! for each enabled Fil-C flavor:
//!
//! * **compat** libraries, which stand in for Fil-C's yolo libc and forward to
//!   the host glibc, so that glibc's own `ld.so` can load the Fil-C runtime;
//! * the **runtime shim** (`fr_*` functions), a flat C API over the runtime's
//!   inline internals, compiled against the matching runtime headers.
//!
//! Everything here is unsafe and low level; the `fil-rust` crate builds the
//! safe API on top of it.

#![allow(clippy::missing_safety_doc)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
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

/// Version of the `fr_*` API this crate expects from the shim.
pub const SHIM_ABI_VERSION: u32 = 2;

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

/// What the shim reports about a pointer's capability.
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

/// A pizlonated getter: the `pizlonated_<symbol>` export of a Fil-C object.
pub type Getter = unsafe extern "C" fn(*mut FilcThread, *const c_void) -> RawPtr;
/// Rust entry point for executor threads (`is_main` is 1 for the Fil-C main thread).
pub type ThreadMainFn = unsafe extern "C" fn(*mut FilcThread, c_int);
/// Rust entry point for callbacks; returns the size of the return values written.
pub type CallbackFn = unsafe extern "C" fn(*mut FilcThread, u64, usize) -> usize;
/// `filc_start_program` from libpizlo.
pub type StartProgramFn = unsafe extern "C" fn(c_int, *mut *mut c_char, Getter, Getter);

macro_rules! shim_api {
    ($($name:ident : fn($($arg:ty),*) $(-> $ret:ty)?;)*) => {
        /// Function table of the runtime shim, resolved with `dlsym`.
        #[derive(Clone, Copy)]
        pub struct Shim {
            $(pub $name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)*
        }

        impl Shim {
            /// Resolves every `fr_*` function from a `dlopen` handle of the shim.
            pub unsafe fn load(handle: *mut c_void) -> Result<Shim, String> {
                Ok(Shim {
                    $($name: unsafe {
                        let sym = dlsym_checked(handle, concat!(stringify!($name), "\0"))?;
                        std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(sym)
                    },)*
                })
            }
        }
    };
}

shim_api! {
    fr_abi_version: fn() -> u32;
    fr_sizeof_thread: fn() -> usize;
    fr_runtime_is_glibc_flavor: fn() -> c_int;
    fr_init: fn(ThreadMainFn, CallbackFn);
    fr_main_getter: fn(*mut FilcThread, *const c_void) -> RawPtr;
    fr_my_thread: fn() -> *mut FilcThread;
    fr_check_thread_layout: fn(*mut FilcThread) -> c_int;
    fr_enter: fn(*mut FilcThread);
    fr_exit: fn(*mut FilcThread);
    fr_is_entered: fn(*mut FilcThread) -> c_int;
    fr_pollcheck: fn(*mut FilcThread);
    fr_with_native_frame: fn(*mut FilcThread, unsafe extern "C" fn(*mut c_void), *mut c_void);
    fr_worker_function: fn() -> RawPtr;
    fr_panic: fn(*const c_char);
    fr_args_check_failure: fn(usize, usize);
    fr_root_capacity: fn() -> usize;
    fr_root_set: fn(*mut FilcThread, usize, RawPtr) -> c_int;
    fr_root_get: fn(*mut FilcThread, usize, *mut RawPtr) -> c_int;
    fr_ptr_info_get: fn(RawPtr, *mut PtrInfo) -> c_int;
    fr_check: fn(RawPtr, usize, c_int) -> c_int;
    fr_allocate: fn(*mut FilcThread, usize, usize) -> RawPtr;
    fr_copy_in: fn(*mut FilcThread, RawPtr, *const c_void, usize) -> c_int;
    fr_copy_out: fn(*mut FilcThread, RawPtr, *mut c_void, usize) -> c_int;
    fr_memmove: fn(*mut FilcThread, RawPtr, RawPtr, usize) -> c_int;
    fr_strlen: fn(RawPtr, *mut usize) -> c_int;
    fr_load_ptr: fn(*mut FilcThread, RawPtr, *mut RawPtr) -> c_int;
    fr_store_ptr: fn(*mut FilcThread, RawPtr, RawPtr) -> c_int;
    fr_track: fn(*mut FilcThread, RawPtr);
    fr_call_getter: fn(*mut FilcThread, *mut c_void) -> RawPtr;
    fr_check_function: fn(RawPtr) -> c_int;
    fr_cc_begin: fn(*mut FilcThread, usize);
    fr_cc_set_word: fn(*mut FilcThread, usize, usize, u64) -> c_int;
    fr_cc_set_ptr: fn(*mut FilcThread, usize, usize, RawPtr) -> c_int;
    fr_cc_get_word: fn(*mut FilcThread, usize, usize, *mut u64) -> c_int;
    fr_cc_get_ptr: fn(*mut FilcThread, usize, usize, *mut RawPtr) -> c_int;
    fr_call: fn(*mut FilcThread, RawPtr, usize, *mut usize) -> c_int;
    fr_closure_new: fn(*mut FilcThread, u64) -> RawPtr;
}

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
static GLIBC_COMPAT: [SupportLib; 4] = [
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
    SupportLib {
        soname: "libpizlo.so",
        bytes: include_bytes!(concat!(env!("OUT_DIR"), "/compat_glibc_pizlo.so")),
    },
];

impl Flavor {
    /// Compat libraries to load (in order, `RTLD_GLOBAL`) before anything from
    /// the Fil-C distribution. `None` if this flavor's support wasn't built.
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

    /// The runtime shim built for this flavor, if support was built.
    pub fn shim(self) -> Option<&'static [u8]> {
        match self {
            #[cfg(feature = "musl")]
            Flavor::Musl => Some(include_bytes!(concat!(env!("OUT_DIR"), "/shim_musl.so"))),
            #[cfg(feature = "glibc")]
            Flavor::Glibc => Some(include_bytes!(concat!(env!("OUT_DIR"), "/shim_glibc.so"))),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    /// Imports of libpizlo to redirect from the host libc to a function of
    /// a compat library: `(imported symbol, replacement)`, the replacement
    /// living in the compat library at [`Flavor::stand_in_index`]. These are
    /// the calls where running next to Rust, on the host libc, needs
    /// different behavior.
    pub fn import_redirects(self) -> &'static [(&'static str, &'static str)] {
        match self {
            #[cfg(target_arch = "x86_64")]
            Flavor::Musl => &[
                // musl zeroes the padding in 64-bit cmsghdrs; glibc has none.
                ("sendmsg", "fil_rust_musl_sendmsg\0"),
                ("sendmmsg", "fil_rust_musl_sendmmsg\0"),
                // Signals that land on Rust threads (compat_signals.inc).
                ("sigaction", "fil_rust_sigaction\0"),
            ],
            #[cfg(target_arch = "aarch64")]
            Flavor::Musl => &[
                ("sendmsg", "fil_rust_musl_sendmsg\0"),
                ("sendmmsg", "fil_rust_musl_sendmmsg\0"),
                ("sigaction", "fil_rust_sigaction\0"),
                // glibc's aarch64 pthread_attr_t and pthread_mutex_t are 8
                // bytes larger than musl's, and their init functions clear
                // the whole glibc size (compat_musl.c).
                ("pthread_attr_init", "fil_rust_musl_pthread_attr_init\0"),
                ("pthread_attr_destroy", "fil_rust_musl_pthread_attr_destroy\0"),
                ("pthread_attr_getstack", "fil_rust_musl_pthread_attr_getstack\0"),
                ("pthread_attr_setstacksize", "fil_rust_musl_pthread_attr_setstacksize\0"),
                ("pthread_getattr_np", "fil_rust_musl_pthread_getattr_np\0"),
                ("pthread_create", "fil_rust_musl_pthread_create\0"),
                ("pthread_mutex_init", "fil_rust_musl_pthread_mutex_init\0"),
            ],
            Flavor::Glibc => &[("sigaction", "fil_rust_sigaction\0")],
        }
    }

    /// Index in [`Flavor::compat_libs`] of the stand-in for the yolo libc,
    /// which holds the redirect targets and `fil_rust_signals_init`.
    pub fn stand_in_index(self) -> usize {
        match self {
            Flavor::Musl => 0,
            Flavor::Glibc => 1,
        }
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

/// Points the GOT entries through which the loaded object `path` imports
/// each `(symbol, address)` of `redirects` at `address` instead, so that just
/// that object's calls go elsewhere. Fails if the object doesn't import one
/// of the symbols. Returns the number of entries patched.
///
/// # Safety
///
/// Each address must be a function compatible with the symbol it replaces,
/// and nothing may be calling through those entries concurrently.
pub unsafe fn redirect_imports(
    path: &std::path::Path,
    redirects: &[(&str, *const c_void)],
) -> Result<usize, String> {
    struct Search<'a> {
        path: &'a [u8],
        found: Option<(usize, *const libc::Elf64_Phdr, usize)>,
    }
    unsafe extern "C" fn find(info: *mut libc::dl_phdr_info, _size: usize, data: *mut c_void) -> c_int {
        unsafe {
            let search = &mut *(data as *mut Search);
            let info = &*info;
            if !info.dlpi_name.is_null() && CStr::from_ptr(info.dlpi_name).to_bytes() == search.path {
                search.found = Some((info.dlpi_addr as usize, info.dlpi_phdr, info.dlpi_phnum as usize));
                return 1;
            }
            0
        }
    }
    #[repr(C)]
    struct Elf64Dyn {
        d_tag: i64,
        d_un: u64,
    }
    use std::os::unix::ffi::OsStrExt;
    let mut search = Search { path: path.as_os_str().as_bytes(), found: None };
    unsafe { libc::dl_iterate_phdr(Some(find), (&mut search as *mut Search).cast()) };
    let (base, phdr, phnum) = search.found.ok_or_else(|| format!("{} is not loaded", path.display()))?;
    let phdrs = unsafe { std::slice::from_raw_parts(phdr, phnum) };

    // glibc relocates the address-valued dynamic entries in place; be
    // robust to loaders that don't.
    let addr = |v: u64| -> usize { if (v as usize) < base { base + v as usize } else { v as usize } };
    let dynamic = phdrs.iter().find(|p| p.p_type == libc::PT_DYNAMIC).ok_or("no PT_DYNAMIC")?;
    let relro = phdrs.iter().find(|p| p.p_type == libc::PT_GNU_RELRO).map(|p| {
        let start = base + p.p_vaddr as usize;
        (start, start + p.p_memsz as usize)
    });
    let (mut strtab, mut symtab, mut rela, mut relasz, mut jmprel, mut pltrelsz) = (0, 0, 0, 0, 0, 0);
    let mut d = (base + dynamic.p_vaddr as usize) as *const Elf64Dyn;
    loop {
        let entry = unsafe { &*d };
        match entry.d_tag {
            0 => break,
            5 => strtab = addr(entry.d_un),  // DT_STRTAB
            6 => symtab = addr(entry.d_un),  // DT_SYMTAB
            7 => rela = addr(entry.d_un),    // DT_RELA
            8 => relasz = entry.d_un as usize, // DT_RELASZ
            23 => jmprel = addr(entry.d_un), // DT_JMPREL
            2 => pltrelsz = entry.d_un as usize, // DT_PLTRELSZ
            _ => {}
        }
        d = unsafe { d.add(1) };
    }
    if strtab == 0 || symtab == 0 {
        return Err("no symbol table".into());
    }

    #[cfg(target_arch = "x86_64")]
    const R_GLOB_DAT: u32 = 6; // R_X86_64_GLOB_DAT
    #[cfg(target_arch = "x86_64")]
    const R_JUMP_SLOT: u32 = 7; // R_X86_64_JUMP_SLOT
    #[cfg(target_arch = "aarch64")]
    const R_GLOB_DAT: u32 = 1025; // R_AARCH64_GLOB_DAT
    #[cfg(target_arch = "aarch64")]
    const R_JUMP_SLOT: u32 = 1026; // R_AARCH64_JUMP_SLOT
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
    let mut patched = Vec::new();
    for (table, size) in [(rela, relasz), (jmprel, pltrelsz)] {
        if table == 0 {
            continue;
        }
        let relocs = unsafe { std::slice::from_raw_parts(table as *const libc::Elf64_Rela, size / 24) };
        for r in relocs {
            let kind = (r.r_info & 0xffff_ffff) as u32;
            if kind != R_GLOB_DAT && kind != R_JUMP_SLOT {
                continue;
            }
            let sym = unsafe { &*(symtab as *const libc::Elf64_Sym).add((r.r_info >> 32) as usize) };
            let name = unsafe { CStr::from_ptr((strtab + sym.st_name as usize) as *const c_char) };
            let Some((symbol, target)) = redirects.iter().find(|(n, _)| n.as_bytes() == name.to_bytes()) else {
                continue;
            };
            let slot = base + r.r_offset as usize;
            let in_relro = relro.is_some_and(|(s, e)| slot >= s && slot < e);
            let page_start = slot & !(page - 1);
            unsafe {
                if in_relro && libc::mprotect(page_start as *mut c_void, page, libc::PROT_READ | libc::PROT_WRITE) != 0 {
                    return Err(format!("mprotect: {}", std::io::Error::last_os_error()));
                }
                std::ptr::write_volatile(slot as *mut *const c_void, *target);
                if in_relro {
                    libc::mprotect(page_start as *mut c_void, page, libc::PROT_READ);
                }
            }
            patched.push(*symbol);
        }
    }
    if let Some((missing, _)) = redirects.iter().find(|(n, _)| !patched.contains(n)) {
        return Err(format!("{} does not import {missing}", path.display()));
    }
    Ok(patched.len())
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
