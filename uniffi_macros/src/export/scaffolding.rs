/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use proc_macro2::{Ident, TokenStream};
use quote::quote;
use std::iter;

use super::attributes::AsyncRuntime;
use crate::{
    ffiops,
    fnsig::{FnKind, FnSignature, ReceiverArg},
};

fn wrap_async_future_expr(rust_fn_call: TokenStream, ar: Option<&AsyncRuntime>) -> TokenStream {
    match ar {
        Some(AsyncRuntime::Tokio(_)) => {
            quote! { ::uniffi::deps::async_compat::Compat::new(#rust_fn_call) }
        }
        None => {
            #[cfg(feature = "default-async-runtime-tokio")]
            {
                quote! {{
                    #[cfg(not(target_arch = "wasm32"))]
                    {
                        ::uniffi::deps::async_compat::Compat::new(#rust_fn_call)
                    }
                    #[cfg(target_arch = "wasm32")]
                    {
                        #rust_fn_call
                    }
                }}
            }
            #[cfg(not(feature = "default-async-runtime-tokio"))]
            {
                rust_fn_call
            }
        }
    }
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
        #scaffolding_func
        #metadata_items
    })
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
        let rust_fn_call = if is_trait {
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
    let use_trait = use_trait.map(|tr| quote! { use #tr; });
    if let Some(stream_return) = &sig.stream_return {
        let item_ty = &stream_return.item_ty;
        let error_ty = &stream_return.error_ty;
        let stream_next_ident = Ident::new(
            &uniffi_meta::fn_stream_next_symbol_name(&sig.mod_path, &sig.name),
            proc_macro2::Span::call_site(),
        );
        let stream_cancel_ident = Ident::new(
            &uniffi_meta::fn_stream_cancel_symbol_name(&sig.mod_path, &sig.name),
            proc_macro2::Span::call_site(),
        );
        let registry_ident = Ident::new(
            &format!("__UNIFFI_STREAM_REGISTRY_{}", ffi_fn_name).to_ascii_uppercase(),
            proc_macro2::Span::call_site(),
        );
        let scaffolding_fn_ffi_buffer_version =
            ffi_buffer_scaffolding_fn(&ffi_ident, &quote! { ::uniffi::Handle }, &param_types, true);

        return Ok(quote! {
            #[doc(hidden)]
            static #registry_ident: ::uniffi::RustStreamRegistry<#item_ty, #error_ty> =
                ::uniffi::deps::once_cell::sync::Lazy::new(::std::default::Default::default);

            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #ffi_ident(
                #(#param_names: #param_types,)*
                call_status: &mut ::uniffi::RustCallStatus,
            ) -> ::uniffi::Handle {
                #use_trait
                ::uniffi::deps::trace!("calling: {}", #ffi_fn_name);
                let uniffi_lift_args = #lift_closure;
                ::uniffi::rust_call(call_status, || {
                    let uniffi_lifted_args: ::std::result::Result<_, (&'static str, ::uniffi::deps::anyhow::Error)> =
                        uniffi_lift_args();
                    match uniffi_lifted_args {
                        ::std::result::Result::Ok(uniffi_args) => {
                            ::uniffi::deps::trace!("lift_args success: {}", #ffi_fn_name);
                            let uniffi_result = #rust_fn_call;
                            ::uniffi::deps::trace!("call success: {}", #ffi_fn_name);
                            let uniffi_result = #convert_result;
                            ::std::result::Result::Ok(::uniffi::rust_stream_new(&#registry_ident, uniffi_result))
                        }
                        ::std::result::Result::Err((arg_name, error)) => {
                            ::uniffi::deps::trace!("lift_args error: {}", #ffi_fn_name);
                            ::std::result::Result::Err(::uniffi::RustCallError::InternalError(
                                ::std::format!("Failed to convert arg '{arg_name}':\n{error:?}")
                            ))
                        },
                    }
                })
            }

            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #stream_next_ident(handle: ::uniffi::Handle) -> ::uniffi::Handle {
                ::uniffi::rust_stream_next::<#item_ty, #error_ty, crate::UniFfiTag>(
                    &#registry_ident,
                    handle,
                    crate::UniFfiTag,
                )
            }

            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #stream_cancel_ident(handle: ::uniffi::Handle) {
                ::uniffi::rust_stream_cancel::<#item_ty, #error_ty>(&#registry_ident, handle)
            }

            #scaffolding_fn_ffi_buffer_version
        });
    }
    let ffi_return_ty = ffiops::lower_return_type(return_ty);
    let lower_return = ffiops::lower_return(return_ty);
    let handle_failed_lift = ffiops::lower_return_handle_failed_lift(return_ty);

    Ok(if !sig.is_async {
        let scaffolding_fn_ffi_buffer_version =
            ffi_buffer_scaffolding_fn(&ffi_ident, &ffi_return_ty, &param_types, true);
        quote! {
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
        let future_expr = wrap_async_future_expr(rust_fn_call, ar);
        let scaffolding_fn_ffi_buffer_version =
            ffi_buffer_scaffolding_fn(&ffi_ident, &quote! { ::uniffi::Handle}, &param_types, false);

        quote! {
            #[doc(hidden)]
            #[unsafe(no_mangle)]
            pub extern "C" fn #ffi_ident(#(#param_names: #param_types,)*) -> ::uniffi::Handle {
                ::uniffi::deps::trace!("calling: {}", #name);
                let uniffi_lifted_args = (#lift_closure)();
                ::uniffi::rust_future_new::<_, #return_ty, _>(
                    async move {
                        match uniffi_lifted_args {
                            ::std::result::Result::Ok(uniffi_args) => {
                                let uniffi_result = #future_expr.await;
                                Ok(#convert_result)
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
    })
}

#[cfg(test)]
mod tests {
    use super::wrap_async_future_expr;
    use crate::export::AsyncRuntime;
    use proc_macro2::Span;
    use quote::quote;
    use syn::LitStr;

    #[test]
    fn explicit_tokio_async_runtime_wraps_future() {
        let runtime = AsyncRuntime::Tokio(LitStr::new("tokio", Span::call_site()));
        let tokens = wrap_async_future_expr(quote! { call_me() }, Some(&runtime)).to_string();
        assert!(tokens.contains("async_compat"));
        assert!(tokens.contains("Compat"));
        assert!(tokens.contains("call_me"));
    }

    #[cfg(feature = "default-async-runtime-tokio")]
    #[test]
    fn default_tokio_feature_wraps_non_wasm_only() {
        let tokens = wrap_async_future_expr(quote! { call_me() }, None).to_string();
        assert!(tokens.contains("async_compat"));
        assert!(tokens.contains("target_arch"));
        assert!(tokens.contains("wasm32"));
        assert!(tokens.contains("call_me"));
    }

    #[cfg(not(feature = "default-async-runtime-tokio"))]
    #[test]
    fn without_feature_default_async_runtime_is_unchanged() {
        let tokens = wrap_async_future_expr(quote! { call_me() }, None).to_string();
        assert!(!tokens.contains("async_compat"));
        assert_eq!(tokens, quote! { call_me() }.to_string());
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
