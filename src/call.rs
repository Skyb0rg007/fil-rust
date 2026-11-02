//! Calling Fil-C functions, and the calling-convention marshalling shared
//! with callbacks.
//!
//! Fil-C's generic calling convention passes arguments (and return values) in
//! 8-byte slots of a per-thread buffer, with a parallel buffer holding the
//! capability of each pointer slot. The callee checks every slot it reads, so
//! a mismatched signature can only produce a Fil-C panic, never corruption.
//!
//! Aggregates follow clang's x86-64 SysV lowering, which Fil-C keeps: an
//! aggregate of at most 16 bytes travels as one slot per eightbyte (a lone
//! pointer stays a pointer); larger ones are passed by reference (`byval`) and
//! returned through a hidden first argument (`sret`).

use std::marker::PhantomData;

use fil_rust_sys::RawPtr;

use crate::ptr::{FilPtr, Opaque};
use crate::types::{FilType, Leaf, LeafKind, Word, raw_add};
use crate::{Error, Fil, Result, error};

/// How a type travels through the calling convention.
#[derive(Clone, Debug, PartialEq, Eq)]
#[doc(hidden)]
pub enum Class {
    /// `void`.
    Void,
    /// One slot per eightbyte.
    Words(Vec<LeafKind>),
    /// By reference (`byval` argument / `sret` return).
    Memory,
}

#[doc(hidden)]
pub fn classify<T: FilType + ?Sized>() -> Result<Class> {
    if T::SIZE == 0 {
        return Ok(Class::Void);
    }
    let mut leaves: Vec<Leaf> = Vec::new();
    T::leaves(&mut leaves, 0);
    if leaves.iter().any(|l| l.size > 8 || l.size == 0) {
        return Err(Error::UnsupportedAbi("scalars wider than 8 bytes".into()));
    }
    // Packed (misaligned) members force memory class.
    if T::SIZE > 16 || leaves.iter().any(|l| l.offset % l.size != 0) {
        return Ok(Class::Memory);
    }
    Ok(Class::Words(eightbyte_kinds::<T>(&leaves)))
}

/// The kind of each eightbyte of `T`: a pointer if a pointer occupies it,
/// else a float if it holds only floats, else an integer.
fn eightbyte_kinds<T: FilType + ?Sized>(leaves: &[Leaf]) -> Vec<LeafKind> {
    (0..T::SIZE.div_ceil(8))
        .map(|w| {
            let in_word: Vec<&Leaf> = leaves.iter().filter(|l| l.offset / 8 == w).collect();
            if in_word.iter().any(|l| l.kind == LeafKind::Ptr) {
                LeafKind::Ptr
            } else if !in_word.is_empty() && in_word.iter().all(|l| l.kind == LeafKind::Float) {
                LeafKind::Float
            } else {
                LeafKind::Int
            }
        })
        .collect()
}

/// How an argument of type `T` starting at slot `at` occupies the argument
/// buffer: `pad` unused slots, then one slot per kind.
///
/// Memory-class arguments (C's `byval`) are copied into the buffer inline,
/// aligned to their alignment (at least 8).
fn arg_layout<T: FilType + ?Sized>(class: &Class, at: usize) -> (usize, Vec<LeafKind>) {
    match class {
        Class::Void => (0, Vec::new()),
        Class::Words(kinds) => (0, kinds.clone()),
        Class::Memory => {
            let align = T::ALIGN.max(8);
            let offset = at * 8;
            let pad = (offset.div_ceil(align) * align - offset) / 8;
            let mut leaves = Vec::new();
            T::leaves(&mut leaves, 0);
            (pad, eightbyte_kinds::<T>(&leaves))
        }
    }
}

fn round8(n: usize) -> usize {
    n.div_ceil(8) * 8
}

