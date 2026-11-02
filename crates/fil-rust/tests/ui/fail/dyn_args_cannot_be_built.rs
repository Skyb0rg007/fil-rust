// A FilArg implementation can only delegate to the crate's own ones.
use fil_rust::{DynArgs, Fil, FilArg, Result};

struct Forged;

impl<'f> FilArg<'f> for Forged {
    fn push(&self, _fil: &Fil<'f>, out: &mut DynArgs<'f>) -> Result<()> {
        out.words.push(0x1000);
        Ok(())
    }
}

fn main() {}
