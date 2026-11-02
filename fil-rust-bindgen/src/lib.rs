//! Generates [fil-rust](https://docs.rs/fil-rust) bindings from C headers.
//!
//! [bindgen] parses the header; this crate then rewrites its output into
//! fil-rust's safe types:
//!
//! | C | generated |
//! |---|---|
//! | `struct s { ... }` | `#[derive(FilType)] struct s` (with a `'f` lifetime if it holds pointers), its size and alignment checked against clang's layout |
//! | `struct s;` (opaque) | `enum s {}`, usable as a pointee |
//! | `T *` | `Ptr<T>` in signatures, `FilPtr<'f, T>` in struct fields |
//! | `void *` | `Ptr` (`Ptr<Opaque>`) |
//! | `typedef R (*f)(A...)` | `type f = fn(A...) -> R`, usable with `Callback<f>` |
//! | function pointer field | `FilFn<'f, fn(A...) -> R>` |
//! | functions, `extern` globals | one [`foreign!`] interface struct |
//! | `#define`s, enum constants, typedefs | constants and type aliases, as bindgen emits them |
//!
//! Items fil-rust can't express are left out, with a comment in the output
//! saying why (see [`Bindings::skipped`]): variadic functions (call them with
//! `DynCall` instead), bitfields, unions and flexible array members by value,
//! and 128-bit or `long double` values.
//!
//! In a build script:
//!
//! ```no_run
//! let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
//! fil_rust_bindgen::Builder::new("include/api.h")
//!     .interface_name("Api")
//!     .generate()
//!     .expect("generating bindings")
//!     .write_to_file(out.join("api.rs"))
//!     .unwrap();
//! ```
//!
//! and in the crate, `include!(concat!(env!("OUT_DIR"), "/api.rs"));` defines
//! a module (`api_ffi`, see [`Builder::module_name`]) holding the bindings.
//!
//! [`foreign!`]: https://docs.rs/fil-rust/latest/fil_rust/macro.foreign.html

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use syn::{Fields, ForeignItem, GenericArgument, Ident, Item, PathArguments, ReturnType, Type};

/// Configures binding generation for one header.
#[derive(Clone, Debug)]
pub struct Builder {
    header: PathBuf,
    clang_args: Vec<String>,
    allowlist_files: Vec<String>,
    interface: Option<String>,
    module: Option<String>,
}

impl Builder {
    /// Bindings for the declarations in `header`.
    pub fn new(header: impl Into<PathBuf>) -> Builder {
        Builder { header: header.into(), clang_args: Vec::new(), allowlist_files: Vec::new(), interface: None, module: None }
    }

    /// An argument for clang (`-I`, `-D`, ...).
    pub fn clang_arg(mut self, arg: impl Into<String>) -> Builder {
        self.clang_args.push(arg.into());
        self
    }

    /// Several clang arguments.
    pub fn clang_args<I: IntoIterator<Item = S>, S: Into<String>>(mut self, args: I) -> Builder {
        self.clang_args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Also generate bindings for declarations in files matching `regex`.
    /// By default only the header itself is bound (plus the types its
    /// declarations use).
    pub fn allowlist_file(mut self, regex: impl Into<String>) -> Builder {
        self.allowlist_files.push(regex.into());
        self
    }

    /// The name of the `foreign!` struct holding the functions and globals.
    /// Defaults to the header's file name in CamelCase.
    pub fn interface_name(mut self, name: impl Into<String>) -> Builder {
        self.interface = Some(name.into());
        self
    }

    /// The name of the generated module. Defaults to the header's file name
    /// followed by `_ffi` (`foo.h` → `foo_ffi`), which keeps it from
    /// colliding with a type named after the header.
    pub fn module_name(mut self, name: impl Into<String>) -> Builder {
        self.module = Some(name.into());
        self
    }

    /// Runs bindgen and rewrites its output.
    pub fn generate(self) -> Result<Bindings, Error> {
        let header = self.header.to_str().ok_or_else(|| Error("header path is not UTF-8".into()))?.to_string();
        let stem = file_stem(&self.header);
        let mut builder = bindgen::Builder::default()
            .header(&header)
            .allowlist_file(regex_escape(&header))
            .clang_args(&self.clang_args)
            .generate_comments(true)
            .layout_tests(true)
            .derive_debug(false)
            .derive_copy(false)
            .size_t_is_usize(true)
            .prepend_enum_name(false);
        for f in &self.allowlist_files {
            builder = builder.allowlist_file(f);
        }
        let raw = builder.generate().map_err(|e| Error(format!("bindgen failed on {header}: {e}")))?.to_string();
        let interface = self.interface.unwrap_or_else(|| camel_case(&stem));
        let module = self.module.unwrap_or_else(|| format!("{}_ffi", snake_case(&stem)));
        rewrite(&raw, &header, &interface, &module)
    }
}

/// Generated bindings.
#[derive(Clone, Debug)]
pub struct Bindings {
    code: String,
    skipped: Vec<String>,
}

impl Bindings {
    /// Items that were left out, with the reason.
    pub fn skipped(&self) -> &[String] {
        &self.skipped
    }

