//! `<stdio.h>`. A `FILE` is a 216-byte object in glibc's layout, which
//! glibc's headers read from when they inline `getc` and the like. Its buffer
//! pointers stay null, so those always call `__uflow` and `__overflow`, and
//! the libc writes its EOF and error bits into `_flags` for `feof_unlocked`.
//! Nothing here trusts what's in the object: a stream is found by the object's
//! identity. stdin, stdout and stderr are Rust's, so Fil-C output stays in
//! order with `println!`; other streams are the host's.
//!
//! Formatting and copying happen entered; I/O happens exited, under the
//! stream's own lock. An open stream lives until it's closed, as in C, and
//! closing one frees its object, so a later use traps.

use std::collections::HashMap;
use std::ffi::{CString, c_int, c_long};
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use crate::abi::*;
use crate::natives::{Rest, natives};
use crate::object::{self, Access};
use crate::glibc::set_errno;
use crate::sys::plain;
use crate::view::{self, Bytes};
use crate::{ptrs, thread};

use super::formatting;
use super::varargs::Arguments;

const FILE_SIZE: usize = 216;
const FILENO: usize = 112;
const IO_MAGIC: u32 = 0xfbad_0000;
const EOF_SEEN: u32 = 0x10;
const ERR_SEEN: u32 = 0x20;
const EOF: c_int = -1;

enum Backend {
    Stdin,
    Stdout,
    Stderr,
    /// A host `FILE *`.
    Host(usize),
}

struct State {
    backend: Backend,
    pushback: Vec<u8>,
    eof: bool,
    error: bool,
    /// For stdout under `_IONBF`: Rust's is line-buffered, so this flushes
    /// after every write.
    unbuffered: bool,
    fd: c_int,
}

impl State {
    fn host(&self) -> Option<*mut libc::FILE> {
        match self.backend {
            Backend::Host(file) => Some(file as *mut libc::FILE),
            _ => None,
        }
    }

    fn note_host(&mut self) {
        if let Some(file) = self.host() {
            unsafe {
                self.eof |= libc::feof(file) != 0;
                self.error |= libc::ferror(file) != 0;
            }
        }
    }

    fn read_byte(&mut self) -> Option<u8> {
        if let Some(byte) = self.pushback.pop() {
            return Some(byte);
        }
        match self.backend {
            Backend::Stdin => {
                let mut stdin = std::io::stdin().lock();
                match stdin.fill_buf() {
                    Ok([]) => {
                        self.eof = true;
                        None
                    }
                    Ok(buffer) => {
                        let byte = buffer[0];
                        stdin.consume(1);
                        Some(byte)
                    }
                    Err(_) => {
                        self.error = true;
                        None
                    }
                }
            }
            Backend::Host(file) => {
                let c = unsafe { libc::fgetc(file as *mut libc::FILE) };
                if c == EOF {
                    self.note_host();
                    None
                } else {
                    Some(c as u8)
                }
            }
            Backend::Stdout | Backend::Stderr => {
                self.error = true;
                unsafe { *libc::__errno_location() = libc::EBADF };
                None
            }
        }
    }

    /// Up to `limit` bytes, stopping after `delimiter`.
    fn read_until(&mut self, limit: usize, delimiter: Option<u8>) -> Vec<u8> {
        let mut bytes = Vec::new();
        while bytes.len() < limit {
            if matches!(self.backend, Backend::Stdin) && self.pushback.is_empty() {
                // A chunk at a time, under one lock.
                let mut stdin = std::io::stdin().lock();
                let buffer = match stdin.fill_buf() {
                    Ok(buffer) => buffer,
                    Err(_) => {
                        self.error = true;
                        break;
                    }
                };
                if buffer.is_empty() {
                    self.eof = true;
                    break;
                }
                let room = (limit - bytes.len()).min(buffer.len());
                let take = match delimiter.and_then(|delimiter| buffer[..room].iter().position(|&byte| byte == delimiter)) {
                    Some(at) => at + 1,
                    None => room,
                };
                bytes.extend_from_slice(&buffer[..take]);
                let done = delimiter.is_some_and(|delimiter| bytes.last() == Some(&delimiter));
                stdin.consume(take);
                if done {
                    break;
                }
                continue;
            }
            let Some(byte) = self.read_byte() else { break };
            bytes.push(byte);
            if Some(byte) == delimiter {
                break;
            }
        }
        bytes
    }

    fn write(&mut self, bytes: &[u8]) -> bool {
        let written = match self.backend {
            Backend::Stdout => {
                let mut stdout = std::io::stdout().lock();
                stdout.write_all(bytes).is_ok() && (!self.unbuffered || stdout.flush().is_ok())
            }
            Backend::Stderr => std::io::stderr().write_all(bytes).is_ok(),
            Backend::Host(file) => {
                bytes.is_empty() || unsafe { libc::fwrite(bytes.as_ptr().cast(), 1, bytes.len(), file as *mut libc::FILE) } == bytes.len()
            }
            Backend::Stdin => {
                unsafe { *libc::__errno_location() = libc::EBADF };
                false
            }
        };
        if !written {
            self.error = true;
        }
        written
    }

