#[derive(fil_rust::FilType)]
struct Bounded<'f>
where
    'f: 'static,
{
    p: fil_rust::FilPtr<'f, u8>,
}

fn main() {}
