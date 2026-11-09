//! The printf family's formatting. The format is parsed here and each
//! conversion formatted by the host's `snprintf` on its own, from a copy:
//! nothing of Fil-C's memory reaches the host. `%s` needs its terminator
//! inside the object unless a precision bounds it, and then reads no further
//! than that or the object's end; `%n` writes through a checked pointer.

use std::ffi::{CString, c_char, c_int};

use crate::abi::*;
use crate::view::{self, Bytes};

use super::varargs::Arguments;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Length {
    None,
    Char,
    Short,
    Long,
    LongLong,
    Max,
    Size,
    Diff,
    LongDouble,
}

/// What an argument is read as.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Int,
    Word,
    Double,
    LongDouble,
    Pointer,
}

/// A value read for a conversion.
#[derive(Clone, Copy)]
enum Value {
    Word(u64),
    Double(f64),
    Pointer(Ptr),
    /// A terminated copy of a wide string, for `%ls`.
    Wide(*const libc::wchar_t),
}

#[derive(Clone, Copy)]
enum Count {
    None,
    Given(usize),
    /// `*`, or `*n$`.
    Argument(Option<usize>),
}

struct Spec {
    flags: Vec<u8>,
    width: Count,
    precision: Count,
    length: Length,
    conversion: u8,
    position: Option<usize>,
}

enum Piece {
    Text(Vec<u8>),
    Conversion(Spec),
}

fn number(format: &[u8], at: &mut usize) -> Option<usize> {
    let start = *at;
    while *at < format.len() && format[*at].is_ascii_digit() {
        *at += 1;
    }
    (*at > start).then(|| std::str::from_utf8(&format[start..*at]).unwrap().parse().unwrap_or(usize::MAX))
}

/// `n$` at `at`, if it's there.
fn position(format: &[u8], at: &mut usize) -> Option<usize> {
    let mut look = *at;
    let value = number(format, &mut look)?;
    if format.get(look) == Some(&b'$') {
        *at = look + 1;
        Some(value)
    } else {
        None
    }
}

fn count(format: &[u8], at: &mut usize) -> Count {
    if format.get(*at) == Some(&b'*') {
        *at += 1;
        return Count::Argument(position(format, at));
    }
    number(format, at).map_or(Count::None, Count::Given)
}

fn parse(format: &[u8]) -> Vec<Piece> {
    let mut pieces = Vec::new();
    let mut text = Vec::new();
    let mut at = 0;
    while at < format.len() {
        if format[at] != b'%' {
            text.push(format[at]);
            at += 1;
            continue;
        }
        at += 1;
        if format.get(at) == Some(&b'%') {
            text.push(b'%');
            at += 1;
            continue;
        }
        let position = position(format, &mut at);
        let mut flags = Vec::new();
        while let Some(&flag) = format.get(at).filter(|flag| b"-+ #0'I".contains(flag)) {
            flags.push(flag);
            at += 1;
        }
        let width = count(format, &mut at);
        let precision = if format.get(at) == Some(&b'.') {
            at += 1;
            match count(format, &mut at) {
                Count::None => Count::Given(0),
                given => given,
            }
        } else {
            Count::None
        };
        let mut length = Length::None;
        loop {
            length = match (format.get(at), length) {
                (Some(b'h'), Length::Short) => Length::Char,
                (Some(b'h'), _) => Length::Short,
                (Some(b'l'), Length::Long) => Length::LongLong,
                (Some(b'l'), _) => Length::Long,
                (Some(b'q'), _) => Length::LongLong,
                (Some(b'L'), _) => Length::LongDouble,
                (Some(b'j'), _) => Length::Max,
                (Some(b'z' | b'Z'), _) => Length::Size,
                (Some(b't'), _) => Length::Diff,
                _ => break,
            };
            at += 1;
        }
        let Some(&conversion) = format.get(at) else {
            // A lone `%` at the end, which glibc prints.
            text.push(b'%');
            break;
        };
        at += 1;
        if !text.is_empty() {
            pieces.push(Piece::Text(std::mem::take(&mut text)));
        }
        pieces.push(Piece::Conversion(Spec { flags, width, precision, length, conversion, position }));
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    pieces
}

fn kind(spec: &Spec) -> Option<Kind> {
    Some(match spec.conversion {
        b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'c' | b'b' | b'B' => match spec.length {
            Length::None | Length::Char | Length::Short => Kind::Int,
            _ => Kind::Word,
        },
        b'C' => Kind::Int,
        b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
            if spec.length == Length::LongDouble { Kind::LongDouble } else { Kind::Double }
        }
        b's' | b'S' | b'p' | b'n' => Kind::Pointer,
        _ => return None,
    })
}

