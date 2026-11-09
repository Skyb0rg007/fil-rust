//! Sockets.
//!
//! A `msghdr` and the `cmsghdr`s in its control data are copied, with any
//! padding cleaned, as musl's `sendmsg` does: musl's layout keeps some lengths
//! in 32 bits with padding after them where the kernel's are 64 bits, and the
//! control data is whatever the program wrote.

use std::ffi::{c_int, c_uint, c_void};
use std::mem::zeroed;
use std::ptr::null_mut;

use crate::abi::*;
use crate::natives::natives;
use crate::object::{self, Access};
use crate::ptrs;
use crate::glibc::set_errno;
use crate::sys::{array_size, input, iovecs, maybe_out, one_in, one_out, output, sys};
use crate::thread;

#[repr(C)]
struct UserMessage {
    name: *mut c_void,
    name_length: c_uint,
    iovecs: *mut c_void,
    iovec_count: c_int,
    padding_1: c_int,
    control: *mut c_void,
    control_length: c_uint,
    padding_2: c_uint,
    flags: c_int,
}

#[repr(C)]
struct UserMultiMessage {
    message: UserMessage,
    length: c_uint,
}

/// The message the kernel gets, and the memory it refers to.
struct Message {
    header: libc::msghdr,
    _iovecs: Vec<libc::iovec>,
    _control: Vec<u64>,
}

/// The most control data musl will send, which copies it on its stack.
const MAX_CONTROL: usize = 1024;

/// musl's `cmsghdr`: the kernel reads `length` and the padding as one length.
#[repr(C)]
struct UserControlHeader {
    length: c_uint,
    padding: c_int,
    level: c_int,
    kind: c_int,
}

/// Zeroes the padding of each header, walking them the way musl's
/// `CMSG_FIRSTHDR` and `CMSG_NXTHDR` do.
fn clean_control(control: &mut [u8]) {
    let header = size_of::<UserControlHeader>();
    let mut at = 0;
    while control.len() - at >= header {
        let length = u32::from_ne_bytes(control[at..at + 4].try_into().unwrap()) as usize;
        control[at + 4..at + 8].fill(0);
        let step = length.next_multiple_of(size_of::<usize>());
        if length < header || step + header >= control.len() - at {
            break;
        }
        at += step;
    }
}

/// Loads a pointer out of a `UserMessage`, and sees that it has `size` bytes.
unsafe fn field(thread: *mut Thread, message: Ptr, at: *mut *mut c_void, size: usize, access: Access) -> *mut c_void {
    unsafe {
        let value = ptrs::load(message.with_ptr(at.cast()));
        thread::track(thread, value.object());
        object::check_access(value, size, access);
        value.ptr.cast()
    }
}

/// Reads a `UserMessage`, which the caller has checked. Fails with an `errno`.
unsafe fn message(thread: *mut Thread, user: Ptr, access: Access) -> Result<Message, c_int> {
    unsafe {
        let this = user.ptr.cast::<UserMessage>();
        let mut iovec_list = {
            let list = ptrs::load(user.with_ptr((&raw mut (*this).iovecs).cast()));
            thread::track(thread, list.object());
            iovecs(thread, list, (*this).iovec_count, access)
        };
        let mut header: libc::msghdr = zeroed();
        header.msg_iov = iovec_list.as_mut_ptr();
        header.msg_iovlen = iovec_list.len();
        header.msg_flags = (*this).flags;
        let name_length = (*this).name_length;
        if name_length != 0 {
            header.msg_name = field(thread, user, &raw mut (*this).name, name_length as usize, access);
            header.msg_namelen = name_length;
        }
        let control_length = (*this).control_length as usize;
        let mut control = Vec::new();
        if control_length != 0 {
            let user_control = field(thread, user, &raw mut (*this).control, control_length, access);
            header.msg_control = user_control;
            header.msg_controllen = control_length;
            if matches!(access, Access::Read) {
                if control_length > MAX_CONTROL {
                    return Err(libc::ENOMEM);
                }
                control = vec![0u64; control_length.div_ceil(size_of::<u64>())];
                let bytes = std::slice::from_raw_parts_mut(control.as_mut_ptr().cast::<u8>(), control_length);
                std::ptr::copy_nonoverlapping(user_control.cast::<u8>(), bytes.as_mut_ptr(), control_length);
                clean_control(bytes);
                header.msg_control = control.as_mut_ptr().cast();
            }
        }
        Ok(Message { header, _iovecs: iovec_list, _control: control })
    }
}

unsafe fn report_message(header: &libc::msghdr, user: *mut UserMessage) {
    unsafe {
        (*user).name_length = header.msg_namelen;
        (*user).control_length = header.msg_controllen as c_uint;
        (*user).flags = header.msg_flags;
    }
}