/// Encodes an argument (or a register-class return value) as
/// calling-convention words appended to `out`.
fn encode_value<'f, T: FilType + ?Sized>(
    fil: &Fil<'f>,
    value: &T::Value<'f>,
    out: &mut Vec<Word>,
) -> Result<()> {
    if let Some(word) = T::to_word(value) {
        out.push(word);
        return Ok(());
    }
    let class = classify::<T>()?;
    if class == Class::Void {
        return Err(Error::UnsupportedAbi("void can't be passed as an argument".into()));
    }
    let (pad, kinds) = arg_layout::<T>(&class, out.len());
    out.extend(std::iter::repeat_n(Word::Bits(0), pad));
    let tmp = fil.alloc_raw(round8(T::SIZE), T::ALIGN.max(16))?;
    T::write(fil, tmp, value)?;
    for (i, kind) in kinds.iter().enumerate() {
        let at = raw_add(tmp, i * 8);
        out.push(match kind {
            LeafKind::Ptr => Word::Ptr(fil.load_ptr_raw(at)?),
            _ => {
                let mut buf = [0u8; 8];
                fil.copy_out_raw(at, &mut buf)?;
                Word::Bits(u64::from_ne_bytes(buf))
            }
        });
    }
    Ok(())
}

/// Decodes the argument of type `T` starting at `words[*at]`, advancing `at`.
fn decode_arg<'f, T: FilType + ?Sized>(
    fil: &Fil<'f>,
    class: &Class,
    words: &[Word],
    at: &mut usize,
) -> Result<T::Value<'f>> {
    let (pad, kinds) = arg_layout::<T>(class, *at);
    let start = *at + pad;
    *at = start + kinds.len();
    let taken = &words[start..*at];
    if let [word] = taken
        && let Some(value) = T::from_word(fil, *word)
    {
        return Ok(value);
    }
    from_words::<T>(fil, &kinds, taken)
}

/// Decodes a register-class return value.
fn decode_value<'f, T: FilType + ?Sized>(
    fil: &Fil<'f>,
    class: &Class,
    words: &[Word],
) -> Result<T::Value<'f>> {
    match class {
        Class::Void => T::read(fil, RawPtr::NULL),
        Class::Words(kinds) => {
            if let [word] = words
                && let Some(value) = T::from_word(fil, *word)
            {
                return Ok(value);
            }
            from_words::<T>(fil, kinds, words)
        }
        Class::Memory => unreachable!("memory-class values are returned through sret"),
    }
}

fn from_words<'f, T: FilType + ?Sized>(
    fil: &Fil<'f>,
    kinds: &[LeafKind],
    words: &[Word],
) -> Result<T::Value<'f>> {
    let tmp = fil.alloc_raw(round8(T::SIZE), T::ALIGN.max(16))?;
    for (i, (kind, word)) in kinds.iter().zip(words).enumerate() {
        let at = raw_add(tmp, i * 8);
        match (kind, word) {
            (LeafKind::Ptr, Word::Ptr(p)) => fil.store_ptr_raw(at, *p)?,
            (_, Word::Bits(bits)) => fil.copy_in_raw(at, &bits.to_ne_bytes())?,
            (_, Word::Ptr(p)) => fil.copy_in_raw(at, &(p.ptr as u64).to_ne_bytes())?,
        }
    }
    T::read(fil, tmp)
}

/// Writes `words` into the cc buffers and calls `function`. Returns the size
/// of the return values.
fn invoke(fil: &Fil<'_>, function: RawPtr, words: &[Word]) -> Result<usize> {
    let shim = fil.shim();
    let size = words.len() * 8;
    unsafe { (shim.fr_cc_begin)(fil.thread, size) };
    for (i, word) in words.iter().enumerate() {
        let status = match *word {
            Word::Bits(bits) => unsafe { (shim.fr_cc_set_word)(fil.thread, size, i * 8, bits) },
            Word::Ptr(p) => unsafe { (shim.fr_cc_set_ptr)(fil.thread, size, i * 8, p) },
        };
        error::check(status)?;
    }
    let mut return_size = 0usize;
    error::check(unsafe { (shim.fr_call)(fil.thread, function, size, &mut return_size) })?;
    Ok(return_size)
}

/// Copies `kinds.len()` words out of the cc buffer of `size` bytes.
fn read_words(fil: &Fil<'_>, size: usize, kinds: &[Option<LeafKind>]) -> Result<Vec<Word>> {
    let shim = fil.shim();
    kinds
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            if *kind == Some(LeafKind::Ptr) {
                let mut p = RawPtr::NULL;
                error::check(unsafe { (shim.fr_cc_get_ptr)(fil.thread, size, i * 8, &mut p) })?;
                Ok(Word::Ptr(p))
            } else {
                let mut bits = 0u64;
                error::check(unsafe { (shim.fr_cc_get_word)(fil.thread, size, i * 8, &mut bits) })?;
                Ok(Word::Bits(bits))
            }
        })
        .collect()
}