unsafe fn read(arguments: &mut Arguments, thread: *mut Thread, kind: Kind) -> Value {
    unsafe {
        match kind {
            Kind::Int => Value::Word(arguments.int(thread) as i64 as u64),
            Kind::Word => Value::Word(arguments.word(thread)),
            Kind::Double => Value::Double(arguments.double(thread)),
            Kind::LongDouble => Value::Double(arguments.long_double(thread)),
            Kind::Pointer => Value::Pointer(arguments.pointer(thread)),
        }
    }
}

/// Every argument the format takes, in order. With positions (`%2$d`), they're
/// read in position order, once their kinds are known.
unsafe fn gather(pieces: &[Piece], arguments: &mut Arguments, thread: *mut Thread) -> Vec<Value> {
    let positional = pieces.iter().any(|piece| matches!(piece, Piece::Conversion(spec) if spec.position.is_some()));
    if !positional {
        let mut values = Vec::new();
        for piece in pieces {
            let Piece::Conversion(spec) = piece else { continue };
            for counted in [spec.width, spec.precision] {
                if let Count::Argument(_) = counted {
                    values.push(unsafe { read(arguments, thread, Kind::Int) });
                }
            }
            if let Some(kind) = kind(spec) {
                values.push(unsafe { read(arguments, thread, kind) });
            }
        }
        return values;
    }
    let mut kinds: Vec<Option<Kind>> = Vec::new();
    let mut note = |index: usize, kind: Kind| {
        if index == 0 || index > 9000 {
            crate::failure::safety_panic(format_args!("printf: bad argument position {index}."));
        }
        if kinds.len() < index {
            kinds.resize(index, None);
        }
        kinds[index - 1] = Some(kind);
    };
    for piece in pieces {
        let Piece::Conversion(spec) = piece else { continue };
        for counted in [spec.width, spec.precision] {
            if let Count::Argument(Some(index)) = counted {
                note(index, Kind::Int);
            }
        }
        if let (Some(index), Some(kind)) = (spec.position, kind(spec)) {
            note(index, kind);
        }
    }
    kinds
        .into_iter()
        .map(|kind| match kind {
            Some(kind) => unsafe { read(arguments, thread, kind) },
            None => crate::failure::safety_panic(format_args!("printf: an argument position is skipped.")),
        })
        .collect()
}

