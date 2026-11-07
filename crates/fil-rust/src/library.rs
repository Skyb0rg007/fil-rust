//! Fil-C shared libraries.

use std::ffi::{CString, c_void};
use std::path::{Path, PathBuf};

use fil_rust_sys::{Flavor, RawPtr, dlerror_string, dlopen_path, loaded_objects};

use crate::call::{FilFn, FilSig, Function};
use crate::ptr::{FilPtr, Opaque};
use crate::runtime::{Backend, Config, Inner};
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
    pub(crate) fn handle(&self) -> *mut c_void {
        self.handle
    }

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
        // dlsym takes the loader's lock, which a thread in dlopen holds while
        // library constructors run, and those enter the runtime (creating
        // threads, for one, waits for every entered thread). So never wait
        // for it while entered.
        let handle = self.handle as usize;
        let libc = crate::runtime::loaded_libc_handle() as usize;
        let getter = crate::runtime::outside_runtime(|| unsafe {
            libc::dlerror();
            let mut getter = libc::dlsym(handle as *mut c_void, symbol.as_ptr());
            // A library loaded before the libc depends on its stand-in.
            if getter.is_null() && libc != 0 {
                getter = libc::dlsym(libc as *mut c_void, symbol.as_ptr());
            }
            if getter.is_null() {
                let _ = dlerror_string();
            }
            getter as usize
        });
        if getter == 0 { Err(Error::MissingSymbol(name.to_string())) } else { Ok(getter as *mut c_void) }
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

/// Fails if the process has mapped what fil-rust means to stand in for. The
/// real yolo libc would put two C libraries in one address space. On the Rust
/// backend, the same goes for libpizlo, which would bring a second runtime, and
/// for a Fil-C libc that wasn't loaded through here, which would have no thread
/// to run on.
pub(crate) fn check_no_foreign_runtime(config: &Config, has_libc: bool) -> Result<()> {
    let flavor = config.flavor();
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
        let foreign_libc = !has_libc && flavor.libc_stand_ins().iter().any(|lib| lib.soname == base);
        if config.backend() == Backend::Rust && (base == "libpizlo.so" || foreign_libc) {
            return Err(Error::Load(format!(
                "{name} was loaded into the process, but the Rust runtime stands in for it"
            )));
        }
    }
    Ok(())
}

/// The first thing `data`, a library, needs from the Fil-C libc, if anything:
/// a function nothing else in the process can give it, by name, as that's what
/// tells. Every Fil-C library is linked against the libc whether it uses it or
/// not.
pub(crate) fn libc_import(inner: &Inner, data: &[u8]) -> Option<String> {
    let mut seen = Vec::new();
    let mut wanted = Vec::new();
    let mut defined = std::collections::HashSet::new();
    let mut loaded = Vec::new();
    let mut pending = vec![data.to_vec()];
    while let Some(object) = pending.pop() {
        for symbol in elf::symbols(&object) {
            if symbol.defined {
                defined.insert(symbol.name);
            } else if !symbol.weak && symbol.name.starts_with("pizlonated_") {
                wanted.push(symbol.name);
            }
        }
        for name in elf::needed(&object) {
            if seen.contains(&name) || RUNTIME_LIBRARIES.contains(&name.as_str()) {
                continue;
            }
            seen.push(name.clone());
            if let Some(handle) = open_if_loaded(&name) {
                loaded.push(handle as usize);
            } else if let Some(contents) = inner
                .config
                .search_dirs()
                .iter()
                .map(|dir| dir.join(&name))
                .find(|path| path.is_file())
                .and_then(|path| std::fs::read(path).ok())
            {
                pending.push(contents);
            }
        }
    }
    let runtime = inner.runtime_lib.handle() as usize;
    wanted.into_iter().find(|symbol| {
        !defined.contains(symbol) && {
            let name = CString::new(symbol.as_str()).unwrap_or_default();
            !std::iter::once(runtime)
                .chain(loaded.iter().copied())
                .any(|handle| !unsafe { libc::dlsym(handle as *mut c_void, name.as_ptr()) }.is_null())
        }
    })
}

/// What every Fil-C library is linked against, and says nothing of what it uses.
const RUNTIME_LIBRARIES: [&str; 3] = ["libc.so", "libpizlo.so", "libyoloc.so"];

fn open_if_loaded(name: &str) -> Option<*mut c_void> {
    let c = CString::new(name).ok()?;
    unsafe {
        let handle = libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_NOLOAD);
        if handle.is_null() {
            libc::dlerror();
            None
        } else {
            Some(handle)
        }
    }
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
    check_no_foreign_runtime(&inner.config, inner.has_libc())?;
    Ok(Library::from_handle(handle, path.to_path_buf()))
}

/// A Fil-C library compiled into the program, usually by
/// [`fil-rust-build`](https://docs.rs/fil-rust-build) in a build script and
/// included with [`include_library!`](crate::include_library), with one
/// build per flavor.
///
/// [`Fil::load_embedded`] loads the build for the runtime's flavor from an
/// anonymous memory file, with the same checks as [`Fil::load`]. Its soname
/// is `lib<name>.so`, so embedded libraries linked against each other find
/// their dependencies once those are loaded (load dependencies first).
#[derive(Clone, Copy)]
pub struct EmbeddedLibrary {
    name: &'static str,
    musl: Option<&'static [u8]>,
    glibc: Option<&'static [u8]>,
}

