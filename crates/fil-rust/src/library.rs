//! Fil-C shared libraries.

use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};

use fil_rust_sys::{Flavor, RawPtr, dlerror_string, dlopen_path, loaded_objects};

use crate::call::{FilFn, FilSig, Function};
use crate::ptr::{FilPtr, Opaque};
use crate::runtime::Inner;
use crate::types::FilType;
use crate::{Error, Fil, Result};

/// A loaded Fil-C shared library. Libraries are never unloaded, so this is a
/// cheap, shareable handle.
pub struct Library {
    handle: *mut c_void,
    path: PathBuf,
}

// SAFETY: a dlopen handle may be used from any thread; the library is never
// closed.
unsafe impl Send for Library {}
unsafe impl Sync for Library {}

impl std::fmt::Debug for Library {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Library").field("path", &self.path).finish()
    }
}

impl Clone for Library {
    fn clone(&self) -> Self {
        Library { handle: self.handle, path: self.path.clone() }
    }
}

impl Library {
    pub(crate) fn from_handle(handle: *mut c_void, path: PathBuf) -> Library {
        Library { handle, path }
    }

    /// The path the library was opened from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn getter(&self, name: &str) -> Result<*mut c_void> {
        let symbol = CString::new(format!("pizlonated_{name}"))
            .map_err(|_| Error::MissingSymbol(name.to_string()))?;
        unsafe {
            libc::dlerror();
            let getter = libc::dlsym(self.handle, symbol.as_ptr());
            if getter.is_null() {
                let _ = dlerror_string();
                Err(Error::MissingSymbol(name.to_string()))
            } else {
                Ok(getter)
            }
        }
    }

    /// Whether the library (or one of its dependencies) defines the Fil-C
    /// symbol `name`.
    pub fn has_symbol(&self, name: &str) -> bool {
        self.getter(name).is_ok()
    }

    /// The address of the symbol `name` (a function or a global variable).
    pub fn symbol<'f>(&self, fil: &Fil<'f>, name: &str) -> Result<FilPtr<'f, Opaque>> {
        let getter = self.getter(name)?;
        let raw: RawPtr = unsafe { (fil.shim().fr_call_getter)(fil.thread, getter) };
        Ok(FilPtr::from_raw(raw))
    }

    /// The function `name`, with signature `Sig`.
    ///
    /// The signature is your claim about the C prototype. If it's wrong, the
    /// call fails safely: the callee rejects missing arguments with a Fil-C
    /// panic and treats mistyped pointers as capability-less.
    pub fn function<Sig: FilSig>(&self, fil: &Fil<'_>, name: &str) -> Result<Function<Sig>> {
        let ptr = self.symbol(fil, name)?;
        Function::from_global(FilFn::<Sig>::from_ptr(ptr)?)
    }

    /// A pointer to the global variable `name`, of C type `T`.
    pub fn global<'f, T: FilType>(&self, fil: &Fil<'f>, name: &str) -> Result<FilPtr<'f, T>> {
        let ptr = self.symbol(fil, name)?;
        match ptr.capability() {
            Some(cap) if cap.special => Err(Error::SpecialObject),
            Some(_) => Ok(ptr.cast()),
            None => Err(Error::NullCapability),
        }
    }
}

/// Fails if the real yolo libc of the distribution got mapped into the
/// process. That would put two C libraries in one address space.
pub(crate) fn check_no_real_yolo_libc(flavor: Flavor) -> Result<()> {
    for name in loaded_objects() {
        if name.starts_with("/proc/self/fd/") {
            continue;
        }
        let base = name.rsplit('/').next().unwrap_or(&name);
        if flavor.real_yolo_libc_names().contains(&base) {
            return Err(Error::Load(format!(
                "the distribution's own yolo libc ({name}) was loaded into the process; \
                 Fil-C objects must only be loaded through fil-rust"
            )));
        }
    }
    Ok(())
}

fn is_loaded(name: &str) -> bool {
    let Ok(c) = CString::new(name) else { return false };
    unsafe {
        let handle = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
        if handle.is_null() {
            libc::dlerror();
            false
        } else {
            libc::dlclose(handle);
            true
        }
    }
}

fn preload_dependencies(
    search_dirs: &[PathBuf],
    path: &Path,
    visited: &mut Vec<PathBuf>,
) -> Result<()> {
    if visited.iter().any(|p| p == path) {
        return Ok(());
    }
    visited.push(path.to_path_buf());
    let Ok(data) = std::fs::read(path) else { return Ok(()) };
    for needed in elf::needed(&data) {
        if is_loaded(&needed) {
            continue;
        }
        let Some(found) = search_dirs.iter().map(|d| d.join(&needed)).find(|p| p.is_file()) else {
            continue;
        };
        preload_dependencies(search_dirs, &found, visited)?;
        unsafe { dlopen_path(&found, libc::RTLD_NOW | libc::RTLD_LOCAL) }.map_err(Error::Load)?;
    }
    Ok(())
}

/// Libraries that only one flavor's Fil-C objects are linked against.
fn flavor_libraries(flavor: Flavor) -> &'static [&'static str] {
    match flavor {
        Flavor::Musl => &["libc.so", "libyoloc.so"],
        Flavor::Glibc => &[
            "libc.so.6666",
            "libm.so.6666",
            "libdl.so.6662",
            "libpthread.so.6660",
            "libyolocimpl.so",
            "libyolomimpl.so",
        ],
    }
}