/// Formats one conversion of `spec`'s with the host's `snprintf`.
fn host_format(spec: &Spec, width: Option<i32>, precision: Option<i32>, value: Value) -> Vec<u8> {
    let mut format = b"%".to_vec();
    format.extend(spec.flags.iter().filter(|&&flag| flag != b'I'));
    if width.is_some() {
        format.push(b'*');
    }
    if precision.is_some() {
        format.extend_from_slice(b".*");
    }
    let length: &[u8] = match spec.length {
        Length::Char => b"hh",
        Length::Short => b"h",
        Length::Long => b"l",
        Length::LongLong => b"ll",
        Length::Max => b"j",
        Length::Size => b"z",
        Length::Diff => b"t",
        Length::None | Length::LongDouble => b"",
    };
    let (length, conversion) = match spec.conversion {
        b'C' => (&b"l"[..], b'c'),
        b'S' => (&b"l"[..], b's'),
        b'c' | b's' if spec.length == Length::Long => (&b"l"[..], spec.conversion),
        b'c' | b's' | b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => (&b""[..], spec.conversion),
        conversion => (length, conversion),
    };
    format.extend_from_slice(length);
    format.push(conversion);
    let format = CString::new(format).unwrap();
    let mut buffer = vec![0u8; 512];
    loop {
        let size = buffer.len();
        let out = buffer.as_mut_ptr().cast::<c_char>();
        let (w, p) = (width.unwrap_or(0), precision.unwrap_or(0));
        let written = unsafe {
            match (value, width.is_some(), precision.is_some()) {
                (Value::Word(v), false, false) => libc::snprintf(out, size, format.as_ptr(), v),
                (Value::Word(v), true, false) => libc::snprintf(out, size, format.as_ptr(), w, v),
                (Value::Word(v), false, true) => libc::snprintf(out, size, format.as_ptr(), p, v),
                (Value::Word(v), true, true) => libc::snprintf(out, size, format.as_ptr(), w, p, v),
                (Value::Double(v), false, false) => libc::snprintf(out, size, format.as_ptr(), v),
                (Value::Double(v), true, false) => libc::snprintf(out, size, format.as_ptr(), w, v),
                (Value::Double(v), false, true) => libc::snprintf(out, size, format.as_ptr(), p, v),
                (Value::Double(v), true, true) => libc::snprintf(out, size, format.as_ptr(), w, p, v),
                (Value::Wide(v), false, false) => libc::snprintf(out, size, format.as_ptr(), v),
                (Value::Wide(v), true, false) => libc::snprintf(out, size, format.as_ptr(), w, v),
                (Value::Wide(v), false, true) => libc::snprintf(out, size, format.as_ptr(), p, v),
                (Value::Wide(v), true, true) => libc::snprintf(out, size, format.as_ptr(), w, p, v),
                (Value::Pointer(_), ..) => unreachable!(),
            }
        };
        if written < 0 {
            return Vec::new();
        }
        if (written as usize) < size {
            buffer.truncate(written as usize);
            return buffer;
        }
        buffer.resize(written as usize + 1, 0);
    }
}

/// `bytes` padded to `width`, on the left unless `left`.
fn pad(bytes: &[u8], width: Option<i32>, left: bool) -> Vec<u8> {
    let width = width.unwrap_or(0).unsigned_abs() as usize;
    if bytes.len() >= width {
        return bytes.to_vec();
    }
    let fill = vec![b' '; width - bytes.len()];
    if left { [bytes, &fill].concat() } else { [&fill[..], bytes].concat() }
}

/// What `%n` writes, of the size its length says.
fn store_count(target: Ptr, length: Length, written: usize) {
    let size = match length {
        Length::Char => 1,
        Length::Short => 2,
        Length::None => 4,
        _ => 8,
    };
    view::write(target, &(written as u64).to_le_bytes()[..size]);
}

/// `spec`, left-justified if `left`.
fn with_left(spec: &Spec, left: bool) -> Spec {
    let mut flags = spec.flags.clone();
    if left && !flags.contains(&b'-') {
        flags.push(b'-');
    }
    Spec { flags, width: spec.width, precision: spec.precision, length: spec.length, conversion: spec.conversion, position: None }
}

/// A terminated copy of the wide string at `string`. Each character makes at
/// least one byte, so a precision bounds how many are read; without one, the
/// terminator has to be in the object.
fn wide_string(string: Ptr, precision: Option<i32>) -> Vec<libc::wchar_t> {
    const UNIT: usize = size_of::<libc::wchar_t>();
    let limit = precision.map_or(usize::MAX, |precision| (precision as usize).saturating_mul(UNIT));
    let available = Bytes::readable_up_to(string, limit);
    let mut wide = Vec::new();
    let mut index = 0;
    loop {
        if index + UNIT > available.len() {
            if precision.is_none() {
                crate::failure::safety_panic(format_args!("wide string is not null-terminated (ptr = {:p}).", string.ptr));
            }
            break;
        }
        let unit = libc::wchar_t::from_le_bytes(std::array::from_fn(|offset| available.get(index + offset)));
        if unit == 0 {
            break;
        }
        wide.push(unit);
        index += UNIT;
    }
    wide.push(0);
    wide
}

/// Formats the string at `format` with `arguments`.
pub unsafe fn format(thread: *mut Thread, format: Ptr, arguments: Arguments) -> Vec<u8> {
    unsafe { format_text(thread, &view::c_string(format), arguments) }
}