impl EmbeddedLibrary {
    /// A library named `name` (its soname is `lib<name>.so`), with its
    /// builds for each flavor. A prebuilt library can be embedded with
    /// `include_bytes!`.
    pub const fn new(name: &'static str, musl: Option<&'static [u8]>, glibc: Option<&'static [u8]>) -> Self {
        EmbeddedLibrary { name, musl, glibc }
    }

    /// The library's name, without the `lib` prefix and `.so` suffix.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The soname the library was built with, `lib<name>.so`.
    pub fn soname(&self) -> String {
        format!("lib{}.so", self.name)
    }

    /// The build for `flavor`, if there is one.
    pub fn bytes(&self, flavor: Flavor) -> Option<&'static [u8]> {
        match flavor {
            Flavor::Musl => self.musl,
            Flavor::Glibc => self.glibc,
        }
    }
}

impl std::fmt::Debug for EmbeddedLibrary {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EmbeddedLibrary")
            .field("name", &self.name)
            .field("musl", &self.musl.map(<[u8]>::len))
            .field("glibc", &self.glibc.map(<[u8]>::len))
            .finish()
    }
}

/// Embedded libraries already written to memory files, by soname, with the
/// bytes they were written from.
static EMBEDDED: std::sync::Mutex<Vec<(String, &'static [u8], PathBuf)>> = std::sync::Mutex::new(Vec::new());

/// Opens an embedded library. Like [`open`], must run on an exited Fil-C
/// thread.
pub(crate) fn open_embedded(inner: &Inner, library: &EmbeddedLibrary) -> Result<Library> {
    let flavor = inner.config.flavor();
    let soname = library.soname();
    if library.name.is_empty() || library.name.contains(['/', '\0']) {
        return Err(Error::Load(format!("{:?} is not a valid library name", library.name)));
    }
    let Some(bytes) = library.bytes(flavor) else {
        return Err(Error::Load(format!(
            "{soname} has no build for the {flavor:?} flavor (fil-rust-build builds a flavor \
             only when it finds that flavor's Fil-C compiler; see its documentation)"
        )));
    };
    let path = {
        let mut embedded = EMBEDDED.lock().unwrap_or_else(|e| e.into_inner());
        match embedded.iter().find(|(name, ..)| *name == soname) {
            // The same library again: the loader returns the loaded one.
            Some((_, loaded, path)) if *loaded == bytes => path.clone(),
            Some(_) => {
                return Err(Error::Load(format!("a different library named {soname} is already loaded")));
            }
            None => {
                // The loader matches dependencies by soname, but opening by
                // path would load a second copy of a library with this
                // soname loaded from elsewhere.
                if is_loaded(&soname) {
                    return Err(Error::Load(format!(
                        "a library with the soname {soname} is already loaded from a file"
                    )));
                }
                let path = fil_rust_sys::memfd_path(&soname, bytes).map_err(Error::Load)?;
                embedded.push((soname, bytes, path.clone()));
                path
            }
        }
    };
    open(inner, &path)
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

    const SHT_DYNSYM: u32 = 11;
    const SHN_UNDEF: u16 = 0;
    const STB_WEAK: u8 = 2;
    const SYMBOL_SIZE: usize = 24;

    /// A name in the dynamic symbol table.
    pub struct Symbol {
        pub name: String,
        pub defined: bool,
        pub weak: bool,
    }

    /// The dynamic symbols of a 64-bit little-endian ELF file (empty if it isn't
    /// one), by way of its section headers, which every Fil-C library has.
    pub fn symbols(d: &[u8]) -> Vec<Symbol> {
        parse_symbols(d).unwrap_or_default()
    }

    fn parse_symbols(d: &[u8]) -> Option<Vec<Symbol>> {
        if d.get(0..4)? != b"\x7fELF" || *d.get(4)? != 2 || *d.get(5)? != 1 {
            return None;
        }
        let shoff = usize::try_from(u64_at(d, 0x28)?).ok()?;
        let shentsize = u16_at(d, 0x3a)? as usize;
        let shnum = u16_at(d, 0x3c)? as usize;
        let section = |index: usize| shoff.checked_add(index.checked_mul(shentsize)?);
        for index in 0..shnum {
            let header = section(index)?;
            if u32_at(d, header.checked_add(4)?)? != SHT_DYNSYM {
                continue;
            }
            let offset = usize::try_from(u64_at(d, header.checked_add(24)?)?).ok()?;
            let size = usize::try_from(u64_at(d, header.checked_add(32)?)?).ok()?;
            let strings_header = section(u32_at(d, header.checked_add(40)?)? as usize)?;
            let strings = usize::try_from(u64_at(d, strings_header.checked_add(24)?)?).ok()?;
            return (0..size / SYMBOL_SIZE)
                .map(|entry| {
                    let symbol = offset.checked_add(entry.checked_mul(SYMBOL_SIZE)?)?;
                    let start = strings.checked_add(u32_at(d, symbol)? as usize)?;
                    let end = start.checked_add(d.get(start..)?.iter().position(|&b| b == 0)?)?;
                    Some(Symbol {
                        name: String::from_utf8_lossy(&d[start..end]).into_owned(),
                        defined: u16_at(d, symbol.checked_add(6)?)? != SHN_UNDEF,
                        weak: *d.get(symbol.checked_add(4)?)? >> 4 == STB_WEAK,
                    })
                })
                .collect();
        }
        None
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
