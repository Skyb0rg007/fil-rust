//! Compiles C and C++ sources with [Fil-C](https://fil-c.org/) in a Cargo
//! build script, into a shared library that the program embeds with
//! `fil_rust::include_library!` and loads with `Fil::load_embedded`. The C
//! code then runs memory-safe inside the Rust process, with no library to
//! install next to the program.
//!
//! ```no_run
//! // build.rs
//! fn main() {
//!     fil_rust_build::Build::new()
//!         .file("c/zap.c")
//!         .include("c")
//!         .define("ZAP_FAST", None)
//!         .compile("zap");
//! }
//! ```
//!
//! ```ignore
//! // src/lib.rs
//! static ZAP: fil_rust::EmbeddedLibrary = fil_rust::include_library!("zap");
//! ```
//!
//! # Flavors and compilers
//!
//! A Fil-C library is built for one flavor of Fil-C (musl or glibc, see
//! fil-rust), and only loads into a runtime of that flavor. `compile` builds
//! the library once per flavor, each with that flavor's compilers:
//!
//! | flavor | C compiler | C++ compiler |
//! |---|---|---|
//! | musl | `FILC_CC`, else `$FILC_MUSL_ROOT/bin/filcc` or `$FILC_ROOT/bin/filcc` | `FILC_CXX`, else `fil++` there |
//! | glibc (x86-64) | `FILC_GLIBC_CC`, else `/opt/fil/bin/filcc` | `FILC_GLIBC_CXX`, else `/opt/fil/bin/fil++` |
//!
//! By default every flavor with a compiler is built, and finding none is an
//! error; [`Build::flavor`] requires specific ones instead. Loading the
//! library into a runtime whose flavor wasn't built is an error at run time.
//!
//! # What gets built
//!
//! Each source is compiled to an object (`.c` as C; `.cc`, `.cpp`, `.cxx`
//! and `.C` as C++) with `-fPIC`, the optimization level and debug info of
//! Cargo's profile (unless set), and then linked into `lib<name>.so` with the
//! soname `lib<name>.so`, by the C++ driver if any source is C++. Compiler
//! warnings are passed on as Cargo warnings, and the build script reruns
//! when a source, a header it includes (from the compiler's dependency
//! output) or one of the variables above changes.
//!
//! Libraries can link against each other ([`Build::link_with`]); load the
//! dependency first at run time, and the dependent library finds it by
//! soname.

// The examples are build scripts, whose `main` is the point.
#![allow(clippy::needless_doctest_main)]

use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A flavor of Fil-C: which libc its Fil-C code is built against.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Flavor {
    /// The musl-based "pizfix" distribution.
    Musl,
    /// The glibc-based "optfil" distribution (`/opt/fil`), x86-64 only.
    Glibc,
}

impl Flavor {
    /// Every flavor.
    pub const ALL: [Flavor; 2] = [Flavor::Musl, Flavor::Glibc];

    /// The flavor's name, as fil-rust's `FILC_FLAVOR` spells it.
    pub fn name(self) -> &'static str {
        match self {
            Flavor::Musl => "musl",
            Flavor::Glibc => "glibc",
        }
    }
}

impl fmt::Display for Flavor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a build failed.
#[derive(Debug)]
pub struct Error {
    message: String,
}