fn word_kinds(class: &Class) -> Vec<Option<LeafKind>> {
    match class {
        Class::Void => Vec::new(),
        Class::Words(kinds) => kinds.iter().copied().map(Some).collect(),
        Class::Memory => Vec::new(),
    }
}

fn decode_return<'f, R: FilType>(
    fil: &Fil<'f>,
    class: &Class,
    return_size: usize,
    sret: Option<RawPtr>,
) -> Result<R::Value<'f>> {
    match class {
        Class::Void => R::read(fil, RawPtr::NULL),
        Class::Memory => R::read(fil, sret.expect("sret allocated for memory-class returns")),
        Class::Words(kinds) => {
            let needed = kinds.len() * 8;
            if return_size < needed {
                return Err(Error::ReturnTooSmall { expected: needed, actual: return_size });
            }
            let words = read_words(fil, return_size, &word_kinds(class))?;
            decode_value::<R>(fil, class, &words)
        }
    }
}

/// A C function signature, written as a Rust `fn` pointer type over
/// [`FilType`] markers: `fn(i32, Ptr<c_char>) -> usize`.
///
/// Implemented for `fn` types of up to 12 arguments.
pub trait FilSig: 'static {
    /// The argument values, as a tuple.
    type Args<'f>;
    /// The return value.
    type Ret<'f>;

    #[doc(hidden)]
    fn call_raw<'f>(fil: &Fil<'f>, function: RawPtr, args: Self::Args<'f>) -> Result<Self::Ret<'f>>;

    #[doc(hidden)]
    fn callback_args<'f>(fil: &Fil<'f>, argument_size: usize) -> (Self::Args<'f>, Option<RawPtr>);

    #[doc(hidden)]
    fn callback_return<'f>(fil: &Fil<'f>, value: &Self::Ret<'f>, sret: Option<RawPtr>) -> Result<usize>;
}

macro_rules! impl_sig {
    ($($a:ident),*) => {
        impl<R: FilType + 'static, $($a: FilType + 'static),*> FilSig for fn($($a),*) -> R {
            type Args<'f> = ($($a::Value<'f>,)*);
            type Ret<'f> = R::Value<'f>;

            #[allow(non_snake_case, unused_mut)]
            fn call_raw<'f>(fil: &Fil<'f>, function: RawPtr, args: Self::Args<'f>) -> Result<R::Value<'f>> {
                let ret_class = classify::<R>()?;
                let mut words = Vec::new();
                let sret = if ret_class == Class::Memory {
                    let p = fil.alloc_raw(R::SIZE, R::ALIGN.max(16))?;
                    words.push(Word::Ptr(p));
                    Some(p)
                } else {
                    None
                };
                let ($($a,)*) = args;
                $(encode_value::<$a>(fil, &$a, &mut words)?;)*
                let return_size = invoke(fil, function, &words)?;
                decode_return::<R>(fil, &ret_class, return_size, sret)
            }

            #[allow(non_snake_case, unused_mut, unused_variables)]
            fn callback_args<'f>(fil: &Fil<'f>, argument_size: usize) -> (Self::Args<'f>, Option<RawPtr>) {
                let fail = |msg: String| -> ! { crate::callback::fail(fil, &msg) };
                let ret_class = classify::<R>().unwrap_or_else(|e| fail(e.to_string()));
                $(let $a = classify::<$a>().unwrap_or_else(|e| fail(e.to_string()));)*
                let mut kinds: Vec<Option<LeafKind>> = Vec::new();
                if ret_class == Class::Memory {
                    kinds.push(Some(LeafKind::Ptr));
                }
                $(
                    let (pad, arg_kinds) = arg_layout::<$a>(&$a, kinds.len());
                    kinds.extend(std::iter::repeat_n(None, pad));
                    kinds.extend(arg_kinds.into_iter().map(Some));
                )*
                let needed = kinds.len() * 8;
                if argument_size < needed {
                    unsafe { (fil.shim().fr_args_check_failure)(argument_size, needed) };
                    unreachable!("fr_args_check_failure does not return");
                }
                // Copy every slot out before decoding: decoding may allocate,
                // and anything that reaches a safepoint can reuse the buffer.
                let words = read_words(fil, argument_size, &kinds).unwrap_or_else(|e| fail(e.to_string()));
                let mut at = usize::from(ret_class == Class::Memory);
                let sret = if ret_class == Class::Memory {
                    match words[0] {
                        Word::Ptr(p) => Some(p),
                        _ => fail("missing sret pointer".into()),
                    }
                } else {
                    None
                };
                $(let $a = decode_arg::<$a>(fil, &$a, &words, &mut at)
                    .unwrap_or_else(|e| fail(format!("decoding a callback argument: {e}")));)*
                (($($a,)*), sret)
            }

            fn callback_return<'f>(fil: &Fil<'f>, value: &R::Value<'f>, sret: Option<RawPtr>) -> Result<usize> {
                let ret_class = classify::<R>()?;
                let mut words = Vec::new();
                match ret_class {
                    Class::Void => words.push(Word::Bits(0)),
                    Class::Memory => {
                        R::write(fil, sret.ok_or(Error::NullCapability)?, value)?;
                        words.push(Word::Bits(0));
                    }
                    Class::Words(_) => encode_value::<R>(fil, value, &mut words)?,
                }
                let shim = fil.shim();
                let size = words.len() * 8;
                unsafe { (shim.fr_cc_begin)(fil.thread, size) };
                for (i, word) in words.iter().enumerate() {
                    let status = match *word {
                        Word::Bits(bits) => unsafe { (shim.fr_cc_set_word)(fil.thread, size, i * 8, bits) },
                        Word::Ptr(p) => unsafe { (shim.fr_cc_set_ptr)(fil.thread, size, i * 8, p) },
                    };
                    error::check(status)?;
                }
                Ok(size)
            }
        }
    };
}

