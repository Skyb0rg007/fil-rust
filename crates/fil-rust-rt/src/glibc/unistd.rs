//! System calls under glibc's names. Most are the musl flavor's `zsys_*`
//! natives, which already return -1 and set `errno` as glibc's wrappers do,
//! and check the structures they pass on against the host's layouts, which on
//! x86-64 are the glibc flavor's too. The rest are here.

use std::collections::HashMap;
use std::ffi::{c_int, c_long, c_uint};
use std::sync::OnceLock;

use crate::abi::*;
use crate::natives::natives;
use crate::start::set_errno;
use crate::sys::{one_out, output, plain, sys, text};
use crate::{calls, maps, net, process, view};

/// `zsys_*` natives that glibc's function of the same name doesn't match:
/// its `timer_t` is a pointer, `sched_yield` returns a value, `getcwd` may
/// allocate, and the signal, exit and loader ones belong to the parts of the
/// libc that aren't here yet.
const DIFFERENT: &[&str] = &[
    "sched_yield",
    "getcwd",
    "getdents",
    "timer_create",
    "timer_delete",
    "timer_getoverrun",
    "timer_gettime",
    "timer_settime",
    "exit_soft",
    "exit_hard",
    "abort",
    "dlopen",
    "dlsym",
    "dladdr",
    "sigaction",
    "sigaltstack",
    "sigsuspend",
    "sigwait",
    "sigtimedwait",
    "sigqueue",
    "signalfd",
    "sigprocmask",
    "sigpending",
    "raise",
    "raise_without_exit",
    "raw_sched_getaffinity",
];

/// glibc's other names for some of these: large-file twins, and the like.
const RENAMES: &[(&str, &str)] = &[
    ("open64", "open"),
    ("openat64", "openat"),
    ("creat64", "creat"),
    ("pread64", "pread"),
    ("pwrite64", "pwrite"),
    ("preadv64", "preadv"),
    ("pwritev64", "pwritev"),
    ("lseek64", "lseek"),
    ("ftruncate64", "ftruncate"),
    ("truncate64", "truncate"),
    ("fallocate64", "fallocate"),
    ("posix_fallocate64", "posix_fallocate"),
    ("posix_fadvise64", "posix_fadvise"),
    ("mmap64", "mmap"),
    ("statfs64", "statfs"),
    ("fstatfs64", "fstatfs"),
    ("statvfs64", "statvfs"),
    ("fstatvfs64", "fstatvfs"),
    ("getrlimit64", "getrlimit"),
    ("setrlimit64", "setrlimit"),
    ("fstat64", "fstat"),
    ("fstatat64", "fstatat"),
    ("newfstatat", "fstatat"),
    ("stat64", "stat"),
    ("lstat64", "lstat"),
    ("getdents64", "zsys_getdents"),
    ("__getpagesize", "getpagesize"),
    ("_Exit", "_exit"),
    ("__sched_yield", "sched_yield"),
    ("__getpid", "getpid"),
    ("__open", "open"),
    ("__open64", "open"),
    ("__close", "close"),
    ("__read", "read"),
    ("__write", "write"),
];

fn at_cwd_stat(thread: *mut Thread, path: Ptr, buffer: Ptr, flags: c_int) -> c_int {
    let path = unsafe { text(path) };
    let buffer = one_out::<libc::stat>(buffer);
    unsafe { sys(thread, || libc::fstatat(libc::AT_FDCWD, path.as_ptr(), buffer, flags)) }
}

