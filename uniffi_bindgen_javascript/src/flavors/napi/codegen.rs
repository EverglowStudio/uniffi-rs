//! Rust code generator for the napi flavor.
//!
//! This module renders a single Rust bridge file per component. The
//! surrounding `mod.rs` owns output layout and the tiny JavaScript
//! adapter; this file owns type lowering/lifting, callback handling,
//! async exports, and napi-specific surface generation.

use anyhow::{bail, ensure, Result};
use heck::{ToSnakeCase, ToUpperCamelCase};
use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::parse2;
use uniffi_bindgen::interface::{
    Argument, AsType, Callable, ComponentInterface, Constructor, Enum, Field, Function, Method,
    Object, ObjectImpl, Record, TraitKind, Type, Variant,
};

use crate::callback_metadata;

pub fn render_napi_rust(ci: &ComponentInterface) -> Result<String> {
    let generator = Generator::new(ci, CallbackAsyncReturn::Promise);
    generator.validate()?;
    let tokens = generator.render()?;
    let file = parse2::<syn::File>(tokens)?;
    Ok(prettyplease::unparse(&file))
}

pub fn render_ohos_rust(
    ci: &ComponentInterface,
    identity_export: &str,
    contract_digest: &str,
) -> Result<String> {
    ensure!(
        identity_export == super::ohos_bridge_identity_export(contract_digest),
        "invalid OHOS bridge identity export"
    );
    ensure!(
        contract_digest.len() == 64 && contract_digest.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid OHOS facade contract digest"
    );
    let generator = Generator::new(ci, CallbackAsyncReturn::Direct);
    generator.validate()?;
    let tokens = generator.render()?;
    let file = parse2::<syn::File>(tokens)?;
    let mut rust = prettyplease::unparse(&file);
    let identity_ident = rust_ident(identity_export);
    let identity_tokens = quote! {
        #[allow(non_snake_case)]
        #[napi]
        pub fn #identity_ident() -> String {
            #contract_digest.to_string()
        }
    };
    let identity_file = parse2::<syn::File>(identity_tokens)?;
    rust.push_str(&prettyplease::unparse(&identity_file));
    Ok(rust
        .replace("uniffi-bindgen-napi", "uniffi-bindgen-ohos")
        .replace("use napi_derive::napi;", "use napi_derive_ohos::napi;")
        .replace("napi::", "napi_ohos::"))
}

pub(crate) struct Generator<'a> {
    ci: &'a ComponentInterface,
    callback_async_return: CallbackAsyncReturn,
}

/// How an async foreign callback returns through a threadsafe function.
///
/// Node N-API gives us a JavaScript Promise as the direct callback result,
/// while ArkTS' N-API implementation only completes the TSFN return callback
/// for a concrete value.  Keep that platform distinction in the generator so
/// the two bridge ABIs cannot silently drift through post-generation edits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CallbackAsyncReturn {
    Promise,
    Direct,
}

impl<'a> Generator<'a> {
    fn new(ci: &'a ComponentInterface, callback_async_return: CallbackAsyncReturn) -> Self {
        Self {
            ci,
            callback_async_return,
        }
    }

    fn async_callbacks_return_directly(&self) -> bool {
        self.callback_async_return == CallbackAsyncReturn::Direct
    }

    fn has_stream_functions(&self) -> bool {
        self.ci
            .function_definitions()
            .iter()
            .any(|function| matches!(function.return_type(), Some(Type::Stream { .. })))
    }

