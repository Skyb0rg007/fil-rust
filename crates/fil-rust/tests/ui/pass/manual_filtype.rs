// A hand-written FilType (a newtype over a C `int`) and FilArg: they work by
// delegating to the crate's own implementations.
use fil_rust::{DynArgs, Fil, FilArg, FilType, Leaf, Place, Result, Word};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Celsius(i32);

impl FilType for Celsius {
    type Value<'f> = Celsius;
    const SIZE: usize = 4;
    const ALIGN: usize = 4;

    fn read<'f>(fil: &Fil<'f>, at: Place<'f>) -> Result<Celsius> {
        i32::read(fil, at).map(Celsius)
    }

    fn write<'f>(fil: &Fil<'f>, at: Place<'f>, value: &Celsius) -> Result<()> {
        i32::write(fil, at, &value.0)
    }

    fn leaves(out: &mut Vec<Leaf>, offset: usize) {
        i32::leaves(out, offset)
    }

    fn to_word<'f>(value: &Celsius) -> Option<Word<'f>> {
        Some(Word::bits(value.0 as i64 as u64))
    }

    fn from_word<'f>(fil: &Fil<'f>, word: Word<'f>) -> Option<Celsius> {
        i32::from_word(fil, word).map(Celsius)
    }
}

impl<'f> FilArg<'f> for Celsius {
    fn push(&self, fil: &Fil<'f>, out: &mut DynArgs<'f>) -> Result<()> {
        self.0.push(fil, out)
    }
}

/// A pair of them, reading the second at an offset.
struct Range;

impl FilType for Range {
    type Value<'f> = (Celsius, Celsius);
    const SIZE: usize = 8;
    const ALIGN: usize = 4;

    fn read<'f>(fil: &Fil<'f>, at: Place<'f>) -> Result<(Celsius, Celsius)> {
        Ok((Celsius::read(fil, at)?, Celsius::read(fil, at.byte_add(4))?))
    }

    fn write<'f>(fil: &Fil<'f>, at: Place<'f>, value: &(Celsius, Celsius)) -> Result<()> {
        Celsius::write(fil, at, &value.0)?;
        Celsius::write(fil, at.byte_add(4), &value.1)
    }

    fn leaves(out: &mut Vec<Leaf>, offset: usize) {
        Celsius::leaves(out, offset);
        Celsius::leaves(out, offset + 4);
    }
}

fn main() {
    let rt = fil_rust::Runtime::start_from_env().unwrap();
    let libc = rt.libc();
    rt.run(|fil| {
        let p = fil.alloc::<Range>(&(Celsius(-5), Celsius(30))).unwrap();
        assert_eq!(p.read(fil).unwrap(), (Celsius(-5), Celsius(30)));
        let abs = libc.function::<fn(Celsius) -> Celsius>(fil, "abs").unwrap();
        assert_eq!(abs.call(fil, (Celsius(-3),)).unwrap(), Celsius(3));
        let _ = abs.dyn_call(fil).arg(Celsius(-4)).call::<Celsius>();
    });
}