impl Error {
    fn new(message: impl Into<String>) -> Error {
        Error { message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

/// The environment variables compilers are found through.
const COMPILER_VARS: &[&str] =
    &["FILC_CC", "FILC_CXX", "FILC_MUSL_ROOT", "FILC_ROOT", "FILC_GLIBC_CC", "FILC_GLIBC_CXX"];

/// The default install location of the glibc distribution.
const OPTFIL: &str = "/opt/fil";

/// Builds one Fil-C shared library from C and C++ sources.
#[derive(Clone, Debug)]
pub struct Build {
    files: Vec<PathBuf>,
    includes: Vec<PathBuf>,
    defines: Vec<(String, Option<String>)>,
    flags: Vec<OsString>,
    link_args: Vec<OsString>,
    libs: Vec<String>,
    deps: Vec<Output>,
    flavors: Vec<Flavor>,
    opt_level: Option<String>,
    debug: Option<bool>,
    out_dir: Option<PathBuf>,
    cargo_metadata: bool,
}

impl Default for Build {
    fn default() -> Self {
        Build::new()
    }
}

impl Build {
    /// An empty build.
    pub fn new() -> Build {
        Build {
            files: Vec::new(),
            includes: Vec::new(),
            defines: Vec::new(),
            flags: Vec::new(),
            link_args: Vec::new(),
            libs: Vec::new(),
            deps: Vec::new(),
            flavors: Vec::new(),
            opt_level: None,
            debug: None,
            out_dir: None,
            cargo_metadata: true,
        }
    }

    /// Adds a source file.
    pub fn file(&mut self, path: impl AsRef<Path>) -> &mut Build {
        self.files.push(path.as_ref().to_path_buf());
        self
    }

    /// Adds source files.
    pub fn files<P: AsRef<Path>>(&mut self, paths: impl IntoIterator<Item = P>) -> &mut Build {
        self.files.extend(paths.into_iter().map(|p| p.as_ref().to_path_buf()));
        self
    }

    /// Adds an include directory (`-I`).
    pub fn include(&mut self, dir: impl AsRef<Path>) -> &mut Build {
        self.includes.push(dir.as_ref().to_path_buf());
        self
    }

    /// Defines a macro (`-Dname` or `-Dname=value`).
    pub fn define(&mut self, name: &str, value: Option<&str>) -> &mut Build {
        self.defines.push((name.to_string(), value.map(str::to_string)));
        self
    }

    /// Passes a flag to every compile (`-std=c++20`, `-Wall`).
    pub fn flag(&mut self, flag: impl AsRef<OsStr>) -> &mut Build {
        self.flags.push(flag.as_ref().to_os_string());
        self
    }

    /// Passes an argument to the link (`-Wl,...`, `-L...`).
    pub fn link_arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Build {
        self.link_args.push(arg.as_ref().to_os_string());
        self
    }

    /// Links against a library of the Fil-C distribution (`-l<name>`), such
    /// as `m` or `pthread`.
    pub fn lib(&mut self, name: &str) -> &mut Build {
        self.libs.push(name.to_string());
        self
    }

    /// Links against a library built earlier by this build script, for each
    /// flavor. At run time, load it before this one.
    pub fn link_with(&mut self, library: &Output) -> &mut Build {
        self.deps.push(library.clone());
        self
    }

    /// Builds for `flavor`, failing if its compiler isn't found. Without
    /// any calls, every flavor with a compiler is built.
    pub fn flavor(&mut self, flavor: Flavor) -> &mut Build {
        if !self.flavors.contains(&flavor) {
            self.flavors.push(flavor);
        }
        self
    }

    /// The optimization level (`0`-`3`, `s`, `z`); by default Cargo's
    /// (`OPT_LEVEL`).
    pub fn opt_level(&mut self, level: &str) -> &mut Build {
        self.opt_level = Some(level.to_string());
        self
    }

    /// Whether to emit debug info; by default Cargo's (`DEBUG`).
    pub fn debug(&mut self, debug: bool) -> &mut Build {
        self.debug = Some(debug);
        self
    }

    /// Where to build; by default `$OUT_DIR/fil-rust-build`.
    /// `fil_rust::include_library!` only finds libraries built in the
    /// default place.
    pub fn out_dir(&mut self, dir: impl AsRef<Path>) -> &mut Build {
        self.out_dir = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Whether to print `cargo:` directives (rerun conditions and
    /// warnings); on by default.
    pub fn cargo_metadata(&mut self, on: bool) -> &mut Build {
        self.cargo_metadata = on;
        self
    }

    /// Builds `lib<name>.so` for each flavor, panicking with the compiler's
    /// output on failure (the usual way for a build script to fail).
    pub fn compile(&self, name: &str) -> Output {
        match self.try_compile(name) {
            Ok(output) => output,
            Err(e) => panic!("fil-rust-build: {e}"),
        }
    }

    /// Builds `lib<name>.so` for each flavor.
    pub fn try_compile(&self, name: &str) -> Result<Output, Error> {
        check_name(name)?;
        if self.files.is_empty() {
            return Err(Error::new(format!("lib{name}.so has no source files")));
        }
        if self.cargo_metadata {
            for var in COMPILER_VARS {
                println!("cargo:rerun-if-env-changed={var}");
            }
        }
        let out_dir = match &self.out_dir {
            Some(dir) => dir.clone(),
            None => PathBuf::from(
                env::var_os("OUT_DIR").ok_or_else(|| Error::new("OUT_DIR isn't set (not in a build script?)"))?,
            )
            .join("fil-rust-build"),
        };
        let arch = env::var("CARGO_CFG_TARGET_ARCH").ok();
        let flavors = resolve_flavors(&self.flavors, arch.as_deref(), find_compiler)?;
        let cwd = env::current_dir().map_err(|e| Error::new(format!("current directory: {e}")))?;

        let mut libraries = Vec::new();
        let mut inputs = Vec::new();
        for &(flavor, ref compilers) in &flavors {
            let dir = out_dir.join(flavor.name());
            std::fs::create_dir_all(&dir).map_err(|e| Error::new(format!("creating {}: {e}", dir.display())))?;
            let objects = self.compile_objects(name, flavor, compilers, &dir, &cwd, &mut inputs)?;
            let lib = dir.join(format!("lib{name}.so"));
            self.link(name, flavor, compilers, &objects, &lib)?;
            libraries.push((flavor, lib));
        }

        // What include_library! reads: one expression per flavor.
        for flavor in Flavor::ALL {
            let expr = match libraries.iter().find(|(f, _)| *f == flavor) {
                Some((_, lib)) => {
                    let lib = lib.to_str().ok_or_else(|| Error::new(format!("{} isn't UTF-8", lib.display())))?;
                    format!("::core::option::Option::Some(::core::include_bytes!({lib:?}) as &[u8])\n")
                }
                None => "::core::option::Option::None\n".to_string(),
            };
            let file = out_dir.join(format!("{name}.{flavor}.rs"));
            std::fs::write(&file, expr).map_err(|e| Error::new(format!("writing {}: {e}", file.display())))?;
        }

        inputs.sort();
        inputs.dedup();
        if self.cargo_metadata {
            for input in &inputs {
                println!("cargo:rerun-if-changed={}", input.display());
            }
        }
        Ok(Output { name: name.to_string(), libraries, inputs })
    }

    /// Compiles every source for one flavor, in parallel, recording the
    /// files each depends on in `inputs`.
    fn compile_objects(
        &self,
        name: &str,
        flavor: Flavor,
        compilers: &Compilers,
        dir: &Path,
        cwd: &Path,
        inputs: &mut Vec<PathBuf>,
    ) -> Result<Vec<PathBuf>, Error> {
        let opt = self.opt_level.clone().or_else(|| env::var("OPT_LEVEL").ok()).unwrap_or_else(|| "2".into());
        let debug = self.debug.unwrap_or_else(|| {
            env::var("DEBUG").is_ok_and(|d| !matches!(d.as_str(), "" | "false" | "0" | "none"))
        });
        let jobs: Vec<(PathBuf, PathBuf, Command)> = self
            .files
            .iter()
            .enumerate()
            .map(|(i, src)| {
                let stem = src.file_stem().unwrap_or(OsStr::new("source")).to_string_lossy();
                let obj = dir.join(format!("{name}-{i}-{stem}.o"));
                let mut cmd = Command::new(if is_cxx(src) { &compilers.cxx } else { &compilers.cc });
                cmd.args(["-c", "-fPIC", &format!("-O{opt}")]);
                if debug {
                    cmd.arg("-g");
                }
                for inc in &self.includes {
                    cmd.arg("-I").arg(inc);
                }
                for (k, v) in &self.defines {
                    cmd.arg(match v {
                        Some(v) => format!("-D{k}={v}"),
                        None => format!("-D{k}"),
                    });
                }
                cmd.args(&self.flags);
                cmd.arg("-MMD").arg("-MF").arg(obj.with_extension("d"));
                cmd.arg("-o").arg(&obj).arg(src);
                (src.clone(), obj, cmd)
            })
            .collect();
        let sources: Vec<(PathBuf, PathBuf)> = jobs.iter().map(|(src, obj, _)| (src.clone(), obj.clone())).collect();

        let workers = env::var("NUM_JOBS")
            .ok()
            .and_then(|n| n.parse().ok())
            .or_else(|| std::thread::available_parallelism().ok().map(usize::from))
            .unwrap_or(1)
            .clamp(1, jobs.len());
        let next = AtomicUsize::new(0);
        let results: Mutex<Vec<(usize, Result<String, Error>)>> = Mutex::new(Vec::new());
        let jobs = Mutex::new(jobs.into_iter().map(Some).collect::<Vec<_>>());
        std::thread::scope(|s| {
            for _ in 0..workers {
                s.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(job) = jobs.lock().unwrap().get_mut(i).and_then(Option::take) else { break };
                        let (src, _, mut cmd) = job;
                        let result = run(&mut cmd, &format!("compiling {} ({flavor})", src.display()));
                        results.lock().unwrap().push((i, result));
                    }
                });
            }
        });
        let mut results = results.into_inner().unwrap();
        results.sort_by_key(|(i, _)| *i);

        for ((_, result), (src, obj)) in results.into_iter().zip(&sources) {
            let warnings = result?;
            self.warn(&warnings);
            inputs.push(cwd.join(src));
            if let Ok(deps) = std::fs::read_to_string(obj.with_extension("d")) {
                inputs.extend(parse_depfile(&deps).into_iter().map(|p| cwd.join(p)));
            }
        }
        Ok(sources.into_iter().map(|(_, obj)| obj).collect())
    }

    fn link(
        &self,
        name: &str,
        flavor: Flavor,
        compilers: &Compilers,
        objects: &[PathBuf],
        lib: &Path,
    ) -> Result<(), Error> {
        let driver = if self.files.iter().any(|f| is_cxx(f)) { &compilers.cxx } else { &compilers.cc };
        let mut cmd = Command::new(driver);
        cmd.args(["-shared", "-fPIC", &format!("-Wl,-soname,lib{name}.so"), "-o"]).arg(lib).args(objects);
        for dep in &self.deps {
            let path = dep.path(flavor).ok_or_else(|| {
                Error::new(format!("lib{name}.so links with lib{}.so, which wasn't built for {flavor}", dep.name))
            })?;
            cmd.arg("-L").arg(path.parent().unwrap()).arg(format!("-l{}", dep.name));
        }
        cmd.args(&self.link_args);
        for l in &self.libs {
            cmd.arg(format!("-l{l}"));
        }
        let warnings = run(&mut cmd, &format!("linking lib{name}.so ({flavor})"))?;
        self.warn(&warnings);
        Ok(())
    }

    fn warn(&self, output: &str) {
        if self.cargo_metadata {
            for line in output.lines().filter(|l| !l.trim().is_empty()) {
                println!("cargo:warning={line}");
            }
        }
    }
}

/// The libraries a [`Build`] made.
#[derive(Clone, Debug)]
pub struct Output {
    name: String,
    libraries: Vec<(Flavor, PathBuf)>,
    inputs: Vec<PathBuf>,
}

impl Output {
    /// The library's name (its file and soname are `lib<name>.so`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The flavors it was built for.
    pub fn flavors(&self) -> impl Iterator<Item = Flavor> + '_ {
        self.libraries.iter().map(|(f, _)| *f)
    }

    /// The files the build read: the sources and the headers they include
    /// (outside system directories), as absolute paths.
    pub fn inputs(&self) -> &[PathBuf] {
        &self.inputs
    }

    /// The library built for `flavor`.
    pub fn path(&self, flavor: Flavor) -> Option<&Path> {
        self.libraries.iter().find(|(f, _)| *f == flavor).map(|(_, p)| p.as_path())
    }
}

/// A flavor's compiler drivers.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Compilers {
    cc: PathBuf,
    cxx: PathBuf,
}

