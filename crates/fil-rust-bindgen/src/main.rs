//! `fil-rust-bindgen <header> [options] [-- <clang args>]`

use std::process::ExitCode;

const USAGE: &str = "\
usage: fil-rust-bindgen <header.h> [options] [-- <clang args>...]

Generates fil-rust bindings (FilType structs and a foreign! interface) for
the declarations in a C header.

options:
  -o, --output <file>       write to <file> instead of stdout
  --interface <Name>        name of the foreign! struct (default: header name in CamelCase)
  --module <name>           name of the generated module (default: <header>_ffi)
  --allowlist-file <regex>  also bind declarations from matching files
  -h, --help                show this help";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut header = None;
    let mut output = None;
    let mut interface = None;
    let mut module = None;
    let mut allowlist = Vec::new();
    let mut clang_args = Vec::new();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| args.next().ok_or_else(|| format!("{name} needs a value"));
        let result = match arg.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "-o" | "--output" => value(&arg).map(|v| output = Some(v)),
            "--interface" => value(&arg).map(|v| interface = Some(v)),
            "--module" => value(&arg).map(|v| module = Some(v)),
            "--allowlist-file" => value(&arg).map(|v| allowlist.push(v)),
            "--" => {
                clang_args.extend(args.by_ref());
                Ok(())
            }
            s if s.starts_with('-') => Err(format!("unknown option {s}")),
            _ if header.is_none() => {
                header = Some(arg);
                Ok(())
            }
            _ => Err("only one header may be given".into()),
        };
        if let Err(e) = result {
            eprintln!("fil-rust-bindgen: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    let Some(header) = header else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let mut builder = fil_rust_bindgen::Builder::new(header).clang_args(clang_args);
    if let Some(i) = interface {
        builder = builder.interface_name(i);
    }
    if let Some(m) = module {
        builder = builder.module_name(m);
    }
    for a in allowlist {
        builder = builder.allowlist_file(a);
    }
    let bindings = match builder.generate() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("fil-rust-bindgen: {e}");
            return ExitCode::FAILURE;
        }
    };
    for s in bindings.skipped() {
        eprintln!("fil-rust-bindgen: skipped {s}");
    }
    match output {
        Some(path) => {
            if let Err(e) = bindings.write_to_file(&path) {
                eprintln!("fil-rust-bindgen: writing {path}: {e}");
                return ExitCode::FAILURE;
            }
        }
        None => print!("{bindings}"),
    }
    ExitCode::SUCCESS
}
