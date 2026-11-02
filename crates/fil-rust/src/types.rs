//! How Rust types map onto Fil-C memory and the Fil-C calling convention.

use fil_rust_sys::RawPtr;

use crate::{Fil, Result};

/// The class of a scalar leaf inside a type's layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeafKind {
    /// An integer (or anything else passed in integer registers).
    Int,
    /// An `f32` or `f64`.
    Float,
    /// A pointer, which carries a capability.
    Ptr,
}

/// A scalar component of a type's layout, used to classify the type for the
/// calling convention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub offset: usize,
    pub size: usize,
    pub kind: LeafKind,
}

/// One 8-byte calling-convention slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Word {
    /// Raw bits (integers and floats).
    Bits(u64),
    /// A pointer together with its capability.
    Ptr(RawPtr),
}

/// A type with a defined representation in Fil-C memory.
///
/// `Self` is a *marker* describing the C type; `Value<'f>` is the Rust value
/// you read and write. They differ only for types containing pointers: a
/// `FilPtr<'static, T>` field has the value type `FilPtr<'f, T>`, tied to the
/// scope `'f` that keeps the pointee alive.
///
/// Implementations can't cause memory unsafety: every access goes through
/// capability-checked runtime operations. Use `#[derive(FilType)]` for C
/// structs rather than implementing this by hand.
pub trait FilType {
    /// The Rust value for this type within scope `'f`.
    type Value<'f>;
    /// `sizeof` of the C type.
    const SIZE: usize;
    /// `alignof` of the C type.
    const ALIGN: usize;
    /// Whether this is a struct or array rather than a scalar. Some
    /// architectures pass a one-member struct differently from the member.
    #[doc(hidden)]
    const AGGREGATE: bool = false;

    /// Reads a value from Fil-C memory at `at`.
    fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<Self::Value<'f>>;
    /// Writes a value to Fil-C memory at `at`.
    fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &Self::Value<'f>) -> Result<()>;
    /// Appends this type's scalar leaves (with `offset` added) to `out`.
    fn leaves(out: &mut Vec<Leaf>, offset: usize);

    /// Fast path for scalars: the value as a single calling-convention word.
    fn to_word(_value: &Self::Value<'_>) -> Option<Word> {
        None
    }
    /// Fast path for scalars: the value from a single calling-convention word.
    fn from_word<'f>(_fil: &Fil<'f>, _word: Word) -> Option<Self::Value<'f>> {
        None
    }
}

/// Returns `at` moved forward by `offset` bytes (same capability).
#[inline]
pub fn raw_add(at: RawPtr, offset: usize) -> RawPtr {
    RawPtr { ptr: at.ptr.wrapping_byte_add(offset), lower: at.lower }
}

macro_rules! int_type {
    ($($t:ty),*) => {$(
        impl FilType for $t {
            type Value<'f> = $t;
            const SIZE: usize = std::mem::size_of::<$t>();
            const ALIGN: usize = std::mem::align_of::<$t>();

            fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<$t> {
                let mut buf = [0u8; std::mem::size_of::<$t>()];
                fil.copy_out_raw(at, &mut buf)?;
                Ok(<$t>::from_ne_bytes(buf))
            }

            fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &$t) -> Result<()> {
                fil.copy_in_raw(at, &value.to_ne_bytes())
            }

            fn leaves(out: &mut Vec<Leaf>, offset: usize) {
                out.push(Leaf { offset, size: Self::SIZE, kind: LeafKind::Int });
            }

            fn to_word(value: &$t) -> Option<Word> {
                // Sign- or zero-extend like the C ABI; the callee reads only
                // the bytes of its declared type.
                Some(Word::Bits(*value as i128 as u64))
            }

            fn from_word<'f>(_fil: &Fil<'f>, word: Word) -> Option<$t> {
                match word {
                    Word::Bits(bits) => Some(bits as $t),
                    Word::Ptr(p) => Some(p.ptr as usize as $t),
                }
            }
        }
    )*};
}

int_type!(i8, u8, i16, u16, i32, u32, i64, u64, isize, usize);

impl FilType for bool {
    type Value<'f> = bool;
    const SIZE: usize = 1;
    const ALIGN: usize = 1;

    fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<bool> {
        Ok(u8::read(fil, at)? != 0)
    }

    fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &bool) -> Result<()> {
        u8::write(fil, at, &(*value as u8))
    }

    fn leaves(out: &mut Vec<Leaf>, offset: usize) {
        out.push(Leaf { offset, size: 1, kind: LeafKind::Int });
    }

    fn to_word(value: &bool) -> Option<Word> {
        Some(Word::Bits(*value as u64))
    }

    fn from_word<'f>(_fil: &Fil<'f>, word: Word) -> Option<bool> {
        match word {
            Word::Bits(bits) => Some(bits as u8 != 0),
            Word::Ptr(p) => Some(!p.ptr.is_null()),
        }
    }
}

macro_rules! float_type {
    ($t:ty, $bits:ty) => {
        impl FilType for $t {
            type Value<'f> = $t;
            const SIZE: usize = std::mem::size_of::<$t>();
            const ALIGN: usize = std::mem::align_of::<$t>();

            fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<$t> {
                Ok(<$t>::from_bits(<$bits>::read(fil, at)?))
            }

            fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &$t) -> Result<()> {
                <$bits>::write(fil, at, &value.to_bits())
            }

            fn leaves(out: &mut Vec<Leaf>, offset: usize) {
                out.push(Leaf { offset, size: Self::SIZE, kind: LeafKind::Float });
            }

            fn to_word(value: &$t) -> Option<Word> {
                Some(Word::Bits(value.to_bits() as u64))
            }

            fn from_word<'f>(_fil: &Fil<'f>, word: Word) -> Option<$t> {
                match word {
                    Word::Bits(bits) => Some(<$t>::from_bits(bits as $bits)),
                    Word::Ptr(_) => None,
                }
            }
        }
    };
}

float_type!(f32, u32);
float_type!(f64, u64);

/// `void`: only meaningful as a return type.
impl FilType for () {
    type Value<'f> = ();
    const SIZE: usize = 0;
    const ALIGN: usize = 1;

    fn read<'f>(_fil: &Fil<'f>, _at: RawPtr) -> Result<()> {
        Ok(())
    }

    fn write<'f>(_fil: &Fil<'f>, _at: RawPtr, _value: &()) -> Result<()> {
        Ok(())
    }

    fn leaves(_out: &mut Vec<Leaf>, _offset: usize) {}
}

impl<T: FilType, const N: usize> FilType for [T; N] {
    type Value<'f> = [T::Value<'f>; N];
    const SIZE: usize = T::SIZE * N;
    const ALIGN: usize = T::ALIGN;
    const AGGREGATE: bool = true;

    fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<Self::Value<'f>> {
        let mut items = Vec::with_capacity(N);
        for i in 0..N {
            items.push(T::read(fil, raw_add(at, i * T::SIZE))?);
        }
        match items.try_into() {
            Ok(array) => Ok(array),
            Err(_) => unreachable!("array has exactly N elements"),
        }
    }

    fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &Self::Value<'f>) -> Result<()> {
        for (i, item) in value.iter().enumerate() {
            T::write(fil, raw_add(at, i * T::SIZE), item)?;
        }
        Ok(())
    }

    fn leaves(out: &mut Vec<Leaf>, offset: usize) {
        for i in 0..N {
            T::leaves(out, offset + i * T::SIZE);
        }
    }
}

/// The C layout of a struct, computed at compile time by `#[derive(FilType)]`.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct CLayout<const N: usize> {
    pub offsets: [usize; N],
    pub size: usize,
    pub align: usize,
}

const fn align_up(value: usize, align: usize) -> usize {
    value.div_ceil(align) * align
}

/// Lays out fields given as `(size, align)` the way a C compiler does.
#[doc(hidden)]
pub const fn c_layout<const N: usize>(fields: [(usize, usize); N]) -> CLayout<N> {
    let mut offsets = [0usize; N];
    let mut end = 0usize;
    let mut align = 1usize;
    let mut i = 0;
    while i < N {
        let (size, field_align) = fields[i];
        let offset = align_up(end, field_align);
        offsets[i] = offset;
        end = offset + size;
        if field_align > align {
            align = field_align;
        }
        i += 1;
    }
    CLayout { offsets, size: align_up(end, align), align }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_layout_matches_c_rules() {
        // struct { char a; int b; char c; double d; short e; }
        let l = c_layout([(1, 1), (4, 4), (1, 1), (8, 8), (2, 2)]);
        assert_eq!(l.offsets, [0, 4, 8, 16, 24]);
        assert_eq!(l.size, 32);
        assert_eq!(l.align, 8);
        // struct { char a[3]; }
        let l = c_layout([(3, 1)]);
        assert_eq!(l.size, 3);
        // empty struct
        let l = c_layout::<0>([]);
        assert_eq!((l.size, l.align), (0, 1));
    }
}