/// Formats `format` with `arguments`.
pub unsafe fn format_text(thread: *mut Thread, format: &[u8], mut arguments: Arguments) -> Vec<u8> {
    let pieces = parse(format);
    let values = unsafe { gather(&pieces, &mut arguments, thread) };
    let mut out = Vec::new();
    let mut next = 0usize;
    let mut take = |position: Option<usize>| -> Value {
        match position {
            Some(index) => values[index - 1],
            None => {
                next += 1;
                values[next - 1]
            }
        }
    };
    for piece in &pieces {
        let spec = match piece {
            Piece::Text(text) => {
                out.extend_from_slice(text);
                continue;
            }
            Piece::Conversion(spec) => spec,
        };
        let as_int = |value: Value| match value {
            Value::Word(word) => word as u32 as i32,
            _ => 0,
        };
        let mut width = match spec.width {
            Count::None => None,
            Count::Given(width) => Some(width.min(i32::MAX as usize) as i32),
            Count::Argument(position) => Some(as_int(take(position))),
        };
        let mut left = spec.flags.contains(&b'-');
        if let Some(negative) = width.filter(|&width| width < 0) {
            width = Some(negative.saturating_neg());
            left = true;
        }
        let precision = match spec.precision {
            Count::None => None,
            Count::Given(precision) => Some(precision.min(i32::MAX as usize) as i32),
            Count::Argument(position) => Some(as_int(take(position))).filter(|&precision| precision >= 0),
        };
        match spec.conversion {
            b's' | b'S' if spec.conversion == b'S' || spec.length == Length::Long => {
                let Value::Pointer(string) = take(spec.position) else { unreachable!() };
                if string.ptr.is_null() {
                    let null = if precision.is_none_or(|precision| precision >= 6) { &b"(null)"[..] } else { b"" };
                    out.extend(pad(null, width, left));
                    continue;
                }
                let wide = wide_string(string, precision);
                out.extend(host_format(&with_left(spec, left), width, precision, Value::Wide(wide.as_ptr())));
            }
            b's' | b'S' => {
                let Value::Pointer(string) = take(spec.position) else { unreachable!() };
                let bytes = if string.ptr.is_null() {
                    // glibc's.
                    if precision.is_none_or(|precision| precision >= 6) { b"(null)".to_vec() } else { Vec::new() }
                } else if let Some(precision) = precision {
                    let available = Bytes::readable_up_to(string, precision as usize);
                    let length = available.find(0).unwrap_or(available.len());
                    available.to_vec(length)
                } else {
                    view::c_string(string)
                };
                out.extend(pad(&bytes, width, left));
            }
            b'p' => {
                let Value::Pointer(pointer) = take(spec.position) else { unreachable!() };
                let text = if pointer.ptr.is_null() { b"(nil)".to_vec() } else { format!("{:#x}", pointer.ptr as usize).into_bytes() };
                out.extend(pad(&text, width, left));
            }
            b'n' => {
                let Value::Pointer(target) = take(spec.position) else { unreachable!() };
                store_count(target, spec.length, out.len());
            }
            b'm' => {
                let number = unsafe { *crate::glibc::errno_location(thread) };
                let text = unsafe { std::ffi::CStr::from_ptr(libc::strerror(number)) }.to_bytes().to_vec();
                out.extend(pad(&text, width, left));
            }
            _ if kind(spec).is_some() => {
                let value = take(spec.position);
                out.extend(host_format(&with_left(spec, left), width, precision, value));
            }
            // `%%` with flags or a width between: glibc prints just the `%`.
            b'%' => out.push(b'%'),
            other => {
                // Not a conversion: glibc prints it as it was.
                out.push(b'%');
                out.push(other);
            }
        }
    }
    out
}

/// The `int` the printf family returns for `count` bytes, or -1 with
/// `EOVERFLOW` past `INT_MAX`.
pub fn result(thread: *mut Thread, count: usize) -> c_int {
    match c_int::try_from(count) {
        Ok(count) => count,
        Err(_) => {
            unsafe { crate::start::set_errno(thread, libc::EOVERFLOW) };
            -1
        }
    }
}