/// Finds `flavor`'s compilers from the environment.
fn find_compiler(flavor: Flavor) -> Option<Compilers> {
    let var = |name: &str| env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from);
    let (cc, cxx) = match flavor {
        Flavor::Musl => {
            let root = var("FILC_MUSL_ROOT").or_else(|| var("FILC_ROOT"));
            let bin = root.map(|r| r.join("bin"));
            (
                var("FILC_CC").or_else(|| bin.as_ref().map(|b| b.join("filcc"))),
                var("FILC_CXX").or_else(|| bin.as_ref().map(|b| b.join("fil++"))),
            )
        }
        Flavor::Glibc => {
            let bin = Path::new(OPTFIL).join("bin");
            (
                var("FILC_GLIBC_CC").or_else(|| Some(bin.join("filcc"))),
                var("FILC_GLIBC_CXX").or_else(|| Some(bin.join("fil++"))),
            )
        }
    };
    let cc = cc.filter(|p| p.is_file())?;
    // Without a C++ driver next to it, C++ sources fail when compiled.
    let cxx = cxx.unwrap_or_else(|| cc.clone());
    Some(Compilers { cc, cxx })
}

/// The flavors to build and their compilers: the requested ones (each must
/// have a compiler), or every one that has a compiler (at least one must).
/// The glibc distribution exists only for x86-64.
fn resolve_flavors(
    requested: &[Flavor],
    target_arch: Option<&str>,
    find: impl Fn(Flavor) -> Option<Compilers>,
) -> Result<Vec<(Flavor, Compilers)>, Error> {
    let available = |f: Flavor| f != Flavor::Glibc || target_arch.is_none_or(|a| a == "x86_64");
    if requested.is_empty() {
        let found: Vec<_> =
            Flavor::ALL.into_iter().filter(|&f| available(f)).filter_map(|f| find(f).map(|c| (f, c))).collect();
        if found.is_empty() {
            return Err(Error::new(
                "found no Fil-C compiler: set FILC_CC (or FILC_ROOT) for the musl flavor, \
                 or FILC_GLIBC_CC (or install /opt/fil) for the glibc flavor",
            ));
        }
        return Ok(found);
    }
    requested
        .iter()
        .map(|&f| {
            if !available(f) {
                return Err(Error::new(format!(
                    "the {f} flavor of Fil-C doesn't exist for {}",
                    target_arch.unwrap_or("this target")
                )));
            }
            let hint = match f {
                Flavor::Musl => "set FILC_CC or FILC_ROOT",
                Flavor::Glibc => "set FILC_GLIBC_CC or install /opt/fil",
            };
            find(f).map(|c| (f, c)).ok_or_else(|| Error::new(format!("found no {f} Fil-C compiler ({hint})")))
        })
        .collect()
}