    fn render(&self) -> Result<TokenStream> {
        let input_stream_descriptors = collect_input_stream_descriptors(self.ci)?;
        let records = self
            .ci
            .record_definitions()
            .iter()
            .map(|record| self.render_record(record))
            .collect::<Result<Vec<_>>>()?;
        let enums = self
            .ci
            .enum_definitions()
            .iter()
            .map(|enum_| self.render_enum(enum_))
            .collect::<Result<Vec<_>>>()?;
        let objects = self
            .ci
            .object_definitions()
            .iter()
            .map(|object| self.render_object(object))
            .collect::<Result<Vec<_>>>()?;
        let functions = self
            .ci
            .function_definitions()
            .iter()
            .map(|function| self.render_function(function))
            .collect::<Result<Vec<_>>>()?;
        let input_stream_helpers = self.render_input_stream_helpers(&input_stream_descriptors)?;
        let stream_helpers = if self.has_stream_functions() {
            quote! {
                fn __uniffi_stream_handle_from_bigint(handle: BigInt) -> Result<::uniffi::Handle> {
                    let (sign, value, lossless) = handle.get_u64();
                    if sign && value != 0 {
                        return Err(Error::new(Status::InvalidArg, "negative stream handle"));
                    }
                    if !lossless || value == 0 {
                        return Err(Error::new(Status::InvalidArg, "invalid stream handle"));
                    }
                    Ok(::uniffi::Handle::from_raw_unchecked(value))
                }
            }
        } else {
            quote!()
        };

        Ok(quote! {
            // Generated by uniffi-bindgen-napi.
            #[allow(unused_imports)]
            use napi::bindgen_prelude::*;
            #[allow(unused_imports)]
            use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
            use napi_derive::napi;

            fn into_napi_error<E: std::fmt::Display>(err: E) -> Error {
                Error::new(Status::GenericFailure, err.to_string())
            }

            #stream_helpers

            #[derive(Clone, Debug, PartialEq, Eq, Hash)]
            struct __UniffiTimestamp(pub ::std::time::SystemTime);

            #[derive(Clone, Debug, PartialEq, Eq, Hash)]
            struct __UniffiDuration(pub ::std::time::Duration);

            #[napi(object)]
            pub struct __UniffiCallbackHandle {
                pub id: u32,
            }

            impl TypeName for __UniffiTimestamp {
                fn type_name() -> &'static str {
                    "Date"
                }

                fn value_type() -> ValueType {
                    ValueType::Object
                }
            }

            impl ValidateNapiValue for __UniffiTimestamp {}

            impl FromNapiValue for __UniffiTimestamp {
                unsafe fn from_napi_value(env: napi::sys::napi_env, napi_val: napi::sys::napi_value) -> Result<Self> {
                    let mut ms = 0.0;
                    napi::check_status!(unsafe { napi::sys::napi_get_date_value(env, napi_val, &mut ms) })?;
                    if !ms.is_finite() {
                        return Err(Error::new(Status::InvalidArg, "invalid Date"));
                    }
                    if ms.abs() > 8.64e15 {
                        return Err(Error::new(Status::InvalidArg, "timestamp exceeds JS Date range"));
                    }
                    let ms_i = ms.trunc() as i64;
                    let ts = if ms_i >= 0 {
                        ::std::time::UNIX_EPOCH
                            .checked_add(::std::time::Duration::from_millis(ms_i as u64))
                            .ok_or_else(|| Error::new(Status::InvalidArg, "timestamp overflow"))?
                    } else {
                        ::std::time::UNIX_EPOCH
                            .checked_sub(::std::time::Duration::from_millis((-ms_i) as u64))
                            .ok_or_else(|| Error::new(Status::InvalidArg, "timestamp overflow"))?
                    };
                    Ok(Self(ts))
                }
            }

            impl ToNapiValue for __UniffiTimestamp {
                unsafe fn to_napi_value(env: napi::sys::napi_env, val: Self) -> Result<napi::sys::napi_value> {
                    let ms = match val.0.duration_since(::std::time::UNIX_EPOCH) {
                        Ok(delta) => (delta.as_secs() as f64) * 1000.0 + (delta.subsec_nanos() as f64) / 1_000_000.0,
                        Err(err) => {
                            let delta = err.duration();
                            -((delta.as_secs() as f64) * 1000.0 + (delta.subsec_nanos() as f64) / 1_000_000.0)
                        }
                    };
                    if !ms.is_finite() || ms.abs() > 8.64e15 {
                        return Err(Error::new(Status::InvalidArg, "timestamp exceeds JS Date range"));
                    }
                    let mut js_value = std::ptr::null_mut();
                    napi::check_status!(unsafe { napi::sys::napi_create_date(env, ms, &mut js_value) })?;
                    Ok(js_value)
                }
            }

            impl TypeName for __UniffiDuration {
                fn type_name() -> &'static str {
                    "number"
                }

                fn value_type() -> ValueType {
                    ValueType::Number
                }
            }

            impl ValidateNapiValue for __UniffiDuration {}

            impl FromNapiValue for __UniffiDuration {
                unsafe fn from_napi_value(env: napi::sys::napi_env, napi_val: napi::sys::napi_value) -> Result<Self> {
                    let ms = f64::from_napi_value(env, napi_val)?;
                    if !ms.is_finite() {
                        return Err(Error::new(Status::InvalidArg, "duration must be finite"));
                    }
                    if ms < 0.0 {
                        return Err(Error::new(Status::InvalidArg, "duration must be non-negative"));
                    }
                    let secs_f = (ms / 1000.0).trunc();
                    if secs_f > u64::MAX as f64 {
                        return Err(Error::new(Status::InvalidArg, "duration exceeds Rust range"));
                    }
                    let mut secs = secs_f as u64;
                    let mut nanos = ((ms % 1000.0) * 1_000_000.0).round() as u32;
                    if nanos == 1_000_000_000 {
                        nanos = 0;
                        secs = secs
                            .checked_add(1)
                            .ok_or_else(|| Error::new(Status::InvalidArg, "duration exceeds Rust range"))?;
                    }
                    Ok(Self(::std::time::Duration::new(secs, nanos)))
                }
            }

            impl ToNapiValue for __UniffiDuration {
                unsafe fn to_napi_value(env: napi::sys::napi_env, val: Self) -> Result<napi::sys::napi_value> {
                    let ms = (val.0.as_secs() as f64) * 1000.0 + (val.0.subsec_nanos() as f64) / 1_000_000.0;
                    if !ms.is_finite() || ms > 9_007_199_254_740_991.0 {
                        return Err(Error::new(Status::InvalidArg, "duration exceeds JS number range"));
                    }
                    f64::to_napi_value(env, ms)
                }
            }

            #input_stream_helpers

            #(#records)*
            #(#enums)*
            #(#objects)*
            #(#functions)*
        })
    }

    fn render_input_stream_helpers(
        &self,
        input_stream_descriptors: &[InputStreamDescriptor],
    ) -> Result<TokenStream> {
        if input_stream_descriptors.is_empty() {
            return Ok(quote!());
        }
        let typed_helpers = input_stream_descriptors
            .iter()
            .map(|descriptor| self.render_typed_input_stream_helper(descriptor))
            .collect::<Result<Vec<_>>>()?;
        Ok(quote! {
            pub struct __UniffiInputStream<NextResult: 'static + FromNapiValue> {
                handle: u32,
                next: std::sync::Arc<
                    ThreadsafeFunction<
                        u32,
                        napi::bindgen_prelude::Promise<NextResult>,
                    >,
                >,
                cancel: std::sync::Arc<ThreadsafeFunction<u32>>,
            }

            impl<NextResult: 'static + FromNapiValue> TypeName for __UniffiInputStream<NextResult> {
                fn type_name() -> &'static str {
                    "UniFfiInputStream"
                }

                fn value_type() -> ValueType {
                    ValueType::Object
                }
            }

            impl<NextResult: 'static + FromNapiValue> ValidateNapiValue
                for __UniffiInputStream<NextResult>
            {
            }

            impl<NextResult: 'static + FromNapiValue> FromNapiValue
                for __UniffiInputStream<NextResult>
            where
                ThreadsafeFunction<
                    u32,
                    napi::bindgen_prelude::Promise<NextResult>,
                >: FromNapiValue,
                ThreadsafeFunction<u32>: FromNapiValue,
            {
                unsafe fn from_napi_value(
                    env: napi::bindgen_prelude::sys::napi_env,
                    napi_val: napi::bindgen_prelude::sys::napi_value,
                ) -> Result<Self> {
                    let mut __scope = std::ptr::null_mut();
                    napi::check_status!(
                        unsafe { napi::bindgen_prelude::sys::napi_open_handle_scope(env, &mut __scope) },
                        "Failed to open input stream wrapper handle scope"
                    )?;
                    let __result = (|| -> Result<Self> {
                        let obj = napi::bindgen_prelude::Object::from_napi_value(env, napi_val)?;
                        Ok(Self {
                            handle: obj.get_named_property_unchecked::<u32>("handle")?,
                            next: std::sync::Arc::new(
                                obj.get_named_property_unchecked::<
                                    ThreadsafeFunction<
                                        u32,
                                        napi::bindgen_prelude::Promise<NextResult>,
                                    >,
                                >("next")?,
                            ),
                            cancel: std::sync::Arc::new(
                                obj.get_named_property_unchecked::<ThreadsafeFunction<u32>>(
                                    "cancel",
                                )?,
                            ),
                        })
                    })();
                    let __close_status = unsafe {
                        napi::bindgen_prelude::sys::napi_close_handle_scope(env, __scope)
                    };
                    napi::check_status!(
                        __close_status,
                        "Failed to close input stream wrapper handle scope"
                    )?;
                    __result
                }
            }

            #(#typed_helpers)*
        })
    }

    fn render_typed_input_stream_helper(
        &self,
        descriptor: &InputStreamDescriptor,
    ) -> Result<TokenStream> {
        let item_type = descriptor.item_type();
        let error_type = descriptor.error_type();
        let next_ident = self.input_stream_next_result_ident(descriptor.input_type())?;
        let ops_ident = self.input_stream_ops_ident(descriptor.input_type())?;
        let item_bridge_ty = self.bridge_return_type(item_type)?;
        let error_bridge_ty = self.bridge_return_type(error_type)?;
        let item_core_ty = self.core_value_type(item_type)?;
        let error_core_ty = self.core_value_type(error_type)?;
        let lowered_value = self.lower_callback_value_expr(quote!(value), item_type)?;
        let lowered_error = self.lower_callback_value_expr(quote!(error), error_type)?;
        Ok(quote! {
            #[napi(object)]
            pub struct #next_ident {
                pub ok: bool,
                pub done: Option<bool>,
                pub value: Option<#item_bridge_ty>,
                pub error: Option<#error_bridge_ty>,
            }

            struct #ops_ident {
                next: std::sync::Arc<
                    ThreadsafeFunction<
                        u32,
                        napi::bindgen_prelude::Promise<#next_ident>,
                    >,
                >,
                cancel: std::sync::Arc<ThreadsafeFunction<u32>>,
                _phantom: std::marker::PhantomData<fn() -> (#item_core_ty, #error_core_ty)>,
            }

            impl ::uniffi::ForeignInputStreamOps<#item_core_ty, #error_core_ty> for #ops_ident {
                fn next(
                    &self,
                    handle: ::uniffi::Handle,
                ) -> ::uniffi::ForeignInputStreamNextFuture<#item_core_ty, #error_core_ty> {
                    let next = self.next.clone();
                    Box::pin(async move {
                        let handle = u32::try_from(handle.as_raw()).unwrap_or_else(|_| {
                            panic!("uniffi input stream handle does not fit in u32")
                        });
                        let promise = next.call_async(Ok(handle)).await.unwrap_or_else(|err| {
                            panic!("uniffi input stream failed to dispatch next(): {}", err)
                        });
                        let result: #next_ident = promise.await.unwrap_or_else(|err| {
                            panic!("uniffi input stream next() dispatcher rejected: {}", err)
                        });
                        if !result.ok {
                            let error = result.error.unwrap_or_else(|| {
                                panic!("uniffi input stream next() returned err without typed error")
                            });
                            return Err(#lowered_error);
                        }
                        if result.done.unwrap_or(false) {
                            return Ok(None);
                        }
                        let value = result.value.unwrap_or_else(|| {
                            panic!("uniffi input stream next() returned value envelope without value")
                        });
                        Ok(Some(#lowered_value))
                    })
                }

                fn cancel(&self, handle: ::uniffi::Handle) {
                    let handle = u32::try_from(handle.as_raw()).unwrap_or_else(|_| {
                        panic!("uniffi input stream handle does not fit in u32")
                    });
                    let _ = self
                        .cancel
                        .call(Ok(handle), ThreadsafeFunctionCallMode::NonBlocking);
                }
            }
        })
    }

    fn validate(&self) -> Result<()> {
        let callback_trait_names = self
            .ci
            .object_definitions()
            .iter()
            .filter(|object| object.has_callback_interface())
            .map(|object| object.name().to_string())
            .collect::<std::collections::HashSet<_>>();

        for callback in self.ci.callback_interface_definitions() {
            if !callback_trait_names.contains(callback.name()) {
                bail!(
                    "callback interface `{}` is not supported unless it is exported via `with_foreign` on an object trait",
                    callback.name()
                );
            }
        }

        for record in self.ci.record_definitions() {
            for field in record.fields() {
                self.ensure_type_supported(&field.as_type(), TypeUsage::Value, "record field")?;
            }
            for constructor in record.constructors() {
                self.validate_callable(constructor, "record constructor")?;
            }
            for method in record.methods() {
                ensure!(
                    !method.takes_self_by_arc(),
                    "record `{}` method `{}` taking self by Arc is not supported in the napi bridge",
                    record.name(),
                    method.name()
                );
                self.validate_callable(method, "record method")?;
            }
        }

        for enum_ in self.ci.enum_definitions() {
            for variant in enum_.variants() {
                for field in variant.fields() {
                    self.ensure_type_supported(&field.as_type(), TypeUsage::Value, "enum field")?;
                }
            }
            for constructor in enum_.constructors() {
                self.validate_callable(constructor, "enum constructor")?;
            }
            for method in enum_.methods() {
                ensure!(
                    !method.takes_self_by_arc(),
                    "enum `{}` method `{}` taking self by Arc is not supported in the napi bridge",
                    enum_.name(),
                    method.name()
                );
                self.validate_callable(method, "enum method")?;
            }
        }

        for object in self.ci.object_definitions() {
            if object.remote() {
                bail!("remote object `{}` is not supported", object.name());
            }
            match object.imp() {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    for constructor in object.constructors() {
                        self.validate_callable(constructor, "constructor")?;
                    }
                    for method in object.methods() {
                        self.validate_callable(method, "method")?;
                    }
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                    ensure!(
                        object.constructors().is_empty(),
                        "callback trait `{}` constructors are not supported",
                        object.name()
                    );
                    for method in object.methods() {
                        for arg in method.arguments() {
                            self.ensure_type_supported(
                                &arg.as_type(),
                                TypeUsage::CallbackArg,
                                "callback argument",
                            )?;
                        }
                        if let Some(return_type) = method.return_type() {
                            self.ensure_type_supported(
                                return_type,
                                TypeUsage::CallbackReturn,
                                "callback return",
                            )?;
                        }
                        if let Some(throws_type) = method.throws_type() {
                            self.ensure_type_supported(
                                throws_type,
                                TypeUsage::CallbackReturn,
                                "callback error",
                            )?;
                        }
                    }
                }
            }
        }

        for function in self.ci.function_definitions() {
            self.validate_callable(function, "function")?;
        }

        Ok(())
    }

    fn validate_callable(&self, callable: &dyn Callable, label: &str) -> Result<()> {
        if matches!(callable.return_type(), Some(Type::Stream { .. })) {
            ensure!(
                label == "function",
                "{label} native stream returns are only supported on top-level functions"
            );
            ensure!(
                !callable.is_async(),
                "{label} native stream returns must be synchronous start functions"
            );
        }
        for arg in callable.arguments() {
            self.ensure_type_supported(&arg.as_type(), TypeUsage::Arg, label)?;
        }
        if let Some(return_type) = callable.return_type() {
            self.ensure_type_supported(return_type, TypeUsage::Return, label)?;
        }
        if let Some(throws_type) = callable.throws_type() {
            self.ensure_type_supported(throws_type, TypeUsage::Error, label)?;
        }
        Ok(())
    }

    fn ensure_type_supported(&self, ty: &Type, usage: TypeUsage, label: &str) -> Result<()> {
        match ty {
            Type::UInt8
            | Type::Int8
            | Type::UInt16
            | Type::Int16
            | Type::UInt32
            | Type::Int32
            | Type::UInt64
            | Type::Int64
            | Type::Float32
            | Type::Float64
            | Type::Boolean
            | Type::String
            | Type::Bytes => Ok(()),
            Type::Record { name, .. } | Type::Enum { name, .. } => {
                ensure!(
                    self.ci.get_type(name).is_some(),
                    "{label} type `{name}` is unresolved"
                );
                Ok(())
            }
            Type::Object { name, imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    if matches!(usage, TypeUsage::Value | TypeUsage::CallbackArg) {
                        bail!("{label} type `{name}` is not supported in nested/value contexts");
                    }
                    Ok(())
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                    ensure!(
                        matches!(usage, TypeUsage::Arg | TypeUsage::CallbackReturn),
                        "{label} type `{name}` is only supported as a direct function/method argument or callback return"
                    );
                    Ok(())
                }
            },
            Type::Optional { inner_type }
            | Type::Sequence { inner_type }
            | Type::Box { inner_type }
            | Type::Set { inner_type } => {
                ensure!(
                    !matches!(inner_type.as_ref(), Type::InputStream { .. }),
                    "{label} nested input stream types are not supported"
                );
                self.ensure_type_supported(inner_type, usage, label)
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                ensure!(
                    !matches!(key_type.as_ref(), Type::InputStream { .. })
                        && !matches!(value_type.as_ref(), Type::InputStream { .. }),
                    "{label} nested input stream types are not supported"
                );
                self.ensure_type_supported(key_type, TypeUsage::Value, label)?;
                self.ensure_type_supported(value_type, TypeUsage::Value, label)
            }
            Type::Stream { .. } => {
                ensure!(
                    matches!(usage, TypeUsage::Return),
                    "{label} native stream is only supported as a direct return value"
                );
                if let Type::Stream {
                    item_type,
                    error_type,
                    ..
                } = ty
                {
                    self.ensure_type_supported(item_type, TypeUsage::Value, "stream item")?;
                    self.ensure_type_supported(error_type, TypeUsage::Error, "stream error")?;
                }
                Ok(())
            }
            Type::InputStream {
                item_type,
                error_type,
                ..
            } => {
                ensure!(
                    matches!(usage, TypeUsage::Arg),
                    "{label} input stream is only supported as a direct argument"
                );
                self.ensure_type_supported(item_type, TypeUsage::Value, "input stream item")?;
                self.ensure_type_supported(error_type, TypeUsage::Error, "input stream error")?;
                Ok(())
            }
            Type::Timestamp | Type::Duration => Ok(()),
            Type::CallbackInterface { name, .. } => {
                ensure!(
                    matches!(usage, TypeUsage::Arg | TypeUsage::CallbackReturn),
                    "{label} type `{name}` is only supported as a direct function/method argument or callback return"
                );
                Ok(())
            }
            Type::Custom { builtin, .. } => self.ensure_type_supported(builtin, usage, label),
        }
    }

    fn render_record(&self, record: &Record) -> Result<TokenStream> {
        let ident = rust_ident(record.name());
        let fields = record
            .fields()
            .iter()
            .map(|field| self.render_record_field(field))
            .collect::<Result<Vec<_>>>()?;
        let into_core_fields = record
            .fields()
            .iter()
            .map(|field| {
                let bridge_field_ident = rust_ident(field.name());
                let core_field_ident = rust_ident(field.rust_name());
                let expr =
                    self.lower_value_expr(quote!(value.#bridge_field_ident), &field.as_type())?;
                Ok(quote!(#core_field_ident: #expr))
            })
            .collect::<Result<Vec<_>>>()?;
        let from_core_fields = record
            .fields()
            .iter()
            .map(|field| {
                let bridge_field_ident = rust_ident(field.name());
                let core_field_ident = rust_ident(field.rust_name());
                let expr =
                    self.lift_value_expr(quote!(value.#core_field_ident), &field.as_type())?;
                Ok(quote!(#bridge_field_ident: #expr))
            })
            .collect::<Result<Vec<_>>>()?;
        let core_path = self.core_type_path(record.as_type());
        let constructors = record
            .constructors()
            .into_iter()
            .map(|constructor| {
                self.render_value_constructor(record.name(), &record.as_type(), constructor)
            })
            .collect::<Result<Vec<_>>>()?;
        let methods = record
            .methods()
            .into_iter()
            .map(|method| self.render_value_method(record.name(), &record.as_type(), method))
            .collect::<Result<Vec<_>>>()?;

        Ok(quote! {
            #[napi(object)]
            #[derive(Clone, Debug)]
            pub struct #ident {
                #(#fields,)*
            }

            impl From<#ident> for #core_path {
                fn from(value: #ident) -> Self {
                    Self {
                        #(#into_core_fields,)*
                    }
                }
            }

            impl From<#core_path> for #ident {
                fn from(value: #core_path) -> Self {
                    Self {
                        #(#from_core_fields,)*
                    }
                }
            }

            #(#constructors)*
            #(#methods)*
        })
    }

    fn render_record_field(&self, field: &Field) -> Result<TokenStream> {
        let ident = rust_ident(field.name());
        let ty = self.bridge_value_type(&field.as_type())?;
        Ok(quote!(pub #ident: #ty))
    }

    fn render_enum(&self, enum_: &Enum) -> Result<TokenStream> {
        let ident = rust_ident(enum_.name());
        let variants = enum_
            .variants()
            .iter()
            .map(|variant| self.render_enum_variant(variant))
            .collect::<Result<Vec<_>>>()?;
        let into_variants = enum_
            .variants()
            .iter()
            .map(|variant| self.render_into_core_variant(enum_, variant))
            .collect::<Result<Vec<_>>>()?;
        let from_variants = enum_
            .variants()
            .iter()
            .map(|variant| self.render_from_core_variant(enum_, variant))
            .collect::<Result<Vec<_>>>()?;
        let core_path = self.core_type_path(enum_.as_type());
        let constructors = enum_
            .constructors()
            .into_iter()
            .map(|constructor| {
                self.render_value_constructor(enum_.name(), &enum_.as_type(), constructor)
            })
            .collect::<Result<Vec<_>>>()?;
        let methods = enum_
            .methods()
            .into_iter()
            .map(|method| self.render_value_method(enum_.name(), &enum_.as_type(), method))
            .collect::<Result<Vec<_>>>()?;

        // `discriminant = "type"` is only valid for tagged unions in
        // napi-rs 3.x (variants carrying payload). For flat enums, emit
        // a string enum so the raw addon surface matches common/enums.ts
        // (`"North" | "South"`), not napi-rs' numeric C-enum default.
        let has_payload = enum_.variants().iter().any(|v| !v.fields().is_empty());
        let napi_attr = if has_payload {
            quote!(#[napi(discriminant = "type")])
        } else {
            quote!(#[napi(string_enum)])
        };

        Ok(quote! {
            #napi_attr
            #[derive(Clone, Debug)]
            pub enum #ident {
                #(#variants,)*
            }

            impl From<#ident> for #core_path {
                fn from(value: #ident) -> Self {
                    match value {
                        #(#into_variants,)*
                    }
                }
            }

            impl From<#core_path> for #ident {
                fn from(value: #core_path) -> Self {
                    match value {
                        #(#from_variants,)*
                    }
                }
            }

            #(#constructors)*
            #(#methods)*
        })
    }

    fn render_enum_variant(&self, variant: &Variant) -> Result<TokenStream> {
        let ident = rust_ident(variant.name());
        if variant.fields().is_empty() {
            return Ok(quote!(#ident));
        }
        let fields = variant
            .fields()
            .iter()
            .map(|field| {
                let field_ident = rust_ident(field.name());
                let field_ty = self.bridge_value_type(&field.as_type())?;
                Ok(quote!(#field_ident: #field_ty))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(quote!(#ident { #(#fields),* }))
    }

    fn render_into_core_variant(&self, enum_: &Enum, variant: &Variant) -> Result<TokenStream> {
        let bridge_enum = rust_ident(enum_.name());
        let core_enum = self.core_type_path(enum_.as_type());
        let bridge_variant_ident = rust_ident(variant.name());
        let core_variant_ident = rust_ident(variant.rust_name());
        if variant.fields().is_empty() {
            return Ok(
                quote!(#bridge_enum::#bridge_variant_ident => #core_enum::#core_variant_ident),
            );
        }
        let bindings = variant
            .fields()
            .iter()
            .map(|field| rust_ident(field.name()))
            .collect::<Vec<_>>();
        let lowers = variant
            .fields()
            .iter()
            .map(|field| {
                let bridge_field_ident = rust_ident(field.name());
                let core_field_ident = rust_ident(field.rust_name());
                let expr = self.lower_value_expr(quote!(#bridge_field_ident), &field.as_type())?;
                Ok(quote!(#core_field_ident: #expr))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(quote!(
            #bridge_enum::#bridge_variant_ident { #(#bindings),* } => #core_enum::#core_variant_ident {
                #(#lowers),*
            }
        ))
    }

    fn render_from_core_variant(&self, enum_: &Enum, variant: &Variant) -> Result<TokenStream> {
        let bridge_enum = rust_ident(enum_.name());
        let core_enum = self.core_type_path(enum_.as_type());
        let bridge_variant_ident = rust_ident(variant.name());
        let core_variant_ident = rust_ident(variant.rust_name());
        if variant.fields().is_empty() {
            return Ok(
                quote!(#core_enum::#core_variant_ident => #bridge_enum::#bridge_variant_ident),
            );
        }
        let bindings = variant
            .fields()
            .iter()
            .map(|field| rust_ident(field.rust_name()))
            .collect::<Vec<_>>();
        let lifts = variant
            .fields()
            .iter()
            .map(|field| {
                let core_field_ident = rust_ident(field.rust_name());
                let bridge_field_ident = rust_ident(field.name());
                let expr = self.lift_value_expr(quote!(#core_field_ident), &field.as_type())?;
                Ok(quote!(#bridge_field_ident: #expr))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(quote!(
            #core_enum::#core_variant_ident { #(#bindings),* } => #bridge_enum::#bridge_variant_ident {
                #(#lifts),*
            }
        ))
    }

    fn render_object(&self, object: &Object) -> Result<TokenStream> {
        match object.imp() {
            ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                self.render_object_class(object)
            }
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                self.render_callback_trait(object)
            }
        }
    }

    fn render_object_class(&self, object: &Object) -> Result<TokenStream> {
        let ident = rust_ident(object.name());
        let inner_type = self.core_object_inner_type(object);
        let constructors = object
            .constructors()
            .into_iter()
            .map(|constructor| self.render_constructor(object, constructor))
            .collect::<Result<Vec<_>>>()?;
        let methods = object
            .methods()
            .into_iter()
            .map(|method| self.render_object_method(method))
            .collect::<Result<Vec<_>>>()?;

        Ok(quote! {
            #[napi]
            pub struct #ident(#inner_type);

            #(#constructors)*
            #(#methods)*
        })
    }

    fn render_callback_trait(&self, object: &Object) -> Result<TokenStream> {
        let ident = rust_ident(object.name());
        let needs_env = object.methods().into_iter().any(|method| {
            !method.is_async() && (method.return_type().is_some() || method.throws_type().is_some())
        });
        let env_field = needs_env.then(|| quote!(env: Option<usize>,));
        let fields = object
            .methods()
            .into_iter()
            .map(|method| self.render_callback_field(object, method))
            .collect::<Result<Vec<_>>>()?;
        let registry_fields = self.render_callback_registry_fields()?;
        let registry_constructor = self.render_callback_registry_constructor(object)?;
        let impl_methods = object
            .methods()
            .into_iter()
            .map(|method| self.render_callback_impl_method(object, method))
            .collect::<Result<Vec<_>>>()?;
        let result_structs = object
            .methods()
            .into_iter()
            .filter(|method| method.throws_type().is_some())
            .map(|method| self.render_callback_result_struct(object, method))
            .collect::<Result<Vec<_>>>()?;
        let from_napi = self.render_callback_from_napi_impl(object, needs_env)?;
        let core_path = self.core_type_path(object.as_type());
        let async_trait_attr = object
            .methods()
            .into_iter()
            .any(|method| method.is_async())
            .then(|| quote!(#[async_trait::async_trait]));

        Ok(quote! {
            pub struct #ident {
                #(#fields,)*
                #(#registry_fields,)*
                __uniffi_callback_registry_id: Option<u32>,
                #env_field
            }

            #(#result_structs)*

            #registry_constructor

            #from_napi

            #async_trait_attr
            impl #core_path for #ident {
                #(#impl_methods)*
            }
        })
    }

    fn render_callback_field(&self, object: &Object, method: &Method) -> Result<TokenStream> {
        let field_ident = rust_ident(method.name());
        let ty = self.callback_field_from_napi_type(object, method)?;
        Ok(quote! {
            pub #field_ident: Option<#ty>
        })
    }

    fn render_callback_direct_field_type(
        &self,
        object: &Object,
        method: &Method,
    ) -> Result<TokenStream> {
        let tsfn_args = self.callback_tsfn_args(method)?;
        if method.is_async() {
            if method.throws_type().is_some() {
                let result_ty = self.callback_result_ident(object, method);
                Ok(self.callback_async_tsfn_type(tsfn_args, quote!(#result_ty)))
            } else if let Some(return_type) = method.return_type() {
                let bridge_return_ty = self.callback_async_bridge_type(return_type)?;
                Ok(self.callback_async_tsfn_type(tsfn_args, bridge_return_ty))
            } else {
                Ok(self.callback_async_tsfn_type(tsfn_args, quote!(())))
            }
        } else if method.throws_type().is_some() {
            let result_ty = self.callback_result_ident(object, method);
            Ok(quote! {
                FunctionRef<#tsfn_args, #result_ty>
            })
        } else if let Some(return_type) = method.return_type() {
            let bridge_return_ty = self.callback_bridge_type(return_type)?;
            Ok(quote! {
                FunctionRef<#tsfn_args, #bridge_return_ty>
            })
        } else {
            Ok(quote! {
                ThreadsafeFunction<#tsfn_args>
            })
        }
    }

    fn render_callback_registry_fields(&self) -> Result<Vec<TokenStream>> {
        self.callback_registry_field_defs()?
            .into_iter()
            .map(|(field_ident, ty)| {
                Ok(quote! {
                    #field_ident: Option<std::sync::Arc<#ty>>
                })
            })
            .collect()
    }

    fn render_callback_registry_constructor(&self, object: &Object) -> Result<TokenStream> {
        let ident = rust_ident(object.name());
        let direct_inits = object
            .methods()
            .into_iter()
            .map(|method| {
                let field_ident = rust_ident(method.name());
                quote!(#field_ident: None,)
            })
            .collect::<Vec<_>>();
        let registry_defs = self.callback_registry_field_defs()?;
        let registry_args = registry_defs
            .iter()
            .map(|(field_ident, ty)| quote!(#field_ident: Option<std::sync::Arc<#ty>>))
            .collect::<Vec<_>>();
        let registry_inits = registry_defs
            .iter()
            .map(|(field_ident, _)| quote!(#field_ident,))
            .collect::<Vec<_>>();
        let env_init = object
            .methods()
            .into_iter()
            .any(|method| {
                !method.is_async()
                    && (method.return_type().is_some() || method.throws_type().is_some())
            })
            .then(|| quote!(env: None,));
        Ok(quote! {
            impl #ident {
                fn __uniffi_from_callback_registry(
                    __uniffi_callback_registry_id: u32,
                    #(#registry_args),*
                ) -> Self {
                    Self {
                        #(#direct_inits)*
                        #(#registry_inits)*
                        __uniffi_callback_registry_id: Some(__uniffi_callback_registry_id),
                        #env_init
                    }
                }
            }
        })
    }

    fn callback_registry_field_defs(&self) -> Result<Vec<(syn::Ident, TokenStream)>> {
        self.ci
            .object_definitions()
            .iter()
            .filter(|object| {
                matches!(
                    object.imp(),
                    ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
                )
            })
            .flat_map(|object| {
                object.methods().into_iter().map(|method| {
                    let ty = self.callback_registry_field_type(object, method)?;
                    Ok((
                        self.callback_registry_field_ident(object.name(), method.name()),
                        ty,
                    ))
                })
            })
            .collect()
    }

    fn callback_registry_field_ident(&self, object_name: &str, method_name: &str) -> syn::Ident {
        rust_ident(&format!(
            "__uniffi_registry_{}_{}",
            object_name.to_snake_case(),
            method_name.to_snake_case()
        ))
    }

    fn callback_registry_field_type(
        &self,
        object: &Object,
        method: &Method,
    ) -> Result<TokenStream> {
        let tsfn_args = self.callback_registry_tsfn_args(method)?;
        if method.is_async() {
            if method.throws_type().is_some() {
                let result_ty = self.callback_result_ident(object, method);
                Ok(self.callback_async_tsfn_type(tsfn_args, quote!(#result_ty)))
            } else if let Some(return_type) = method.return_type() {
                let bridge_return_ty = self.callback_async_bridge_type(return_type)?;
                Ok(self.callback_async_tsfn_type(tsfn_args, bridge_return_ty))
            } else {
                Ok(self.callback_async_tsfn_type(tsfn_args, quote!(())))
            }
        } else if method.throws_type().is_some() {
            let result_ty = self.callback_result_ident(object, method);
            Ok(quote!(ThreadsafeFunction<#tsfn_args, #result_ty>))
        } else if let Some(return_type) = method.return_type() {
            let bridge_return_ty = self.callback_bridge_type(return_type)?;
            Ok(quote!(ThreadsafeFunction<#tsfn_args, #bridge_return_ty>))
        } else {
            Ok(quote!(ThreadsafeFunction<#tsfn_args>))
        }
    }

    fn callback_async_tsfn_type(
        &self,
        tsfn_args: TokenStream,
        result_ty: TokenStream,
    ) -> TokenStream {
        if self.async_callbacks_return_directly() {
            // `napi-ohos` invokes a TSFN with the direct synchronous return
            // value. Keep the default error-first convention enabled: ArkTS
            // completes the direct TSFN return callback only through this
            // `CalleeHandled=true` path.
            quote!(
                ThreadsafeFunction<#tsfn_args, #result_ty, #tsfn_args, napi::Status, true>
            )
        } else {
            quote!(ThreadsafeFunction<#tsfn_args, napi::bindgen_prelude::Promise<#result_ty>>)
        }
    }

    fn render_async_callback_await(
        &self,
        result_ident: &syn::Ident,
        result_ty: Option<TokenStream>,
        callback: TokenStream,
        call_value: TokenStream,
        dispatch_error: &str,
        rejected_error: &str,
        object_name: &str,
        method_name: &str,
    ) -> TokenStream {
        let dispatch_error = format!("callback trait `{{}}`.{{}} {dispatch_error}: {{}}");
        let rejected_error = format!("callback trait `{{}}`.{{}} {rejected_error}: {{}}");
        let result_type_annotation = result_ty.map(|ty| quote!(: #ty));
        if self.async_callbacks_return_directly() {
            quote! {
                let #result_ident #result_type_annotation = #callback.call_async(Ok(#call_value)).await.unwrap_or_else(|err| {
                    panic!(#dispatch_error, #object_name, #method_name, err);
                });
            }
        } else {
            quote! {
                let __callback_promise = #callback.call_async(Ok(#call_value)).await.unwrap_or_else(|err| {
                    panic!(#dispatch_error, #object_name, #method_name, err);
                });
                let #result_ident #result_type_annotation = __callback_promise.await.unwrap_or_else(|err| {
                    panic!(#rejected_error, #object_name, #method_name, err);
                });
            }
        }
    }

    fn render_callback_result_struct(
        &self,
        object: &Object,
        method: &Method,
    ) -> Result<TokenStream> {
        let result_ident = self.callback_result_ident(object, method);
        let error_ty = self.bridge_return_type(
            method
                .throws_type()
                .expect("result structs are only rendered for fallible callbacks"),
        )?;
        let value_field = if let Some(return_type) = method.return_type() {
            if let Some(inner_type) = self.ohos_async_callback_optional_return_inner(method) {
                // A `#[napi(object)]` field of `Option<Option<T>>` cannot
                // distinguish an envelope field whose value is `null` from
                // an envelope field that is missing altogether.  The former
                // is a valid `None` callback result, while the latter must
                // remain a malformed callback-envelope diagnostic.
                //
                // Keep the result value singly optional and make its
                // presence explicit for the OHOS direct-return ABI.
                let value_ty = self.callback_async_bridge_type(inner_type)?;
                quote!(
                    pub has_value: bool,
                    pub value: Option<#value_ty>,
                )
            } else {
                let value_ty = if method.is_async() {
                    self.callback_async_bridge_type(return_type)?
                } else {
                    self.callback_bridge_type(return_type)?
                };
                quote!(pub value: Option<#value_ty>,)
            }
        } else {
            quote!()
        };
        Ok(quote! {
            #[napi(object)]
            pub struct #result_ident {
                pub ok: bool,
                #value_field
                pub error: Option<#error_ty>,
            }
        })
    }

    fn render_callback_impl_method(&self, object: &Object, method: &Method) -> Result<TokenStream> {
        let method_ident = rust_ident(method.name());
        let return_value_ident = format_ident!("__callback_return");
        let object_name = object.name().to_string();
        let method_name = method.name().to_string();
        let args = method
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                let arg_ty = self.core_callback_return_type(&arg.as_type())?;
                Ok(quote!(#arg_ident: #arg_ty))
            })
            .collect::<Result<Vec<_>>>()?;

        let call_value = self.callback_call_value(method)?;
        let registry_call_value = self.callback_registry_call_value(method)?;
        let registry_field_ident = self.callback_registry_field_ident(object.name(), method.name());
        if method.is_async() {
            if method.throws_type().is_some() {
                let result_ty = self.callback_result_ident(object, method);
                let result_ident = format_ident!("__callback_result");
                let error_ty = self.core_type_path(
                    method
                        .throws_type()
                        .expect("fallible async callbacks must have an error type")
                        .clone(),
                );
                let return_ty = match method.return_type() {
                    Some(return_type) => self.core_callback_return_type(return_type)?,
                    None => quote!(()),
                };
                let success = if let Some(return_type) = method.return_type() {
                    if let Some(inner_type) = self.ohos_async_callback_optional_return_inner(method)
                    {
                        let lowered = self.lower_async_callback_value_expr(
                            quote!(__callback_value),
                            inner_type,
                        )?;
                        quote! {
                            if __callback_result.has_value {
                                let __callback_value = __callback_result.value.unwrap_or_else(|| {
                                    panic!(
                                        "callback trait `{}`.{} returned ok with hasValue=true but without a value",
                                        #object_name,
                                        #method_name
                                    );
                                });
                                Ok(Some(#lowered))
                            } else {
                                if __callback_result.value.is_some() {
                                    panic!(
                                        "callback trait `{}`.{} returned ok with hasValue=false but with a value",
                                        #object_name,
                                        #method_name
                                    );
                                }
                                Ok(None)
                            }
                        }
                    } else {
                        let lowered = self.lower_async_callback_value_expr(
                            quote!(__callback_value),
                            return_type,
                        )?;
                        quote! {
                            let __callback_value = __callback_result.value.unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} returned ok without a value",
                                    #object_name,
                                    #method_name
                                );
                            });
                            Ok(#lowered)
                        }
                    }
                } else {
                    quote!(Ok(()))
                };
                let lowered_error = self.lower_callback_value_expr(
                    quote!(__callback_error),
                    method.throws_type().unwrap(),
                )?;
                let registry_await = self.render_async_callback_await(
                    &result_ident,
                    Some(quote!(#result_ty)),
                    quote!(__registry),
                    registry_call_value.clone(),
                    "failed to dispatch returned async JS callback",
                    "returned async JS callback rejected",
                    &object_name,
                    &method_name,
                );
                let direct_await = self.render_async_callback_await(
                    &result_ident,
                    Some(quote!(#result_ty)),
                    quote!(__callback),
                    call_value.clone(),
                    "failed to call async JS callback",
                    "async JS callback rejected",
                    &object_name,
                    &method_name,
                );
                return Ok(quote! {
                    async fn #method_ident(&self, #(#args),*) -> std::result::Result<#return_ty, #error_ty> {
                        if let Some(__id) = self.__uniffi_callback_registry_id {
                            let __registry = self.#registry_field_ident.as_ref().unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} has no returned-callback dispatcher",
                                    #object_name,
                                    #method_name
                                );
                            });
                            #registry_await
                            if __callback_result.ok {
                                #success
                            } else {
                                let __callback_error = __callback_result.error.unwrap_or_else(|| {
                                    panic!(
                                        "callback trait `{}`.{} returned err without a typed error",
                                        #object_name,
                                        #method_name
                                    );
                                });
                                Err(#lowered_error)
                            }
                        } else {
                        let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no JS callback",
                                #object_name,
                                #method_name
                            );
                        });
                        #direct_await
                        if __callback_result.ok {
                            #success
                        } else {
                            let __callback_error = __callback_result.error.unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} returned err without a typed error",
                                    #object_name,
                                    #method_name
                                );
                            });
                            Err(#lowered_error)
                        }
                        }
                    }
                });
            }
            return match method.return_type() {
                Some(return_type) => {
                    let return_ty = self.core_callback_return_type(return_type)?;
                    let lowered = self.lower_async_callback_value_expr(
                        quote!(#return_value_ident),
                        return_type,
                    )?;
                    let registry_await = self.render_async_callback_await(
                        &return_value_ident,
                        None,
                        quote!(__registry),
                        registry_call_value.clone(),
                        "failed to dispatch returned async JS callback",
                        "returned async JS callback rejected",
                        &object_name,
                        &method_name,
                    );
                    let direct_await = self.render_async_callback_await(
                        &return_value_ident,
                        None,
                        quote!(__callback),
                        call_value.clone(),
                        "failed to call async JS callback",
                        "async JS callback rejected",
                        &object_name,
                        &method_name,
                    );
                    Ok(quote! {
                        async fn #method_ident(&self, #(#args),*) -> #return_ty {
                            if let Some(__id) = self.__uniffi_callback_registry_id {
                                let __registry = self.#registry_field_ident.as_ref().unwrap_or_else(|| {
                                    panic!(
                                        "callback trait `{}`.{} has no returned-callback dispatcher",
                                        #object_name,
                                        #method_name
                                    );
                                });
                                #registry_await
                                #lowered
                            } else {
                            let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} has no JS callback",
                                    #object_name,
                                    #method_name
                                );
                            });
                            #direct_await
                            #lowered
                            }
                        }
                    })
                }
                None => {
                    let completion_ident = format_ident!("__callback_completion");
                    let registry_await = self.render_async_callback_await(
                        &completion_ident,
                        None,
                        quote!(__registry),
                        registry_call_value,
                        "failed to dispatch returned async JS callback",
                        "returned async JS callback rejected",
                        &object_name,
                        &method_name,
                    );
                    let direct_await = self.render_async_callback_await(
                        &completion_ident,
                        None,
                        quote!(__callback),
                        call_value,
                        "failed to call async JS callback",
                        "async JS callback rejected",
                        &object_name,
                        &method_name,
                    );
                    Ok(quote! {
                    async fn #method_ident(&self, #(#args),*) {
                        if let Some(__id) = self.__uniffi_callback_registry_id {
                            let __registry = self.#registry_field_ident.as_ref().unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} has no returned-callback dispatcher",
                                    #object_name,
                                    #method_name
                                );
                            });
                            #registry_await
                        } else {
                        let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no JS callback",
                                #object_name,
                                #method_name
                            );
                        });
                        #direct_await
                        }
                    }
                    })
                }
            };
        }
        if let Some(throws_type) = method.throws_type() {
            let return_ty = match method.return_type() {
                Some(return_type) => self.core_callback_return_type(return_type)?,
                None => quote!(()),
            };
            let error_ty = self.core_type_path(throws_type.clone());
            let method_name = method.name().to_string();
            let success = if let Some(return_type) = method.return_type() {
                let lowered =
                    self.lower_callback_value_expr(quote!(__callback_return), return_type)?;
                quote! {
                    let __callback_return = __callback_result.value.unwrap_or_else(|| {
                        panic!(
                            "callback trait `{}`.{} returned ok without a value",
                            #object_name,
                            #method_name
                        );
                    });
                    Ok(#lowered)
                }
            } else {
                quote!(Ok(()))
            };
            let lowered_error =
                self.lower_callback_value_expr(quote!(__callback_error), throws_type)?;
            return Ok(quote! {
                fn #method_ident(&self, #(#args),*) -> std::result::Result<#return_ty, #error_ty> {
                    if let Some(__id) = self.__uniffi_callback_registry_id {
                        let __registry = self.#registry_field_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no returned-callback dispatcher",
                                #object_name,
                                #method_name
                            );
                        });
                        let (__sender, __receiver) = std::sync::mpsc::channel();
                        let __status = __registry.call_with_return_value(
                            Ok(#registry_call_value),
                            ThreadsafeFunctionCallMode::NonBlocking,
                            move |__result, _| {
                                __sender.send(__result).or(Ok(()))
                            },
                        );
                        if __status != napi::Status::Ok {
                            panic!(
                                "callback trait `{}`.{} failed to dispatch returned JS callback: {}",
                                #object_name,
                                #method_name,
                                __status
                            );
                        }
                        let __callback_result = __receiver.recv().unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} failed to receive returned JS callback result: {}",
                                #object_name,
                                #method_name,
                                err
                            );
                        }).unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} returned JS callback threw an unexpected JS error: {}",
                                #object_name,
                                #method_name,
                                err
                            );
                        });
                        if __callback_result.ok {
                            #success
                        } else {
                            let __callback_error = __callback_result.error.unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} returned err without a typed error",
                                    #object_name,
                                    #method_name
                                );
                            });
                            Err(#lowered_error)
                        }
                    } else {
                    let __env = napi::bindgen_prelude::Env::from_raw(
                        self.env.unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no JS env",
                                #object_name,
                                #method_name
                            );
                        }) as napi::bindgen_prelude::sys::napi_env,
                    );
                    let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                        panic!(
                            "callback trait `{}`.{} has no JS callback",
                            #object_name,
                            #method_name
                        );
                    });
                    let __callback_result = __callback.borrow_back(&__env).unwrap_or_else(|err| {
                        panic!(
                            "callback trait `{}`.{} failed to borrow JS function: {}",
                            #object_name,
                            #method_name,
                            err
                        );
                    }).call(#call_value).unwrap_or_else(|err| {
                        panic!(
                            "callback trait `{}`.{} threw an unexpected JS error: {}",
                            #object_name,
                            #method_name,
                            err
                        );
                    });
                    if __callback_result.ok {
                        #success
                    } else {
                        let __callback_error = __callback_result.error.unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} returned err without a typed error",
                                #object_name,
                                #method_name
                            );
                        });
                        Err(#lowered_error)
                    }
                    }
                }
            });
        }
        match method.return_type() {
            Some(return_type) => {
                let return_ty = self.core_callback_return_type(return_type)?;
                let lowered =
                    self.lower_callback_value_expr(quote!(#return_value_ident), return_type)?;
                let method_name = method.name().to_string();
                Ok(quote! {
                    fn #method_ident(&self, #(#args),*) -> #return_ty {
                        if let Some(__id) = self.__uniffi_callback_registry_id {
                            let __registry = self.#registry_field_ident.as_ref().unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} has no returned-callback dispatcher",
                                    #object_name,
                                    #method_name
                                );
                            });
                            let (__sender, __receiver) = std::sync::mpsc::channel();
                            let __status = __registry.call_with_return_value(
                                Ok(#registry_call_value),
                                ThreadsafeFunctionCallMode::NonBlocking,
                                move |__result, _| {
                                    __sender.send(__result).or(Ok(()))
                                },
                            );
                            if __status != napi::Status::Ok {
                                panic!(
                                    "callback trait `{}`.{} failed to dispatch returned JS callback: {}",
                                    #object_name,
                                    #method_name,
                                    __status
                                );
                            }
                            let #return_value_ident = __receiver.recv().unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} failed to receive returned JS callback result: {}",
                                    #object_name,
                                    #method_name,
                                    err
                                );
                            }).unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} returned JS callback threw in JS callback: {}",
                                    #object_name,
                                    #method_name,
                                    err
                                );
                            });
                            #lowered
                        } else {
                        let __env = napi::bindgen_prelude::Env::from_raw(
                            self.env.unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} has no JS env",
                                    #object_name,
                                    #method_name
                                );
                            }) as napi::bindgen_prelude::sys::napi_env,
                        );
                        let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no JS callback",
                                #object_name,
                                #method_name
                            );
                        });
                        let #return_value_ident = __callback.borrow_back(&__env).unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} failed to borrow JS function: {}",
                                #object_name,
                                #method_name,
                                err
                            );
                        }).call(#call_value).unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} threw in JS callback: {}",
                                #object_name,
                                #method_name,
                                err
                            );
                        });
                        #lowered
                        }
                    }
                })
            }
            None => Ok(quote! {
                fn #method_ident(&self, #(#args),*) {
                    if let Some(__id) = self.__uniffi_callback_registry_id {
                        let __registry = self.#registry_field_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no returned-callback dispatcher",
                                #object_name,
                                #method_name
                            );
                        });
                        let _ = __registry.call(Ok(#registry_call_value), ThreadsafeFunctionCallMode::NonBlocking);
                    } else {
                        let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no JS callback",
                                #object_name,
                                #method_name
                            );
                        });
                        let _ = __callback.call(Ok(#call_value), ThreadsafeFunctionCallMode::NonBlocking);
                    }
                }
            }),
        }
    }

    fn render_callback_from_napi_impl(
        &self,
        object: &Object,
        needs_env: bool,
    ) -> Result<TokenStream> {
        let ident = rust_ident(object.name());
        let type_name = object.name();
        let env_init = needs_env.then(|| quote!(env: Some(env as usize),));
        let needs_return_dispatcher = self.callback_object_needs_return_dispatcher(object);
        let field_inits = object
            .methods()
            .into_iter()
            .map(|method| {
                let field_ident = rust_ident(method.name());
                let field_name = crate::js_names::method_name(method.name());
                let ty = self.callback_field_from_napi_type(object, method)?;
                Ok(quote! {
                    #field_ident: Some(obj.get_named_property_unchecked::<#ty>(#field_name)?),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let registry_inits = self
            .callback_registry_field_defs()?
            .into_iter()
            .map(|(field_ident, ty)| {
                if needs_return_dispatcher {
                    quote! {
                        #field_ident: Some(std::sync::Arc::new(
                            obj.get_named_property_unchecked::<#ty>("__uniffiCallbackDispatcher")?
                        )),
                    }
                } else {
                    quote!(#field_ident: None,)
                }
            })
            .collect::<Vec<_>>();

        Ok(quote! {
            impl napi::bindgen_prelude::TypeName for #ident {
                fn type_name() -> &'static str {
                    #type_name
                }

                fn value_type() -> napi::ValueType {
                    napi::ValueType::Object
                }
            }

            impl napi::bindgen_prelude::ValidateNapiValue for #ident {
                unsafe fn validate(
                    env: napi::bindgen_prelude::sys::napi_env,
                    napi_val: napi::bindgen_prelude::sys::napi_value,
                ) -> napi::bindgen_prelude::Result<napi::bindgen_prelude::sys::napi_value> {
                    match napi::bindgen_prelude::type_of!(env, napi_val)? {
                        napi::ValueType::Object => Ok(std::ptr::null_mut()),
                        _ => Err(napi::bindgen_prelude::Error::new(
                            napi::bindgen_prelude::Status::InvalidArg,
                            format!("Value is not a `{}` callback object", #type_name),
                        )),
                    }
                }
            }

            impl napi::bindgen_prelude::FromNapiValue for #ident {
                unsafe fn from_napi_value(
                    env: napi::bindgen_prelude::sys::napi_env,
                    napi_val: napi::bindgen_prelude::sys::napi_value,
                ) -> napi::bindgen_prelude::Result<Self> {
                    let mut __scope = std::ptr::null_mut();
                    napi::check_status!(
                        unsafe { napi::bindgen_prelude::sys::napi_open_handle_scope(env, &mut __scope) },
                        "Failed to open callback wrapper handle scope"
                    )?;
                    let __result = (|| -> napi::bindgen_prelude::Result<Self> {
                        let obj = napi::bindgen_prelude::Object::from_napi_value(env, napi_val)?;
                        let obj = if obj
                            .get_named_property_unchecked::<bool>("__uniffiCallback")
                            .unwrap_or(false)
                        {
                            obj.get_named_property_unchecked::<napi::bindgen_prelude::Object>("object")?
                        } else {
                            obj
                        };
                        Ok(Self {
                            #(#field_inits)*
                            #(#registry_inits)*
                            __uniffi_callback_registry_id: None,
                            #env_init
                        })
                    })();
                    let __close_status = unsafe {
                        napi::bindgen_prelude::sys::napi_close_handle_scope(env, __scope)
                    };
                    napi::check_status!(
                        __close_status,
                        "Failed to close callback wrapper handle scope"
                    )?;
                    __result
                }
            }

            impl napi::bindgen_prelude::ToNapiValue for #ident {
                unsafe fn to_napi_value(
                    _env: napi::bindgen_prelude::sys::napi_env,
                    _val: Self,
                ) -> napi::bindgen_prelude::Result<napi::bindgen_prelude::sys::napi_value> {
                    Err(napi::bindgen_prelude::Error::new(
                        napi::bindgen_prelude::Status::GenericFailure,
                        "callback wrapper values are inbound-only",
                    ))
                }
            }
        })
    }

    fn callback_field_from_napi_type(
        &self,
        object: &Object,
        method: &Method,
    ) -> Result<TokenStream> {
        self.render_callback_direct_field_type(object, method)
    }

    fn render_constructor(
        &self,
        object: &Object,
        constructor: &Constructor,
    ) -> Result<TokenStream> {
        let function_name = crate::dispatch_key::constructor_key(object.name(), constructor);
        let fn_ident = rust_ident(&function_name);
        let object_ident = rust_ident(object.name());
        let object_ty = object.as_type();
        let core_path = self.core_type_path(object.as_type());
        let args = constructor
            .arguments()
            .into_iter()
            .map(|arg| self.render_signature_arg(arg))
            .collect::<Result<Vec<_>>>()?;
        let lowered = constructor
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                self.lower_arg_expr(arg_ident, &arg.as_type())
            })
            .collect::<Result<Vec<_>>>()?;
        let core_fn_ident = rust_ident(constructor.name());

        let call = quote!(#core_path::#core_fn_ident(#(#lowered),*));
        let call = if constructor.is_async() {
            quote!(#call.await)
        } else {
            call
        };
        let body = self.render_result_body(call, Some(&object_ty), constructor.throws_type())?;

        if constructor.is_async() {
            Ok(quote! {
                #[napi]
                pub async fn #fn_ident(#(#args),*) -> Result<#object_ident> {
                    #body
                }
            })
        } else {
            Ok(quote! {
                #[napi]
                pub fn #fn_ident(#(#args),*) -> Result<#object_ident> {
                    #body
                }
            })
        }
    }

    fn render_object_method(&self, method: &Method) -> Result<TokenStream> {
        let object_ident = rust_ident(method.object_name());
        let function_name = crate::dispatch_key::object_method_key(method);
        let fn_ident = rust_ident(&function_name);
        let method_ident = rust_ident(method.name());
        let receiver_ident = rust_ident("handle");
        let args = method
            .arguments()
            .into_iter()
            .map(|arg| self.render_signature_arg(arg))
            .collect::<Result<Vec<_>>>()?;
        let lowered = method
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                self.lower_arg_expr(arg_ident, &arg.as_type())
            })
            .collect::<Result<Vec<_>>>()?;
        let output_ty = match method.return_type() {
            Some(return_type) => self.bridge_return_type(return_type)?,
            None => quote!(()),
        };

        if method.is_async() {
            // `ClassInstance` contains N-API state and is not `Send`.  The
            // synchronous N-API entrypoint lowers all arguments, clones the
            // core `Arc`, and releases the receiver before it creates the
            // Send future that will cross an async suspension.
            let lowered_bindings = method
                .arguments()
                .into_iter()
                .map(|arg| {
                    let arg_ident = rust_ident(arg.name());
                    let lowered_ident = rust_ident(&format!("__uniffi_{}", arg.name()));
                    let lowered_expr = self.lower_arg_expr(arg_ident, &arg.as_type())?;
                    Ok(quote!(let #lowered_ident = #lowered_expr;))
                })
                .collect::<Result<Vec<_>>>()?;
            let lowered_args = method
                .arguments()
                .into_iter()
                .map(|arg| rust_ident(&format!("__uniffi_{}", arg.name())))
                .collect::<Vec<_>>();
            let async_receiver = if method.takes_self_by_arc() {
                quote!(__uniffi_core)
            } else {
                quote!(__uniffi_core.as_ref())
            };
            let call = quote!(#async_receiver.#method_ident(#(#lowered_args),*).await);
            let body = self.render_result_body(call, method.return_type(), method.throws_type())?;
            Ok(quote! {
                #[napi]
                pub fn #fn_ident(__uniffi_env: Env, #receiver_ident: ClassInstance<'_, #object_ident>, #(#args),*) -> Result<PromiseRaw<'static, #output_ty>> {
                    #(#lowered_bindings)*
                    let __uniffi_core = (*(#receiver_ident)).0.clone();
                    let __uniffi_future = async move {
                        #body
                    };
                    drop(#receiver_ident);
                    let __uniffi_promise = __uniffi_env.spawn_future(__uniffi_future)?;
                    Ok(unsafe {
                        // The raw JS promise is returned immediately; the lifetime only
                        // ties PromiseRaw to the Env used to create it.
                        std::mem::transmute::<PromiseRaw<'_, #output_ty>, PromiseRaw<'static, #output_ty>>(
                            __uniffi_promise,
                        )
                    })
                }
            })
        } else {
            let receiver = if method.takes_self_by_arc() {
                quote!((*(#receiver_ident)).0.clone())
            } else {
                quote!((*(#receiver_ident)).0.as_ref())
            };
            let call = quote!(#receiver.#method_ident(#(#lowered),*));
            let body = self.render_result_body(call, method.return_type(), method.throws_type())?;
            Ok(quote! {
                #[napi]
                pub fn #fn_ident(#receiver_ident: ClassInstance<'_, #object_ident>, #(#args),*) -> Result<#output_ty> {
                    #body
                }
            })
        }
    }

    fn render_value_method(
        &self,
        owner_name: &str,
        owner_ty: &Type,
        method: &Method,
    ) -> Result<TokenStream> {
        let function_name = crate::dispatch_key::method_key(owner_name, method);
        let fn_ident = rust_ident(&function_name);
        let self_ident = rust_ident("self_");
        let self_bridge_ty = self.bridge_value_type(owner_ty)?;
        let self_core_ty = self.core_type_path(owner_ty.clone());
        let self_core = self.lower_value_expr(quote!(#self_ident), owner_ty)?;
        let args = method
            .arguments()
            .into_iter()
            .map(|arg| self.render_signature_arg(arg))
            .collect::<Result<Vec<_>>>()?;
        let lowered_arg_bindings = method
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                let local_ident = rust_ident(&format!("__arg_{}", arg.name()));
                let lowered = self.lower_arg_expr(arg_ident, &arg.as_type())?;
                Ok(quote!(let #local_ident = #lowered;))
            })
            .collect::<Result<Vec<_>>>()?;
        let call_args = method
            .arguments()
            .into_iter()
            .map(|arg| {
                let local_ident = rust_ident(&format!("__arg_{}", arg.name()));
                if arg.by_ref() || matches!(arg.as_type(), Type::Record { .. } | Type::Enum { .. })
                {
                    quote!(&#local_ident)
                } else {
                    quote!(#local_ident)
                }
            })
            .collect::<Vec<_>>();
        let method_ident = rust_ident(method.name());
        let call = quote!({
            let __self: #self_core_ty = #self_core;
            #(#lowered_arg_bindings)*
            __self.#method_ident(#(#call_args),*)
        });
        let call = if method.is_async() {
            quote!(#call.await)
        } else {
            call
        };
        let output_ty = match method.return_type() {
            Some(return_type) => self.bridge_return_type(return_type)?,
            None => quote!(()),
        };
        let body = self.render_result_body(call, method.return_type(), method.throws_type())?;

        if method.is_async() {
            Ok(quote! {
                #[napi]
                pub async fn #fn_ident(#self_ident: #self_bridge_ty, #(#args),*) -> Result<#output_ty> {
                    #body
                }
            })
        } else {
            Ok(quote! {
                #[napi]
                pub fn #fn_ident(#self_ident: #self_bridge_ty, #(#args),*) -> Result<#output_ty> {
                    #body
                }
            })
        }
    }

    fn render_value_constructor(
        &self,
        owner_name: &str,
        owner_ty: &Type,
        constructor: &Constructor,
    ) -> Result<TokenStream> {
        let function_name = crate::dispatch_key::constructor_key(owner_name, constructor);
        let fn_ident = rust_ident(&function_name);
        let output_ty = self.bridge_return_type(owner_ty)?;
        let core_path = self.core_type_path(owner_ty.clone());
        let args = constructor
            .arguments()
            .into_iter()
            .map(|arg| self.render_signature_arg(arg))
            .collect::<Result<Vec<_>>>()?;
        let lowered_arg_bindings = constructor
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                let local_ident = rust_ident(&format!("__arg_{}", arg.name()));
                let lowered = self.lower_arg_expr(arg_ident, &arg.as_type())?;
                Ok(quote!(let #local_ident = #lowered;))
            })
            .collect::<Result<Vec<_>>>()?;
        let call_args = constructor
            .arguments()
            .into_iter()
            .map(|arg| {
                let local_ident = rust_ident(&format!("__arg_{}", arg.name()));
                if arg.by_ref() || matches!(arg.as_type(), Type::Record { .. } | Type::Enum { .. })
                {
                    quote!(&#local_ident)
                } else {
                    quote!(#local_ident)
                }
            })
            .collect::<Vec<_>>();
        let core_fn_ident = rust_ident(constructor.name());
        let call = quote!({
            #(#lowered_arg_bindings)*
            #core_path::#core_fn_ident(#(#call_args),*)
        });
        let call = if constructor.is_async() {
            quote!(#call.await)
        } else {
            call
        };
        let body = self.render_result_body(call, Some(owner_ty), constructor.throws_type())?;

        if constructor.is_async() {
            Ok(quote! {
                #[napi]
                pub async fn #fn_ident(#(#args),*) -> Result<#output_ty> {
                    #body
                }
            })
        } else {
            Ok(quote! {
                #[napi]
                pub fn #fn_ident(#(#args),*) -> Result<#output_ty> {
                    #body
                }
            })
        }
    }

    fn render_function(&self, function: &Function) -> Result<TokenStream> {
        if let Some(Type::Stream {
            item_type,
            error_type,
            ..
        }) = function.return_type()
        {
            return self.render_stream_function(function, item_type, error_type);
        }
        let fn_ident = rust_ident(function.name());
        let fn_path = self.core_item_path(self.ci.crate_name(), function.name());
        let args = function
            .arguments()
            .into_iter()
            .map(|arg| self.render_signature_arg(arg))
            .collect::<Result<Vec<_>>>()?;
        let lowered = function
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                self.lower_arg_expr(arg_ident, &arg.as_type())
            })
            .collect::<Result<Vec<_>>>()?;
        let has_class_instance_arg = function.arguments().into_iter().any(|arg| {
            matches!(
                arg.as_type(),
                Type::Object {
                    imp: ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly),
                    ..
                }
            )
        });
        let call = quote!(#fn_path(#(#lowered),*));
        let call = if function.is_async() {
            quote!(#call.await)
        } else {
            call
        };
        let output_ty = match function.return_type() {
            Some(return_type) => self.bridge_return_type(return_type)?,
            None => quote!(()),
        };
        let body = self.render_result_body(call, function.return_type(), function.throws_type())?;

        if function.is_async() {
            if has_class_instance_arg {
                let lowered_bindings = function
                    .arguments()
                    .into_iter()
                    .map(|arg| {
                        let arg_ident = rust_ident(arg.name());
                        let lowered_ident = rust_ident(&format!("__uniffi_{}", arg.name()));
                        let lowered_expr = self.lower_arg_expr(arg_ident, &arg.as_type())?;
                        Ok(quote!(let #lowered_ident = #lowered_expr;))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lowered_args = function
                    .arguments()
                    .into_iter()
                    .map(|arg| rust_ident(&format!("__uniffi_{}", arg.name())))
                    .collect::<Vec<_>>();
                let call = quote!(#fn_path(#(#lowered_args),*).await);
                let body =
                    self.render_result_body(call, function.return_type(), function.throws_type())?;
                Ok(quote! {
                    #[napi]
                    pub fn #fn_ident(__uniffi_env: Env, #(#args),*) -> Result<PromiseRaw<'static, #output_ty>> {
                        #(#lowered_bindings)*
                        let __uniffi_promise = __uniffi_env.spawn_future(async move {
                            #body
                        })?;
                        Ok(unsafe {
                            // The raw JS promise is returned immediately; the lifetime only
                            // ties PromiseRaw to the Env used to create it.
                            std::mem::transmute::<PromiseRaw<'_, #output_ty>, PromiseRaw<'static, #output_ty>>(
                                __uniffi_promise,
                            )
                        })
                    }
                })
            } else {
                Ok(quote! {
                    #[napi]
                    pub async fn #fn_ident(#(#args),*) -> Result<#output_ty> {
                        #body
                    }
                })
            }
        } else {
            Ok(quote! {
                #[napi]
                pub fn #fn_ident(#(#args),*) -> Result<#output_ty> {
                    #body
                }
            })
        }
    }

    fn render_stream_function(
        &self,
        function: &Function,
        item_type: &Type,
        error_type: &Type,
    ) -> Result<TokenStream> {
        let fn_ident = rust_ident(function.name());
        let next_ident = rust_ident(&crate::dispatch_key::stream_next_key(function.name()));
        let cancel_ident = rust_ident(&crate::dispatch_key::stream_cancel_key(function.name()));
        let next_struct_ident = self.stream_next_struct_ident(function);
        let registry_ident = self.stream_registry_ident(function);
        let fn_path = self.core_item_path(self.ci.crate_name(), function.name());
        let args = function
            .arguments()
            .into_iter()
            .map(|arg| self.render_signature_arg(arg))
            .collect::<Result<Vec<_>>>()?;
        let lowered = function
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                self.lower_arg_expr(arg_ident, &arg.as_type())
            })
            .collect::<Result<Vec<_>>>()?;
        let item_core_ty = self.core_value_type(item_type)?;
        let error_core_ty = self.core_value_type(error_type)?;
        let item_bridge_ty = self.bridge_return_type(item_type)?;
        let error_bridge_ty = self.bridge_return_type(error_type)?;
        let lifted_value = self.lift_value_expr(quote!(value), item_type)?;
        let lifted_error = self.lift_value_expr(quote!(err), error_type)?;

        Ok(quote! {
            static #registry_ident: ::uniffi::RustStreamRegistry<#item_core_ty, #error_core_ty> =
                ::uniffi::deps::once_cell::sync::Lazy::new(|| ::std::sync::Mutex::new(::std::collections::HashMap::new()));

            #[napi(object)]
            pub struct #next_struct_ident {
                pub done: bool,
                pub value: Option<#item_bridge_ty>,
                pub error: Option<#error_bridge_ty>,
            }

            #[napi]
            pub fn #fn_ident(#(#args),*) -> Result<BigInt> {
                let stream = #fn_path(#(#lowered),*);
                let handle = ::uniffi::rust_stream_new(&#registry_ident, stream);
                Ok(BigInt::from(handle.as_raw()))
            }

            #[napi]
            pub async fn #next_ident(handle: BigInt) -> Result<#next_struct_ident> {
                let handle = __uniffi_stream_handle_from_bigint(handle)?;
                match ::uniffi::rust_stream_next_async::<#item_core_ty, #error_core_ty>(
                    &#registry_ident,
                    handle,
                )
                .await
                {
                    Ok(Ok(Some(value))) => Ok(#next_struct_ident {
                        done: false,
                        value: Some(#lifted_value),
                        error: None,
                    }),
                    Ok(Ok(None)) => Ok(#next_struct_ident {
                        done: true,
                        value: None,
                        error: None,
                    }),
                    Ok(Err(err)) => Ok(#next_struct_ident {
                        done: false,
                        value: None,
                        error: Some(#lifted_error),
                    }),
                    Err(err) => Err(Error::new(Status::GenericFailure, format!("{err:?}"))),
                }
            }

            #[napi]
            pub fn #cancel_ident(handle: BigInt) -> Result<()> {
                let handle = __uniffi_stream_handle_from_bigint(handle)?;
                ::uniffi::rust_stream_cancel::<#item_core_ty, #error_core_ty>(
                    &#registry_ident,
                    handle,
                );
                Ok(())
            }
        })
    }

    fn render_signature_arg(&self, arg: &Argument) -> Result<TokenStream> {
        let ident = rust_ident(arg.name());
        let ty = self.bridge_arg_type(&arg.as_type())?;
        Ok(quote!(#ident: #ty))
    }

    fn render_result_body(
        &self,
        call: TokenStream,
        return_type: Option<&Type>,
        throws_type: Option<&Type>,
    ) -> Result<TokenStream> {
        match (return_type, throws_type) {
            (Some(return_type), Some(_)) => {
                let lifted = self.lift_value_expr(quote!(value), return_type)?;
                Ok(quote! {
                    match #call {
                        Ok(value) => Ok(#lifted),
                        Err(err) => Err(into_napi_error(err)),
                    }
                })
            }
            (Some(return_type), None) => {
                let lifted = self.lift_value_expr(call, return_type)?;
                Ok(quote!(Ok(#lifted)))
            }
            (None, Some(_)) => Ok(quote!(#call.map_err(into_napi_error))),
            (None, None) => Ok(quote!(Ok(#call))),
        }
    }

    fn bridge_arg_type(&self, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::InputStream { .. } => {
                let next_ident = self.input_stream_next_result_ident(ty)?;
                Ok(quote!(__UniffiInputStream<#next_ident>))
            }
            Type::Object { name, imp, .. } => {
                let ident = rust_ident(name);
                match imp {
                    ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                        Ok(quote!(ClassInstance<'_, #ident>))
                    }
                    ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                        Ok(quote!(#ident))
                    }
                }
            }
            Type::Optional { inner_type } => {
                let inner = self.bridge_arg_type(inner_type)?;
                Ok(quote!(Option<#inner>))
            }
            Type::Sequence { inner_type } => {
                let inner = self.bridge_arg_type(inner_type)?;
                Ok(quote!(Vec<#inner>))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.bridge_value_type(key_type)?;
                let value = self.bridge_value_type(value_type)?;
                Ok(quote!(std::collections::HashMap<#key, #value>))
            }
            _ => self.bridge_value_type(ty),
        }
    }

    fn bridge_return_type(&self, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::Optional { inner_type } => {
                let inner = self.bridge_return_type(inner_type)?;
                Ok(quote!(Option<#inner>))
            }
            Type::Sequence { inner_type } => {
                let inner = self.bridge_return_type(inner_type)?;
                Ok(quote!(Vec<#inner>))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.bridge_value_type(key_type)?;
                let value = self.bridge_return_type(value_type)?;
                Ok(quote!(std::collections::HashMap<#key, #value>))
            }
            _ => self.bridge_value_type(ty),
        }
    }

    fn bridge_value_type(&self, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::UInt8 => Ok(quote!(u8)),
            Type::Int8 => Ok(quote!(i8)),
            Type::UInt16 => Ok(quote!(u16)),
            Type::Int16 => Ok(quote!(i16)),
            Type::UInt32 => Ok(quote!(u32)),
            Type::Int32 => Ok(quote!(i32)),
            Type::UInt64 | Type::Int64 => Ok(quote!(napi::bindgen_prelude::BigInt)),
            // N-API (including napi-ohos) exposes JavaScript numbers as f64.
            // Keep f32 in the core API and convert at the bridge boundary.
            Type::Float32 => Ok(quote!(f64)),
            Type::Float64 => Ok(quote!(f64)),
            Type::Boolean => Ok(quote!(bool)),
            Type::String => Ok(quote!(String)),
            Type::Bytes => Ok(quote!(Buffer)),
            Type::Record { name, .. } | Type::Enum { name, .. } => {
                let ident = rust_ident(name);
                Ok(quote!(#ident))
            }
            Type::Object { name, imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    let ident = rust_ident(name);
                    Ok(quote!(#ident))
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                    bail!("callback trait `{name}` is not supported as a nested or return value")
                }
            },
            Type::Optional { inner_type } => {
                let inner = self.bridge_value_type(inner_type)?;
                Ok(quote!(Option<#inner>))
            }
            Type::Sequence { inner_type } => {
                let inner = self.bridge_value_type(inner_type)?;
                Ok(quote!(Vec<#inner>))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.bridge_value_type(key_type)?;
                let value = self.bridge_value_type(value_type)?;
                Ok(quote!(std::collections::HashMap<#key, #value>))
            }
            Type::Box { inner_type } => self.bridge_value_type(inner_type),
            Type::Set { inner_type } => {
                let inner = self.bridge_value_type(inner_type)?;
                Ok(quote!(std::collections::HashSet<#inner>))
            }
            Type::Stream { .. } => bail!("native streams are not wired into napi bridge types yet"),
            Type::InputStream { .. } => {
                bail!("input streams are not wired into napi bridge types yet")
            }
            Type::Timestamp => Ok(quote!(__UniffiTimestamp)),
            Type::Duration => Ok(quote!(__UniffiDuration)),
            Type::CallbackInterface { name, .. } => {
                let ident = rust_ident(name);
                Ok(quote!(#ident))
            }
            Type::Custom { builtin, .. } => self.bridge_value_type(builtin),
        }
    }

    fn lower_arg_expr(&self, ident: syn::Ident, ty: &Type) -> Result<TokenStream> {
        match ty {
            // u64/i64 cross the napi boundary as BigInt (JS `bigint`).
            // napi-rs does not impl FromNapiValue for u64, and i64 maps
            // to JS `number` which loses precision. BigInt keeps the raw
            // addon surface aligned with the public bigint-first contract.
            Type::UInt64 => Ok(quote!({
                let (__sign, __val, __lossless) = #ident.get_u64();
                if __sign && __val != 0 {
                    return Err(napi::Error::new(napi::Status::InvalidArg, "negative value cannot be converted to u64"));
                }
                if !__lossless {
                    return Err(napi::Error::new(napi::Status::InvalidArg, "BigInt value does not fit into u64"));
                }
                __val
            })),
            Type::Int64 => Ok(quote!({
                let (__val, __lossless) = #ident.get_i64();
                if !__lossless {
                    return Err(napi::Error::new(napi::Status::InvalidArg, "BigInt value does not fit into i64"));
                }
                __val
            })),
            Type::Object { imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    Ok(quote!((*(#ident)).0.clone()))
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                    let trait_path = self.core_type_path(ty.clone());
                    Ok(quote!(std::sync::Arc::new(#ident) as std::sync::Arc<dyn #trait_path>))
                }
            },
            Type::InputStream { .. } => {
                let ops_ident = self.input_stream_ops_ident(ty)?;
                Ok(quote!({
                    let __stream = #ident;
                    ::uniffi::UniFfiInputStream::from_handle_and_ops(
                        ::uniffi::Handle::from_raw_unchecked(u64::from(__stream.handle)),
                        std::sync::Arc::new(#ops_ident {
                            next: __stream.next.clone(),
                            cancel: __stream.cancel.clone(),
                            _phantom: std::marker::PhantomData,
                        }),
                    )
                }))
            }
            Type::Custom {
                module_path,
                builtin,
                ..
            } => {
                let builtin_lower = self.lower_arg_expr(ident, builtin)?;
                let builtin_ty = self.core_value_type(builtin)?;
                let custom_ty = self.core_type_path(ty.clone());
                let tag_ty = self.core_tag_path(module_path);
                Ok(quote!({
                    let __builtin = { #builtin_lower };
                    let __ffi = <#builtin_ty as ::uniffi::Lower<#tag_ty>>::lower(__builtin);
                    <#custom_ty as ::uniffi::Lift<#tag_ty>>::try_lift(__ffi)
                        .map_err(into_napi_error)?
                }))
            }
            _ => self.lower_value_expr(quote!(#ident), ty),
        }
    }

    fn lower_value_expr(&self, expr: TokenStream, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::UInt8
            | Type::Int8
            | Type::UInt16
            | Type::Int16
            | Type::UInt32
            | Type::Int32
            | Type::Float64
            | Type::Boolean
            | Type::String => Ok(expr),
            // JavaScript numbers reach the N-API bridge as f64. Rust's `as`
            // conversion preserves the existing JS binding behavior for NaN
            // and infinities while applying the required IEEE-754 f32 rounding
            // (or overflow to infinity) for the core contract.
            Type::Float32 => Ok(quote!(#expr as f32)),
            // BigInt → u64: reject negative and out-of-range values.
            Type::UInt64 => Ok(quote!({
                let __big = #expr;
                let (__sign, __val, __lossless) = __big.get_u64();
                if __sign && __val != 0 {
                    panic!("negative BigInt value cannot be converted to u64");
                }
                if !__lossless {
                    panic!("BigInt value does not fit into u64");
                }
                __val
            })),
            // BigInt → i64: reject values outside the i64 range.
            Type::Int64 => Ok(quote!({
                let __big = #expr;
                let (__val, __lossless) = __big.get_i64();
                if !__lossless {
                    panic!("BigInt value does not fit into i64");
                }
                __val
            })),
            Type::Bytes => Ok(quote!(#expr.into())),
            Type::Record { .. } | Type::Enum { .. } => Ok(quote!(#expr.into())),
            Type::Object { imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    Ok(quote!(#expr.0.clone()))
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                    bail!("callback traits are not supported here")
                }
            },
            Type::Optional { inner_type } => {
                let inner = self.lower_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.map(|value| { #inner })))
            }
            Type::Sequence { inner_type } => {
                let inner = self.lower_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.into_iter().map(|value| { #inner }).collect()))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.lower_value_expr(quote!(key), key_type)?;
                let value = self.lower_value_expr(quote!(value), value_type)?;
                Ok(quote!(
                    #expr
                        .into_iter()
                        .map(|(key, value)| ({ #key }, { #value }))
                        .collect()
                ))
            }
            Type::Box { inner_type } => self.lower_value_expr(expr, inner_type),
            Type::Set { inner_type } => {
                let inner = self.lower_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.into_iter().map(|value| { #inner }).collect()))
            }
            Type::Stream { .. } => bail!("native streams are not wired into napi lowering yet"),
            Type::InputStream { .. } => {
                bail!("input streams are not wired into napi lowering yet")
            }
            Type::Timestamp => Ok(quote!(#expr.0)),
            Type::Duration => Ok(quote!(#expr.0)),
            Type::CallbackInterface { name, .. } => {
                bail!("callback interface `{name}` is not supported directly")
            }
            Type::Custom {
                module_path,
                builtin,
                ..
            } => {
                let builtin_lower = self.lower_value_expr(expr, builtin)?;
                let builtin_ty = self.core_value_type(builtin)?;
                let custom_ty = self.core_type_path(ty.clone());
                let tag_ty = self.core_tag_path(module_path);
                Ok(quote!({
                    let __builtin = { #builtin_lower };
                    let __ffi = <#builtin_ty as ::uniffi::Lower<#tag_ty>>::lower(__builtin);
                    <#custom_ty as ::uniffi::Lift<#tag_ty>>::try_lift(__ffi)
                        .expect("uniffi napi custom type lift failed")
                }))
            }
        }
    }

    fn lift_value_expr(&self, expr: TokenStream, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::UInt8
            | Type::Int8
            | Type::UInt16
            | Type::Int16
            | Type::UInt32
            | Type::Int32
            | Type::Float64
            | Type::Boolean
            | Type::String => Ok(expr),
            // N-API has no f32 value conversion; JavaScript observes every
            // number as f64 while the core API remains f32.
            Type::Float32 => Ok(quote!(#expr as f64)),
            // u64/i64 → BigInt for JS `bigint`.
            Type::UInt64 | Type::Int64 => Ok(quote!(napi::bindgen_prelude::BigInt::from(#expr))),
            Type::Bytes => Ok(quote!(#expr.into())),
            Type::Record { .. } | Type::Enum { .. } => Ok(quote!(#expr.into())),
            Type::Object { name, imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    let ident = rust_ident(name);
                    Ok(quote!(#ident(#expr)))
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => bail!(
                    "callback trait `{name}` cannot be returned to JavaScript in the napi bridge"
                ),
            },
            Type::Optional { inner_type } => {
                let inner = self.lift_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.map(|value| { #inner })))
            }
            Type::Sequence { inner_type } => {
                let inner = self.lift_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.into_iter().map(|value| { #inner }).collect()))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.lift_value_expr(quote!(key), key_type)?;
                let value = self.lift_value_expr(quote!(value), value_type)?;
                Ok(quote!(
                    #expr
                        .into_iter()
                        .map(|(key, value)| ({ #key }, { #value }))
                        .collect()
                ))
            }
            Type::Box { inner_type } => self.lift_value_expr(expr, inner_type),
            Type::Set { inner_type } => {
                let inner = self.lift_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.into_iter().map(|value| { #inner }).collect()))
            }
            Type::Stream { .. } => bail!("native streams are not wired into napi lifting yet"),
            Type::InputStream { .. } => bail!("input streams are not wired into napi lifting yet"),
            Type::Timestamp => Ok(quote!(__UniffiTimestamp(#expr))),
            Type::Duration => Ok(quote!(__UniffiDuration(#expr))),
            Type::CallbackInterface { name, .. } => {
                bail!("callback interface `{name}` cannot be returned directly")
            }
            Type::Custom { module_path, .. } => {
                let custom_ty = self.core_type_path(ty.clone());
                let tag_ty = self.core_tag_path(module_path);
                let builtin = match ty {
                    Type::Custom { builtin, .. } => builtin.as_ref(),
                    _ => unreachable!(),
                };
                let builtin_ty = self.core_value_type(builtin)?;
                let builtin_value = quote!({
                    let __builtin = <#builtin_ty as ::uniffi::Lift<#tag_ty>>::try_lift(
                        <#custom_ty as ::uniffi::Lower<#tag_ty>>::lower(#expr),
                    )
                    .expect("uniffi napi custom type lift failed");
                    __builtin
                });
                self.lift_value_expr(builtin_value, builtin)
            }
        }
    }

    fn callback_tsfn_args(&self, method: &Method) -> Result<TokenStream> {
        match method.arguments().as_slice() {
            [] => Ok(quote!(())),
            [arg] => self.bridge_value_type(&arg.as_type()),
            args => {
                let tys = args
                    .iter()
                    .map(|arg| self.bridge_value_type(&arg.as_type()))
                    .collect::<Result<Vec<_>>>()?;
                Ok(quote!(FnArgs<(#(#tys),*)>))
            }
        }
    }

    fn callback_registry_tsfn_args(&self, method: &Method) -> Result<TokenStream> {
        let arg_tys = method
            .arguments()
            .into_iter()
            .map(|arg| self.bridge_value_type(&arg.as_type()))
            .collect::<Result<Vec<_>>>()?;
        Ok(quote!(FnArgs<(u32, String, #(#arg_tys),*)>))
    }

    fn callback_registry_call_value(&self, method: &Method) -> Result<TokenStream> {
        let method_name = crate::js_names::method_name(method.name());
        let args = method
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                self.lift_value_expr(quote!(#arg_ident), &arg.as_type())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(quote!(FnArgs::from((__id, #method_name.to_string(), #(#args),*))))
    }

    fn callback_object_needs_return_dispatcher(&self, object: &Object) -> bool {
        object.methods().into_iter().any(|method| {
            method.is_async()
                && method
                    .return_type()
                    .is_some_and(callback_metadata::is_callback_return_type)
        })
    }

    /// Returns the inner type when an OHOS direct-return fallible async
    /// callback has a top-level optional success value.
    ///
    /// The callback envelope itself needs an optional `value` field for
    /// fallible methods.  Representing an optional success value directly
    /// would otherwise generate `Option<Option<T>>`, which N-API cannot
    /// faithfully reconstruct from JavaScript `null` versus a missing field.
    fn ohos_async_callback_optional_return_inner<'b>(
        &self,
        method: &'b Method,
    ) -> Option<&'b Type> {
        if !self.async_callbacks_return_directly() || !method.is_async() {
            return None;
        }
        match method.return_type() {
            Some(Type::Optional { inner_type }) => Some(inner_type),
            _ => None,
        }
    }

    fn callback_async_bridge_type(&self, ty: &Type) -> Result<TokenStream> {
        if callback_metadata::is_callback_return_type(ty) {
            Ok(quote!(__UniffiCallbackHandle))
        } else if callback_metadata::contains_callback_return_type(ty) {
            bail!(
                "async callback methods returning nested callback traits/interfaces are not supported in the N-API/Electron backend yet"
            )
        } else {
            self.callback_bridge_type(ty)
        }
    }

    fn callback_bridge_type(&self, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::Optional { inner_type } => {
                let inner = self.callback_bridge_type(inner_type)?;
                Ok(quote!(Option<#inner>))
            }
            Type::Sequence { inner_type } => {
                let inner = self.callback_bridge_type(inner_type)?;
                Ok(quote!(Vec<#inner>))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.bridge_value_type(key_type)?;
                let value = self.callback_bridge_type(value_type)?;
                Ok(quote!(std::collections::HashMap<#key, #value>))
            }
            Type::Object { name, imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    let ident = rust_ident(name);
                    Ok(quote!(napi::bindgen_prelude::ClassInstance<'static, #ident>))
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                    let ident = rust_ident(name);
                    Ok(quote!(#ident))
                }
            },
            _ => self.bridge_return_type(ty),
        }
    }

    fn lower_callback_value_expr(&self, expr: TokenStream, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::UInt8
            | Type::Int8
            | Type::UInt16
            | Type::Int16
            | Type::UInt32
            | Type::Int32
            | Type::Float64
            | Type::Boolean
            | Type::String => Ok(expr),
            Type::Float32 => Ok(quote!(#expr as f32)),
            Type::UInt64 => Ok(quote!({
                let __big = #expr;
                let (__sign, __val, __lossless) = __big.get_u64();
                if __sign && __val != 0 {
                    panic!("callback returned a negative value for u64");
                }
                if !__lossless {
                    panic!("callback returned a BigInt value that does not fit into u64");
                }
                __val
            })),
            Type::Int64 => Ok(quote!({
                let __big = #expr;
                let (__val, __lossless) = __big.get_i64();
                if !__lossless {
                    panic!("callback returned a BigInt value that does not fit into i64");
                }
                __val
            })),
            Type::Bytes => Ok(quote!(#expr.into())),
            Type::Record { .. } | Type::Enum { .. } => Ok(quote!(#expr.into())),
            Type::Object { imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly) => {
                    Ok(quote!((*(#expr)).0.clone()))
                }
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly) => {
                    let core_path = self.core_type_path(ty.clone());
                    Ok(quote!(std::sync::Arc::new(#expr) as std::sync::Arc<dyn #core_path>))
                }
            },
            Type::Optional { inner_type } => {
                let inner = self.lower_callback_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.map(|value| { #inner })))
            }
            Type::Sequence { inner_type } => {
                let inner = self.lower_callback_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.into_iter().map(|value| { #inner }).collect()))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.lower_callback_value_expr(quote!(key), key_type)?;
                let value = self.lower_callback_value_expr(quote!(value), value_type)?;
                Ok(quote!(
                    #expr
                        .into_iter()
                        .map(|(key, value)| ({ #key }, { #value }))
                        .collect()
                ))
            }
            Type::Box { inner_type } => self.lower_callback_value_expr(expr, inner_type),
            Type::Set { inner_type } => {
                let inner = self.lower_callback_value_expr(quote!(value), inner_type)?;
                Ok(quote!(#expr.into_iter().map(|value| { #inner }).collect()))
            }
            Type::Stream { .. } => {
                bail!("native streams are not supported in callback values yet")
            }
            Type::InputStream { .. } => {
                bail!("input streams are not supported in callback values yet")
            }
            Type::Timestamp => Ok(quote!(#expr.0)),
            Type::Duration => Ok(quote!(#expr.0)),
            Type::CallbackInterface { .. } => {
                let core_path = self.core_type_path(ty.clone());
                Ok(quote!(std::sync::Arc::new(#expr) as std::sync::Arc<dyn #core_path>))
            }
            Type::Custom {
                module_path,
                builtin,
                ..
            } => {
                let builtin_lower = self.lower_callback_value_expr(expr, builtin)?;
                let builtin_ty = self.core_value_type(builtin)?;
                let custom_ty = self.core_type_path(ty.clone());
                let tag_ty = self.core_tag_path(module_path);
                Ok(quote!({
                    let __builtin = { #builtin_lower };
                    let __ffi = <#builtin_ty as ::uniffi::Lower<#tag_ty>>::lower(__builtin);
                    <#custom_ty as ::uniffi::Lift<#tag_ty>>::try_lift(__ffi)
                        .expect("uniffi napi custom callback type lift failed")
                }))
            }
        }
    }

    fn lower_async_callback_value_expr(&self, expr: TokenStream, ty: &Type) -> Result<TokenStream> {
        if callback_metadata::is_callback_return_type(ty) {
            self.lower_callback_handle_expr(expr, ty)
        } else {
            self.lower_callback_value_expr(expr, ty)
        }
    }

    fn lower_callback_handle_expr(&self, expr: TokenStream, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::Object {
                name,
                imp: ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly),
                ..
            }
            | Type::CallbackInterface { name, .. } => {
                let ident = rust_ident(name);
                let core_path = self.core_type_path(ty.clone());
                let dispatch_args = self
                    .callback_registry_field_defs()?
                    .into_iter()
                    .map(|(field_ident, _)| quote!(self.#field_ident.clone()))
                    .collect::<Vec<_>>();
                Ok(quote!({
                    let __handle = #expr;
                    std::sync::Arc::new(#ident::__uniffi_from_callback_registry(
                        __handle.id,
                        #(#dispatch_args),*
                    )) as std::sync::Arc<dyn #core_path>
                }))
            }
            _ => self.lower_callback_value_expr(expr, ty),
        }
    }

    fn callback_call_value(&self, method: &Method) -> Result<TokenStream> {
        let args = method
            .arguments()
            .into_iter()
            .map(|arg| {
                let arg_ident = rust_ident(arg.name());
                self.lift_value_expr(quote!(#arg_ident), &arg.as_type())
            })
            .collect::<Result<Vec<_>>>()?;
        match args.as_slice() {
            [] => Ok(quote!(())),
            [arg] => Ok(quote!(#arg)),
            _ => Ok(quote!((#(#args),*).into())),
        }
    }

    fn core_callback_return_type(&self, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::UInt8 => Ok(quote!(u8)),
            Type::Int8 => Ok(quote!(i8)),
            Type::UInt16 => Ok(quote!(u16)),
            Type::Int16 => Ok(quote!(i16)),
            Type::UInt32 => Ok(quote!(u32)),
            Type::Int32 => Ok(quote!(i32)),
            Type::UInt64 => Ok(quote!(u64)),
            Type::Int64 => Ok(quote!(i64)),
            Type::Float32 => Ok(quote!(f32)),
            Type::Float64 => Ok(quote!(f64)),
            Type::Boolean => Ok(quote!(bool)),
            Type::String => Ok(quote!(String)),
            Type::Bytes => Ok(quote!(Vec<u8>)),
            Type::Record { .. } | Type::Enum { .. } | Type::Custom { .. } => {
                let core_path = self.core_type_path(ty.clone());
                Ok(quote!(#core_path))
            }
            Type::Object { imp, .. } => match imp {
                ObjectImpl::Struct => {
                    let core_path = self.core_type_path(ty.clone());
                    Ok(quote!(std::sync::Arc<#core_path>))
                }
                ObjectImpl::Trait(_) => {
                    let core_path = self.core_type_path(ty.clone());
                    Ok(quote!(std::sync::Arc<dyn #core_path>))
                }
            },
            Type::Optional { inner_type } => {
                let inner = self.core_callback_return_type(inner_type)?;
                Ok(quote!(Option<#inner>))
            }
            Type::Sequence { inner_type } => {
                let inner = self.core_callback_return_type(inner_type)?;
                Ok(quote!(Vec<#inner>))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.core_callback_return_type(key_type)?;
                let value = self.core_callback_return_type(value_type)?;
                Ok(quote!(std::collections::HashMap<#key, #value>))
            }
            Type::Box { inner_type } => self.core_callback_return_type(inner_type),
            Type::Set { inner_type } => {
                let inner = self.core_callback_return_type(inner_type)?;
                Ok(quote!(std::collections::HashSet<#inner>))
            }
            Type::Stream { .. } => {
                bail!("native streams are not supported in callback returns yet")
            }
            Type::InputStream { .. } => {
                bail!("input streams are not supported in callback returns yet")
            }
            Type::Timestamp => Ok(quote!(::std::time::SystemTime)),
            Type::Duration => Ok(quote!(::std::time::Duration)),
            Type::CallbackInterface { .. } => {
                let core_path = self.core_type_path(ty.clone());
                Ok(quote!(std::sync::Arc<dyn #core_path>))
            }
        }
    }

    fn core_object_inner_type(&self, object: &Object) -> TokenStream {
        let core_path = self.core_type_path(object.as_type());
        match object.imp() {
            ObjectImpl::Struct => quote!(std::sync::Arc<#core_path>),
            ObjectImpl::Trait(_) => {
                quote!(std::sync::Arc<dyn #core_path>)
            }
        }
    }

    fn core_item_path(&self, module_path: &str, name: &str) -> TokenStream {
        let module = rust_path(module_path);
        let ident = rust_ident(name);
        quote!(#module::#ident)
    }

    fn core_public_item_path(&self, module_path: &str, public_path: &str) -> TokenStream {
        let crate_root = module_path.split("::").next().unwrap_or(module_path);
        let crate_ident = rust_ident(crate_root);
        let public_path = rust_path(public_path);
        quote!(#crate_ident::#public_path)
    }

    fn core_record_path(&self, module_path: &str, name: &str) -> TokenStream {
        let Some(record) = self.ci.get_record_definition(name) else {
            return self.core_item_path(module_path, name);
        };
        match record.rust_path() {
            Some(public_path) => self.core_public_item_path(module_path, public_path),
            None => self.core_item_path(module_path, record.rust_name()),
        }
    }

    fn core_enum_path(&self, module_path: &str, name: &str) -> TokenStream {
        let Some(enum_) = self.ci.get_enum_definition(name) else {
            return self.core_item_path(module_path, name);
        };
        match enum_.rust_path() {
            Some(public_path) => self.core_public_item_path(module_path, public_path),
            None => self.core_item_path(module_path, enum_.rust_name()),
        }
    }

    fn core_type_path(&self, ty: Type) -> TokenStream {
        match ty {
            Type::Record { module_path, name } => self.core_record_path(&module_path, &name),
            Type::Enum { module_path, name } => self.core_enum_path(&module_path, &name),
            Type::CallbackInterface { module_path, name }
            | Type::Object {
                module_path, name, ..
            }
            | Type::Custom {
                module_path, name, ..
            } => {
                let module = rust_path(&module_path);
                let ident = rust_ident(&name);
                quote!(#module::#ident)
            }
            _ => unreachable!("core_type_path only supports named types"),
        }
    }

    fn core_value_type(&self, ty: &Type) -> Result<TokenStream> {
        match ty {
            Type::UInt8 => Ok(quote!(u8)),
            Type::Int8 => Ok(quote!(i8)),
            Type::UInt16 => Ok(quote!(u16)),
            Type::Int16 => Ok(quote!(i16)),
            Type::UInt32 => Ok(quote!(u32)),
            Type::Int32 => Ok(quote!(i32)),
            Type::UInt64 => Ok(quote!(u64)),
            Type::Int64 => Ok(quote!(i64)),
            Type::Float32 => Ok(quote!(f32)),
            Type::Float64 => Ok(quote!(f64)),
            Type::Boolean => Ok(quote!(bool)),
            Type::String => Ok(quote!(String)),
            Type::Bytes => Ok(quote!(Vec<u8>)),
            Type::Record { .. }
            | Type::Enum { .. }
            | Type::CallbackInterface { .. }
            | Type::Object { .. }
            | Type::Custom { .. } => Ok(self.core_type_path(ty.clone())),
            Type::Optional { inner_type } => {
                let inner = self.core_value_type(inner_type)?;
                Ok(quote!(Option<#inner>))
            }
            Type::Sequence { inner_type } => {
                let inner = self.core_value_type(inner_type)?;
                Ok(quote!(Vec<#inner>))
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                let key = self.core_value_type(key_type)?;
                let value = self.core_value_type(value_type)?;
                Ok(quote!(std::collections::HashMap<#key, #value>))
            }
            Type::Box { inner_type } => self.core_value_type(inner_type),
            Type::Set { inner_type } => {
                let inner = self.core_value_type(inner_type)?;
                Ok(quote!(std::collections::HashSet<#inner>))
            }
            Type::Stream { .. } => bail!("nested native stream types are not supported"),
            Type::InputStream { .. } => bail!("nested input stream types are not supported"),
            Type::Timestamp => Ok(quote!(::std::time::SystemTime)),
            Type::Duration => Ok(quote!(::std::time::Duration)),
        }
    }

    fn core_tag_path(&self, module_path: &str) -> TokenStream {
        let crate_root = module_path
            .split("::")
            .next()
            .expect("custom type module path should have a crate root");
        let root = rust_ident(crate_root);
        quote!(#root::UniFfiTag)
    }

    fn callback_result_ident(&self, object: &Object, method: &Method) -> syn::Ident {
        format_ident!(
            "__Uniffi{}{}CallbackResult",
            object.name(),
            method.name().to_upper_camel_case()
        )
    }

    fn stream_registry_ident(&self, function: &Function) -> syn::Ident {
        format_ident!(
            "__UNIFFI_{}_STREAMS",
            function.name().to_snake_case().to_uppercase()
        )
    }

    fn stream_next_struct_ident(&self, function: &Function) -> syn::Ident {
        format_ident!(
            "__Uniffi{}StreamNext",
            function.name().to_upper_camel_case()
        )
    }

    fn input_stream_next_result_ident(&self, ty: &Type) -> Result<syn::Ident> {
        Ok(format_ident!(
            "__UniffiInputStream{}Next",
            describe_input_stream_type(ty)?.suffix()
        ))
    }

    fn input_stream_ops_ident(&self, ty: &Type) -> Result<syn::Ident> {
        Ok(format_ident!(
            "__UniffiInputStream{}Ops",
            describe_input_stream_type(ty)?.suffix()
        ))
    }
}

/// The single source of truth for an input stream bridge specialization.
///
/// `canonical` is a length-framed structural encoding of the item/error pair.
/// It deliberately ignores the outer `is_send` bit because that bit does not
/// change the foreign stream operations or their generated bridge types.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct InputStreamDescriptor {
    input_type: Type,
    item_type: Type,
    error_type: Type,
    canonical: String,
    fingerprint: String,
    suffix: String,
}

impl InputStreamDescriptor {
    pub(super) fn input_type(&self) -> &Type {
        &self.input_type
    }

    pub(super) fn item_type(&self) -> &Type {
        &self.item_type
    }

    pub(super) fn error_type(&self) -> &Type {
        &self.error_type
    }

    pub(super) fn canonical(&self) -> &str {
        &self.canonical
    }

    pub(super) fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub(super) fn suffix(&self) -> &str {
        &self.suffix
    }
}

/// Collect every input stream type accepted by the N-API generator.
///
/// Keep this callable list aligned with `Generator::validate`: top-level
/// functions, Rust-owned object constructors/methods, and value-type
/// constructors/methods are generated. Foreign callback methods are not
/// included because input streams are not supported as callback arguments.
pub(super) fn collect_input_stream_descriptors(
    ci: &ComponentInterface,
) -> Result<Vec<InputStreamDescriptor>> {
    collect_input_stream_descriptors_with_builders(
        ci,
        stable_fingerprint,
        build_input_stream_suffix,
    )
}

/// Describe one direct input-stream argument using the same canonical rules as
/// the component collector. This is used at individual lowering sites after
/// the component-wide collector has performed collision validation.
pub(super) fn describe_input_stream_type(ty: &Type) -> Result<InputStreamDescriptor> {
    describe_input_stream_type_with_builders(ty, stable_fingerprint, build_input_stream_suffix)
}

fn collect_input_stream_descriptors_with_builders<Fingerprint, Suffix>(
    ci: &ComponentInterface,
    fingerprint_builder: Fingerprint,
    suffix_builder: Suffix,
) -> Result<Vec<InputStreamDescriptor>>
where
    Fingerprint: Fn(&str) -> String,
    Suffix: Fn(&str, &str, &Type, &Type) -> String,
{
    let candidates = collect_input_stream_candidates(ci)?;
    let mut by_canonical = std::collections::BTreeMap::<String, InputStreamDescriptor>::new();
    let mut canonical_origins = std::collections::BTreeMap::<String, String>::new();
    let mut fingerprint_owners = std::collections::BTreeMap::<String, (String, String)>::new();
    let mut suffix_owners = std::collections::BTreeMap::<String, (String, String)>::new();

    for (ty, origin) in candidates {
        let descriptor =
            describe_input_stream_type_with_builders(&ty, &fingerprint_builder, &suffix_builder)?;
        if let Some(existing) = by_canonical.get(descriptor.canonical()) {
            ensure!(
                existing.item_type() == descriptor.item_type()
                    && existing.error_type() == descriptor.error_type()
                    && existing.fingerprint() == descriptor.fingerprint()
                    && existing.suffix() == descriptor.suffix(),
                "canonical input stream descriptor collision between `{}` and `{origin}` for `{}`",
                canonical_origins
                    .get(descriptor.canonical())
                    .map(String::as_str)
                    .unwrap_or("unknown callable"),
                descriptor.canonical()
            );
            continue;
        }

        if let Some((owned_canonical, owned_origin)) =
            fingerprint_owners.get(descriptor.fingerprint())
        {
            bail!(
                "input stream descriptor fingerprint collision for `{}`: `{owned_origin}` uses `{owned_canonical}`, but `{origin}` uses `{}`",
                descriptor.fingerprint(),
                descriptor.canonical()
            );
        }

        if let Some((owned_canonical, owned_origin)) = suffix_owners.get(descriptor.suffix()) {
            bail!(
                "input stream descriptor suffix collision for `{}`: `{owned_origin}` uses `{owned_canonical}`, but `{origin}` uses `{}`",
                descriptor.suffix(),
                descriptor.canonical()
            );
        }

        suffix_owners.insert(
            descriptor.suffix().to_string(),
            (descriptor.canonical().to_string(), origin.clone()),
        );
        fingerprint_owners.insert(
            descriptor.fingerprint().to_string(),
            (descriptor.canonical().to_string(), origin.clone()),
        );
        canonical_origins.insert(descriptor.canonical().to_string(), origin);
        by_canonical.insert(descriptor.canonical().to_string(), descriptor);
    }

    Ok(by_canonical.into_values().collect())
}

fn collect_input_stream_candidates(ci: &ComponentInterface) -> Result<Vec<(Type, String)>> {
    let mut candidates = Vec::new();

    for function in ci.function_definitions() {
        collect_callable_input_stream_candidates(
            function,
            &format!("function `{}`", function.name()),
            &mut candidates,
        )?;
    }

    for object in ci.object_definitions() {
        if !matches!(
            object.imp(),
            ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly)
        ) {
            continue;
        }
        for constructor in object.constructors() {
            collect_callable_input_stream_candidates(
                constructor,
                &format!(
                    "object `{}` constructor `{}`",
                    object.name(),
                    constructor.name()
                ),
                &mut candidates,
            )?;
        }
        for method in object.methods() {
            collect_callable_input_stream_candidates(
                method,
                &format!("object `{}` method `{}`", object.name(), method.name()),
                &mut candidates,
            )?;
        }
    }

    for record in ci.record_definitions() {
        for constructor in record.constructors() {
            collect_callable_input_stream_candidates(
                constructor,
                &format!(
                    "record `{}` constructor `{}`",
                    record.name(),
                    constructor.name()
                ),
                &mut candidates,
            )?;
        }
        for method in record.methods() {
            collect_callable_input_stream_candidates(
                method,
                &format!("record `{}` method `{}`", record.name(), method.name()),
                &mut candidates,
            )?;
        }
    }

    for enum_ in ci.enum_definitions() {
        for constructor in enum_.constructors() {
            collect_callable_input_stream_candidates(
                constructor,
                &format!(
                    "enum `{}` constructor `{}`",
                    enum_.name(),
                    constructor.name()
                ),
                &mut candidates,
            )?;
        }
        for method in enum_.methods() {
            collect_callable_input_stream_candidates(
                method,
                &format!("enum `{}` method `{}`", enum_.name(), method.name()),
                &mut candidates,
            )?;
        }
    }

    Ok(candidates)
}

fn collect_callable_input_stream_candidates(
    callable: &dyn Callable,
    callable_label: &str,
    out: &mut Vec<(Type, String)>,
) -> Result<()> {
    for argument in callable.arguments() {
        let ty = argument.as_type();
        if matches!(ty, Type::InputStream { .. }) {
            out.push((
                ty,
                format!("{callable_label} argument `{}`", argument.name()),
            ));
            continue;
        }
        if ty
            .iter_types()
            .any(|nested| matches!(nested, Type::InputStream { .. }))
        {
            bail!(
                "{callable_label} argument `{}` contains a nested input stream; input streams are only supported as direct arguments",
                argument.name()
            );
        }
    }
    Ok(())
}

fn describe_input_stream_type_with_builders<Fingerprint, Suffix>(
    ty: &Type,
    fingerprint_builder: Fingerprint,
    suffix_builder: Suffix,
) -> Result<InputStreamDescriptor>
where
    Fingerprint: Fn(&str) -> String,
    Suffix: Fn(&str, &str, &Type, &Type) -> String,
{
    let Type::InputStream {
        item_type,
        error_type,
        ..
    } = ty
    else {
        bail!("input stream descriptor requested for a non-input-stream type")
    };

    let item_canonical = canonical_type(item_type);
    let error_canonical = canonical_type(error_type);
    let canonical = canonical_node(
        "input-stream-descriptor",
        &[item_canonical, error_canonical],
    );
    let fingerprint = fingerprint_builder(&canonical);
    let suffix = suffix_builder(&canonical, &fingerprint, item_type, error_type);
    ensure!(
        suffix
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic()),
        "input stream descriptor suffix must begin with an ASCII letter"
    );
    ensure!(
        suffix
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'),
        "input stream descriptor suffix contains an invalid identifier character"
    );

    Ok(InputStreamDescriptor {
        input_type: ty.clone(),
        item_type: (**item_type).clone(),
        error_type: (**error_type).clone(),
        canonical,
        fingerprint,
        suffix,
    })
}

fn build_input_stream_suffix(
    _canonical: &str,
    fingerprint: &str,
    item_type: &Type,
    error_type: &Type,
) -> String {
    const MAX_READABLE_LEN: usize = 64;
    let readable = format!(
        "Item{}Error{}",
        readable_type_name(item_type),
        readable_type_name(error_type)
    );
    let readable = readable.chars().take(MAX_READABLE_LEN).collect::<String>();
    format!("{readable}Fingerprint{fingerprint}")
}

fn readable_type_name(ty: &Type) -> String {
    match ty {
        Type::UInt8 => "UInt8".into(),
        Type::Int8 => "Int8".into(),
        Type::UInt16 => "UInt16".into(),
        Type::Int16 => "Int16".into(),
        Type::UInt32 => "UInt32".into(),
        Type::Int32 => "Int32".into(),
        Type::UInt64 => "UInt64".into(),
        Type::Int64 => "Int64".into(),
        Type::Float32 => "Float32".into(),
        Type::Float64 => "Float64".into(),
        Type::Boolean => "Boolean".into(),
        Type::String => "String".into(),
        Type::Bytes => "Bytes".into(),
        Type::Timestamp => "Timestamp".into(),
        Type::Duration => "Duration".into(),
        Type::Record { name, .. } => format!("Record{}", readable_named_type(name)),
        Type::Enum { name, .. } => format!("Enum{}", readable_named_type(name)),
        Type::Object { name, .. } => format!("Object{}", readable_named_type(name)),
        Type::CallbackInterface { name, .. } => {
            format!("Callback{}", readable_named_type(name))
        }
        Type::Custom { name, .. } => format!("Custom{}", readable_named_type(name)),
        Type::Optional { inner_type } => {
            format!("Optional{}", readable_type_name(inner_type))
        }
        Type::Sequence { inner_type } => {
            format!("Sequence{}", readable_type_name(inner_type))
        }
        Type::Map {
            key_type,
            value_type,
        } => format!(
            "Map{}To{}",
            readable_type_name(key_type),
            readable_type_name(value_type)
        ),
        Type::Box { inner_type } => format!("Box{}", readable_type_name(inner_type)),
        Type::Set { inner_type } => format!("Set{}", readable_type_name(inner_type)),
        Type::Stream {
            item_type,
            error_type,
            ..
        } => format!(
            "Stream{}Error{}",
            readable_type_name(item_type),
            readable_type_name(error_type)
        ),
        Type::InputStream {
            item_type,
            error_type,
            ..
        } => format!(
            "InputStream{}Error{}",
            readable_type_name(item_type),
            readable_type_name(error_type)
        ),
    }
}

fn readable_named_type(name: &str) -> String {
    let name = sanitize_ident(name).to_upper_camel_case();
    if name.is_empty() {
        "Unnamed".to_string()
    } else {
        name
    }
}

fn canonical_type(ty: &Type) -> String {
    match ty {
        Type::UInt8 => canonical_node("uint8", &[]),
        Type::Int8 => canonical_node("int8", &[]),
        Type::UInt16 => canonical_node("uint16", &[]),
        Type::Int16 => canonical_node("int16", &[]),
        Type::UInt32 => canonical_node("uint32", &[]),
        Type::Int32 => canonical_node("int32", &[]),
        Type::UInt64 => canonical_node("uint64", &[]),
        Type::Int64 => canonical_node("int64", &[]),
        Type::Float32 => canonical_node("float32", &[]),
        Type::Float64 => canonical_node("float64", &[]),
        Type::Boolean => canonical_node("boolean", &[]),
        Type::String => canonical_node("string", &[]),
        Type::Bytes => canonical_node("bytes", &[]),
        Type::Timestamp => canonical_node("timestamp", &[]),
        Type::Duration => canonical_node("duration", &[]),
        Type::Record { module_path, name } => {
            canonical_node("record", &[module_path.clone(), name.clone()])
        }
        Type::Enum { module_path, name } => {
            canonical_node("enum", &[module_path.clone(), name.clone()])
        }
        Type::Object {
            module_path,
            name,
            imp,
        } => canonical_node(
            "object",
            &[
                module_path.clone(),
                name.clone(),
                object_impl_canonical_name(*imp).to_string(),
            ],
        ),
        Type::CallbackInterface { module_path, name } => {
            canonical_node("callback-interface", &[module_path.clone(), name.clone()])
        }
        Type::Box { inner_type } => canonical_node("box", &[canonical_type(inner_type)]),
        Type::Optional { inner_type } => canonical_node("optional", &[canonical_type(inner_type)]),
        Type::Sequence { inner_type } => canonical_node("sequence", &[canonical_type(inner_type)]),
        Type::Map {
            key_type,
            value_type,
        } => canonical_node(
            "map",
            &[canonical_type(key_type), canonical_type(value_type)],
        ),
        Type::Set { inner_type } => canonical_node("set", &[canonical_type(inner_type)]),
        Type::Stream {
            item_type,
            error_type,
            is_send,
        } => canonical_node(
            "stream",
            &[
                canonical_type(item_type),
                canonical_type(error_type),
                bool_canonical_name(*is_send).to_string(),
            ],
        ),
        Type::InputStream {
            item_type,
            error_type,
            is_send,
        } => canonical_node(
            "input-stream",
            &[
                canonical_type(item_type),
                canonical_type(error_type),
                bool_canonical_name(*is_send).to_string(),
            ],
        ),
        Type::Custom {
            module_path,
            name,
            builtin,
        } => canonical_node(
            "custom",
            &[module_path.clone(), name.clone(), canonical_type(builtin)],
        ),
    }
}

fn canonical_node(tag: &str, children: &[String]) -> String {
    let mut out = format!("{}:{tag}{}:", tag.len(), children.len());
    for child in children {
        out.push_str(&format!("{}:{child}", child.len()));
    }
    out
}

fn object_impl_canonical_name(imp: ObjectImpl) -> &'static str {
    match imp {
        ObjectImpl::Struct => "struct",
        ObjectImpl::Trait(TraitKind::RustOnly) => "trait-rust-only",
        ObjectImpl::Trait(TraitKind::Both) => "trait-both",
        ObjectImpl::Trait(TraitKind::ForeignOnly) => "trait-foreign-only",
    }
}

fn bool_canonical_name(value: bool) -> &'static str {
    if value {
        "true"
    } else {
        "false"
    }
}

fn stable_fingerprint(value: &str) -> String {
    // FNV-1a is intentionally implemented here rather than using
    // `DefaultHasher`, whose output is not a stable serialization contract.
    // The collector still checks the resulting suffix for collisions before
    // generating any bridge specialization.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

#[derive(Clone, Copy)]
enum TypeUsage {
    Arg,
    Return,
    Value,
    CallbackArg,
    CallbackReturn,
    Error,
}

fn rust_ident(name: &str) -> syn::Ident {
    syn::parse_str::<syn::Ident>(name)
        .or_else(|_| syn::parse_str::<syn::Ident>(&format!("r#{name}")))
        .unwrap_or_else(|_| format_ident!("__uniffi_{}", sanitize_ident(name)))
}

fn sanitize_ident(name: &str) -> String {
    name.chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect()
}

fn rust_path(path: &str) -> TokenStream {
    let segments = path.split("::").map(rust_ident).collect::<Vec<_>>();
    quote!(#(#segments)::*)
}

#[cfg(test)]
mod renamed_record_core_path_tests {
    use super::super::ohos_bridge_identity_export;
    use super::*;
    use uniffi_meta::{
        EnumMetadata, EnumShape, FieldMetadata, FnMetadata, MetadataGroup, NamespaceMetadata,
        RecordMetadata, VariantMetadata,
    };

    const MODULE_PATH: &str = "dual_model_fixture";

    fn named_record(name: &str) -> Type {
        Type::Record {
            module_path: MODULE_PATH.into(),
            name: name.into(),
        }
    }

    fn named_enum(name: &str) -> Type {
        Type::Enum {
            module_path: MODULE_PATH.into(),
            name: name.into(),
        }
    }

    fn field(name: &str, orig_name: Option<&str>, ty: Type) -> FieldMetadata {
        FieldMetadata {
            name: name.into(),
            orig_name: orig_name.map(Into::into),
            ty,
            default: None,
            docstring: None,
        }
    }

    fn record(
        name: &str,
        orig_name: &str,
        rust_path: &str,
        fields: Vec<FieldMetadata>,
    ) -> RecordMetadata {
        RecordMetadata {
            module_path: MODULE_PATH.into(),
            name: name.into(),
            orig_name: Some(orig_name.into()),
            rust_path: Some(rust_path.into()),
            remote: false,
            fields,
            docstring: None,
        }
    }

    fn dual_model_fixture() -> ComponentInterface {
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: MODULE_PATH.into(),
                name: MODULE_PATH.into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };

        group.add_item(
            EnumMetadata {
                module_path: MODULE_PATH.into(),
                name: "DolphinConversationRoute".into(),
                orig_name: Some("ConversationRouteKind".into()),
                rust_path: Some("ffi_types::DolphinConversationRoute".into()),
                shape: EnumShape::Enum,
                remote: false,
                variants: vec![
                    VariantMetadata {
                        name: "DirectRoute".into(),
                        orig_name: Some("Direct".into()),
                        discr: None,
                        fields: vec![],
                        docstring: None,
                    },
                    VariantMetadata {
                        name: "GroupRoute".into(),
                        orig_name: Some("Group".into()),
                        discr: None,
                        fields: vec![],
                        docstring: None,
                    },
                ],
                discr_type: None,
                non_exhaustive: false,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            record(
                "DolphinConversationRow",
                "Model",
                "ffi_types::DolphinConversationRow",
                vec![
                    field("rowRevision", Some("revision"), Type::Int64),
                    field("route", None, named_enum("DolphinConversationRoute")),
                ],
            )
            .into(),
        );
        group.add_item(
            record(
                "DolphinMessageRow",
                "Model",
                "ffi_types::DolphinMessageRow",
                vec![field("content", None, Type::String)],
            )
            .into(),
        );
        group.add_item(
            record(
                "DolphinConversationPage",
                "Page",
                "ffi_types::DolphinConversationPage",
                vec![
                    field(
                        "items",
                        None,
                        Type::Sequence {
                            inner_type: Box::new(named_record("DolphinConversationRow")),
                        },
                    ),
                    field(
                        "nextMessage",
                        Some("next_message"),
                        Type::Optional {
                            inner_type: Box::new(named_record("DolphinMessageRow")),
                        },
                    ),
                ],
            )
            .into(),
        );
        group.add_item(
            FnMetadata {
                module_path: MODULE_PATH.into(),
                name: "load_conversations".into(),
                orig_name: None,
                is_async: false,
                inputs: vec![],
                return_type: Some(named_record("DolphinConversationPage")),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );

        ComponentInterface::from_metadata(group).expect("dual Model fixture metadata")
    }

    #[test]
    fn napi_and_ohos_use_foreign_names_at_the_bridge_and_public_rust_paths_in_core() {
        let ci = dual_model_fixture();
        let napi = render_napi_rust(&ci).expect("NAPI codegen must succeed");
        let digest = "0".repeat(64);
        let identity = ohos_bridge_identity_export(&digest);
        let ohos = render_ohos_rust(&ci, &identity, &digest).expect("OHOS codegen must succeed");

        for generated in [&napi, &ohos] {
            for core_path in [
                "dual_model_fixture::ffi_types::DolphinConversationRow",
                "dual_model_fixture::ffi_types::DolphinMessageRow",
                "dual_model_fixture::ffi_types::DolphinConversationPage",
                "dual_model_fixture::ffi_types::DolphinConversationRoute",
            ] {
                assert!(
                    generated.contains(core_path),
                    "generated bridge omitted public core path `{core_path}`:\n{generated}"
                );
            }
            assert!(
                generated.contains("revision: {") && generated.contains("value.rowRevision"),
                "foreign record field must lower into its Rust field name:\n{generated}"
            );
            assert!(
                generated.contains("rowRevision:")
                    && generated.contains("BigInt::from(value.revision)"),
                "Rust record field must lift into its foreign field name:\n{generated}"
            );
            assert!(
                generated.contains("DolphinConversationRoute::DirectRoute =>")
                    && generated.contains(
                        "dual_model_fixture::ffi_types::DolphinConversationRoute::Direct"
                    ),
                "foreign enum variant must lower into its Rust variant name:\n{generated}"
            );
            assert!(
                generated
                    .contains("dual_model_fixture::ffi_types::DolphinConversationRoute::Direct =>")
                    && generated.contains("DolphinConversationRoute::DirectRoute"),
                "Rust enum variant must lift into its foreign variant name:\n{generated}"
            );
            assert!(generated.contains("pub items: Vec<DolphinConversationRow>"));
            assert!(generated.contains("pub nextMessage: Option<DolphinMessageRow>"));
        }
        assert!(ohos.contains("use napi_ohos::bindgen_prelude::*;"));
    }
}

#[cfg(test)]
mod ohos_async_callback_return_tests {
    use super::super::ohos_bridge_identity_export;
    use super::*;
    use uniffi_meta::{
        EnumMetadata, EnumShape, FnParamMetadata, MetadataGroup, MethodMetadata, NamespaceMetadata,
        ObjectMetadata, VariantMetadata,
    };

    const MODULE_PATH: &str = "ohos_async_callback_fixture";

    fn callback_object(name: &str) -> Type {
        Type::Object {
            module_path: MODULE_PATH.into(),
            name: name.into(),
            imp: ObjectImpl::Trait(TraitKind::ForeignOnly),
        }
    }

    fn async_method(
        self_name: &str,
        name: &str,
        inputs: Vec<FnParamMetadata>,
        return_type: Option<Type>,
        throws: Option<Type>,
    ) -> MethodMetadata {
        MethodMetadata {
            module_path: MODULE_PATH.into(),
            self_name: self_name.into(),
            name: name.into(),
            orig_name: None,
            is_async: true,
            inputs,
            return_type,
            throws,
            takes_self_by_arc: false,
            checksum: None,
            docstring: None,
        }
    }

    fn fixture() -> ComponentInterface {
        let callback_error = Type::Enum {
            module_path: MODULE_PATH.into(),
            name: "CallbackFailure".into(),
        };
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: MODULE_PATH.into(),
                name: MODULE_PATH.into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        for name in ["AsyncWorker", "ChildWorker"] {
            group.add_item(
                ObjectMetadata {
                    module_path: MODULE_PATH.into(),
                    name: name.into(),
                    orig_name: None,
                    remote: false,
                    imp: ObjectImpl::Trait(TraitKind::ForeignOnly),
                    docstring: None,
                }
                .into(),
            );
        }
        group.add_item(
            EnumMetadata {
                module_path: MODULE_PATH.into(),
                name: "CallbackFailure".into(),
                orig_name: None,
                rust_path: None,
                shape: EnumShape::Enum,
                remote: false,
                variants: vec![VariantMetadata {
                    name: "Failed".into(),
                    orig_name: None,
                    discr: None,
                    fields: vec![],
                    docstring: None,
                }],
                discr_type: None,
                non_exhaustive: false,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            async_method(
                "AsyncWorker",
                "compute",
                vec![FnParamMetadata::simple("value", Type::UInt32)],
                Some(Type::UInt32),
                None,
            )
            .into(),
        );
        group.add_item(
            async_method(
                "AsyncWorker",
                "checked",
                vec![FnParamMetadata::simple("value", Type::UInt32)],
                Some(Type::UInt32),
                Some(callback_error.clone()),
            )
            .into(),
        );
        group.add_item(
            async_method(
                "AsyncWorker",
                "checked_optional",
                vec![FnParamMetadata::simple("value", Type::UInt32)],
                Some(Type::Optional {
                    inner_type: Box::new(Type::UInt32),
                }),
                Some(callback_error),
            )
            .into(),
        );
        group.add_item(
            async_method(
                "AsyncWorker",
                "make_child",
                vec![],
                Some(callback_object("ChildWorker")),
                None,
            )
            .into(),
        );
        group
            .add_item(async_method("ChildWorker", "read", vec![], Some(Type::UInt32), None).into());
        ComponentInterface::from_metadata(group).expect("async callback fixture metadata")
    }

    #[test]
    fn ohos_async_callbacks_return_direct_values_while_node_keeps_promises() {
        let ci = fixture();
        let digest = "0".repeat(64);
        let identity = ohos_bridge_identity_export(&digest);
        let node = render_napi_rust(&ci).expect("Node N-API codegen must succeed");
        let ohos = render_ohos_rust(&ci, &identity, &digest).expect("OHOS codegen must succeed");
        let node_compact = node.split_whitespace().collect::<String>();
        let ohos_compact = ohos.split_whitespace().collect::<String>();

        assert!(
            node_compact.contains("ThreadsafeFunction<u32,napi::bindgen_prelude::Promise<u32>>")
                && node_compact.contains("call_async(Ok(value)).await")
                && node_compact.contains("__callback_promise.await"),
            "Node async callback ABI must remain Promise + two awaits:\n{node}"
        );
        assert!(
            ohos_compact.contains("ThreadsafeFunction<u32,u32,u32,napi_ohos::Status,true>")
                && ohos_compact.contains("call_async(Ok(value)).await")
                && ohos_compact.contains("call_async(Ok(())).await")
                && !ohos_compact.contains("__callback_promise.await"),
            "OHOS direct callback ABI must use error-first concrete returns and one await:\n{ohos}"
        );
        assert!(
            ohos.contains("__uniffi_registry_child_worker_read")
                && !ohos_compact.contains("napi_ohos::bindgen_prelude::Promise"),
            "OHOS returned-callback registry must use the same direct return ABI:\n{ohos}"
        );
        assert!(
            ohos_compact.contains("__UniffiAsyncWorkerCheckedCallbackResult")
                && ohos_compact.contains("napi_ohos::Status,true")
                && !ohos_compact.contains("napi_ohos::Status,false"),
            "OHOS fallible async callbacks must use error-first direct typed envelopes:\n{ohos}"
        );
        assert!(
            node.contains("pub value: Option<Option<u32>>")
                && !node.contains("pub has_value: bool"),
            "Node fallible async callback envelopes must preserve their Promise ABI:\n{node}"
        );
        assert!(
            ohos.contains("pub has_value: bool")
                && ohos.contains("pub value: Option<u32>")
                && !ohos.contains("pub value: Option<Option<u32>>")
                && ohos.contains("if __callback_result.has_value")
                && ohos.contains("Ok(Some(")
                && ohos.contains("Ok(None)")
                && ohos.contains("hasValue=true but without a value")
                && ohos.contains("hasValue=false but with a value"),
            "OHOS optional fallible async callbacks must use hasValue plus a single optional value:\n{ohos}"
        );
        let sidecar = super::super::render_ohos_extra_types(&ci)
            .expect("OHOS callback type sidecar must render");
        assert!(
            sidecar.contains("compute?: (value: number) => number")
                && sidecar.contains(
                    "checked?: (value: number) => UniffiAsyncWorkerCheckedCallbackResult"
                )
                && !sidecar.contains("Promise<"),
            "OHOS callback sidecar must expose synchronous callback returns:\n{sidecar}"
        );
    }
}

#[cfg(test)]
mod async_object_receiver_tests {
    use super::super::ohos_bridge_identity_export;
    use super::*;
    use uniffi_meta::{MetadataGroup, MethodMetadata, NamespaceMetadata, ObjectMetadata};

    fn fixture() -> ComponentInterface {
        let module_path = "async_object_receiver_fixture";
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: module_path.into(),
                name: module_path.into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        group.add_item(
            ObjectMetadata {
                module_path: module_path.into(),
                name: "AsyncService".into(),
                orig_name: None,
                remote: false,
                imp: ObjectImpl::Struct,
                docstring: None,
            }
            .into(),
        );
        for (name, takes_self_by_arc) in [("borrowed_async", false), ("arc_async", true)] {
            group.add_item(
                MethodMetadata {
                    module_path: module_path.into(),
                    self_name: "AsyncService".into(),
                    name: name.into(),
                    orig_name: None,
                    is_async: true,
                    inputs: vec![],
                    return_type: Some(Type::String),
                    throws: None,
                    takes_self_by_arc,
                    checksum: None,
                    docstring: None,
                }
                .into(),
            );
        }
        ComponentInterface::from_metadata(group).expect("async object receiver fixture metadata")
    }

    #[test]
    fn async_object_methods_drop_napi_receivers_before_awaiting_core_futures() {
        let ci = fixture();
        let digest = "0".repeat(64);
        let identity = ohos_bridge_identity_export(&digest);
        let rendered = [
            render_napi_rust(&ci).expect("NAPI codegen must succeed"),
            render_ohos_rust(&ci, &identity, &digest).expect("OHOS codegen must succeed"),
        ];

        for source in rendered {
            let compact = source.split_whitespace().collect::<String>();
            assert!(
                compact.contains("pubfnasync_service_borrowed_async(__uniffi_env:Env,handle:ClassInstance<'_,AsyncService>,)->Result<PromiseRaw<'static,String>>"),
                "async object receiver must start a synchronous N-API Promise boundary:\n{source}"
            );
            assert!(
                compact.contains("let__uniffi_core=(*(handle)).0.clone();"),
                "async object receiver did not clone the core Arc:\n{source}"
            );
            assert!(
                compact.contains("let__uniffi_future=asyncmove{Ok(__uniffi_core.as_ref().borrowed_async().await)}"),
                "&self async object receiver changed semantics:\n{source}"
            );
            assert!(
                compact
                    .contains("let__uniffi_future=asyncmove{Ok(__uniffi_core.arc_async().await)}"),
                "Arc<Self> async object receiver changed semantics:\n{source}"
            );
            assert_eq!(
                compact
                    .matches("drop(handle);let__uniffi_promise=__uniffi_env.spawn_future(__uniffi_future)?;")
                    .count(),
                2,
                "N-API ClassInstance must be released before creating the Send future:\n{source}"
            );
            assert!(
                !compact.contains("unsafeimplSend"),
                "the bridge must not mark N-API state Send:\n{source}"
            );
        }
    }
}

#[cfg(test)]
mod float32_bridge_tests {
    use super::super::ohos_bridge_identity_export;
    use super::*;
    use uniffi_meta::{
        FieldMetadata, FnMetadata, FnParamMetadata, MetadataGroup, NamespaceMetadata,
        RecordMetadata,
    };

    const MODULE_PATH: &str = "float32_bridge_fixture";

    fn fixture() -> ComponentInterface {
        let record_type = Type::Record {
            module_path: MODULE_PATH.into(),
            name: "Float32Record".into(),
        };
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: MODULE_PATH.into(),
                name: MODULE_PATH.into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        group.add_item(
            RecordMetadata {
                module_path: MODULE_PATH.into(),
                name: "Float32Record".into(),
                orig_name: None,
                rust_path: None,
                remote: false,
                fields: vec![FieldMetadata {
                    name: "speed".into(),
                    orig_name: None,
                    ty: Type::Float32,
                    default: None,
                    docstring: None,
                }],
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            FnMetadata {
                module_path: MODULE_PATH.into(),
                name: "roundtrip_float32_record".into(),
                orig_name: None,
                is_async: false,
                inputs: vec![FnParamMetadata::simple("value", record_type.clone())],
                return_type: Some(record_type),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        ComponentInterface::from_metadata(group).expect("float32 record fixture metadata")
    }

    #[test]
    fn float32_record_uses_js_number_f64_and_preserves_core_f32_contract() {
        let ci = fixture();
        let digest = "0".repeat(64);
        let identity = ohos_bridge_identity_export(&digest);
        let rendered = [
            render_napi_rust(&ci).expect("NAPI codegen must succeed"),
            render_ohos_rust(&ci, &identity, &digest).expect("OHOS codegen must succeed"),
        ];

        for source in rendered {
            assert!(
                source.contains("pub speed: f64"),
                "N-API record fields must use the JavaScript number bridge type:\n{source}"
            );
            assert!(
                source.contains("speed: value.speed as f32"),
                "record lowering must narrow the JS number at the core boundary:\n{source}"
            );
            assert!(
                source.contains("speed: value.speed as f64"),
                "record lifting must widen the core f32 at the JS boundary:\n{source}"
            );
            assert!(
                !source.contains("pub speed: f32"),
                "N-API must not expose an unsupported f32 field:\n{source}"
            );
        }
    }
}

#[cfg(test)]
mod input_stream_descriptor_tests {
    use super::*;
    use serde_json::Value;
    use uniffi_meta::{
        ConstructorMetadata, EnumMetadata, EnumShape, FnMetadata, FnParamMetadata, MetadataGroup,
        MethodMetadata, NamespaceMetadata, ObjectMetadata, RecordMetadata, VariantMetadata,
    };

    fn input_stream(item_type: Type, error_type: Type) -> Type {
        Type::InputStream {
            item_type: Box::new(item_type),
            error_type: Box::new(error_type),
            is_send: true,
        }
    }

    fn argument(name: &str, ty: Type) -> FnParamMetadata {
        FnParamMetadata::simple(name, ty)
    }

    fn callable_fixture() -> ComponentInterface {
        let module_path = "descriptor_fixture";
        let object_type = Type::Object {
            module_path: module_path.to_string(),
            name: "InputObject".to_string(),
            imp: ObjectImpl::Struct,
        };
        let record_type = Type::Record {
            module_path: module_path.to_string(),
            name: "InputRecord".to_string(),
        };
        let enum_type = Type::Enum {
            module_path: module_path.to_string(),
            name: "InputEnum".to_string(),
        };
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: module_path.to_string(),
                name: module_path.to_string(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };

        group.add_item(
            ObjectMetadata {
                module_path: module_path.to_string(),
                name: "InputObject".to_string(),
                orig_name: None,
                remote: false,
                imp: ObjectImpl::Struct,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            RecordMetadata {
                module_path: module_path.to_string(),
                name: "InputRecord".to_string(),
                orig_name: None,
                rust_path: None,
                remote: false,
                fields: vec![],
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            EnumMetadata {
                module_path: module_path.to_string(),
                name: "InputEnum".to_string(),
                orig_name: None,
                rust_path: None,
                shape: EnumShape::Enum,
                remote: false,
                variants: vec![VariantMetadata {
                    name: "Only".to_string(),
                    orig_name: None,
                    discr: None,
                    fields: vec![],
                    docstring: None,
                }],
                discr_type: None,
                non_exhaustive: false,
                docstring: None,
            }
            .into(),
        );

        group.add_item(
            FnMetadata {
                module_path: module_path.to_string(),
                name: "consume_free".to_string(),
                orig_name: None,
                is_async: true,
                inputs: vec![
                    argument("first", input_stream(Type::UInt8, Type::String)),
                    argument("second", input_stream(Type::UInt16, Type::String)),
                    argument("duplicate", input_stream(Type::UInt8, Type::String)),
                ],
                return_type: None,
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );

        for (self_name, self_type, constructor_item, method_item) in [
            ("InputObject", object_type, Type::UInt32, Type::Int32),
            ("InputRecord", record_type, Type::UInt64, Type::Int64),
            ("InputEnum", enum_type, Type::Float32, Type::Float64),
        ] {
            group.add_item(
                ConstructorMetadata {
                    module_path: module_path.to_string(),
                    self_name: self_name.to_string(),
                    self_type: Some(self_type.clone()),
                    name: "from_input".to_string(),
                    orig_name: None,
                    is_async: true,
                    inputs: vec![argument(
                        "source",
                        input_stream(constructor_item, Type::String),
                    )],
                    throws: None,
                    checksum: None,
                    docstring: None,
                }
                .into(),
            );
            group.add_item(
                MethodMetadata {
                    module_path: module_path.to_string(),
                    self_name: self_name.to_string(),
                    name: "merge_input".to_string(),
                    orig_name: None,
                    is_async: true,
                    inputs: vec![argument("source", input_stream(method_item, Type::String))],
                    return_type: None,
                    throws: None,
                    takes_self_by_arc: false,
                    checksum: None,
                    docstring: None,
                }
                .into(),
            );
        }

        ComponentInterface::from_metadata(group).expect("callable descriptor fixture")
    }

    fn output_stream_fixture() -> ComponentInterface {
        let module_path = "stream_error_fixture";
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: module_path.to_string(),
                name: module_path.to_string(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        group.add_item(
            EnumMetadata {
                module_path: module_path.to_string(),
                name: "ReadError".to_string(),
                orig_name: None,
                rust_path: None,
                shape: EnumShape::Enum,
                remote: false,
                variants: vec![VariantMetadata {
                    name: "StorageInvalidated".to_string(),
                    orig_name: None,
                    discr: None,
                    fields: vec![],
                    docstring: None,
                }],
                discr_type: None,
                non_exhaustive: false,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            FnMetadata {
                module_path: module_path.to_string(),
                name: "observe".to_string(),
                orig_name: None,
                is_async: false,
                inputs: vec![],
                return_type: Some(Type::Stream {
                    item_type: Box::new(Type::UInt32),
                    error_type: Box::new(Type::Enum {
                        module_path: module_path.to_string(),
                        name: "ReadError".to_string(),
                    }),
                    is_send: true,
                }),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        ComponentInterface::from_metadata(group).expect("output stream error fixture")
    }

    #[test]
    fn output_stream_business_error_is_lifted_into_the_next_envelope() {
        let rust = render_napi_rust(&output_stream_fixture()).unwrap();
        assert!(
            rust.contains("pub error: Option<ReadError>"),
            "generated output stream next envelope lost its typed error:\n{rust}"
        );
        assert!(
            rust.contains("error: Some(err.into())"),
            "generated output stream next path did not lift its typed error:\n{rust}"
        );
    }

    #[test]
    fn canonical_encoding_distinguishes_ambiguous_type_spellings() {
        let sequence = describe_input_stream_type(&input_stream(
            Type::Sequence {
                inner_type: Box::new(Type::UInt8),
            },
            Type::String,
        ))
        .unwrap();
        let sequence_named_record = describe_input_stream_type(&input_stream(
            Type::Record {
                module_path: "component_a::types".to_string(),
                name: "SequenceUInt8".to_string(),
            },
            Type::String,
        ))
        .unwrap();
        assert_ne!(sequence.canonical(), sequence_named_record.canonical());
        assert_ne!(sequence.suffix(), sequence_named_record.suffix());

        let foo_bar = describe_input_stream_type(&input_stream(
            Type::Record {
                module_path: "component_a::types".to_string(),
                name: "FooBar".to_string(),
            },
            Type::String,
        ))
        .unwrap();
        let foo_underscore_bar = describe_input_stream_type(&input_stream(
            Type::Record {
                module_path: "component_a::types".to_string(),
                name: "Foo_Bar".to_string(),
            },
            Type::String,
        ))
        .unwrap();
        assert_ne!(foo_bar.canonical(), foo_underscore_bar.canonical());
        assert_ne!(foo_bar.suffix(), foo_underscore_bar.suffix());
    }

    #[test]
    fn canonical_encoding_is_structural_stable_and_error_sensitive() {
        let nested_item = Type::Map {
            key_type: Box::new(Type::String),
            value_type: Box::new(Type::Optional {
                inner_type: Box::new(Type::Sequence {
                    inner_type: Box::new(Type::Custom {
                        module_path: "component_a::types".to_string(),
                        name: "UserId".to_string(),
                        builtin: Box::new(Type::UInt64),
                    }),
                }),
            }),
        };
        let first = describe_input_stream_type(&input_stream(
            nested_item.clone(),
            Type::Enum {
                module_path: "component_a::errors".to_string(),
                name: "ReadError".to_string(),
            },
        ))
        .unwrap();
        let repeated = describe_input_stream_type(&input_stream(
            nested_item.clone(),
            Type::Enum {
                module_path: "component_a::errors".to_string(),
                name: "ReadError".to_string(),
            },
        ))
        .unwrap();
        let other_error = describe_input_stream_type(&input_stream(
            nested_item,
            Type::Enum {
                module_path: "component_a::errors".to_string(),
                name: "WriteError".to_string(),
            },
        ))
        .unwrap();
        assert_eq!(first, repeated);
        assert_eq!(first.fingerprint().len(), 16);
        assert_ne!(first.canonical(), other_error.canonical());
        assert_ne!(first.suffix(), other_error.suffix());
        assert!(first
            .suffix()
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_'));

        let mut local_outer = first.input_type().clone();
        let Type::InputStream { is_send, .. } = &mut local_outer else {
            unreachable!()
        };
        *is_send = false;
        let local_outer = describe_input_stream_type(&local_outer).unwrap();
        assert_eq!(first.canonical(), local_outer.canonical());
        assert_eq!(first.suffix(), local_outer.suffix());
    }

    #[test]
    fn canonical_encoding_uses_logical_module_identity_across_components() {
        let first = describe_input_stream_type(&input_stream(
            Type::Record {
                module_path: "component_a::types".to_string(),
                name: "Payload".to_string(),
            },
            Type::String,
        ))
        .unwrap();
        let second = describe_input_stream_type(&input_stream(
            Type::Record {
                module_path: "component_b::types".to_string(),
                name: "Payload".to_string(),
            },
            Type::String,
        ))
        .unwrap();
        assert_ne!(first.canonical(), second.canonical());
        assert_ne!(first.suffix(), second.suffix());
        assert!(!first.canonical().contains("/Users/"));
    }

    #[test]
    fn collector_covers_all_generated_callable_kinds_and_deduplicates() {
        let ci = callable_fixture();
        let descriptors = collect_input_stream_descriptors(&ci).unwrap();
        assert_eq!(descriptors.len(), 8);

        let item_types = descriptors
            .iter()
            .map(InputStreamDescriptor::item_type)
            .cloned()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            item_types,
            [
                Type::UInt8,
                Type::UInt16,
                Type::UInt32,
                Type::Int32,
                Type::UInt64,
                Type::Int64,
                Type::Float32,
                Type::Float64,
            ]
            .into_iter()
            .collect()
        );

        let rust = render_napi_rust(&ci).unwrap();
        for descriptor in &descriptors {
            assert!(rust.contains(&format!(
                "struct __UniffiInputStream{}Next",
                descriptor.suffix()
            )));
        }

        let contract = super::super::render_ohos_facade_contract(&ci).unwrap();
        let contract: Value = serde_json::from_str(&contract).unwrap();
        let contract_suffixes = contract["inputStreams"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["suffix"].as_str().unwrap().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        let descriptor_suffixes = descriptors
            .iter()
            .map(|descriptor| descriptor.suffix().to_string())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(contract_suffixes, descriptor_suffixes);
    }

    #[test]
    fn collector_rejects_suffix_collisions_instead_of_overwriting() {
        let ci = callable_fixture();
        let error = collect_input_stream_descriptors_with_builders(
            &ci,
            stable_fingerprint,
            |_canonical, _fingerprint, _item_type, _error_type| "ForcedCollision".to_string(),
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("suffix collision"), "{message}");
        assert!(message.contains("ForcedCollision"), "{message}");
    }

    #[test]
    fn collector_rejects_fingerprint_collisions_even_when_readable_names_differ() {
        let ci = callable_fixture();
        let error = collect_input_stream_descriptors_with_builders(
            &ci,
            |_canonical| "forcedfingerprint".to_string(),
            build_input_stream_suffix,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("fingerprint collision"), "{message}");
        assert!(message.contains("forcedfingerprint"), "{message}");
    }

    #[test]
    fn custom_u64_conversion_uses_core_builtin_before_bigint_bridge() {
        let ci = callable_fixture();
        let generator = Generator::new(&ci, CallbackAsyncReturn::Promise);
        let ty = Type::Custom {
            module_path: "descriptor_fixture::types".into(),
            name: "EventId".into(),
            builtin: Box::new(Type::UInt64),
        };
        let lowered = generator
            .lower_value_expr(quote!(value), &ty)
            .unwrap()
            .to_string();
        let lifted = generator
            .lift_value_expr(quote!(value), &ty)
            .unwrap()
            .to_string();
        assert!(lowered.contains("< u64 as :: uniffi :: Lower"), "{lowered}");
        assert!(
            !lowered.contains("BigInt as :: uniffi :: Lower"),
            "{lowered}"
        );
        assert!(lifted.contains("< u64 as :: uniffi :: Lift"), "{lifted}");
        assert!(!lifted.contains("BigInt as :: uniffi :: Lift"), "{lifted}");
        assert!(lifted.contains("BigInt :: from"), "{lifted}");
    }
}
