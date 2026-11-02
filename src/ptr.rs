//! Capability-carrying pointers into Fil-C memory.

use std::ffi::{CString, c_char};
use std::fmt;
use std::marker::PhantomData;

use fil_rust_sys::{PtrInfo, RawPtr, flags, special};

use crate::call::{FilFn, FilSig};
use crate::types::{FilType, Leaf, LeafKind, Word, raw_add};
use crate::{Error, Fil, Result};

/// Marker for pointers whose pointee type is unknown (`void*`).
pub enum Opaque {}

/// A pointer into Fil-C memory, valid within the scope `'f`.
///
/// A `FilPtr` is an address plus the capability that grants access through
/// it, exactly like a pointer inside Fil-C. Safe code can't forge one: you get
/// them from allocations, from Fil-C calls, from loading pointer fields, or as
/// capability-less values from [`FilPtr::null`] and [`FilPtr::from_addr`].
///
/// Every access is checked against the live capability, so a `FilPtr` whose
/// object has since been freed (or that Fil-C code handed back out of bounds)
/// produces an [`Error`], never undefined behavior.
///
/// `T` is a marker for the pointee's C type (see [`FilType`]); it only
/// affects typed reads, writes and element-wise arithmetic.
///
/// The pointee is kept alive by the scope `'f` (a native frame of the Fil-C
/// thread you are running on). To keep a pointer longer, root it with
/// [`Fil::root`].
pub struct FilPtr<'f, T: ?Sized = Opaque> {
    raw: RawPtr,
    _marker: PhantomData<(&'f (), *const T)>,
}

/// A pointer type for use in signatures and struct fields, where no scope is
/// in play: `Function<fn(Ptr<c_char>) -> usize>`. The values you pass and get
/// back are [`FilPtr<'f, T>`].
pub type Ptr<T = Opaque> = FilPtr<'static, T>;

impl<T: ?Sized> Clone for FilPtr<'_, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: ?Sized> Copy for FilPtr<'_, T> {}

impl<T: ?Sized> PartialEq for FilPtr<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}

impl<T: ?Sized> Eq for FilPtr<'_, T> {}

impl<T: ?Sized> fmt::Debug for FilPtr<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FilPtr({:p}, lower={:p})", self.raw.ptr, self.raw.lower)
    }
}

/// What a pointer's capability currently allows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    /// Lowest address the capability allows.
    pub lower: usize,
    /// One past the highest address the capability allows.
    pub upper: usize,
    pub readonly: bool,
    pub freed: bool,
    pub global: bool,
    /// The object is a function (it can be called but not read).
    pub function: bool,
    /// The object is some other special runtime object.
    pub special: bool,
}

impl<'f, T: ?Sized> FilPtr<'f, T> {
    pub(crate) fn from_raw(raw: RawPtr) -> Self {
        FilPtr { raw, _marker: PhantomData }
    }

    pub(crate) fn raw(self) -> RawPtr {
        self.raw
    }

    /// The null pointer.
    pub fn null() -> Self {
        Self::from_raw(RawPtr::NULL)
    }

    /// A pointer with the given address and no capability: it can be passed
    /// to Fil-C (as C code casting an integer to a pointer would), but never
    /// dereferenced.
    pub fn from_addr(addr: usize) -> Self {
        Self::from_raw(RawPtr::from_addr(addr))
    }

    /// The pointer's address.
    pub fn addr(self) -> usize {
        self.raw.ptr as usize
    }

    /// Whether the address is zero.
    pub fn is_null(self) -> bool {
        self.raw.ptr.is_null()
    }

    /// Whether the pointer carries a capability at all.
    pub fn has_capability(self) -> bool {
        !self.raw.lower.is_null()
    }