    /// Writes the bindings to `path`.
    pub fn write_to_file(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        std::fs::write(path, &self.code)
    }
}

impl fmt::Display for Bindings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.code)
    }
}

/// A binding generation failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------

/// What a C type name refers to.
#[derive(Clone)]
enum Named {
    /// A struct fil-rust can represent. `lifetime`: it holds pointers.
    Struct { lifetime: bool },
    /// An opaque struct or a union: only usable behind a pointer.
    Opaque,
    /// A typedef of a function pointer, emitted as a `fn` type.
    FnAlias,
    /// Any other typedef. `lifetime`: its target holds pointers.
    Alias { lifetime: bool },
    /// A typedef that can't be represented.
    Unsupported(String),
}

struct StructDef {
    attrs: Vec<syn::Attribute>,
    fields: Vec<(Ident, Type, Vec<syn::Attribute>)>,
}

/// Where a type appears, which decides how pointers are spelled.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Ctx {
    /// A function signature or a pointee: `Ptr<T>`, `S<'static>`.
    Sig,
    /// A struct field or a typedef target: `FilPtr<'f, T>`, `S<'f>`.
    Field,
}

struct Rewriter {
    names: BTreeMap<String, Named>,
}

fn rewrite(raw: &str, header: &str, interface: &str, module: &str) -> Result<Bindings, Error> {
    let file = syn::parse_file(raw).map_err(|e| Error(format!("parsing bindgen output: {e}")))?;

    let mut structs: BTreeMap<String, StructDef> = BTreeMap::new();
    let mut struct_order: Vec<String> = Vec::new();
    let mut layouts: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut aliases: Vec<syn::ItemType> = Vec::new();
    let mut consts: Vec<syn::ItemConst> = Vec::new();
    let mut functions: Vec<syn::ForeignItemFn> = Vec::new();
    let mut statics: Vec<syn::ForeignItemStatic> = Vec::new();
    let mut names: BTreeMap<String, Named> = BTreeMap::new();
    let mut skipped: Vec<String> = Vec::new();
    let mut unrepresentable: BTreeMap<String, String> = BTreeMap::new();

    for item in file.items {
        if bindgen_helper(&item) {
            continue;
        }
        match item {
            Item::Struct(s) => {
                let name = s.ident.to_string();
                let Fields::Named(fields) = &s.fields else {
                    unrepresentable.insert(name.clone(), "unsupported struct shape".into());
                    names.insert(name, Named::Opaque);
                    continue;
                };
                let field_names: Vec<String> =
                    fields.named.iter().map(|f| f.ident.as_ref().unwrap().to_string()).collect();
                if field_names == ["_unused"] {
                    names.insert(name, Named::Opaque);
                    continue;
                }
                if let Some(why) = fields.named.iter().find_map(unsupported_field) {
                    unrepresentable.insert(name.clone(), why);
                    names.insert(name, Named::Opaque);
                    continue;
                }
                let doc = s.attrs.iter().filter(|a| a.path().is_ident("doc")).cloned().collect();
                structs.insert(
                    name.clone(),
                    StructDef {
                        attrs: doc,
                        fields: fields
                            .named
                            .iter()
                            .map(|f| {
                                let doc = f.attrs.iter().filter(|a| a.path().is_ident("doc")).cloned().collect();
                                (f.ident.clone().unwrap(), f.ty.clone(), doc)
                            })
                            .collect(),
                    },
                );
                struct_order.push(name);
            }
            Item::Union(u) => {
                let name = u.ident.to_string();
                unrepresentable.insert(name.clone(), "unions can't be passed by value".into());
                names.insert(name, Named::Opaque);
            }
            Item::Type(t) => {
                if fn_pointer(&t.ty).is_some() {
                    names.insert(t.ident.to_string(), Named::FnAlias);
                } else {
                    names.insert(t.ident.to_string(), Named::Alias { lifetime: false });
                }
                aliases.push(t);
            }
            Item::Const(c) if c.ident == "_" => {
                if let Some((name, size, align)) = parse_layout(&c) {
                    layouts.insert(name, (size, align));
                }
            }
            Item::Const(c) => consts.push(c),
            Item::ForeignMod(m) => {
                for fi in m.items {
                    match fi {
                        ForeignItem::Fn(f) => functions.push(f),
                        ForeignItem::Static(s) => statics.push(s),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    for name in &struct_order {
        names.insert(name.clone(), Named::Struct { lifetime: false });
    }
    let mut rw = Rewriter { names };

    // Which structs and typedefs hold pointers (directly or through other
    // structs): iterate to a fixed point.
    loop {
        let mut changed = false;
        for name in &struct_order {
            let lifetime = structs[name].fields.iter().any(|(_, ty, _)| rw.holds_pointer(ty));
            if lifetime && matches!(rw.names[name], Named::Struct { lifetime: false }) {
                rw.names.insert(name.clone(), Named::Struct { lifetime: true });
                changed = true;
            }
        }
        for a in &aliases {
            let name = a.ident.to_string();
            if matches!(rw.names[&name], Named::Alias { lifetime: false }) && rw.holds_pointer(&a.ty) {
                rw.names.insert(name, Named::Alias { lifetime: true });
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }

    // Typedefs that can't be represented, and structs whose fields can't be
    // (which become opaque): iterate to a fixed point, since each can make
    // the other unrepresentable.
    let mut failed_aliases: Vec<(String, String)> = Vec::new();
    loop {
        let mut changed = false;
        for a in &aliases {
            let name = a.ident.to_string();
            let result = match &rw.names[&name] {
                Named::Unsupported(_) => continue,
                Named::FnAlias => rw.map_fn(fn_pointer(&a.ty).unwrap()).map(drop),
                _ => rw.map(&a.ty, Ctx::Field).map(drop),
            };
            if let Err(why) = result {
                rw.names.insert(name.clone(), Named::Unsupported(why.clone()));
                failed_aliases.push((name, why));
                changed = true;
            }
        }
        for name in &struct_order {
            if unrepresentable.contains_key(name) {
                continue;
            }
            for (field, ty, _) in &structs[name].fields {
                if let Err(why) = rw.map(ty, Ctx::Field) {
                    unrepresentable.insert(name.clone(), format!("field `{field}`: {why}"));
                    rw.names.insert(name.clone(), Named::Opaque);
                    changed = true;
                    break;
                }
            }
        }
        if !changed {
            break;
        }
    }

    let mut out: Vec<TokenStream> = Vec::new();

    for c in &consts {
        let c = rewrite_raw_paths(c.clone());
        out.push(quote!(#c));
    }

    for (name, why) in &failed_aliases {
        skip(&mut out, &mut skipped, format!("typedef `{name}`: {why}"));
    }
    for a in &aliases {
        let ident = &a.ident;
        let doc = a.attrs.iter().filter(|a| a.path().is_ident("doc"));
        let name = ident.to_string();
        let item = match &rw.names[&name] {
            Named::Unsupported(_) => continue,
            Named::FnAlias => rw.map_fn(fn_pointer(&a.ty).unwrap()).map(|f| quote!(#(#doc)* pub type #ident = #f;)),
            Named::Alias { lifetime: true } => rw.map(&a.ty, Ctx::Field).map(|t| quote!(#(#doc)* pub type #ident<'f> = #t;)),
            _ => rw.map(&a.ty, Ctx::Sig).map(|t| quote!(#(#doc)* pub type #ident = #t;)),
        };
        match item {
            Ok(item) => out.push(item),
            Err(why) => skip(&mut out, &mut skipped, format!("typedef `{name}`: {why}")),
        }
    }

    // Opaque types (and the structs and unions fil-rust can't represent).
    let opaque: BTreeSet<&String> =
        rw.names.iter().filter(|(_, n)| matches!(n, Named::Opaque)).map(|(k, _)| k).collect();
    for name in opaque {
        if let Some(why) = unrepresentable.get(name) {
            skip(&mut out, &mut skipped, format!("`{name}` is only usable behind a pointer: {why}"));
        }
        let ident = Ident::new(name, Span::call_site());
        let doc = format!(" The C type `{name}`, only usable behind a pointer.");
        out.push(quote!(#[doc = #doc] pub enum #ident {}));
    }

    for name in &struct_order {
        if matches!(rw.names[name], Named::Opaque) {
            continue;
        }
        let def = &structs[name];
        let ident = Ident::new(name, Span::call_site());
        let lifetime = matches!(rw.names[name], Named::Struct { lifetime: true });
        let fields = def.fields.iter().map(|(f, ty, doc)| {
            let ty = rw.map(ty, Ctx::Field).expect("checked above");
            quote!(#(#doc)* pub #f: #ty)
        });
        let generics = if lifetime { quote!(<'f>) } else { quote!() };
        let layout = layouts.get(name).map(|(size, align)| {
            let size = syn::LitInt::new(&size.to_string(), Span::call_site());
            let align = syn::LitInt::new(&align.to_string(), Span::call_site());
            quote!(#[fil(size = #size, align = #align)])
        });
        let doc = &def.attrs;
        out.push(quote! {
            #(#doc)*
            #[derive(::fil_rust::FilType, Clone, Copy, Debug)]
            #layout
            pub struct #ident #generics { #(#fields,)* }
        });
    }

    // The interface: functions and globals.
    let mut entries: Vec<TokenStream> = Vec::new();
    for s in &statics {
        let name = s.ident.to_string();
        match rw.map(&s.ty, Ctx::Sig) {
            Ok(ty) => {
                let ident = &s.ident;
                let doc = s.attrs.iter().filter(|a| a.path().is_ident("doc"));
                entries.push(quote!(#(#doc)* static #ident: #ty;));
            }
            Err(why) => skip(&mut out, &mut skipped, format!("global `{name}`: {why}")),
        }
    }
    for f in &functions {
        let name = f.sig.ident.to_string();
        if f.sig.variadic.is_some() {
            skip(
                &mut out,
                &mut skipped,
                format!("function `{name}`: variadic; call it with `library().function(..)?.dyn_call(fil)`"),
            );
            continue;
        }
        match rw.map_function(f) {
            Ok(entry) => entries.push(entry),
            Err(why) => skip(&mut out, &mut skipped, format!("function `{name}`: {why}")),
        }
    }
    if !entries.is_empty() {
        let ident = Ident::new(interface, Span::call_site());
        let doc = format!(" The functions and globals declared in `{}`.", file_name(header));
        // prettyplease can't format macro bodies, so format the entries as an
        // extern block and turn it into the macro call afterwards.
        let marker = format!("{FOREIGN_MARKER}{ident}|{doc}");
        out.push(quote! {
            #[doc = #marker]
            unsafe extern "C" { #(#entries)* }
        });
    }

    let module = Ident::new(module, Span::call_site());
    let doc = format!(" Bindings for `{}`, generated by fil-rust-bindgen.", file_name(header));
    let mut skipped_docs: Vec<String> = Vec::new();
    if !skipped.is_empty() {
        skipped_docs.push(String::new());
        skipped_docs.push(" Left out:".into());
        skipped_docs.extend(skipped.iter().map(|s| format!(" * {s}")));
    }
    let tokens = quote! {
        #[doc = #doc]
        #(#[doc = #skipped_docs])*
        #[allow(non_camel_case_types, non_snake_case, non_upper_case_globals, dead_code, clippy::all)]
        pub mod #module {
            #[allow(unused_imports)]
            use ::fil_rust::{FilFn, FilPtr, Opaque, Ptr};
            #[allow(unused_imports)]
            use ::std::ffi::{
                c_char, c_double, c_float, c_int, c_long, c_longlong, c_schar, c_short, c_uchar, c_uint, c_ulong,
                c_ulonglong, c_ushort,
            };
            #(#out)*
        }
    };
    let file: syn::File = syn::parse2(tokens).map_err(|e| Error(format!("internal error: {e}")))?;
    let mut code = String::from("// Generated by fil-rust-bindgen. Do not edit.\n\n");
    code.push_str(&extern_to_foreign(&prettyplease::unparse(&file)));
    Ok(Bindings { code, skipped })
}

const FOREIGN_MARKER: &str = "__fil_rust_foreign__|";

/// Turns the marked `extern "C" { .. }` block into a `foreign!` call.
fn extern_to_foreign(code: &str) -> String {
    let mut out = String::new();
    let mut lines = code.lines();
    while let Some(line) = lines.next() {
        let Some(at) = line.find(FOREIGN_MARKER) else {
            out.push_str(line);
            out.push('\n');
            continue;
        };
        let indent = &line[..line.len() - line.trim_start().len()];
        let (name, doc) = line[at + FOREIGN_MARKER.len()..].split_once('|').unwrap();
        let doc = doc.trim_end_matches('"').trim_end_matches(']');
        lines.next(); // unsafe extern "C" {
        out.push_str(&format!("{indent}::fil_rust::foreign! {{\n{indent}    ///{doc}\n{indent}    pub struct {name} {{\n"));
        let close = format!("{indent}}}");
        for body in lines.by_ref() {
            if body == close {
                break;
            }
            if body.is_empty() {
                out.push('\n');
            } else {
                out.push_str(&format!("    {body}\n"));
            }
        }
        out.push_str(&format!("{indent}    }}\n{indent}}}\n"));
    }
    out
}

fn skip(_out: &mut Vec<TokenStream>, skipped: &mut Vec<String>, why: String) {
    skipped.push(why);
}

impl Rewriter {
    /// Whether `ty`, in a struct field, involves a pointer.
    fn holds_pointer(&self, ty: &Type) -> bool {
        match ty {
            Type::Ptr(_) => true,
            Type::Array(a) => self.holds_pointer(&a.elem),
            Type::Path(p) => {
                if fn_pointer(ty).is_some() {
                    return true;
                }
                let name = p.path.segments.last().unwrap().ident.to_string();
                matches!(
                    self.names.get(&name),
                    Some(Named::Struct { lifetime: true } | Named::Alias { lifetime: true } | Named::FnAlias)
                )
            }
            _ => false,
        }
    }

    fn map_function(&self, f: &syn::ForeignItemFn) -> Result<TokenStream, String> {
        let ident = &f.sig.ident;
        let mut args = Vec::new();
        for (i, input) in f.sig.inputs.iter().enumerate() {
            let syn::FnArg::Typed(pt) = input else { return Err("unexpected receiver".into()) };
            let name = match &*pt.pat {
                syn::Pat::Ident(p) if p.ident != "fil" && p.ident != "self" => p.ident.clone(),
                syn::Pat::Ident(p) => format_ident!("{}_", p.ident),
                _ => format_ident!("arg{}", i),
            };
            let ty = self.map(&pt.ty, Ctx::Sig).map_err(|e| format!("parameter `{name}`: {e}"))?;
            args.push(quote!(#name: #ty));
        }
        let ret = match &f.sig.output {
            ReturnType::Default => quote!(),
            ReturnType::Type(_, ty) if matches!(**ty, Type::Never(_)) => quote!(),
            ReturnType::Type(_, ty) => {
                let ty = self.map(ty, Ctx::Sig).map_err(|e| format!("return type: {e}"))?;
                quote!(-> #ty)
            }
        };
        let doc = f.attrs.iter().filter(|a| a.path().is_ident("doc"));
        Ok(quote!(#(#doc)* fn #ident(#(#args),*) #ret;))
    }

    /// `unsafe extern "C" fn(A..) -> R` → `fn(A'..) -> R'`.
    fn map_fn(&self, f: &syn::TypeFnPtr) -> Result<TokenStream, String> {
        if f.variadic.is_some() {
            return Err("variadic function pointer".into());
        }
        let args = f.inputs.iter().map(|a| self.map(&a.ty, Ctx::Sig)).collect::<Result<Vec<_>, _>>()?;
        if args.len() > 12 {
            return Err("function pointers take at most 12 arguments".into());
        }
        let ret = match &f.output {
            ReturnType::Default => quote!(),
            ReturnType::Type(_, ty) => {
                let ty = self.map(ty, Ctx::Sig)?;
                quote!(-> #ty)
            }
        };
        Ok(quote!(fn(#(#args),*) #ret))
    }

    /// Maps a bindgen type to its fil-rust spelling.
    fn map(&self, ty: &Type, ctx: Ctx) -> Result<TokenStream, String> {
        match ty {
            Type::Ptr(p) => {
                let pointee = self.map_pointee(&p.elem)?;
                Ok(match (ctx, pointee.to_string() == "Opaque") {
                    (Ctx::Sig, true) => quote!(Ptr),
                    (Ctx::Sig, false) => quote!(Ptr<#pointee>),
                    (Ctx::Field, true) => quote!(FilPtr<'f>),
                    (Ctx::Field, false) => quote!(FilPtr<'f, #pointee>),
                })
            }
            Type::Array(a) => {
                let elem = self.map(&a.elem, ctx)?;
                let len = match &a.len {
                    syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(n), .. }) => {
                        let n = syn::LitInt::new(n.base10_digits(), n.span());
                        quote!(#n)
                    }
                    other => quote!(#other),
                };
                Ok(quote!([#elem; #len]))
            }
            Type::Tuple(t) if t.elems.is_empty() => Ok(quote!(())),
            Type::Path(_) => {
                if let Some(f) = fn_pointer(ty) {
                    let f = self.map_fn(f)?;
                    return Ok(match ctx {
                        Ctx::Sig => f,
                        Ctx::Field => quote!(FilFn<'f, #f>),
                    });
                }
                let Type::Path(p) = ty else { unreachable!() };
                let last = p.path.segments.last().unwrap();
                let name = last.ident.to_string();
                if let Some(prim) = primitive(&name) {
                    return prim;
                }
                let ident = &last.ident;
                match self.names.get(&name) {
                    Some(Named::Struct { lifetime: true } | Named::Alias { lifetime: true }) => Ok(match ctx {
                        Ctx::Sig => quote!(#ident<'static>),
                        Ctx::Field => quote!(#ident<'f>),
                    }),
                    Some(Named::FnAlias) => Ok(match ctx {
                        Ctx::Sig => quote!(#ident),
                        Ctx::Field => quote!(FilFn<'f, #ident>),
                    }),
                    Some(Named::Opaque) => Err(format!("`{name}` is only usable behind a pointer")),
                    Some(Named::Unsupported(why)) => Err(format!("`{name}`: {why}")),
                    Some(_) => Ok(quote!(#ident)),
                    None => Err(format!("unknown type `{name}`")),
                }
            }
            other => Err(format!("unsupported type `{}`", quote!(#other))),
        }
    }

    /// The type marker behind a pointer.
    fn map_pointee(&self, ty: &Type) -> Result<TokenStream, String> {
        if let Type::Path(p) = ty {
            let last = p.path.segments.last().unwrap();
            let name = last.ident.to_string();
            if name == "c_void" {
                return Ok(quote!(Opaque));
            }
            if matches!(self.names.get(&name), Some(Named::Opaque)) {
                let ident = &last.ident;
                return Ok(quote!(#ident));
            }
        }
        // A pointer to something that can't be represented by value is still
        // a pointer: fall back to an untyped one.
        Ok(self.map(ty, Ctx::Sig).unwrap_or_else(|_| quote!(Opaque)))
    }
}

fn primitive(name: &str) -> Option<Result<TokenStream, String>> {
    let ok = |t: TokenStream| Some(Ok(t));
    match name {
        "i8" | "u8" | "i16" | "u16" | "i32" | "u32" | "i64" | "u64" | "isize" | "usize" | "f32" | "f64" | "bool" => {
            let ident = Ident::new(name, Span::call_site());
            ok(quote!(#ident))
        }
        "c_char" | "c_schar" | "c_uchar" | "c_short" | "c_ushort" | "c_int" | "c_uint" | "c_long" | "c_ulong"
        | "c_longlong" | "c_ulonglong" | "c_float" | "c_double" => {
            let ident = Ident::new(name, Span::call_site());
            ok(quote!(#ident))
        }
        "i128" | "u128" => Some(Err("128-bit integers (or `long double`) aren't supported".into())),
        "c_void" => Some(Err("`void` is only usable behind a pointer".into())),
        _ => None,
    }
}

/// `Option<unsafe extern "C" fn(..)>` (how bindgen spells a nullable
/// function pointer), or a bare `fn` type.
fn fn_pointer(ty: &Type) -> Option<&syn::TypeFnPtr> {
    match ty {
        Type::FnPtr(f) => Some(f),
        Type::Path(p) => {
            let last = p.path.segments.last()?;
            if last.ident != "Option" {
                return None;
            }
            let PathArguments::AngleBracketed(args) = &last.arguments else { return None };
            match args.args.first()? {
                GenericArgument::Type(Type::FnPtr(f)) => Some(f),
                _ => None,
            }
        }
        _ => None,
    }
}

/// bindgen's support types (`__BindgenBitfieldUnit` and friends) and their
/// impls: the types that use them are left out anyway.
fn bindgen_helper(item: &Item) -> bool {
    let name = match item {
        Item::Struct(s) => s.ident.to_string(),
        Item::Union(u) => u.ident.to_string(),
        Item::Impl(_) => return true,
        _ => return false,
    };
    name.starts_with("__Bindgen") || name == "__IncompleteArrayField"
}

fn unsupported_field(f: &syn::Field) -> Option<String> {
    let name = f.ident.as_ref()?.to_string();
    if name.starts_with("_bitfield") {
        return Some("bitfields aren't supported".into());
    }
    let ty = quote!(#f).to_string();
    if ty.contains("__IncompleteArrayField") {
        return Some("flexible array members aren't supported".into());
    }
    if ty.contains("__BindgenUnionField") {
        return Some("unions aren't supported".into());
    }
    if ty.contains("__BindgenBitfieldUnit") {
        return Some("bitfields aren't supported".into());
    }
    None
}

/// Reads `["Size of S"][size_of::<S>() - 16usize]` style layout checks.
fn parse_layout(c: &syn::ItemConst) -> Option<(String, u64, u64)> {
    let text = quote!(#c).to_string();
    let grab = |label: &str| -> Option<(String, u64)> {
        let start = text.find(label)? + label.len();
        let rest = &text[start..];
        let name_end = rest.find('"')?;
        let name = rest[..name_end].to_string();
        let minus = rest.find("- ")? + 2;
        let digits: String = rest[minus..].chars().take_while(|c| c.is_ascii_digit()).collect();
        Some((name, digits.parse().ok()?))
    };
    let (name, size) = grab("\"Size of ")?;
    let (_, align) = grab("\"Alignment of ")?;
    Some((name, size, align))
}

/// bindgen spells C types `::std::os::raw::c_int`; prefer `::std::ffi`.
fn rewrite_raw_paths(c: syn::ItemConst) -> syn::ItemConst {
    let text = quote!(#c).to_string().replace(":: std :: os :: raw ::", "");
    syn::parse_str(&text).unwrap_or(c)
}

fn file_name(path: &str) -> String {
    Path::new(path).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string())
}

fn file_stem(path: &Path) -> String {
    path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "bindings".into())
}

fn camel_case(s: &str) -> String {
    s.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut cs = w.chars();
            cs.next().map(|c| c.to_ascii_uppercase().to_string() + cs.as_str()).unwrap_or_default()
        })
        .collect::<String>()
}

fn snake_case(s: &str) -> String {
    let s: String = s.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    if s.starts_with(|c: char| c.is_ascii_digit()) { format!("_{s}") } else { s }
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(camel_case("my-api_v2"), "MyApiV2");
        assert_eq!(snake_case("My-Api"), "my_api");
        assert_eq!(regex_escape("a/b.h"), "a/b\\.h");
    }
}