fn check_name(name: &str) -> Result<(), Error> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        return Err(Error::new(format!(
            "{name:?} isn't a valid library name (use letters, digits, '_', '-' and '.')"
        )));
    }
    Ok(())
}

fn is_cxx(path: &Path) -> bool {
    matches!(path.extension().and_then(OsStr::to_str), Some("cc" | "cpp" | "cxx" | "c++" | "C"))
}

/// Runs a compiler, returning its diagnostics on success and an error with
/// them on failure.
fn run(cmd: &mut Command, what: &str) -> Result<String, Error> {
    let out = cmd.output().map_err(|e| Error::new(format!("{what}: running {:?}: {e}", cmd.get_program())))?;
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stderr));
    if out.status.success() {
        Ok(text)
    } else {
        Err(Error::new(format!("{what} failed ({}):\n{cmd:?}\n{text}", out.status)))
    }
}

/// The prerequisites in a Make-style dependency file (`-MD` output) for one
/// target.
fn parse_depfile(text: &str) -> Vec<PathBuf> {
    let joined = text.replace("\\\r\n", " ").replace("\\\n", " ");
    let Some(line) = joined.lines().next() else { return Vec::new() };
    // The target ends at the first colon followed by whitespace.
    let Some(colon) = line.find(": ").or_else(|| line.strip_suffix(':').map(str::len)) else {
        return Vec::new();
    };
    let mut deps = Vec::new();
    let mut current = String::new();
    let mut chars = line[colon + 1..].chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek().is_some_and(|n| *n == ' ' || *n == '#') => current.push(chars.next().unwrap()),
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                current.push('$');
            }
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    deps.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        deps.push(PathBuf::from(current));
    }
    deps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depfiles_list_prerequisites_with_escapes_and_continuations() {
        let text = "out/a.o: src/a.c include/a.h \\\n  include/my\\ dir/b.h cost$$.h\n";
        assert_eq!(
            parse_depfile(text),
            ["src/a.c", "include/a.h", "include/my dir/b.h", "cost$.h"].map(PathBuf::from)
        );
        assert_eq!(parse_depfile("a.o:\n"), Vec::<PathBuf>::new());
        assert_eq!(parse_depfile(""), Vec::<PathBuf>::new());
    }

    fn fake(f: Flavor) -> Compilers {
        Compilers { cc: PathBuf::from(format!("/{f}/cc")), cxx: PathBuf::from(format!("/{f}/c++")) }
    }

    #[test]
    fn every_flavor_with_a_compiler_is_built_by_default() {
        let both = resolve_flavors(&[], Some("x86_64"), |f| Some(fake(f))).unwrap();
        assert_eq!(both.iter().map(|(f, _)| *f).collect::<Vec<_>>(), [Flavor::Musl, Flavor::Glibc]);
        let musl = resolve_flavors(&[], Some("x86_64"), |f| (f == Flavor::Musl).then(|| fake(f))).unwrap();
        assert_eq!(musl, [(Flavor::Musl, fake(Flavor::Musl))]);
        // glibc doesn't exist for aarch64, even with a compiler.
        let arm = resolve_flavors(&[], Some("aarch64"), |f| Some(fake(f))).unwrap();
        assert_eq!(arm, [(Flavor::Musl, fake(Flavor::Musl))]);
        let none = resolve_flavors(&[], Some("x86_64"), |_| None).unwrap_err();
        assert!(none.to_string().contains("found no Fil-C compiler"), "{none}");
    }

    #[test]
    fn requested_flavors_must_have_a_compiler() {
        let err = resolve_flavors(&[Flavor::Glibc], Some("x86_64"), |f| (f == Flavor::Musl).then(|| fake(f)))
            .unwrap_err();
        assert!(err.to_string().contains("FILC_GLIBC_CC"), "{err}");
        let err = resolve_flavors(&[Flavor::Glibc], Some("aarch64"), |f| Some(fake(f))).unwrap_err();
        assert!(err.to_string().contains("doesn't exist for aarch64"), "{err}");
        let only = resolve_flavors(&[Flavor::Glibc], Some("x86_64"), |f| Some(fake(f))).unwrap();
        assert_eq!(only, [(Flavor::Glibc, fake(Flavor::Glibc))]);
    }

    #[test]
    fn library_names_are_file_name_safe() {
        assert!(check_name("zap_2.0-x").is_ok());
        for bad in ["", "a/b", "a b", "../x", "x\0"] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }
}