/// A `fn` type inside a signature or struct is a C function pointer; its
/// values are [`FilFn`]s.
macro_rules! impl_fn_ptr_type {
    ($($a:ident),*) => {
        impl<R: FilType + 'static, $($a: FilType + 'static),*> FilType for fn($($a),*) -> R {
            type Value<'f> = FilFn<'f, Self>;
            const SIZE: usize = 8;
            const ALIGN: usize = 8;

            fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<FilFn<'f, Self>> {
                <FilFn<'static, Self> as FilType>::read(fil, at)
            }

            fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &FilFn<'f, Self>) -> Result<()> {
                <FilFn<'static, Self> as FilType>::write(fil, at, value)
            }

            fn leaves(out: &mut Vec<Leaf>, offset: usize) {
                out.push(Leaf { offset, size: 8, kind: LeafKind::Ptr });
            }

            fn to_word(value: &FilFn<'_, Self>) -> Option<Word> {
                Some(Word::Ptr(value.raw))
            }

            fn from_word<'f>(fil: &Fil<'f>, word: Word) -> Option<FilFn<'f, Self>> {
                <FilFn<'static, Self> as FilType>::from_word(fil, word)
            }
        }
    };
}

macro_rules! impl_sig_and_fn_ptr {
    ($($a:ident),*) => {
        impl_sig!($($a),*);
        impl_fn_ptr_type!($($a),*);
    };
}

impl_sig_and_fn_ptr!();
impl_sig_and_fn_ptr!(A1);
impl_sig_and_fn_ptr!(A1, A2);
impl_sig_and_fn_ptr!(A1, A2, A3);
impl_sig_and_fn_ptr!(A1, A2, A3, A4);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5, A6);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5, A6, A7);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5, A6, A7, A8);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5, A6, A7, A8, A9);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
impl_sig_and_fn_ptr!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);

/// A Fil-C function pointer with signature `Sig`, valid within scope `'f`.
pub struct FilFn<'f, Sig> {
    raw: RawPtr,
    _marker: FnMarker<'f, Sig>,
}

/// Scope `'f`, covariant in `Sig`, `!Send`/`!Sync` (like `FilPtr`).
type FnMarker<'f, Sig> = PhantomData<(&'f (), fn() -> Sig, *const ())>;

impl<Sig> Clone for FilFn<'_, Sig> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Sig> Copy for FilFn<'_, Sig> {}

