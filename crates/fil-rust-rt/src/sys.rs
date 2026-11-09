//! `zsys_*`: the system calls behind the Fil-C libc.
//!
//! Each one checks what it was given, leaves the runtime, makes the call, and
//! reports failure through the libc's errno. Upstream makes these calls through
//! its yolo libc, and this makes them through the host's. Their structures match
//! the kernel's on the flavors supported here.
//!
//! What's here can be said in a line. System calls that need more thought are
//! further down, and those that aren't here yet fail by name.

use std::ffi::{CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr::{null, null_mut};
use std::sync::atomic::Ordering;

use crate::abi::*;
use crate::failure::safety_panic;
use crate::natives::{Rest, natives, string_of};
use crate::object::{self, Access};
use crate::start::{set_dlerror, set_errno};
use crate::thread;

const F_SETSIG: c_int = 10;
const F_GETSIG: c_int = 11;

const FUTEX_WAIT: c_int = 0;
const FUTEX_WAKE: c_int = 1;
const FUTEX_REQUEUE: c_int = 3;
const FUTEX_LOCK_PI: c_int = 6;
const FUTEX_UNLOCK_PI: c_int = 7;
const FUTEX_PRIVATE: c_int = 128;

/// musl keeps these signals for itself, and so does glibc for the first two.
const SIGCANCEL: c_int = 33;
const SIGSYNCCALL: c_int = 34;
const SIGTIMER: c_int = 32;

/// Whether `signal` belongs to the host libc, and Fil-C code can't send it.
pub fn is_internal_signal(signal: c_int) -> bool {
    matches!(signal, SIGTIMER | SIGCANCEL | SIGSYNCCALL)
}

/// Makes a call that may block, with the thread exited. Anything it leaves in
/// `errno` goes to the libc.
pub(crate) unsafe fn sys<R>(thread: *mut Thread, call: impl FnOnce() -> R) -> R {
    unsafe {
        thread::exit(thread);
        *libc::__errno_location() = 0;
        let result = call();
        let error = *libc::__errno_location();
        thread::enter(thread);
        if error != 0 {
            set_errno(thread, error);
        }
        result
    }
}

/// What a call made without exiting left in `errno`, for the libc.
pub(crate) unsafe fn sys_result(thread: *mut Thread, result: c_int) -> c_int {
    if result < 0 {
        unsafe { set_errno(thread, *libc::__errno_location()) };
    }
    result
}

/// [`sys`] for calls that can't fail.
pub(crate) unsafe fn plain<R>(thread: *mut Thread, call: impl FnOnce() -> R) -> R {
    unsafe {
        thread::exit(thread);
        let result = call();
        thread::enter(thread);
        result
    }
}

/// A buffer to read, as the kernel would.
pub(crate) fn input<T>(ptr: Ptr, size: usize) -> *const T {
    object::check_access(ptr, size, Access::Read);
    ptr.ptr.cast()
}

/// A buffer the kernel will write, whatever it was holding.
pub(crate) fn output<T>(ptr: Ptr, size: usize) -> *mut T {
    object::check_access(ptr, size, Access::Write);
    ptr.ptr.cast()
}

pub(crate) fn one_in<T>(ptr: Ptr) -> *const T {
    input(ptr, size_of::<T>())
}

pub(crate) fn one_out<T>(ptr: Ptr) -> *mut T {
    output(ptr, size_of::<T>())
}

pub(crate) fn maybe_in<T>(ptr: Ptr) -> *const T {
    if ptr.ptr.is_null() { null() } else { one_in(ptr) }
}

pub(crate) fn maybe_out<T>(ptr: Ptr) -> *mut T {
    if ptr.ptr.is_null() { null_mut() } else { one_out(ptr) }
}

/// A copy of a string, which is safe from other threads while the call runs.
pub(crate) unsafe fn text(ptr: Ptr) -> CString {
    unsafe { CString::from_vec_unchecked(string_of(ptr)) }
}

pub(crate) fn array_size(size: usize, count: usize) -> usize {
    size.checked_mul(count)
        .unwrap_or_else(|| safety_panic(format_args!("size argument too big, causes overflow; size = {count}.")))
}

/// Reads the entries of a Fil-C `iovec` array and checks what they point at.
pub(crate) unsafe fn iovecs(thread: *mut Thread, user: Ptr, count: c_int, access: Access) -> Vec<libc::iovec> {
    unsafe { iovecs_of(thread, user, count.max(0) as usize, Some(access)) }
}

/// [`iovecs`], leaving what they point at alone if `access` is `None`.
pub(crate) unsafe fn iovecs_of(thread: *mut Thread, user: Ptr, count: usize, access: Option<Access>) -> Vec<libc::iovec> {
    let entry_size = size_of::<libc::iovec>();
    object::check_access(user, array_size(entry_size, count), Access::Read);
    (0..count)
        .map(|index| unsafe {
            let entry = user.ptr.add(index * entry_size);
            let base = crate::ptrs::load(user.with_ptr(entry));
            thread::track(thread, base.object());
            let length = (*entry.cast::<libc::iovec>()).iov_len;
            if let Some(access) = access {
                object::check_access(base, length, access);
            }
            libc::iovec { iov_base: base.ptr.cast(), iov_len: length }
        })
        .collect()
}

/// Bytes an `fd_set` needs for `count` descriptors.
fn fd_set_size(count: c_int) -> usize {
    (count as usize).div_ceil(8)
}

/// Fil-C's `struct epoll_event`. The kernel's is packed on x86-64, so its data
/// sits at 4; this one's is at 8. A pointer in the data has already gone
/// through the support library's exact pointer table, so it's an integer here.
#[repr(C)]
#[derive(Clone, Copy)]
struct UserEpollEvent {
    events: u32,
    data: u64,
}

/// Waits for up to `count` events into a buffer of the kernel's, then copies
/// the ones it gets into the program's. The whole of the program's buffer is
/// checked first, as the kernel could fill all of it.
unsafe fn epoll_waited(thread: *mut Thread, events: Ptr, count: c_int, wait: impl FnOnce(*mut libc::epoll_event) -> c_int) -> c_int {
    if count <= 0 {
        // The kernel turns these down without touching the buffer.
        return unsafe { sys(thread, || wait(null_mut())) };
    }
    let user = output::<UserEpollEvent>(events, array_size(size_of::<UserEpollEvent>(), count as usize));
    let mut host = vec![libc::epoll_event { events: 0, u64: 0 }; count as usize];
    let result = unsafe { sys(thread, || wait(host.as_mut_ptr())) };
    for (index, event) in host.iter().take(result.max(0) as usize).enumerate() {
        unsafe { user.add(index).write_unaligned(UserEpollEvent { events: event.events, data: event.u64 }) };
    }
    result
}

/// Lets go of the signals that aren't the program's to touch.
pub(crate) unsafe fn user_sigset(user: Ptr) -> libc::sigset_t {
    unsafe {
        let mut set = one_in::<libc::sigset_t>(user).read();
        for signal in [SIGTIMER, SIGCANCEL, SIGSYNCCALL] {
            libc::sigdelset(&mut set, signal);
        }
        set
    }
}

/// The argument of an `ioctl`-like call, about which nothing is known but what
/// the kernel does with it. Integers can't be told from pointers, so only those
/// that can't be addresses go through. Otherwise it gets a copy of the bytes the
/// pointer can reach, ending at a guard page, and if the kernel faults, more of
/// them. It can't see past what the capability allows, and what it writes
/// goes back.
unsafe fn with_guarded_argument(thread: *mut Thread, argument: Ptr, call: impl Fn(*mut c_void) -> c_int) -> c_int {
    const MIN_ADDRESS: usize = 1 << 32;
    const PAGE: usize = 4096;
    if (argument.ptr as usize) < MIN_ADDRESS {
        return unsafe { sys(thread, || call(argument.ptr.cast())) };
    }
    let available = if argument.lower.is_null() {
        0
    } else {
        (unsafe { (*argument.object()).upper() } as usize).saturating_sub(argument.ptr as usize)
    };
    object::check_access(argument, available, Access::Read);
    let mut limit = 128;
    loop {
        let extent = available.min(limit);
        let mapped = extent.div_ceil(PAGE).max(1) * PAGE;
        unsafe {
            let map = libc::mmap(
                null_mut(),
                mapped + PAGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if map == libc::MAP_FAILED {
                crate::failure::internal_panic(format_args!("cannot map space for a system call argument"));
            }
            libc::mprotect(map.cast::<u8>().add(mapped).cast(), PAGE, libc::PROT_NONE);
            let space = map.cast::<u8>().add(mapped - extent);
            let original = std::slice::from_raw_parts(argument.ptr, extent).to_vec();
            space.copy_from_nonoverlapping(original.as_ptr(), extent);
            thread::exit(thread);
            *libc::__errno_location() = 0;
            let result = call(space.cast());
            let error = *libc::__errno_location();
            thread::enter(thread);
            if error != libc::EFAULT {
                if error == 0 && std::slice::from_raw_parts(space, extent) != original.as_slice() {
                    object::check_access(argument, extent, Access::Write);
                    argument.ptr.copy_from_nonoverlapping(space, extent);
                }
                libc::munmap(map, mapped + PAGE);
                if error != 0 {
                    set_errno(thread, error);
                }
                return result;
            }
            libc::munmap(map, mapped + PAGE);
        }
        if limit > extent {
            safety_panic(format_args!(
                "was only able to pass {extent} bytes to the system call but the kernel wanted more (ptr = {:p}).",
                argument.ptr
            ));
        }
        limit *= 2;
    }
}

/// A raw `futex` call that reports errors the way musl's `__syscall` does.
unsafe fn futex(address: *mut u8, operation: c_int, a: usize, b: usize, c: usize) -> c_long {
    unsafe {
        let result = libc::syscall(libc::SYS_futex, address, operation, a, b, c);
        if result < 0 { -(*libc::__errno_location() as c_long) } else { result }
    }
}

fn private_flag(private: c_int) -> c_int {
    if private != 0 { FUTEX_PRIVATE } else { 0 }
}

/// Waits until `*address` isn't `value`, or the time comes.
unsafe fn futex_timed_wait(address: *mut u8, value: c_int, clock: c_int, at: *const libc::timespec, private: c_int) -> c_int {
    let operation = FUTEX_WAIT | private_flag(private);
    let mut remaining = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    let mut timeout: *const libc::timespec = null();
    if !at.is_null() {
        let at = unsafe { *at };
        if at.tv_nsec as c_ulong >= 1_000_000_000 {
            return libc::EINVAL;
        }
        if unsafe { libc::clock_gettime(clock, &mut remaining) } != 0 {
            return libc::EINVAL;
        }
        remaining.tv_sec = at.tv_sec - remaining.tv_sec;
        remaining.tv_nsec = at.tv_nsec - remaining.tv_nsec;
        if remaining.tv_nsec < 0 {
            remaining.tv_sec -= 1;
            remaining.tv_nsec += 1_000_000_000;
        }
        if remaining.tv_sec < 0 {
            return libc::ETIMEDOUT;
        }
        timeout = &remaining;
    }
    let result = -(unsafe { futex(address, operation, value as usize, timeout as usize, 0) }) as c_int;
    match result {
        libc::EINTR if crate::signal::INTERRUPTING_HANDLERS.load(Ordering::Relaxed) => libc::EINTR,
        libc::ETIMEDOUT | libc::ECANCELED => result,
        _ => 0,
    }
}

/// `dlopen` hands out a capability for the handle.
unsafe fn dl_handle_object(thread: *mut Thread, handle: *mut c_void) -> Ptr {
    unsafe {
        let object = object::allocate_special(thread, size_of::<*mut c_void>(), SPECIAL_DL_HANDLE);
        lower_for_object(object).cast::<*mut c_void>().write(handle);
        Ptr::for_object(object)
    }
}

/// The `dlsym` handle behind a capability.
fn dl_handle(handle: Ptr) -> *mut c_void {
    if handle.ptr.is_null() {
        return null_mut();
    }
    object::check_special(handle, SPECIAL_DL_HANDLE, "dl handle");
    unsafe { handle.ptr.cast::<*mut c_void>().read() }
}

/// The host's message for the last failure, with what it was about.
unsafe fn report_dl_failure(thread: *mut Thread) {
    let message = unsafe { libc::dlerror() };
    let message = if message.is_null() {
        b"unknown error".to_vec()
    } else {
        unsafe { std::ffi::CStr::from_ptr(message) }.to_bytes().to_vec()
    };
    unsafe { set_dlerror(thread, &message) };
}

natives! {
    // Integers, in and out.
    fn zsys_close_impl(thread: *mut Thread, fd: c_int) -> c_int { unsafe { sys(thread, || libc::close(fd)) } }
    fn zsys_dup_impl(thread: *mut Thread, fd: c_int) -> c_int { unsafe { sys(thread, || libc::dup(fd)) } }
    fn zsys_dup2_impl(thread: *mut Thread, old: c_int, new: c_int) -> c_int { unsafe { sys(thread, || libc::dup2(old, new)) } }
    fn zsys_dup3_impl(thread: *mut Thread, old: c_int, new: c_int, flags: c_int) -> c_int { unsafe { sys(thread, || libc::dup3(old, new, flags)) } }
    fn zsys_close_range_impl(thread: *mut Thread, first: c_uint, last: c_uint, flags: c_int) -> c_int { unsafe { sys(thread, || libc::syscall(libc::SYS_close_range, first, last, flags) as c_int) } }
    fn zsys_lseek(thread: *mut Thread, fd: c_int, offset: i64, whence: c_int) -> i64 { unsafe { sys(thread, || libc::lseek(fd, offset, whence)) } }
    fn zsys_fchdir(thread: *mut Thread, fd: c_int) -> c_int { unsafe { sys(thread, || libc::fchdir(fd)) } }
    fn zsys_fchmod(thread: *mut Thread, fd: c_int, mode: c_uint) -> c_int { unsafe { sys(thread, || libc::fchmod(fd, mode)) } }
    fn zsys_fchown(thread: *mut Thread, fd: c_int, owner: c_uint, group: c_uint) -> c_int { unsafe { sys(thread, || libc::fchown(fd, owner, group)) } }
    fn zsys_fdatasync(thread: *mut Thread, fd: c_int) -> c_int { unsafe { sys(thread, || libc::fdatasync(fd)) } }
    fn zsys_fsync(thread: *mut Thread, fd: c_int) -> c_int { unsafe { sys(thread, || libc::fsync(fd)) } }
    fn zsys_syncfs(thread: *mut Thread, fd: c_int) -> c_int { unsafe { sys(thread, || libc::syncfs(fd)) } }
    fn zsys_sync(thread: *mut Thread) -> () { unsafe { plain(thread, || libc::sync()) } }
    fn zsys_ftruncate(thread: *mut Thread, fd: c_int, length: i64) -> c_int { unsafe { sys(thread, || libc::ftruncate(fd, length)) } }
    fn zsys_flock(thread: *mut Thread, fd: c_int, operation: c_int) -> c_int { unsafe { sys(thread, || libc::flock(fd, operation)) } }
    fn zsys_fallocate(thread: *mut Thread, fd: c_int, mode: c_int, offset: i64, length: i64) -> c_int { unsafe { sys(thread, || libc::fallocate(fd, mode, offset, length)) } }
    fn zsys_posix_fallocate(thread: *mut Thread, fd: c_int, offset: i64, length: i64) -> c_int { unsafe { sys(thread, || libc::posix_fallocate(fd, offset, length)) } }
    fn zsys_posix_fadvise(thread: *mut Thread, fd: c_int, base: i64, length: i64, advice: c_int) -> c_int { unsafe { sys(thread, || libc::posix_fadvise(fd, base, length, advice)) } }
    fn zsys_getuid(thread: *mut Thread) -> u32 { unsafe { plain(thread, || libc::getuid()) } }
    fn zsys_geteuid(thread: *mut Thread) -> u32 { unsafe { plain(thread, || libc::geteuid()) } }
    fn zsys_getgid(thread: *mut Thread) -> u32 { unsafe { plain(thread, || libc::getgid()) } }
    fn zsys_getegid(thread: *mut Thread) -> u32 { unsafe { plain(thread, || libc::getegid()) } }
    fn zsys_getpid(thread: *mut Thread) -> c_int { unsafe { plain(thread, || libc::getpid()) } }
    fn zsys_getppid(thread: *mut Thread) -> c_int { unsafe { plain(thread, || libc::getppid()) } }
    fn zsys_getpgrp(thread: *mut Thread) -> c_int { unsafe { plain(thread, || libc::getpgrp()) } }
    fn zsys_getpgid(thread: *mut Thread, pid: c_int) -> c_int { unsafe { sys(thread, || libc::getpgid(pid)) } }
    fn zsys_getsid(thread: *mut Thread, pid: c_int) -> c_int { unsafe { sys(thread, || libc::getsid(pid)) } }
    fn zsys_setsid(thread: *mut Thread) -> c_int { unsafe { sys(thread, || libc::setsid()) } }
    fn zsys_setpgid(thread: *mut Thread, pid: c_int, group: c_int) -> c_int { unsafe { sys(thread, || libc::setpgid(pid, group)) } }
    fn zsys_setuid(thread: *mut Thread, id: c_uint) -> c_int { unsafe { sys(thread, || libc::setuid(id)) } }
    fn zsys_seteuid(thread: *mut Thread, id: c_uint) -> c_int { unsafe { sys(thread, || libc::seteuid(id)) } }
    fn zsys_setreuid(thread: *mut Thread, real: c_uint, effective: c_uint) -> c_int { unsafe { sys(thread, || libc::setreuid(real, effective)) } }
    fn zsys_setresuid(thread: *mut Thread, real: c_uint, effective: c_uint, saved: c_uint) -> c_int { unsafe { sys(thread, || libc::setresuid(real, effective, saved)) } }
    fn zsys_setgid(thread: *mut Thread, id: c_uint) -> c_int { unsafe { sys(thread, || libc::setgid(id)) } }
    fn zsys_setegid(thread: *mut Thread, id: c_uint) -> c_int { unsafe { sys(thread, || libc::setegid(id)) } }
    fn zsys_setregid(thread: *mut Thread, real: c_uint, effective: c_uint) -> c_int { unsafe { sys(thread, || libc::setregid(real, effective)) } }
    fn zsys_setresgid(thread: *mut Thread, real: c_uint, effective: c_uint, saved: c_uint) -> c_int { unsafe { sys(thread, || libc::setresgid(real, effective, saved)) } }
    fn zsys_setfsuid(thread: *mut Thread, id: c_uint) -> c_int { unsafe { sys(thread, || libc::setfsuid(id)) } }
    fn zsys_setfsgid(thread: *mut Thread, id: c_uint) -> c_int { unsafe { sys(thread, || libc::setfsgid(id)) } }
    fn zsys_getpriority(thread: *mut Thread, which: c_int, who: c_int) -> c_int { unsafe { sys(thread, || libc::getpriority(which as _, who as _)) } }
    fn zsys_setpriority(thread: *mut Thread, which: c_int, who: c_int, priority: c_int) -> c_int { unsafe { sys(thread, || libc::setpriority(which as _, who as _, priority)) } }
    fn zsys_umask(thread: *mut Thread, mask: c_uint) -> u32 { unsafe { plain(thread, || libc::umask(mask)) } }
    fn zsys_mlockall(thread: *mut Thread, flags: c_int) -> c_int { unsafe { sys(thread, || libc::mlockall(flags)) } }
    fn zsys_munlockall(thread: *mut Thread) -> c_int { unsafe { sys(thread, || libc::munlockall()) } }
    fn zsys_sched_yield(thread: *mut Thread) -> () { unsafe { plain(thread, || libc::sched_yield()) }; }
    fn zsys_sched_getcpu(thread: *mut Thread) -> c_int { unsafe { sys(thread, || libc::sched_getcpu()) } }
    fn zsys_sched_getscheduler(thread: *mut Thread, pid: c_int) -> c_int { unsafe { sys(thread, || libc::sched_getscheduler(pid)) } }
    fn zsys_sched_get_priority_min(thread: *mut Thread, policy: c_int) -> c_int { unsafe { sys(thread, || libc::sched_get_priority_min(policy)) } }
    fn zsys_sched_get_priority_max(thread: *mut Thread, policy: c_int) -> c_int { unsafe { sys(thread, || libc::sched_get_priority_max(policy)) } }
    fn zsys_pause(thread: *mut Thread) -> c_int {
        // Only a handler ends it, not being woken for one that's blocked.
        loop {
            let before = unsafe { crate::signal::delivered(thread) };
            let result = unsafe { sys(thread, || libc::pause()) };
            if unsafe { crate::signal::delivered(thread) } != before {
                return result;
            }
        }
    }
    fn zsys_isatty(thread: *mut Thread, fd: c_int) -> c_int { unsafe { sys(thread, || libc::isatty(fd)) } }
    fn zsys_tee(thread: *mut Thread, input: c_int, output: c_int, size: usize, flags: c_uint) -> i64 { unsafe { sys(thread, || libc::tee(input, output, size, flags)) as i64 } }
    fn zsys_eventfd(thread: *mut Thread, value: c_uint, flags: c_int) -> c_int { unsafe { sys(thread, || libc::eventfd(value, flags)) } }
    fn zsys_inotify_init1(thread: *mut Thread, flags: c_int) -> c_int { unsafe { sys(thread, || libc::inotify_init1(flags)) } }
    fn zsys_inotify_rm_watch(thread: *mut Thread, fd: c_int, watch: c_int) -> c_int { unsafe { sys(thread, || libc::inotify_rm_watch(fd, watch)) } }
    fn zsys_timerfd_create(thread: *mut Thread, clock: c_int, flags: c_int) -> c_int { unsafe { sys(thread, || libc::timerfd_create(clock, flags)) } }

    // Paths.
    fn zsys_chdir(thread: *mut Thread, path: Ptr) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::chdir(path.as_ptr())) } }
    fn zsys_chroot(thread: *mut Thread, path: Ptr) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::chroot(path.as_ptr())) } }
    fn zsys_rmdir(thread: *mut Thread, path: Ptr) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::rmdir(path.as_ptr())) } }
    fn zsys_unlink(thread: *mut Thread, path: Ptr) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::unlink(path.as_ptr())) } }
    fn zsys_chmod(thread: *mut Thread, path: Ptr, mode: c_uint) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::chmod(path.as_ptr(), mode)) } }
    fn zsys_mkfifo(thread: *mut Thread, path: Ptr, mode: c_uint) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::mkfifo(path.as_ptr(), mode)) } }
    fn zsys_truncate(thread: *mut Thread, path: Ptr, length: i64) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::truncate(path.as_ptr(), length)) } }
    fn zsys_chown(thread: *mut Thread, path: Ptr, owner: c_uint, group: c_uint) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::chown(path.as_ptr(), owner, group)) } }
    fn zsys_lchown(thread: *mut Thread, path: Ptr, owner: c_uint, group: c_uint) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::lchown(path.as_ptr(), owner, group)) } }
    fn zsys_mknod(thread: *mut Thread, path: Ptr, mode: c_uint, device: u64) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::mknod(path.as_ptr(), mode, device)) } }
    fn zsys_umount2(thread: *mut Thread, path: Ptr, flags: c_int) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::umount2(path.as_ptr(), flags)) } }
    fn zsys_mkdirat(thread: *mut Thread, directory: c_int, path: Ptr, mode: c_uint) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::mkdirat(directory, path.as_ptr(), mode)) } }
    fn zsys_mknodat(thread: *mut Thread, directory: c_int, path: Ptr, mode: c_uint, device: u64) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::mknodat(directory, path.as_ptr(), mode, device)) } }
    fn zsys_unlinkat(thread: *mut Thread, directory: c_int, path: Ptr, flags: c_int) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::unlinkat(directory, path.as_ptr(), flags)) } }
    fn zsys_faccessat(thread: *mut Thread, directory: c_int, path: Ptr, mode: c_int, flags: c_int) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::faccessat(directory, path.as_ptr(), mode, flags)) } }
    fn zsys_fchmodat(thread: *mut Thread, directory: c_int, path: Ptr, mode: c_uint, flags: c_int) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::fchmodat(directory, path.as_ptr(), mode, flags)) } }
    fn zsys_fchownat(thread: *mut Thread, directory: c_int, path: Ptr, owner: c_uint, group: c_uint, flags: c_int) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::fchownat(directory, path.as_ptr(), owner, group, flags)) } }
    fn zsys_inotify_add_watch(thread: *mut Thread, fd: c_int, path: Ptr, mask: c_uint) -> c_int { let path = unsafe { text(path) }; unsafe { sys(thread, || libc::inotify_add_watch(fd, path.as_ptr(), mask)) } }
    fn zsys_memfd_create(thread: *mut Thread, name: Ptr, flags: c_uint) -> c_int { let name = unsafe { text(name) }; unsafe { sys(thread, || libc::memfd_create(name.as_ptr(), flags)) } }

    // Two paths.
    fn zsys_link(thread: *mut Thread, old: Ptr, new: Ptr) -> c_int { let (old, new) = unsafe { (text(old), text(new)) }; unsafe { sys(thread, || libc::link(old.as_ptr(), new.as_ptr())) } }
    fn zsys_symlink(thread: *mut Thread, target: Ptr, path: Ptr) -> c_int { let (target, path) = unsafe { (text(target), text(path)) }; unsafe { sys(thread, || libc::symlink(target.as_ptr(), path.as_ptr())) } }
    fn zsys_rename(thread: *mut Thread, old: Ptr, new: Ptr) -> c_int { let (old, new) = unsafe { (text(old), text(new)) }; unsafe { sys(thread, || libc::rename(old.as_ptr(), new.as_ptr())) } }
    fn zsys_symlinkat(thread: *mut Thread, target: Ptr, directory: c_int, path: Ptr) -> c_int { let (target, path) = unsafe { (text(target), text(path)) }; unsafe { sys(thread, || libc::symlinkat(target.as_ptr(), directory, path.as_ptr())) } }
    fn zsys_linkat(thread: *mut Thread, old_directory: c_int, old: Ptr, new_directory: c_int, new: Ptr, flags: c_int) -> c_int { let (old, new) = unsafe { (text(old), text(new)) }; unsafe { sys(thread, || libc::linkat(old_directory, old.as_ptr(), new_directory, new.as_ptr(), flags)) } }
    fn zsys_renameat(thread: *mut Thread, old_directory: c_int, old: Ptr, new_directory: c_int, new: Ptr) -> c_int { let (old, new) = unsafe { (text(old), text(new)) }; unsafe { sys(thread, || libc::renameat(old_directory, old.as_ptr(), new_directory, new.as_ptr())) } }

    // Buffers.
    fn zsys_read(thread: *mut Thread, fd: c_int, buffer: Ptr, size: usize) -> i64 { let buffer = output(buffer, size); unsafe { sys(thread, || libc::read(fd, buffer, size)) as i64 } }
    fn zsys_write(thread: *mut Thread, fd: c_int, buffer: Ptr, size: usize) -> i64 { let buffer = input(buffer, size); unsafe { sys(thread, || libc::write(fd, buffer, size)) as i64 } }
    fn zsys_pread(thread: *mut Thread, fd: c_int, buffer: Ptr, size: usize, offset: i64) -> i64 { let buffer = output(buffer, size); unsafe { sys(thread, || libc::pread(fd, buffer, size, offset)) as i64 } }
    fn zsys_pwrite(thread: *mut Thread, fd: c_int, buffer: Ptr, size: usize, offset: i64) -> i64 { let buffer = input(buffer, size); unsafe { sys(thread, || libc::pwrite(fd, buffer, size, offset)) as i64 } }
    fn zsys_readv(thread: *mut Thread, fd: c_int, vectors: Ptr, count: c_int) -> i64 { let vectors = unsafe { iovecs(thread, vectors, count, Access::Write) }; unsafe { sys(thread, || libc::readv(fd, vectors.as_ptr(), count)) as i64 } }
    fn zsys_writev(thread: *mut Thread, fd: c_int, vectors: Ptr, count: c_int) -> i64 { let vectors = unsafe { iovecs(thread, vectors, count, Access::Read) }; unsafe { sys(thread, || libc::writev(fd, vectors.as_ptr(), count)) as i64 } }
    fn zsys_preadv(thread: *mut Thread, fd: c_int, vectors: Ptr, count: c_int, offset: i64) -> i64 { let vectors = unsafe { iovecs(thread, vectors, count, Access::Write) }; unsafe { sys(thread, || libc::preadv(fd, vectors.as_ptr(), count, offset)) as i64 } }
    fn zsys_pwritev(thread: *mut Thread, fd: c_int, vectors: Ptr, count: c_int, offset: i64) -> i64 { let vectors = unsafe { iovecs(thread, vectors, count, Access::Read) }; unsafe { sys(thread, || libc::pwritev(fd, vectors.as_ptr(), count, offset)) as i64 } }
    fn zsys_getdents(thread: *mut Thread, fd: c_int, buffer: Ptr, size: usize) -> c_int { let buffer = output::<u8>(buffer, size); unsafe { sys(thread, || libc::syscall(libc::SYS_getdents64, fd, buffer, size) as c_int) } }
    fn zsys_getrandom(thread: *mut Thread, buffer: Ptr, size: usize, flags: c_uint) -> i64 { let buffer = output(buffer, size); unsafe { sys(thread, || libc::getrandom(buffer, size, flags)) as i64 } }
    fn zsys_readlink(thread: *mut Thread, path: Ptr, buffer: Ptr, size: usize) -> i64 { let path = unsafe { text(path) }; let buffer = output(buffer, size); unsafe { sys(thread, || libc::readlink(path.as_ptr(), buffer, size)) as i64 } }
    fn zsys_readlinkat(thread: *mut Thread, directory: c_int, path: Ptr, buffer: Ptr, size: usize) -> i64 { let path = unsafe { text(path) }; let buffer = output(buffer, size); unsafe { sys(thread, || libc::readlinkat(directory, path.as_ptr(), buffer, size)) as i64 } }
    fn zsys_sched_getaffinity(thread: *mut Thread, tid: c_int, size: usize, set: Ptr) -> c_int { let set = output(set, size); unsafe { sys(thread, || libc::sched_getaffinity(tid, size, set)) } }
    fn zsys_sched_setaffinity(thread: *mut Thread, tid: c_int, size: usize, set: Ptr) -> c_int { let set = input(set, size); unsafe { sys(thread, || libc::sched_setaffinity(tid, size, set)) } }
    fn zsys_getgroups(thread: *mut Thread, size: c_int, list: Ptr) -> c_int { let list = output(list, array_size(size_of::<c_uint>(), size.max(0) as usize)); unsafe { sys(thread, || libc::getgroups(size, list)) } }
    fn zsys_setgroups(thread: *mut Thread, size: usize, list: Ptr) -> c_int { let list = input(list, array_size(size_of::<c_uint>(), size)); unsafe { sys(thread, || libc::setgroups(size, list)) } }
    fn zsys_sendfile(thread: *mut Thread, out_fd: c_int, in_fd: c_int, offset: Ptr, count: usize) -> i64 { let offset = maybe_out(offset); unsafe { sys(thread, || libc::sendfile(out_fd, in_fd, offset, count)) as i64 } }
    fn zsys_copy_file_range(thread: *mut Thread, input: c_int, input_offset: Ptr, output: c_int, output_offset: Ptr, size: usize, flags: c_uint) -> i64 { let (a, b) = (maybe_out::<i64>(input_offset), maybe_out::<i64>(output_offset)); unsafe { sys(thread, || libc::syscall(libc::SYS_copy_file_range, input, a, output, b, size, flags)) } }
    fn zsys_splice(thread: *mut Thread, input: c_int, input_offset: Ptr, output: c_int, output_offset: Ptr, size: usize, flags: c_uint) -> i64 { let (a, b) = (maybe_out(input_offset), maybe_out(output_offset)); unsafe { sys(thread, || libc::splice(input, a, output, b, size, flags)) as i64 } }
    fn zsys_flistxattr(thread: *mut Thread, fd: c_int, list: Ptr, size: usize) -> i64 { let list = output(list, size); unsafe { sys(thread, || libc::flistxattr(fd, list, size)) as i64 } }
    fn zsys_listxattr(thread: *mut Thread, path: Ptr, list: Ptr, size: usize) -> i64 { let path = unsafe { text(path) }; let list = output(list, size); unsafe { sys(thread, || libc::listxattr(path.as_ptr(), list, size)) as i64 } }
    fn zsys_llistxattr(thread: *mut Thread, path: Ptr, list: Ptr, size: usize) -> i64 { let path = unsafe { text(path) }; let list = output(list, size); unsafe { sys(thread, || libc::llistxattr(path.as_ptr(), list, size)) as i64 } }
    fn zsys_getxattr(thread: *mut Thread, path: Ptr, name: Ptr, value: Ptr, size: usize) -> i64 { let (path, name) = unsafe { (text(path), text(name)) }; let value = output(value, size); unsafe { sys(thread, || libc::getxattr(path.as_ptr(), name.as_ptr(), value, size)) as i64 } }
    fn zsys_lgetxattr(thread: *mut Thread, path: Ptr, name: Ptr, value: Ptr, size: usize) -> i64 { let (path, name) = unsafe { (text(path), text(name)) }; let value = output(value, size); unsafe { sys(thread, || libc::lgetxattr(path.as_ptr(), name.as_ptr(), value, size)) as i64 } }
    fn zsys_fgetxattr(thread: *mut Thread, fd: c_int, name: Ptr, value: Ptr, size: usize) -> i64 { let name = unsafe { text(name) }; let value = output(value, size); unsafe { sys(thread, || libc::fgetxattr(fd, name.as_ptr(), value, size)) as i64 } }
    fn zsys_setxattr(thread: *mut Thread, path: Ptr, name: Ptr, value: Ptr, size: usize, flags: c_int) -> c_int { let (path, name) = unsafe { (text(path), text(name)) }; let value = input(value, size); unsafe { sys(thread, || libc::setxattr(path.as_ptr(), name.as_ptr(), value, size, flags)) } }
    fn zsys_lsetxattr(thread: *mut Thread, path: Ptr, name: Ptr, value: Ptr, size: usize, flags: c_int) -> c_int { let (path, name) = unsafe { (text(path), text(name)) }; let value = input(value, size); unsafe { sys(thread, || libc::lsetxattr(path.as_ptr(), name.as_ptr(), value, size, flags)) } }
    fn zsys_fsetxattr(thread: *mut Thread, fd: c_int, name: Ptr, value: Ptr, size: usize, flags: c_int) -> c_int { let name = unsafe { text(name) }; let value = input(value, size); unsafe { sys(thread, || libc::fsetxattr(fd, name.as_ptr(), value, size, flags)) } }
    fn zsys_removexattr(thread: *mut Thread, path: Ptr, name: Ptr) -> c_int { let (path, name) = unsafe { (text(path), text(name)) }; unsafe { sys(thread, || libc::removexattr(path.as_ptr(), name.as_ptr())) } }
    fn zsys_lremovexattr(thread: *mut Thread, path: Ptr, name: Ptr) -> c_int { let (path, name) = unsafe { (text(path), text(name)) }; unsafe { sys(thread, || libc::lremovexattr(path.as_ptr(), name.as_ptr())) } }
    fn zsys_fremovexattr(thread: *mut Thread, fd: c_int, name: Ptr) -> c_int { let name = unsafe { text(name) }; unsafe { sys(thread, || libc::fremovexattr(fd, name.as_ptr())) } }

    // Structures that hold no pointers.
    fn zsys_fstat(thread: *mut Thread, fd: c_int, stat: Ptr) -> c_int { let stat = one_out(stat); unsafe { sys(thread, || libc::fstat(fd, stat)) } }
    fn zsys_fstatat(thread: *mut Thread, directory: c_int, path: Ptr, stat: Ptr, flags: c_int) -> c_int { let path = unsafe { text(path) }; let stat = one_out(stat); unsafe { sys(thread, || libc::fstatat(directory, path.as_ptr(), stat, flags)) } }
    fn zsys_statfs(thread: *mut Thread, path: Ptr, buffer: Ptr) -> c_int { let path = unsafe { text(path) }; let buffer = one_out(buffer); unsafe { sys(thread, || libc::statfs(path.as_ptr(), buffer)) } }
    fn zsys_fstatfs(thread: *mut Thread, fd: c_int, buffer: Ptr) -> c_int { let buffer = one_out(buffer); unsafe { sys(thread, || libc::fstatfs(fd, buffer)) } }
    fn zsys_statvfs(thread: *mut Thread, path: Ptr, buffer: Ptr) -> c_int { let path = unsafe { text(path) }; let buffer = one_out(buffer); unsafe { sys(thread, || libc::statvfs(path.as_ptr(), buffer)) } }
    fn zsys_fstatvfs(thread: *mut Thread, fd: c_int, buffer: Ptr) -> c_int { let buffer = one_out(buffer); unsafe { sys(thread, || libc::fstatvfs(fd, buffer)) } }
    fn zsys_clock_gettime(thread: *mut Thread, clock: c_int, time: Ptr) -> c_int { let time = one_out(time); unsafe { sys(thread, || libc::clock_gettime(clock, time)) } }
    fn zsys_clock_getres(thread: *mut Thread, clock: c_int, time: Ptr) -> c_int { let time = maybe_out(time); unsafe { sys(thread, || libc::clock_getres(clock, time)) } }
    fn zsys_clock_nanosleep(thread: *mut Thread, clock: c_int, flags: c_int, request: Ptr, remaining: Ptr) -> c_int { let (request, remaining) = (one_in(request), maybe_out(remaining)); unsafe { sys(thread, || libc::clock_nanosleep(clock, flags, request, remaining)) } }
    fn zsys_nanosleep(thread: *mut Thread, request: Ptr, remaining: Ptr) -> c_int { let (request, remaining) = (one_in(request), maybe_out(remaining)); unsafe { sys(thread, || libc::nanosleep(request, remaining)) } }
    fn zsys_getrlimit(thread: *mut Thread, resource: c_int, limit: Ptr) -> c_int { let limit = one_out(limit); unsafe { sys(thread, || libc::getrlimit(resource as _, limit)) } }
    fn zsys_setrlimit(thread: *mut Thread, resource: c_int, limit: Ptr) -> c_int { let limit = one_in(limit); unsafe { sys(thread, || libc::setrlimit(resource as _, limit)) } }
    fn zsys_getrusage(thread: *mut Thread, who: c_int, usage: Ptr) -> c_int { let usage = one_out(usage); unsafe { sys(thread, || libc::getrusage(who as _, usage)) } }
    fn zsys_getitimer(thread: *mut Thread, which: c_int, value: Ptr) -> c_int { let value = one_out(value); unsafe { sys(thread, || libc::getitimer(which as _, value)) } }
    fn zsys_setitimer(thread: *mut Thread, which: c_int, new: Ptr, old: Ptr) -> c_int { let (new, old) = (one_in(new), maybe_out(old)); unsafe { sys(thread, || libc::setitimer(which as _, new, old)) } }
    fn zsys_uname(thread: *mut Thread, buffer: Ptr) -> c_int { let buffer = one_out(buffer); unsafe { sys(thread, || libc::uname(buffer)) } }
    fn zsys_times(thread: *mut Thread, buffer: Ptr) -> i64 { let buffer = maybe_out(buffer); unsafe { sys(thread, || libc::times(buffer)) } }
    fn zsys_sysinfo(thread: *mut Thread, info: Ptr) -> c_int { let info = one_out(info); unsafe { sys(thread, || libc::sysinfo(info)) } }
    fn zsys_pipe(thread: *mut Thread, fds: Ptr) -> c_int { let fds = output(fds, 2 * size_of::<c_int>()); unsafe { sys(thread, || libc::pipe(fds)) } }
    fn zsys_pipe2(thread: *mut Thread, fds: Ptr, flags: c_int) -> c_int { let fds = output(fds, 2 * size_of::<c_int>()); unsafe { sys(thread, || libc::pipe2(fds, flags)) } }
    fn zsys_getresuid(thread: *mut Thread, real: Ptr, effective: Ptr, saved: Ptr) -> c_int { let (a, b, c) = (one_out(real), one_out(effective), one_out(saved)); unsafe { sys(thread, || libc::getresuid(a, b, c)) } }
    fn zsys_getresgid(thread: *mut Thread, real: Ptr, effective: Ptr, saved: Ptr) -> c_int { let (a, b, c) = (one_out(real), one_out(effective), one_out(saved)); unsafe { sys(thread, || libc::getresgid(a, b, c)) } }
    fn zsys_sched_getparam(thread: *mut Thread, pid: c_int, parameter: Ptr) -> c_int { let parameter = one_out(parameter); unsafe { sys(thread, || libc::sched_getparam(pid, parameter)) } }
    fn zsys_sched_setscheduler(thread: *mut Thread, pid: c_int, policy: c_int, parameter: Ptr) -> c_int { let parameter = one_in(parameter); unsafe { sys(thread, || libc::sched_setscheduler(pid, policy, parameter)) } }
    fn zsys_futimens(thread: *mut Thread, fd: c_int, times: Ptr) -> c_int { let times = if times.ptr.is_null() { null() } else { input(times, 2 * size_of::<libc::timespec>()) }; unsafe { sys(thread, || libc::futimens(fd, times)) } }
    fn zsys_utimensat(thread: *mut Thread, directory: c_int, path: Ptr, times: Ptr, flags: c_int) -> c_int { let path = (!path.ptr.is_null()).then(|| unsafe { text(path) }); let times = if times.ptr.is_null() { null() } else { input(times, 2 * size_of::<libc::timespec>()) }; unsafe { sys(thread, || libc::utimensat(directory, path.as_ref().map_or(null(), |path| path.as_ptr()), times, flags)) } }
    fn zsys_timerfd_settime(thread: *mut Thread, fd: c_int, flags: c_int, new: Ptr, old: Ptr) -> c_int { let (new, old) = (one_in(new), maybe_out(old)); unsafe { sys(thread, || libc::timerfd_settime(fd, flags, new, old)) } }
    fn zsys_timerfd_gettime(thread: *mut Thread, fd: c_int, value: Ptr) -> c_int { let value = one_out(value); unsafe { sys(thread, || libc::timerfd_gettime(fd, value)) } }

    // Waiting on descriptors.
    fn zsys_poll(thread: *mut Thread, fds: Ptr, count: c_ulong, timeout: c_int) -> c_int { let fds = output(fds, array_size(size_of::<libc::pollfd>(), count as usize)); unsafe { sys(thread, || libc::poll(fds, count, timeout)) } }
    fn zsys_ppoll(thread: *mut Thread, fds: Ptr, count: c_ulong, timeout: Ptr, mask: Ptr) -> c_int {
        let fds = output(fds, array_size(size_of::<libc::pollfd>(), count as usize));
        let timeout = maybe_in(timeout);
        let mask = (!mask.ptr.is_null()).then(|| unsafe { user_sigset(mask) });
        unsafe { sys(thread, || libc::ppoll(fds, count, timeout, mask.as_ref().map_or(null(), |mask| mask))) }
    }
    fn zsys_epoll_create1_impl(thread: *mut Thread, flags: c_int) -> c_int { unsafe { sys(thread, || libc::epoll_create1(flags)) } }
    fn zsys_epoll_ctl_impl(thread: *mut Thread, epoll: c_int, operation: c_int, fd: c_int, event: Ptr) -> c_int {
        let mut host = (!event.ptr.is_null()).then(|| unsafe {
            let user = one_in::<UserEpollEvent>(event).read_unaligned();
            libc::epoll_event { events: user.events, u64: user.data }
        });
        unsafe { sys(thread, || libc::epoll_ctl(epoll, operation, fd, host.as_mut().map_or(null_mut(), |host| host))) }
    }
    fn zsys_epoll_wait_impl(thread: *mut Thread, epoll: c_int, events: Ptr, count: c_int, timeout: c_int) -> c_int {
        unsafe { epoll_waited(thread, events, count, |host| libc::epoll_wait(epoll, host, count, timeout)) }
    }
    fn zsys_epoll_pwait_impl(thread: *mut Thread, epoll: c_int, events: Ptr, count: c_int, timeout: c_int, mask: Ptr) -> c_int {
        let mask = (!mask.ptr.is_null()).then(|| unsafe { user_sigset(mask) });
        unsafe { epoll_waited(thread, events, count, |host| libc::epoll_pwait(epoll, host, count, timeout, mask.as_ref().map_or(null(), |mask| mask))) }
    }
    fn zsys_select(thread: *mut Thread, count: c_int, read: Ptr, write: Ptr, except: Ptr, timeout: Ptr) -> c_int {
        if count < 0 {
            unsafe { set_errno(thread, libc::EINVAL) };
            return -1;
        }
        let set = |ptr: Ptr| -> *mut libc::fd_set { if ptr.ptr.is_null() { null_mut() } else { output(ptr, fd_set_size(count)) } };
        let (read, write, except) = (set(read), set(write), set(except));
        let timeout = maybe_out(timeout);
        unsafe { sys(thread, || libc::select(count, read, write, except, timeout)) }
    }
    fn zsys_pselect(thread: *mut Thread, count: c_int, read: Ptr, write: Ptr, except: Ptr, timeout: Ptr, mask: Ptr) -> c_int {
        if count < 0 {
            unsafe { set_errno(thread, libc::EINVAL) };
            return -1;
        }
        let set = |ptr: Ptr| -> *mut libc::fd_set { if ptr.ptr.is_null() { null_mut() } else { output(ptr, fd_set_size(count)) } };
        let (read, write, except) = (set(read), set(write), set(except));
        let timeout = maybe_in(timeout);
        let mask = (!mask.ptr.is_null()).then(|| unsafe { user_sigset(mask) });
        unsafe { sys(thread, || libc::pselect(count, read, write, except, timeout, mask.as_ref().map_or(null(), |mask| mask))) }
    }

    // Files, with variable arguments.
    fn zsys_open(thread: *mut Thread, path: Ptr, flags: c_int, rest: Rest) -> c_int {
        let mode = if rest.has_next() { unsafe { rest.next::<c_uint>(thread) } } else { 0 };
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::open(path.as_ptr(), flags, mode)) }
    }
    fn zsys_openat(thread: *mut Thread, directory: c_int, path: Ptr, flags: c_int, rest: Rest) -> c_int {
        let mode = if rest.has_next() { unsafe { rest.next::<c_uint>(thread) } } else { 0 };
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::openat(directory, path.as_ptr(), flags, mode)) }
    }
    fn zsys_fcntl_impl(thread: *mut Thread, fd: c_int, command: c_int, rest: Rest) -> c_int {
        match command {
            libc::F_DUPFD | libc::F_DUPFD_CLOEXEC | libc::F_SETFD | libc::F_SETFL | libc::F_SETOWN | libc::F_ADD_SEALS
            | libc::F_SETLEASE | libc::F_NOTIFY | libc::F_SETPIPE_SZ | F_SETSIG => {
                let argument = unsafe { rest.next::<c_int>(thread) };
                unsafe { sys(thread, || libc::fcntl(fd, command, argument)) }
            }
            libc::F_GETLK | libc::F_OFD_GETLK => {
                let lock: *mut libc::flock = one_out(unsafe { rest.next::<Ptr>(thread) });
                unsafe { sys(thread, || libc::fcntl(fd, command, lock)) }
            }
            libc::F_SETLK | libc::F_SETLKW | libc::F_OFD_SETLK | libc::F_OFD_SETLKW | libc::F_CANCELLK => {
                let lock: *const libc::flock = one_in(unsafe { rest.next::<Ptr>(thread) });
                unsafe { sys(thread, || libc::fcntl(fd, command, lock)) }
            }
            libc::F_GETFD | libc::F_GETFL | libc::F_GETOWN | libc::F_GET_SEALS | libc::F_GETLEASE | libc::F_GETPIPE_SZ | F_GETSIG => {
                unsafe { sys(thread, || libc::fcntl(fd, command)) }
            }
            _ => {
                unsafe { set_errno(thread, libc::EINVAL) };
                -1
            }
        }
    }
    fn zsys_ioctl(thread: *mut Thread, fd: c_int, request: c_int, rest: Rest) -> c_int {
        let argument = if rest.has_next() { unsafe { rest.next::<Ptr>(thread) } } else { Ptr::NULL };
        unsafe { with_guarded_argument(thread, argument, |guarded| libc::ioctl(fd, request as _, guarded)) }
    }

    // The rest.
    fn zsys_getcwd(thread: *mut Thread, buffer: Ptr, size: usize) -> Ptr {
        if buffer.ptr.is_null() {
            safety_panic(format_args!("buffer passed to zsys_getcwd must not be NULL."));
        }
        if size == 0 {
            safety_panic(format_args!("size passed to zsys_getcwd must not be 0."));
        }
        let raw = output::<c_char>(buffer, size);
        if unsafe { sys(thread, || libc::getcwd(raw, size)) }.is_null() { Ptr::NULL } else { buffer }
    }

    fn zsys_sigprocmask(thread: *mut Thread, how: c_int, set: Ptr, old: Ptr) -> c_int {
        let set = (!set.ptr.is_null()).then(|| unsafe { user_sigset(set) });
        let mut previous = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        let result = unsafe {
            plain(thread, || libc::pthread_sigmask(how, set.as_ref().map_or(null(), |set| set), &mut previous))
        };
        if result != 0 {
            unsafe { set_errno(thread, result) };
            return -1;
        }
        if !old.ptr.is_null() {
            unsafe { one_out::<libc::sigset_t>(old).write(previous) };
        }
        unsafe {
            crate::signal::note_mask(thread);
            crate::signal::deliver_unblocked_soon(thread);
        }
        0
    }

    fn zsys_sigpending(thread: *mut Thread, set: Ptr) -> c_int { let set = one_out(set); unsafe { sys(thread, || libc::sigpending(set)) } }

    fn zsys_raise(thread: *mut Thread, signal: c_int) -> c_int {
        if is_internal_signal(signal) {
            unsafe { set_errno(thread, libc::ENOSYS) };
            return -1;
        }
        unsafe { sys(thread, || libc::raise(signal)) }
    }

    fn zsys_kill(thread: *mut Thread, pid: c_int, signal: c_int) -> c_int {
        if is_internal_signal(signal) {
            unsafe { set_errno(thread, libc::ENOSYS) };
            return -1;
        }
        unsafe { sys(thread, || libc::kill(pid, signal)) }
    }

    fn zsys_exit_soft(thread: *mut Thread, code: c_int) -> () {
        unsafe {
            thread::exit(thread);
            libc::exit(code)
        }
    }

    fn zsys_exit_hard(thread: *mut Thread, code: c_int) -> () {
        unsafe {
            thread::exit(thread);
            libc::_exit(code)
        }
    }

    fn zsys_abort(thread: *mut Thread) -> () {
        unsafe {
            thread::exit(thread);
            libc::abort()
        }
    }

    fn zsys_futex_wake(thread: *mut Thread, address: Ptr, count: c_int, private: c_int) -> () {
        let operation = FUTEX_WAKE | private_flag(private);
        let count = if count < 0 { c_int::MAX } else { count };
        unsafe { plain(thread, || futex(address.ptr, operation, count as usize, 0, 0)) };
    }

    fn zsys_futex_wait(thread: *mut Thread, address: Ptr, value: c_int, private: c_int) -> () {
        let raw = input::<u8>(address, size_of::<c_int>()).cast_mut();
        let operation = FUTEX_WAIT | private_flag(private);
        unsafe { plain(thread, || futex(raw, operation, value as usize, 0, 0)) };
    }

    fn zsys_futex_timedwait(thread: *mut Thread, address: Ptr, value: c_int, clock: c_int, timeout: Ptr, private: c_int) -> c_int {
        let raw = input::<u8>(address, size_of::<c_int>()).cast_mut();
        let timeout = maybe_in(timeout);
        unsafe { plain(thread, || futex_timed_wait(raw, value, clock, timeout, private)) }
    }

    fn zsys_futex_unlock_pi(thread: *mut Thread, address: Ptr, private: c_int) -> c_int {
        let raw = output::<u8>(address, size_of::<c_int>());
        let operation = FUTEX_UNLOCK_PI | private_flag(private);
        unsafe { plain(thread, || futex(raw, operation, 0, 0, 0)) as c_int }
    }

    fn zsys_futex_lock_pi(thread: *mut Thread, address: Ptr, private: c_int, timeout: Ptr) -> c_int {
        let raw = output::<u8>(address, size_of::<c_int>());
        let timeout = maybe_in::<libc::timespec>(timeout);
        let operation = FUTEX_LOCK_PI | private_flag(private);
        unsafe { plain(thread, || futex(raw, operation, 0, timeout as usize, 0)) as c_int }
    }

    fn zsys_futex_requeue(thread: *mut Thread, address: Ptr, private: c_int, wake: c_int, requeue: c_int, target: Ptr) -> () {
        let operation = FUTEX_REQUEUE | private_flag(private);
        unsafe { plain(thread, || futex(address.ptr, operation, wake as usize, requeue as usize, target.ptr as usize)) };
    }

    fn zsys_dlopen(thread: *mut Thread, path: Ptr, flags: c_int) -> Ptr {
        let path = (!path.ptr.is_null()).then(|| unsafe { text(path) });
        let handle = unsafe { plain(thread, || libc::dlopen(path.as_ref().map_or(null(), |path| path.as_ptr()), flags)) };
        if handle.is_null() {
            unsafe { report_dl_failure(thread) };
            return Ptr::NULL;
        }
        unsafe { dl_handle_object(thread, handle) }
    }

    fn zsys_dladdr(thread: *mut Thread, address: Ptr, info: Ptr) -> c_int {
        let mut found: libc::Dl_info = unsafe { std::mem::zeroed() };
        let result = unsafe { plain(thread, || libc::dladdr(address.ptr.cast(), &mut found)) };
        if result == 0 {
            unsafe { report_dl_failure(thread) };
            return 0;
        }
        object::check_access(info, size_of::<libc::Dl_info>(), Access::Write);
        let name = unsafe { std::ffi::CStr::from_ptr(found.dli_fname) }.to_bytes();
        unsafe {
            let name = crate::start::c_string(thread, name);
            let at = |index: usize| info.with_ptr(info.ptr.add(index * size_of::<Ptr>() / 2));
            crate::ptrs::store(thread, at(0), name);
            // No one can get at the object through this address. The symbol
            // isn't known, as it isn't upstream.
            crate::ptrs::store(thread, at(1), Ptr { ptr: found.dli_fbase.cast(), lower: null_mut() });
            crate::ptrs::store(thread, at(2), Ptr::NULL);
            crate::ptrs::store(thread, at(3), Ptr::NULL);
        }
        result
    }

    fn zsys_dlsym(thread: *mut Thread, handle: Ptr, symbol: Ptr) -> Ptr {
        let handle = dl_handle(handle);
        let mut name = b"pizlonated_".to_vec();
        name.extend_from_slice(&unsafe { string_of(symbol) });
        let name = unsafe { CString::from_vec_unchecked(name) };
        let getter = unsafe { plain(thread, || libc::dlsym(handle, name.as_ptr())) };
        if getter.is_null() {
            unsafe { report_dl_failure(thread) };
            return Ptr::NULL;
        }
        unsafe { std::mem::transmute::<*mut c_void, GetterFn>(getter)(thread, null()) }
    }
}
