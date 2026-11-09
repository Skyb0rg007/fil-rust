//! The `sscanf` family: the input is a copy of the string, up to its
//! terminator, which has to be in the object; numbers go through the host's
//! parsers on copies bounded by the field width; every result is stored
//! through a checked pointer, so a `%s` too long for its buffer traps as the
//! store would.

use std::ffi::{CString, c_char, c_int};

use crate::abi::*;
use crate::natives::{Rest, f64_to_x87, natives};
use crate::view;

use super::varargs::Arguments;

#[derive(Clone, Copy, PartialEq)]
enum Length {
    None,
    Char,
    Short,
    Long,
    LongLong,
    LongDouble,
    Size,
}

/// Why scanning stopped early.
enum Stop {
    /// The input ran out: `EOF` if nothing was stored yet.
    Input,
    /// It didn't match.
    Matching,
}

struct Scanner<'a> {
    input: &'a [u8],
    at: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.input.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(|byte| byte.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    /// Up to `width` bytes from here, as a terminated copy for a host parser.
    fn field(&self, width: usize) -> CString {
        let end = self.input.len().min(self.at.saturating_add(width));
        CString::new(&self.input[self.at..end]).unwrap()
    }
}

/// A scanset, `%[...]`: which bytes it takes.
fn scanset(format: &[u8], at: &mut usize) -> [bool; 256] {
    let mut set = [false; 256];
    let negated = format.get(*at) == Some(&b'^');
    if negated {
        *at += 1;
    }
    let start = *at;
    while let Some(&byte) = format.get(*at) {
        if byte == b']' && *at > start {
            break;
        }
        if byte == b'-' && *at > start && format.get(*at + 1).is_some_and(|&next| next != b']') {
            let (low, high) = (format[*at - 1], format[*at + 1]);
            for value in low.min(high)..=low.max(high) {
                set[value as usize] = true;
            }
            *at += 2;
            continue;
        }
        set[byte as usize] = true;
        *at += 1;
    }
    *at += 1;
    if negated {
        for value in set.iter_mut() {
            *value = !*value;
        }
    }
    set
}

fn store_integer(target: Ptr, length: Length, value: u64) {
    let size = match length {
        Length::Char => 1,
        Length::Short => 2,
        Length::None => 4,
        _ => 8,
    };
    view::write(target, &value.to_le_bytes()[..size]);
}

fn store_float(target: Ptr, length: Length, value: f64) {
    match length {
        Length::Long => view::write(target, &value.to_le_bytes()),
        Length::LongDouble => {
            let (mantissa, sign_exponent) = f64_to_x87(value);
            let mut bytes = [0u8; 16];
            bytes[..8].copy_from_slice(&mantissa.to_le_bytes());
            bytes[8..10].copy_from_slice(&sign_exponent.to_le_bytes());
            view::write(target, &bytes);
        }
        _ => view::write(target, &(value as f32).to_le_bytes()),
    }
}

