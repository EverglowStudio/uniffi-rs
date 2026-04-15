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
    Object, ObjectImpl, Record, Type, Variant,
};

use crate::callback_metadata;

pub fn render_napi_rust(ci: &ComponentInterface) -> Result<String> {
    let generator = Generator::new(ci);
    generator.validate()?;
    let tokens = generator.render()?;
    let file = parse2::<syn::File>(tokens)?;
    Ok(prettyplease::unparse(&file))
}

pub fn render_ohos_rust(ci: &ComponentInterface) -> Result<String> {
    let rust = render_napi_rust(ci)?;
    Ok(rust
        .replace("uniffi-bindgen-napi", "uniffi-bindgen-ohos")
        .replace("use napi_derive::napi;", "use napi_derive_ohos::napi;")
        .replace("napi::", "napi_ohos::"))
}

pub(crate) struct Generator<'a> {
    ci: &'a ComponentInterface,
}

impl<'a> Generator<'a> {
    fn new(ci: &'a ComponentInterface) -> Self {
        Self { ci }
    }

    fn has_stream_functions(&self) -> bool {
        self.ci
            .function_definitions()
            .iter()
            .any(|function| matches!(function.return_type(), Some(Type::Stream { .. })))
    }

    fn input_stream_types(&self) -> Vec<Type> {
        let mut out = std::collections::BTreeMap::new();
        for function in self.ci.function_definitions() {
            for arg in function.arguments() {
                self.collect_input_stream_type(&arg.as_type(), &mut out);
            }
        }
        for object in self.ci.object_definitions() {
            for constructor in object.constructors() {
                for arg in constructor.arguments() {
                    self.collect_input_stream_type(&arg.as_type(), &mut out);
                }
            }
            for method in object.methods() {
                for arg in method.arguments() {
                    self.collect_input_stream_type(&arg.as_type(), &mut out);
                }
            }
        }
        for record in self.ci.record_definitions() {
            for constructor in record.constructors() {
                for arg in constructor.arguments() {
                    self.collect_input_stream_type(&arg.as_type(), &mut out);
                }
            }
            for method in record.methods() {
                for arg in method.arguments() {
                    self.collect_input_stream_type(&arg.as_type(), &mut out);
                }
            }
        }
        for enum_ in self.ci.enum_definitions() {
            for constructor in enum_.constructors() {
                for arg in constructor.arguments() {
                    self.collect_input_stream_type(&arg.as_type(), &mut out);
                }
            }
            for method in enum_.methods() {
                for arg in method.arguments() {
                    self.collect_input_stream_type(&arg.as_type(), &mut out);
                }
            }
        }
        out.into_values().collect()
    }

