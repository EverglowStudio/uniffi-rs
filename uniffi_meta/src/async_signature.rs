/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared, target-independent syntax classification for macros and source parsing.
//!
//! This module recognizes standard spellings and common boxed aliases by their final
//! name. It does not resolve arbitrary user aliases or infer compiler trait bounds.
//! Every Rust type (including explicit Send, Sync and lifetime bounds) is retained.

use syn::{
    spanned::Spanned, visit::Visit, GenericArgument, PathArguments, ReturnType, Signature, Type,
    TypeParamBound,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AsyncForm {
    ImplTrait,
    PinBoxDyn,
    BoxFuture,
    LocalBoxFuture,
    BoxStream,
    LocalBoxStream,
    UniFfiStream,
}

#[derive(Clone, Debug)]
pub struct FutureSignature {
    pub rust_type: Type,
    pub form: AsyncForm,
    pub output: Type,
}

#[derive(Clone, Debug)]
pub struct StreamSignature {
    pub rust_type: Type,
    pub form: AsyncForm,
    /// Successful item payload; Option<T> remains an item, distinct from EOF.
    pub item: Type,
    /// None for Item=T. Independent from a factory's Result error.
    pub error: Option<Type>,
    /// A syntactic promise only, never an inferred or target-weakened bound.
    pub explicit_send: bool,
}

#[derive(Clone, Debug)]
pub struct ReturnSignature {
    pub rust_return: ReturnType,
    pub rust_async: bool,
    /// Explicit future returned by the Rust declaration, if any.
    pub future: Option<FutureSignature>,
    /// Zero for synchronous returns; one for async fn or an explicit Future.
    /// An async fn returning another Future is rejected, never flattened.
    pub await_count: usize,
    /// Type after the required awaits, still including the factory Result.
    pub output: Type,
    /// Successful factory return (possibly a stream). None denotes unit.
    pub return_type: Option<Type>,
    pub throws: Option<Type>,
    pub stream: Option<StreamSignature>,
}

impl ReturnSignature {
    pub fn is_async(&self) -> bool {
        self.await_count != 0
    }
}

pub fn classify_signature(sig: &Signature) -> syn::Result<ReturnSignature> {
    validate_generics(&sig.generics)?;
    classify_return(&sig.output, sig.asyncness.is_some())
}

/// Validate explicit bounds that generated foreign Future/Stream implementations
/// cannot promise. Call only for traits implemented by the host, with the actual
/// target architecture. Rust-only declarations are outside this boundary.
///
/// This only inspects return syntax, including streams inside Future Output. It
/// neither rewrites bounds nor infers Send from boxed alias names: UniFFI's
/// portable BoxFuture/BoxStream aliases deliberately omit Send on wasm.
/// SyncOutsideWasm is permitted on wasm; any target-specific rewriting belongs
/// to the generator, not this validation pass.
pub fn validate_foreign_async_bounds(sig: &Signature, wasm: bool) -> syn::Result<()> {
    struct Validator {
        wasm: bool,
        error: Option<syn::Error>,
    }
    impl<'ast> Visit<'ast> for Validator {
        fn visit_type(&mut self, ty: &'ast Type) {
            if self.error.is_some() {
                return;
            }
            let bounds = match ty {
                Type::ImplTrait(ty) => Some(&ty.bounds),
                Type::TraitObject(ty) => Some(&ty.bounds),
                _ => None,
            };
            if let Some(bounds) = bounds.filter(|bounds| {
                bounds.iter().any(|bound| {
                    matches!(bound, TypeParamBound::Trait(bound) if bound.path.segments.last()
                    .is_some_and(|segment| segment.ident == "Future" || segment.ident == "Stream"))
                })
            }) {
                for bound in bounds {
                    let TypeParamBound::Trait(bound) = bound else {
                        continue;
                    };
                    if !matches!(bound.modifier, syn::TraitBoundModifier::None) {
                        continue;
                    }
                    let Some(segment) = bound.path.segments.last() else {
                        continue;
                    };
                    let message = if segment.ident == "Sync"
                        || (!self.wasm && segment.ident == "SyncOutsideWasm")
                    {
                        Some("foreign Future/Stream implementations do not support Sync bounds")
                    } else if self.wasm && segment.ident == "Send" {
                        Some("foreign Future/Stream implementations on wasm do not support explicit Send; use SendOutsideWasm or uniffi::async_runtime::BoxFuture/BoxStream")
                    } else {
                        None
                    };
                    if let Some(message) = message {
                        self.error = Some(syn::Error::new_spanned(bound, message));
                        return;
                    }
                }
            }
            syn::visit::visit_type(self, ty);
        }
    }
    let mut validator = Validator { wasm, error: None };
    validator.visit_return_type(&sig.output);
    validator.error.map_or(Ok(()), Err)
}

pub fn validate_generics(generics: &syn::Generics) -> syn::Result<()> {
    if let Some(param) = generics
        .params
        .iter()
        .find(|p| !matches!(p, syn::GenericParam::Lifetime(_)))
    {
        return Err(syn::Error::new_spanned(
            param,
            "type and const generic UniFFI declarations are not supported",
        ));
    }
    if let Some(predicate) = generics.where_clause.as_ref().and_then(|clause| {
        clause
            .predicates
            .iter()
            .find(|p| !matches!(p, syn::WherePredicate::Lifetime(_)))
    }) {
        return Err(syn::Error::new_spanned(
            predicate,
            "type generic bounds on UniFFI declarations are not supported",
        ));
    }
    Ok(())
}

pub fn classify_return(ret: &ReturnType, rust_async: bool) -> syn::Result<ReturnSignature> {
    let declared = match ret {
        ReturnType::Default => syn::parse_quote!(()),
        ReturnType::Type(_, ty) => (**ty).clone(),
    };
    let future = classify_future(&declared)?;
    if rust_async && future.is_some() {
        return Err(syn::Error::new_spanned(
            &declared,
            "nested Future returns are not supported; await the inner future in Rust",
        ));
    }
    let output = future.as_ref().map_or(&declared, |f| &f.output).clone();
    let (ok, throws) = split_result(&output)?;
    let stream = classify_stream(&ok)?;
    if stream.is_none() {
        validate_value_type(&ok)?;
    }
    if let Some(err) = &throws {
        validate_value_type(err)?;
    }
    let return_type = if is_unit(&ok) { None } else { Some(ok) };
    Ok(ReturnSignature {
        rust_return: ret.clone(),
        rust_async,
        await_count: usize::from(rust_async || future.is_some()),
        future,
        output,
        return_type,
        throws,
        stream,
    })
}

pub fn classify_future(ty: &Type) -> syn::Result<Option<FutureSignature>> {
    let Some((form, output, _)) = classify_async_type(ty, "Future", "Output")? else {
        return Ok(None);
    };
    Ok(Some(FutureSignature {
        rust_type: ty.clone(),
        form,
        output,
    }))
}

pub fn classify_stream(ty: &Type) -> syn::Result<Option<StreamSignature>> {
    if let Some(args) = named_args(ty, "UniFfiStream") {
        let types = type_args(args, 2, 2, false)?;
        let item = types[0].clone();
        let error = types.get(1).map(|t| (*t).clone());
        validate_value_type(&item)?;
        if let Some(err) = &error {
            validate_value_type(err)?;
        }
        return Ok(Some(StreamSignature {
            rust_type: ty.clone(),
            form: AsyncForm::UniFfiStream,
            item,
            error,
            explicit_send: false,
        }));
    }
    let Some((form, item, explicit_send)) = classify_async_type(ty, "Stream", "Item")? else {
        return Ok(None);
    };
    let (item, error) = split_result(&item)?;
    validate_value_type(&item)?;
    if let Some(err) = &error {
        validate_value_type(err)?;
    }
    Ok(Some(StreamSignature {
        rust_type: ty.clone(),
        form,
        item,
        error,
        explicit_send,
    }))
}

/// Reject async values hidden in data containers and borrowed return payloads.
/// Callers also use this on record fields; direct async inputs/returns are classified first.
/// Normal named data types and their aliases are left for the existing type resolver.
pub fn validate_value_type(ty: &Type) -> syn::Result<()> {
    struct Validator(Option<syn::Error>);
    impl<'ast> Visit<'ast> for Validator {
        fn visit_type(&mut self, ty: &'ast Type) {
            if self.0.is_some() {
                return;
            }
            let message = match ungroup(ty) {
                Type::Reference(_) | Type::Ptr(_) => {
                    Some("borrowed or pointer return/data types are not supported across UniFFI")
                }
                Type::ImplTrait(_) => {
                    Some("nested Future/Stream or unsupported impl Trait in UniFFI data")
                }
                Type::Path(p)
                    if p.path.segments.last().is_some_and(|s| {
                        matches!(
                            s.ident.to_string().as_str(),
                            "Future"
                                | "Stream"
                                | "BoxFuture"
                                | "LocalBoxFuture"
                                | "BoxStream"
                                | "LocalBoxStream"
                                | "UniFfiStream"
                                | "UniFfiInputStream"
                        )
                    }) =>
                {
                    Some("nested Future/Stream in UniFFI data is not supported")
                }
                Type::TraitObject(t)
                    if t.bounds.iter().any(|b| match b {
                        TypeParamBound::Trait(t) => t
                            .path
                            .segments
                            .last()
                            .is_some_and(|s| s.ident == "Future" || s.ident == "Stream"),
                        _ => false,
                    }) =>
                {
                    Some("nested Future/Stream in UniFFI data is not supported")
                }
                _ => None,
            };
            if let Some(message) = message {
                self.0 = Some(syn::Error::new_spanned(ty, message));
            } else {
                syn::visit::visit_type(self, ty);
            }
        }
    }
    let mut validator = Validator(None);
    validator.visit_type(ty);
    validator.0.map_or(Ok(()), Err)
}

/// Only the standard two-argument Result spelling is unpacked here. The source
/// parser can still resolve its pre-existing ordinary data aliases independently.
pub fn split_result(ty: &Type) -> syn::Result<(Type, Option<Type>)> {
    match named_args(ty, "Result") {
        Some(PathArguments::AngleBracketed(args)) if args.args.len() == 1 => {
            // std::io::Result<T> and user Result<T> aliases need name resolution.
            Ok((ty.clone(), None))
        }
        Some(args) => {
            let args = type_args(args, 2, 2, false)?;
            Ok((args[0].clone(), Some(args[1].clone())))
        }
        None => Ok((ty.clone(), None)),
    }
}

fn classify_async_type(
    ty: &Type,
    trait_name: &str,
    associated: &str,
) -> syn::Result<Option<(AsyncForm, Type, bool)>> {
    let aliases = if trait_name == "Future" {
        [
            ("BoxFuture", AsyncForm::BoxFuture, true),
            ("LocalBoxFuture", AsyncForm::LocalBoxFuture, false),
        ]
    } else {
        [
            ("BoxStream", AsyncForm::BoxStream, true),
            ("LocalBoxStream", AsyncForm::LocalBoxStream, false),
        ]
    };
    for (name, form, send) in aliases {
        if let Some(args) = named_args(ty, name) {
            let args = type_args(args, 1, 1, true)?;
            return Ok(Some((form, args[0].clone(), send)));
        }
    }
    let (form, bounds) = match ungroup(ty) {
        Type::ImplTrait(t) => (AsyncForm::ImplTrait, &t.bounds),
        _ => {
            let Some(pin) = named_args(ty, "Pin") else {
                return Ok(None);
            };
            let pin = type_args(pin, 1, 1, false)?;
            let Some(boxed) = named_args(pin[0], "Box") else {
                return Ok(None);
            };
            let boxed = type_args(boxed, 1, 1, false)?;
            let Type::TraitObject(t) = ungroup(boxed[0]) else {
                return Ok(None);
            };
            (AsyncForm::PinBoxDyn, &t.bounds)
        }
    };
    let mut output = None;
    let mut send = false;
    for bound in bounds {
        let TypeParamBound::Trait(bound) = bound else {
            continue;
        };
        let Some(seg) = bound.path.segments.last() else {
            continue;
        };
        send |= seg.ident == "Send";
        if seg.ident != trait_name {
            continue;
        }
        if output.is_some()
            || bound.lifetimes.is_some()
            || !matches!(bound.modifier, syn::TraitBoundModifier::None)
        {
            return Err(syn::Error::new_spanned(
                bound,
                "unsupported async trait bound",
            ));
        }
        let PathArguments::AngleBracketed(args) = &seg.arguments else {
            return Err(syn::Error::new_spanned(
                seg,
                format!("{trait_name} must specify {associated}"),
            ));
        };
        if args.args.len() != 1 {
            return Err(syn::Error::new_spanned(
                args,
                format!("{trait_name} must specify only {associated}"),
            ));
        }
        let Some(GenericArgument::AssocType(binding)) = args.args.first() else {
            return Err(syn::Error::new_spanned(
                args,
                format!("expected {associated} = Type"),
            ));
        };
        if binding.ident != associated || binding.generics.is_some() {
            return Err(syn::Error::new_spanned(
                binding,
                format!("expected {associated} = Type"),
            ));
        }
        output = Some(binding.ty.clone());
    }
    Ok(output.map(|output| (form, output, send)))
}

fn named_args<'a>(ty: &'a Type, name: &str) -> Option<&'a PathArguments> {
    let Type::Path(path) = ungroup(ty) else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let segment = path.path.segments.last()?;
    (segment.ident == name).then_some(&segment.arguments)
}

