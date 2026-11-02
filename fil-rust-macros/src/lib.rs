//! `#[derive(FilType)]` for C structs.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, GenericParam, Index, Lifetime, parse_macro_input};

/// Derives `fil_rust::FilType` for a struct describing a C struct.
///
/// Fields are laid out with C rules, in declaration order, whatever the
/// struct's Rust representation is (so `#[repr(C)]` isn't required). Field
/// types must implement `FilType`: integers, floats, `bool`, arrays,
/// `FilPtr`/`FilFn` and other derived structs.
///
/// A struct holding pointers takes one lifetime parameter, used for its
/// pointer fields:
///
/// ```ignore
/// #[derive(FilType)]
/// struct Named<'f> {
///     name: FilPtr<'f, c_char>,
///     len: i32,
/// }
/// ```
///
/// Its marker type (for signatures) is `Named<'static>`, and its values are
/// `Named<'f>`.
///
/// `#[fil(size = N, align = M)]` on the struct adds compile-time checks
/// against the sizes the C compiler reports.
#[proc_macro_derive(FilType, attributes(fil))]
pub fn derive_fil_type(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

struct Checks {
    size: Option<syn::Expr>,
    align: Option<syn::Expr>,
}

fn parse_checks(input: &DeriveInput) -> syn::Result<Checks> {
    let mut checks = Checks { size: None, align: None };
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("fil")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("size") {
                checks.size = Some(meta.value()?.parse()?);
                Ok(())
            } else if meta.path.is_ident("align") {
                checks.align = Some(meta.value()?.parse()?);
                Ok(())
            } else {
                Err(meta.error("expected `size` or `align`"))
            }
        })?;
    }
    Ok(checks)
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    let name = &input.ident;
    let checks = parse_checks(&input)?;

    let mut lifetime: Option<Lifetime> = None;
    for param in &input.generics.params {
        match param {
            GenericParam::Lifetime(l) if lifetime.is_none() => lifetime = Some(l.lifetime.clone()),
            GenericParam::Lifetime(l) => {
                return Err(syn::Error::new_spanned(l, "FilType structs take at most one lifetime"));
            }
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "FilType can't be derived for structs with type or const parameters",
                ));
            }
        }
    }
    if let Some(w) = &input.generics.where_clause {
        return Err(syn::Error::new_spanned(w, "FilType can't be derived with a where clause"));
    }

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new(Span::call_site(), "FilType can only be derived for structs"));
    };

    let fields: Vec<(TokenStream2, &syn::Type)> = match &data.fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let ident = f.ident.as_ref().unwrap();
                (quote!(#ident), &f.ty)
            })
            .collect(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let index = Index::from(i);
                (quote!(#index), &f.ty)
            })
            .collect(),
        Fields::Unit => Vec::new(),
    };

    let krate = quote!(::fil_rust);
    let private = quote!(#krate::__private);
    let n = fields.len();
    let fil_lt = Lifetime::new("'__fil", Span::call_site());

    let (impl_generics, ty_generics, value_ty) = match &lifetime {
        Some(lt) => (quote!(<#lt>), quote!(<#lt>), quote!(#name<#fil_lt>)),
        None => (quote!(), quote!(), quote!(#name)),
    };

    let layout_entries = fields.iter().map(|(_, ty)| {
        quote!((<#ty as #krate::FilType>::SIZE, <#ty as #krate::FilType>::ALIGN))
    });

    let reads = fields.iter().enumerate().map(|(i, (member, ty))| {
        quote! {
            #member: <#ty as #krate::FilType>::read(
                fil,
                #private::raw_add(at, Self::__FIL_LAYOUT.offsets[#i]),
            )?
        }
    });

    let writes = fields.iter().enumerate().map(|(i, (member, ty))| {
        quote! {
            <#ty as #krate::FilType>::write(
                fil,
                #private::raw_add(at, Self::__FIL_LAYOUT.offsets[#i]),
                &value.#member,
            )?;
        }
    });

    let leaves = fields.iter().enumerate().map(|(i, (_, ty))| {
        quote! {
            <#ty as #krate::FilType>::leaves(out, offset + Self::__FIL_LAYOUT.offsets[#i]);
        }
    });

    let check_ident = format_ident!("__FIL_CHECK_{}", name);
    let check_ty = match &lifetime {
        Some(_) => quote!(#name<'static>),
        None => quote!(#name),
    };
    let size_check = checks.size.map(|size| {
        quote! {
            assert!(
                <#check_ty as #krate::FilType>::SIZE == #size,
                concat!("size of ", stringify!(#name), " doesn't match #[fil(size)]"),
            );
        }
    });
    let align_check = checks.align.map(|align| {
        quote! {
            assert!(
                <#check_ty as #krate::FilType>::ALIGN == #align,
                concat!("alignment of ", stringify!(#name), " doesn't match #[fil(align)]"),
            );
        }
    });
    let checks = if size_check.is_some() || align_check.is_some() {
        quote! {
            #[allow(non_upper_case_globals)]
            const #check_ident: () = { #size_check #align_check };
        }
    } else {
        quote!()
    };

    Ok(quote! {
        impl #impl_generics #name #ty_generics {
            #[doc(hidden)]
            const __FIL_LAYOUT: #private::CLayout<#n> = #private::c_layout([#(#layout_entries),*]);
        }

        impl #impl_generics #krate::FilType for #name #ty_generics {
            type Value<#fil_lt> = #value_ty;
            const SIZE: usize = Self::__FIL_LAYOUT.size;
            const ALIGN: usize = Self::__FIL_LAYOUT.align;

            #[allow(unused_variables)]
            fn read<#fil_lt>(
                fil: &#krate::Fil<#fil_lt>,
                at: #private::RawPtr,
            ) -> #krate::Result<#value_ty> {
                ::core::result::Result::Ok(#name { #(#reads),* })
            }

            #[allow(unused_variables)]
            fn write<#fil_lt>(
                fil: &#krate::Fil<#fil_lt>,
                at: #private::RawPtr,
                value: &#value_ty,
            ) -> #krate::Result<()> {
                #(#writes)*
                ::core::result::Result::Ok(())
            }

            #[allow(unused_variables)]
            fn leaves(out: &mut ::std::vec::Vec<#private::Leaf>, offset: usize) {
                #(#leaves)*
            }
        }

        #checks
    })
}