/// How much room the caller gave for an address, which it can only leave out
/// if it isn't asking for one.
unsafe fn address_room(thread: *mut Thread, address: Ptr, length: Ptr) -> Result<Option<c_uint>, ()> {
    if !length.ptr.is_null() {
        let room = unsafe { *one_out::<c_uint>(length) };
        object::check_access(address, room as usize, Access::Write);
        return Ok(Some(room));
    }
    if !address.ptr.is_null() {
        unsafe { set_errno(thread, libc::EINVAL) };
        return Err(());
    }
    Ok(None)
}

/// A call that fills in an address.
unsafe fn with_address<R: PartialOrd + Default>(
    thread: *mut Thread,
    address: Ptr,
    length: Ptr,
    failed: R,
    call: impl FnOnce(*mut libc::sockaddr, *mut libc::socklen_t) -> R,
) -> R {
    let Ok(room) = (unsafe { address_room(thread, address, length) }) else { return failed };
    let mut room = room;
    let room_at = room.as_mut().map_or(null_mut(), |room| room as *mut c_uint);
    let result = unsafe { sys(thread, || call(address.ptr.cast(), room_at)) };
    if result >= R::default()
        && let Some(room) = room
    {
        unsafe { *one_out::<c_uint>(length) = room };
    }
    result
}

/// `getsockname` and the like, which need the address.
unsafe fn name_call(
    thread: *mut Thread,
    address: Ptr,
    length: Ptr,
    call: impl FnOnce(*mut libc::sockaddr, *mut libc::socklen_t) -> c_int,
) -> c_int {
    let mut room = unsafe { *one_out::<c_uint>(length) };
    object::check_access(address, room as usize, Access::Write);
    let result = unsafe { sys(thread, || call(address.ptr.cast(), &mut room)) };
    if result >= 0 {
        unsafe { *one_out::<c_uint>(length) = room };
    }
    result
}

