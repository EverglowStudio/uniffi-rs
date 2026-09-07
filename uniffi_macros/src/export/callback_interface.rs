/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{
    export::ImplItem,
    ffiops,
    fnsig::FnSignature,
    util::{
        async_trait_annotation, create_metadata_items, derive_ffi_traits, ident_to_string,
        tagged_impl_header, wasm_single_threaded_annotation,
    },
};
use proc_macro2::{Span, TokenStream};
use quote::{format_ident, quote};
use std::iter;
use syn::Ident;

/// Generate a trait impl that calls foreign callbacks
///
/// This generates:
///    * A `repr(C)` VTable struct where each field is the FFI function for the trait method.
///    * A FFI function for foreign code to set their VTable for the interface
///    * An implementation of the trait using that VTable
pub(super) fn trait_impl(
    mod_path: &str,
    trait_ident: &Ident,
    items: &[ImplItem],
    for_trait_interface: bool,
) -> syn::Result<TokenStream> {
    let trait_name = ident_to_string(trait_ident);
    let trait_impl_ident = trait_impl_ident(&trait_name);
    let vtable_type = format_ident!("UniFfiTraitVtable{trait_name}");
    let vtable_cell = format_ident!("UNIFFI_TRAIT_CELL_{}", trait_name.to_uppercase());
    let init_ident = Ident::new(
        &uniffi_meta::init_callback_vtable_fn_symbol_name(mod_path, &trait_name),
        Span::call_site(),
    );
    let methods = items
        .iter()
        .map(|item| match item {
            ImplItem::Constructor(sig) => Err(syn::Error::new(
                sig.span,
                "Constructors not allowed in trait interfaces",
            )),
            ImplItem::Method(sig) => Ok(sig),
        })
        .collect::<syn::Result<Vec<_>>>()?;

    let uniffi_foreign_handle_method = for_trait_interface.then(|| {
        quote! {
            fn uniffi_foreign_handle(&self) -> ::std::option::Option<::uniffi::Handle> {
                let vtable = #vtable_cell.get();
                ::std::option::Option::Some(::uniffi::Handle::from_raw_unchecked((vtable.uniffi_clone)(self.handle)))
            }
        }
    });

    let vtable_fields = methods.iter().map(|sig| {
        let ident = &sig.ident;
        let param_names = sig.scaffolding_param_names();
        let param_types = sig.scaffolding_param_types();
        let lift_return_type = if sig.stream_return.is_some() {
            quote! { ::uniffi::Handle }
        } else {
            ffiops::lift_return_type(&sig.return_ty)
        };
        let method = if !sig.is_async {
            quote! {
                pub #ident: extern "C" fn(
                    uniffi_handle: u64,
                    #(#param_names: #param_types,)*
                    uniffi_out_return: &mut #lift_return_type,
                    uniffi_out_call_status: &mut ::uniffi::RustCallStatus,
                ),
            }
        } else {
            quote! {
                pub #ident: extern "C" fn(
                    uniffi_handle: u64,
                    #(#param_names: #param_types,)*
                    uniffi_callback: ::uniffi::ForeignFutureCallback<#lift_return_type>,
                    uniffi_callback_data: u64,
                    uniffi_out_dropped_callback: &mut ::uniffi::ForeignFutureDroppedCallbackStruct,
                ),
            }
        };
        let stream_fields = sig.stream_return.as_ref().map(|stream| {
            let next = format_ident!("{}_stream_next", sig.ident);
            let cancel = format_ident!("{}_stream_cancel", sig.ident);
            let close = format_ident!("{}_stream_close", sig.ident);
            let item = &stream.item_ty;
            let error = &stream.error_ty;
            let input_return = ffiops::lift_return_type(
                quote! { ::std::result::Result<::std::option::Option<#item>, #error> },
            );
            quote! {
                pub #next: ::uniffi::ForeignInputStreamNextCallback<#input_return>,
                pub #cancel: ::uniffi::ForeignInputStreamCancelCallback,
                pub #close: ::uniffi::ForeignInputStreamCloseCallback,
            }
        });
        quote! { #method #stream_fields }
    });

    let trait_impl_methods = methods
        .iter()
        .map(|sig| gen_method_impl(sig, &vtable_cell))
        .collect::<syn::Result<Vec<_>>>()?;
    let has_async_method = methods.iter().any(|m| m.is_async);

    // Conditionally apply the async_trait attribute with or without ?Send based on the target
    let impl_attributes = has_async_method.then(async_trait_annotation);

    let single_threaded_annotation = wasm_single_threaded_annotation();

    Ok(quote! {
        #[allow(missing_docs)]
        #[repr(C)]
        pub struct #vtable_type {
            pub uniffi_free: extern "C" fn(handle: u64),
            pub uniffi_clone: extern "C" fn(handle: u64) -> u64,
            #(#vtable_fields)*
        }

        static #vtable_cell: ::uniffi::UniffiForeignPointerCell::<#vtable_type> = ::uniffi::UniffiForeignPointerCell::<#vtable_type>::new();

        #[allow(missing_docs)]
        #[unsafe(no_mangle)]
        pub extern "C" fn #init_ident(vtable: ::std::ptr::NonNull<#vtable_type>) {
            #vtable_cell.set(vtable);
        }

        #[derive(Debug)]
        struct #trait_impl_ident {
            handle: u64,
        }

        impl #trait_impl_ident {
            fn new(handle: u64) -> Self {
                Self { handle }
            }
        }

        #single_threaded_annotation
        ::uniffi::deps::static_assertions::assert_impl_all!(#trait_impl_ident: ::core::marker::Send);

        #impl_attributes
        impl #trait_ident for #trait_impl_ident {
            #(#trait_impl_methods)*
            #uniffi_foreign_handle_method
        }

        impl ::std::ops::Drop for #trait_impl_ident {
            fn drop(&mut self) {
                let vtable = #vtable_cell.get();
                (vtable.uniffi_free)(self.handle);
            }
        }
    })
}