    fn collect_input_stream_type(
        &self,
        ty: &Type,
        out: &mut std::collections::BTreeMap<String, Type>,
    ) {
        match ty {
            Type::InputStream { .. } => {
                out.insert(self.input_stream_suffix(ty), ty.clone());
            }
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
                self.collect_input_stream_type(inner_type, out)
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                self.collect_input_stream_type(key_type, out);
                self.collect_input_stream_type(value_type, out);
            }
            Type::Custom { builtin, .. } => self.collect_input_stream_type(builtin, out),
            _ => {}
        }
    }

    fn render(&self) -> Result<TokenStream> {
        let input_stream_types = self.input_stream_types();
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
        let input_stream_helpers = self.render_input_stream_helpers(&input_stream_types)?;
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

    fn render_input_stream_helpers(&self, input_stream_types: &[Type]) -> Result<TokenStream> {
        if input_stream_types.is_empty() {
            return Ok(quote!());
        }
        let typed_helpers = input_stream_types
            .iter()
            .map(|ty| self.render_typed_input_stream_helper(ty))
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

    fn render_typed_input_stream_helper(&self, ty: &Type) -> Result<TokenStream> {
        let Type::InputStream {
            item_type,
            error_type,
            ..
        } = ty
        else {
            bail!("render_typed_input_stream_helper called with non-input-stream type")
        };
        let next_ident = self.input_stream_next_result_ident(ty);
        let ops_ident = self.input_stream_ops_ident(ty);
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
                ObjectImpl::Struct | ObjectImpl::Trait => {
                    for constructor in object.constructors() {
                        self.validate_callable(constructor, "constructor")?;
                    }
                    for method in object.methods() {
                        self.validate_callable(method, "method")?;
                    }
                }
                ObjectImpl::CallbackTrait => {
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
                ObjectImpl::Struct | ObjectImpl::Trait => {
                    if matches!(usage, TypeUsage::Value | TypeUsage::CallbackArg) {
                        bail!("{label} type `{name}` is not supported in nested/value contexts");
                    }
                    Ok(())
                }
                ObjectImpl::CallbackTrait => {
                    ensure!(
                        matches!(usage, TypeUsage::Arg | TypeUsage::CallbackReturn),
                        "{label} type `{name}` is only supported as a direct function/method argument or callback return"
                    );
                    Ok(())
                }
            },
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
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
                let field_ident = rust_ident(field.name());
                let expr = self.lower_value_expr(quote!(value.#field_ident), &field.as_type())?;
                Ok(quote!(#field_ident: #expr))
            })
            .collect::<Result<Vec<_>>>()?;
        let from_core_fields = record
            .fields()
            .iter()
            .map(|field| {
                let field_ident = rust_ident(field.name());
                let expr = self.lift_value_expr(quote!(value.#field_ident), &field.as_type())?;
                Ok(quote!(#field_ident: #expr))
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
        let variant_ident = rust_ident(variant.name());
        if variant.fields().is_empty() {
            return Ok(quote!(#bridge_enum::#variant_ident => #core_enum::#variant_ident));
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
                let field_ident = rust_ident(field.name());
                let expr = self.lower_value_expr(quote!(#field_ident), &field.as_type())?;
                Ok(quote!(#field_ident: #expr))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(quote!(
            #bridge_enum::#variant_ident { #(#bindings),* } => #core_enum::#variant_ident {
                #(#lowers),*
            }
        ))
    }

    fn render_from_core_variant(&self, enum_: &Enum, variant: &Variant) -> Result<TokenStream> {
        let bridge_enum = rust_ident(enum_.name());
        let core_enum = self.core_type_path(enum_.as_type());
        let variant_ident = rust_ident(variant.name());
        if variant.fields().is_empty() {
            return Ok(quote!(#core_enum::#variant_ident => #bridge_enum::#variant_ident));
        }
        let bindings = variant
            .fields()
            .iter()
            .map(|field| rust_ident(field.name()))
            .collect::<Vec<_>>();
        let lifts = variant
            .fields()
            .iter()
            .map(|field| {
                let field_ident = rust_ident(field.name());
                let expr = self.lift_value_expr(quote!(#field_ident), &field.as_type())?;
                Ok(quote!(#field_ident: #expr))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(quote!(
            #core_enum::#variant_ident { #(#bindings),* } => #bridge_enum::#variant_ident {
                #(#lifts),*
            }
        ))
    }

    fn render_object(&self, object: &Object) -> Result<TokenStream> {
        match object.imp() {
            ObjectImpl::Struct | ObjectImpl::Trait => self.render_object_class(object),
            ObjectImpl::CallbackTrait => self.render_callback_trait(object),
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
                Ok(quote! {
                    ThreadsafeFunction<#tsfn_args, napi::bindgen_prelude::Promise<#result_ty>>
                })
            } else if let Some(return_type) = method.return_type() {
                let bridge_return_ty = self.callback_async_bridge_type(return_type)?;
                Ok(quote! {
                    ThreadsafeFunction<#tsfn_args, napi::bindgen_prelude::Promise<#bridge_return_ty>>
                })
            } else {
                Ok(quote! {
                    ThreadsafeFunction<
                        #tsfn_args,
                        napi::bindgen_prelude::Promise<()>,
                    >
                })
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
            .filter(|object| matches!(object.imp(), ObjectImpl::CallbackTrait))
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
                Ok(quote! {
                    ThreadsafeFunction<#tsfn_args, napi::bindgen_prelude::Promise<#result_ty>>
                })
            } else if let Some(return_type) = method.return_type() {
                let bridge_return_ty = self.callback_async_bridge_type(return_type)?;
                Ok(quote! {
                    ThreadsafeFunction<#tsfn_args, napi::bindgen_prelude::Promise<#bridge_return_ty>>
                })
            } else {
                Ok(quote! {
                    ThreadsafeFunction<#tsfn_args, napi::bindgen_prelude::Promise<()>>
                })
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
            let value_ty = if method.is_async() {
                self.callback_async_bridge_type(return_type)?
            } else {
                self.callback_bridge_type(return_type)?
            };
            quote!(pub value: Option<#value_ty>,)
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
                let result_ident = self.callback_result_ident(object, method);
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
                    let lowered = self
                        .lower_async_callback_value_expr(quote!(__callback_value), return_type)?;
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
                } else {
                    quote!(Ok(()))
                };
                let lowered_error = self.lower_callback_value_expr(
                    quote!(__callback_error),
                    method.throws_type().unwrap(),
                )?;
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
                            let __callback_promise = __registry.call_async(Ok(#registry_call_value)).await.unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} failed to dispatch returned async JS callback: {}",
                                    #object_name,
                                    #method_name,
                                    err
                                );
                            });
                            let __callback_result: #result_ident = __callback_promise.await.unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} returned async JS callback rejected: {}",
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
                        let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no JS callback",
                                #object_name,
                                #method_name
                            );
                        });
                        let __callback_promise = __callback.call_async(Ok(#call_value)).await.unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} failed to call async JS callback: {}",
                                #object_name,
                                #method_name,
                                err
                            );
                        });
                        let __callback_result: #result_ident = __callback_promise.await.unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} async JS callback rejected: {}",
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
            return match method.return_type() {
                Some(return_type) => {
                    let return_ty = self.core_callback_return_type(return_type)?;
                    let lowered = self.lower_async_callback_value_expr(
                        quote!(#return_value_ident),
                        return_type,
                    )?;
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
                                let __callback_promise = __registry.call_async(Ok(#registry_call_value)).await.unwrap_or_else(|err| {
                                    panic!(
                                        "callback trait `{}`.{} failed to dispatch returned async JS callback: {}",
                                        #object_name,
                                        #method_name,
                                        err
                                    );
                                });
                                let #return_value_ident = __callback_promise.await.unwrap_or_else(|err| {
                                    panic!(
                                        "callback trait `{}`.{} returned async JS callback rejected: {}",
                                        #object_name,
                                        #method_name,
                                        err
                                    );
                                });
                                #lowered
                            } else {
                            let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} has no JS callback",
                                    #object_name,
                                    #method_name
                                );
                            });
                            let __callback_promise = __callback.call_async(Ok(#call_value)).await.unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} failed to call async JS callback: {}",
                                    #object_name,
                                    #method_name,
                                    err
                                );
                            });
                            let #return_value_ident = __callback_promise.await.unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} async JS callback rejected: {}",
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
                    async fn #method_ident(&self, #(#args),*) {
                        if let Some(__id) = self.__uniffi_callback_registry_id {
                            let __registry = self.#registry_field_ident.as_ref().unwrap_or_else(|| {
                                panic!(
                                    "callback trait `{}`.{} has no returned-callback dispatcher",
                                    #object_name,
                                    #method_name
                                );
                            });
                            let __callback_promise = __registry.call_async(Ok(#registry_call_value)).await.unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} failed to dispatch returned async JS callback: {}",
                                    #object_name,
                                    #method_name,
                                    err
                                );
                            });
                            __callback_promise.await.unwrap_or_else(|err| {
                                panic!(
                                    "callback trait `{}`.{} returned async JS callback rejected: {}",
                                    #object_name,
                                    #method_name,
                                    err
                                );
                            });
                        } else {
                        let __callback = self.#method_ident.as_ref().unwrap_or_else(|| {
                            panic!(
                                "callback trait `{}`.{} has no JS callback",
                                #object_name,
                                #method_name
                            );
                        });
                        let __callback_promise = __callback.call_async(Ok(#call_value)).await.unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} failed to call async JS callback: {}",
                                #object_name,
                                #method_name,
                                err
                            );
                        });
                        __callback_promise.await.unwrap_or_else(|err| {
                            panic!(
                                "callback trait `{}`.{} async JS callback rejected: {}",
                                #object_name,
                                #method_name,
                                err
                            );
                        });
                        }
                    }
                }),
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
        let receiver = if method.takes_self_by_arc() {
            quote!((*(#receiver_ident)).0.clone())
        } else {
            quote!((*(#receiver_ident)).0.as_ref())
        };
        let call = quote!(#receiver.#method_ident(#(#lowered),*));
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
                pub async fn #fn_ident(#receiver_ident: ClassInstance<#object_ident>, #(#args),*) -> Result<#output_ty> {
                    #body
                }
            })
        } else {
            Ok(quote! {
                #[napi]
                pub fn #fn_ident(#receiver_ident: ClassInstance<#object_ident>, #(#args),*) -> Result<#output_ty> {
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
        let lifted_value = self.lift_value_expr(quote!(value), item_type)?;

        Ok(quote! {
            static #registry_ident: ::uniffi::RustStreamRegistry<#item_core_ty, #error_core_ty> =
                ::uniffi::deps::once_cell::sync::Lazy::new(|| ::std::sync::Mutex::new(::std::collections::HashMap::new()));

            #[napi(object)]
            pub struct #next_struct_ident {
                pub done: bool,
                pub value: Option<#item_bridge_ty>,
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
                    }),
                    Ok(Ok(None)) => Ok(#next_struct_ident {
                        done: true,
                        value: None,
                    }),
                    Ok(Err(err)) => Err(into_napi_error(err)),
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
                let next_ident = self.input_stream_next_result_ident(ty);
                Ok(quote!(__UniffiInputStream<#next_ident>))
            }
            Type::Object { name, imp, .. } => {
                let ident = rust_ident(name);
                match imp {
                    ObjectImpl::Struct | ObjectImpl::Trait => Ok(quote!(ClassInstance<#ident>)),
                    ObjectImpl::CallbackTrait => Ok(quote!(#ident)),
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
            Type::Float32 => Ok(quote!(f32)),
            Type::Float64 => Ok(quote!(f64)),
            Type::Boolean => Ok(quote!(bool)),
            Type::String => Ok(quote!(String)),
            Type::Bytes => Ok(quote!(Buffer)),
            Type::Record { name, .. } | Type::Enum { name, .. } => {
                let ident = rust_ident(name);
                Ok(quote!(#ident))
            }
            Type::Object { name, imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait => {
                    let ident = rust_ident(name);
                    Ok(quote!(#ident))
                }
                ObjectImpl::CallbackTrait => {
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
            Type::Int64 => {
                let lowered = self.lower_value_expr(quote!(#ident), &Type::Int64)?;
                Ok(lowered)
            }
            Type::Object { imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait => Ok(quote!((*(#ident)).0.clone())),
                ObjectImpl::CallbackTrait => {
                    let trait_path = self.core_type_path(ty.clone());
                    Ok(quote!(std::sync::Arc::new(#ident) as std::sync::Arc<dyn #trait_path>))
                }
            },
            Type::InputStream { .. } => {
                let ops_ident = self.input_stream_ops_ident(ty);
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
            | Type::Float32
            | Type::Float64
            | Type::Boolean
            | Type::String => Ok(expr),
            // BigInt → u64: reject negative and out-of-range values.
            Type::UInt64 => Ok(quote!({
                let __big = #expr;
                let (__sign, __val, __lossless) = __big.get_u64();
                if __sign && __val != 0 {
                    return Err(napi::Error::new(napi::Status::InvalidArg, "negative value cannot be converted to u64"));
                }
                if !__lossless {
                    return Err(napi::Error::new(napi::Status::InvalidArg, "BigInt value does not fit into u64"));
                }
                __val
            })),
            // BigInt → i64: reject values outside the i64 range.
            Type::Int64 => Ok(quote!({
                let __big = #expr;
                let (__val, __lossless) = __big.get_i64();
                if !__lossless {
                    return Err(napi::Error::new(napi::Status::InvalidArg, "BigInt value does not fit into i64"));
                }
                __val
            })),
            Type::Bytes => Ok(quote!(#expr.into())),
            Type::Record { .. } | Type::Enum { .. } => Ok(quote!(#expr.into())),
            Type::Object { imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait => Ok(quote!(#expr.0.clone())),
                ObjectImpl::CallbackTrait => bail!("callback traits are not supported here"),
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
                let builtin_ty = self.bridge_value_type(builtin)?;
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
            | Type::Float32
            | Type::Float64
            | Type::Boolean
            | Type::String => Ok(expr),
            // u64/i64 → BigInt for JS `bigint`.
            Type::UInt64 | Type::Int64 => Ok(quote!(napi::bindgen_prelude::BigInt::from(#expr))),
            Type::Bytes => Ok(quote!(#expr.into())),
            Type::Record { .. } | Type::Enum { .. } => Ok(quote!(#expr.into())),
            Type::Object { name, imp, .. } => match imp {
                ObjectImpl::Struct | ObjectImpl::Trait => {
                    let ident = rust_ident(name);
                    Ok(quote!(#ident(#expr)))
                }
                ObjectImpl::CallbackTrait => bail!(
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
                let builtin_ty = self.bridge_value_type(builtin)?;
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
                ObjectImpl::Struct | ObjectImpl::Trait => {
                    let ident = rust_ident(name);
                    Ok(quote!(napi::bindgen_prelude::ClassInstance<'static, #ident>))
                }
                ObjectImpl::CallbackTrait => {
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
            | Type::Float32
            | Type::Float64
            | Type::Boolean
            | Type::String => Ok(expr),
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
                ObjectImpl::Struct | ObjectImpl::Trait => Ok(quote!((*(#expr)).0.clone())),
                ObjectImpl::CallbackTrait => {
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
                let builtin_ty = self.bridge_value_type(builtin)?;
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
                imp: ObjectImpl::CallbackTrait,
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
                ObjectImpl::Trait | ObjectImpl::CallbackTrait => {
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
            ObjectImpl::Trait | ObjectImpl::CallbackTrait => {
                quote!(std::sync::Arc<dyn #core_path>)
            }
        }
    }

    fn core_item_path(&self, module_path: &str, name: &str) -> TokenStream {
        let module = rust_path(module_path);
        let ident = rust_ident(name);
        quote!(#module::#ident)
    }

    fn core_type_path(&self, ty: Type) -> TokenStream {
        match ty {
            Type::Record { module_path, name }
            | Type::Enum { module_path, name }
            | Type::CallbackInterface { module_path, name }
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

    fn input_stream_next_result_ident(&self, ty: &Type) -> syn::Ident {
        format_ident!("__UniffiInputStream{}Next", self.input_stream_suffix(ty))
    }

    fn input_stream_ops_ident(&self, ty: &Type) -> syn::Ident {
        format_ident!("__UniffiInputStream{}Ops", self.input_stream_suffix(ty))
    }

    fn input_stream_suffix(&self, ty: &Type) -> String {
        match ty {
            Type::InputStream {
                item_type,
                error_type,
                ..
            } => format!(
                "{}{}",
                self.type_suffix(item_type),
                self.type_suffix(error_type)
            ),
            _ => self.type_suffix(ty),
        }
    }

    fn type_suffix(&self, ty: &Type) -> String {
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
            Type::Record { name, .. }
            | Type::Enum { name, .. }
            | Type::Object { name, .. }
            | Type::CallbackInterface { name, .. }
            | Type::Custom { name, .. } => sanitize_ident(name).to_upper_camel_case(),
            Type::Optional { inner_type } => format!("Optional{}", self.type_suffix(inner_type)),
            Type::Sequence { inner_type } => format!("Sequence{}", self.type_suffix(inner_type)),
            Type::Map {
                key_type,
                value_type,
            } => format!(
                "Map{}{}",
                self.type_suffix(key_type),
                self.type_suffix(value_type)
            ),
            Type::Stream {
                item_type,
                error_type,
                ..
            } => format!(
                "Stream{}{}",
                self.type_suffix(item_type),
                self.type_suffix(error_type)
            ),
            Type::InputStream { .. } => format!("InputStream{}", self.input_stream_suffix(ty)),
        }
    }
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