natives! {
    fn zsys_socket(thread: *mut Thread, domain: c_int, kind: c_int, protocol: c_int) -> c_int {
        unsafe { sys(thread, || libc::socket(domain, kind, protocol)) }
    }

    fn zsys_socketpair(thread: *mut Thread, domain: c_int, kind: c_int, protocol: c_int, pair: Ptr) -> c_int {
        let pair = output::<c_int>(pair, 2 * size_of::<c_int>());
        unsafe { sys(thread, || libc::socketpair(domain, kind, protocol, pair)) }
    }

    fn zsys_bind(thread: *mut Thread, fd: c_int, address: Ptr, length: c_uint) -> c_int {
        let address = input::<libc::sockaddr>(address, length as usize);
        unsafe { sys(thread, || libc::bind(fd, address, length)) }
    }

    fn zsys_connect(thread: *mut Thread, fd: c_int, address: Ptr, length: c_uint) -> c_int {
        let address = input::<libc::sockaddr>(address, length as usize);
        unsafe { sys(thread, || libc::connect(fd, address, length)) }
    }

    fn zsys_listen(thread: *mut Thread, fd: c_int, backlog: c_int) -> c_int {
        unsafe { sys(thread, || libc::listen(fd, backlog)) }
    }

    fn zsys_shutdown(thread: *mut Thread, fd: c_int, how: c_int) -> c_int {
        unsafe { sys(thread, || libc::shutdown(fd, how)) }
    }

    fn zsys_accept(thread: *mut Thread, fd: c_int, address: Ptr, length: Ptr) -> c_int {
        unsafe { with_address(thread, address, length, -1, |address, length| libc::accept(fd, address, length)) }
    }

    fn zsys_accept4(thread: *mut Thread, fd: c_int, address: Ptr, length: Ptr, flags: c_int) -> c_int {
        unsafe { with_address(thread, address, length, -1, |address, length| libc::accept4(fd, address, length, flags)) }
    }

    fn zsys_getsockname(thread: *mut Thread, fd: c_int, address: Ptr, length: Ptr) -> c_int {
        unsafe { name_call(thread, address, length, |address, length| libc::getsockname(fd, address, length)) }
    }

    fn zsys_getpeername(thread: *mut Thread, fd: c_int, address: Ptr, length: Ptr) -> c_int {
        unsafe { name_call(thread, address, length, |address, length| libc::getpeername(fd, address, length)) }
    }

    fn zsys_getsockopt(thread: *mut Thread, fd: c_int, level: c_int, name: c_int, value: Ptr, length: Ptr) -> c_int {
        unsafe { name_call(thread, value, length, |value, length| libc::getsockopt(fd, level, name, value.cast(), length)) }
    }

    fn zsys_setsockopt(thread: *mut Thread, fd: c_int, level: c_int, name: c_int, value: Ptr, length: c_uint) -> c_int {
        let value = input::<c_void>(value, length as usize);
        unsafe { sys(thread, || libc::setsockopt(fd, level, name, value, length)) }
    }

    fn zsys_sendto(thread: *mut Thread, fd: c_int, buffer: Ptr, length: usize, flags: c_int, address: Ptr, address_length: c_uint) -> i64 {
        let buffer = input::<c_void>(buffer, length);
        let address = input::<libc::sockaddr>(address, address_length as usize);
        unsafe { sys(thread, || libc::sendto(fd, buffer, length, flags, address, address_length) as i64) }
    }

    fn zsys_recvfrom(thread: *mut Thread, fd: c_int, buffer: Ptr, length: usize, flags: c_int, address: Ptr, address_length: Ptr) -> i64 {
        let buffer = output::<c_void>(buffer, length);
        unsafe {
            with_address(thread, address, address_length, -1, |address, address_length| {
                libc::recvfrom(fd, buffer, length, flags, address, address_length) as i64
            })
        }
    }

    fn zsys_sendmsg(thread: *mut Thread, fd: c_int, user: Ptr, flags: c_int) -> i64 {
        one_in::<UserMessage>(user);
        let message = match unsafe { message(thread, user, Access::Read) } {
            Ok(message) => message,
            Err(error) => return crate::calls::fail(thread, error) as i64,
        };
        unsafe { sys(thread, || libc::sendmsg(fd, &message.header, flags) as i64) }
    }

    fn zsys_recvmsg(thread: *mut Thread, fd: c_int, user: Ptr, flags: c_int) -> i64 {
        one_out::<UserMessage>(user);
        let Ok(mut message) = (unsafe { message(thread, user, Access::Write) }) else { unreachable!() };
        let result = unsafe { sys(thread, || libc::recvmsg(fd, &mut message.header, flags) as i64) };
        if result >= 0 {
            unsafe { report_message(&message.header, one_out(user)) };
        }
        result
    }

    fn zsys_sendmmsg(thread: *mut Thread, fd: c_int, user: Ptr, count: c_uint, flags: c_int) -> c_int {
        let size = array_size(size_of::<UserMultiMessage>(), count as usize);
        let entries = output::<UserMultiMessage>(user, size);
        let messages: Result<Vec<Message>, c_int> = (0..count as usize)
            .map(|index| unsafe { message(thread, user.with_ptr(entries.add(index).cast()), Access::Read) })
            .collect();
        let messages = match messages {
            Ok(messages) => messages,
            Err(error) => return crate::calls::fail(thread, error),
        };
        let mut headers: Vec<libc::mmsghdr> = messages.iter().map(|message| libc::mmsghdr { msg_hdr: message.header, msg_len: 0 }).collect();
        let result = unsafe { sys(thread, || libc::sendmmsg(fd, headers.as_mut_ptr(), count, flags)) };
        if result >= 0 {
            let entries = output::<UserMultiMessage>(user, size);
            for (index, header) in headers.iter().enumerate() {
                unsafe { (*entries.add(index)).length = header.msg_len };
            }
        }
        result
    }

    fn zsys_recvmmsg(thread: *mut Thread, fd: c_int, user: Ptr, count: c_uint, flags: c_int, timeout: Ptr) -> c_int {
        let timeout = maybe_out::<libc::timespec>(timeout);
        let size = array_size(size_of::<UserMultiMessage>(), count as usize);
        let entries = output::<UserMultiMessage>(user, size);
        let messages: Vec<Message> = (0..count as usize)
            .map(|index| unsafe { message(thread, user.with_ptr(entries.add(index).cast()), Access::Write) })
            .map(|message| message.unwrap_or_else(|_| unreachable!()))
            .collect();
        let mut headers: Vec<libc::mmsghdr> = messages.iter().map(|message| libc::mmsghdr { msg_hdr: message.header, msg_len: 0 }).collect();
        let result = unsafe { sys(thread, || libc::recvmmsg(fd, headers.as_mut_ptr(), count, flags as _, timeout)) };
        if result >= 0 {
            let entries = output::<UserMultiMessage>(user, size);
            for (index, header) in headers.iter().enumerate() {
                unsafe {
                    (*entries.add(index)).length = header.msg_len;
                    report_message(&header.msg_hdr, &raw mut (*entries.add(index)).message);
                }
            }
        }
        result
    }
}

const _: () = assert!(size_of::<UserMessage>() == 56 && size_of::<UserMultiMessage>() == 64);
const _: () = assert!(size_of::<UserControlHeader>() == size_of::<libc::cmsghdr>());