natives! {
    fn stat(thread: *mut Thread, path: Ptr, buffer: Ptr) -> c_int {
        at_cwd_stat(thread, path, buffer, 0)
    }

    fn lstat(thread: *mut Thread, path: Ptr, buffer: Ptr) -> c_int {
        at_cwd_stat(thread, path, buffer, libc::AT_SYMLINK_NOFOLLOW)
    }

    fn creat(thread: *mut Thread, path: Ptr, mode: c_uint) -> c_int {
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::open(path.as_ptr(), libc::O_CREAT | libc::O_WRONLY | libc::O_TRUNC, mode)) }
    }

    fn getcwd(thread: *mut Thread, buffer: Ptr, size: usize) -> Ptr {
        if !buffer.ptr.is_null() {
            if size == 0 {
                unsafe { set_errno(thread, libc::EINVAL) };
                return Ptr::NULL;
            }
            let raw = output(buffer, size);
            return if unsafe { sys(thread, || libc::getcwd(raw, size)) }.is_null() { Ptr::NULL } else { buffer };
        }
        // glibc's extension: a new string, at least `size` long if that's
        // given and fits.
        let directory = match unsafe { plain(thread, std::env::current_dir) } {
            Ok(directory) => directory,
            Err(error) => {
                unsafe { set_errno(thread, error.raw_os_error().unwrap_or(libc::ENOENT)) };
                return Ptr::NULL;
            }
        };
        let bytes = std::os::unix::ffi::OsStrExt::as_bytes(directory.as_os_str()).to_vec();
        if size != 0 && bytes.len() >= size {
            unsafe { set_errno(thread, libc::ERANGE) };
            return Ptr::NULL;
        }
        unsafe { view::new_c_string(thread, &bytes) }
    }

    fn sched_yield(thread: *mut Thread) -> c_int {
        unsafe { plain(thread, || libc::sched_yield()) }
    }

    fn wait(thread: *mut Thread, status: Ptr) -> c_int {
        let status = crate::sys::maybe_out::<c_int>(status);
        unsafe { sys(thread, || libc::waitpid(-1, status, 0)) }
    }

    fn sleep(thread: *mut Thread, seconds: c_uint) -> c_uint {
        unsafe { plain(thread, || libc::sleep(seconds)) }
    }

    fn usleep(thread: *mut Thread, microseconds: c_uint) -> c_int {
        unsafe { sys(thread, || libc::usleep(microseconds)) }
    }

    fn sysconf(thread: *mut Thread, name: c_int) -> c_long {
        unsafe { sys(thread, || libc::sysconf(name)) }
    }

    fn getpagesize(thread: *mut Thread) -> c_int {
        unsafe { plain(thread, || libc::sysconf(libc::_SC_PAGESIZE)) as c_int }
    }

    fn get_nprocs(thread: *mut Thread) -> c_int {
        unsafe { plain(thread, || libc::sysconf(libc::_SC_NPROCESSORS_ONLN)) as c_int }
    }

    fn get_nprocs_conf(thread: *mut Thread) -> c_int {
        unsafe { plain(thread, || libc::sysconf(libc::_SC_NPROCESSORS_CONF)) as c_int }
    }

    fn gettid(thread: *mut Thread) -> c_int {
        unsafe { plain(thread, || libc::gettid()) }
    }

    fn _exit(thread: *mut Thread, code: c_int) -> () {
        unsafe {
            crate::thread::exit(thread);
            libc::_exit(code)
        }
    }

    fn send(thread: *mut Thread, fd: c_int, buffer: Ptr, length: usize, flags: c_int) -> i64 {
        let buffer = crate::sys::input::<libc::c_void>(buffer, length);
        unsafe { sys(thread, || libc::send(fd, buffer, length, flags)) as i64 }
    }

    fn recv(thread: *mut Thread, fd: c_int, buffer: Ptr, length: usize, flags: c_int) -> i64 {
        let buffer = output::<libc::c_void>(buffer, length);
        unsafe { sys(thread, || libc::recv(fd, buffer, length, flags)) as i64 }
    }
}

/// Every system call here, keyed by glibc's symbol.
pub fn aliases() -> Vec<(&'static str, *const ())> {
    static ALIASES: OnceLock<Vec<(&'static str, usize)>> = OnceLock::new();
    ALIASES
        .get_or_init(|| {
            let musl = crate::sys::table().into_iter().chain(calls::table()).chain(net::table()).chain(maps::table()).chain(process::table());
            let mut by_name: HashMap<String, *const ()> = HashMap::new();
            for (name, getter) in musl {
                let Some(call) = name.strip_prefix("pizlonated_zsys_") else { continue };
                by_name.insert(format!("zsys_{call}"), getter);
                let call = call.strip_suffix("_impl").unwrap_or(call);
                if !DIFFERENT.contains(&call) {
                    by_name.insert(call.to_string(), getter);
                }
            }
            for (name, getter) in table() {
                by_name.insert(name.strip_prefix("pizlonated_").unwrap().to_string(), getter);
            }
            for (name, target) in RENAMES {
                let getter = by_name[*target];
                by_name.insert(name.to_string(), getter);
            }
            by_name
                .into_iter()
                .filter(|(name, _)| !name.starts_with("zsys_"))
                .map(|(name, getter)| (&*format!("pizlonated_{name}").leak(), getter as usize))
                .collect()
        })
        .iter()
        .map(|&(name, getter)| (name, getter as *const ()))
        .collect()
}
