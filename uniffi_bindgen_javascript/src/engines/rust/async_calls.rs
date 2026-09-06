//! Private per-call cancellation transport shared by the generated engines.
//!
//! The three control operations follow the public operation table, preserving
//! the engines' dense ID contract. They never appear as business methods.

use super::*;

pub(super) const CONTROL_NAMES: [&str; 3] = [
    "__uniffi_create_call",
    "__uniffi_cancel_call",
    "__uniffi_release_call",
];

pub(super) fn managed(operation: &RustOperationPlan) -> bool {
    operation.async_kind == AsyncKind::Async
        && matches!(
            operation.call_target,
            RustCallTarget::FreeFunction { .. }
                | RustCallTarget::Constructor { .. }
                | RustCallTarget::Method { .. }
        )
}

/// Local helpers own the lifting carriers and return a framework Result outside
/// the complete business return (including its declared Result, if any).
pub(super) fn local_method(operation: &RustOperationPlan) -> bool {
    matches!(
        operation.call_target,
        RustCallTarget::Method {
            object_kind: uniffi_js_engine_schema::RustObjectKind::LocalStruct,
            ..
        }
    )
}

pub(super) fn local_error_helper(flavor: Option<NativeFlavor>) -> TokenStream {
    let name = quote! {
        let name = if matches!(error, ::uniffi::async_runtime::LocalError::Cancelled) {
            "UniffiCancelled".to_owned()
        } else {
            format!("LocalError{error:?}")
        };
    };
    if let Some(flavor) = flavor {
        let engine = engine_crate_path(flavor);
        quote! {
            fn __uniffi_local_error(error: ::uniffi::async_runtime::LocalError) -> #engine::BridgeErrorDescriptor {
                #name
                #engine::BridgeErrorDescriptor {
                    domain: #engine::ErrorDomain::Backend,
                    error_name: name,
                    variant: None,
                    data: #engine::ErrorData::Null,
                    message: error.to_string(),
                    native_stack: None,
                }
            }
        }
    } else {
        quote! {
            fn __uniffi_local_error(error: ::uniffi::async_runtime::LocalError) -> JsValue {
                #name
                let value = js_sys::Error::new(&error.to_string());
                value.set_name(&name);
                js_sys::Reflect::set(&value, &JsValue::from_str("errorName"), &JsValue::from_str(&name))
                    .expect("fresh local Error is extensible");
                value.into()
            }
        }
    }
}

fn result_ok_type(ty: &Type) -> Result<Type> {
    let syn::Type::Path(path) = ty else {
        bail!("fallible native wrapper is not a Result")
    };
    let syn::PathArguments::AngleBracketed(args) = &path.path.segments.last().unwrap().arguments
    else {
        bail!("fallible native wrapper has no Result arguments")
    };
    let Some(syn::GenericArgument::Type(ok)) = args.args.first() else {
        bail!("fallible native wrapper has no result type")
    };
    Ok(ok.clone())
}