impl<Sig> std::fmt::Debug for FilFn<'_, Sig> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FilFn({:p})", self.raw.ptr)
    }
}

impl<'f, Sig: FilSig> FilFn<'f, Sig> {
    /// Checks that `ptr` is a callable Fil-C function.
    pub fn from_ptr(ptr: FilPtr<'f, Opaque>) -> Result<Self> {
        let shim = crate::runtime::shim().ok_or(Error::NotAFunction)?;
        error::check(unsafe { (shim.fr_check_function)(ptr.raw()) })?;
        Ok(FilFn { raw: ptr.raw(), _marker: PhantomData })
    }

    /// A null function pointer, for optional callbacks in C structs and
    /// arguments. Calling it is an error.
    pub fn null() -> Self {
        FilFn::from_raw_unchecked(FilPtr::<Opaque>::null().raw())
    }

    /// Whether this is a null function pointer.
    pub fn is_null(self) -> bool {
        self.as_ptr().is_null()
    }

    pub(crate) fn from_raw_unchecked(raw: RawPtr) -> Self {
        FilFn { raw, _marker: PhantomData }
    }

    /// Calls the function.
    pub fn call(self, fil: &Fil<'f>, args: Sig::Args<'f>) -> Result<Sig::Ret<'f>> {
        Sig::call_raw(fil, self.raw, args)
    }

    /// The function pointer as a plain pointer.
    pub fn as_ptr(self) -> FilPtr<'f, Opaque> {
        FilPtr::from_raw(self.raw)
    }

    /// Reinterprets the signature (like a C function-pointer cast; a wrong
    /// signature yields a Fil-C panic or garbage values, never corruption).
    pub fn cast<Sig2: FilSig>(self) -> FilFn<'f, Sig2> {
        FilFn::from_raw_unchecked(self.raw)
    }

    /// Starts an untyped call, for variadic functions.
    pub fn dyn_call<'a>(self, fil: &'a Fil<'f>) -> DynCall<'a, 'f> {
        DynCall::new(fil, self.raw)
    }
}

impl<'x, Sig: FilSig> FilType for FilFn<'x, Sig> {
    type Value<'f> = FilFn<'f, Sig>;
    const SIZE: usize = 8;
    const ALIGN: usize = 8;

    fn read<'f>(fil: &Fil<'f>, at: RawPtr) -> Result<FilFn<'f, Sig>> {
        // Not validated here: calling checks it's a function.
        Ok(FilFn::from_raw_unchecked(fil.load_ptr_raw(at)?))
    }

    fn write<'f>(fil: &Fil<'f>, at: RawPtr, value: &FilFn<'f, Sig>) -> Result<()> {
        fil.store_ptr_raw(at, value.raw)
    }

    fn leaves(out: &mut Vec<Leaf>, offset: usize) {
        out.push(Leaf { offset, size: 8, kind: LeafKind::Ptr });
    }

    fn to_word(value: &FilFn<'_, Sig>) -> Option<Word> {
        Some(Word::Ptr(value.raw))
    }

    fn from_word<'f>(_fil: &Fil<'f>, word: Word) -> Option<FilFn<'f, Sig>> {
        Some(match word {
            Word::Ptr(raw) => FilFn::from_raw_unchecked(raw),
            Word::Bits(bits) => FilFn::from_raw_unchecked(RawPtr::from_addr(bits as usize)),
        })
    }
}

/// A function exported by a Fil-C library. Unlike [`FilFn`], it isn't tied
/// to a scope: library functions are global objects that live as long as the
/// process. It is `Send` and `Sync`, so it can be looked up once and shared.
pub struct Function<Sig> {
    raw: RawPtr,
    _marker: PhantomData<fn() -> Sig>,
}

// SAFETY: the pointer refers to a global (never freed, never moved) function
// object of a library that is never unloaded; using it still requires a `Fil`.
unsafe impl<Sig> Send for Function<Sig> {}
unsafe impl<Sig> Sync for Function<Sig> {}

impl<Sig> Clone for Function<Sig> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Sig> Copy for Function<Sig> {}

impl<Sig> std::fmt::Debug for Function<Sig> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Function({:p})", self.raw.ptr)
    }
}

