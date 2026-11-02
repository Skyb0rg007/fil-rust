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
        Some(u16::from_le_bytes(d.get(o..o.checked_add(2)?)?.try_into().ok()?))
    }
    fn u32_at(d: &[u8], o: usize) -> Option<u32> {
        Some(u32::from_le_bytes(d.get(o..o.checked_add(4)?)?.try_into().ok()?))
    }
    fn u64_at(d: &[u8], o: usize) -> Option<u64> {
        Some(u64::from_le_bytes(d.get(o..o.checked_add(8)?)?.try_into().ok()?))
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

    /// Every offset, size and address comes from the file, so all arithmetic
    /// on them is checked: a malformed file gives `None`, never a panic.
    fn parse(d: &[u8]) -> Option<Vec<String>> {
        if d.get(0..4)? != b"\x7fELF" || *d.get(4)? != 2 || *d.get(5)? != 1 {
            return None;
        }
        let at = |base: usize, offset: usize| base.checked_add(offset);
        let phoff = usize::try_from(u64_at(d, 0x20)?).ok()?;
        let phentsize = u16_at(d, 0x36)? as usize;
        let phnum = u16_at(d, 0x38)? as usize;
        let mut loads = Vec::new();
        let mut dynamic = None;
        for i in 0..phnum {
            let ph = at(phoff, i.checked_mul(phentsize)?)?;
            let kind = u32_at(d, ph)?;
            let offset = u64_at(d, at(ph, 8)?)?;
            let vaddr = u64_at(d, at(ph, 16)?)?;
            let filesz = u64_at(d, at(ph, 32)?)?;
            match kind {
                PT_LOAD => loads.push((vaddr, offset, filesz)),
                PT_DYNAMIC => {
                    dynamic = Some((usize::try_from(offset).ok()?, usize::try_from(filesz).ok()?))
                }
                _ => {}
            }
        }
        let (dyn_off, dyn_size) = dynamic?;
        let dyn_end = at(dyn_off, dyn_size)?;
        let mut needed_offsets = Vec::new();
        let mut strtab_vaddr = None;
        let mut entry = dyn_off;
        while at(entry, 16)? <= dyn_end {
            let tag = u64_at(d, entry)?;
            let val = u64_at(d, entry + 8)?;
            match tag {
                DT_NULL => break,
                DT_NEEDED => needed_offsets.push(usize::try_from(val).ok()?),
                DT_STRTAB => strtab_vaddr = Some(val),
                _ => {}
            }
            entry += 16;
        }
        let strtab_vaddr = strtab_vaddr?;
        let strtab = loads
            .iter()
            .find(|(vaddr, _, size)| {
                strtab_vaddr >= *vaddr && vaddr.checked_add(*size).is_some_and(|end| strtab_vaddr < end)
            })
            .and_then(|(vaddr, offset, _)| (strtab_vaddr - vaddr).checked_add(*offset))
            .and_then(|o| usize::try_from(o).ok())?;
        needed_offsets
            .into_iter()
            .map(|o| {
                let start = at(strtab, o)?;
                let end = start + d.get(start..)?.iter().position(|&b| b == 0)?;
                Some(String::from_utf8_lossy(&d[start..end]).into_owned())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::elf::needed;
    use proptest::prelude::*;

    /// Real ELF files to mutate: this test binary, and the C library it runs on.
    fn seeds() -> &'static [Vec<u8>] {
        static SEEDS: std::sync::OnceLock<Vec<Vec<u8>>> = std::sync::OnceLock::new();
        SEEDS.get_or_init(|| {
            let mut seeds = vec![std::fs::read("/proc/self/exe").unwrap()];
            if let Some(root) = std::env::var_os("FILC_ROOT")
                && let Ok(d) = std::fs::read(std::path::Path::new(&root).join("pizfix/lib/libc.so"))
            {
                seeds.push(d);
            }
            seeds
        })
    }

    fn put_u64(d: &mut [u8], at: usize, v: u64) {
        if let Some(slot) = d.get_mut(at..at + 8) {
            slot.copy_from_slice(&v.to_le_bytes());
        }
    }

    #[test]
    fn real_files_list_their_dependencies() {
        let exe = std::fs::read("/proc/self/exe").unwrap();
        let deps = needed(&exe);
        assert!(deps.iter().any(|d| d.starts_with("libc.so")), "{deps:?}");
    }

    #[test]
    fn not_elf() {
        for d in [&b""[..], b"\x7fEL", b"\x7fELF", b"#!/bin/sh\n", &[0x7f, b'E', b'L', b'F', 1, 1][..]] {
            assert!(needed(d).is_empty());
        }
    }

    #[test]
    fn offsets_near_the_top_of_the_address_space() {
        let exe = std::fs::read("/proc/self/exe").unwrap();
        // e_phoff, then each program header's p_offset/p_vaddr/p_filesz.
        let phoff = u64::from_le_bytes(exe[0x20..0x28].try_into().unwrap()) as usize;
        let phnum = u16::from_le_bytes(exe[0x38..0x3a].try_into().unwrap()) as usize;
        let mut fields = vec![0x20];
        for i in 0..phnum {
            fields.extend([phoff + i * 56 + 8, phoff + i * 56 + 16, phoff + i * 56 + 32]);
        }
        for field in fields {
            for v in [u64::MAX, u64::MAX - 7, u64::MAX - 15, 1 << 63, usize::MAX as u64 / 2] {
                let mut d = exe.clone();
                put_u64(&mut d, field, v);
                let _ = needed(&d);
            }
        }
        // A huge program header size.
        let mut d = exe.clone();
        d[0x36..0x38].copy_from_slice(&u16::MAX.to_le_bytes());
        let _ = needed(&d);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn truncated_files_never_panic(seed in 0usize..2, cut in any::<prop::sample::Index>()) {
            let seeds = seeds();
            let d = &seeds[seed % seeds.len()];
            let _ = needed(&d[..cut.index(d.len())]);
        }

        #[test]
        fn corrupted_files_never_panic(
            seed in 0usize..2,
            edits in prop::collection::vec((any::<prop::sample::Index>(), any::<u64>(), 0u8..3), 1..16),
        ) {
            let seeds = seeds();
            let mut d = seeds[seed % seeds.len()].clone();
            // Concentrate edits on the headers, where the offsets are.
            let hot = d.len().min(4096);
            for (at, v, kind) in edits {
                let len = if kind == 0 { hot } else { d.len() };
                let at = at.index(len);
                match kind {
                    0 | 1 => put_u64(&mut d, at, v),
                    _ => d[at] = v as u8,
                }
            }
            let _ = needed(&d);
        }

        #[test]
        fn random_bytes_after_an_elf_header_never_panic(tail in prop::collection::vec(any::<u8>(), 0..512)) {
            let mut d = vec![0x7f, b'E', b'L', b'F', 2, 1];
            d.extend(tail);
            let _ = needed(&d);
        }
    }
}
