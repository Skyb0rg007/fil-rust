//! The rest of the system calls: what a program administers its machine with,
//! which needs more than a line because of what has to be looked at first.
//!
//! The ones upstream's musl flavor doesn't support aren't here either, and fail
//! by name: `fanotify_*`, `getcpu`, `pkey_*`, `process_madvise`,
//! `process_mrelease`, `epoll_pwait2` and `dlvsym`.

use std::ffi::{CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::ptr::null;

use crate::abi::*;
use crate::failure::{internal_panic, safety_panic};
use crate::natives::{Rest, natives};
use crate::object::{self, Access};
use crate::ptrs;
use crate::start::set_errno;
use crate::sys::{array_size, input, iovecs_of, is_internal_signal, maybe_in, maybe_out, one_in, one_out, output, plain, sys, text};
use crate::thread;

/// Only x86-64 has these. Anywhere else the kernel will turn them down.
const SYS_ARCH_PRCTL: c_long = if cfg!(target_arch = "x86_64") { 158 } else { -1 };
const SYS_MODIFY_LDT: c_long = if cfg!(target_arch = "x86_64") { 154 } else { -1 };
const SYS_IOPL: c_long = if cfg!(target_arch = "x86_64") { 172 } else { -1 };
const SYS_IOPERM: c_long = if cfg!(target_arch = "x86_64") { 173 } else { -1 };
const SYS_FUTIMESAT: c_long = if cfg!(target_arch = "x86_64") { 261 } else { -1 };
const SYS_STATMOUNT: c_long = 457;
const SYS_LISTMOUNT: c_long = 458;

const ARCH_SET_GS: c_int = 0x1001;
const ARCH_SET_FS: c_int = 0x1002;
const ARCH_GET_FS: c_int = 0x1003;
const ARCH_GET_GS: c_int = 0x1004;
const ARCH_GET_CPUID: c_int = 0x1011;
const ARCH_SET_CPUID: c_int = 0x1012;

const CAP_VERSION_1: u32 = 0x19980330;
const CAP_VERSION_2: u32 = 0x20071026;
const CAP_VERSION_3: u32 = 0x20080522;
const CAP_DATA_SIZE: usize = 12;

const TIMEVAL_SIZE: usize = 16;
const TIMEZONE_SIZE: usize = 8;
const TIMES_SIZE: usize = 2 * TIMEVAL_SIZE;
const KSIGEVENT_SIZE: usize = 24;
const STATX_SIZE: usize = 256;
const SEMBUF_SIZE: usize = 6;
const SEMINFO_SIZE: usize = 40;
const SEMID_DS_SIZE: usize = 104;
const MSQID_DS_SIZE: usize = 120;
const MSG_TEXT_OFFSET: usize = 8;
const FILE_HANDLE_HEADER: usize = 8;
const OPEN_HOW_SIZE: usize = 24;
const LANDLOCK_PATH_BENEATH_SIZE: usize = 12;
const PERF_ATTR_SIZE_OFFSET: usize = 4;
const PERF_ATTR_SIZE_VER0: usize = 64;
const DQBLK_SIZE: usize = 72;
const DQINFO_SIZE: usize = 24;
const SOCK_FPROG_SIZE: usize = 16;
const SOCK_FILTER_SIZE: usize = 8;
const KEYCTL_PKEY_QUERY_SIZE: usize = 56;
const MNT_ID_REQ_SIZE_VER0: usize = 24;
const PAGE_SIZE: usize = 4096;

const SEM_STAT: c_int = 18;
const SEM_INFO: c_int = 19;
const SEM_STAT_ANY: c_int = 20;
const IPC_INFO: c_int = 3;
const GETPID: c_int = 11;
const GETVAL: c_int = 12;
const GETALL: c_int = 13;
const GETNCNT: c_int = 14;
const GETZCNT: c_int = 15;
const SETVAL: c_int = 16;
const SETALL: c_int = 17;

/// Runs a system call that takes machine words.
macro_rules! call {
    ($thread:expr, $number:expr $(, $argument:expr)* $(,)?) => {
        sys($thread, || libc::syscall($number $(, $argument)*))
    };
}

fn optional(string: &Option<CString>) -> *const c_char {
    string.as_ref().map_or(null(), |string| string.as_ptr())
}

unsafe fn text_or_null(ptr: Ptr) -> Option<CString> {
    (!ptr.ptr.is_null()).then(|| unsafe { text(ptr) })
}

pub(crate) fn fail(thread: *mut Thread, error: c_int) -> c_int {
    unsafe { set_errno(thread, error) };
    -1
}

fn fail_long(thread: *mut Thread, error: c_int) -> i64 {
    fail(thread, error) as i64
}

fn nodemask_size(maxnode: c_ulong) -> usize {
    let bits = c_ulong::BITS as usize;
    let Some(total) = (maxnode as usize).checked_add(bits - 1) else {
        safety_panic(format_args!("size argument too big, causes overflow; size = {maxnode}."));
    };
    array_size(size_of::<c_ulong>(), total / bits)
}

fn add_size(a: usize, b: usize) -> usize {
    a.checked_add(b).unwrap_or_else(|| safety_panic(format_args!("size argument too big, causes overflow; size = {b}.")))
}

/// Checks a `struct file_handle`, whose size is in it.
fn file_handle(handle: Ptr) -> *mut c_void {
    let header = output::<u32>(handle, FILE_HANDLE_HEADER);
    let size = add_size(FILE_HANDLE_HEADER, unsafe { *header } as usize);
    output(handle, size)
}

/// A copy of what the kernel takes as a sized request, since it reads the size
/// again and another thread could change it in between. `Err` is for errno.
fn mount_request(request: Ptr) -> Result<Vec<u64>, c_int> {
    let size = unsafe { *one_in::<u32>(request) } as usize;
    if size < MNT_ID_REQ_SIZE_VER0 {
        return Err(libc::EINVAL);
    }
    if size > PAGE_SIZE {
        return Err(libc::E2BIG);
    }
    let bytes = input::<u8>(request, size);
    let mut copy = vec![0u64; size.div_ceil(8)];
    unsafe {
        std::ptr::copy_nonoverlapping(bytes, copy.as_mut_ptr().cast(), size);
        *copy.as_mut_ptr().cast::<u32>() = size as u32;
    }
    Ok(copy)
}

/// Sees that a version of capabilities is known, or tells the caller which is.
unsafe fn capability_data(thread: *mut Thread, header: *mut u32, data: Ptr, access: Access) -> Result<(), c_int> {
    if data.ptr.is_null() {
        return Ok(());
    }
    let size = match unsafe { *header } {
        CAP_VERSION_1 => CAP_DATA_SIZE,
        CAP_VERSION_2 | CAP_VERSION_3 => 2 * CAP_DATA_SIZE,
        _ => unsafe {
            *header = CAP_VERSION_3;
            let mut data = [0u8; 2 * CAP_DATA_SIZE];
            return Err(call!(thread, libc::SYS_capget, header, data.as_mut_ptr()) as c_int);
        },
    };
    object::check_access(data, size, access);
    Ok(())
}

#[repr(C)]
struct DhParams {
    private: i32,
    prime: i32,
    base: i32,
}

#[repr(C)]
struct KdfParams {
    hash_name: *const c_char,
    other_info: *const c_char,
    other_info_length: u32,
    spare: [u32; 8],
}

#[repr(C)]
struct PkeyParams {
    key_id: i32,
    in_length: u32,
    out_length: u32,
    spare: [u32; 7],
}

const KEYCTL_GET_KEYRING_ID: c_int = 0;
const KEYCTL_JOIN_SESSION_KEYRING: c_int = 1;
const KEYCTL_UPDATE: c_int = 2;
const KEYCTL_REVOKE: c_int = 3;
const KEYCTL_CHOWN: c_int = 4;
const KEYCTL_SETPERM: c_int = 5;
const KEYCTL_DESCRIBE: c_int = 6;
const KEYCTL_CLEAR: c_int = 7;
const KEYCTL_LINK: c_int = 8;
const KEYCTL_UNLINK: c_int = 9;
const KEYCTL_SEARCH: c_int = 10;
const KEYCTL_READ: c_int = 11;
const KEYCTL_INSTANTIATE: c_int = 12;
const KEYCTL_NEGATE: c_int = 13;
const KEYCTL_SET_REQKEY_KEYRING: c_int = 14;
const KEYCTL_SET_TIMEOUT: c_int = 15;
const KEYCTL_ASSUME_AUTHORITY: c_int = 16;
const KEYCTL_GET_SECURITY: c_int = 17;
const KEYCTL_SESSION_TO_PARENT: c_int = 18;
const KEYCTL_REJECT: c_int = 19;
const KEYCTL_INSTANTIATE_IOV: c_int = 20;
const KEYCTL_INVALIDATE: c_int = 21;
const KEYCTL_GET_PERSISTENT: c_int = 22;
const KEYCTL_DH_COMPUTE: c_int = 23;
const KEYCTL_PKEY_QUERY: c_int = 24;
const KEYCTL_PKEY_ENCRYPT: c_int = 25;
const KEYCTL_PKEY_DECRYPT: c_int = 26;
const KEYCTL_PKEY_SIGN: c_int = 27;
const KEYCTL_PKEY_VERIFY: c_int = 28;
const KEYCTL_RESTRICT_KEYRING: c_int = 29;
const KEYCTL_MOVE: c_int = 30;

#[allow(clippy::too_many_arguments)] // As many as keyctl's PKEY operations take.
unsafe fn pkey_transform(thread: *mut Thread, operation: c_int, key: c_int, info: Ptr, data: Ptr, data_length: usize, out: Ptr, out_length: usize) -> i64 {
    let info = unsafe { text_or_null(info) };
    let data = input::<c_void>(data, data_length);
    let out = output::<c_void>(out, out_length);
    let params = PkeyParams { key_id: key, in_length: data_length as u32, out_length: out_length as u32, spare: [0; 7] };
    unsafe { call!(thread, libc::SYS_keyctl, operation, &params, optional(&info), data, out) }
}

fn check_single_thread(what: &str) {
    let threads = thread::lock_threads().0.len();
    if threads != 1 {
        safety_panic(format_args!("cannot be multithreaded while doing {what} (currently have {threads} threads)."));
    }
}

unsafe fn prctl(thread: *mut Thread, option: c_int, a: usize, b: usize, c: usize, d: usize) -> c_int {
    unsafe { sys(thread, || libc::prctl(option, a, b, c, d)) }
}

unsafe fn set_seccomp(thread: *mut Thread, rest: &mut Rest) -> c_int {
    const SECCOMP_MODE_STRICT: usize = 1;
    const SECCOMP_MODE_FILTER: usize = 2;
    let mode: usize = unsafe { rest.next(thread) };
    if mode == SECCOMP_MODE_STRICT {
        internal_panic(format_args!("cannot PR_SET_SECCOMP SECCOMP_MODE_STRICT; that's currently too restrictive for the Fil-C runtime."));
    }
    if mode != SECCOMP_MODE_FILTER {
        return fail(thread, libc::ENOSYS);
    }
    check_single_thread("PR_SET_SECCOMP");
    #[repr(C)]
    struct Program {
        length: u16,
        filter: *const c_void,
    }
    let program: Ptr = unsafe { rest.next(thread) };
    let user = input::<Program>(program, SOCK_FPROG_SIZE);
    unsafe {
        let filter = ptrs::load(program.with_ptr((&raw const (*user).filter).cast_mut().cast()));
        thread::track(thread, filter.object());
        let length = (*user).length;
        if length != 0 && !filter.ptr.is_null() {
            object::check_access(filter, array_size(SOCK_FILTER_SIZE, length as usize), Access::Read);
        }
        let native = Program { length, filter: filter.ptr.cast() };
        prctl(thread, libc::PR_SET_SECCOMP, mode, &native as *const Program as usize, 0, 0)
    }
}

natives! {
    fn zsys_access(thread: *mut Thread, path: Ptr, mode: c_int) -> c_int {
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::access(path.as_ptr(), mode)) }
    }

    fn zsys_eaccess(thread: *mut Thread, path: Ptr, mode: c_int) -> c_int {
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::eaccess(path.as_ptr(), mode)) }
    }

    fn zsys_acct(thread: *mut Thread, path: Ptr) -> c_int {
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::acct(path.as_ptr())) }
    }

    fn zsys_mkdir(thread: *mut Thread, path: Ptr, mode: c_uint) -> c_int {
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::mkdir(path.as_ptr(), mode)) }
    }

    fn zsys_lchmod(thread: *mut Thread, path: Ptr, mode: c_uint) -> c_int {
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::fchmodat(libc::AT_FDCWD, path.as_ptr(), mode, libc::AT_SYMLINK_NOFOLLOW)) }
    }

    fn zsys_utimes(thread: *mut Thread, path: Ptr, times: Ptr) -> c_int {
        let path = unsafe { text(path) };
        let times = if times.ptr.is_null() { null() } else { input::<libc::timeval>(times, TIMES_SIZE) };
        unsafe { sys(thread, || libc::utimes(path.as_ptr(), times)) }
    }

    fn zsys_lutimes(thread: *mut Thread, path: Ptr, times: Ptr) -> c_int {
        let path = unsafe { text(path) };
        let times = if times.ptr.is_null() { null() } else { input::<libc::timeval>(times, TIMES_SIZE) };
        unsafe { sys(thread, || libc::lutimes(path.as_ptr(), times)) }
    }

    fn zsys_futimes(thread: *mut Thread, fd: c_int, times: Ptr) -> c_int {
        let times = if times.ptr.is_null() { null() } else { input::<libc::timeval>(times, TIMES_SIZE) };
        unsafe { sys(thread, || libc::futimes(fd, times)) }
    }

    fn zsys_futimesat(thread: *mut Thread, fd: c_int, path: Ptr, times: Ptr) -> c_int {
        let times = if times.ptr.is_null() { null() } else { input::<libc::timeval>(times, TIMES_SIZE) };
        let path = unsafe { text(path) };
        unsafe { call!(thread, SYS_FUTIMESAT, fd, path.as_ptr(), times) as c_int }
    }

    fn zsys_alarm(thread: *mut Thread, seconds: c_uint) -> u32 {
        unsafe { plain(thread, || libc::alarm(seconds)) }
    }

    fn zsys_gettimeofday(thread: *mut Thread, time: Ptr, zone: Ptr) -> c_int {
        let time = if time.ptr.is_null() { null_mut() } else { output::<libc::timeval>(time, TIMEVAL_SIZE) };
        let zone = if zone.ptr.is_null() { null_mut() } else { output::<c_void>(zone, TIMEZONE_SIZE) };
        unsafe { sys(thread, || libc::gettimeofday(time, zone.cast())) }
    }

    fn zsys_settimeofday(thread: *mut Thread, time: Ptr, zone: Ptr) -> c_int {
        let time = if time.ptr.is_null() { null() } else { input::<libc::timeval>(time, TIMEVAL_SIZE) };
        let zone = if zone.ptr.is_null() { null() } else { input::<libc::timezone>(zone, TIMEZONE_SIZE) };
        unsafe { sys(thread, || libc::settimeofday(time, zone)) }
    }

    fn zsys_adjtime(thread: *mut Thread, delta: Ptr, old: Ptr) -> c_int {
        let delta = if delta.ptr.is_null() { null() } else { input::<libc::timeval>(delta, TIMEVAL_SIZE) };
        let old = if old.ptr.is_null() { null_mut() } else { output::<libc::timeval>(old, TIMEVAL_SIZE) };
        unsafe { sys(thread, || libc::adjtime(delta, old)) }
    }

    fn zsys_clock_adjtime(thread: *mut Thread, clock: c_int, buffer: Ptr) -> c_int {
        let buffer = one_out::<libc::timex>(buffer);
        unsafe { sys(thread, || libc::clock_adjtime(clock, buffer)) }
    }

    fn zsys_clock_settime(thread: *mut Thread, clock: c_int, time: Ptr) -> c_int {
        let time = one_in::<libc::timespec>(time);
        unsafe { sys(thread, || libc::clock_settime(clock, time)) }
    }

    fn zsys_timer_create(thread: *mut Thread, clock: c_int, event: Ptr, timer: Ptr) -> c_int {
        let event = if event.ptr.is_null() { null_mut() } else { output::<c_void>(event, KSIGEVENT_SIZE) };
        let timer = one_out::<c_int>(timer);
        unsafe { call!(thread, libc::SYS_timer_create, clock, event, timer) as c_int }
    }

    fn zsys_timer_delete(thread: *mut Thread, timer: c_int) -> c_int {
        unsafe { call!(thread, libc::SYS_timer_delete, timer) as c_int }
    }

    fn zsys_timer_getoverrun(thread: *mut Thread, timer: c_int) -> c_int {
        unsafe { call!(thread, libc::SYS_timer_getoverrun, timer) as c_int }
    }

    fn zsys_timer_gettime(thread: *mut Thread, timer: c_int, value: Ptr) -> c_int {
        let value = maybe_out::<libc::itimerspec>(value);
        unsafe { call!(thread, libc::SYS_timer_gettime, timer, value) as c_int }
    }

    fn zsys_timer_settime(thread: *mut Thread, timer: c_int, flags: c_int, value: Ptr, old: Ptr) -> c_int {
        let value = maybe_in::<libc::itimerspec>(value);
        let old = maybe_out::<libc::itimerspec>(old);
        unsafe { call!(thread, libc::SYS_timer_settime, timer, flags, value, old) as c_int }
    }

    fn zsys_sched_rr_get_interval(thread: *mut Thread, pid: c_int, interval: Ptr) -> c_int {
        let interval = one_out::<libc::timespec>(interval);
        unsafe { sys(thread, || libc::sched_rr_get_interval(pid, interval)) }
    }

    fn zsys_sched_setparam(thread: *mut Thread, pid: c_int, parameters: Ptr) -> c_int {
        let parameters = one_in::<libc::sched_param>(parameters);
        unsafe { sys(thread, || libc::sched_setparam(pid, parameters)) }
    }

    fn zsys_raw_sched_getaffinity(thread: *mut Thread, tid: c_int, size: usize, set: Ptr) -> c_int {
        let set = output::<c_ulong>(set, size);
        unsafe { call!(thread, libc::SYS_sched_getaffinity, tid, size, set) as c_int }
    }

    fn zsys_get_mempolicy(thread: *mut Thread, mode: Ptr, nodemask: Ptr, maxnode: u64, address: Ptr, flags: u64) -> i64 {
        let mode = maybe_out::<c_int>(mode);
        let nodemask = if nodemask.ptr.is_null() { null_mut() } else { output::<c_ulong>(nodemask, nodemask_size(maxnode)) };
        if !address.ptr.is_null() {
            object::check_access(address, 1, Access::Read);
        }
        unsafe { call!(thread, libc::SYS_get_mempolicy, mode, nodemask, maxnode, address.ptr, flags) }
    }

    fn zsys_set_mempolicy(thread: *mut Thread, mode: c_int, nodemask: Ptr, maxnode: u64) -> i64 {
        let nodemask = if nodemask.ptr.is_null() { null() } else { input::<c_ulong>(nodemask, nodemask_size(maxnode)) };
        unsafe { call!(thread, libc::SYS_set_mempolicy, mode, nodemask, maxnode) }
    }

    fn zsys_gethostname(thread: *mut Thread, name: Ptr, length: usize) -> c_int {
        let name = output::<c_char>(name, length);
        unsafe { sys(thread, || libc::gethostname(name, length)) }
    }

    fn zsys_sethostname(thread: *mut Thread, name: Ptr, length: usize) -> c_int {
        let name = input::<c_char>(name, length);
        unsafe { sys(thread, || libc::sethostname(name, length)) }
    }

    fn zsys_getdomainname(thread: *mut Thread, name: Ptr, length: usize) -> c_int {
        let name = output::<c_char>(name, length);
        unsafe { sys(thread, || libc::getdomainname(name, length)) }
    }

    fn zsys_setdomainname(thread: *mut Thread, name: Ptr, length: usize) -> c_int {
        let name = input::<c_char>(name, length);
        unsafe { sys(thread, || libc::setdomainname(name, length)) }
    }

    fn zsys_fpathconf(thread: *mut Thread, fd: c_int, name: c_int) -> i64 {
        unsafe { sys(thread, || libc::fpathconf(fd, name)) }
    }

    fn zsys_pathconf(thread: *mut Thread, path: Ptr, name: c_int) -> i64 {
        let path = unsafe { text(path) };
        unsafe { sys(thread, || libc::pathconf(path.as_ptr(), name)) }
    }

    fn zsys_inotify_init(thread: *mut Thread) -> c_int {
        unsafe { sys(thread, || libc::inotify_init()) }
    }

    fn zsys_statx(thread: *mut Thread, fd: c_int, path: Ptr, flags: c_int, mask: c_uint, result: Ptr) -> c_int {
        let path = unsafe { text(path) };
        let result = output::<c_void>(result, STATX_SIZE);
        unsafe { call!(thread, libc::SYS_statx, fd, path.as_ptr(), flags, mask, result) as c_int }
    }

    fn zsys_renameat2(thread: *mut Thread, old_fd: c_int, old: Ptr, new_fd: c_int, new: Ptr, flags: c_uint) -> c_int {
        let old = unsafe { text(old) };
        let new = unsafe { text(new) };
        unsafe { call!(thread, libc::SYS_renameat2, old_fd, old.as_ptr(), new_fd, new.as_ptr(), flags) as c_int }
    }

    fn zsys_openat2(thread: *mut Thread, fd: c_int, path: Ptr, how: Ptr, size: usize) -> c_int {
        let path = unsafe { text(path) };
        let bytes = input::<u8>(how, size);
        // A later kernel could grow this with a pointer, so what it would
        // look at beyond the fields known here has to be zero.
        let mut actual_size = size;
        if size > OPEN_HOW_SIZE {
            for index in OPEN_HOW_SIZE..size {
                if unsafe { *bytes.add(index) } != 0 {
                    return fail(thread, libc::E2BIG);
                }
            }
            actual_size = OPEN_HOW_SIZE;
        }
        unsafe { call!(thread, libc::SYS_openat2, fd, path.as_ptr(), bytes, actual_size) as c_int }
    }

    fn zsys_name_to_handle_at(thread: *mut Thread, fd: c_int, path: Ptr, handle: Ptr, mount_id: Ptr, flags: c_int) -> c_int {
        let path = unsafe { text(path) };
        let handle = file_handle(handle);
        let mount_id = one_out::<c_int>(mount_id);
        unsafe { call!(thread, libc::SYS_name_to_handle_at, fd, path.as_ptr(), handle, mount_id, flags) as c_int }
    }

    fn zsys_open_by_handle_at(thread: *mut Thread, fd: c_int, handle: Ptr, flags: c_int) -> c_int {
        let handle = file_handle(handle);
        unsafe { call!(thread, libc::SYS_open_by_handle_at, fd, handle, flags) as c_int }
    }

    fn zsys_mount(thread: *mut Thread, source: Ptr, target: Ptr, kind: Ptr, flags: u64, data: Ptr) -> c_int {
        let source = unsafe { text_or_null(source) };
        let target = unsafe { text(target) };
        let kind = unsafe { text_or_null(kind) };
        let data = unsafe { text_or_null(data) };
        unsafe { sys(thread, || libc::mount(optional(&source), target.as_ptr(), optional(&kind), flags, optional(&data).cast())) }
    }

    fn zsys_mount_setattr(thread: *mut Thread, fd: c_int, path: Ptr, flags: c_uint, attributes: Ptr, size: usize) -> c_int {
        let path = unsafe { text_or_null(path) };
        let attributes = input::<c_void>(attributes, size);
        unsafe { call!(thread, libc::SYS_mount_setattr, fd, optional(&path), flags, attributes, size) as c_int }
    }

    fn zsys_move_mount(thread: *mut Thread, from_fd: c_int, from: Ptr, to_fd: c_int, to: Ptr, flags: c_uint) -> c_int {
        let from = unsafe { text(from) };
        let to = unsafe { text(to) };
        unsafe { call!(thread, libc::SYS_move_mount, from_fd, from.as_ptr(), to_fd, to.as_ptr(), flags) as c_int }
    }

    fn zsys_open_tree(thread: *mut Thread, fd: c_int, path: Ptr, flags: c_uint) -> c_int {
        let path = unsafe { text(path) };
        unsafe { call!(thread, libc::SYS_open_tree, fd, path.as_ptr(), flags) as c_int }
    }

    fn zsys_fsopen(thread: *mut Thread, name: Ptr, flags: c_uint) -> c_int {
        let name = unsafe { text(name) };
        unsafe { call!(thread, libc::SYS_fsopen, name.as_ptr(), flags) as c_int }
    }

    fn zsys_fspick(thread: *mut Thread, fd: c_int, path: Ptr, flags: c_uint) -> c_int {
        let path = unsafe { text(path) };
        unsafe { call!(thread, libc::SYS_fspick, fd, path.as_ptr(), flags) as c_int }
    }

    fn zsys_fsmount(thread: *mut Thread, fd: c_int, flags: c_uint, attributes: c_uint) -> c_int {
        unsafe { call!(thread, libc::SYS_fsmount, fd, flags, attributes) as c_int }
    }

    fn zsys_fsconfig(thread: *mut Thread, fd: c_int, command: c_uint, key: Ptr, data: Ptr, auxiliary: c_int) -> c_int {
        const SET_FLAG: c_uint = 0;
        const SET_STRING: c_uint = 1;
        const SET_BINARY: c_uint = 2;
        const SET_PATH: c_uint = 3;
        const SET_PATH_EMPTY: c_uint = 4;
        const SET_FD: c_uint = 5;
        const CMD_CREATE: c_uint = 6;
        const CMD_RECONFIGURE: c_uint = 7;
        const CMD_CREATE_EXCL: c_uint = 8;
        let null_data = |name: &str| {
            if !data.ptr.is_null() {
                safety_panic(format_args!("data must be NULL for {name}."));
            }
        };
        let null_auxiliary = |name: &str| {
            if auxiliary != 0 {
                safety_panic(format_args!("aux must be 0 for {name}."));
            }
        };
        let mut string = None;
        let mut raw = null();
        let mut need_key = true;
        match command {
            SET_FLAG => {
                null_data("FSCONFIG_SET_FLAG");
                null_auxiliary("FSCONFIG_SET_FLAG");
            }
            SET_STRING => {
                null_auxiliary("FSCONFIG_SET_STRING");
                string = Some(unsafe { text(data) });
            }
            SET_BINARY => raw = input::<c_void>(data, auxiliary as usize),
            SET_PATH | SET_PATH_EMPTY => string = Some(unsafe { text(data) }),
            SET_FD => null_data("FSCONFIG_SET_FD"),
            CMD_CREATE | CMD_RECONFIGURE | CMD_CREATE_EXCL => {
                let name = match command {
                    CMD_CREATE => "FSCONFIG_CMD_CREATE",
                    CMD_RECONFIGURE => "FSCONFIG_CMD_RECONFIGURE",
                    _ => "FSCONFIG_CMD_CREATE_EXCL",
                };
                need_key = false;
                if !key.ptr.is_null() {
                    safety_panic(format_args!("key must be NULL for {name}."));
                }
                null_data(name);
                null_auxiliary(name);
            }
            _ => return fail(thread, libc::EINVAL),
        }
        let key = if need_key { Some(unsafe { text(key) }) } else { None };
        if let Some(string) = &string {
            raw = string.as_ptr().cast();
        }
        unsafe { call!(thread, libc::SYS_fsconfig, fd, command, optional(&key), raw, auxiliary) as c_int }
    }

    fn zsys_statmount(thread: *mut Thread, request: Ptr, buffer: Ptr, size: usize, flags: c_uint) -> i64 {
        let request = match mount_request(request) {
            Ok(request) => request,
            Err(error) => return fail_long(thread, error),
        };
        let buffer = output::<c_void>(buffer, size);
        unsafe { call!(thread, SYS_STATMOUNT, request.as_ptr(), buffer, size, flags) }
    }

    fn zsys_listmount(thread: *mut Thread, request: Ptr, list: Ptr, count: usize, flags: c_uint) -> i64 {
        let request = match mount_request(request) {
            Ok(request) => request,
            Err(error) => return fail_long(thread, error),
        };
        let list = output::<u64>(list, array_size(8, count));
        unsafe { call!(thread, SYS_LISTMOUNT, request.as_ptr(), list, count, flags) }
    }

    fn zsys_pivot_root(thread: *mut Thread, new_root: Ptr, put_old: Ptr) -> c_int {
        let new_root = unsafe { text(new_root) };
        let put_old = unsafe { text(put_old) };
        unsafe { call!(thread, libc::SYS_pivot_root, new_root.as_ptr(), put_old.as_ptr()) as c_int }
    }

    fn zsys_swapon(thread: *mut Thread, path: Ptr, flags: c_int) -> c_int {
        let path = unsafe { text(path) };
        unsafe { call!(thread, libc::SYS_swapon, path.as_ptr(), flags) as c_int }
    }

    fn zsys_swapoff(thread: *mut Thread, path: Ptr) -> c_int {
        let path = unsafe { text(path) };
        unsafe { call!(thread, libc::SYS_swapoff, path.as_ptr()) as c_int }
    }

    fn zsys_quotactl(thread: *mut Thread, command: c_int, special: Ptr, id: c_int, address: Ptr) -> c_int {
        const Q_SYNC: c_int = 0x800001;
        const Q_QUOTAON: c_int = 0x800002;
        const Q_QUOTAOFF: c_int = 0x800003;
        const Q_GETFMT: c_int = 0x800004;
        const Q_GETINFO: c_int = 0x800005;
        const Q_SETINFO: c_int = 0x800006;
        const Q_GETQUOTA: c_int = 0x800007;
        const Q_SETQUOTA: c_int = 0x800008;
        let special = unsafe { text_or_null(special) };
        let mut string = None;
        let mut raw = null::<c_char>();
        match (command as c_uint >> 8) as c_int {
            Q_QUOTAON => string = unsafe { text_or_null(address) },
            Q_QUOTAOFF | Q_SYNC => {}
            Q_GETQUOTA => raw = output::<c_char>(address, DQBLK_SIZE),
            Q_SETQUOTA => raw = input::<c_char>(address, DQBLK_SIZE),
            Q_GETINFO => raw = output::<c_char>(address, DQINFO_SIZE),
            Q_SETINFO => raw = input::<c_char>(address, DQINFO_SIZE),
            Q_GETFMT => raw = output::<c_char>(address, 4),
            _ => safety_panic(format_args!("unrecognized quotactl command {command}.")),
        }
        if string.is_some() {
            raw = optional(&string);
        }
        unsafe { call!(thread, libc::SYS_quotactl, command, optional(&special), id, raw) as c_int }
    }

    fn zsys_reboot(thread: *mut Thread, how: c_int) -> c_int {
        unsafe { sys(thread, || libc::reboot(how)) }
    }

    fn zsys_syslog(thread: *mut Thread, kind: c_int, buffer: Ptr, length: c_int) -> c_int {
        const CONSOLE_LEVEL: c_int = 8;
        if kind != CONSOLE_LEVEL {
            object::check_access(buffer, length as usize, Access::Write);
        }
        unsafe { sys(thread, || libc::klogctl(kind, buffer.ptr.cast(), length)) }
    }

    fn zsys_unshare(thread: *mut Thread, flags: c_int) -> c_int {
        unsafe { sys(thread, || libc::unshare(flags)) }
    }

    fn zsys_setns(thread: *mut Thread, fd: c_int, kind: c_int) -> c_int {
        unsafe { sys(thread, || libc::setns(fd, kind)) }
    }

    fn zsys_vhangup(thread: *mut Thread) -> c_int {
        unsafe { call!(thread, libc::SYS_vhangup) as c_int }
    }

    fn zsys_personality(thread: *mut Thread, persona: u64) -> c_int {
        unsafe { sys(thread, || libc::personality(persona)) }
    }

    fn zsys_ioperm(thread: *mut Thread, from: u64, count: u64, on: c_int) -> c_int {
        unsafe { call!(thread, SYS_IOPERM, from, count, on) as c_int }
    }

    fn zsys_iopl(thread: *mut Thread, level: c_int) -> c_int {
        unsafe { call!(thread, SYS_IOPL, level) as c_int }
    }

    fn zsys_modify_ldt(thread: *mut Thread, function: c_int, buffer: Ptr, size: u64) -> c_int {
        if function != 0 {
            safety_panic(format_args!("modify_ldt with func != 0 not allowed."));
        }
        let buffer = output::<c_void>(buffer, size as usize);
        unsafe { call!(thread, SYS_MODIFY_LDT, 0, buffer, size) as c_int }
    }

    fn zsys_arch_prctl(thread: *mut Thread, code: c_int, address: Ptr) -> c_int {
        match code {
            ARCH_SET_CPUID => {}
            ARCH_GET_CPUID | ARCH_GET_FS | ARCH_GET_GS => {
                object::check_access(address, size_of::<c_ulong>(), Access::Write);
            }
            ARCH_SET_FS => safety_panic(format_args!("cannot arch_prctl(ARCH_SET_FS).")),
            ARCH_SET_GS => safety_panic(format_args!("cannot arch_prctl(ARCH_SET_GS).")),
            _ => return fail(thread, libc::EINVAL),
        }
        unsafe { call!(thread, SYS_ARCH_PRCTL, code, address.ptr) as c_int }
    }

    fn zsys_capget(thread: *mut Thread, header: Ptr, data: Ptr) -> c_int {
        let header = output::<u32>(header, 8);
        if let Err(result) = unsafe { capability_data(thread, header, data, Access::Write) } {
            return result;
        }
        unsafe { call!(thread, libc::SYS_capget, header, data.ptr) as c_int }
    }

    fn zsys_capset(thread: *mut Thread, header: Ptr, data: Ptr) -> c_int {
        let header = output::<u32>(header, 8);
        if let Err(result) = unsafe { capability_data(thread, header, data, Access::Read) } {
            return result;
        }
        unsafe { call!(thread, libc::SYS_capset, header, data.ptr) as c_int }
    }

    fn zsys_init_module(thread: *mut Thread, image: Ptr, length: u64, parameters: Ptr) -> c_int {
        let image = input::<c_void>(image, length as usize);
        let parameters = unsafe { text(parameters) };
        unsafe { call!(thread, libc::SYS_init_module, image, length, parameters.as_ptr()) as c_int }
    }

    fn zsys_finit_module(thread: *mut Thread, fd: c_int, parameters: Ptr, flags: c_int) -> c_int {
        let parameters = unsafe { text(parameters) };
        unsafe { call!(thread, libc::SYS_finit_module, fd, parameters.as_ptr(), flags) as c_int }
    }

    fn zsys_delete_module(thread: *mut Thread, name: Ptr, flags: c_int) -> c_int {
        let name = unsafe { text(name) };
        unsafe { call!(thread, libc::SYS_delete_module, name.as_ptr(), flags) as c_int }
    }

    fn zsys_landlock_create_ruleset(thread: *mut Thread, attributes: Ptr, size: usize, flags: c_uint) -> c_int {
        let attributes = input::<c_void>(attributes, size);
        unsafe { call!(thread, libc::SYS_landlock_create_ruleset, attributes, size, flags) as c_int }
    }

    fn zsys_landlock_add_rule(thread: *mut Thread, fd: c_int, kind: c_int, attributes: Ptr, flags: c_uint) -> c_int {
        const LANDLOCK_RULE_PATH_BENEATH: c_int = 1;
        if kind != LANDLOCK_RULE_PATH_BENEATH {
            return fail(thread, libc::ENOSYS);
        }
        let attributes = input::<c_void>(attributes, LANDLOCK_PATH_BENEATH_SIZE);
        unsafe { call!(thread, libc::SYS_landlock_add_rule, fd, kind, attributes, flags) as c_int }
    }

    fn zsys_landlock_restrict_self(thread: *mut Thread, fd: c_int, flags: c_uint) -> c_int {
        unsafe { call!(thread, libc::SYS_landlock_restrict_self, fd, flags) as c_int }
    }

    fn zsys_perf_event_open(thread: *mut Thread, attributes: Ptr, pid: c_int, cpu: c_int, fd: c_int, flags: u64) -> c_int {
        let size_at = attributes.with_ptr(attributes.ptr.wrapping_add(PERF_ATTR_SIZE_OFFSET));
        // A size of zero has the kernel read the smallest version there is.
        let size = (unsafe { *one_in::<c_uint>(size_at) } as usize).max(PERF_ATTR_SIZE_VER0);
        // It isn't known whether the kernel writes this.
        let attributes = output::<c_void>(attributes, size);
        unsafe { call!(thread, libc::SYS_perf_event_open, attributes, pid, cpu, fd, flags) as c_int }
    }

    fn zsys_pidfd_open(thread: *mut Thread, pid: c_int, flags: c_uint) -> c_int {
        unsafe { call!(thread, libc::SYS_pidfd_open, pid, flags) as c_int }
    }

    fn zsys_pidfd_getfd(thread: *mut Thread, fd: c_int, target: c_int, flags: c_uint) -> c_int {
        unsafe { call!(thread, libc::SYS_pidfd_getfd, fd, target, flags) as c_int }
    }

    fn zsys_pidfd_send_signal(thread: *mut Thread, fd: c_int, signal: c_int, info: Ptr, flags: c_uint) -> c_int {
        let info = maybe_out::<libc::siginfo_t>(info);
        unsafe { call!(thread, libc::SYS_pidfd_send_signal, fd, signal, info, flags) as c_int }
    }

    fn zsys_wait4(thread: *mut Thread, pid: c_int, status: Ptr, options: c_int, usage: Ptr) -> c_int {
        let status = maybe_out::<c_int>(status);
        let usage = maybe_out::<libc::rusage>(usage);
        unsafe { sys(thread, || libc::wait4(pid, status, options, usage)) }
    }

    fn zsys_waitpid(thread: *mut Thread, pid: c_int, status: Ptr, options: c_int) -> c_int {
        let status = maybe_out::<c_int>(status);
        unsafe { sys(thread, || libc::waitpid(pid, status, options)) }
    }

    fn zsys_waitid(thread: *mut Thread, kind: c_int, id: c_uint, info: Ptr, options: c_int) -> c_int {
        let info = maybe_out::<libc::siginfo_t>(info);
        unsafe { sys(thread, || libc::waitid(kind as libc::idtype_t, id, info, options)) }
    }

    fn zsys_semget(thread: *mut Thread, key: c_int, count: c_int, flags: c_int) -> c_int {
        unsafe { sys(thread, || libc::semget(key, count, flags)) }
    }

    fn zsys_semctl(thread: *mut Thread, id: c_int, number: c_int, command: c_int, rest: Rest) -> c_int {
        let structure = |rest: &mut Rest, size: usize| {
            let ptr: Ptr = unsafe { rest.next(thread) };
            output::<c_void>(ptr, size) as usize
        };
        let argument = match command {
            libc::IPC_STAT | libc::IPC_SET | SEM_STAT => structure(&mut rest, SEMID_DS_SIZE),
            libc::IPC_RMID | GETNCNT | GETPID | GETVAL | GETZCNT => 0,
            IPC_INFO | SEM_INFO | SEM_STAT_ANY => structure(&mut rest, SEMINFO_SIZE),
            GETALL | SETALL => return fail(thread, libc::ENOSYS),
            SETVAL => unsafe { rest.next::<c_int>(thread) as usize },
            _ => return fail(thread, libc::EINVAL),
        };
        unsafe { call!(thread, libc::SYS_semctl, id, number, command, argument) as c_int }
    }

    fn zsys_semop(thread: *mut Thread, id: c_int, operations: Ptr, count: usize) -> c_int {
        let operations = output::<libc::sembuf>(operations, array_size(SEMBUF_SIZE, count));
        unsafe { sys(thread, || libc::semop(id, operations, count)) }
    }

    fn zsys_semtimedop(thread: *mut Thread, id: c_int, operations: Ptr, count: usize, timeout: Ptr) -> c_int {
        let timeout = maybe_in::<libc::timespec>(timeout);
        let operations = output::<libc::sembuf>(operations, array_size(SEMBUF_SIZE, count));
        unsafe { call!(thread, libc::SYS_semtimedop, id, operations, count, timeout) as c_int }
    }

    fn zsys_msgget(thread: *mut Thread, key: c_int, flags: c_int) -> c_int {
        unsafe { sys(thread, || libc::msgget(key, flags)) }
    }

    fn zsys_msgctl(thread: *mut Thread, id: c_int, command: c_int, buffer: Ptr) -> c_int {
        let buffer = if buffer.ptr.is_null() { null_mut() } else { output::<libc::msqid_ds>(buffer, MSQID_DS_SIZE) };
        unsafe { sys(thread, || libc::msgctl(id, command, buffer)) }
    }

    fn zsys_msgsnd(thread: *mut Thread, id: c_int, message: Ptr, size: usize, flags: c_int) -> c_int {
        let message = input::<c_void>(message, add_size(MSG_TEXT_OFFSET, size));
        unsafe { sys(thread, || libc::msgsnd(id, message, size, flags)) }
    }

    fn zsys_msgrcv(thread: *mut Thread, id: c_int, message: Ptr, size: usize, kind: i64, flags: c_int) -> i64 {
        let message = output::<c_void>(message, add_size(MSG_TEXT_OFFSET, size));
        unsafe { sys(thread, || libc::msgrcv(id, message, size, kind, flags) as i64) }
    }

    fn zsys_add_key(thread: *mut Thread, kind: Ptr, description: Ptr, payload: Ptr, length: usize, keyring: c_int) -> c_int {
        let kind = unsafe { text(kind) };
        let description = unsafe { text(description) };
        let payload = input::<c_void>(payload, length);
        unsafe { call!(thread, libc::SYS_add_key, kind.as_ptr(), description.as_ptr(), payload, length, keyring) as c_int }
    }

    fn zsys_request_key(thread: *mut Thread, kind: Ptr, description: Ptr, callout: Ptr, keyring: c_int) -> c_int {
        let kind = unsafe { text(kind) };
        let description = unsafe { text(description) };
        let callout = unsafe { text_or_null(callout) };
        unsafe { call!(thread, libc::SYS_request_key, kind.as_ptr(), description.as_ptr(), optional(&callout), keyring) as c_int }
    }

    fn zsys_keyctl(thread: *mut Thread, operation: c_int, rest: Rest) -> i64 {
        macro_rules! next { ($ty:ty) => { unsafe { rest.next::<$ty>(thread) } } }
        match operation {
            KEYCTL_GET_KEYRING_ID | KEYCTL_LINK | KEYCTL_SETPERM | KEYCTL_UNLINK | KEYCTL_SET_TIMEOUT | KEYCTL_GET_PERSISTENT => {
                let (a, b) = (next!(c_int), next!(c_int));
                unsafe { call!(thread, libc::SYS_keyctl, operation, a, b) }
            }
            KEYCTL_JOIN_SESSION_KEYRING => {
                let description = next!(Ptr);
                let description = unsafe { text_or_null(description) };
                unsafe { call!(thread, libc::SYS_keyctl, operation, optional(&description)) }
            }
            KEYCTL_REVOKE | KEYCTL_CLEAR | KEYCTL_SET_REQKEY_KEYRING | KEYCTL_ASSUME_AUTHORITY | KEYCTL_INVALIDATE => {
                let a = next!(c_int);
                unsafe { call!(thread, libc::SYS_keyctl, operation, a) }
            }
            KEYCTL_UPDATE => {
                let (id, payload, length) = (next!(c_int), next!(Ptr), next!(usize));
                let payload = input::<c_void>(payload, length);
                unsafe { call!(thread, libc::SYS_keyctl, operation, id, payload, length) }
            }
            KEYCTL_CHOWN | KEYCTL_NEGATE => {
                let (a, b, c) = (next!(c_int), next!(c_int), next!(c_int));
                unsafe { call!(thread, libc::SYS_keyctl, operation, a, b, c) }
            }
            KEYCTL_DESCRIBE | KEYCTL_READ | KEYCTL_GET_SECURITY => {
                let (id, buffer, length) = (next!(c_int), next!(Ptr), next!(usize));
                let buffer = output::<c_void>(buffer, length);
                unsafe { call!(thread, libc::SYS_keyctl, operation, id, buffer, length) }
            }
            KEYCTL_SEARCH => {
                let (keyring, kind, description, destination) = (next!(c_int), next!(Ptr), next!(Ptr), next!(c_int));
                let kind = unsafe { text(kind) };
                let description = unsafe { text(description) };
                unsafe { call!(thread, libc::SYS_keyctl, operation, keyring, kind.as_ptr(), description.as_ptr(), destination) }
            }
            KEYCTL_INSTANTIATE => {
                let (id, payload, length, keyring) = (next!(c_int), next!(Ptr), next!(usize), next!(c_int));
                let payload = input::<c_void>(payload, length);
                unsafe { call!(thread, libc::SYS_keyctl, operation, id, payload, length, keyring) }
            }
            KEYCTL_SESSION_TO_PARENT => unsafe { call!(thread, libc::SYS_keyctl, operation) },
            KEYCTL_REJECT | KEYCTL_MOVE => {
                let (a, b, c, d) = (next!(c_int), next!(c_int), next!(c_int), next!(c_int));
                unsafe { call!(thread, libc::SYS_keyctl, operation, a, b, c, d) }
            }
            KEYCTL_INSTANTIATE_IOV => {
                let (id, list, count, keyring) = (next!(c_int), next!(Ptr), next!(c_uint), next!(c_int));
                let list = unsafe { iovecs_of(thread, list, count as usize, Some(Access::Read)) };
                unsafe { call!(thread, libc::SYS_keyctl, operation, id, list.as_ptr(), count, keyring) }
            }
            KEYCTL_RESTRICT_KEYRING => {
                let (keyring, kind, restriction) = (next!(c_int), next!(Ptr), next!(Ptr));
                let kind = unsafe { text_or_null(kind) };
                let restriction = unsafe { text_or_null(restriction) };
                unsafe { call!(thread, libc::SYS_keyctl, operation, keyring, optional(&kind), optional(&restriction)) }
            }
            KEYCTL_DH_COMPUTE => safety_panic(format_args!("invalid direct use of keyctl(KEYCTL_DH_COMPUTE); use keyctl_dh_compute or keyctl_dh_compute_kdf instead.")),
            KEYCTL_PKEY_QUERY => safety_panic(format_args!("invalid direct use of keyctl(KEYCTL_PKEY_QUERY); use keyctl_pkey_query instead.")),
            KEYCTL_PKEY_ENCRYPT => safety_panic(format_args!("invalid direct use of keyctl(KEYCTL_PKEY_ENCRYPT); use keyctl_pkey_encrypt instead.")),
            KEYCTL_PKEY_DECRYPT => safety_panic(format_args!("invalid direct use of keyctl(KEYCTL_PKEY_DECRYPT); use keyctl_pkey_decrypt instead.")),
            KEYCTL_PKEY_SIGN => safety_panic(format_args!("invalid direct use of keyctl(KEYCTL_PKEY_SIGN); use keyctl_pkey_sign instead.")),
            KEYCTL_PKEY_VERIFY => safety_panic(format_args!("invalid direct use of keyctl(KEYCTL_PKEY_VERIFY); use keyctl_pkey_verify instead.")),
            _ => fail_long(thread, libc::ENOSYS),
        }
    }

    fn zsys_keyctl_dh_compute(thread: *mut Thread, private: c_int, prime: c_int, base: c_int, buffer: Ptr, length: usize) -> i64 {
        let buffer = output::<c_void>(buffer, length);
        let params = DhParams { private, prime, base };
        unsafe { call!(thread, libc::SYS_keyctl, KEYCTL_DH_COMPUTE, &params, buffer, length, null::<c_void>()) }
    }

    fn zsys_keyctl_dh_compute_kdf(thread: *mut Thread, private: c_int, prime: c_int, base: c_int, hash_name: Ptr, other_info: Ptr, other_length: usize, buffer: Ptr, length: usize) -> i64 {
        let hash_name = unsafe { text(hash_name) };
        let other_info = output::<c_char>(other_info, other_length);
        let buffer = output::<c_void>(buffer, length);
        let params = DhParams { private, prime, base };
        let kdf = KdfParams { hash_name: hash_name.as_ptr(), other_info, other_info_length: other_length as u32, spare: [0; 8] };
        unsafe { call!(thread, libc::SYS_keyctl, KEYCTL_DH_COMPUTE, &params, buffer, length, &kdf) }
    }

    fn zsys_keyctl_pkey_query(thread: *mut Thread, key: c_int, info: Ptr, result: Ptr) -> i64 {
        let info = unsafe { text_or_null(info) };
        let result = if result.ptr.is_null() { null_mut() } else { output::<c_void>(result, KEYCTL_PKEY_QUERY_SIZE) };
        unsafe { call!(thread, libc::SYS_keyctl, KEYCTL_PKEY_QUERY, key, null::<c_void>(), optional(&info), result) }
    }

    fn zsys_keyctl_pkey_encrypt(thread: *mut Thread, key: c_int, info: Ptr, data: Ptr, data_length: usize, out: Ptr, out_length: usize) -> i64 {
        unsafe { pkey_transform(thread, KEYCTL_PKEY_ENCRYPT, key, info, data, data_length, out, out_length) }
    }

    fn zsys_keyctl_pkey_decrypt(thread: *mut Thread, key: c_int, info: Ptr, data: Ptr, data_length: usize, out: Ptr, out_length: usize) -> i64 {
        unsafe { pkey_transform(thread, KEYCTL_PKEY_DECRYPT, key, info, data, data_length, out, out_length) }
    }

    fn zsys_keyctl_pkey_sign(thread: *mut Thread, key: c_int, info: Ptr, data: Ptr, data_length: usize, out: Ptr, out_length: usize) -> i64 {
        unsafe { pkey_transform(thread, KEYCTL_PKEY_SIGN, key, info, data, data_length, out, out_length) }
    }

    fn zsys_keyctl_pkey_verify(thread: *mut Thread, key: c_int, info: Ptr, data: Ptr, data_length: usize, signature: Ptr, signature_length: usize) -> i64 {
        let info = unsafe { text_or_null(info) };
        let data = input::<c_void>(data, data_length);
        let signature = input::<c_void>(signature, signature_length);
        let params = PkeyParams { key_id: key, in_length: data_length as u32, out_length: signature_length as u32, spare: [0; 7] };
        unsafe { call!(thread, libc::SYS_keyctl, KEYCTL_PKEY_VERIFY, &params, optional(&info), data, signature) }
    }

    fn zsys_prctl(thread: *mut Thread, option: c_int, rest: Rest) -> c_int {
        const PR_SET_TIMING: c_int = 14;
        const PR_SET_TSC: c_int = 26;
        const PR_SET_FPEMU: c_int = 10;
        const PR_SVE_SET_VL: c_int = 50;
        const PR_SVE_GET_VL: c_int = 51;
        const PR_GET_FPEMU: c_int = 9;
        const PR_GET_FPEXC: c_int = 11;
        const PR_GET_TIMING: c_int = 13;
        const PR_GET_TSC: c_int = 25;
        const PR_GET_IO_FLUSHER: c_int = 58;
        const PR_GET_ENDIAN: c_int = 19;
        const PR_SET_ENDIAN: c_int = 20;
        const PR_GET_UNALIGN: c_int = 5;
        const PR_SET_UNALIGN: c_int = 6;
        const PR_SET_TAGGED_ADDR_CTRL: c_int = 55;
        const PR_GET_TAGGED_ADDR_CTRL: c_int = 56;
        const PR_GET_FP_MODE: c_int = 46;
        match option {
            libc::PR_SET_DUMPABLE | libc::PR_SET_CHILD_SUBREAPER | PR_SET_FPEMU | libc::PR_SET_KEEPCAPS
            | libc::PR_SET_SECUREBITS | PR_SVE_SET_VL | libc::PR_SET_THP_DISABLE | PR_SET_TIMING | PR_SET_TSC => {
                let value: usize = unsafe { rest.next(thread) };
                unsafe { prctl(thread, option, value, 0, 0, 0) }
            }
            libc::PR_SET_PDEATHSIG => {
                let value: usize = unsafe { rest.next(thread) };
                if is_internal_signal(value as c_int) {
                    safety_panic(format_args!("cannot set signal {value} as PTHREADSIG."));
                }
                unsafe { prctl(thread, option, value, 0, 0, 0) }
            }
            libc::PR_SET_NO_NEW_PRIVS => {
                let value: usize = unsafe { rest.next(thread) };
                check_single_thread("PR_SET_NO_NEW_PRIVS");
                unsafe { prctl(thread, option, value, 0, 0, 0) }
            }
            libc::PR_GET_SPECULATION_CTRL | libc::PR_SET_SPECULATION_CTRL => {
                let (a, b, c, d) = unsafe { (rest.next(thread), rest.next(thread), rest.next(thread), rest.next(thread)) };
                unsafe { prctl(thread, option, a, b, c, d) }
            }
            libc::PR_SET_SECCOMP => unsafe { set_seccomp(thread, &mut rest) },
            libc::PR_SET_NAME => {
                let name = unsafe { text(rest.next(thread)) };
                unsafe { prctl(thread, option, name.as_ptr() as usize, 0, 0, 0) }
            }
            libc::PR_GET_NAME => {
                let buffer: Ptr = unsafe { rest.next(thread) };
                let buffer = output::<c_char>(buffer, 16);
                unsafe { prctl(thread, option, buffer as usize, 0, 0, 0) }
            }
            libc::PR_GET_DUMPABLE | libc::PR_GET_NO_NEW_PRIVS | libc::PR_GET_SECCOMP | libc::PR_GET_TIMERSLACK
            | PR_GET_FP_MODE | PR_GET_IO_FLUSHER | libc::PR_GET_KEEPCAPS | libc::PR_GET_SECUREBITS | PR_SVE_GET_VL
            | PR_GET_TAGGED_ADDR_CTRL | libc::PR_GET_THP_DISABLE | PR_GET_TIMING => unsafe { prctl(thread, option, 0, 0, 0, 0) },
            PR_GET_ENDIAN | PR_SET_ENDIAN | PR_SET_TAGGED_ADDR_CTRL | PR_SET_UNALIGN | PR_GET_UNALIGN => fail(thread, libc::EINVAL),
            PR_GET_FPEXC | libc::PR_GET_PDEATHSIG | libc::PR_GET_CHILD_SUBREAPER | PR_GET_FPEMU | PR_GET_TSC => {
                let result: Ptr = unsafe { rest.next(thread) };
                let result = one_out::<c_int>(result);
                unsafe { prctl(thread, option, result as usize, 0, 0, 0) }
            }
            _ => fail(thread, libc::ENOSYS),
        }
    }
}