    fn flush(&mut self) -> bool {
        let flushed = match self.backend {
            Backend::Stdout => std::io::stdout().flush().is_ok(),
            Backend::Stderr => std::io::stderr().flush().is_ok(),
            Backend::Host(file) => unsafe { libc::fflush(file as *mut libc::FILE) == 0 },
            Backend::Stdin => true,
        };
        if !flushed {
            self.error = true;
        }
        flushed
    }

    fn seek(&mut self, offset: i64, whence: c_int) -> c_int {
        let result = match self.backend {
            Backend::Host(file) => unsafe { libc::fseeko(file as *mut libc::FILE, offset, whence) },
            _ => {
                self.flush();
                if unsafe { libc::lseek(self.fd, offset, whence) } < 0 { -1 } else { 0 }
            }
        };
        if result == 0 {
            self.pushback.clear();
            self.eof = false;
        }
        result
    }

    fn tell(&mut self) -> i64 {
        let position = match self.backend {
            Backend::Host(file) => unsafe { libc::ftello(file as *mut libc::FILE) },
            _ => unsafe { libc::lseek(self.fd, 0, libc::SEEK_CUR) },
        };
        if position < 0 { position } else { (position - self.pushback.len() as i64).max(0) }
    }

    fn close(&mut self) -> c_int {
        match self.backend {
            Backend::Host(file) => unsafe { libc::fclose(file as *mut libc::FILE) },
            Backend::Stdin => unsafe { libc::close(self.fd) },
            Backend::Stdout | Backend::Stderr => {
                let flushed = self.flush();
                if unsafe { libc::close(self.fd) } != 0 || !flushed { EOF } else { 0 }
            }
        }
    }
}

struct Stream {
    state: Mutex<State>,
}

type Streams = Option<HashMap<usize, Arc<Stream>>>;

/// Open streams by their object's lower bound. Roots; locked only entered,
/// never across an allocation.
static STREAMS: Mutex<Streams> = Mutex::new(None);

pub fn for_each_root(mut visit: impl FnMut(*mut Object)) {
    let streams = STREAMS.lock().unwrap_or_else(|e| e.into_inner());
    for &lower in streams.iter().flat_map(|streams| streams.keys()) {
        visit(object_for_lower(lower as *mut u8));
    }
}

/// The stream `file` is, which has to be the start of a `FILE` this libc
/// opened and hasn't closed.
fn stream(file: Ptr) -> Arc<Stream> {
    if file.ptr.is_null() {
        crate::failure::safety_panic(format_args!("cannot use a null FILE."));
    }
    object::check_access(file, FILE_SIZE, Access::Read);
    let streams = STREAMS.lock().unwrap_or_else(|e| e.into_inner());
    match streams.as_ref().and_then(|streams| streams.get(&(file.lower as usize))).filter(|_| file.ptr == file.lower) {
        Some(stream) => stream.clone(),
        None => crate::failure::safety_panic(format_args!("not a FILE this libc opened (ptr = {:p}).", file.ptr)),
    }
}

/// Writes the stream's EOF and error bits where glibc's headers look.
fn mirror(file: Ptr, eof: bool, error: bool) {
    let flags = IO_MAGIC | if eof { EOF_SEEN } else { 0 } | if error { ERR_SEEN } else { 0 };
    view::write(file, &flags.to_le_bytes());
}

/// Runs `operation` on `file`'s stream, exited and under its lock. An
/// `errno` the host sets comes back to Fil-C's.
unsafe fn with<R>(me: *mut Thread, file: Ptr, operation: impl FnOnce(&mut State) -> R) -> R {
    let stream = stream(file);
    let (result, eof, error, number) = unsafe {
        plain(me, || {
            let mut state = stream.state.lock().unwrap_or_else(|e| e.into_inner());
            *libc::__errno_location() = 0;
            let result = operation(&mut state);
            (result, state.eof, state.error, *libc::__errno_location())
        })
    };
    if number != 0 {
        unsafe { set_errno(me, number) };
    }
    mirror(file, eof, error);
    result
}

/// A new `FILE` for `backend`.
unsafe fn open(me: *mut Thread, backend: Backend, fd: c_int) -> Ptr {
    unsafe {
        let object = object::allocate(me, FILE_SIZE, 0);
        let file = Ptr::for_object(object);
        let stream = Arc::new(Stream { state: Mutex::new(State { backend, pushback: Vec::new(), eof: false, error: false, unbuffered: false, fd }) });
        STREAMS.lock().unwrap_or_else(|e| e.into_inner()).get_or_insert_with(HashMap::new).insert(file.lower as usize, stream);
        thread::track(me, object);
        mirror(file, false, false);
        view::write(file.with_ptr(file.ptr.add(FILENO)), &fd.to_le_bytes());
        file
    }
}