pub(super) fn registry(managed_calls: bool) -> TokenStream {
    let start_call = managed_calls.then(|| {
        quote! {
            struct __UniffiCallLease {
                id: u64,
                context: ::uniffi::async_runtime::CallContext,
            }
            impl Drop for __UniffiCallLease {
                fn drop(&mut self) {
                    let context = __uniffi_with_calls(|calls| calls.calls.remove(&self.id));
                    drop(context);
                }
            }
            fn __uniffi_start_call(id: f64) -> __UniffiCallLease {
                let id = __uniffi_call_key(id);
                let context = ::uniffi::async_runtime::CallContext::new(Default::default());
                let cancelled = __uniffi_with_calls(|calls| {
                    use ::std::collections::hash_map::Entry;
                    match calls.calls.entry(id) {
                        Entry::Vacant(entry) => { entry.insert(Some(context.clone())); false }
                        Entry::Occupied(mut entry) => {
                            assert!(entry.get().is_none(), "async call ID is already active");
                            entry.insert(Some(context.clone()));
                            true
                        }
                    }
                });
                if cancelled { context.cancel(); }
                __UniffiCallLease { id, context }
            }
        }
    });
    quote! {
        #start_call
        #[derive(Default)]
        struct __UniffiCalls {
            next: u64,
            // None is a cancellation received before the first business poll.
            // It lives only until that call starts or JS observes its settlement.
            calls: ::std::collections::HashMap<u64, Option<::uniffi::async_runtime::CallContext>>,
        }
        #[cfg(not(target_arch = "wasm32"))]
        fn __uniffi_with_calls<T>(f: impl FnOnce(&mut __UniffiCalls) -> T) -> T {
            static CALLS: ::std::sync::OnceLock<::std::sync::Mutex<__UniffiCalls>> = ::std::sync::OnceLock::new();
            let mut calls = CALLS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
            f(&mut calls)
        }
        #[cfg(target_arch = "wasm32")]
        fn __uniffi_with_calls<T>(f: impl FnOnce(&mut __UniffiCalls) -> T) -> T {
            ::std::thread_local! {
                static CALLS: ::std::cell::RefCell<__UniffiCalls> = Default::default();
            }
            CALLS.with(|calls| f(&mut calls.borrow_mut()))
        }
        fn __uniffi_call_key(id: f64) -> u64 {
            assert!(id.is_finite() && id.fract() == 0.0 && (1.0..=9007199254740991.0).contains(&id), "invalid async call ID");
            id as u64
        }
        // Reserve IDs, without allocating call contexts. Blocks also isolate
        // independent JS sessions/Workers sharing one native addon registry.
        fn __uniffi_create_call() -> f64 {
            __uniffi_with_calls(|calls| {
                const BLOCK: u64 = 4096;
                let end = calls.next.checked_add(BLOCK).expect("async call ID exhausted");
                assert!(end <= 9007199254740991, "async call ID exhausted");
                let first = calls.next + 1;
                calls.next = end;
                first as f64
            })
        }
        fn __uniffi_cancel_call(id: f64) {
            let id = __uniffi_call_key(id);
            // Wake after unlocking: foreign callbacks can reenter the bridge.
            let context = __uniffi_with_calls(|calls| calls.calls.entry(id).or_insert(None).clone());
            if let Some(context) = context { context.cancel(); }
        }
        fn __uniffi_release_call(id: f64) {
            let id = __uniffi_call_key(id);
            // Only cancelled calls need a final acknowledgement from JS: a
            // late abort may arrive after the Rust lease already removed itself.
            // A running call always owns its cleanup and cannot be removed here.
            __uniffi_with_calls(|calls| {
                if matches!(calls.calls.get(&id), Some(None)) { calls.calls.remove(&id); }
            });
        }
    }
}

pub(super) fn native_helpers(flavor: NativeFlavor, managed_calls: bool) -> TokenStream {
    let engine = engine_crate_path(flavor);
    let registry = registry(managed_calls);
    if !managed_calls {
        return registry;
    }
    quote! {
        #registry
        fn __uniffi_call_error(error: #engine::BridgeErrorDescriptor) -> #engine::BridgeErrorDescriptor { error }
        fn __uniffi_cancelled_error(_: ::uniffi::async_runtime::Cancelled) -> #engine::BridgeErrorDescriptor {
            #engine::BridgeErrorDescriptor {
                domain: #engine::ErrorDomain::Backend,
                error_name: "UniffiCancelled".to_owned(),
                variant: None,
                data: #engine::ErrorData::Null,
                message: "call cancelled".to_owned(),
                native_stack: None,
            }
        }
    }
}