impl<Sig: FilSig> Function<Sig> {
    /// Wraps a function pointer if it is a global (library) function.
    pub fn from_global(f: FilFn<'_, Sig>) -> Result<Self> {
        let cap = f.as_ptr().capability().ok_or(Error::NullCapability)?;
        if !cap.function {
            return Err(Error::NotAFunction);
        }
        if cap.global {
            Ok(Function { raw: f.raw, _marker: PhantomData })
        } else {
            Err(Error::Unsupported("only global functions can outlive their scope".into()))
        }
    }

    /// Calls the function.
    pub fn call<'f>(&self, fil: &Fil<'f>, args: Sig::Args<'f>) -> Result<Sig::Ret<'f>> {
        Sig::call_raw(fil, self.raw, args)
    }

    /// The function as a scoped function pointer (to pass it to Fil-C).
    pub fn bind<'f>(&self, _fil: &Fil<'f>) -> FilFn<'f, Sig> {
        FilFn::from_raw_unchecked(self.raw)
    }

    /// Reinterprets the signature.
    pub fn cast<Sig2: FilSig>(self) -> Function<Sig2> {
        Function { raw: self.raw, _marker: PhantomData }
    }

    /// Starts an untyped call, for variadic functions.
    pub fn dyn_call<'a, 'f>(&self, fil: &'a Fil<'f>) -> DynCall<'a, 'f> {
        DynCall::new(fil, self.raw)
    }
}

/// A value that can be passed to [`DynCall::arg`].
pub trait FilArg<'f> {
    #[doc(hidden)]
    fn push(&self, fil: &Fil<'f>, out: &mut Vec<Word>) -> Result<()>;
}

macro_rules! scalar_arg {
    ($($t:ty),*) => {$(
        impl<'f> FilArg<'f> for $t {
            fn push(&self, fil: &Fil<'f>, out: &mut Vec<Word>) -> Result<()> {
                encode_value::<$t>(fil, self, out)
            }
        }
    )*};
}

scalar_arg!(i8, u8, i16, u16, i32, u32, i64, u64, isize, usize, f32, f64, bool);

impl<'f, T: ?Sized> FilArg<'f> for FilPtr<'f, T> {
    fn push(&self, _fil: &Fil<'f>, out: &mut Vec<Word>) -> Result<()> {
        out.push(Word::Ptr(self.raw()));
        Ok(())
    }
}

impl<'f, S> FilArg<'f> for FilFn<'f, S> {
    fn push(&self, _fil: &Fil<'f>, out: &mut Vec<Word>) -> Result<()> {
        out.push(Word::Ptr(self.raw));
        Ok(())
    }
}

/// An untyped call, built one argument at a time. This is how to call
/// variadic functions: pass arguments after C's default promotions (`f64`
/// for floats, at least `i32` for integers).
pub struct DynCall<'a, 'f> {
    fil: &'a Fil<'f>,
    function: RawPtr,
    words: Vec<Word>,
    error: Option<Error>,
}

impl<'a, 'f> DynCall<'a, 'f> {
    fn new(fil: &'a Fil<'f>, function: RawPtr) -> Self {
        DynCall { fil, function, words: Vec::new(), error: None }
    }

    /// Adds an argument.
    pub fn arg(mut self, value: impl FilArg<'f>) -> Self {
        if self.error.is_none()
            && let Err(e) = value.push(self.fil, &mut self.words)
        {
            self.error = Some(e);
        }
        self
    }

    /// Adds an argument of a [`FilType`] (for example a struct by value).
    pub fn arg_value<T: FilType>(mut self, value: &T::Value<'f>) -> Self {
        if self.error.is_none()
            && let Err(e) = encode_value::<T>(self.fil, value, &mut self.words)
        {
            self.error = Some(e);
        }
        self
    }

    /// Performs the call, decoding the return value as `R`.
    pub fn call<R: FilType>(mut self) -> Result<R::Value<'f>> {
        if let Some(e) = self.error {
            return Err(e);
        }
        let class = classify::<R>()?;
        let sret = if class == Class::Memory {
            let p = self.fil.alloc_raw(R::SIZE, R::ALIGN.max(16))?;
            self.words.insert(0, Word::Ptr(p));
            Some(p)
        } else {
            None
        };
        let return_size = invoke(self.fil, self.function, &self.words)?;
        decode_return::<R>(self.fil, &class, return_size, sret)
    }
}