/// stdin, stdout and stderr, made once.
fn standard(me: *mut Thread) -> [Ptr; 3] {
    static MADE: Mutex<Option<[(usize, usize); 3]>> = Mutex::new(None);
    let mut made = unsafe { thread::lock_entered(me, &MADE) };
    let made = made.get_or_insert_with(|| unsafe {
        [(Backend::Stdin, 0), (Backend::Stdout, 1), (Backend::Stderr, 2)].map(|(backend, fd)| {
            let file = open(me, backend, fd);
            let holder = object::allocate(me, WORD_SIZE, 0);
            crate::globals::keep(holder);
            let holder = Ptr::for_object(holder);
            ptrs::store(me, holder, file);
            (holder.ptr as usize, holder.lower as usize)
        })
    });
    made.map(|(ptr, lower)| Ptr { ptr: ptr as *mut u8, lower: lower as *mut u8 })
}

fn standard_file(me: *mut Thread, index: usize) -> Ptr {
    unsafe { ptrs::load(standard(me)[index]) }
}

unsafe extern "C" fn stdin_storage(me: *mut Thread, _: *const Origin) -> Ptr {
    standard(me)[0]
}

unsafe extern "C" fn stdout_storage(me: *mut Thread, _: *const Origin) -> Ptr {
    standard(me)[1]
}

unsafe extern "C" fn stderr_storage(me: *mut Thread, _: *const Origin) -> Ptr {
    standard(me)[2]
}

// The `FILE`s themselves, which is where glibc's `_IO_2_1_stdout_` and the
// like keep theirs.
unsafe extern "C" fn stdin_object(me: *mut Thread, _: *const Origin) -> Ptr {
    standard_file(me, 0)
}

unsafe extern "C" fn stdout_object(me: *mut Thread, _: *const Origin) -> Ptr {
    standard_file(me, 1)
}

unsafe extern "C" fn stderr_object(me: *mut Thread, _: *const Origin) -> Ptr {
    standard_file(me, 2)
}

/// Flushes every stream, before the process exits.
pub(crate) unsafe fn flush_all(me: *mut Thread) {
    let streams: Vec<Arc<Stream>> = STREAMS.lock().unwrap_or_else(|e| e.into_inner()).iter().flat_map(|streams| streams.values().cloned()).collect();
    unsafe {
        plain(me, || {
            for stream in streams {
                stream.state.lock().unwrap_or_else(|e| e.into_inner()).flush();
            }
            drop(std::io::stdout().flush());
        })
    };
}

fn c_text(ptr: Ptr) -> CString {
    unsafe { CString::from_vec_unchecked(view::c_string(ptr)) }
}

/// A host stream as a new `FILE`, or null with `errno` from the host.
unsafe fn adopt_host(me: *mut Thread, file: *mut libc::FILE, error: c_int) -> Ptr {
    if file.is_null() {
        unsafe { set_errno(me, error) };
        return Ptr::NULL;
    }
    unsafe { open(me, Backend::Host(file as usize), libc::fileno(file)) }
}

/// Runs a host call that opens a stream, exited, with its `errno`.
unsafe fn host_open(me: *mut Thread, call: impl FnOnce() -> *mut libc::FILE) -> Ptr {
    let (file, error) = unsafe {
        plain(me, || {
            *libc::__errno_location() = 0;
            let file = call();
            (file, *libc::__errno_location())
        })
    };
    unsafe { adopt_host(me, file, error) }
}

unsafe fn close(me: *mut Thread, file: Ptr) -> c_int {
    let stream = stream(file);
    STREAMS.lock().unwrap_or_else(|e| e.into_inner()).as_mut().unwrap().remove(&(file.lower as usize));
    let result = unsafe { plain(me, || stream.state.lock().unwrap_or_else(|e| e.into_inner()).close()) };
    unsafe { object::free(file) };
    result
}

unsafe fn put(me: *mut Thread, file: Ptr, bytes: Vec<u8>) -> bool {
    unsafe { with(me, file, |state| state.write(&bytes)) }
}

unsafe fn get(me: *mut Thread, file: Ptr) -> c_int {
    unsafe { with(me, file, |state| state.read_byte()) }.map_or(EOF, c_int::from)
}

unsafe fn put_char(me: *mut Thread, file: Ptr, c: c_int) -> c_int {
    if unsafe { put(me, file, vec![c as u8]) } { c as u8 as c_int } else { EOF }
}

/// What the printf family writes to a stream.
unsafe fn print(me: *mut Thread, file: Ptr, format: Ptr, arguments: Arguments) -> c_int {
    let text = unsafe { formatting::format(me, format, arguments) };
    let count = text.len();
    if unsafe { put(me, file, text) } { formatting::result(me, count) } else { -1 }
}

/// `snprintf`'s: as much as fits in `size` with a terminator.
unsafe fn print_into(me: *mut Thread, buffer: Ptr, size: usize, format: Ptr, arguments: Arguments) -> c_int {
    let mut text = unsafe { formatting::format(me, format, arguments) };
    let count = text.len();
    if size > 0 {
        text.truncate(size - 1);
        text.push(0);
        view::write(buffer, &text);
    }
    formatting::result(me, count)
}

/// `sprintf`'s, with no size: past the buffer's end, it fails as a store
/// there would.
unsafe fn print_unbounded(me: *mut Thread, buffer: Ptr, format: Ptr, arguments: Arguments) -> c_int {
    let mut text = unsafe { formatting::format(me, format, arguments) };
    let count = text.len();
    text.push(0);
    view::write(buffer, &text);
    formatting::result(me, count)
}