pub fn trait_impl_ident(trait_name: &str) -> Ident {
    Ident::new(
        &format!("UniFFICallbackHandler{trait_name}"),
        Span::call_site(),
    )
}

pub fn ffi_converter_callback_interface_impl(
    trait_ident: &Ident,
    trait_impl_ident: &Ident,
) -> TokenStream {
    // TODO: support remote callback interfaces
    let remote = false;
    let trait_name = ident_to_string(trait_ident);
    let dyn_trait = quote! { dyn #trait_ident };
    let box_dyn_trait = quote! { ::std::boxed::Box<#dyn_trait> };
    let lift_impl_spec = tagged_impl_header("Lift", &box_dyn_trait, remote);
    let type_id_impl_specs = [
        tagged_impl_header("TypeId", &box_dyn_trait, remote),
        tagged_impl_header("TypeId", &dyn_trait, remote),
    ]
    .into_iter();
    let derive_ffi_traits = derive_ffi_traits(&box_dyn_trait, remote, &["LiftRef", "LiftReturn"]);
    let try_lift_self = ffiops::try_lift(quote! { Self });

    quote! {
        #[doc(hidden)]
        #[automatically_derived]
        unsafe #lift_impl_spec {
            type FfiType = u64;

            fn try_lift(v: Self::FfiType) -> ::uniffi::deps::anyhow::Result<Self> {
                ::std::result::Result::Ok(::std::boxed::Box::new(<#trait_impl_ident>::new(v)))
            }

            fn try_read(buf: &mut &[u8]) -> ::uniffi::deps::anyhow::Result<Self> {
                use ::uniffi::deps::bytes::Buf;
                ::uniffi::check_remaining(buf, 8)?;
                #try_lift_self(buf.get_u64())
            }
        }

        #(
            #[doc(hidden)]
            #[automatically_derived]
            #type_id_impl_specs {
                const TYPE_ID_META: ::uniffi::MetadataBuffer = ::uniffi::MetadataBuffer::from_code(
                    ::uniffi::metadata::codes::TYPE_CALLBACK_INTERFACE,
                )
                .concat_str(module_path!())
                .concat_str(#trait_name);
            }
        )*

        #derive_ffi_traits
    }
}

/// Generate a single method that invokes a foreign-supplied callback.
fn gen_method_impl(sig: &FnSignature, vtable_cell: &Ident) -> syn::Result<TokenStream> {
    // Generated locals must not shadow parameters from the business signature.
    let stream_return = Ident::new("UniffiCallbackStreamReturn", Span::mixed_site());
    let stream_handle = Ident::new("handle", Span::mixed_site());
    let vtable = Ident::new("vtable", Span::mixed_site());
    let receiver = Ident::new("uniffi_receiver", Span::mixed_site());
    let result = Ident::new("uniffi_result", Span::mixed_site());
    let return_value = Ident::new("value", Span::mixed_site());
    let status = Ident::new("status", Span::mixed_site());
    let callback = Ident::new("callback", Span::mixed_site());
    let callback_data = Ident::new("callback_data", Span::mixed_site());
    let dropped = Ident::new("dropped", Span::mixed_site());
    let mut variants = Vec::new();
    for &wasm in super::async_types::variants() {
        let signature = super::async_types::normalize_signature(&sig.original_signature, wasm)?;
        let output_type = super::async_types::normalized_output(&sig.original_signature, wasm)?;
        let target = super::async_types::target_attribute(wasm);
        let explicit_future = sig.is_async && sig.original_signature.asyncness.is_none();
        let receiver_handle = if explicit_future {
            quote! { #receiver.handle }
        } else {
            quote! { self.handle }
        };
        let ident = &sig.ident;
        let (callback_return, stream_converter) = if let Some(stream) = &sig.stream_return {
            let item = &stream.item_ty;
            let error = &stream.error_ty;
            let next = format_ident!("{}_stream_next", ident);
            let cancel = format_ident!("{}_stream_cancel", ident);
            let close = format_ident!("{}_stream_close", ident);
            let converter = quote! {
                struct #stream_return(::uniffi::UniFfiInputStream<#item, #error>);
                // SAFETY: the successful ABI result owns one handle belonging
                // to this method's typed producer vtable. Bind its disposer
                // during lift, including when a cancelled call discards it.
                unsafe impl ::uniffi::LiftReturn<crate::UniFfiTag> for #stream_return {
                    type ReturnType = ::uniffi::Handle;
                    fn try_lift_successful_return(handle: Self::ReturnType) -> ::uniffi::deps::anyhow::Result<Self> {
                        let vtable = #vtable_cell.get();
                        Ok(Self(::uniffi::UniFfiInputStream::from_foreign_callbacks::<crate::UniFfiTag>(
                            handle, vtable.#next, vtable.#cancel, vtable.#close,
                        )))
                    }
                }
            };
            let ty = match &sig.factory_error_ty {
                Some(error) => quote! { ::std::result::Result<#stream_return, #error> },
                None => quote! { #stream_return },
            };
            (ty, Some(converter))
        } else {
            (sig.return_ty.clone(), None)
        };
        let lift_return_type = ffiops::lift_return_type(&callback_return);
        let lift = ffiops::lift_foreign_return(&callback_return);
        let input_arguments: Vec<_> = sig
            .args
            .iter()
            .filter(|arg| arg.input_stream.is_some())
            .map(|arg| &arg.ident)
            .collect();
        let input_handles: Vec<_> = input_arguments
            .iter()
            .map(|ident| format_ident!("__uniffi_input_{}", ident, span = Span::mixed_site()))
            .collect();
        let lower_exprs = sig.args.iter().map(|arg| {
            let ident = &arg.ident;
            if arg.input_stream.is_some() {
                let handle = format_ident!("__uniffi_input_{}", ident, span = Span::mixed_site());
                quote! { #handle }
            } else {
                let lower = ffiops::lower(&arg.ty);
                quote! { #lower(#ident) }
            }
        });
        let call = if !sig.is_async {
            quote! {
                let mut #status = ::uniffi::RustCallStatus::default();
                let mut #return_value: #lift_return_type = ::uniffi::FfiDefault::ffi_default();
                (#vtable.#ident)(#receiver_handle, #(#lower_exprs,)* &mut #return_value, &mut #status);
                #lift(#return_value, #status)
            }
        } else {
            quote! {
                ::uniffi::foreign_async_call::<_, #callback_return, crate::UniFfiTag>(
                    move |#callback, #callback_data, #dropped| {
                        (#vtable.#ident)(#receiver_handle, #(#lower_exprs,)* #callback, #callback_data, #dropped);
                    }
                ).await
            }
        };
        let converted = if let Some(stream) = &sig.stream_return {
            let stream_value = if input_arguments.is_empty() {
                quote! { #stream_handle.0 }
            } else {
                quote! { #stream_handle.0.retain_until_close((#(#input_arguments,)*)) }
            };
            let stream_value = if stream.is_fallible {
                stream_value
            } else {
                quote! { ::uniffi::deps::futures_util::StreamExt::map(#stream_value, |item| match item {
                    Ok(value) => value, Err(never) => match never {},
                }) }
            };
            // The expected trait return type performs the dyn coercion here.
            if sig.factory_error_ty.is_some() {
                quote! { match #result { Ok(#stream_handle) => Ok(::std::boxed::Box::pin(#stream_value)), Err(error) => Err(error) } }
            } else {
                quote! {{ let #stream_handle = #result; ::std::boxed::Box::pin(#stream_value) }}
            }
        } else {
            quote! { #result }
        };
        let release_receiver = explicit_future.then(|| quote! { ::std::mem::drop(#receiver); });
        let release_inputs = sig.stream_return.is_none().then(|| {
            quote! {
                ::std::mem::drop((#(#input_arguments,)*));
            }
        });
        let body = quote! {
            #stream_converter
            let #vtable = #vtable_cell.get();
            #(let #input_handles = #input_arguments.handle();)*
            let #result = { #call };
            // Ordinary callbacks release input frames after completion. A
            // returned stream instead owns them through its close acknowledgement.
            #release_inputs
            #release_receiver
            let uniffi_output: #output_type = #converted;
            uniffi_output
        };
        let body = if explicit_future {
            quote! {
                // A returned 'static Future can outlive this handler. Keep an
                // owned foreign lease until completion or Future destruction.
                let #receiver = Self::new((#vtable_cell.get().uniffi_clone)(self.handle));
                ::std::boxed::Box::pin(async move { #body })
            }
        } else {
            body
        };
        variants.push(quote! { #target #signature { #body } });
    }
    Ok(quote! { #(#variants)* })
}

pub(super) fn metadata_items(
    self_ident: &Ident,
    items: &[ImplItem],
    docstring: String,
) -> syn::Result<Vec<TokenStream>> {
    let trait_name = ident_to_string(self_ident);
    let callback_interface_items = create_metadata_items(
        "callback_interface",
        &trait_name,
        quote! {
            ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::CALLBACK_INTERFACE)
                .concat_str(module_path!())
                .concat_str(#trait_name)
                .concat_long_str(#docstring)
        },
        None,
    );

    iter::once(Ok(callback_interface_items))
        .chain(items.iter().map(|item| match item {
            ImplItem::Method(sig) => sig.metadata_items(),
            _ => unreachable!("traits have no constructors"),
        }))
        .collect()
}