/// Scans `input` by `format`, storing through pointers from `arguments`.
/// Returns how many it stored, or `EOF` if the input ran out first.
pub unsafe fn scan(thread: *mut Thread, input: &[u8], format: &[u8], mut arguments: Arguments) -> c_int {
    let mut scanner = Scanner { input, at: 0 };
    let mut stored = 0;
    let mut converted_any = false;
    let mut at = 0;
    let result = (|| -> Result<(), Stop> {
        while at < format.len() {
            let byte = format[at];
            if byte.is_ascii_whitespace() {
                scanner.skip_space();
                at += 1;
                continue;
            }
            if byte != b'%' {
                if scanner.peek().is_none() {
                    return Err(Stop::Input);
                }
                if scanner.peek() != Some(byte) {
                    return Err(Stop::Matching);
                }
                scanner.at += 1;
                at += 1;
                continue;
            }
            at += 1;
            if format.get(at) == Some(&b'%') {
                at += 1;
                scanner.skip_space();
                match scanner.peek() {
                    None => return Err(Stop::Input),
                    Some(b'%') => scanner.at += 1,
                    Some(_) => return Err(Stop::Matching),
                }
                continue;
            }
            let suppress = format.get(at) == Some(&b'*');
            if suppress {
                at += 1;
            }
            let start = at;
            while format.get(at).is_some_and(|byte| byte.is_ascii_digit()) {
                at += 1;
            }
            let width = std::str::from_utf8(&format[start..at]).unwrap().parse::<usize>().ok().filter(|&width| width > 0);
            let mut length = Length::None;
            loop {
                length = match (format.get(at), length) {
                    (Some(b'h'), Length::Short) => Length::Char,
                    (Some(b'h'), _) => Length::Short,
                    (Some(b'l'), Length::Long) => Length::LongLong,
                    (Some(b'l'), _) => Length::Long,
                    (Some(b'q'), _) => Length::LongLong,
                    (Some(b'L'), _) => Length::LongDouble,
                    (Some(b'j' | b'z' | b't'), _) => Length::Size,
                    _ => break,
                };
                at += 1;
            }
            let Some(&conversion) = format.get(at) else { return Err(Stop::Matching) };
            at += 1;
            let mut target = || if suppress { Ptr::NULL } else { unsafe { arguments.pointer(thread) } };
            match conversion {
                b'n' => {
                    let target = target();
                    if !suppress {
                        store_integer(target, length, scanner.at as u64);
                    }
                }
                b'd' | b'i' | b'u' | b'o' | b'x' | b'X' | b'p' | b'b' => {
                    scanner.skip_space();
                    if scanner.peek().is_none() {
                        return Err(Stop::Input);
                    }
                    let base = match conversion {
                        b'd' | b'u' => 10,
                        b'i' => 0,
                        b'o' => 8,
                        b'b' => 2,
                        _ => 16,
                    };
                    let field = scanner.field(width.unwrap_or(usize::MAX));
                    let mut end = std::ptr::null_mut::<c_char>();
                    let value = unsafe {
                        if conversion == b'd' || conversion == b'i' {
                            libc::strtoll(field.as_ptr(), &mut end, base) as u64
                        } else {
                            libc::strtoull(field.as_ptr(), &mut end, base)
                        }
                    };
                    let consumed = end as usize - field.as_ptr() as usize;
                    if consumed == 0 {
                        return Err(Stop::Matching);
                    }
                    scanner.at += consumed;
                    converted_any = true;
                    let target = target();
                    if !suppress {
                        if conversion == b'p' {
                            unsafe { crate::ptrs::store(thread, target, Ptr { ptr: value as *mut u8, lower: std::ptr::null_mut() }) };
                        } else {
                            store_integer(target, length, value);
                        }
                        stored += 1;
                    }
                }
                b'f' | b'F' | b'e' | b'E' | b'g' | b'G' | b'a' | b'A' => {
                    scanner.skip_space();
                    if scanner.peek().is_none() {
                        return Err(Stop::Input);
                    }
                    let field = scanner.field(width.unwrap_or(usize::MAX));
                    let mut end = std::ptr::null_mut::<c_char>();
                    let value = unsafe { libc::strtod(field.as_ptr(), &mut end) };
                    let consumed = end as usize - field.as_ptr() as usize;
                    if consumed == 0 {
                        return Err(Stop::Matching);
                    }
                    scanner.at += consumed;
                    converted_any = true;
                    let target = target();
                    if !suppress {
                        store_float(target, length, value);
                        stored += 1;
                    }
                }
                b's' | b'c' | b'[' => {
                    let set = (conversion == b'[').then(|| scanset(format, &mut at));
                    if conversion == b's' {
                        scanner.skip_space();
                    }
                    if scanner.peek().is_none() {
                        return Err(Stop::Input);
                    }
                    let width = width.unwrap_or(if conversion == b'c' { 1 } else { usize::MAX });
                    let start = scanner.at;
                    while scanner.at - start < width
                        && let Some(byte) = scanner.peek()
                    {
                        let takes = match &set {
                            Some(set) => set[byte as usize],
                            None => conversion == b'c' || !byte.is_ascii_whitespace(),
                        };
                        if !takes {
                            break;
                        }
                        scanner.at += 1;
                    }
                    let taken = &input[start..scanner.at];
                    if taken.is_empty() || conversion == b'c' && taken.len() < width {
                        return Err(if scanner.peek().is_none() { Stop::Input } else { Stop::Matching });
                    }
                    converted_any = true;
                    let target = target();
                    if !suppress {
                        let mut bytes = taken.to_vec();
                        if length == Length::Long {
                            // Into a wide string, a character for each byte.
                            let mut units: Vec<u8> = bytes.iter().flat_map(|&byte| (byte as u32).to_le_bytes()).collect();
                            if conversion != b'c' {
                                units.extend_from_slice(&[0; 4]);
                            }
                            view::write(target, &units);
                        } else {
                            if conversion != b'c' {
                                bytes.push(0);
                            }
                            view::write(target, &bytes);
                        }
                        stored += 1;
                    }
                }
                _ => return Err(Stop::Matching),
            }
        }
        Ok(())
    })();
    match result {
        Err(Stop::Input) if stored == 0 && !converted_any => libc::EOF,
        _ => stored,
    }
}

unsafe fn scan_string(thread: *mut Thread, input: Ptr, format: Ptr, arguments: Arguments) -> c_int {
    let (input, format) = (view::c_string(input), view::c_string(format));
    unsafe { scan(thread, &input, &format, arguments) }
}

natives! {
    fn sscanf(thread: *mut Thread, input: Ptr, format: Ptr, rest: Rest) -> c_int {
        unsafe { scan_string(thread, input, format, Arguments::Call(rest)) }
    }

    fn vsscanf(thread: *mut Thread, input: Ptr, format: Ptr, list: Ptr) -> c_int {
        unsafe { scan_string(thread, input, format, Arguments::List(list)) }
    }
}

pub fn aliases() -> Vec<(&'static str, *const ())> {
    vec![
        ("pizlonated___isoc99_sscanf", sscanf::getter as *const ()),
        ("pizlonated___isoc23_sscanf", sscanf::getter as *const ()),
        ("pizlonated__IO_sscanf", sscanf::getter as *const ()),
        ("pizlonated___isoc99_vsscanf", vsscanf::getter as *const ()),
        ("pizlonated___isoc23_vsscanf", vsscanf::getter as *const ()),
        ("pizlonated__IO_vsscanf", vsscanf::getter as *const ()),
    ]
}
