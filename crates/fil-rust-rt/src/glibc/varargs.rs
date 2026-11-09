//! Variadic arguments, from a call (`Rest`) or a `va_list`. Either way each
//! argument is one 8-byte slot, a pointer with its capability, and a
//! `long double` two aligned ones.

use crate::abi::*;
use crate::natives::Rest;
use crate::object::{self, Access};
use crate::ptrs;

/// Where the arguments come from.
pub enum Arguments {
    Call(Rest),
    /// A `va_list`'s storage, whose first word is a capability for the next
    /// argument.
    List(Ptr),
}

impl Arguments {
    /// The next slot of `size` bytes aligned to `alignment`, as a pointer with
    /// its capability for words that hold one.
    unsafe fn next_slot(&mut self, thread: *mut Thread, size: usize, alignment: usize) -> Ptr {
        match self {
            Arguments::Call(rest) => {
                if !rest.has_next() {
                    crate::failure::safety_panic(format_args!("not enough variadic arguments."));
                }
                unsafe { rest.next::<Ptr>(thread) }
            }
            Arguments::List(list) => unsafe {
                let value = ptrs::load(*list);
                let alignment = alignment.max(WORD_SIZE);
                let address = (value.ptr as usize).next_multiple_of(alignment);
                let at = value.with_ptr(address as *mut u8);
                object::check_access(at, size, Access::Read);
                object::check_word_access(*list, Access::Write);
                // Only the address moves on; the capability stays.
                *list.ptr.cast::<usize>() = address + size;
                if size == WORD_SIZE { ptrs::load(at) } else { at }
            },
        }
    }

    /// An integer argument, as its 8-byte slot holds it.
    pub unsafe fn word(&mut self, thread: *mut Thread) -> u64 {
        unsafe { self.next_slot(thread, WORD_SIZE, WORD_SIZE) }.ptr as u64
    }

    pub unsafe fn int(&mut self, thread: *mut Thread) -> i32 {
        unsafe { self.word(thread) as u32 as i32 }
    }

    pub unsafe fn pointer(&mut self, thread: *mut Thread) -> Ptr {
        unsafe { self.next_slot(thread, WORD_SIZE, WORD_SIZE) }
    }

    pub unsafe fn double(&mut self, thread: *mut Thread) -> f64 {
        f64::from_bits(unsafe { self.word(thread) })
    }

    /// A `long double`, as near as a `double` gets.
    pub unsafe fn long_double(&mut self, thread: *mut Thread) -> f64 {
        match self {
            // Passed in a call, it takes two slots at a 16-byte boundary.
            Arguments::Call(rest) => {
                rest.align(16);
                let low = unsafe { self.word(thread) };
                let high = unsafe { self.word(thread) };
                x87_to_f64(low, high as u16)
            }
            Arguments::List(_) => {
                let at = unsafe { self.next_slot(thread, 16, 16) };
                let bytes = crate::view::Bytes::exactly(at, 16, Access::Read);
                let word = |offset: usize, count: usize| (0..count).fold(0u64, |value, index| value | (bytes.get(offset + index) as u64) << (8 * index));
                x87_to_f64(word(0, 8), word(8, 2) as u16)
            }
        }
    }
}

/// An x87 80-bit extended value, as a `double`.
fn x87_to_f64(mantissa: u64, sign_exponent: u16) -> f64 {
    let negative = sign_exponent & 0x8000 != 0;
    let exponent = (sign_exponent & 0x7fff) as i32;
    let magnitude = if exponent == 0 && mantissa == 0 {
        0.0
    } else if exponent == 0x7fff {
        if mantissa << 1 == 0 { f64::INFINITY } else { f64::NAN }
    } else {
        (mantissa as f64) * 2f64.powi(exponent - 16383 - 63)
    };
    if negative { -magnitude } else { magnitude }
}