/// Wrap the typed function before engine macro expansion, so the original
/// return type and all conversion/error code remain type checked by Rust.
pub(super) fn wrap(
    source: &str,
    operations: &[RustOperationPlan],
    flavor: Option<NativeFlavor>,
) -> Result<String> {
    let mut file = syn::parse_file(source).context("parsing typed async wrappers")?;
    let prefix = if flavor.is_some() {
        "__uniffi_native_operation_"
    } else {
        "__uniffi_typed_wasm_operation_"
    };
    for item in &mut file.items {
        let syn::Item::Fn(function) = item else {
            continue;
        };
        let name = function.sig.ident.to_string();
        let Some(id) = name
            .strip_prefix(prefix)
            .and_then(|id| id.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(operation) = operations
            .iter()
            .find(|op| op.operation_id.index() == id && managed(op))
        else {
            continue;
        };
        let statements = &function.block.stmts;
        let old_output: Type = match &function.sig.output {
            syn::ReturnType::Default => syn::parse_quote!(()),
            syn::ReturnType::Type(_, ty) => *ty.clone(),
        };
        // Every lowered argument belongs to the call even if the business
        // implementation does not use it before suspending.
        let captures = function
            .sig
            .inputs
            .iter()
            .filter_map(|arg| match arg {
                syn::FnArg::Typed(arg) => Some(&arg.pat),
                _ => None,
            })
            .map(|arg| quote!(let _ = &#arg;))
            .collect::<Vec<_>>();
        let future = quote!(async move { #(#captures)* #(#statements)* });
        let (output, execute) = if let Some(flavor) = flavor {
            let engine = engine_crate_path(flavor);
            // Run commits the complete raw result before either framework or
            // declared error conversion. Cancellation can still drop all Rust
            // resources without leaving an undisclosed handle in the registry.
            let (old_output, committed) = if local_method(operation) {
                (
                    result_ok_type(&old_output)?,
                    quote!(__uniffi_context.run(#future).await.map_err(__uniffi_cancelled_error)?.map_err(__uniffi_local_error)),
                )
            } else {
                (
                    old_output,
                    quote!(__uniffi_context.run(#future).await.map_err(__uniffi_cancelled_error)),
                )
            };
            if operation.throws.is_some() {
                let ok = result_ok_type(&old_output)?;
                let map = path_for_helper(id, "error", "error");
                (
                    quote!(::std::result::Result<#ok, #engine::BridgeErrorDescriptor>),
                    quote! {
                        #committed?.map_err(#map)
                    },
                )
            } else {
                (
                    quote!(::std::result::Result<#old_output, #engine::BridgeErrorDescriptor>),
                    quote! { #committed },
                )
            }
        } else {
            let has_host = function.sig.inputs.iter().any(|arg| match arg {
                syn::FnArg::Typed(arg) => {
                    matches!(&*arg.pat, syn::Pat::Ident(name) if name.ident == "host")
                }
                _ => false,
            });
            let future = if has_host {
                quote!(__uniffi_with_call_host(#future, __uniffi_host))
            } else {
                future
            };
            let host = has_host.then(|| quote!(let __uniffi_host = host.clone();));
            // The Wasm renderer runs only argument lowering and the business
            // future in CallContext, while the result still owns Rust resources.
            // Keep the host around that run AND its subsequent result/error
            // lifting, including early conversion errors and cleanup polls.
            (
                quote!(#old_output),
                quote! {
                    #host
                    #future.await
                },
            )
        };
        function
            .sig
            .inputs
            .push(syn::parse_quote!(__uniffi_call_id: f64));
        function.sig.output = syn::parse2(quote!(-> #output))?;
        function.block = syn::parse2(quote!({
            let __uniffi_call = __uniffi_start_call(__uniffi_call_id);
            let __uniffi_context = &__uniffi_call.context;
            #execute
        }))?;
    }
    Ok(file.into_token_stream().to_string())
}

pub(super) fn family_controls(start: usize) -> Result<Vec<FamilyOperation>> {
    CONTROL_NAMES
        .iter()
        .enumerate()
        .map(|(index, _)| {
            Ok(FamilyOperation {
                id: u32::try_from(start + index)?,
                kind: FamilyKind::Function,
                async_kind: FamilyAsync::Sync,
                fallible: false,
                argument_count: usize::from(index != 0),
                dispatch: FamilyDispatch::Native,
                receiver: None,
                result_resources: Vec::new(),
                callbacks: Vec::new(),
                streams: Vec::new(),
                stream_slot: None,
            })
        })
        .collect()
}

pub(super) fn napi_controls(start: usize) -> Result<Vec<napi_uniffi_engine::RustOperationPlan>> {
    use napi_uniffi_engine::*;
    CONTROL_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            Ok(napi_uniffi_engine::RustOperationPlan {
                operation_id: u32::try_from(start + index)?,
                target: RustOperationTarget::Native {
                    call: syn::parse_str(&format!("crate::{name}"))?,
                },
                receiver: None,
                arguments: if index == 0 {
                    vec![]
                } else {
                    vec![RustArgumentPlan {
                        name: rust_ident("call_id"),
                        binding: ArgumentBinding::Direct {
                            carrier_type: syn::parse_quote!(f64),
                        },
                    }]
                },
                return_binding: if index == 0 {
                    ReturnBinding::Direct {
                        carrier_type: syn::parse_quote!(f64),
                    }
                } else {
                    ReturnBinding::Unit
                },
                error_binding: ErrorBinding::Infallible,
            })
        })
        .collect()
}

#[cfg(feature = "ohos")]
pub(super) fn ohos_controls(
    start: usize,
) -> Result<Vec<napi_ohos_uniffi_engine::OhosOperationPlan>> {
    use napi_ohos_uniffi_engine::*;
    CONTROL_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            Ok(OhosOperationPlan {
                operation_id: u32::try_from(start + index)?,
                target: OhosOperationTarget::Native {
                    call: syn::parse_str(&format!("crate::{name}"))?,
                },
                receiver: None,
                arguments: if index == 0 {
                    vec![]
                } else {
                    vec![OhosArgumentPlan {
                        name: rust_ident("call_id"),
                        binding: OhosArgumentBinding::Direct {
                            carrier_type: syn::parse_quote!(f64),
                        },
                    }]
                },
                return_binding: if index == 0 {
                    OhosReturnBinding::Direct {
                        carrier_type: syn::parse_quote!(f64),
                    }
                } else {
                    OhosReturnBinding::Unit
                },
                error_binding: OhosErrorBinding::Infallible,
            })
        })
        .collect()
}

pub(super) fn wasm_call_argument() -> wasm_bindgen_uniffi_engine::WasmArgumentBinding {
    use wasm_bindgen_uniffi_engine::*;
    WasmArgumentBinding {
        public_name: "__uniffi_call_id".into(),
        rust_name: "__uniffi_call_id".into(),
        rust_type: WasmRustType::Scalar(WasmScalarType::F64),
        carrier: WasmRustCarrier::Primitive,
        abi_carrier: WasmCarrier::F64,
        ownership: WasmOwnership::Owned,
        conversion: WasmConversionRecipe::Identity,
    }
}

pub(super) fn wasm_controls(
    start: usize,
) -> Result<Vec<wasm_bindgen_uniffi_engine::WasmOperationPlan>> {
    use wasm_bindgen_uniffi_engine::*;
    CONTROL_NAMES
        .iter()
        .enumerate()
        .map(|(index, name)| {
            let path = wasm_bindgen_uniffi_engine::RustPath::new(["crate".into(), (*name).into()])?;
            let arg = wasm_call_argument();
            Ok(WasmOperationPlan {
                operation_id: u32::try_from(start + index)?,
                source_key: WasmOperationSourceKey {
                    component: "__uniffi_async".into(),
                    owner: WasmOperationOwner::Namespace,
                    kind: WasmOperationKind::Function,
                    name: (*name).into(),
                },
                component_id: 0,
                owner: WasmOperationOwner::Namespace,
                kind: WasmOperationKind::Function,
                callback_method_id: None,
                call_target: WasmCallTarget::FreeFunction {
                    module: wasm_bindgen_uniffi_engine::RustPath::new(["crate".into()])?,
                    item: (*name).into(),
                },
                rust_call: path,
                receiver: None,
                arguments: if index == 0 {
                    vec![]
                } else {
                    vec![arg.clone()]
                },
                return_value: (index == 0).then_some(WasmReturnBinding {
                    rust_type: arg.rust_type,
                    carrier: arg.carrier,
                    abi_carrier: arg.abi_carrier,
                    ownership: arg.ownership,
                    conversion: arg.conversion,
                }),
                async_kind: WasmAsyncKind::Sync,
                throws: None,
                callback_use_sites: vec![],
                resource_use_sites: vec![],
                resource_hooks: vec![],
                stream_resources: vec![],
            })
        })
        .collect()
}