fn type_args(
    args: &PathArguments,
    min: usize,
    max: usize,
    lifetime: bool,
) -> syn::Result<Vec<&Type>> {
    let PathArguments::AngleBracketed(args) = args else {
        return Err(syn::Error::new(args.span(), "missing async type arguments"));
    };
    let mut types = vec![];
    let mut lifetimes = 0;
    for arg in &args.args {
        match arg {
            GenericArgument::Type(ty) => types.push(ty),
            GenericArgument::Lifetime(_) if lifetime && types.is_empty() && lifetimes == 0 => {
                lifetimes += 1
            }
            _ => {
                return Err(syn::Error::new_spanned(
                    arg,
                    "unsupported async type argument",
                ))
            }
        }
    }
    if types.len() < min || types.len() > max {
        return Err(syn::Error::new_spanned(
            args,
            "incorrect number of async type arguments",
        ));
    }
    Ok(types)
}

fn ungroup(mut ty: &Type) -> &Type {
    loop {
        ty = match ty {
            Type::Paren(p) => &p.elem,
            Type::Group(g) => &g.elem,
            _ => return ty,
        };
    }
}

fn is_unit(ty: &Type) -> bool {
    matches!(ungroup(ty), Type::Tuple(t) if t.elems.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use quote::ToTokens;

    fn classify(source: &str) -> ReturnSignature {
        classify_signature(&syn::parse_str::<Signature>(source).unwrap()).unwrap()
    }

    #[test]
    fn equivalent_future_outputs_and_await_counts() {
        for source in [
            "async fn fetch() -> Result<u32, Error>",
            "fn fetch() -> impl std::future::Future<Output = Result<u32, Error>> + Send",
            "fn fetch() -> std::pin::Pin<Box<dyn Future<Output = Result<u32, Error>> + Send + Sync + 'static>>",
            "fn fetch() -> futures::future::BoxFuture<'static, Result<u32, Error>>",
            "fn fetch() -> LocalBoxFuture<'static, Result<u32, Error>>",
        ] {
            let result = classify(source);
            assert_eq!(result.await_count, 1, "{source}");
            assert_eq!(result.output, syn::parse_quote!(Result<u32, Error>));
            assert_eq!(result.return_type, Some(syn::parse_quote!(u32)));
            assert_eq!(result.throws, Some(syn::parse_quote!(Error)));
            assert!(result.stream.is_none());
        }
        assert_eq!(classify("fn fetch() -> u32").await_count, 0);
    }

    #[test]
    fn async_fn_returning_explicit_future_is_rejected() {
        for source in [
            "async fn fetch() -> impl Future<Output = u32>",
            "async fn fetch() -> Pin<Box<dyn Future<Output = u32> + Send + 'static>>",
            "async fn fetch() -> BoxFuture<'static, u32>",
            "async fn fetch() -> LocalBoxFuture<'static, u32>",
            "async fn fetch() -> impl Future<Output = Result<impl Stream<Item = u32>, Error>>",
        ] {
            let sig: Signature = syn::parse_str(source).unwrap();
            let err = classify_return(&sig.output, true).unwrap_err();
            assert_eq!(
                err.to_string(),
                "nested Future returns are not supported; await the inner future in Rust"
            );
        }
    }

    #[test]
    fn single_await_stream_factories_remain_supported() {
        for source in [
            "async fn items() -> impl Stream<Item = u32>",
            "async fn items() -> BoxStream<'static, u32>",
            "async fn items() -> Result<LocalBoxStream<'static, u32>, Error>",
            "fn items() -> impl Future<Output = impl Stream<Item = u32>>",
            "fn items() -> BoxFuture<'static, Result<BoxStream<'static, u32>, Error>>",
        ] {
            let classified = classify(source);
            assert_eq!(classified.await_count, 1);
            assert!(classified.stream.is_some(), "{source}");
        }
    }

    #[test]
    fn preserves_rust_bounds_and_form() {
        let source: Signature = syn::parse_quote!(fn fetch() -> Pin<Box<dyn Future<Output = u32> + Send + Sync + 'static>>);
        let result = classify_signature(&source).unwrap();
        assert_eq!(result.rust_return, source.output);
        let future = result.future.unwrap();
        assert_eq!(future.form, AsyncForm::PinBoxDyn);
        assert_eq!(
            future.rust_type.to_token_stream().to_string(),
            "Pin < Box < dyn Future < Output = u32 > + Send + Sync + 'static > >"
        );
        let stream = classify("fn items() -> LocalBoxStream<'static, u32>")
            .stream
            .unwrap();
        assert!(!stream.explicit_send);
        assert_eq!(stream.form, AsyncForm::LocalBoxStream);
    }

    #[test]
    fn stream_factories_separate_item_and_factory_errors() {
        for source in [
            "async fn items() -> Result<impl Stream<Item = Result<Option<u32>, ItemError>>, FactoryError>",
            "fn items() -> impl Future<Output = Result<Pin<Box<dyn Stream<Item = Result<Option<u32>, ItemError>> + Send + 'static>>, FactoryError>>",
            "fn items() -> BoxFuture<'static, Result<BoxStream<'static, Result<Option<u32>, ItemError>>, FactoryError>>",
            "fn items() -> impl Future<Output = Result<UniFfiStream<Option<u32>, ItemError>, FactoryError>>",
        ] {
            let result = classify(source);
            assert_eq!(result.await_count, 1);
            assert_eq!(result.throws, Some(syn::parse_quote!(FactoryError)));
            let stream = result.stream.unwrap();
            assert_eq!(stream.item, syn::parse_quote!(Option<u32>));
            assert_eq!(stream.error, Some(syn::parse_quote!(ItemError)));
        }
    }

    #[test]
    fn infallible_items_and_unit_output_are_distinct() {
        for source in [
            "fn items() -> impl Stream<Item = Option<u32>>",
            "fn items() -> Pin<Box<dyn Stream<Item = Option<u32>> + Send>>",
            "fn items() -> BoxStream<'static, Option<u32>>",
            "fn items() -> LocalBoxStream<'static, Option<u32>>",
        ] {
            let result = classify(source);
            assert!(!result.is_async());
            let stream = result.stream.unwrap();
            assert_eq!(stream.item, syn::parse_quote!(Option<u32>));
            assert!(stream.error.is_none());
        }
        assert_eq!(
            classify("fn items() -> UniFfiStream<u32, ()>")
                .stream
                .unwrap()
                .error,
            Some(syn::parse_quote!(()))
        );
        assert!(classify("fn finish() -> impl Future<Output = ()>")
            .return_type
            .is_none());
        assert!(classify("async fn finish()").return_type.is_none());
    }

    #[test]
    fn borrowed_inputs_and_future_lifetimes_are_preserved() {
        for source in [
            "fn fetch<'a>(&'a self) -> BoxFuture<'a, u32>",
            "fn fetch<'a>(input: &'a str) -> LocalBoxFuture<'a, u32>",
            "fn fetch<'a: 'b, 'b>(input: &'a str) -> BoxFuture<'b, u32> where 'a: 'b",
        ] {
            let sig: Signature = syn::parse_str(source).unwrap();
            let classified = classify_signature(&sig).unwrap();
            assert_eq!(classified.await_count, 1);
            assert_eq!(classified.rust_return, sig.output);
            assert_eq!(classified.return_type, Some(syn::parse_quote!(u32)));
        }
        let sig: Signature =
            syn::parse_quote!(fn bad<'a>(input: &'a str) -> BoxFuture<'a, &'a str>);
        assert!(classify_signature(&sig).is_err());
    }

    #[test]
    fn unsupported_declarations_have_spanned_errors() {
        for source in [
            "fn bad<T>() -> T",
            "fn bad() -> u32 where Self: Send",
            "fn bad() -> &str",
            "fn bad() -> impl Future<Output = &str>",
            "fn bad() -> impl Future<Output = impl Future<Output = u32>>",
            "fn bad() -> impl Stream<Item = impl Stream<Item = u32>>",
            "fn bad() -> impl Stream<Item = Result<impl Future<Output = u32>, Error>>",
            "fn bad() -> Option<BoxStream<'static, u32>>",
            "fn bad() -> Result<BoxFuture<'static, u32>, Error>",
            "fn bad() -> impl Future",
            "fn bad() -> impl Future<Wrong = u32>",
            "fn bad() -> BoxFuture<'static, u32, u64>",
            "fn bad() -> BoxStream<'static, &str>",
            "fn bad() -> impl Stream<Item = u32, Extra = bool>",
        ] {
            let sig = syn::parse_str::<Signature>(source).unwrap();
            let err = classify_signature(&sig).expect_err(source);
            assert!(!err.to_compile_error().is_empty(), "{source}");
        }
    }

    #[test]
    fn foreign_async_sync_bounds_are_rejected_on_both_targets() {
        for source in [
            "fn run(&self) -> impl Future<Output = u32> + Sync",
            "fn run(&self) -> Pin<Box<dyn Future<Output = u32> + std::marker::Sync>>",
            "fn run(&self) -> impl Stream<Item = u32> + Sync",
            "async fn run(&self) -> Result<Pin<Box<dyn Stream<Item = u32> + Sync>>, Error>",
            "fn run(&self) -> BoxFuture<'static, Result<impl Stream<Item = u32> + Sync, Error>>",
        ] {
            let sig: Signature = syn::parse_str(source).unwrap();
            for wasm in [false, true] {
                let err = validate_foreign_async_bounds(&sig, wasm).unwrap_err();
                assert_eq!(
                    err.to_string(),
                    "foreign Future/Stream implementations do not support Sync bounds"
                );
            }
        }
    }

    #[test]
    fn foreign_async_explicit_send_is_rejected_only_on_wasm() {
        for source in [
            "fn run(&self) -> impl Future<Output = u32> + Send",
            "fn run(&self) -> Pin<Box<dyn Future<Output = u32> + std::marker::Send + 'static>>",
            "async fn run(&self) -> impl Stream<Item = u32> + Send",
            "fn run(&self) -> impl Future<Output = Result<Pin<Box<dyn Stream<Item = u32> + Send>>, Error>>",
            "fn run(&self) -> uniffi::async_runtime::BoxFuture<'static, Result<impl Stream<Item = u32> + Send, Error>>",
        ] {
            let sig: Signature = syn::parse_str(source).unwrap();
            validate_foreign_async_bounds(&sig, false).unwrap();
            let err = validate_foreign_async_bounds(&sig, true).unwrap_err();
            assert!(err.to_string().contains("explicit Send"));
            assert!(err.to_string().contains("SendOutsideWasm or uniffi::async_runtime::BoxFuture/BoxStream"));
        }
    }

    #[test]
    fn foreign_async_portable_markers_and_aliases_are_not_inferred_or_rewritten() {
        for source in [
            "fn run(&self) -> impl Future<Output = u32> + SendOutsideWasm",
            "fn run(&self) -> impl Stream<Item = u32> + uniffi::async_runtime::SendOutsideWasm",
            "fn run(&self) -> uniffi::async_runtime::BoxFuture<'static, u32>",
            "fn run(&self) -> uniffi::async_runtime::BoxStream<'static, u32>",
            "fn run(&self) -> BoxFuture<'static, Result<BoxStream<'static, u32>, Error>>",
            "fn run(&self) -> LocalBoxFuture<'static, LocalBoxStream<'static, u32>>",
        ] {
            let sig: Signature = syn::parse_str(source).unwrap();
            let original = sig.clone();
            for wasm in [false, true] {
                validate_foreign_async_bounds(&sig, wasm).unwrap();
            }
            assert_eq!(sig, original);
        }
        for source in [
            "fn run(&self) -> impl Future<Output = u32> + SendOutsideWasm + SyncOutsideWasm",
            "fn run(&self) -> BoxFuture<'static, impl Stream<Item = u32> + uniffi::async_runtime::SyncOutsideWasm>",
        ] {
            let sig: Signature = syn::parse_str(source).unwrap();
            let original = sig.clone();
            assert!(validate_foreign_async_bounds(&sig, false).unwrap_err().to_string().contains("Sync bounds"));
            validate_foreign_async_bounds(&sig, true).unwrap();
            assert_eq!(sig, original);
        }
    }

    #[test]
    fn foreign_async_validation_does_not_restrict_inputs_or_data_bounds() {
        for source in [
            "fn run(&self, input: impl Future<Output = u32> + Send + Sync) -> u32",
            "fn run(&self) -> BoxFuture<'static, Arc<dyn Data + Send + Sync>>",
            "fn run(&self) -> Arc<dyn Data + Send + Sync>",
        ] {
            let sig: Signature = syn::parse_str(source).unwrap();
            for wasm in [false, true] {
                validate_foreign_async_bounds(&sig, wasm).unwrap();
            }
        }
    }
}
