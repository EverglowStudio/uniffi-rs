/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use proc_macro2::{Ident, TokenStream};
use quote::{format_ident, quote};
use std::iter;

use super::attributes::AsyncRuntime;
use crate::{
    ffiops,
    fnsig::{FnKind, FnSignature, ReceiverArg},
};

fn future_constructor(output: &TokenStream, ar: Option<&AsyncRuntime>) -> TokenStream {
    match ar {
        Some(AsyncRuntime::Tokio(_)) => quote! { ::uniffi::rust_future_new_tokio::<_, #output, _> },
        None => {
            #[cfg(feature = "default-async-runtime-tokio")]
            {
                quote! {{
                    #[cfg(not(target_arch = "wasm32"))]
                    { ::uniffi::rust_future_new_tokio::<_, #output, _> }
                    #[cfg(target_arch = "wasm32")]
                    { ::uniffi::rust_future_new::<_, #output, _> }
                }}
            }
            #[cfg(not(feature = "default-async-runtime-tokio"))]
            {
                quote! { ::uniffi::rust_future_new::<_, #output, _> }
            }
        }
    }
}

/// Other Rust scaffolding backends pass the complete call driver here. The
/// source macro knows the resolved export attribute and convenience feature;
/// metadata consumers need not reconstruct either from a business signature.
pub(super) fn gen_async_runtime_helper(
    sig: &FnSignature,
    ar: Option<&AsyncRuntime>,
) -> syn::Result<TokenStream> {
    if !sig.is_async {
        return Ok(TokenStream::new());
    }
    let ident = format_ident!("{}_runtime", sig.scaffolding_fn_ident()?);
    let body = match ar {
        Some(AsyncRuntime::Tokio(_)) => {
            quote! { ::uniffi::deps::async_compat::Compat::new(future) }
        }
        None => {
            #[cfg(feature = "default-async-runtime-tokio")]
            {
                quote! {
                    #[cfg(not(target_arch = "wasm32"))]
                    { ::uniffi::deps::async_compat::Compat::new(future) }
                    #[cfg(target_arch = "wasm32")]
                    { future }
                }
            }
            #[cfg(not(feature = "default-async-runtime-tokio"))]
            {
                quote! { future }
            }
        }
    };
    Ok(quote! {
        #[doc(hidden)]
        pub fn #ident<F: ::std::future::Future>(future: F) -> impl ::std::future::Future<Output = F::Output> {
            #body
        }
    })
}

pub(super) fn gen_fn_scaffolding(
    sig: FnSignature,
    ar: Option<&AsyncRuntime>,
    udl_mode: bool,
) -> syn::Result<TokenStream> {
    if sig.receiver.is_some() {
        return Err(syn::Error::new(
            sig.span,
            "Unexpected self param (Note: uniffi::export must be used on the impl block, not its containing fn's)"
        ));
    }
    if !sig.is_async {
        if let Some(async_runtime) = ar {
            return Err(syn::Error::new_spanned(
                async_runtime,
                "this attribute is only allowed on async functions",
            ));
        }
    }
    let metadata_items = (!udl_mode).then(|| {
        sig.metadata_items()
            .unwrap_or_else(syn::Error::into_compile_error)
    });
    let scaffolding_func = gen_ffi_function(&sig, ar, udl_mode, None)?;
    Ok(quote! {
        #scaffolding_func
        #metadata_items
    })
}

pub(super) fn gen_constructor_scaffolding(
    sig: FnSignature,
    ar: Option<&AsyncRuntime>,
    udl_mode: bool,
) -> syn::Result<TokenStream> {
    if sig.receiver.is_some() {
        return Err(syn::Error::new(
            sig.span,
            "constructors must not have a self parameter",
        ));
    }
    let metadata_items = (!udl_mode).then(|| {
        sig.metadata_items()
            .unwrap_or_else(syn::Error::into_compile_error)
    });
    let scaffolding_func = gen_ffi_function(&sig, ar, udl_mode, None)?;
    Ok(quote! {
        #scaffolding_func
        #metadata_items
    })
}

pub(super) fn gen_method_scaffolding(
    sig: FnSignature,
    ar: Option<&AsyncRuntime>,
    udl_mode: bool,
    use_trait: Option<&syn::Path>,
) -> syn::Result<TokenStream> {
    let ar = sig.async_runtime.as_ref().or(ar);
    let local_helper = if sig.is_local {
        gen_local_method(&sig, use_trait)?
    } else {
        TokenStream::new()
    };
    let scaffolding_func = if sig.receiver.is_none() {
        return Err(syn::Error::new(
            sig.span,
            "associated functions are not currently supported",
        ));
    } else {
        gen_ffi_function(&sig, ar, udl_mode, use_trait)?
    };

    let metadata_items = (!udl_mode).then(|| {
        sig.metadata_items()
            .unwrap_or_else(syn::Error::into_compile_error)
    });
    Ok(quote! {
        #local_helper
        #scaffolding_func
        #metadata_items
    })
}

/// The helper takes owned arguments; any Rust borrows are made inside the owner.
pub(super) fn gen_local_method(
    sig: &FnSignature,
    use_trait: Option<&syn::Path>,
) -> syn::Result<TokenStream> {
    if !matches!(sig.require_receiver()?, ReceiverArg::Ref) {
        return Err(syn::Error::new(
            sig.span,
            "local methods must take &self; the object body never leaves its owner",
        ));
    }
    let FnKind::Method { self_ident, .. } = &sig.kind else {
        return Err(syn::Error::new(
            sig.span,
            "local export requires an inherent method",
        ));
    };
    let helper = local_helper_ident(sig);
    let method = &sig.ident;
    let return_ty = &sig.return_ty;
    let names: Vec<_> = (0..sig.args.len())
        .map(|i| format_ident!("__uniffi_arg_{i}"))
        .collect();
    let types = sig.args.iter().map(|arg| &arg.ty);
    let args = sig.rust_call_params(false);
    let mut call = match use_trait {
        Some(trait_path) => quote! { <#self_ident as #trait_path>::#method(__uniffi_body, #args) },
        None => quote! { __uniffi_body.#method(#args) },
    };
    for _ in 0..sig.rust_await_count {
        call = quote! { (#call).await };
    }
    let flatten = sig.stream_return.is_some().then(|| quote! { ? });
    if let Some(stream) = &sig.stream_return {
        let boxed = stream.box_stream(quote! { uniffi_stream });
        let local = quote! { ::uniffi::async_runtime::local_stream(#boxed) };
        call = if sig.factory_error_ty.is_some() {
            quote! {
                match #call {
                    Ok(uniffi_stream) => #local.map(Ok),
                    Err(error) => Ok(Err(error)),
                }
            }
        } else {
            quote! {{ let uniffi_stream = #call; #local }}
        };
    }
    Ok(quote! {
        impl #self_ident {
            #[doc(hidden)]
            pub async fn #helper(
                __uniffi_proxy: ::std::sync::Arc<::uniffi::async_runtime::LocalObject<Self>>,
                #(#names: #types,)*
            ) -> ::std::result::Result<#return_ty, ::uniffi::async_runtime::LocalError> {
                let uniffi_args = (#(#names,)*);
                __uniffi_proxy.call(move |__uniffi_body| ::std::boxed::Box::pin(async move {
                    #call
                })).await #flatten
            }
        }
    })
}

fn local_helper_ident(sig: &FnSignature) -> Ident {
    format_ident!(
        "__uniffi_local_{}",
        crate::util::ident_to_string(&sig.ident)
    )
}

/// Every local object exposes a repeatable close operation independent of its
/// business methods. Closing the proxy waits for owner-side destruction.
pub(crate) fn gen_local_close(
    self_ident: &Ident,
    foreign_name: &str,
    udl_mode: bool,
) -> syn::Result<TokenStream> {
    let mut sig = FnSignature::new_method(
        self_ident.clone(),
        Ident::new(foreign_name, self_ident.span()),
        syn::parse_quote! { async fn close(&self) },
        super::attributes::ExportFnArgs::default(),
        "Cancel this local object's calls and wait for owner cleanup and destruction.".into(),
    )?;
    sig.is_local = true;
    let scaffolding = gen_ffi_function(&sig, None, udl_mode, None)?;
    let metadata = (!udl_mode).then(|| sig.metadata_items()).transpose()?;
    let helper = gen_local_close_helper(self_ident);
    Ok(quote! {
        #helper
        #scaffolding
        #metadata
    })
}

pub(crate) fn gen_local_close_helper(self_ident: &Ident) -> TokenStream {
    quote! {
        impl #self_ident {
            #[doc(hidden)]
            pub async fn __uniffi_local_close(
                proxy: ::std::sync::Arc<::uniffi::async_runtime::LocalObject<Self>>,
            ) -> ::std::result::Result<(), ::uniffi::async_runtime::LocalError> {
                proxy.close().await
            }
        }
    }
}

#[cfg(not(feature = "scaffolding"))]
pub(crate) fn gen_local_close_without_scaffolding(
    self_ident: &Ident,
    foreign_name: &str,
) -> syn::Result<TokenStream> {
    let sig = FnSignature::new_method(
        self_ident.clone(),
        Ident::new(foreign_name, self_ident.span()),
        syn::parse_quote! { async fn close(&self) },
        super::attributes::ExportFnArgs::default(),
        String::new(),
    )?;
    let helper = gen_local_close_helper(self_ident);
    let runtime = gen_async_runtime_helper(&sig, None)?;
    Ok(quote! { #helper #runtime })
}

// Pieces of code for the scaffolding function
struct ScaffoldingBits {
    /// Parameter names for the scaffolding function
    param_names: Vec<TokenStream>,
    /// Parameter types for the scaffolding function
    param_types: Vec<TokenStream>,
    /// Lift closure.  See `FnSignature::lift_closure` for an explanation of this.
    lift_closure: TokenStream,
    /// Expression to call the Rust function after a successful lift.
    rust_fn_call: TokenStream,
    /// Convert the result of `rust_fn_call`, stored in a variable named `uniffi_result` into its final value.
    /// This is used to do things like error conversion / Arc wrapping
    convert_result: TokenStream,
}

impl ScaffoldingBits {
    fn new_for_function(sig: &FnSignature, udl_mode: bool) -> Self {
        let ident = &sig.ident;
        let call_params = sig.rust_call_params(false);
        let rust_fn_call = quote! { #ident(#call_params) };
        // UDL mode adds an extra conversion (#1749)
        let convert_result = if udl_mode && sig.looks_like_result {
            quote! { uniffi_result.map_err(::std::convert::Into::into) }
        } else {
            quote! { uniffi_result }
        };

        Self {
            param_names: sig.scaffolding_param_names().collect(),
            param_types: sig.scaffolding_param_types().collect(),
            lift_closure: sig.lift_closure(None),
            rust_fn_call,
            convert_result,
        }
    }

    fn new_for_method(
        sig: &FnSignature,
        self_ident: &Ident,
        is_trait: bool,
        udl_mode: bool,
    ) -> syn::Result<Self> {
        let ident = &sig.ident;
        let self_type = if is_trait {
            quote! { dyn #self_ident }
        } else if sig.is_local {
            quote! { ::uniffi::async_runtime::LocalObject<#self_ident> }
        } else {
            quote! { #self_ident }
        };

        let ref_type = ffiops::lift_ref_type(&self_type);
        let lift_type = ffiops::lift_type(&ref_type);
        let try_lift = ffiops::try_lift(&ref_type);

        let lift_closure = sig.lift_closure(Some(quote! {
            match #try_lift(uniffi_self_lowered) {
                ::std::result::Result::Ok(v) => v,
                ::std::result::Result::Err(e) => {
                    return ::std::result::Result::Err(("self", e));
                }
            }
        }));
        let call_params = sig.rust_call_params(true);
        let rust_fn_call = if sig.is_local {
            let helper = local_helper_ident(sig);
            let params = (1..=sig.args.len()).map(|i| {
                let idx = syn::Index::from(i);
                quote! { uniffi_args.#idx }
            });
            quote! { #self_ident::#helper(uniffi_args.0, #(#params),*) }
        } else if is_trait {
            // For traits use the fully-qualified function name to disambiguate
            let receiver_expr = match sig.require_receiver()? {
                ReceiverArg::Ref => quote! { &*uniffi_args.0 },
                ReceiverArg::Arc => quote! { uniffi_args.0 },
            };
            quote! { <dyn #self_ident as #self_ident>::#ident(#receiver_expr, #call_params) }
        } else {
            // For non-traits use method call syntax, which papers over differences between Arc<T>
            // and T.  Inherent methods always take precedence over other functions
            quote! { uniffi_args.0.#ident(#call_params) }
        };
        // UDL mode adds an extra conversion (#1749)
        let convert_result = if udl_mode && sig.looks_like_result {
            quote! { uniffi_result .map_err(::std::convert::Into::into) }
        } else {
            quote! { uniffi_result }
        };

        Ok(Self {
            param_names: iter::once(quote! { uniffi_self_lowered })
                .chain(sig.scaffolding_param_names())
                .collect(),
            param_types: iter::once(quote! { #lift_type })
                .chain(sig.scaffolding_param_types())
                .collect(),
            lift_closure,
            rust_fn_call,
            convert_result,
        })
    }

    fn new_for_constructor(sig: &FnSignature, self_ident: &Ident, udl_mode: bool) -> Self {
        let ident = &sig.ident;
        let call_params = sig.rust_call_params(false);
        let rust_fn_call = quote! { #self_ident::#ident(#call_params) };
        // UDL mode adds extra conversions (#1749)
        let convert_result = match (udl_mode, sig.looks_like_result) {
            // For UDL
            (true, false) => quote! { ::std::sync::Arc::new(uniffi_result) },
            (true, true) => {
                quote! { uniffi_result.map(::std::sync::Arc::new).map_err(::std::convert::Into::into) }
            }
            (false, _) => quote! { uniffi_result },
        };

        Self {
            param_names: sig.scaffolding_param_names().collect(),
            param_types: sig.scaffolding_param_types().collect(),
            lift_closure: sig.lift_closure(None),
            rust_fn_call,
            convert_result,
        }
    }
}

/// Generate a scaffolding function
///
/// `pre_fn_call` is the statements that we should execute before the rust call
/// `rust_fn` is the Rust function to call.
pub(super) fn gen_ffi_function(
    sig: &FnSignature,
    ar: Option<&AsyncRuntime>,
    udl_mode: bool,
    use_trait: Option<&syn::Path>,
) -> syn::Result<TokenStream> {
    let runtime_adapter = gen_async_runtime_helper(sig, ar)?;
    let ScaffoldingBits {
        param_names,
        param_types,
        lift_closure,
        rust_fn_call,
        convert_result,
    } = match &sig.kind {
        FnKind::Function => ScaffoldingBits::new_for_function(sig, udl_mode),
        FnKind::Method { self_ident, .. } => {
            ScaffoldingBits::new_for_method(sig, self_ident, false, udl_mode)?
        }
        FnKind::TraitMethod { self_ident, .. } => {
            ScaffoldingBits::new_for_method(sig, self_ident, true, udl_mode)?
        }
        FnKind::Constructor { self_ident, .. } => {
            ScaffoldingBits::new_for_constructor(sig, self_ident, udl_mode)
        }
    };

    let ffi_ident = sig.scaffolding_fn_ident()?;
    let ffi_fn_name = ffi_ident.to_string();
    let name = &sig.name;
    let return_ty = &sig.return_ty;
    let use_trait = use_trait
        .filter(|_| !sig.is_local)
        .map(|tr| quote! { use #tr; });
    let input_stream_callback_registrations = gen_input_stream_callback_registrations(sig);
    if sig.stream_return.is_some() {
        let stream_scaffolding = gen_stream_ffi_function(
            sig,
            ar,
            use_trait,
            ScaffoldingBits {
                param_names,
                param_types,
                lift_closure,
                rust_fn_call,
                convert_result,
            },
        )?;
        return Ok(quote! { #runtime_adapter #stream_scaffolding });
    }
    let ffi_return_ty = ffiops::lower_return_type(return_ty);
    let lower_return = ffiops::lower_return(return_ty);
    let handle_failed_lift = ffiops::lower_return_handle_failed_lift(return_ty);

    let scaffolding = if !sig.is_async {
        let scaffolding_fn_ffi_buffer_version =
            ffi_buffer_scaffolding_fn(&ffi_ident, &ffi_return_ty, &param_types, true);
        quote! {
            #input_stream_callback_registrations

            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #ffi_ident(
                #(#param_names: #param_types,)*
                call_status: &mut ::uniffi::RustCallStatus,
            ) -> #ffi_return_ty {
                #use_trait
                ::uniffi::deps::trace!("calling: {}", #ffi_fn_name);
                let uniffi_lift_args = #lift_closure;
                ::uniffi::rust_call(call_status, || {
                    let result = match uniffi_lift_args() {
                        ::std::result::Result::Ok(uniffi_args) => {
                            ::uniffi::deps::trace!("lift_args success: {}", #ffi_fn_name);
                            let uniffi_result = #rust_fn_call;
                            ::uniffi::deps::trace!("call success: {}", #ffi_fn_name);
                            let uniffi_lowered_return = #lower_return(#convert_result);
                            ::uniffi::deps::trace!("lower_return success: {}", #ffi_fn_name);
                            uniffi_lowered_return
                        }
                        ::std::result::Result::Err((arg_name, error)) => {
                            ::uniffi::deps::trace!("lift_args error: {}", #ffi_fn_name);
                            #handle_failed_lift(::uniffi::LiftArgsError { arg_name, error} )
                        },
                    };
                    result
                })
            }

            #scaffolding_fn_ffi_buffer_version
        }
    } else {
        let future_expr = rust_fn_call;
        let future_output = if sig.is_local {
            quote! { ::uniffi::InternalCallResult<#return_ty> }
        } else {
            quote! { #return_ty }
        };
        let new_future = future_constructor(&future_output, ar);
        let async_result = if sig.is_local {
            quote! {
                ::uniffi::InternalCallResult(
                    uniffi_result.map(|uniffi_result| #convert_result)
                        .map_err(::uniffi::RustCallError::from)
                )
            }
        } else {
            convert_result
        };
        let scaffolding_fn_ffi_buffer_version =
            ffi_buffer_scaffolding_fn(&ffi_ident, &quote! { ::uniffi::Handle}, &param_types, false);

        quote! {
            #input_stream_callback_registrations

            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #ffi_ident(#(#param_names: #param_types,)*) -> ::uniffi::Handle {
                ::uniffi::deps::trace!("calling: {}", #name);
                let uniffi_lifted_args = (#lift_closure)();
                (#new_future)(
                    async move {
                        match uniffi_lifted_args {
                            ::std::result::Result::Ok(uniffi_args) => {
                                let uniffi_result = #future_expr.await;
                                Ok(#async_result)
                            }
                            ::std::result::Result::Err((arg_name, error)) => {
                                Err(::uniffi::LiftArgsError { arg_name, error })
                            },
                        }
                    },
                    crate::UniFfiTag
                )
            }

            #scaffolding_fn_ffi_buffer_version
        }
    };
    Ok(quote! { #runtime_adapter #scaffolding })
}

fn gen_stream_ffi_function(
    sig: &FnSignature,
    ar: Option<&AsyncRuntime>,
    use_trait: Option<TokenStream>,
    bits: ScaffoldingBits,
) -> syn::Result<TokenStream> {
    let stream = sig.stream_return.as_ref().unwrap();
    let item = &stream.item_ty;
    let error = &stream.error_ty;
    let ffi_ident = sig.scaffolding_fn_ident()?;
    let ffi_name = ffi_ident.to_string();
    let next = format_ident!("{}_stream_next", ffi_name);
    let cancel = format_ident!("{}_stream_cancel", ffi_name);
    let close = format_ident!("{}_stream_close", ffi_name);
    let registry = format_ident!("__UNIFFI_STREAM_REGISTRY_{}", ffi_name.to_ascii_uppercase());
    let ScaffoldingBits {
        param_names,
        param_types,
        lift_closure,
        rust_fn_call,
        convert_result,
    } = bits;
    let output = quote! { ::uniffi::RustStreamOutput<#item, #error> };
    let output = match &sig.factory_error_ty {
        Some(factory_error) => quote! { ::std::result::Result<#output, #factory_error> },
        None => output,
    };
    // A local helper has already converted its stream on owner before transfer.
    let stream_value = if sig.is_local {
        quote! { uniffi_stream }
    } else {
        stream.box_stream(quote! { uniffi_stream })
    };
    let converted = quote! { ::uniffi::RustStreamOutput::new(&#registry, #stream_value) };
    let converted = if sig.factory_error_ty.is_some() {
        quote! { uniffi_result.map(|uniffi_stream| #converted) }
    } else {
        quote! {{ let uniffi_stream = uniffi_result; #converted }}
    };
    let (output, converted) = if sig.is_local {
        (
            quote! { ::uniffi::InternalCallResult<#output> },
            quote! {
                ::uniffi::InternalCallResult(uniffi_result.map(|uniffi_result| #converted)
                    .map_err(::uniffi::RustCallError::from))
            },
        )
    } else {
        (output, converted)
    };
    let input_registrations = gen_input_stream_callback_registrations(sig);
    let function = if sig.is_async {
        let future = rust_fn_call;
        let new_future = future_constructor(&output, ar);
        let buffer = ffi_buffer_scaffolding_fn(
            &ffi_ident,
            &quote! { ::uniffi::Handle },
            &param_types,
            false,
        );
        quote! {
            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #ffi_ident(#(#param_names: #param_types,)*) -> ::uniffi::Handle {
                #use_trait
                let args = (#lift_closure)();
                (#new_future)(async move {
                    match args {
                        Ok(uniffi_args) => {
                            let uniffi_result = #future.await;
                            let uniffi_result = #convert_result;
                            Ok(#converted)
                        }
                        Err((arg_name, error)) => Err(::uniffi::LiftArgsError { arg_name, error }),
                    }
                }, crate::UniFfiTag)
            }
            #buffer
        }
    } else {
        let lower = ffiops::lower_return(&output);
        let failed_lift = ffiops::lower_return_handle_failed_lift(&output);
        let buffer =
            ffi_buffer_scaffolding_fn(&ffi_ident, &quote! { ::uniffi::Handle }, &param_types, true);
        quote! {
            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #ffi_ident(#(#param_names: #param_types,)* call_status: &mut ::uniffi::RustCallStatus) -> ::uniffi::Handle {
                #use_trait
                ::uniffi::rust_call(call_status, || {
                    match (#lift_closure)() {
                        Ok(uniffi_args) => {
                            let uniffi_result = #rust_fn_call;
                            let uniffi_result = #convert_result;
                            #lower(#converted)
                        }
                        Err((arg_name, error)) => #failed_lift(::uniffi::LiftArgsError { arg_name, error }),
                    }
                })
            }
            #buffer
        }
    };
    Ok(quote! {
        #input_registrations
        #[doc(hidden)]
        static #registry: ::uniffi::RustStreamRegistry<#item, #error> =
            ::uniffi::deps::once_cell::sync::Lazy::new(::std::default::Default::default);
        #function
        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn #next(handle: ::uniffi::Handle) -> ::uniffi::Handle {
            ::uniffi::rust_stream_next::<#item, #error, crate::UniFfiTag>(&#registry, handle, crate::UniFfiTag)
        }
        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn #cancel(handle: ::uniffi::Handle) {
            ::uniffi::rust_stream_cancel::<#item, #error>(&#registry, handle)
        }
        #[doc(hidden)]
        #[unsafe(no_mangle)]
        pub extern "C" fn #close(handle: ::uniffi::Handle) -> ::uniffi::Handle {
            ::uniffi::rust_stream_close::<#item, #error, _>(&#registry, handle, crate::UniFfiTag)
        }
    })
}

fn gen_input_stream_callback_registrations(sig: &FnSignature) -> TokenStream {
    let registrations = sig.args.iter().filter_map(|arg| {
        let input_stream = arg.input_stream.as_ref()?;
        let cell_ident = sig.input_stream_callback_cell_ident(arg);
        let init_ident = sig.input_stream_init_fn_ident(arg);
        let next_ty = FnSignature::input_stream_next_callback_type(input_stream);
        let tuple_ty = FnSignature::input_stream_callback_tuple_type(input_stream);
        Some(quote! {
            #[doc(hidden)]
            static #cell_ident: ::uniffi::deps::once_cell::sync::OnceCell<#tuple_ty> =
                ::uniffi::deps::once_cell::sync::OnceCell::new();

            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #init_ident(
                next: #next_ty,
                cancel: ::uniffi::ForeignInputStreamCancelCallback,
                close: ::uniffi::ForeignInputStreamCloseCallback,
            ) {
                let _ = #cell_ident.set((next, cancel, close));
            }
        })
    });
    quote! {
        #(#registrations)*
    }
}

#[cfg(test)]
mod tests {
    use super::{future_constructor, gen_ffi_function};
    use crate::{
        export::{AsyncRuntime, ExportFnArgs},
        fnsig::{FnKind, FnSignature},
    };
    use proc_macro2::Span;
    use quote::quote;
    use syn::LitStr;

    #[test]
    fn explicit_tokio_async_runtime_wraps_entire_call() {
        let runtime = AsyncRuntime::Tokio(LitStr::new("tokio", Span::call_site()));
        let tokens = future_constructor(&quote! { u32 }, Some(&runtime)).to_string();
        assert!(tokens.contains("rust_future_new_tokio"));
        assert!(tokens.contains("u32"));
    }

    #[cfg(feature = "default-async-runtime-tokio")]
    #[test]
    fn default_tokio_feature_selects_non_wasm_only() {
        let tokens = future_constructor(&quote! { u32 }, None).to_string();
        assert!(tokens.contains("rust_future_new_tokio"));
        assert!(tokens.contains("target_arch"));
        assert!(tokens.contains("wasm32"));
        assert!(tokens.contains("u32"));
    }

    #[cfg(not(feature = "default-async-runtime-tokio"))]
    #[test]
    fn without_feature_default_async_runtime_is_unchanged() {
        let tokens = future_constructor(&quote! { u32 }, None).to_string();
        assert!(!tokens.contains("rust_future_new_tokio"));
        assert_eq!(
            tokens,
            quote! { ::uniffi::rust_future_new::<_, u32, _> }.to_string()
        );
    }

    #[test]
    fn input_stream_scaffolding_uses_registered_callbacks() {
        let item: syn::ItemFn = syn::parse_quote! {
            async fn sum_events(
                events: uniffi::UniFfiInputStream<u32, MyError>
            ) -> Result<u64, MyError> {
                unreachable!()
            }
        };
        let sig = FnSignature::new(
            FnKind::Function,
            item.sig,
            ExportFnArgs::default(),
            String::new(),
        )
        .unwrap();
        let tokens = gen_ffi_function(&sig, None, false, None)
            .unwrap()
            .to_string();

        assert!(tokens.contains("input_stream_events_init"));
        assert!(tokens.contains("UNIFFI_INPUT_STREAM_CALLBACKS"));
        assert!(tokens.contains("OnceCell"));
        assert!(tokens.contains("from_foreign_callbacks"));
        assert!(!tokens.contains("try_lift (events)"));
    }
}

#[cfg(feature = "scaffolding-ffi-buffer-fns")]
fn ffi_buffer_scaffolding_fn(
    fn_ident: &Ident,
    return_type: &TokenStream,
    param_types: &[TokenStream],
    has_rust_call_status: bool,
) -> TokenStream {
    let fn_name = fn_ident.to_string();
    let ffi_buffer_fn_name = uniffi_meta::ffi_buffer_symbol_name(&fn_name);
    let ident = Ident::new(&ffi_buffer_fn_name, proc_macro2::Span::call_site());
    let type_list: Vec<_> = param_types.iter().map(|ty| quote! { #ty }).collect();
    if has_rust_call_status {
        quote! {
            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn #ident(
                arg_ptr: *mut ::uniffi::FfiBufferElement,
                return_ptr: *mut ::uniffi::FfiBufferElement,
            ) {
                let mut arg_buf = unsafe { ::std::slice::from_raw_parts(arg_ptr, ::uniffi::ffi_buffer_size!(#(#type_list),*)) };
                let mut return_buf = unsafe { ::std::slice::from_raw_parts_mut(return_ptr, ::uniffi::ffi_buffer_size!(#return_type, ::uniffi::RustCallStatus)) };
                let mut out_status: ::uniffi::RustCallStatus = ::std::default::Default::default();

                let return_value = #fn_ident(
                    #(
                        <#type_list as ::uniffi::FfiSerialize>::read(&mut arg_buf),
                    )*
                    &mut out_status,
                );
                <#return_type as ::uniffi::FfiSerialize>::write(&mut return_buf, return_value);
                <::uniffi::RustCallStatus as ::uniffi::FfiSerialize>::write(&mut return_buf, out_status);
            }
        }
    } else {
        quote! {
            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub unsafe extern "C" fn #ident(
                arg_ptr: *mut ::uniffi::FfiBufferElement,
                return_ptr: *mut ::uniffi::FfiBufferElement,
            ) {
                let mut arg_buf = unsafe { ::std::slice::from_raw_parts(arg_ptr, ::uniffi::ffi_buffer_size!(#(#type_list),*)) };
                let mut return_buf = unsafe { ::std::slice::from_raw_parts_mut(return_ptr, ::uniffi::ffi_buffer_size!(#return_type)) };

                let return_value = #fn_ident(#(
                    <#type_list as ::uniffi::FfiSerialize>::read(&mut arg_buf),
                )*);
                <#return_type as ::uniffi::FfiSerialize>::put(&mut return_buf, return_value);
            }
        }
    }
}

#[cfg(not(feature = "scaffolding-ffi-buffer-fns"))]
fn ffi_buffer_scaffolding_fn(
    _fn_ident: &Ident,
    _return_type: &TokenStream,
    _param_types: &[TokenStream],
    _add_rust_call_status: bool,
) -> TokenStream {
    quote! {}
}