    /// Reinterprets the pointee type. Like a C cast, this can't make an
    /// access succeed that the capability doesn't allow.
    pub fn cast<U: ?Sized>(self) -> FilPtr<'f, U> {
        FilPtr::from_raw(self.raw)
    }

    /// Moves the address by `bytes` (wrapping). The capability is unchanged,
    /// so moving out of bounds is allowed but accessing there is not.
    pub fn byte_offset(self, bytes: isize) -> Self {
        Self::from_raw(RawPtr { ptr: self.raw.ptr.wrapping_byte_offset(bytes), lower: self.raw.lower })
    }

    /// Moves the address forward by `bytes`.
    pub fn byte_add(self, bytes: usize) -> Self {
        Self::from_raw(raw_add(self.raw, bytes))
    }

    /// Reports the capability's bounds and flags (`None` without a capability).
    pub fn capability(self) -> Option<Capability> {
        let mut info = PtrInfo::default();
        let shim = crate::runtime::shim()?;
        let status = unsafe { (shim.fr_ptr_info_get)(self.raw, &mut info) };
        if status != 0 {
            return None;
        }
        Some(Capability {
            lower: info.lower as usize,
            upper: info.upper as usize,
            readonly: info.flags & flags::READONLY != 0,
            freed: info.flags & flags::FREE != 0,
            global: info.flags & flags::GLOBAL != 0,
            function: info.special_type == special::FUNCTION,
            special: info.special_type != special::NONE,
        })
    }

    /// Bytes accessible from this address to the end of the object (0 if the
    /// pointer has no capability, is out of bounds, or the object was freed).
    pub fn remaining(self) -> usize {
        match self.capability() {
            Some(c) if !c.freed && !c.special && self.addr() >= c.lower => {
                c.upper.saturating_sub(self.addr())
            }
            _ => 0,
        }
    }

    /// Checks that `len` bytes starting here could be read (or, with `write`,
    /// written) right now.
    pub fn check(self, len: usize, write: bool) -> Result<()> {
        let shim = crate::runtime::shim().ok_or(Error::NullCapability)?;
        crate::error::check(unsafe { (shim.fr_check)(self.raw, len, write as i32) })
    }

    /// Copies bytes out of Fil-C memory into `buf`.
    pub fn read_bytes(self, fil: &Fil<'_>, buf: &mut [u8]) -> Result<()> {
        fil.copy_out_raw(self.raw, buf)
    }

    /// Copies `len` bytes out of Fil-C memory.
    pub fn read_vec(self, fil: &Fil<'_>, len: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        fil.copy_out_raw(self.raw, &mut buf)?;
        Ok(buf)
    }

    /// Copies `bytes` into Fil-C memory. Bytes written over a pointer leave
    /// its capability in place, just as in C under Fil-C.
    pub fn write_bytes(self, fil: &Fil<'_>, bytes: &[u8]) -> Result<()> {
        fil.copy_in_raw(self.raw, bytes)
    }

    /// Fills `len` bytes with `value`.
    pub fn fill(self, fil: &Fil<'_>, value: u8, len: usize) -> Result<()> {
        self.check(len, true)?;
        let chunk = [value; 4096];
        let mut done = 0;
        while done < len {
            let n = (len - done).min(chunk.len());
            fil.copy_in_raw(raw_add(self.raw, done), &chunk[..n])?;
            done += n;
        }
        Ok(())
    }

    /// Copies `len` bytes from `src` to here (overlap allowed). Pointers in
    /// the copied range keep their capabilities, like `memmove` in Fil-C.
    pub fn copy_from<U: ?Sized>(self, fil: &Fil<'_>, src: FilPtr<'_, U>, len: usize) -> Result<()> {
        fil.memmove_raw(self.raw, src.raw, len)
    }

    /// Reads the NUL-terminated string starting here (without the NUL). The
    /// terminator must lie within the object.
    pub fn read_cstring(self, fil: &Fil<'_>) -> Result<CString> {
        let len = fil.strlen_raw(self.raw)?;
        let bytes = self.read_vec(fil, len)?;
        Ok(CString::new(bytes).expect("strlen stops at the first NUL"))
    }

    /// Reads the NUL-terminated UTF-8 string starting here.
    pub fn read_string(self, fil: &Fil<'_>) -> Result<String> {
        let c = self.read_cstring(fil)?;
        c.into_string().map_err(|_| Error::Utf8)
    }

    /// Views this pointer as a function with signature `Sig`. Fails unless
    /// it points to a callable Fil-C function.
    pub fn as_fn<Sig: FilSig>(self) -> Result<FilFn<'f, Sig>> {
        FilFn::from_ptr(self.cast())
    }
}