unsafe fn print_new(me: *mut Thread, place: Ptr, format: Ptr, arguments: Arguments) -> c_int {
    let text = unsafe { formatting::format(me, format, arguments) };
    let copy = unsafe { view::new_c_string(me, &text) };
    unsafe { ptrs::store(me, place, copy) };
    formatting::result(me, text.len())
}

unsafe fn print_fd(me: *mut Thread, fd: c_int, format: Ptr, arguments: Arguments) -> c_int {
    let text = unsafe { formatting::format(me, format, arguments) };
    let written = unsafe {
        plain(me, || {
            let mut written = 0;
            while written < text.len() {
                let result = libc::write(fd, text[written..].as_ptr().cast(), text.len() - written);
                if result <= 0 {
                    return Err(*libc::__errno_location());
                }
                written += result as usize;
            }
            Ok(written)
        })
    };
    match written {
        Ok(written) => formatting::result(me, written),
        Err(error) => {
            unsafe { set_errno(me, error) };
            -1
        }
    }
}

/// `getdelim`'s: the line goes into `*line`, grown as needed.
unsafe fn get_delimited(me: *mut Thread, line: Ptr, size: Ptr, delimiter: c_int, file: Ptr) -> i64 {
    object::check_word_access(line, Access::Write);
    object::check_access(size, WORD_SIZE, Access::Write);
    let bytes = unsafe { with(me, file, |state| state.read_until(usize::MAX, Some(delimiter as u8))) };
    if bytes.is_empty() {
        return -1;
    }
    unsafe {
        let mut buffer = ptrs::load(line);
        let capacity = u64::from_le_bytes(std::array::from_fn(|index| Bytes::exactly(size, 8, Access::Read).get(index))) as usize;
        if buffer.ptr.is_null() || capacity < bytes.len() + 1 || buffer.available() < bytes.len() + 1 {
            let wanted = (bytes.len() + 1).max(120);
            buffer = crate::natives::realloc(me, buffer, wanted, MIN_ALIGN);
            thread::track(me, buffer.object());
            ptrs::store(me, line, buffer);
            view::write(size, &(wanted as u64).to_le_bytes());
        }
        view::write(buffer, &[&bytes[..], &[0]].concat());
    }
    bytes.len() as i64
}