/// Checks that `path` is a Fil-C library of `flavor` before anything in it
/// runs. Loading a native library would run its native constructors and
/// hand out its symbols as if they were checked Fil-C code; loading one of
/// the other flavor would bring a second Fil-C libc into the process.
pub(crate) fn check_library(flavor: Flavor, path: &Path) -> Result<()> {
    let data = std::fs::read(path).map_err(|e| Error::Load(format!("reading {}: {e}", path.display())))?;
    if !data.starts_with(b"\x7fELF") {
        return Err(Error::Load(format!("{} is not an ELF shared library", path.display())));
    }
    let needed = elf::needed(&data);
    if !needed.iter().any(|n| n == "libpizlo.so") {
        return Err(Error::Load(format!(
            "{} is not a Fil-C library (it isn't linked against the Fil-C runtime, libpizlo.so)",
            path.display()
        )));
    }
    let other = match flavor {
        Flavor::Musl => Flavor::Glibc,
        Flavor::Glibc => Flavor::Musl,
    };
    if let Some(lib) = needed.iter().find(|n| flavor_libraries(other).contains(&n.as_str())) {
        return Err(Error::Load(format!(
            "{} was built for the {other:?} flavor of Fil-C (it needs {lib}), but the runtime runs the {flavor:?} flavor",
            path.display()
        )));
    }
    Ok(())
}

/// Opens a library. Must run on a Fil-C thread that is *exited*: Fil-C
/// constructors enter the runtime themselves.
pub(crate) fn open(inner: &Inner, path: &Path) -> Result<Library> {
    if !path.is_file() {
        return Err(Error::Load(format!("{} does not exist", path.display())));
    }
    check_library(inner.config.flavor(), path)?;
    let mut visited = Vec::new();
    preload_dependencies(&inner.config.search_dirs(), path, &mut visited)?;
    let handle = unsafe { dlopen_path(path, libc::RTLD_NOW | libc::RTLD_LOCAL) }.map_err(Error::Load)?;
    check_no_real_yolo_libc(inner.config.flavor())?;
    Ok(Library::from_handle(handle, path.to_path_buf()))
}

/// Just enough ELF parsing to list `DT_NEEDED` entries.
pub(crate) mod elf {
    fn u16_at(d: &[u8], o: usize) -> Option<u16> {
        Some(u16::from_le_bytes(d.get(o..o + 2)?.try_into().ok()?))
    }
    fn u32_at(d: &[u8], o: usize) -> Option<u32> {
        Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
    }
    fn u64_at(d: &[u8], o: usize) -> Option<u64> {
        Some(u64::from_le_bytes(d.get(o..o + 8)?.try_into().ok()?))
    }

    const PT_LOAD: u32 = 1;
    const PT_DYNAMIC: u32 = 2;
    const DT_NULL: u64 = 0;
    const DT_NEEDED: u64 = 1;
    const DT_STRTAB: u64 = 5;

    /// The `DT_NEEDED` names of a 64-bit little-endian ELF file (empty if it
    /// isn't one).
    pub fn needed(d: &[u8]) -> Vec<String> {
        parse(d).unwrap_or_default()
    }

    fn parse(d: &[u8]) -> Option<Vec<String>> {
        if d.get(0..4)? != b"\x7fELF" || *d.get(4)? != 2 || *d.get(5)? != 1 {
            return None;
        }
        let phoff = u64_at(d, 0x20)? as usize;
        let phentsize = u16_at(d, 0x36)? as usize;
        let phnum = u16_at(d, 0x38)? as usize;
        let mut loads = Vec::new();
        let mut dynamic = None;
        for i in 0..phnum {
            let ph = phoff + i * phentsize;
            let kind = u32_at(d, ph)?;
            let offset = u64_at(d, ph + 8)?;
            let vaddr = u64_at(d, ph + 16)?;
            let filesz = u64_at(d, ph + 32)?;
            match kind {
                PT_LOAD => loads.push((vaddr, offset, filesz)),
                PT_DYNAMIC => dynamic = Some((offset as usize, filesz as usize)),
                _ => {}
            }
        }
        let (dyn_off, dyn_size) = dynamic?;
        let mut needed_offsets = Vec::new();
        let mut strtab_vaddr = None;
        let mut at = dyn_off;
        while at + 16 <= dyn_off + dyn_size {
            let tag = u64_at(d, at)?;
            let val = u64_at(d, at + 8)?;
            match tag {
                DT_NULL => break,
                DT_NEEDED => needed_offsets.push(val as usize),
                DT_STRTAB => strtab_vaddr = Some(val),
                _ => {}
            }
            at += 16;
        }
        let strtab_vaddr = strtab_vaddr?;
        let strtab = loads
            .iter()
            .find(|(vaddr, _, size)| strtab_vaddr >= *vaddr && strtab_vaddr < vaddr + size)
            .map(|(vaddr, offset, _)| (strtab_vaddr - vaddr + offset) as usize)?;
        needed_offsets
            .into_iter()
            .map(|o| {
                let start = strtab + o;
                let end = start + d.get(start..)?.iter().position(|&b| b == 0)?;
                Some(String::from_utf8_lossy(&d[start..end]).into_owned())
            })
            .collect()
    }
}
