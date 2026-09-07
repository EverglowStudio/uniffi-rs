/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Erase opaque trait returns at the dyn boundary. Free and inherent functions
//! keep their original Rust signatures and allocation behavior.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{
    punctuated::Punctuated, GenericArgument, PathArguments, Signature, Token, Type, TypeParamBound,
};
use uniffi_meta::async_signature::{classify_signature, AsyncForm, ReturnSignature};

pub(super) fn variants() -> &'static [bool] {
    if cfg!(feature = "wasm-unstable-single-threaded") {
        &[false, true]
    } else {
        &[false]
    }
}

pub(super) fn target_attribute(wasm: bool) -> TokenStream {
    if !cfg!(feature = "wasm-unstable-single-threaded") {
        return quote! {};
    }
    if wasm {
        quote! { #[cfg(target_arch = "wasm32")] }
    } else {
        quote! { #[cfg(not(target_arch = "wasm32"))] }
    }
}

pub(super) fn normalize_signature(sig: &Signature, wasm: bool) -> syn::Result<Signature> {
    let declaration = classify_signature(sig)?;
    let mut output = sig.clone();
    if let syn::ReturnType::Type(_, ty) = &mut output.output {
        *ty = Box::new(erase_type(ty, sig, wasm)?);
    }
    // An async function stays async: async-trait handles its receiver lifetimes.
    // A returned Future becomes a boxed Future with the same declared bounds.
    debug_assert_eq!(declaration.rust_async, output.asyncness.is_some());
    Ok(output)
}

pub(super) fn normalized_output(sig: &Signature, wasm: bool) -> syn::Result<Type> {
    erase_type(&classify_signature(sig)?.output, sig, wasm)
}

fn erase_type(ty: &Type, sig: &Signature, wasm: bool) -> syn::Result<Type> {
    match ty {
        Type::ImplTrait(opaque) => {
            let mut bounds: Punctuated<TypeParamBound, Token![+]> = Punctuated::new();
            let mut is_future = false;
            let mut has_send = false;
            let mut has_lifetime = false;
            for bound in &opaque.bounds {
                let bound = match bound {
                    TypeParamBound::Trait(t) => {
                        let mut t = t.clone();
                        let name = t.path.segments.last().unwrap().ident.to_string();
                        if matches!(name.as_str(), "SendOutsideWasm" | "SyncOutsideWasm") {
                            if wasm {
                                continue;
                            }
                            let target = if name == "SendOutsideWasm" {
                                "Send"
                            } else {
                                "Sync"
                            };
                            t.path = syn::parse_str(target)?;
                        }
                        let name = t.path.segments.last().unwrap().ident.to_string();
                        has_send |= name == "Send";
                        is_future |= name == "Future";
                        for segment in &mut t.path.segments {
                            if let PathArguments::AngleBracketed(args) = &mut segment.arguments {
                                for arg in &mut args.args {
                                    if let GenericArgument::AssocType(assoc) = arg {
                                        assoc.ty = erase_type(&assoc.ty, sig, wasm)?;
                                    }
                                }
                            }
                        }
                        TypeParamBound::Trait(t)
                    }
                    TypeParamBound::Lifetime(lifetime) => {
                        has_lifetime = true;
                        TypeParamBound::Lifetime(lifetime.clone())
                    }
                    _ => {
                        return Err(syn::Error::new_spanned(
                            bound,
                            "unsupported bound on an exported opaque trait return",
                        ))
                    }
                };
                bounds.push(bound);
            }
            if !wasm && !has_send {
                bounds.push(syn::parse_quote!(Send));
            }
            if !has_lifetime {
                let lifetime: syn::Lifetime = if is_future
                    && sig
                        .inputs
                        .iter()
                        .any(|arg| matches!(arg, syn::FnArg::Receiver(r) if r.reference.is_some()))
                {
                    sig.inputs
                        .iter()
                        .find_map(|arg| match arg {
                            syn::FnArg::Receiver(r) => {
                                r.reference.as_ref().and_then(|(_, lt)| lt.clone())
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| syn::parse_quote!('_))
                } else {
                    syn::parse_quote!('static)
                };
                bounds.push(TypeParamBound::Lifetime(lifetime));
            }
            Ok(syn::parse_quote!(::std::pin::Pin<::std::boxed::Box<dyn #bounds>>))
        }
        Type::Path(path) => {
            let mut path = path.clone();
            for segment in &mut path.path.segments {
                if let PathArguments::AngleBracketed(args) = &mut segment.arguments {
                    for arg in &mut args.args {
                        match arg {
                            GenericArgument::Type(ty) => *ty = erase_type(ty, sig, wasm)?,
                            GenericArgument::AssocType(assoc) => {
                                assoc.ty = erase_type(&assoc.ty, sig, wasm)?
                            }
                            _ => {}
                        }
                    }
                }
            }
            Ok(Type::Path(path))
        }
        Type::TraitObject(object) => {
            let mut object = object.clone();
            for bound in &mut object.bounds {
                if let TypeParamBound::Trait(t) = bound {
                    for segment in &mut t.path.segments {
                        if let PathArguments::AngleBracketed(args) = &mut segment.arguments {
                            for arg in &mut args.args {
                                if let GenericArgument::AssocType(assoc) = arg {
                                    assoc.ty = erase_type(&assoc.ty, sig, wasm)?;
                                }
                            }
                        }
                    }
                }
            }
            Ok(Type::TraitObject(object))
        }
        _ => Ok(ty.clone()),
    }
}

/// Convert only an opaque stream return; its Item and error remain Rust types.
fn stream_output(
    declaration: &ReturnSignature,
    sig: &Signature,
    wasm: bool,
    value: TokenStream,
) -> syn::Result<TokenStream> {
    let Some(stream) = &declaration.stream else {
        return Ok(value);
    };
    if stream.form != AsyncForm::ImplTrait {
        return Ok(value);
    }
    let ty = erase_type(&stream.rust_type, sig, wasm)?;
    let boxed = quote! {{
        let uniffi_boxed: #ty = ::std::boxed::Box::pin(uniffi_stream);
        uniffi_boxed
    }};
    Ok(if declaration.throws.is_some() {
        quote! { (#value).map(|uniffi_stream| #boxed) }
    } else {
        quote! {{ let uniffi_stream = #value; #boxed }}
    })
}

pub(super) fn normalize_body(
    sig: &Signature,
    block: &syn::Block,
    wasm: bool,
) -> syn::Result<syn::Block> {
    let declaration = classify_signature(sig)?;
    let opaque_stream = declaration
        .stream
        .as_ref()
        .is_some_and(|s| s.form == AsyncForm::ImplTrait);
    if !opaque_stream
        && !declaration
            .future
            .as_ref()
            .is_some_and(|f| f.form == AsyncForm::ImplTrait)
    {
        return Ok(block.clone());
    }
    let body = if sig.asyncness.is_some() {
        let value = quote! { (async move #block).await };
        stream_output(&declaration, sig, wasm, value)?
    } else if declaration.future.is_some() && !opaque_stream {
        quote! { ::std::boxed::Box::pin((move || #block)()) }
    } else if declaration.future.is_some() {
        let value = stream_output(&declaration, sig, wasm, quote! { uniffi_future.await })?;
        // Keep the original function's synchronous work before Future creation.
        quote! {
            let uniffi_future = (move || #block)();
            ::std::boxed::Box::pin(async move { #value })
        }
    } else {
        stream_output(&declaration, sig, wasm, quote! { (move || #block)() })?
    };
    Ok(syn::parse_quote!({ #body }))
}