natives! {
    fn fopen(thread: *mut Thread, path: Ptr, mode: Ptr) -> Ptr {
        let (path, mode) = (c_text(path), c_text(mode));
        unsafe { host_open(thread, || libc::fopen(path.as_ptr(), mode.as_ptr())) }
    }

    fn fdopen(thread: *mut Thread, fd: c_int, mode: Ptr) -> Ptr {
        let mode = c_text(mode);
        unsafe { host_open(thread, || libc::fdopen(fd, mode.as_ptr())) }
    }

    fn tmpfile(thread: *mut Thread) -> Ptr {
        unsafe { host_open(thread, || libc::tmpfile()) }
    }

    fn popen(thread: *mut Thread, command: Ptr, mode: Ptr) -> Ptr {
        let (command, mode) = (c_text(command), c_text(mode));
        unsafe { host_open(thread, || libc::popen(command.as_ptr(), mode.as_ptr())) }
    }

    fn pclose(thread: *mut Thread, file: Ptr) -> c_int {
        let stream = stream(file);
        STREAMS.lock().unwrap_or_else(|e| e.into_inner()).as_mut().unwrap().remove(&(file.lower as usize));
        let result = unsafe {
            plain(thread, || match stream.state.lock().unwrap_or_else(|e| e.into_inner()).host() {
                Some(host) => libc::pclose(host),
                None => -1,
            })
        };
        unsafe { object::free(file) };
        result
    }

    fn freopen(thread: *mut Thread, path: Ptr, mode: Ptr, file: Ptr) -> Ptr {
        let path = (!path.ptr.is_null()).then(|| c_text(path));
        let mode = c_text(mode);
        let reopened = unsafe {
            with(thread, file, |state| {
                state.pushback.clear();
                state.eof = false;
                state.error = false;
                match state.host() {
                    Some(host) => {
                        let path = path.as_ref().map_or(std::ptr::null(), |path| path.as_ptr());
                        !libc::freopen(path, mode.as_ptr(), host).is_null()
                    }
                    None => {
                        // A standard stream: what's at its descriptor changes.
                        state.flush();
                        let Some(path) = path.as_ref() else { return true };
                        let flags = match mode.as_bytes().first() {
                            Some(b'r') if !mode.as_bytes().contains(&b'+') => libc::O_RDONLY,
                            Some(b'r') => libc::O_RDWR,
                            Some(b'w') => libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC,
                            _ => libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
                        };
                        let fd = libc::open(path.as_ptr(), flags, 0o666);
                        if fd < 0 {
                            return false;
                        }
                        let moved = libc::dup2(fd, state.fd) >= 0;
                        libc::close(fd);
                        moved
                    }
                }
            })
        };
        if reopened { file } else { Ptr::NULL }
    }

    fn fclose(thread: *mut Thread, file: Ptr) -> c_int {
        unsafe { close(thread, file) }
    }

    fn fflush(thread: *mut Thread, file: Ptr) -> c_int {
        if file.ptr.is_null() {
            unsafe { flush_all(thread) };
            return 0;
        }
        if unsafe { with(thread, file, |state| state.flush()) } { 0 } else { EOF }
    }

    fn fread(thread: *mut Thread, buffer: Ptr, size: usize, count: usize, file: Ptr) -> usize {
        let Some(total) = size.checked_mul(count).filter(|&total| total > 0) else { return 0 };
        object::check_access(buffer, total, Access::Write);
        let bytes = unsafe { with(thread, file, |state| state.read_until(total, None)) };
        view::write(buffer, &bytes);
        bytes.len() / size
    }

    fn fwrite(thread: *mut Thread, buffer: Ptr, size: usize, count: usize, file: Ptr) -> usize {
        let Some(total) = size.checked_mul(count).filter(|&total| total > 0) else { return 0 };
        let bytes = Bytes::exactly(buffer, total, Access::Read).to_vec(total);
        if unsafe { put(thread, file, bytes) } { count } else { 0 }
    }

    fn fgetc(thread: *mut Thread, file: Ptr) -> c_int {
        unsafe { get(thread, file) }
    }

    fn getchar(thread: *mut Thread) -> c_int {
        let file = standard_file(thread, 0);
        unsafe { get(thread, file) }
    }

    fn fgets(thread: *mut Thread, buffer: Ptr, size: c_int, file: Ptr) -> Ptr {
        if size <= 0 {
            unsafe { set_errno(thread, libc::EINVAL) };
            return Ptr::NULL;
        }
        object::check_access(buffer, size as usize, Access::Write);
        let bytes = unsafe { with(thread, file, |state| state.read_until(size as usize - 1, Some(b'\n'))) };
        if bytes.is_empty() && size > 1 {
            return Ptr::NULL;
        }
        view::write(buffer, &[&bytes[..], &[0]].concat());
        buffer
    }

    fn fputc(thread: *mut Thread, c: c_int, file: Ptr) -> c_int {
        unsafe { put_char(thread, file, c) }
    }

    fn putchar(thread: *mut Thread, c: c_int) -> c_int {
        let file = standard_file(thread, 1);
        unsafe { put_char(thread, file, c) }
    }

    // With the buffer always empty, the next character, as `fgetc`.
    // Wide characters, in the C locale's encoding.
    fn fputwc(thread: *mut Thread, c: u32, file: Ptr) -> u32 {
        let Some(bytes) = crate::glibc::wide::encode(c as libc::wchar_t) else {
            unsafe { set_errno(thread, libc::EILSEQ) };
            return u32::MAX;
        };
        if unsafe { put(thread, file, bytes) } { c } else { u32::MAX }
    }

    fn putwchar(thread: *mut Thread, c: u32) -> u32 {
        let file = standard_file(thread, 1);
        match crate::glibc::wide::encode(c as libc::wchar_t) {
            Some(bytes) if unsafe { put(thread, file, bytes.clone()) } => c,
            _ => u32::MAX,
        }
    }

    fn fputws(thread: *mut Thread, string: Ptr, file: Ptr) -> c_int {
        let units = {
            let bytes = Bytes::readable(string);
            let mut units = Vec::new();
            let mut index = 0;
            loop {
                if index + 4 > bytes.len() {
                    crate::failure::safety_panic(format_args!("wide string is not null-terminated (ptr = {:p}).", string.ptr));
                }
                let unit = i32::from_le_bytes(std::array::from_fn(|offset| bytes.get(index + offset)));
                if unit == 0 {
                    break units;
                }
                units.push(unit);
                index += 4;
            }
        };
        let mut out = Vec::new();
        for unit in units {
            match crate::glibc::wide::encode(unit) {
                Some(bytes) => out.extend(bytes),
                None => {
                    unsafe { set_errno(thread, libc::EILSEQ) };
                    return -1;
                }
            }
        }
        if unsafe { put(thread, file, out) } { 1 } else { -1 }
    }

    fn fgetwc(thread: *mut Thread, file: Ptr) -> u32 {
        let c = unsafe { get(thread, file) };
        if c == EOF {
            return u32::MAX;
        }
        match crate::glibc::wide::decode(c as u8) {
            Some(unit) => unit as u32,
            None => {
                unsafe { set_errno(thread, libc::EILSEQ) };
                u32::MAX
            }
        }
    }

    fn getwchar(thread: *mut Thread) -> u32 {
        let file = standard_file(thread, 0);
        let c = unsafe { get(thread, file) };
        if c == EOF { u32::MAX } else { crate::glibc::wide::decode(c as u8).map_or(u32::MAX, |unit| unit as u32) }
    }

    fn ungetwc(thread: *mut Thread, c: u32, file: Ptr) -> u32 {
        if c == u32::MAX {
            return u32::MAX;
        }
        let Some(bytes) = crate::glibc::wide::encode(c as libc::wchar_t) else { return u32::MAX };
        unsafe {
            with(thread, file, |state| {
                state.pushback.extend(bytes.iter().rev());
                state.eof = false;
            })
        };
        c
    }

    // Streams have no orientation here: either kind of call works.
    fn fwide(thread: *mut Thread, file: Ptr, mode: c_int) -> c_int {
        let _ = stream(file);
        mode.signum()
    }

    fn __uflow(thread: *mut Thread, file: Ptr) -> c_int {
        unsafe { get(thread, file) }
    }

    fn __overflow(thread: *mut Thread, file: Ptr, c: c_int) -> c_int {
        if c == EOF {
            return if unsafe { with(thread, file, |state| state.flush()) } { 0 } else { EOF };
        }
        unsafe { put_char(thread, file, c) }
    }

    fn fputs(thread: *mut Thread, string: Ptr, file: Ptr) -> c_int {
        let bytes = view::c_string(string);
        if unsafe { put(thread, file, bytes) } { 1 } else { EOF }
    }

    fn puts(thread: *mut Thread, string: Ptr) -> c_int {
        let mut bytes = view::c_string(string);
        bytes.push(b'\n');
        let count = bytes.len();
        let file = standard_file(thread, 1);
        if unsafe { put(thread, file, bytes) } { count.min(c_int::MAX as usize) as c_int } else { EOF }
    }

    fn ungetc(thread: *mut Thread, c: c_int, file: Ptr) -> c_int {
        if c == EOF {
            return EOF;
        }
        unsafe {
            with(thread, file, |state| {
                state.pushback.push(c as u8);
                state.eof = false;
            })
        };
        c as u8 as c_int
    }

    fn feof(thread: *mut Thread, file: Ptr) -> c_int {
        unsafe { with(thread, file, |state| state.eof as c_int) }
    }

    fn ferror(thread: *mut Thread, file: Ptr) -> c_int {
        unsafe { with(thread, file, |state| state.error as c_int) }
    }

    fn clearerr(thread: *mut Thread, file: Ptr) -> () {
        unsafe {
            with(thread, file, |state| {
                state.eof = false;
                state.error = false;
                if let Some(host) = state.host() {
                    libc::clearerr(host);
                }
            })
        };
    }

    fn fileno(thread: *mut Thread, file: Ptr) -> c_int {
        unsafe { with(thread, file, |state| state.fd) }
    }

    fn fseek(thread: *mut Thread, file: Ptr, offset: c_long, whence: c_int) -> c_int {
        unsafe { with(thread, file, |state| state.seek(offset, whence)) }
    }

    fn ftell(thread: *mut Thread, file: Ptr) -> c_long {
        unsafe { with(thread, file, |state| state.tell()) }
    }

    fn rewind(thread: *mut Thread, file: Ptr) -> () {
        unsafe {
            with(thread, file, |state| {
                state.seek(0, libc::SEEK_SET);
                state.error = false;
            })
        };
    }

    // glibc's `fpos_t`: the offset, then a conversion state that's all zero
    // in the C locale.
    fn fgetpos(thread: *mut Thread, file: Ptr, position: Ptr) -> c_int {
        let offset = unsafe { with(thread, file, |state| state.tell()) };
        if offset < 0 {
            return -1;
        }
        view::write(position, &[&offset.to_le_bytes()[..], &[0; 8]].concat());
        0
    }

    fn fsetpos(thread: *mut Thread, file: Ptr, position: Ptr) -> c_int {
        let bytes = Bytes::exactly(position, 8, Access::Read);
        let offset = i64::from_le_bytes(std::array::from_fn(|index| bytes.get(index)));
        unsafe { with(thread, file, |state| state.seek(offset, libc::SEEK_SET)) }
    }

    // The program's buffer isn't used: the host's or Rust's buffers are.
    fn setvbuf(thread: *mut Thread, file: Ptr, buffer: Ptr, mode: c_int, size: usize) -> c_int {
        // glibc leaves `errno` alone for a bad mode.
        if !matches!(mode, libc::_IOFBF | libc::_IOLBF | libc::_IONBF) {
            return EOF;
        }
        unsafe {
            with(thread, file, |state| {
                state.unbuffered = mode == libc::_IONBF;
                match state.host() {
                    Some(host) => libc::setvbuf(host, std::ptr::null_mut(), mode, size),
                    None => 0,
                }
            })
        }
    }

    fn setbuf(thread: *mut Thread, file: Ptr, buffer: Ptr) -> () {
        let mode = if buffer.ptr.is_null() { libc::_IONBF } else { libc::_IOFBF };
        unsafe {
            with(thread, file, |state| {
                state.unbuffered = mode == libc::_IONBF;
                if let Some(host) = state.host() {
                    libc::setvbuf(host, std::ptr::null_mut(), mode, libc::BUFSIZ as usize);
                }
            })
        };
    }

    fn setlinebuf(thread: *mut Thread, file: Ptr) -> () {
        unsafe {
            with(thread, file, |state| {
                state.unbuffered = false;
                if let Some(host) = state.host() {
                    libc::setvbuf(host, std::ptr::null_mut(), libc::_IOLBF, 0);
                }
            })
        };
    }

    // Every call takes the stream's lock for itself.
    fn flockfile(thread: *mut Thread, file: Ptr) -> () {
        let _ = stream(file);
    }

    fn funlockfile(thread: *mut Thread, file: Ptr) -> () {
        let _ = stream(file);
    }

    fn ftrylockfile(thread: *mut Thread, file: Ptr) -> c_int {
        let _ = stream(file);
        0
    }

    fn getline(thread: *mut Thread, line: Ptr, size: Ptr, file: Ptr) -> i64 {
        unsafe { get_delimited(thread, line, size, b'\n' as c_int, file) }
    }

    fn getdelim(thread: *mut Thread, line: Ptr, size: Ptr, delimiter: c_int, file: Ptr) -> i64 {
        unsafe { get_delimited(thread, line, size, delimiter, file) }
    }

    // Every stream's lock is held across the fork, so the child gets none
    // halfway through a write.
    fn fork(thread: *mut Thread) -> c_int {
        unsafe { crate::glibc::pthread::before_fork(thread) };
        let mut streams: Vec<(usize, Arc<Stream>)> =
            STREAMS.lock().unwrap_or_else(|e| e.into_inner()).iter().flat_map(|streams| streams.iter().map(|(&lower, stream)| (lower, stream.clone()))).collect();
        streams.sort_by_key(|&(lower, _)| lower);
        let held: Vec<_> = unsafe { plain(thread, || streams.iter().map(|(_, stream)| stream.state.lock().unwrap_or_else(|e| e.into_inner())).collect()) };
        let (result, error) = unsafe { crate::gc::fork(thread) };
        drop(held);
        if result < 0 {
            unsafe { set_errno(thread, error) };
        } else {
            unsafe { crate::glibc::pthread::after_fork(thread, result == 0) };
        }
        result
    }

    fn remove(thread: *mut Thread, path: Ptr) -> c_int {
        let path = c_text(path);
        unsafe { crate::sys::sys(thread, || libc::remove(path.as_ptr())) }
    }

    fn printf(thread: *mut Thread, format: Ptr, rest: Rest) -> c_int {
        let file = standard_file(thread, 1);
        unsafe { print(thread, file, format, Arguments::Call(rest)) }
    }

    fn vprintf(thread: *mut Thread, format: Ptr, list: Ptr) -> c_int {
        let file = standard_file(thread, 1);
        unsafe { print(thread, file, format, Arguments::List(list)) }
    }

    fn fprintf(thread: *mut Thread, file: Ptr, format: Ptr, rest: Rest) -> c_int {
        unsafe { print(thread, file, format, Arguments::Call(rest)) }
    }

    fn vfprintf(thread: *mut Thread, file: Ptr, format: Ptr, list: Ptr) -> c_int {
        unsafe { print(thread, file, format, Arguments::List(list)) }
    }

    fn dprintf(thread: *mut Thread, fd: c_int, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_fd(thread, fd, format, Arguments::Call(rest)) }
    }

    fn vdprintf(thread: *mut Thread, fd: c_int, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_fd(thread, fd, format, Arguments::List(list)) }
    }

    fn sprintf(thread: *mut Thread, buffer: Ptr, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_unbounded(thread, buffer, format, Arguments::Call(rest)) }
    }

    fn vsprintf(thread: *mut Thread, buffer: Ptr, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_unbounded(thread, buffer, format, Arguments::List(list)) }
    }

    fn snprintf(thread: *mut Thread, buffer: Ptr, size: usize, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_into(thread, buffer, size, format, Arguments::Call(rest)) }
    }

    fn vsnprintf(thread: *mut Thread, buffer: Ptr, size: usize, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_into(thread, buffer, size, format, Arguments::List(list)) }
    }

    fn asprintf(thread: *mut Thread, place: Ptr, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_new(thread, place, format, Arguments::Call(rest)) }
    }

    fn vasprintf(thread: *mut Thread, place: Ptr, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_new(thread, place, format, Arguments::List(list)) }
    }

    // `_FORTIFY_SOURCE`'s, with a flag and the buffer's size as the compiler
    // knew it, which the checks here make moot.
    fn __printf_chk(thread: *mut Thread, flag: c_int, format: Ptr, rest: Rest) -> c_int {
        let file = standard_file(thread, 1);
        unsafe { print(thread, file, format, Arguments::Call(rest)) }
    }

    fn __vprintf_chk(thread: *mut Thread, flag: c_int, format: Ptr, list: Ptr) -> c_int {
        let file = standard_file(thread, 1);
        unsafe { print(thread, file, format, Arguments::List(list)) }
    }

    fn __fprintf_chk(thread: *mut Thread, file: Ptr, flag: c_int, format: Ptr, rest: Rest) -> c_int {
        unsafe { print(thread, file, format, Arguments::Call(rest)) }
    }

    fn __vfprintf_chk(thread: *mut Thread, file: Ptr, flag: c_int, format: Ptr, list: Ptr) -> c_int {
        unsafe { print(thread, file, format, Arguments::List(list)) }
    }

    fn __sprintf_chk(thread: *mut Thread, buffer: Ptr, flag: c_int, size: usize, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_unbounded(thread, buffer, format, Arguments::Call(rest)) }
    }

    fn __vsprintf_chk(thread: *mut Thread, buffer: Ptr, flag: c_int, size: usize, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_unbounded(thread, buffer, format, Arguments::List(list)) }
    }

    fn __snprintf_chk(thread: *mut Thread, buffer: Ptr, size: usize, flag: c_int, length: usize, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_into(thread, buffer, size, format, Arguments::Call(rest)) }
    }

    fn __vsnprintf_chk(thread: *mut Thread, buffer: Ptr, size: usize, flag: c_int, length: usize, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_into(thread, buffer, size, format, Arguments::List(list)) }
    }

    fn __dprintf_chk(thread: *mut Thread, fd: c_int, flag: c_int, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_fd(thread, fd, format, Arguments::Call(rest)) }
    }

    fn __vdprintf_chk(thread: *mut Thread, fd: c_int, flag: c_int, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_fd(thread, fd, format, Arguments::List(list)) }
    }

    fn __asprintf_chk(thread: *mut Thread, place: Ptr, flag: c_int, format: Ptr, rest: Rest) -> c_int {
        unsafe { print_new(thread, place, format, Arguments::Call(rest)) }
    }

    fn __vasprintf_chk(thread: *mut Thread, place: Ptr, flag: c_int, format: Ptr, list: Ptr) -> c_int {
        unsafe { print_new(thread, place, format, Arguments::List(list)) }
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated_stdin", stdin_storage as *const ()),
        ("pizlonated_stdout", stdout_storage as *const ()),
        ("pizlonated_stderr", stderr_storage as *const ()),
        ("pizlonated__IO_2_1_stdin_", stdin_object as *const ()),
        ("pizlonated__IO_2_1_stdout_", stdout_object as *const ()),
        ("pizlonated__IO_2_1_stderr_", stderr_object as *const ()),
        ("pizlonated___fork", fork::getter as *const ()),
        ("pizlonated___libc_fork", fork::getter as *const ()),
        ("pizlonated_vfork", fork::getter as *const ()),
        ("pizlonated___vfork", fork::getter as *const ()),
        ("pizlonated_fopen64", fopen::getter as *const ()),
        ("pizlonated_freopen64", freopen::getter as *const ()),
        ("pizlonated_tmpfile64", tmpfile::getter as *const ()),
        ("pizlonated_getc", fgetc::getter as *const ()),
        ("pizlonated__IO_getc", fgetc::getter as *const ()),
        ("pizlonated_getc_unlocked", fgetc::getter as *const ()),
        ("pizlonated_fgetc_unlocked", fgetc::getter as *const ()),
        ("pizlonated_getchar_unlocked", getchar::getter as *const ()),
        ("pizlonated_fgets_unlocked", fgets::getter as *const ()),
        ("pizlonated_putc", fputc::getter as *const ()),
        ("pizlonated_putwc", fputwc::getter as *const ()),
        ("pizlonated_putwc_unlocked", fputwc::getter as *const ()),
        ("pizlonated_fputwc_unlocked", fputwc::getter as *const ()),
        ("pizlonated_getwc", fgetwc::getter as *const ()),
        ("pizlonated_getwc_unlocked", fgetwc::getter as *const ()),
        ("pizlonated_fgetwc_unlocked", fgetwc::getter as *const ()),
        ("pizlonated_fputws_unlocked", fputws::getter as *const ()),
        ("pizlonated__IO_putc", fputc::getter as *const ()),
        ("pizlonated_putc_unlocked", fputc::getter as *const ()),
        ("pizlonated_fputc_unlocked", fputc::getter as *const ()),
        ("pizlonated_putchar_unlocked", putchar::getter as *const ()),
        ("pizlonated_fputs_unlocked", fputs::getter as *const ()),
        ("pizlonated_fwrite_unlocked", fwrite::getter as *const ()),
        ("pizlonated_fread_unlocked", fread::getter as *const ()),
        ("pizlonated_fflush_unlocked", fflush::getter as *const ()),
        ("pizlonated_feof_unlocked", feof::getter as *const ()),
        ("pizlonated_ferror_unlocked", ferror::getter as *const ()),
        ("pizlonated_clearerr_unlocked", clearerr::getter as *const ()),
        ("pizlonated_fileno_unlocked", fileno::getter as *const ()),
        ("pizlonated_fseeko", fseek::getter as *const ()),
        ("pizlonated_fseeko64", fseek::getter as *const ()),
        ("pizlonated_ftello", ftell::getter as *const ()),
        ("pizlonated_ftello64", ftell::getter as *const ()),
        ("pizlonated_fgetpos64", fgetpos::getter as *const ()),
        ("pizlonated_fsetpos64", fsetpos::getter as *const ()),
        ("pizlonated___getdelim", getdelim::getter as *const ()),
        ("pizlonated__IO_puts", puts::getter as *const ()),
        ("pizlonated__IO_flockfile", flockfile::getter as *const ()),
        ("pizlonated__IO_funlockfile", funlockfile::getter as *const ()),
        ("pizlonated__IO_ftrylockfile", ftrylockfile::getter as *const ()),
        ("pizlonated_setbuffer", setbuf::getter as *const ()),
    ]
}