impl<'f, T: FilType + ?Sized> FilPtr<'f, T> {
    /// Moves the address forward by `count` elements of `T`.
    // Named after `<*const T>::add`, not `std::ops::Add`.
    #[allow(clippy::should_implement_trait)]
    pub fn add(self, count: usize) -> Self {
        self.byte_add(count * T::SIZE)
    }

    /// Reads the `T` this pointer points to.
    pub fn read(self, fil: &Fil<'f>) -> Result<T::Value<'f>> {
        T::read(fil, self.raw)
    }

    /// Writes a `T` where this pointer points.
    pub fn write(self, fil: &Fil<'f>, value: &T::Value<'f>) -> Result<()> {
        T::write(fil, self.raw, value)
    }

    /// Reads element `index` of the array starting here.
    pub fn read_at(self, fil: &Fil<'f>, index: usize) -> Result<T::Value<'f>> {
        self.add(index).read(fil)
    }

    /// Writes element `index` of the array starting here.
    pub fn write_at(self, fil: &Fil<'f>, index: usize, value: &T::Value<'f>) -> Result<()> {
        self.add(index).write(fil, value)
    }

    /// Reads `count` consecutive elements.
    pub fn read_array(self, fil: &Fil<'f>, count: usize) -> Result<Vec<T::Value<'f>>> {
        self.check(count.checked_mul(T::SIZE).ok_or(Error::OutOfBounds)?, false)?;
        (0..count).map(|i| self.read_at(fil, i)).collect()
    }

    /// Writes consecutive elements starting here.
    pub fn write_array(self, fil: &Fil<'f>, values: &[T::Value<'f>]) -> Result<()> {
        self.check(values.len().checked_mul(T::SIZE).ok_or(Error::OutOfBounds)?, true)?;
        for (i, value) in values.iter().enumerate() {
            self.write_at(fil, i, value)?;
        }
        Ok(())
    }

    /// Number of whole `T` elements accessible from here.
    pub fn len(self) -> usize {
        self.remaining().checked_div(T::SIZE).unwrap_or(0)
    }

    /// Whether no whole `T` is accessible from here.
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

impl<'f> FilPtr<'f, c_char> {
    /// Reads the NUL-terminated string this pointer points to.
    pub fn to_string_lossy(self, fil: &Fil<'f>) -> Result<String> {
        Ok(self.read_cstring(fil)?.to_string_lossy().into_owned())
    }
}

impl<'x, T: ?Sized> FilType for FilPtr<'x, T> {
    type Value<'f> = FilPtr<'f, T>;
    const SIZE: usize = 8;
    const ALIGN: usize = 8;

    fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<FilPtr<'f, T>> {
        Ok(FilPtr::from_raw(fil.load_ptr_raw(at)?))
    }

    fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &FilPtr<'f, T>) -> Result<()> {
        fil.store_ptr_raw(at, value.raw)
    }

    fn leaves(out: &mut Vec<Leaf>, offset: usize) {
        out.push(Leaf { offset, size: 8, kind: LeafKind::Ptr });
    }

    fn to_word(value: &FilPtr<'_, T>) -> Option<Word> {
        Some(Word::Ptr(value.raw))
    }

    fn from_word<'f>(_fil: &Fil<'f>, word: Word) -> Option<FilPtr<'f, T>> {
        Some(match word {
            Word::Ptr(raw) => FilPtr::from_raw(raw),
            Word::Bits(bits) => FilPtr::from_addr(bits as usize),
        })
    }
}
