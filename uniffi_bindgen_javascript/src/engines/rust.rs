//! Mechanical Rust bridge-plan projection.
//!
//! The adapter below deliberately contains no UniFFI metadata logic.  It
//! maps the owned `RustBridgePlan` values to the engine crates' own DTOs and
//! asks those crates to print their private trampoline.  Public facade
//! source is rendered by `uniffi_js_facade` separately.

use std::collections::BTreeSet;

use crate::frontend::NormalizedPackage;
use anyhow::{anyhow, bail, Context, Result};
use proc_macro2::{Ident, Span, TokenStream};
use quote::{quote, ToTokens};
use syn::{Path, Type};
use uniffi_js_abi::{AsyncKind, OperationKind, OperationOwner, Ownership, ScalarType, TypeId};
use uniffi_js_engine_schema::{
    BridgePlan, ConversionRecipe, EngineKind, RustArgumentBinding, RustCallTarget, RustCarrier,
    RustNamedTypeKind, RustOperationPlan, RustPath, RustResourceHook, RustType, RustValueBinding,
    RustVariantPayload, ValuePath, ValuePathSegment,
};

#[path = "rust/wasm_source.rs"]
pub(crate) mod wasm_source;

type FamilyOperation = napi_uniffi_engine::napi_family_core::FamilyOperationInput;
type FamilyInput = napi_uniffi_engine::napi_family_core::FamilyPlanInput;
type FamilyPlan = napi_uniffi_engine::napi_family_core::FamilyPlan;
type FamilyFlavor = napi_uniffi_engine::napi_family_core::HostFlavor;
type FamilyDispatch = napi_uniffi_engine::napi_family_core::OperationDispatch;
type FamilyKind = napi_uniffi_engine::napi_family_core::OperationKind;
type FamilyAsync = napi_uniffi_engine::napi_family_core::AsyncKind;
type FamilyReceiver = napi_uniffi_engine::napi_family_core::ReceiverBinding;
type FamilyResource = napi_uniffi_engine::napi_family_core::ResourceBinding;
type FamilyResourceKind = napi_uniffi_engine::napi_family_core::ResourceKind;
type FamilyResourceOwnership = napi_uniffi_engine::napi_family_core::ResourceOwnership;
type FamilyResultResource = napi_uniffi_engine::napi_family_core::ResultResourceUseSite;
type FamilyCallback = napi_uniffi_engine::napi_family_core::CallbackUseSite;
type FamilyStream = napi_uniffi_engine::napi_family_core::StreamUseSite;
type FamilyStreamBinding = napi_uniffi_engine::napi_family_core::StreamValueBinding;
type FamilyStreamSlot = napi_uniffi_engine::napi_family_core::StreamSlotIdentity;
type FamilyPathType = napi_uniffi_engine::napi_family_core::ValuePath;

/// Engine-neutral resource selector used by both N-API and wasm adapters.
/// The canonical schema intentionally does not expose an engine-specific
/// `Optional` segment; this private expanded path keeps null-aware traversal
/// in one walker and is projected mechanically below.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ResourcePathSegment {
    Receiver,
    Argument(u32),
    Return,
    StreamItem,
    StreamError,
    Optional,
    Field(String),
    Variant(String),
    SequenceItem,
    SetItem,
    MapKey,
    MapValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ExpandedResourceUseSite {
    path: Vec<ResourcePathSegment>,
    kind: FamilyResourceKind,
    type_id: Option<u32>,
    ownership: Ownership,
}

fn family_resource_path(path: &[ResourcePathSegment]) -> FamilyPathType {
    use napi_uniffi_engine::napi_family_core::ValuePathSegment as S;
    FamilyPathType::new(
        path.iter()
            .map(|segment| match segment {
                ResourcePathSegment::Receiver => {
                    unreachable!("N-API family result paths cannot start at receiver")
                }
                ResourcePathSegment::Argument(index) => S::Argument(*index),
                ResourcePathSegment::Return => S::Return,
                ResourcePathSegment::StreamItem => S::StreamItem,
                ResourcePathSegment::StreamError => S::StreamError,
                ResourcePathSegment::Optional => S::Optional,
                ResourcePathSegment::Field(name) => S::Field(name.clone()),
                ResourcePathSegment::Variant(name) => S::Variant(name.clone()),
                ResourcePathSegment::SequenceItem => S::SequenceElement,
                ResourcePathSegment::SetItem => S::SetElement,
                ResourcePathSegment::MapKey => S::MapKey,
                ResourcePathSegment::MapValue => S::MapValue,
            })
            .collect::<Vec<_>>(),
    )
}

fn wasm_resource_path(path: &[ResourcePathSegment]) -> wasm_bindgen_uniffi_engine::WasmValuePath {
    use wasm_bindgen_uniffi_engine::WasmValuePathSegment as S;
    wasm_bindgen_uniffi_engine::WasmValuePath::new(
        path.iter()
            .map(|segment| match segment {
                ResourcePathSegment::Receiver => S::Receiver,
                ResourcePathSegment::Argument(index) => S::Argument(*index),
                ResourcePathSegment::Return => S::Return,
                ResourcePathSegment::StreamItem => S::StreamItem,
                ResourcePathSegment::StreamError => S::StreamError,
                ResourcePathSegment::Optional => S::Optional,
                ResourcePathSegment::Field(name) => S::Field(name.clone()),
                ResourcePathSegment::Variant(name) => S::Variant(name.clone()),
                ResourcePathSegment::SequenceItem => S::SequenceItem,
                ResourcePathSegment::SetItem => S::SetItem,
                ResourcePathSegment::MapKey => S::MapKey,
                ResourcePathSegment::MapValue => S::MapValue,
            })
            .collect::<Vec<_>>(),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeFlavor {
    Node,
    #[cfg(feature = "ohos")]
    Ohos,
}

impl NativeFlavor {
    fn prefix(self) -> &'static str {
        match self {
            Self::Node => "napi",
            #[cfg(feature = "ohos")]
            Self::Ohos => "napi_ohos",
        }
    }

    fn derive_import(self) -> &'static str {
        match self {
            Self::Node => "napi_derive::napi",
            #[cfg(feature = "ohos")]
            Self::Ohos => "napi_derive_ohos::napi",
        }
    }

    fn error_descriptor(self) -> Path {
        syn::parse_str(match self {
            Self::Node => "napi_uniffi_engine::BridgeErrorDescriptor",
            #[cfg(feature = "ohos")]
            Self::Ohos => "napi_ohos_uniffi_engine::BridgeErrorDescriptor",
        })
        .expect("engine error descriptor path is valid")
    }
}

fn rust_path(path: &RustPath) -> Result<Path> {
    let value = path.segments.join("::");
    syn::parse_str(&value).with_context(|| format!("invalid Rust path {value:?}"))
}

fn rust_ident(value: &str) -> Ident {
    if let Some(value) = value.strip_prefix("r#") {
        Ident::new_raw(value, Span::call_site())
    } else if syn::parse_str::<Ident>(value).is_err() {
        if matches!(value, "crate" | "self" | "Self" | "super") {
            Ident::new(&format!("__uniffi_{value}"), Span::call_site())
        } else {
            Ident::new_raw(value, Span::call_site())
        }
    } else {
        Ident::new(value, Span::call_site())
    }
}

/// Render an operation argument at the Rust call boundary.
///
/// The normalized binding keeps the callable's source-level ownership.  A
/// borrowed argument is lowered to the engine's owned carrier first, then
/// passed by reference to the generated trampoline.  Keeping this decision
/// on the binding (rather than inferring it from a type or method name)
/// preserves by-value arguments while matching the original Rust signature.
pub(super) fn rust_call_argument(expression: TokenStream, ownership: Ownership) -> TokenStream {
    match ownership {
        Ownership::Owned => expression,
        Ownership::Borrowed => quote!(&#expression),
    }
}

fn syn_type(ty: &RustType, engine: &str) -> Result<Type> {
    let text = match ty {
        RustType::Unit => "()".to_owned(),
        RustType::Scalar(scalar) => match scalar {
            uniffi_js_abi::ScalarType::Bool => "bool",
            uniffi_js_abi::ScalarType::I8 => "i8",
            uniffi_js_abi::ScalarType::I16 => "i16",
            uniffi_js_abi::ScalarType::I32 => "i32",
            uniffi_js_abi::ScalarType::I64 => "i64",
            uniffi_js_abi::ScalarType::U8 => "u8",
            uniffi_js_abi::ScalarType::U16 => "u16",
            uniffi_js_abi::ScalarType::U32 => "u32",
            uniffi_js_abi::ScalarType::U64 => "u64",
            uniffi_js_abi::ScalarType::F32 => "f32",
            uniffi_js_abi::ScalarType::F64 => "f64",
            uniffi_js_abi::ScalarType::String => "String",
            uniffi_js_abi::ScalarType::Bytes => "Vec<u8>",
        }
        .to_owned(),
        RustType::Timestamp => "std::time::SystemTime".to_owned(),
        RustType::Duration => "std::time::Duration".to_owned(),
        RustType::Path(path) => path.segments.join("::"),
        RustType::Option(inner) => {
            format!("Option<{}>", syn_type(inner, engine)?.to_token_stream())
        }
        RustType::Sequence(inner) => format!("Vec<{}>", syn_type(inner, engine)?.to_token_stream()),
        RustType::Map(key, value) => format!(
            "std::collections::HashMap<{}, {}>",
            syn_type(key, engine)?.to_token_stream(),
            syn_type(value, engine)?.to_token_stream()
        ),
        RustType::Set(inner) => format!(
            "std::collections::HashSet<{}>",
            syn_type(inner, engine)?.to_token_stream()
        ),
        RustType::Stream { item: inner, .. } => format!(
            "std::sync::Arc<{}>",
            syn_type(inner, engine)?.to_token_stream()
        ),
        RustType::InputStream { item: inner, .. } => format!(
            "std::sync::Arc<{}>",
            syn_type(inner, engine)?.to_token_stream()
        ),
        RustType::StreamStep { item, error } => format!(
            "std::result::Result<{}, {}>",
            syn_type(item, engine)?.to_token_stream(),
            syn_type(error, engine)?.to_token_stream()
        ),
        RustType::Custom(inner) => syn_type(inner, engine)?.to_token_stream().to_string(),
    };
    syn::parse_str(&text).with_context(|| format!("invalid {engine} Rust carrier type {text:?}"))
}

fn path_for_helper(operation: u32, position: &str, prefix: &str) -> Path {
    syn::parse_str(&format!("crate::__uniffi_{prefix}_{operation}_{position}"))
        .expect("generated helper path is a valid Rust identifier")
}

fn ident_for_helper(operation: u32, position: &str, prefix: &str) -> Ident {
    rust_ident(&format!("__uniffi_{prefix}_{operation}_{position}"))
}

fn stream_step_carrier_type(operation: u32) -> Type {
    syn::parse_str(&format!("__UniffiNapiStreamStep{operation}"))
        .expect("generated stream-step carrier type is valid")
}

fn input_stream_step_carrier_ident(operation: u32, resource: u32) -> Ident {
    rust_ident(&format!("__UniffiNapiInputStep{operation}_{resource}"))
}

fn input_stream_helper_ident(operation: u32, resource: u32) -> Ident {
    rust_ident(&format!("__uniffi_input_stream_{operation}_{resource}"))
}

fn operation_host_lower_ident(operation: u32, argument: usize) -> Ident {
    rust_ident(&format!("__uniffi_lower_host_{operation}_{argument}"))
}

fn output_stream_registry_ident(parent: u32, use_site: u32) -> Ident {
    rust_ident(&format!(
        "__UNIFFI_OUTPUT_STREAM_REGISTRY_{parent}_{use_site}"
    ))
}

fn output_stream_registry_path(parent: u32, use_site: u32) -> Path {
    syn::parse_str(&format!(
        "crate::__UNIFFI_OUTPUT_STREAM_REGISTRY_{parent}_{use_site}"
    ))
    .expect("generated output-stream registry path is valid")
}

fn output_stream_close_ident(parent: u32, use_site: u32) -> Ident {
    rust_ident(&format!("__uniffi_close_output_stream_{parent}_{use_site}"))
}

fn path_for_type_helper(type_id: u32, prefix: &str) -> Path {
    syn::parse_str(&format!("crate::__uniffi_{prefix}_type_{type_id}"))
        .expect("generated type helper path is a valid Rust identifier")
}

fn conversion_requires_host(conversion: &ConversionRecipe) -> bool {
    match conversion {
        ConversionRecipe::Callback(_)
        | ConversionRecipe::InputStream { .. }
        | ConversionRecipe::OutputStream { .. } => true,
        ConversionRecipe::Optional(inner)
        | ConversionRecipe::Sequence(inner)
        | ConversionRecipe::Set(inner)
        | ConversionRecipe::Custom(_, inner) => conversion_requires_host(inner),
        ConversionRecipe::Map(key, value) => {
            conversion_requires_host(key) || conversion_requires_host(value)
        }
        ConversionRecipe::StreamStep { item, error } => {
            conversion_requires_host(item) || conversion_requires_host(error)
        }
        _ => false,
    }
}

/// Return the N-API carrier type for one canonical Rust value.  Named values
/// use the generated object/enum carrier below; the core Rust type is only
/// used after the engine trampoline has called the typed lower/lift helper.
fn napi_carrier_type(
    binding: &RustValueBinding,
    named_types: &[uniffi_js_engine_schema::RustNamedTypePlan],
) -> Result<Type> {
    napi_carrier_type_for(binding, named_types, NativeFlavor::Node)
}

/// Engine-private carrier objects used for resources which the N-API session
/// tracks by reference.  The public facade deliberately keeps these objects
/// opaque: it stores the complete value returned by the backend and sends it
/// back unchanged, while the engine reads the `handle` field only at the
/// native dispatch boundary.  Callback IDs remain plain `u32` values and do
/// not use these carriers.
fn napi_object_lease_type() -> Type {
    syn::parse_quote!(__UniffiNapiObjectLease)
}

fn napi_output_stream_lease_type() -> Type {
    syn::parse_quote!(__UniffiNapiOutputStreamLease)
}

fn napi_carrier_type_for(
    binding: &RustValueBinding,
    named_types: &[uniffi_js_engine_schema::RustNamedTypePlan],
    flavor: NativeFlavor,
) -> Result<Type> {
    fn recurse(
        ty: &RustType,
        conversion: &ConversionRecipe,
        named_types: &[uniffi_js_engine_schema::RustNamedTypePlan],
        flavor: NativeFlavor,
    ) -> Result<Type> {
        let type_for_id = |id: uniffi_js_abi::TypeId| {
            if named_types.iter().any(|entry| entry.id == id) {
                Ok::<Type, anyhow::Error>(syn::parse_str(&format!(
                    "__UniffiNapiType{}",
                    id.index()
                ))?)
            } else {
                Err(anyhow!("missing canonical Rust named type {}", id.index()))
            }
        };
        match conversion {
            ConversionRecipe::BigInt
                if matches!(
                    ty,
                    RustType::Scalar(
                        uniffi_js_abi::ScalarType::I64 | uniffi_js_abi::ScalarType::U64
                    )
                ) =>
            {
                syn::parse_str(&format!("{}::bindgen_prelude::BigInt", flavor.prefix()))
                    .map_err(Into::into)
            }
            ConversionRecipe::Record(id)
            | ConversionRecipe::Enum(id)
            | ConversionRecipe::Error(id) => type_for_id(*id),
            ConversionRecipe::Object(_) => Ok(napi_object_lease_type()),
            ConversionRecipe::Callback(_) | ConversionRecipe::InputStream { .. } => {
                syn::parse_str("u32").map_err(Into::into)
            }
            ConversionRecipe::OutputStream { .. } => Ok(napi_output_stream_lease_type()),
            ConversionRecipe::Custom(id, inner) => {
                // Operation bindings carry a named custom's Rust path (for
                // example `core::Email`) rather than a `RustType::Custom`
                // wrapper.  Resolve the canonical builtin binding from the
                // named-type table before recursing, otherwise napi-rs would
                // try to marshal the core custom type itself.
                if let Some(named) = named_types.iter().find(|entry| entry.id == *id) {
                    if let uniffi_js_engine_schema::RustNamedTypeKind::Custom {
                        inner: named_inner,
                        ..
                    } = &named.kind
                    {
                        return recurse(
                            &named_inner.rust_type,
                            &named_inner.conversion,
                            named_types,
                            flavor,
                        );
                    }
                }
                let inner_ty = match ty {
                    RustType::Custom(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                recurse(inner_ty, inner, named_types, flavor)
            }
            ConversionRecipe::Optional(inner) => {
                let inner_ty = match ty {
                    RustType::Option(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                Ok(syn::parse_str(&format!(
                    "Option<{}>",
                    recurse(inner_ty, inner, named_types, flavor)?.to_token_stream()
                ))?)
            }
            ConversionRecipe::Sequence(inner) => {
                let inner_ty = match ty {
                    RustType::Sequence(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                Ok(syn::parse_str(&format!(
                    "Vec<{}>",
                    recurse(inner_ty, inner, named_types, flavor)?.to_token_stream()
                ))?)
            }
            ConversionRecipe::Map(key, value) => {
                let (key_ty, value_ty) = match ty {
                    RustType::Map(key_ty, value_ty) => (key_ty.as_ref(), value_ty.as_ref()),
                    _ => (ty, ty),
                };
                Ok(syn::parse_str(&format!(
                    "__UniffiNapiMap<{}, {}>",
                    recurse(key_ty, key, named_types, flavor)?.to_token_stream(),
                    recurse(value_ty, value, named_types, flavor)?.to_token_stream()
                ))?)
            }
            ConversionRecipe::Set(inner) => {
                let inner_ty = match ty {
                    RustType::Set(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                Ok(syn::parse_str(&format!(
                    "std::collections::HashSet<{}>",
                    recurse(inner_ty, inner, named_types, flavor)?.to_token_stream()
                ))?)
            }
            ConversionRecipe::Timestamp => syn::parse_str("__UniffiTimestamp").map_err(Into::into),
            ConversionRecipe::Duration => syn::parse_str("__UniffiDuration").map_err(Into::into),
            // JavaScript has one IEEE-754 Number carrier.  Keep the Rust
            // bridge's f32 semantics by converting at the typed helper
            // boundary, while asking napi-rs to marshal an f64.
            ConversionRecipe::Identity if matches!(ty, RustType::Scalar(ScalarType::F32)) => {
                syn::parse_str("f64").map_err(Into::into)
            }
            // A scalar carrier remains the exact type expected by napi-rs.
            // BigInt is handled explicitly by napi_operation below.
            _ => syn_type(ty, flavor.prefix()),
        }
    }
    recurse(&binding.rust_type, &binding.conversion, named_types, flavor)
}

fn family_kind(kind: OperationKind) -> FamilyKind {
    use uniffi_js_abi::OperationKind as K;
    match kind {
        K::Function => FamilyKind::Function,
        K::Constructor => FamilyKind::Constructor,
        K::Method => FamilyKind::Method,
        K::CallbackMethod => FamilyKind::CallbackMethod,
        K::InputStreamPull => FamilyKind::InputStreamPull,
        K::InputStreamCancel => FamilyKind::InputStreamCancel,
        K::OutputStreamStart => FamilyKind::OutputStreamStart,
        K::OutputStreamNext => FamilyKind::OutputStreamNext,
        K::OutputStreamCancel => FamilyKind::OutputStreamCancel,
    }
}

fn family_async(kind: AsyncKind) -> FamilyAsync {
    match kind {
        AsyncKind::Sync => FamilyAsync::Sync,
        AsyncKind::Async => FamilyAsync::Async,
    }
}

fn family_path(path: &ValuePath) -> FamilyPathType {
    use napi_uniffi_engine::napi_family_core::ValuePathSegment as S;
    FamilyPathType::new(
        path.segments()
            .iter()
            .map(|segment| match segment {
                ValuePathSegment::Argument(index) => S::Argument(*index),
                ValuePathSegment::Return => S::Return,
                ValuePathSegment::Field(name) => S::Field(name.clone()),
                ValuePathSegment::Variant(name) => S::Variant(name.clone()),
                ValuePathSegment::SequenceItem => S::SequenceElement,
                ValuePathSegment::SetItem => S::SetElement,
                ValuePathSegment::MapKey => S::MapKey,
                ValuePathSegment::MapValue => S::MapValue,
            })
            .collect::<Vec<_>>(),
    )
}

fn family_receiver_ownership(ownership: Ownership) -> FamilyResourceOwnership {
    match ownership {
        Ownership::Owned => FamilyResourceOwnership::ByArc,
        Ownership::Borrowed => FamilyResourceOwnership::Borrowed,
    }
}

/// Object receivers are held by the package-local lease registry as an
/// `Arc<T>`. The canonical plan keeps source Rust paths independent from that
/// engine-owned carrier detail, so adapters derive the concrete carrier only
/// at this boundary.
fn object_core_type(package: &NormalizedPackage, type_id: uniffi_js_abi::TypeId) -> Result<Type> {
    let named = package
        .rust
        .named_type(type_id)
        .ok_or_else(|| anyhow!("missing object type {}", type_id.index()))?;
    let path = rust_path(&named.rust_path)?;
    let trait_object = matches!(
        named.kind,
        uniffi_js_engine_schema::RustNamedTypeKind::Object {
            kind: uniffi_js_engine_schema::RustObjectKind::TraitRustOnly
                | uniffi_js_engine_schema::RustObjectKind::TraitBoth
                | uniffi_js_engine_schema::RustObjectKind::TraitForeignOnly,
        }
    );
    if trait_object {
        Ok(syn::parse_quote!(std::sync::Arc<dyn #path>))
    } else {
        Ok(syn::parse_quote!(std::sync::Arc<#path>))
    }
}

fn core_type_for_binding(package: &NormalizedPackage, binding: &RustValueBinding) -> Result<Type> {
    fn recurse(
        package: &NormalizedPackage,
        ty: &RustType,
        conversion: &ConversionRecipe,
    ) -> Result<Type> {
        match conversion {
            ConversionRecipe::Object(id) => object_core_type(package, *id),
            ConversionRecipe::Callback(id) => {
                let named = package
                    .rust
                    .named_type(*id)
                    .ok_or_else(|| anyhow!("missing callback type {}", id.index()))?;
                let path = rust_path(&named.rust_path)?;
                Ok(syn::parse_quote!(std::sync::Arc<dyn #path>))
            }
            ConversionRecipe::Optional(inner) => {
                let inner_ty = match ty {
                    RustType::Option(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                let inner = recurse(package, inner_ty, inner)?;
                Ok(syn::parse_quote!(Option<#inner>))
            }
            ConversionRecipe::Sequence(inner) => {
                let inner_ty = match ty {
                    RustType::Sequence(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                let inner = recurse(package, inner_ty, inner)?;
                Ok(syn::parse_quote!(Vec<#inner>))
            }
            ConversionRecipe::Set(inner) => {
                let inner_ty = match ty {
                    RustType::Set(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                let inner = recurse(package, inner_ty, inner)?;
                Ok(syn::parse_quote!(std::collections::HashSet<#inner>))
            }
            ConversionRecipe::Map(key, value) => {
                let (key_ty, value_ty) = match ty {
                    RustType::Map(key_ty, value_ty) => (key_ty.as_ref(), value_ty.as_ref()),
                    _ => (ty, ty),
                };
                let key = recurse(package, key_ty, key)?;
                let value = recurse(package, value_ty, value)?;
                Ok(syn::parse_quote!(std::collections::HashMap<#key, #value>))
            }
            ConversionRecipe::OutputStream { item, error } => {
                let (item_ty, error_ty) = match ty {
                    RustType::Stream { item, error, .. } => (item.as_ref(), error.as_ref()),
                    _ => (ty, ty),
                };
                let item = recurse(package, item_ty, item)?;
                let error = recurse(package, error_ty, error)?;
                Ok(syn::parse_quote!(uniffi::UniFfiStream<#item, #error>))
            }
            ConversionRecipe::InputStream { item, error } => {
                let (item_ty, error_ty) = match ty {
                    RustType::InputStream { item, error, .. } => (item.as_ref(), error.as_ref()),
                    _ => (ty, ty),
                };
                let item = recurse(package, item_ty, item)?;
                let error = recurse(package, error_ty, error)?;
                Ok(syn::parse_quote!(uniffi::UniFfiInputStream<#item, #error>))
            }
            ConversionRecipe::StreamStep { item, error } => {
                let (item_ty, error_ty) = match ty {
                    RustType::StreamStep { item, error } => (item.as_ref(), error.as_ref()),
                    _ => (ty, ty),
                };
                let item = recurse(package, item_ty, item)?;
                let error = recurse(package, error_ty, error)?;
                Ok(syn::parse_quote!(uniffi::UniFfiStreamStep<#item, #error>))
            }
            ConversionRecipe::Custom(_, inner) => recurse(package, ty, inner),
            _ => syn_type(ty, "napi"),
        }
    }
    recurse(package, &binding.rust_type, &binding.conversion)
}

fn stream_binding(binding: &RustValueBinding) -> FamilyStreamBinding {
    use napi_uniffi_engine::napi_family_core::CarrierKind;
    FamilyStreamBinding {
        carrier: match binding.carrier {
            RustCarrier::Primitive => CarrierKind::Primitive,
            RustCarrier::BigInt => CarrierKind::BigInt,
            RustCarrier::Bytes => CarrierKind::Bytes,
            RustCarrier::Timestamp => CarrierKind::Timestamp,
            RustCarrier::Duration => CarrierKind::Duration,
            RustCarrier::LocalAdapter => CarrierKind::LocalAdapter,
            RustCarrier::OpaqueHandle => CarrierKind::OpaqueHandle,
            RustCarrier::CallbackProxy => CarrierKind::CallbackProxy,
            RustCarrier::InputStream => CarrierKind::InputStream,
            RustCarrier::OutputStream => CarrierKind::OutputStream,
            RustCarrier::StreamStep => CarrierKind::StreamStep,
        },
        conversion: family_conversion(&binding.conversion),
    }
}

fn family_conversion(
    conversion: &ConversionRecipe,
) -> napi_uniffi_engine::napi_family_core::ConversionRecipe {
    use napi_uniffi_engine::napi_family_core::ConversionRecipe as C;
    match conversion {
        ConversionRecipe::Identity
        | ConversionRecipe::Timestamp
        | ConversionRecipe::Duration
        | ConversionRecipe::BigInt
        | ConversionRecipe::Bytes => C::Identity,
        ConversionRecipe::Optional(inner) => C::Optional(Box::new(family_conversion(inner))),
        ConversionRecipe::Sequence(inner) => C::Sequence(Box::new(family_conversion(inner))),
        ConversionRecipe::Map(key, value) => C::Map(
            Box::new(family_conversion(key)),
            Box::new(family_conversion(value)),
        ),
        ConversionRecipe::Set(inner) => C::Set(Box::new(family_conversion(inner))),
        ConversionRecipe::Record(id) => C::Record(id.index()),
        ConversionRecipe::Enum(id) => C::Enum(id.index()),
        ConversionRecipe::Error(id) => C::Error(id.index()),
        ConversionRecipe::Object(id) => C::Object(id.index()),
        ConversionRecipe::Custom(id, inner) => {
            C::Custom(id.index(), Box::new(family_conversion(inner)))
        }
        ConversionRecipe::Callback(id) => C::Callback(id.index()),
        ConversionRecipe::InputStream { item, .. } => {
            C::InputStream(Box::new(family_conversion(item)))
        }
        ConversionRecipe::OutputStream { item, .. } => {
            C::OutputStream(Box::new(family_conversion(item)))
        }
        ConversionRecipe::StreamStep { item, error } => C::StreamStep {
            item: Box::new(family_conversion(item)),
            error: Box::new(family_conversion(error)),
        },
    }
}

fn callbacks_for(bridge: &BridgePlan, operation_id: u32) -> Vec<FamilyCallback> {
    bridge
        .callbacks()
        .iter()
        .filter(|callback| callback.operation_id.index() == operation_id)
        .map(|callback| FamilyCallback {
            operation_id,
            callback_type_id: callback.callback_type.index(),
            path: family_path(&callback.path),
            contract: napi_uniffi_engine::napi_family_core::CallbackContract {
                retention: match callback.contract.retention {
                    uniffi_js_engine_schema::CallbackRetention::Scoped => {
                        napi_uniffi_engine::napi_family_core::CallbackRetention::Scoped
                    }
                    uniffi_js_engine_schema::CallbackRetention::Retained => {
                        napi_uniffi_engine::napi_family_core::CallbackRetention::Retained
                    }
                },
                threading: match callback.contract.threading {
                    uniffi_js_engine_schema::CallbackThreading::CallingThread => {
                        napi_uniffi_engine::napi_family_core::CallbackThreading::CallingThread
                    }
                    uniffi_js_engine_schema::CallbackThreading::MayCrossThread => {
                        napi_uniffi_engine::napi_family_core::CallbackThreading::MayCrossThread
                    }
                },
                reentrancy: match callback.contract.reentrancy {
                    uniffi_js_engine_schema::CallbackReentrancy::Forbidden => {
                        napi_uniffi_engine::napi_family_core::CallbackReentrancy::Forbidden
                    }
                    uniffi_js_engine_schema::CallbackReentrancy::Allowed => {
                        napi_uniffi_engine::napi_family_core::CallbackReentrancy::Allowed
                    }
                },
            },
        })
        .collect()
}

fn streams_for(bridge: &BridgePlan, operation: &RustOperationPlan) -> Vec<FamilyStream> {
    bridge
        .streams()
        .iter()
        .filter(|stream| stream.operation_id == operation.operation_id)
        .filter_map(|stream| {
            let resource = operation
                .stream_resources
                .iter()
                .find(|resource| resource.id == stream.id)?;
            Some(FamilyStream {
                operation_id: operation.operation_id.index(),
                use_site_id: stream.id.index(),
                path: family_path(&stream.path),
                direction: match stream.contract.direction {
                    uniffi_js_engine_schema::StreamDirection::Input => {
                        napi_uniffi_engine::napi_family_core::StreamDirection::Input
                    }
                    uniffi_js_engine_schema::StreamDirection::Output => {
                        napi_uniffi_engine::napi_family_core::StreamDirection::Output
                    }
                },
                item: stream_binding(&resource.item),
                error: stream_binding(&resource.error),
                is_send: resource.is_send,
                slots: resource
                    .slot_operation_ids
                    .iter()
                    .map(|(kind, id)| FamilyStreamSlot {
                        use_site_id: stream.id.index(),
                        operation_id: id.index(),
                        kind: family_kind(*kind),
                    })
                    .collect(),
            })
        })
        .collect()
}

fn operation_dispatch(operation: &RustOperationPlan) -> FamilyDispatch {
    match operation.call_target {
        RustCallTarget::CallbackMethod {
            callback_type,
            method_id,
            ..
        } => FamilyDispatch::CallbackHost {
            callback_type_id: callback_type.index(),
            method_id,
        },
        RustCallTarget::StreamHook { hook, .. } => match hook {
            RustResourceHook::PullInputStream => FamilyDispatch::InputStreamHostPull,
            RustResourceHook::CancelInputStream => FamilyDispatch::InputStreamHostCancel,
            _ => FamilyDispatch::Native,
        },
        _ => FamilyDispatch::Native,
    }
}

fn operation_receiver(operation: &RustOperationPlan) -> Option<FamilyReceiver> {
    let receiver = operation.receiver.as_ref()?;
    match receiver.carrier {
        RustCarrier::InputStream => Some(FamilyReceiver::Resource(FamilyResource {
            kind: FamilyResourceKind::InputStream,
            ownership: family_receiver_ownership(receiver.ownership),
        })),
        RustCarrier::OutputStream => Some(FamilyReceiver::Resource(FamilyResource {
            kind: FamilyResourceKind::OutputStream,
            ownership: family_receiver_ownership(receiver.ownership),
        })),
        _ => match receiver.conversion {
            ConversionRecipe::Object(_) => Some(FamilyReceiver::Resource(FamilyResource {
                kind: FamilyResourceKind::Object,
                ownership: family_receiver_ownership(receiver.ownership),
            })),
            _ => Some(FamilyReceiver::Value),
        },
    }
}

fn operation_result_resources(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
) -> Result<Vec<FamilyResultResource>> {
    operation_result_resource_paths(
        package,
        operation,
        vec![ResourcePathSegment::Return],
        Ownership::Owned,
    )
    .map(|resources| {
        resources
            .into_iter()
            .map(|resource| FamilyResultResource {
                operation_id: operation.operation_id.index(),
                path: family_resource_path(&resource.path),
                binding: FamilyResource {
                    kind: resource.kind,
                    ownership: match resource.ownership {
                        Ownership::Borrowed => FamilyResourceOwnership::Borrowed,
                        Ownership::Owned => FamilyResourceOwnership::Owned,
                    },
                },
            })
            .collect()
    })
}

fn operation_result_resource_paths(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
    root: Vec<ResourcePathSegment>,
    ownership: Ownership,
) -> Result<Vec<ExpandedResourceUseSite>> {
    let Some(result) = operation.return_value.as_ref() else {
        return Ok(Vec::new());
    };
    let mut resources = Vec::new();
    let mut visiting = std::collections::BTreeSet::new();
    fn walk(
        package: &NormalizedPackage,
        operation_id: u32,
        path: Vec<ResourcePathSegment>,
        ty: &RustType,
        conversion: &ConversionRecipe,
        ownership: Ownership,
        resources: &mut Vec<ExpandedResourceUseSite>,
        visiting: &mut std::collections::BTreeSet<u32>,
    ) -> Result<()> {
        let push = |kind: FamilyResourceKind, resources: &mut Vec<ExpandedResourceUseSite>| {
            resources.push(ExpandedResourceUseSite {
                path: path.clone(),
                kind,
                type_id: None,
                ownership,
            });
        };
        match conversion {
            ConversionRecipe::Object(id) => resources.push(ExpandedResourceUseSite {
                path,
                kind: FamilyResourceKind::Object,
                type_id: Some(id.index()),
                ownership,
            }),
            ConversionRecipe::InputStream { .. } => {
                push(FamilyResourceKind::InputStream, resources)
            }
            ConversionRecipe::OutputStream { .. } => {
                push(FamilyResourceKind::OutputStream, resources)
            }
            ConversionRecipe::Optional(inner) => {
                let inner_ty = match ty {
                    RustType::Option(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                let path = path
                    .into_iter()
                    .chain([ResourcePathSegment::Optional])
                    .collect();
                walk(
                    package,
                    operation_id,
                    path,
                    inner_ty,
                    inner,
                    ownership,
                    resources,
                    visiting,
                )?;
            }
            ConversionRecipe::Sequence(inner) => {
                let inner_ty = match ty {
                    RustType::Sequence(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                walk(
                    package,
                    operation_id,
                    path.into_iter()
                        .chain([ResourcePathSegment::SequenceItem])
                        .collect(),
                    inner_ty,
                    inner,
                    ownership,
                    resources,
                    visiting,
                )?;
            }
            ConversionRecipe::Set(inner) => {
                let inner_ty = match ty {
                    RustType::Set(inner_ty) => inner_ty.as_ref(),
                    _ => ty,
                };
                walk(
                    package,
                    operation_id,
                    path.into_iter()
                        .chain([ResourcePathSegment::SetItem])
                        .collect(),
                    inner_ty,
                    inner,
                    ownership,
                    resources,
                    visiting,
                )?;
            }
            ConversionRecipe::Map(key, value) => {
                let (key_ty, value_ty) = match ty {
                    RustType::Map(key_ty, value_ty) => (key_ty.as_ref(), value_ty.as_ref()),
                    _ => (ty, ty),
                };
                walk(
                    package,
                    operation_id,
                    path.iter()
                        .cloned()
                        .chain([ResourcePathSegment::MapKey])
                        .collect(),
                    key_ty,
                    key,
                    ownership,
                    resources,
                    visiting,
                )?;
                walk(
                    package,
                    operation_id,
                    path.into_iter()
                        .chain([ResourcePathSegment::MapValue])
                        .collect(),
                    value_ty,
                    value,
                    ownership,
                    resources,
                    visiting,
                )?;
            }
            ConversionRecipe::Custom(_, inner) => walk(
                package,
                operation_id,
                path,
                ty,
                inner,
                ownership,
                resources,
                visiting,
            )?,
            ConversionRecipe::StreamStep { item, error } => {
                let (item_ty, error_ty) = match ty {
                    RustType::StreamStep { item, error } => (item.as_ref(), error.as_ref()),
                    _ => (ty, ty),
                };
                walk(
                    package,
                    operation_id,
                    path.iter()
                        .cloned()
                        .chain([ResourcePathSegment::StreamItem])
                        .collect(),
                    item_ty,
                    item,
                    ownership,
                    resources,
                    visiting,
                )?;
                walk(
                    package,
                    operation_id,
                    path.into_iter()
                        .chain([ResourcePathSegment::StreamError])
                        .collect(),
                    error_ty,
                    error,
                    ownership,
                    resources,
                    visiting,
                )?;
            }
            ConversionRecipe::Record(id) => {
                if !visiting.insert(id.index()) {
                    return Ok(());
                }
                let result = (|| -> Result<()> {
                    let named = package
                        .rust
                        .named_type(*id)
                        .ok_or_else(|| anyhow!("missing Rust named type {}", id.index()))?;
                    let uniffi_js_engine_schema::RustNamedTypeKind::Record { fields } = &named.kind
                    else {
                        bail!("Rust named type {} is not a record", id.index());
                    };
                    for field in fields {
                        walk(
                            package,
                            operation_id,
                            path.iter()
                                .cloned()
                                .chain([ResourcePathSegment::Field(field.public_name.clone())])
                                .collect(),
                            &field.binding.rust_type,
                            &field.binding.conversion,
                            ownership,
                            resources,
                            visiting,
                        )?;
                    }
                    Ok(())
                })();
                visiting.remove(&id.index());
                result?;
            }
            ConversionRecipe::Enum(id) | ConversionRecipe::Error(id) => {
                if !visiting.insert(id.index()) {
                    return Ok(());
                }
                let result = (|| -> Result<()> {
                    let named = package
                        .rust
                        .named_type(*id)
                        .ok_or_else(|| anyhow!("missing Rust named type {}", id.index()))?;
                    let variants = match (&conversion, &named.kind) {
                        (
                            ConversionRecipe::Enum(_),
                            uniffi_js_engine_schema::RustNamedTypeKind::Enum { variants },
                        )
                        | (
                            ConversionRecipe::Error(_),
                            uniffi_js_engine_schema::RustNamedTypeKind::Error { variants },
                        ) => variants,
                        (ConversionRecipe::Enum(_), _) => {
                            bail!("Rust named type {} is not an enum", id.index());
                        }
                        (ConversionRecipe::Error(_), _) => {
                            bail!("Rust named type {} is not an error", id.index());
                        }
                        _ => unreachable!("enum/error conversion arm is exhaustive"),
                    };
                    for variant in variants {
                        let variant_path = path
                            .iter()
                            .cloned()
                            .chain([ResourcePathSegment::Variant(variant.public_name.clone())])
                            .collect::<Vec<_>>();
                        match &variant.payload {
                            uniffi_js_engine_schema::RustVariantPayload::Named(fields) => {
                                for field in fields {
                                    walk(
                                        package,
                                        operation_id,
                                        variant_path
                                            .iter()
                                            .cloned()
                                            .chain([ResourcePathSegment::Field(
                                                field.public_name.clone(),
                                            )])
                                            .collect(),
                                        &field.binding.rust_type,
                                        &field.binding.conversion,
                                        ownership,
                                        resources,
                                        visiting,
                                    )?;
                                }
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Tuple(fields) => {
                                for field in fields {
                                    walk(
                                        package,
                                        operation_id,
                                        variant_path
                                            .iter()
                                            .cloned()
                                            .chain([ResourcePathSegment::Field(
                                                field.public_name.clone(),
                                            )])
                                            .collect(),
                                        &field.binding.rust_type,
                                        &field.binding.conversion,
                                        ownership,
                                        resources,
                                        visiting,
                                    )?;
                                }
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Unit => {}
                        }
                    }
                    Ok(())
                })();
                visiting.remove(&id.index());
                result?;
            }
            ConversionRecipe::Identity
            | ConversionRecipe::Timestamp
            | ConversionRecipe::Duration
            | ConversionRecipe::BigInt
            | ConversionRecipe::Bytes
            | ConversionRecipe::Callback(_) => {}
        }
        Ok(())
    }
    walk(
        package,
        operation.operation_id.index(),
        root,
        &result.rust_type,
        &result.conversion,
        ownership,
        &mut resources,
        &mut visiting,
    )?;
    Ok(resources)
}

fn operation_stream_slot(
    operations: &[RustOperationPlan],
    operation: &RustOperationPlan,
) -> Option<FamilyStreamSlot> {
    // The canonical stream group owns the slot mapping.  Looking at the call
    // target is insufficient because the output-start operation is a normal
    // native operation and the synthetic pull/cancel operations are stream
    // hooks.  Resolve every operation ID mechanically from the group instead.
    operations.iter().find_map(|owner| {
        owner.stream_resources.iter().find_map(|resource| {
            resource
                .slot_operation_ids
                .iter()
                .find_map(|(kind, operation_id)| {
                    (operation_id == &operation.operation_id).then_some(FamilyStreamSlot {
                        use_site_id: resource.id.index(),
                        operation_id: operation.operation_id.index(),
                        kind: family_kind(*kind),
                    })
                })
        })
    })
}

fn family_operations(
    package: &NormalizedPackage,
    flavor: FamilyFlavor,
) -> Result<Vec<FamilyOperation>> {
    let bridge = &package.bridge;
    let engine = match flavor {
        FamilyFlavor::Node => EngineKind::Napi,
        FamilyFlavor::Ohos => EngineKind::OhosNapi,
    };
    let operations = package.rust.engines[&engine].operations.as_slice();
    operations
        .iter()
        .map(|operation| -> Result<FamilyOperation> {
            let dispatch = operation_dispatch(operation);
            let native = matches!(dispatch, FamilyDispatch::Native);
            Ok(FamilyOperation {
                id: operation.operation_id.index(),
                kind: family_kind(operation.kind),
                async_kind: family_async(operation.async_kind),
                fallible: operation.throws.is_some(),
                argument_count: operation.arguments.len(),
                dispatch,
                receiver: if matches!(
                    operation.kind,
                    OperationKind::Method
                        | OperationKind::InputStreamPull
                        | OperationKind::InputStreamCancel
                        | OperationKind::OutputStreamNext
                        | OperationKind::OutputStreamCancel
                ) {
                    operation_receiver(operation)
                } else {
                    None
                },
                result_resources: if native {
                    operation_result_resources(package, operation)?
                } else {
                    Vec::new()
                },
                callbacks: native
                    .then(|| callbacks_for(bridge, operation.operation_id.index()))
                    .unwrap_or_default(),
                streams: native
                    .then(|| streams_for(bridge, operation))
                    .unwrap_or_default(),
                stream_slot: operation_stream_slot(operations, operation),
            })
        })
        .collect()
}

fn family_plan(package: &NormalizedPackage, flavor: FamilyFlavor) -> Result<FamilyPlan> {
    FamilyPlan::build(FamilyInput {
        flavor,
        close_policy: napi_uniffi_engine::ClosePolicy {
            grace_ms: package.bridge.close_policy().grace_ms,
            on_deadline: napi_uniffi_engine::DeadlineAction::Detach,
        },
        operations: family_operations(package, flavor)?,
    })
    .map_err(|error| anyhow!("N-API family plan: {error}"))
}

fn lower_expr(
    binding: &RustValueBinding,
    expression: TokenStream,
    flavor: NativeFlavor,
) -> Result<TokenStream> {
    let engine_crate = engine_crate_path(flavor);
    let error_descriptor = flavor.error_descriptor();
    let value = match &binding.conversion {
        ConversionRecipe::Identity => match binding.rust_type {
            RustType::Scalar(uniffi_js_abi::ScalarType::F32) => quote!(#expression as f32),
            _ => expression,
        },
        ConversionRecipe::BigInt => match binding.rust_type {
            RustType::Scalar(uniffi_js_abi::ScalarType::I64) => quote!({
                let (__uniffi_value, __uniffi_lossless) = #expression.get_i64();
                #engine_crate::napi_family_core::require_lossless_i64(
                    __uniffi_value,
                    __uniffi_lossless,
                )
                .map_err(|error| #error_descriptor::validation(error.to_string()))?
            }),
            RustType::Scalar(uniffi_js_abi::ScalarType::U64) => quote!({
                let (__uniffi_negative, __uniffi_value, __uniffi_lossless) = #expression.get_u64();
                #engine_crate::napi_family_core::require_lossless_u64(
                    __uniffi_negative,
                    __uniffi_value,
                    __uniffi_lossless,
                )
                .map_err(|error| #error_descriptor::validation(error.to_string()))?
            }),
            _ => expression,
        },
        ConversionRecipe::Record(_) | ConversionRecipe::Enum(_) | ConversionRecipe::Error(_) => {
            let id = match &binding.conversion {
                ConversionRecipe::Record(id)
                | ConversionRecipe::Enum(id)
                | ConversionRecipe::Error(id) => id.index(),
                _ => unreachable!(),
            };
            let helper = path_for_type_helper(id, "lower");
            quote!(#helper(#expression)?)
        }
        ConversionRecipe::Object(id) => {
            let helper = path_for_type_helper(id.index(), "lower");
            quote!(#helper(#expression)?)
        }
        ConversionRecipe::Optional(inner) => {
            let nested = lower_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Option(inner_ty) => inner_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *inner.clone(),
                },
                quote!(value),
                flavor,
            )?;
            quote!(#expression
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#nested) })
                .transpose()?)
        }
        ConversionRecipe::Sequence(inner) => {
            let nested = lower_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Sequence(inner_ty) => inner_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *inner.clone(),
                },
                quote!(value),
                flavor,
            )?;
            quote!(#expression
                .into_iter()
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#nested) })
                .collect::<::std::result::Result<Vec<_>, #error_descriptor>>()?)
        }
        ConversionRecipe::Set(inner) => {
            let nested = lower_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Set(inner_ty) => inner_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *inner.clone(),
                },
                quote!(value),
                flavor,
            )?;
            quote!(#expression
                .into_iter()
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#nested) })
                .collect::<::std::result::Result<std::collections::HashSet<_>, #error_descriptor>>()?)
        }
        ConversionRecipe::Map(key, value) => {
            let key_expression = lower_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Map(key_ty, _) => key_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *key.clone(),
                },
                quote!(key),
                flavor,
            )?;
            let value_expression = lower_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Map(_, value_ty) => value_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *value.clone(),
                },
                quote!(value),
                flavor,
            )?;
            quote!(#expression
                .into_iter()
                .map(|(key, value)| -> ::std::result::Result<_, #error_descriptor> {
                    Ok(({ #key_expression }, { #value_expression }))
                })
                .collect::<::std::result::Result<std::collections::HashMap<_, _>, #error_descriptor>>()?)
        }
        ConversionRecipe::Custom(id, _) => {
            let helper = path_for_type_helper(id.index(), "lower");
            quote!(#helper(#expression)?)
        }
        ConversionRecipe::Timestamp | ConversionRecipe::Duration => quote!(#expression.0),
        // The engine owns dedicated carriers for object/callback/stream
        // values.  Their lifecycle helpers are supplied separately; this
        // branch is intentionally conservative until the family adapter has
        // a concrete resource hook for the use site.
        _ => expression,
    };
    Ok(value)
}

fn lift_expr(
    binding: &RustValueBinding,
    expression: TokenStream,
    flavor: NativeFlavor,
) -> Result<TokenStream> {
    let napi = napi_crate_path(flavor);
    let error_descriptor = flavor.error_descriptor();
    // Struct/enum bridge values implement `From<core>` and the reverse.  The
    // recursive container cases preserve the exact named-type boundaries.
    match &binding.conversion {
        ConversionRecipe::Identity => match binding.rust_type {
            RustType::Scalar(uniffi_js_abi::ScalarType::F32) => Ok(quote!(#expression as f64)),
            _ => Ok(expression),
        },
        ConversionRecipe::BigInt => match binding.rust_type {
            RustType::Scalar(uniffi_js_abi::ScalarType::I64) => {
                Ok(quote!(#napi::bindgen_prelude::BigInt {
                    sign_bit: #expression < 0,
                    words: vec![#expression.unsigned_abs()],
                }))
            }
            RustType::Scalar(uniffi_js_abi::ScalarType::U64) => {
                Ok(quote!(#napi::bindgen_prelude::BigInt {
                    sign_bit: false,
                    words: vec![#expression],
                }))
            }
            _ => Ok(expression),
        },
        ConversionRecipe::Record(_) | ConversionRecipe::Enum(_) | ConversionRecipe::Error(_) => {
            let id = match &binding.conversion {
                ConversionRecipe::Record(id)
                | ConversionRecipe::Enum(id)
                | ConversionRecipe::Error(id) => id.index(),
                _ => unreachable!(),
            };
            let helper = path_for_type_helper(id, "lift");
            Ok(quote!(#helper(#expression)?))
        }
        ConversionRecipe::Object(id) => {
            let helper = path_for_type_helper(id.index(), "lift");
            Ok(quote!(#helper(#expression)?))
        }
        ConversionRecipe::Optional(inner) => {
            let nested = lift_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Option(inner_ty) => inner_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *inner.clone(),
                },
                quote!(value),
                flavor,
            )?;
            Ok(quote!(#expression
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#nested) })
                .transpose()?))
        }
        ConversionRecipe::Sequence(inner) => {
            let nested = lift_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Sequence(inner_ty) => inner_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *inner.clone(),
                },
                quote!(value),
                flavor,
            )?;
            Ok(quote!(#expression
                .into_iter()
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#nested) })
                .collect::<::std::result::Result<Vec<_>, #error_descriptor>>()?))
        }
        ConversionRecipe::Set(inner) => {
            let nested = lift_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Set(inner_ty) => inner_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *inner.clone(),
                },
                quote!(value),
                flavor,
            )?;
            Ok(quote!(#expression
                .into_iter()
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#nested) })
                .collect::<::std::result::Result<std::collections::HashSet<_>, #error_descriptor>>()?))
        }
        ConversionRecipe::Map(key, value) => {
            let key_expression = lift_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Map(key_ty, _) => key_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *key.clone(),
                },
                quote!(key),
                flavor,
            )?;
            let value_expression = lift_expr(
                &RustValueBinding {
                    rust_type: match &binding.rust_type {
                        RustType::Map(_, value_ty) => value_ty.as_ref().clone(),
                        _ => RustType::Unit,
                    },
                    carrier: binding.carrier,
                    conversion: *value.clone(),
                },
                quote!(value),
                flavor,
            )?;
            Ok(quote!(#expression
                .into_iter()
                .map(|(key, value)| -> ::std::result::Result<_, #error_descriptor> {
                    Ok(({ #key_expression }, { #value_expression }))
                })
                .collect::<::std::result::Result<__UniffiNapiMap<_, _>, #error_descriptor>>()?))
        }
        ConversionRecipe::Custom(id, _) => {
            let helper = path_for_type_helper(id.index(), "lift");
            Ok(quote!(#helper(#expression)?))
        }
        ConversionRecipe::Timestamp => Ok(quote!(crate::__uniffi_lift_timestamp(#expression)?)),
        ConversionRecipe::Duration => Ok(quote!(__UniffiDuration(#expression))),
        _ => Ok(expression),
    }
}

fn render_named_type_helpers_for(
    package: &NormalizedPackage,
    flavor: NativeFlavor,
) -> Result<String> {
    let mut source = TokenStream::new();
    let error_descriptor = flavor.error_descriptor();
    for named in package.rust.named_types() {
        let type_id = named.id.index();
        let bridge_ident = rust_ident(&format!("__UniffiNapiType{type_id}"));
        let core_path = rust_path(&named.rust_path)?;
        match &named.kind {
            uniffi_js_engine_schema::RustNamedTypeKind::Record { fields } => {
                let bridge_fields = fields
                    .iter()
                    .map(|field| {
                        let ident = rust_ident(&field.public_name);
                        let ty = napi_carrier_type_for(
                            &field.binding,
                            package.rust.named_types(),
                            flavor,
                        )?;
                        let public_name = &field.public_name;
                        Ok::<_, anyhow::Error>(quote! {
                            #[napi(js_name = #public_name)]
                            pub #ident: #ty
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lower_fields = fields
                    .iter()
                    .map(|field| {
                        let rust_name = rust_ident(&field.rust_name);
                        let public_name = rust_ident(&field.public_name);
                        let expression =
                            lower_expr(&field.binding, quote!(value.#public_name), flavor)?;
                        Ok::<_, anyhow::Error>(quote!(#rust_name: #expression))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lift_fields = fields
                    .iter()
                    .map(|field| {
                        let rust_name = rust_ident(&field.rust_name);
                        let public_name = rust_ident(&field.public_name);
                        let expression =
                            lift_expr(&field.binding, quote!(value.#rust_name), flavor)?;
                        Ok::<_, anyhow::Error>(quote!(#public_name: #expression))
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lower_name = rust_ident(&format!("__uniffi_lower_type_{type_id}"));
                let lift_name = rust_ident(&format!("__uniffi_lift_type_{type_id}"));
                source.extend(quote! {
                    #[napi(object, use_nullable = true)]
                    #[derive(Clone, Debug)]
                    pub struct #bridge_ident { #(#bridge_fields,)* }

                    fn #lower_name(value: #bridge_ident) -> ::std::result::Result<#core_path, #error_descriptor> {
                        Ok(#core_path { #(#lower_fields,)* })
                    }

                    fn #lift_name(value: #core_path) -> ::std::result::Result<#bridge_ident, #error_descriptor> {
                        Ok(#bridge_ident { #(#lift_fields,)* })
                    }
                });
            }
            // Enum/error carriers are intentionally left to the tagged-union
            // renderer that consumes their full variant payload table.  The
            // primitive+record path is complete and does not silently emit a
            // partial enum representation.
            uniffi_js_engine_schema::RustNamedTypeKind::Enum { .. }
            | uniffi_js_engine_schema::RustNamedTypeKind::Error { .. } => {
                let variants = match &named.kind {
                    uniffi_js_engine_schema::RustNamedTypeKind::Enum { variants }
                    | uniffi_js_engine_schema::RustNamedTypeKind::Error { variants } => variants,
                    _ => unreachable!(),
                };
                let enum_attribute = if variants.iter().all(|variant| {
                    matches!(
                        &variant.payload,
                        uniffi_js_engine_schema::RustVariantPayload::Unit
                    )
                }) {
                    quote!(#[napi(string_enum)])
                } else {
                    quote!(#[napi(discriminant = "tag")])
                };
                let bridge_variants = variants
                    .iter()
                    .map(|variant| {
                        let variant_ident = rust_ident(&variant.public_name);
                        match &variant.payload {
                            uniffi_js_engine_schema::RustVariantPayload::Unit => {
                                Ok::<_, anyhow::Error>(quote!(#variant_ident))
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Named(fields) => {
                                let fields = fields
                                    .iter()
                                    .map(|field| {
                                        let ident = rust_ident(&field.public_name);
                                        let ty = napi_carrier_type_for(
                                            &field.binding,
                                            package.rust.named_types(),
                                            flavor,
                                        )?;
                                        let js_name = &field.public_name;
                                        Ok::<_, anyhow::Error>(quote! {
                                            #[napi(js_name = #js_name)]
                                            #ident: #ty
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                Ok(quote!(#variant_ident { #(#fields,)* }))
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Tuple(fields) => {
                                let fields = fields
                                    .iter()
                                    .map(|field| {
                                        let ident = rust_ident(&field.public_name);
                                        let ty = napi_carrier_type_for(
                                            &field.binding,
                                            package.rust.named_types(),
                                            flavor,
                                        )?;
                                        let js_name = &field.public_name;
                                        Ok::<_, anyhow::Error>(quote! {
                                            #[napi(js_name = #js_name)]
                                            #ident: #ty
                                        })
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                Ok(quote!(#variant_ident { #(#fields,)* }))
                            }
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lower_variants = variants
                    .iter()
                    .map(|variant| {
                        let variant_ident = rust_ident(&variant.public_name);
                        let core_variant_ident = rust_ident(&variant.rust_name);
                        match &variant.payload {
                            uniffi_js_engine_schema::RustVariantPayload::Unit => {
                                Ok::<_, anyhow::Error>(quote!(
                                    #bridge_ident::#variant_ident => #core_path::#core_variant_ident
                                ))
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Named(fields) => {
                                let bindings = fields
                                    .iter()
                                    .map(|field| rust_ident(&field.public_name))
                                    .collect::<Vec<_>>();
                                let lowered = fields
                                    .iter()
                                    .zip(&bindings)
                                    .map(|(field, ident)| {
                                        let rust_name = rust_ident(&field.rust_name);
                                        let expression = lower_expr(&field.binding, quote!(#ident), flavor)?;
                                        Ok::<_, anyhow::Error>(quote!(#rust_name: #expression))
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                Ok(quote!(
                                    #bridge_ident::#variant_ident { #(#bindings),* } => #core_path::#core_variant_ident { #(#lowered),* }
                                ))
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Tuple(fields) => {
                                let bindings = fields
                                    .iter()
                                    .map(|field| rust_ident(&field.public_name))
                                    .collect::<Vec<_>>();
                                let lowered = fields
                                    .iter()
                                    .zip(&bindings)
                                    .map(|(field, ident)| lower_expr(&field.binding, quote!(#ident), flavor))
                                    .collect::<Result<Vec<_>>>()?;
                                Ok(quote!(
                                    #bridge_ident::#variant_ident { #(#bindings),* } => #core_path::#core_variant_ident ( #(#lowered),* )
                                ))
                            }
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lift_variants = variants
                    .iter()
                    .map(|variant| {
                        let variant_ident = rust_ident(&variant.public_name);
                        let core_variant_ident = rust_ident(&variant.rust_name);
                        match &variant.payload {
                            uniffi_js_engine_schema::RustVariantPayload::Unit => {
                                Ok::<_, anyhow::Error>(quote!(
                                    #core_path::#core_variant_ident => #bridge_ident::#variant_ident
                                ))
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Named(fields) => {
                                let bindings = fields
                                    .iter()
                                    .map(|field| rust_ident(&field.rust_name))
                                    .collect::<Vec<_>>();
                                let lifted = fields
                                    .iter()
                                    .zip(&bindings)
                                    .map(|(field, ident)| {
                                        let public_name = rust_ident(&field.public_name);
                                        let expression = lift_expr(&field.binding, quote!(#ident), flavor)?;
                                        Ok::<_, anyhow::Error>(quote!(#public_name: #expression))
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                Ok(quote!(
                                    #core_path::#core_variant_ident { #(#bindings),* } => #bridge_ident::#variant_ident { #(#lifted),* }
                                ))
                            }
                            uniffi_js_engine_schema::RustVariantPayload::Tuple(fields) => {
                                let bindings = fields
                                    .iter()
                                    .enumerate()
                                    .map(|(index, _)| rust_ident(&format!("field{index}")))
                                    .collect::<Vec<_>>();
                                let lifted = fields
                                    .iter()
                                    .zip(&bindings)
                                    .map(|(field, ident)| {
                                        let public_name = rust_ident(&field.public_name);
                                        let expression = lift_expr(&field.binding, quote!(#ident), flavor)?;
                                        Ok::<_, anyhow::Error>(quote!(#public_name: #expression))
                                    })
                                    .collect::<Result<Vec<_>>>()?;
                                Ok(quote!(
                                    #core_path::#core_variant_ident ( #(#bindings),* ) => #bridge_ident::#variant_ident { #(#lifted),* }
                                ))
                            }
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
                let lower_name = rust_ident(&format!("__uniffi_lower_type_{type_id}"));
                let lift_name = rust_ident(&format!("__uniffi_lift_type_{type_id}"));
                source.extend(quote! {
                    #enum_attribute
                    #[derive(Clone, Debug)]
                    pub enum #bridge_ident { #(#bridge_variants,)* }

                    fn #lower_name(value: #bridge_ident) -> ::std::result::Result<#core_path, #error_descriptor> {
                        Ok(match value { #(#lower_variants,)* })
                    }

                    fn #lift_name(value: #core_path) -> ::std::result::Result<#bridge_ident, #error_descriptor> {
                        Ok(match value { #(#lift_variants,)* })
                    }
                });
            }
            uniffi_js_engine_schema::RustNamedTypeKind::Custom {
                tag_path, inner, ..
            } => {
                let lower_name = rust_ident(&format!("__uniffi_lower_type_{type_id}"));
                let lift_name = rust_ident(&format!("__uniffi_lift_type_{type_id}"));
                let carrier = napi_carrier_type_for(inner, package.rust.named_types(), flavor)?;
                let custom_path = rust_path(&named.rust_path)?;
                let tag = rust_path(tag_path)?;
                let inner_core = syn_type(&inner.rust_type, flavor.prefix())?;
                let lower_builtin = lower_expr(inner, quote!(value), flavor)?;
                let lift_builtin = lift_expr(inner, quote!(value), flavor)?;
                source.extend(quote! {
                    fn #lower_name(value: #carrier) -> ::std::result::Result<#custom_path, #error_descriptor> {
                        let builtin: #inner_core = (|| -> ::std::result::Result<#inner_core, #error_descriptor> {
                            Ok(#lower_builtin)
                        })()?;
                        let ffi = <#inner_core as ::uniffi::Lower<#tag>>::lower(builtin);
                        <#custom_path as ::uniffi::Lift<#tag>>::try_lift(ffi)
                            .map_err(|error| #error_descriptor::validation(error.to_string()))
                    }

                    fn #lift_name(value: #custom_path) -> ::std::result::Result<#carrier, #error_descriptor> {
                        let ffi = <#custom_path as ::uniffi::Lower<#tag>>::lower(value);
                        <#inner_core as ::uniffi::Lift<#tag>>::try_lift(ffi)
                            .map_err(|error| #error_descriptor::validation(error.to_string()))
                            .and_then(|value| {
                                (|| -> ::std::result::Result<#carrier, #error_descriptor> {
                                    Ok(#lift_builtin)
                                })()
                            })
                    }
                });
            }
            uniffi_js_engine_schema::RustNamedTypeKind::Object { .. } => {
                let lower_name = rust_ident(&format!("__uniffi_lower_type_{type_id}"));
                let lift_name = rust_ident(&format!("__uniffi_lift_type_{type_id}"));
                let core = object_core_type(package, named.id)?;
                let carrier = napi_object_lease_type();
                source.extend(quote! {
                    fn #lower_name(value: #carrier) -> ::std::result::Result<#core, #error_descriptor> {
                        crate::__uniffi_take_object::<#core>(value.handle, false)
                            .map_err(|error| #error_descriptor::validation(error))
                    }

                    fn #lift_name(value: #core) -> ::std::result::Result<#carrier, #error_descriptor> {
                        Ok(#carrier {
                            handle: crate::__uniffi_store_object(value),
                            surface_id: "base".to_owned(),
                        })
                    }
                });
            }
            _ => {}
        }
    }
    Ok(source.to_string())
}

/// A package-level, in-memory lease table. Handles are deliberately opaque
/// engine carriers; no identity or checksum is persisted. The table is
/// emitted into the same generated host source as the operation trampolines,
/// so every object return and subsequent receiver/argument use shares one
/// generation-local registry.
fn render_resource_registry(flavor: NativeFlavor) -> String {
    let error_descriptor = flavor.error_descriptor();
    let mut source = TokenStream::new();
    source.extend(quote! {
        use std::any::Any;
        use std::collections::BTreeMap;
        use std::sync::{Mutex, OnceLock};

        // These are engine-private resource carriers.  The facade exposes
        // them only as opaque object leases and the session reads `handle`
        // when it needs to retain/release a resource.  No numeric handle is
        // published as the public return value.
        #[napi(object)]
        #[derive(Clone, Debug)]
        pub struct __UniffiNapiObjectLease {
            pub handle: u32,
            #[napi(js_name = "surfaceId")]
            pub surface_id: String,
        }

        #[napi(object)]
        #[derive(Clone, Debug)]
        pub struct __UniffiNapiOutputStreamLease {
            pub handle: u32,
        }

        static __UNIFFI_RESOURCE_REGISTRY: OnceLock<Mutex<BTreeMap<u32, Box<dyn Any + Send>>>> = OnceLock::new();
        static __UNIFFI_RESOURCE_NEXT: OnceLock<Mutex<u32>> = OnceLock::new();
        // Output streams live in per-use-site typed RustStreamRegistry values,
        // not in the generic object table above.  The session only carries a
        // u32 handle, so retain a private close function alongside each
        // handle and route both cancel/release through this one idempotent
        // dispatch table.
        static __UNIFFI_OUTPUT_STREAM_CLOSES: OnceLock<Mutex<BTreeMap<u32, fn(u32)>>> = OnceLock::new();

        fn __uniffi_output_stream_closes() -> &'static Mutex<BTreeMap<u32, fn(u32)>> {
            __UNIFFI_OUTPUT_STREAM_CLOSES.get_or_init(|| Mutex::new(BTreeMap::new()))
        }

        fn __uniffi_register_output_stream(handle: u32, close: fn(u32)) {
            __uniffi_output_stream_closes()
                .lock()
                .expect("UniFFI output stream close registry poisoned")
                .insert(handle, close);
        }

        fn __uniffi_close_output_stream_dispatch(handle: u32) -> ::std::result::Result<(), #error_descriptor> {
            let close = __uniffi_output_stream_closes()
                .lock()
                .map_err(|_| #error_descriptor::validation("UniFFI output stream close registry poisoned"))?
                .remove(&handle);
            if let Some(close) = close {
                close(handle);
            }
            Ok(())
        }

        fn __uniffi_resource_registry() -> &'static Mutex<BTreeMap<u32, Box<dyn Any + Send>>> {
            __UNIFFI_RESOURCE_REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
        }

        fn __uniffi_store_object<T: Any + Send + 'static>(value: T) -> u32 {
            let counter = __UNIFFI_RESOURCE_NEXT.get_or_init(|| Mutex::new(1));
            let mut next = counter.lock().expect("UniFFI resource counter poisoned");
            let handle = *next;
            *next = next.wrapping_add(1).max(1);
            __uniffi_resource_registry()
                .lock()
                .expect("UniFFI resource registry poisoned")
                .insert(handle, Box::new(value));
            handle
        }

        fn __uniffi_take_object<T: Any + Send + 'static>(handle: u32, owned: bool) -> ::std::result::Result<T, String>
        where
            T: Clone,
        {
            let mut registry = __uniffi_resource_registry()
                .lock()
                .map_err(|_| "UniFFI resource registry poisoned".to_owned())?;
            if owned {
                let value = registry
                    .remove(&handle)
                    .ok_or_else(|| "unknown UniFFI object handle".to_owned())?;
                value
                    .downcast::<T>()
                    .map(|value| *value)
                    .map_err(|_| "UniFFI object handle has an incompatible type".to_owned())
            } else {
                registry
                    .get(&handle)
                    .ok_or_else(|| "unknown UniFFI object handle".to_owned())?
                    .downcast_ref::<T>()
                    .cloned()
                    .ok_or_else(|| "UniFFI object handle has an incompatible type".to_owned())
            }
        }

        fn __uniffi_release_object_impl(handle: u32) -> ::std::result::Result<(), #error_descriptor> {
            __uniffi_resource_registry()
                .lock()
                .map_err(|_| #error_descriptor::validation("UniFFI resource registry poisoned"))?
                .remove(&handle)
                .map(|_| ())
                .ok_or_else(|| #error_descriptor::validation("unknown UniFFI object handle"))
        }

        async fn __uniffi_cancel_output_stream_impl(handle: u32) -> ::std::result::Result<(), #error_descriptor> {
            __uniffi_close_output_stream_dispatch(handle)
        }

        fn __uniffi_release_output_stream_impl(handle: u32) -> ::std::result::Result<(), #error_descriptor> {
            __uniffi_close_output_stream_dispatch(handle)
        }
    });
    source.to_string()
}

fn render_named_type_helpers(package: &NormalizedPackage) -> Result<String> {
    render_named_type_helpers_for(package, NativeFlavor::Node)
}

/// Emit the napi-rs carrier wrappers for UniFFI's temporal values.  The core
/// operation still receives `SystemTime`/`Duration`; only the engine-facing
/// carrier needs an implementation of napi's value conversion traits.
fn render_temporal_helpers(flavor: NativeFlavor) -> Result<String> {
    let napi: Path = syn::parse_str(match flavor {
        NativeFlavor::Node => "napi",
        #[cfg(feature = "ohos")]
        NativeFlavor::Ohos => "napi_ohos",
    })?;
    let error_descriptor = flavor.error_descriptor();
    let source = quote! {
        use #napi::bindgen_prelude::*;

        // napi-rs intentionally maps Rust HashMap values to plain JavaScript
        // objects.  UniFFI's public cross-engine contract uses a real Map, so
        // keep that distinction in an engine-private carrier and convert to
        // the core HashMap only at the typed operation boundary.
        struct __UniffiNapiMap<K, V>(::std::collections::HashMap<K, V>);

        impl<K, V> TypeName for __UniffiNapiMap<K, V> {
            fn type_name() -> &'static str { "Map" }
            fn value_type() -> ValueType { ValueType::Object }
        }

        impl<K, V> ValidateNapiValue for __UniffiNapiMap<K, V>
        where
            K: FromNapiValue + ::std::cmp::Eq + ::std::hash::Hash,
            V: FromNapiValue,
        {}

        impl<K, V> FromNapiValue for __UniffiNapiMap<K, V>
        where
            K: FromNapiValue + ::std::cmp::Eq + ::std::hash::Hash,
            V: FromNapiValue,
        {
            unsafe fn from_napi_value(
                env: #napi::sys::napi_env,
                napi_val: #napi::sys::napi_value,
            ) -> Result<Self> {
                let object = Object::from_raw(env, napi_val);
                let global = Env::from(env).get_global()?;
                let map_class: Function<'_, (), ()> =
                    global.get_named_property_unchecked("Map")?;
                let mut is_map = false;
                let status = #napi::sys::napi_instanceof(
                    env,
                    napi_val,
                    map_class.raw(),
                    &mut is_map,
                );
                if status != #napi::sys::Status::napi_ok {
                    return Err(Error::new(
                        Status::GenericFailure,
                        "failed to validate JavaScript Map",
                    ));
                }
                if !is_map {
                    return Err(Error::new(
                        Status::InvalidArg,
                        "expected a JavaScript Map",
                    ));
                }
                let entries: Function<'_, (), Object> = object.get_named_property("entries")?;
                let iterator = entries.apply(object, ())?;
                let next: Function<'_, (), Object> = iterator.get_named_property("next")?;
                let mut values = ::std::collections::HashMap::new();
                loop {
                    let step: Object = next.apply(iterator, ())?;
                    if step.get_named_property::<bool>("done")? {
                        break;
                    }
                    let (key, value): (K, V) =
                        step.get_named_property_unchecked("value")?;
                    values.insert(key, value);
                }
                Ok(Self(values))
            }
        }

        impl<K, V> ToNapiValue for __UniffiNapiMap<K, V>
        where
            K: ToNapiValue,
            V: ToNapiValue,
        {
            unsafe fn to_napi_value(
                raw_env: #napi::sys::napi_env,
                value: Self,
            ) -> Result<#napi::sys::napi_value> {
                let env = Env::from(raw_env);
                let global = env.get_global()?;
                let map_class = global
                    .get_named_property_unchecked::<Function<'_, Array, ()>>("Map")?;
                let entries = Array::from_vec(&env, value.0.into_iter().collect())?;
                let map = map_class.new_instance(entries)?;
                Ok(map.raw())
            }
        }

        impl<K, V> ::std::iter::IntoIterator for __UniffiNapiMap<K, V> {
            type Item = (K, V);
            type IntoIter = ::std::collections::hash_map::IntoIter<K, V>;

            fn into_iter(self) -> Self::IntoIter {
                self.0.into_iter()
            }
        }

        impl<K, V> ::std::iter::FromIterator<(K, V)> for __UniffiNapiMap<K, V>
        where
            K: ::std::cmp::Eq + ::std::hash::Hash,
        {
            fn from_iter<T>(iter: T) -> Self
            where
                T: IntoIterator<Item = (K, V)>,
            {
                Self(iter.into_iter().collect())
            }
        }

        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        struct __UniffiTimestamp(pub ::std::time::SystemTime);

        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        struct __UniffiDuration(pub ::std::time::Duration);

        impl TypeName for __UniffiTimestamp {
            fn type_name() -> &'static str { "Date" }
            fn value_type() -> ValueType { ValueType::Object }
        }

        impl ValidateNapiValue for __UniffiTimestamp {}

        impl FromNapiValue for __UniffiTimestamp {
            unsafe fn from_napi_value(
                env: #napi::sys::napi_env,
                napi_val: #napi::sys::napi_value,
            ) -> Result<Self> {
                let mut millis = 0.0;
                #napi::check_status!(unsafe {
                    #napi::sys::napi_get_date_value(env, napi_val, &mut millis)
                })?;
                if !millis.is_finite() || millis.abs() > 8.64e15 {
                    return Err(Error::new(Status::InvalidArg, "invalid Date"));
                }
                let millis = millis.trunc() as i64;
                let value = if millis >= 0 {
                    ::std::time::UNIX_EPOCH
                        .checked_add(::std::time::Duration::from_millis(millis as u64))
                        .ok_or_else(|| Error::new(Status::InvalidArg, "timestamp overflow"))?
                } else {
                    ::std::time::UNIX_EPOCH
                        .checked_sub(::std::time::Duration::from_millis((-millis) as u64))
                        .ok_or_else(|| Error::new(Status::InvalidArg, "timestamp overflow"))?
                };
                Ok(Self(value))
            }
        }

        impl ToNapiValue for __UniffiTimestamp {
            unsafe fn to_napi_value(
                env: #napi::sys::napi_env,
                value: Self,
            ) -> Result<#napi::sys::napi_value> {
                let millis = match value.0.duration_since(::std::time::UNIX_EPOCH) {
                    Ok(delta) => (delta.as_secs() as f64) * 1000.0
                        + (delta.subsec_nanos() as f64) / 1_000_000.0,
                    Err(error) => {
                        let delta = error.duration();
                        -((delta.as_secs() as f64) * 1000.0
                            + (delta.subsec_nanos() as f64) / 1_000_000.0)
                    }
                };
                if !millis.is_finite() || millis.abs() > 8.64e15 {
                    return Err(Error::new(Status::InvalidArg, "timestamp exceeds JS Date range"));
                }
                let mut output = std::ptr::null_mut();
                #napi::check_status!(unsafe {
                    #napi::sys::napi_create_date(env, millis, &mut output)
                })?;
                Ok(output)
            }
        }

        fn __uniffi_lift_timestamp(
            value: ::std::time::SystemTime,
        ) -> ::std::result::Result<__UniffiTimestamp, #error_descriptor> {
            let millis = match value.duration_since(::std::time::UNIX_EPOCH) {
                Ok(delta) => (delta.as_secs() as f64) * 1000.0
                    + (delta.subsec_nanos() as f64) / 1_000_000.0,
                Err(error) => {
                    let delta = error.duration();
                    -((delta.as_secs() as f64) * 1000.0
                        + (delta.subsec_nanos() as f64) / 1_000_000.0)
                }
            };
            if !millis.is_finite() || millis.abs() > 8.64e15 {
                return Err(#error_descriptor::validation("timestamp exceeds JS Date range"));
            }
            Ok(__UniffiTimestamp(value))
        }

        impl TypeName for __UniffiDuration {
            fn type_name() -> &'static str { "number" }
            fn value_type() -> ValueType { ValueType::Number }
        }

        impl ValidateNapiValue for __UniffiDuration {}

        impl FromNapiValue for __UniffiDuration {
            unsafe fn from_napi_value(
                env: #napi::sys::napi_env,
                napi_val: #napi::sys::napi_value,
            ) -> Result<Self> {
                let millis = f64::from_napi_value(env, napi_val)?;
                if !millis.is_finite() || millis < 0.0 {
                    return Err(Error::new(
                        Status::InvalidArg,
                        "duration must be a finite non-negative number",
                    ));
                }
                let seconds = (millis / 1000.0).trunc();
                if seconds > u64::MAX as f64 {
                    return Err(Error::new(Status::InvalidArg, "duration exceeds Rust range"));
                }
                let mut seconds = seconds as u64;
                let mut nanos = ((millis % 1000.0) * 1_000_000.0).round() as u32;
                if nanos == 1_000_000_000 {
                    nanos = 0;
                    seconds = seconds
                        .checked_add(1)
                        .ok_or_else(|| Error::new(Status::InvalidArg, "duration exceeds Rust range"))?;
                }
                Ok(Self(::std::time::Duration::new(seconds, nanos)))
            }
        }

        impl ToNapiValue for __UniffiDuration {
            unsafe fn to_napi_value(
                env: #napi::sys::napi_env,
                value: Self,
            ) -> Result<#napi::sys::napi_value> {
                let millis = (value.0.as_secs() as f64) * 1000.0
                    + (value.0.subsec_nanos() as f64) / 1_000_000.0;
                if !millis.is_finite() || millis > 9_007_199_254_740_991.0 {
                    return Err(Error::new(Status::InvalidArg, "duration exceeds JS number range"));
                }
                f64::to_napi_value(env, millis)
            }
        }
    };
    Ok(source.to_string())
}

fn napi_crate_path(flavor: NativeFlavor) -> Path {
    syn::parse_str(match flavor {
        NativeFlavor::Node => "napi",
        #[cfg(feature = "ohos")]
        NativeFlavor::Ohos => "napi_ohos",
    })
    .expect("napi crate path is valid")
}

fn engine_crate_path(flavor: NativeFlavor) -> Path {
    syn::parse_str(match flavor {
        NativeFlavor::Node => "napi_uniffi_engine",
        #[cfg(feature = "ohos")]
        NativeFlavor::Ohos => "napi_ohos_uniffi_engine",
    })
    .expect("napi engine crate path is valid")
}

/// Render callback trait proxies for native hosts.  The proxy is deliberately
/// built around the session Host SPI rather than a second callback registry:
/// the engine supplies the session invoker (and, for retained sites, the
/// lease), while the Host owns dispatch, reentrancy and typed callback result
/// envelopes.  Sync methods use a FunctionRef; async methods use a
/// ThreadsafeFunction so the JavaScript call always runs on the owning VM
/// thread even when Rust invokes the trait from an async worker.
fn render_callback_proxy_helpers(
    package: &NormalizedPackage,
    flavor: NativeFlavor,
    engine: EngineKind,
) -> Result<String> {
    let operations = &package.rust.engines[&engine].operations;
    // A proxy can be introduced by a direct callback argument or by a
    // callback-valued return from another callback method.  Return proxies do
    // not receive a transfer lease: the JS runtime registered the returned
    // callback under the return contract and the surrounding session owns
    // its eventual retention.  Argument proxies retain the engine-provided
    // lease for retained contracts.
    let mut uses = Vec::new();
    // The bridge plan is the canonical source of callback use-sites.  A
    // WithForeign trait projects one source callback contract onto both its
    // object and callback legs, and a callback-valued return can therefore be
    // encountered more than once while walking the Rust operation list.  Do
    // not render the same proxy/helper twice: the operation/path/type/contract
    // tuple is the complete in-memory use-site key.
    let mut seen_use_sites = BTreeSet::new();
    for operation in operations {
        for (index, binding) in operation.arguments.iter().enumerate() {
            let ConversionRecipe::Callback(callback_type) = binding.conversion else {
                continue;
            };
            let contract = package
                .bridge
                .callbacks()
                .iter()
                .find(|callback| {
                    callback.operation_id == operation.operation_id
                        && callback.path.segments() == [ValuePathSegment::Argument(index as u32)]
                })
                .ok_or_else(|| {
                    anyhow!(
                        "missing callback contract for operation {} argument {}",
                        operation.operation_id.index(),
                        index
                    )
                })?;
            let key = (
                operation.operation_id.index(),
                contract.path.clone(),
                callback_type.index(),
                contract.contract,
            );
            if seen_use_sites.insert(key) {
                uses.push((
                    operation,
                    index.to_string(),
                    callback_type,
                    contract.contract,
                    true,
                    false,
                ));
            }
        }
        if matches!(
            &operation.call_target,
            RustCallTarget::CallbackMethod { .. }
        ) {
            if let Some(binding) = &operation.return_value {
                if let ConversionRecipe::Callback(callback_type) = binding.conversion {
                    let contract = package
                        .bridge
                        .callbacks()
                        .iter()
                        .find(|callback| {
                            callback.operation_id == operation.operation_id
                                && callback.path.segments() == [ValuePathSegment::Return]
                        })
                        .ok_or_else(|| {
                            anyhow!(
                                "missing callback contract for callback method return {}",
                                operation.operation_id.index()
                            )
                        })?;
                    let method_id = match &operation.call_target {
                        RustCallTarget::CallbackMethod { method_id, .. } => method_id,
                        _ => unreachable!(),
                    };
                    let key = (
                        operation.operation_id.index(),
                        contract.path.clone(),
                        callback_type.index(),
                        contract.contract,
                    );
                    if seen_use_sites.insert(key) {
                        uses.push((
                            operation,
                            format!("return_{method_id}"),
                            callback_type,
                            contract.contract,
                            false,
                            true,
                        ));
                    }
                }
            }
        }
    }
    if uses.is_empty() {
        return Ok(String::new());
    }

    let napi = napi_crate_path(flavor);
    let error_descriptor = flavor.error_descriptor();
    let mut source = TokenStream::new();

    for (operation, use_label, callback_type, contract, needs_lease, _is_return) in uses {
        let named = package
            .rust
            .named_type(callback_type)
            .ok_or_else(|| anyhow!("missing callback type {}", callback_type.index()))?;
        let callback_path = rust_path(&named.rust_path)?;
        let helper_name = ident_for_helper(operation.operation_id.index(), &use_label, "callback");
        let inner_name = rust_ident(&format!(
            "__UniffiCallbackInner{}_{}",
            operation.operation_id.index(),
            use_label
        ));
        let proxy_name = rust_ident(&format!(
            "__UniffiCallbackProxy{}_{}",
            operation.operation_id.index(),
            use_label
        ));
        let callback_array_helper_name = rust_ident(&format!(
            "__uniffi_callback_array_{}_{}",
            operation.operation_id.index(),
            use_label
        ));
        let engine_crate = engine_crate_path(flavor);
        let contract_type: Path = syn::parse_quote!(#engine_crate::SessionCallbackArgument);
        let lease_type: Path = syn::parse_quote!(#engine_crate::SessionCallbackLease);
        // The public callback builder signature is owned by the forked
        // engine.  Keep the helper's argument order identical for scoped and
        // retained use-sites; retained sites additionally carry the lease.
        let invoker_type: Path = syn::parse_quote!(#engine_crate::SessionCallbackInvoker);
        let lease_field = if needs_lease
            && contract.retention == uniffi_js_engine_schema::CallbackRetention::Retained
        {
            quote! { _lease: #lease_type, }
        } else {
            quote! {}
        };
        let lease_init = if needs_lease
            && contract.retention == uniffi_js_engine_schema::CallbackRetention::Retained
        {
            quote! { _lease: lease, }
        } else {
            quote! {}
        };

        let callback_methods = operations
            .iter()
            .filter(|candidate| {
                matches!(
                    candidate.call_target,
                    RustCallTarget::CallbackMethod {
                        callback_type: method_callback_type,
                        ..
                    } if method_callback_type == callback_type
                )
            })
            .collect::<Vec<_>>();
        if callback_methods.is_empty() {
            bail!(
                "callback type {} has no callback methods",
                callback_type.index()
            );
        }

        let mut result_structs = Vec::new();
        let mut inner_fields = Vec::new();
        let mut inner_init = Vec::new();
        let mut impl_methods = Vec::new();

        for callback_operation in callback_methods {
            let RustCallTarget::CallbackMethod { method_id, .. } = callback_operation.call_target
            else {
                unreachable!()
            };
            let result_name = rust_ident(&format!(
                "__UniffiCallbackResult{}_{}_{}",
                operation.operation_id.index(),
                use_label,
                method_id
            ));
            let payload_name = rust_ident(&format!(
                "__UniffiCallbackPayload{}_{}_{}",
                operation.operation_id.index(),
                use_label,
                method_id
            ));
            // The JS host receives the argument list as its final array
            // parameter.  Keep that array as a raw value built by the
            // `JsValuesTupleIntoVec` implementation on the owning JS
            // thread; carrying an `Array<'static>` in the proxy's async
            // future makes async-trait require an impossible universal
            // `JsValue` lifetime.
            let host_sync_args = quote!(#napi::bindgen_prelude::FnArgs<(u32, u32, u32, #napi::bindgen_prelude::Array<'static>)>);
            let host_async_args = quote!(#napi::bindgen_prelude::FnArgs<(u32, u32, u32, u32, #napi::bindgen_prelude::Array<'static>)>);
            let cross_thread =
                contract.threading == uniffi_js_engine_schema::CallbackThreading::MayCrossThread;
            let sync_cross_field = rust_ident(&format!("sync_cross_{method_id}"));
            let async_field = rust_ident(&format!("async_{method_id}"));
            let sync_call_args_name = rust_ident(&format!(
                "__UniffiCallbackSyncArgs{}_{}_{}",
                operation.operation_id.index(),
                use_label,
                method_id
            ));
            let async_call_args_name = rust_ident(&format!(
                "__UniffiCallbackAsyncArgs{}_{}_{}",
                operation.operation_id.index(),
                use_label,
                method_id
            ));

            let method_ident = match &callback_operation.call_target {
                RustCallTarget::CallbackMethod { item, .. } => rust_ident(item),
                _ => unreachable!(),
            };
            let arg_declarations = callback_operation
                .arguments
                .iter()
                .map(|binding| {
                    let ident = rust_ident(&binding.rust_name);
                    let ty = core_type_for_binding(
                        package,
                        &RustValueBinding {
                            rust_type: binding.rust_type.clone(),
                            carrier: binding.carrier,
                            conversion: binding.conversion.clone(),
                        },
                    )?;
                    Ok::<_, anyhow::Error>(quote!(#ident: #ty))
                })
                .collect::<Result<Vec<_>>>()?;
            let return_type = callback_operation
                .return_value
                .as_ref()
                .map(|binding| {
                    core_type_for_binding(
                        package,
                        &RustValueBinding {
                            rust_type: binding.rust_type.clone(),
                            carrier: binding.carrier,
                            conversion: binding.conversion.clone(),
                        },
                    )
                })
                .transpose()?;
            let return_core_type = return_type.clone().unwrap_or_else(|| syn::parse_quote!(()));
            let return_carrier = if let Some(binding) = &callback_operation.return_value {
                napi_carrier_type_for(
                    &RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    },
                    package.rust.named_types(),
                    flavor,
                )?
            } else {
                syn::parse_quote!(())
            };
            let error_carrier: Type = if let Some(throws) = callback_operation.throws {
                package
                    .rust
                    .named_type(throws)
                    .ok_or_else(|| anyhow!("missing callback error {}", throws.index()))?;
                let error_bridge = rust_ident(&format!("__UniffiNapiType{}", throws.index()));
                syn::parse_quote!(#error_bridge)
            } else {
                syn::parse_quote!(())
            };
            let payload_fields = callback_operation
                .arguments
                .iter()
                .map(|binding| {
                    let ident = rust_ident(&binding.rust_name);
                    let ty = core_type_for_binding(
                        package,
                        &RustValueBinding {
                            rust_type: binding.rust_type.clone(),
                            carrier: binding.carrier,
                            conversion: binding.conversion.clone(),
                        },
                    )?;
                    Ok::<_, anyhow::Error>(quote!(#ident: #ty))
                })
                .collect::<Result<Vec<_>>>()?;
            let payload_values = callback_operation
                .arguments
                .iter()
                .map(|binding| {
                    let ident = rust_ident(&binding.rust_name);
                    quote!(#ident: #ident)
                })
                .collect::<Vec<_>>();
            let callback_array_helper = if result_structs.is_empty() {
                quote! {
                    fn #callback_array_helper_name(
                        env: #napi::sys::napi_env,
                        values: ::std::vec::Vec<#napi::sys::napi_value>,
                    ) -> #napi::bindgen_prelude::Result<#napi::sys::napi_value> {
                        let mut array = ::std::ptr::null_mut();
                        #napi::bindgen_prelude::check_status!(
                            unsafe {
                                #napi::sys::napi_create_array_with_length(
                                    env,
                                    values.len(),
                                    &mut array,
                                )
                            },
                            "failed to create callback argument array",
                        )?;
                        for (index, value) in values.into_iter().enumerate() {
                            #napi::bindgen_prelude::check_status!(
                                unsafe {
                                    #napi::sys::napi_set_element(
                                        env,
                                        array,
                                        index as u32,
                                        value,
                                    )
                                },
                                "failed to set callback argument array element",
                            )?;
                        }
                        Ok(array)
                    }
                }
            } else {
                quote! {}
            };
            result_structs.push(quote! {
                #callback_array_helper
                #[napi(object)]
                pub struct #result_name {
                    pub ok: bool,
                    pub value: Option<#return_carrier>,
                    pub error: Option<#error_carrier>,
                    pub error_message: Option<String>,
                }
                struct #payload_name {
                    callback_type: u32,
                    callback_id: u32,
                    method_id: u32,
                    invocation_id: u32,
                    #(#payload_fields),*
                }
                struct #sync_call_args_name {
                    callback_type: u32,
                    callback_id: u32,
                    method_id: u32,
                    values: ::std::vec::Vec<#napi::sys::napi_value>,
                }
                unsafe impl Send for #sync_call_args_name {}
                unsafe impl Sync for #sync_call_args_name {}
                impl #napi::bindgen_prelude::JsValuesTupleIntoVec for #sync_call_args_name {
                    fn into_vec(self, env: #napi::sys::napi_env) -> #napi::bindgen_prelude::Result<::std::vec::Vec<#napi::sys::napi_value>> {
                        let mut values = ::std::vec::Vec::with_capacity(4);
                        values.push(unsafe { <u32 as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(env, self.callback_type)? });
                        values.push(unsafe { <u32 as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(env, self.callback_id)? });
                        values.push(unsafe { <u32 as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(env, self.method_id)? });
                        values.push(#callback_array_helper_name(env, self.values)?);
                        Ok(values)
                    }
                }
                struct #async_call_args_name {
                    callback_type: u32,
                    callback_id: u32,
                    method_id: u32,
                    invocation_id: u32,
                    values: ::std::vec::Vec<#napi::sys::napi_value>,
                }
                unsafe impl Send for #async_call_args_name {}
                unsafe impl Sync for #async_call_args_name {}
                impl #napi::bindgen_prelude::JsValuesTupleIntoVec for #async_call_args_name {
                    fn into_vec(self, env: #napi::sys::napi_env) -> #napi::bindgen_prelude::Result<::std::vec::Vec<#napi::sys::napi_value>> {
                        let mut values = ::std::vec::Vec::with_capacity(5);
                        values.push(unsafe { <u32 as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(env, self.callback_type)? });
                        values.push(unsafe { <u32 as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(env, self.callback_id)? });
                        values.push(unsafe { <u32 as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(env, self.method_id)? });
                        values.push(unsafe { <u32 as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(env, self.invocation_id)? });
                        values.push(#callback_array_helper_name(env, self.values)?);
                        Ok(values)
                    }
                }
            });
            let async_result_type = if callback_operation
                .return_value
                .as_ref()
                .is_some_and(|binding| matches!(binding.conversion, ConversionRecipe::Callback(_)))
            {
                // Callback-return async methods retain/lower the returned
                // callback from the owning JavaScript thread.  PromiseRaw is
                // deliberately used here so the worker future never tries
                // to lift a JS value or hold an Env-bound Promise.
                quote!(#napi::bindgen_prelude::PromiseRaw<'static, #result_name>)
            } else {
                quote!(#napi::bindgen_prelude::Promise<#result_name>)
            };
            let sync_cross_field_decl = if cross_thread {
                quote! {
                    #sync_cross_field: #napi::threadsafe_function::ThreadsafeFunction<
                        #payload_name,
                        #result_name,
                        #sync_call_args_name,
                        #napi::Status,
                        false,
                    >,
                }
            } else {
                quote! {}
            };
            inner_fields.push(quote! {
                #sync_cross_field_decl
                #async_field: #napi::threadsafe_function::ThreadsafeFunction<
                    #payload_name,
                    #async_result_type,
                    #async_call_args_name,
                    #napi::Status,
                    false,
                >,
            });
            let async_arg_lifts = callback_operation
                .arguments
                .iter()
                .map(|binding| {
                    let ident = rust_ident(&binding.rust_name);
                    let value_binding = RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    };
                    let lifted = lift_expr(&value_binding, quote!(context.value.#ident), flavor)?;
                    Ok::<_, anyhow::Error>(quote! {
                        let __uniffi_value = (|| -> ::std::result::Result<_, #error_descriptor> {
                            Ok(#lifted)
                        })()
                            .map_err(|error| #napi::Error::new(
                                #napi::Status::GenericFailure,
                                format!("callback argument lowering failed: {:?}", error),
                            ))?;
                        let __uniffi_raw = unsafe {
                            <_ as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(
                                context.env.raw(),
                                __uniffi_value,
                            )
                        }?;
                        __uniffi_args.push(__uniffi_raw);
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let sync_cross_init = if cross_thread {
                quote! {
                    #sync_cross_field: host
                        .get_named_property::<#napi::bindgen_prelude::Function<#host_sync_args, #result_name>>("invokeCallbackSyncResult")
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?
                        // `invokeCallbackSyncResult` is a Host instance
                        // method.  Threadsafe functions retain the function
                        // value but do not retain its receiver, so bind the
                        // host before handing the function to N-API.
                        .bind(host)
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?
                        .build_threadsafe_function::<#payload_name>()
                        .callee_handled::<false>()
                        .build_callback(|context| {
                            let mut __uniffi_args = ::std::vec::Vec::<#napi::sys::napi_value>::new();
                            #(#async_arg_lifts)*
                            Ok(#sync_call_args_name {
                                callback_type: context.value.callback_type,
                                callback_id: context.value.callback_id,
                                method_id: context.value.method_id,
                                values: __uniffi_args,
                            })
                        })
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?,
                }
            } else {
                quote! {}
            };
            inner_init.push(quote! {
                #sync_cross_init
                #async_field: host
                    .get_named_property::<#napi::bindgen_prelude::Function<#host_async_args, #async_result_type>>("invokeCallbackAsyncResult")
                    .map_err(|error| #error_descriptor::backend(error.to_string()))?
                    // See the sync cross-thread path above: an unbound
                    // Host method loses `this` when N-API invokes it.
                    .bind(host)
                    .map_err(|error| #error_descriptor::backend(error.to_string()))?
                    .build_threadsafe_function::<#payload_name>()
                    .callee_handled::<false>()
                    .build_callback(|context| {
                        let mut __uniffi_args = ::std::vec::Vec::<#napi::sys::napi_value>::new();
                        #(#async_arg_lifts)*
                        Ok(#async_call_args_name {
                            callback_type: context.value.callback_type,
                            callback_id: context.value.callback_id,
                            method_id: context.value.method_id,
                            invocation_id: context.value.invocation_id,
                            values: __uniffi_args,
                        })
                    })
                    .map_err(|error| #error_descriptor::backend(error.to_string()))?,
            });
            let sync_arg_lifts = callback_operation
                .arguments
                .iter()
                .map(|binding| {
                    let ident = rust_ident(&binding.rust_name);
                    let value_binding = RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    };
                    let lifted = lift_expr(&value_binding, quote!(#ident), flavor)?;
                    Ok::<_, anyhow::Error>(quote! {
                        let __uniffi_value = (|| -> ::std::result::Result<_, #error_descriptor> {
                            Ok(#lifted)
                        })()
                            .unwrap_or_else(|error| panic!("callback argument lowering failed: {:?}", error));
                        let __uniffi_raw = unsafe {
                            <_ as #napi::bindgen_prelude::ToNapiValue>::to_napi_value(
                                __uniffi_env.raw(),
                                __uniffi_value,
                            )
                        }
                        .unwrap_or_else(|error| panic!("callback argument lowering failed: {}", error));
                        __uniffi_args.push(__uniffi_raw);
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let result_return = if let Some(throws) = callback_operation.throws {
                let named = package
                    .rust
                    .named_type(throws)
                    .ok_or_else(|| anyhow!("missing callback error {}", throws.index()))?;
                let error_path = rust_path(&named.rust_path)?;
                syn::parse_quote!(::std::result::Result<#return_core_type, #error_path>)
            } else {
                return_core_type.clone()
            };
            // Keep the callback error path available to both the synchronous
            // result parser and the owning-JS-thread async callback-return
            // continuation below.  The continuation must never panic while
            // converting a rejected/ill-formed callback result: it reports a
            // string error to the Rust future instead.
            let error_path = if let Some(throws) = callback_operation.throws {
                let named = package
                    .rust
                    .named_type(throws)
                    .ok_or_else(|| anyhow!("missing callback error {}", throws.index()))?;
                rust_path(&named.rust_path)?
            } else {
                // The no-throws branch never interpolates this path, but it
                // still needs a syntactically valid type path for the shared
                // quote construction below.
                syn::parse_quote!(::std::convert::Infallible)
            };
            let error_lower = callback_operation
                .throws
                .map(|throws| path_for_type_helper(throws.index(), "lower"));
            let callback_return_details = if let Some(binding) = &callback_operation.return_value {
                if let ConversionRecipe::Callback(callback_type) = binding.conversion {
                    let callback = package
                        .bridge
                        .callbacks()
                        .iter()
                        .find(|site| {
                            site.operation_id == callback_operation.operation_id
                                && site.path.segments() == [ValuePathSegment::Return]
                                && site.callback_type == callback_type
                        })
                        .ok_or_else(|| {
                            anyhow!(
                                "missing callback contract for callback method return {}",
                                method_id
                            )
                        })?;
                    let return_helper = ident_for_helper(
                        callback_operation.operation_id.index(),
                        &format!("return_{method_id}"),
                        "callback",
                    );
                    let engine_crate = engine_crate_path(flavor);
                    let retention = match callback.contract.retention {
                        uniffi_js_engine_schema::CallbackRetention::Scoped => {
                            quote!(#engine_crate::SessionCallbackRetention::Scoped)
                        }
                        uniffi_js_engine_schema::CallbackRetention::Retained => {
                            quote!(#engine_crate::SessionCallbackRetention::Retained)
                        }
                    };
                    let threading = match callback.contract.threading {
                        uniffi_js_engine_schema::CallbackThreading::CallingThread => {
                            quote!(#engine_crate::SessionCallbackThreading::CallingThread)
                        }
                        uniffi_js_engine_schema::CallbackThreading::MayCrossThread => {
                            quote!(#engine_crate::SessionCallbackThreading::MayCrossThread)
                        }
                    };
                    let reentrancy = match callback.contract.reentrancy {
                        uniffi_js_engine_schema::CallbackReentrancy::Forbidden => {
                            quote!(#engine_crate::SessionCallbackReentrancy::Forbidden)
                        }
                        uniffi_js_engine_schema::CallbackReentrancy::Allowed => {
                            quote!(#engine_crate::SessionCallbackReentrancy::Allowed)
                        }
                    };
                    let returned_callback_type_id = callback_type.index();
                    let contract_expr = quote! {
                        #engine_crate::SessionCallbackArgument {
                            path: vec![#engine_crate::SessionValuePathSegment::Return],
                            callback_type_id: #returned_callback_type_id,
                            retention: #retention,
                            threading: #threading,
                            reentrancy: #reentrancy,
                        }
                    };
                    Some((
                        return_helper,
                        callback_type,
                        returned_callback_type_id,
                        contract_expr,
                    ))
                } else {
                    None
                }
            } else {
                None
            };
            let parse_success = if let Some(binding) = &callback_operation.return_value {
                let value_binding = RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                };
                if let Some((
                    return_helper,
                    _callback_type,
                    returned_callback_type_id,
                    contract_expr,
                )) = &callback_return_details
                {
                    quote! {
                        {
                            let __uniffi_callback_id = __uniffi_result.value.unwrap_or_else(|| panic!("callback method {} returned ok without a callback id", #method_id));
                            let __uniffi_proxy = self
                                .__uniffi_inner
                                .invoker
                                .with_host(|_env, __uniffi_host| {
                                    #return_helper(
                                        __uniffi_host,
                                        #returned_callback_type_id,
                                        __uniffi_callback_id,
                                        #contract_expr,
                                        self.__uniffi_inner.invoker.clone(),
                                    )
                                })
                                .unwrap_or_else(|error| panic!("callback return retain check failed: {}", error))
                                .unwrap_or_else(|error| panic!("callback return proxy construction failed: {:?}", error));
                            __uniffi_proxy
                        }
                    }
                } else {
                    let lower = lower_expr(&value_binding, quote!(__uniffi_carrier), flavor)?;
                    quote! {
                        {
                            let __uniffi_carrier = __uniffi_result.value.unwrap_or_else(|| panic!("callback method {} returned ok without a value", #method_id));
                            (|| -> ::std::result::Result<#return_core_type, #error_descriptor> {
                                Ok(#lower)
                            })()
                            .unwrap_or_else(|error| panic!("callback method {} result lowering failed: {:?}", #method_id, error))
                        }
                    }
                }
            } else {
                quote!(())
            };
            let parse_error = if let Some(throws) = callback_operation.throws {
                let named = package
                    .rust
                    .named_type(throws)
                    .ok_or_else(|| anyhow!("missing callback error {}", throws.index()))?;
                let error_bridge = rust_ident(&format!("__UniffiNapiType{}", throws.index()));
                let error_path = rust_path(&named.rust_path)?;
                quote! {
                    {
                        let __uniffi_error: #error_bridge = __uniffi_result.error.unwrap_or_else(|| panic!("callback method {} returned err without an error", #method_id));
                        let __uniffi_error: #error_path = #error_lower(__uniffi_error)
                            .unwrap_or_else(|error| panic!("callback method {} error lowering failed: {:?}", #method_id, error));
                        Err(__uniffi_error)
                    }
                }
            } else {
                quote! {
                    panic!(
                        "callback method {} returned an error: {}",
                        #method_id,
                        __uniffi_result.error_message.unwrap_or_default(),
                    );
                }
            };
            let success_branch = if callback_operation.throws.is_some() {
                quote!(Ok(#parse_success))
            } else {
                quote!(#parse_success)
            };
            let sync_call = quote! {
                self.__uniffi_inner
                    .invoker
                    .check_open()
                    .unwrap_or_else(|error| panic!("callback invoker is closed: {}", error));
                let __uniffi_result = __uniffi_host
                    .get_named_property::<#napi::bindgen_prelude::Function<#sync_call_args_name, #result_name>>("invokeCallbackSyncResult")
                    .unwrap_or_else(|error| panic!("callback sync host function unavailable: {}", error))
                    .apply(__uniffi_host, #sync_call_args_name {
                        callback_type: self.__uniffi_inner.callback_type,
                        callback_id: self.__uniffi_inner.callback_id,
                        method_id: #method_id,
                        values: __uniffi_args,
                    })
                    .unwrap_or_else(|error| panic!("callback sync host call failed: {}", error));
            };
            let (return_helper, returned_callback_type_id, contract_expr) =
                match &callback_return_details {
                    Some((helper, _callback_type, type_id, contract)) => {
                        (Some(helper), Some(type_id), Some(contract))
                    }
                    None => (None, None, None),
                };
            let cross_sync_call = if cross_thread {
                let callback_result = if callback_return_details.is_some() {
                    if callback_operation.throws.is_some() {
                        quote! {
                            if !__uniffi_js_result.ok {
                                let __uniffi_error = __uniffi_js_result
                                    .error
                                    .ok_or_else(|| format!("callback method {} returned err without an error", #method_id))?;
                                let __uniffi_error: #error_path = #error_lower(__uniffi_error)
                                    .map_err(|error| format!("{error:?}"))?;
                                Ok(::std::result::Result::Err(__uniffi_error))
                            } else {
                                let __uniffi_callback_id = __uniffi_js_result
                                    .value
                                    .ok_or_else(|| format!("callback method {} returned ok without a callback id", #method_id))?;
                                let __uniffi_proxy = match __uniffi_inner.invoker.with_host(|_env, __uniffi_host| {
                                    #return_helper(
                                        __uniffi_host,
                                        #returned_callback_type_id,
                                        __uniffi_callback_id,
                                        #contract_expr,
                                        __uniffi_inner.invoker.clone(),
                                    )
                                    .map_err(|error| format!("callback return proxy construction failed: {:?}", error))
                                }) {
                                    Ok(Ok(value)) => value,
                                    Ok(Err(error)) => return Err(error),
                                    Err(error) => return Err(format!("callback return host unavailable: {}", error)),
                                };
                                Ok(::std::result::Result::Ok(__uniffi_proxy))
                            }
                        }
                    } else {
                        quote! {
                            if !__uniffi_js_result.ok {
                                return Err(format!(
                                    "callback method {} returned an error: {}",
                                    #method_id,
                                    __uniffi_js_result.error_message.unwrap_or_default(),
                                ));
                            }
                            let __uniffi_callback_id = __uniffi_js_result
                                .value
                                .ok_or_else(|| format!("callback method {} returned ok without a callback id", #method_id))?;
                            let __uniffi_proxy = match __uniffi_inner.invoker.with_host(|_env, __uniffi_host| {
                                #return_helper(
                                    __uniffi_host,
                                    #returned_callback_type_id,
                                    __uniffi_callback_id,
                                    #contract_expr,
                                    __uniffi_inner.invoker.clone(),
                                )
                                .map_err(|error| format!("callback return proxy construction failed: {:?}", error))
                            }) {
                                Ok(Ok(value)) => value,
                                Ok(Err(error)) => return Err(error),
                                Err(error) => return Err(format!("callback return host unavailable: {}", error)),
                            };
                            Ok(__uniffi_proxy)
                        }
                    }
                } else {
                    quote! {
                        if __uniffi_js_result.ok {
                            Ok(__uniffi_js_result)
                        } else {
                            Err(format!(
                                "callback method {} returned an error: {}",
                                #method_id,
                                __uniffi_js_result.error_message.unwrap_or_default(),
                            ))
                        }
                    }
                };
                let channel_result = if callback_return_details.is_some() {
                    quote!(::std::result::Result<#result_return, ::std::string::String>)
                } else {
                    quote!(::std::result::Result<#result_name, ::std::string::String>)
                };
                let receive_result = quote! {
                    let __uniffi_owned_result = __uniffi_receiver
                        .recv()
                        .unwrap_or_else(|_| panic!("callback cross-thread result channel closed"))
                        .unwrap_or_else(|__uniffi_error| panic!("{}", __uniffi_error));
                };
                quote! {
                    let __uniffi_inner = self.__uniffi_inner.clone();
                    let (__uniffi_sender, __uniffi_receiver) =
                        ::std::sync::mpsc::sync_channel::<#channel_result>(1);
                    let __uniffi_sender = ::std::sync::Arc::new(::std::sync::Mutex::new(Some(__uniffi_sender)));
                    let __uniffi_sender_callback = __uniffi_sender.clone();
                    let __uniffi_status = self
                        .__uniffi_inner
                        .#sync_cross_field
                        .call_with_return_value(
                            #payload_name {
                                callback_type: self.__uniffi_inner.callback_type,
                                callback_id: self.__uniffi_inner.callback_id,
                                method_id: #method_id,
                                invocation_id: 0,
                                #(#payload_values),*
                            },
                            #napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
                            move |__uniffi_result, _env| {
                                let __uniffi_value = match __uniffi_result {
                                    Ok(__uniffi_js_result) => {
                                        let __uniffi_js_result = __uniffi_js_result;
                                        (|| { #callback_result })()
                                    }
                                    Err(__uniffi_error) => Err(format!("callback sync host call failed: {}", __uniffi_error)),
                                };
                                let mut __uniffi_guard = match __uniffi_sender_callback.lock() {
                                    Ok(__uniffi_guard) => __uniffi_guard,
                                    Err(__uniffi_poisoned) => __uniffi_poisoned.into_inner(),
                                };
                                if let Some(__uniffi_sender) = __uniffi_guard.take() {
                                    let _ = __uniffi_sender.send(__uniffi_value);
                                }
                                Ok(())
                            },
                        );
                    if __uniffi_status != #napi::Status::Ok {
                        let mut __uniffi_guard = match __uniffi_sender.lock() {
                            Ok(__uniffi_guard) => __uniffi_guard,
                            Err(__uniffi_poisoned) => __uniffi_poisoned.into_inner(),
                        };
                        if let Some(__uniffi_sender) = __uniffi_guard.take() {
                            let _ = __uniffi_sender.send(Err(format!("callback sync host call failed with status {:?}", __uniffi_status)));
                        }
                    }
                    #receive_result
                    __uniffi_owned_result
                }
            } else {
                quote! {}
            };
            let async_call = quote! {
                let __uniffi_invocation_id = self.__uniffi_inner.invoker.next_invocation_id().unwrap_or_else(|error| panic!("callback invocation allocation failed: {}", error));
                let __uniffi_promise = self
                    .__uniffi_inner
                    .#async_field
                    .call_async(#payload_name {
                        callback_type: self.__uniffi_inner.callback_type,
                        callback_id: self.__uniffi_inner.callback_id,
                        method_id: #method_id,
                        invocation_id: __uniffi_invocation_id,
                        #(#payload_values),*
                    })
                    .await
                    .unwrap_or_else(|error| panic!("callback async host call failed: {}", error));
                let __uniffi_result = __uniffi_promise.await.unwrap_or_else(|error| panic!("callback async result failed: {}", error));
            };
            let async_return_call = if let Some((
                return_helper,
                _callback_type,
                returned_callback_type_id,
                contract_expr,
            )) = &callback_return_details
            {
                // All conversion, retention and proxy construction happens in
                // the Promise continuation on the owning JS thread.  The
                // continuation returns `Result<(), napi::Error>` only for
                // PromiseRaw's bookkeeping; semantic failures are sent to the
                // Rust future and never panic through the JS callback.
                let callback_value = if callback_operation.throws.is_some() {
                    quote! {
                        (|| -> ::std::result::Result<#result_return, ::std::string::String> {
                            if !__uniffi_result.ok {
                                let __uniffi_error = __uniffi_result
                                    .error
                                    .ok_or_else(|| format!("callback method {} returned err without an error", #method_id))?;
                                let __uniffi_error: #error_path = #error_lower(__uniffi_error)
                                    .map_err(|error| format!("{error:?}"))?;
                                return Ok(::std::result::Result::Err(__uniffi_error));
                            }
                            let __uniffi_callback_id = __uniffi_result
                                .value
                                .ok_or_else(|| format!("callback method {} returned ok without a callback id", #method_id))?;
                            let __uniffi_proxy = __uniffi_callback_inner
                                .invoker
                                .with_host(|_env, __uniffi_host| {
                                    #return_helper(
                                        __uniffi_host,
                                        #returned_callback_type_id,
                                        __uniffi_callback_id,
                                        #contract_expr,
                                        __uniffi_callback_inner.invoker.clone(),
                                    )
                                    .map_err(|error| format!("callback return proxy construction failed: {:?}", error))
                                })
                                .map_err(|error| format!("callback return host unavailable: {}", error))??;
                            Ok(::std::result::Result::Ok(__uniffi_proxy))
                        })()
                    }
                } else {
                    quote! {
                        (|| -> ::std::result::Result<#result_return, ::std::string::String> {
                            if !__uniffi_result.ok {
                                return Err(format!(
                                    "callback method {} returned an error: {}",
                                    #method_id,
                                    __uniffi_result.error_message.unwrap_or_default(),
                                ));
                            }
                            let __uniffi_callback_id = __uniffi_result
                                .value
                                .ok_or_else(|| format!("callback method {} returned ok without a callback id", #method_id))?;
                            let __uniffi_proxy = __uniffi_callback_inner
                                .invoker
                                .with_host(|_env, __uniffi_host| {
                                    #return_helper(
                                        __uniffi_host,
                                        #returned_callback_type_id,
                                        __uniffi_callback_id,
                                        #contract_expr,
                                        __uniffi_callback_inner.invoker.clone(),
                                    )
                                    .map_err(|error| format!("callback return proxy construction failed: {:?}", error))
                                })
                                .map_err(|error| format!("callback return host unavailable: {}", error))??;
                            Ok(__uniffi_proxy)
                        })()
                    }
                };
                Some(quote! {
                    let __uniffi_inner = self.__uniffi_inner.clone();
                    let __uniffi_callback_inner = __uniffi_inner.clone();
                    let (__uniffi_sender, __uniffi_receiver) = futures_channel::oneshot::channel::<::std::result::Result<#result_return, ::std::string::String>>();
                    let __uniffi_sender = ::std::sync::Arc::new(::std::sync::Mutex::new(Some(__uniffi_sender)));
                    let __uniffi_send_error = |__uniffi_message: ::std::string::String| {
                        let mut __uniffi_guard = match __uniffi_sender.lock() {
                            Ok(__uniffi_guard) => __uniffi_guard,
                            Err(__uniffi_poisoned) => __uniffi_poisoned.into_inner(),
                        };
                        if let Some(__uniffi_sender) = __uniffi_guard.take() {
                            let _ = __uniffi_sender.send(Err(__uniffi_message));
                        }
                    };
                    let __uniffi_invocation_id = match __uniffi_inner.invoker.next_invocation_id() {
                        Ok(__uniffi_id) => __uniffi_id,
                        Err(__uniffi_error) => {
                            __uniffi_send_error(format!("callback invocation allocation failed: {}", __uniffi_error));
                            let __uniffi_result = __uniffi_receiver.await.unwrap_or_else(|_| panic!("callback async result channel closed"));
                            return __uniffi_result.unwrap_or_else(|__uniffi_error| panic!("{}", __uniffi_error));
                        }
                    };
                    let __uniffi_sender_callback = __uniffi_sender.clone();
                    let __uniffi_status = __uniffi_inner
                        .#async_field
                        .call_with_return_value(
                            #payload_name {
                                callback_type: __uniffi_inner.callback_type,
                                callback_id: __uniffi_inner.callback_id,
                                method_id: #method_id,
                                invocation_id: __uniffi_invocation_id,
                                #(#payload_values),*
                            },
                            #napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
                            move |__uniffi_promise_result, _env| {
                                let __uniffi_sender_callback = __uniffi_sender_callback.clone();
                                let __uniffi_send = |__uniffi_value| {
                                    let mut __uniffi_guard = match __uniffi_sender_callback.lock() {
                                        Ok(__uniffi_guard) => __uniffi_guard,
                                        Err(__uniffi_poisoned) => __uniffi_poisoned.into_inner(),
                                    };
                                    if let Some(__uniffi_sender) = __uniffi_guard.take() {
                                        let _ = __uniffi_sender.send(__uniffi_value);
                                    }
                                };
                                let Ok(__uniffi_promise) = __uniffi_promise_result else {
                                    __uniffi_send(Err("callback async host call failed".to_owned()));
                                    return Ok(());
                                };
                                let __uniffi_sender_then = __uniffi_sender_callback.clone();
                                let __uniffi_then = match __uniffi_promise.then(move |context| {
                                    let __uniffi_result = context.value;
                                    let __uniffi_value = #callback_value;
                                    let mut __uniffi_guard = match __uniffi_sender_then.lock() {
                                        Ok(__uniffi_guard) => __uniffi_guard,
                                        Err(__uniffi_poisoned) => __uniffi_poisoned.into_inner(),
                                    };
                                    if let Some(__uniffi_sender) = __uniffi_guard.take() {
                                        let _ = __uniffi_sender.send(__uniffi_value);
                                    }
                                    Ok(())
                                }) {
                                    Ok(__uniffi_then) => __uniffi_then,
                                    Err(__uniffi_error) => {
                                        __uniffi_send(Err(format!("callback async result handler registration failed: {}", __uniffi_error)));
                                        return Ok(());
                                    }
                                };
                                // Catch the promise returned by `then`, not
                                // the original promise.  This observes both
                                // an original rejection and any exception
                                // raised while running the continuation.
                                let __uniffi_sender_catch = __uniffi_sender_callback.clone();
                                if let Err(__uniffi_error) = __uniffi_then.catch(move |_context: #napi::bindgen_prelude::CallbackContext<#napi::bindgen_prelude::Unknown>| {
                                    let mut __uniffi_guard = match __uniffi_sender_catch.lock() {
                                        Ok(__uniffi_guard) => __uniffi_guard,
                                        Err(__uniffi_poisoned) => __uniffi_poisoned.into_inner(),
                                    };
                                    if let Some(__uniffi_sender) = __uniffi_guard.take() {
                                        let _ = __uniffi_sender.send(Err("callback async result rejected".to_owned()));
                                    }
                                    Ok(())
                                }) {
                                    __uniffi_send(Err(format!("callback async error handler registration failed: {}", __uniffi_error)));
                                }
                                Ok(())
                            },
                        );
                    if __uniffi_status != #napi::Status::Ok {
                        __uniffi_send_error(format!("callback async host call failed with status {:?}", __uniffi_status));
                    }
                    let __uniffi_result = __uniffi_receiver.await.unwrap_or_else(|_| panic!("callback async result channel closed"));
                    __uniffi_result.unwrap_or_else(|__uniffi_error| panic!("{}", __uniffi_error))
                })
            } else {
                None
            };
            let lower_args = quote! {
                let mut __uniffi_args = ::std::vec::Vec::<#napi::sys::napi_value>::new();
                #(#sync_arg_lifts)*
            };
            let direct_sync_body = quote! {
                let __uniffi_result = self
                    .__uniffi_inner
                    .invoker
                    .with_host(|__uniffi_env, __uniffi_host| {
                        #lower_args
                        #sync_call
                        Ok::<_, #napi::Error>(__uniffi_result)
                    })
                    .unwrap_or_else(|error| panic!("callback sync host call failed: {}", error))
                    .unwrap_or_else(|error| panic!("callback sync host call failed: {}", error));
                if __uniffi_result.ok { #success_branch } else { #parse_error }
            };
            let sync_method_body = if cross_thread {
                if callback_return_details.is_some() {
                    quote! {
                        if self.__uniffi_inner.invoker.is_owner_thread() {
                            #direct_sync_body
                        } else {
                            #cross_sync_call
                        }
                    }
                } else {
                    quote! {
                        if self.__uniffi_inner.invoker.is_owner_thread() {
                            #direct_sync_body
                        } else {
                            let __uniffi_result = { #cross_sync_call };
                            if __uniffi_result.ok { #success_branch } else { #parse_error }
                        }
                    }
                }
            } else {
                quote! {
                    if !self.__uniffi_inner.invoker.is_owner_thread() {
                        panic!("calling_thread callback method invoked off the owning JavaScript thread");
                    }
                    #direct_sync_body
                }
            };
            let method_body = if callback_operation.async_kind == AsyncKind::Async {
                if let Some(async_return_call) = async_return_call {
                    quote! {
                        async fn #method_ident(&self, #(#arg_declarations),*) -> #result_return {
                            #async_return_call
                        }
                    }
                } else {
                    quote! {
                    async fn #method_ident(&self, #(#arg_declarations),*) -> #result_return {
                        #async_call
                        if __uniffi_result.ok { #success_branch } else { #parse_error }
                    }
                    }
                }
            } else {
                quote! {
                    fn #method_ident(&self, #(#arg_declarations),*) -> #result_return {
                        #sync_method_body
                    }
                }
            };
            impl_methods.push(method_body);
        }

        let retained_contract =
            contract.retention == uniffi_js_engine_schema::CallbackRetention::Retained;
        let helper_signature = if needs_lease && retained_contract {
            quote! {
                fn #helper_name(
                    host: &#napi::bindgen_prelude::Object<'static>,
                    callback_type_id: u32,
                    callback_id: u32,
                    contract: #contract_type,
                    invoker: #invoker_type,
                    lease: #lease_type,
                ) -> ::std::result::Result<::std::sync::Arc<dyn #callback_path>, #error_descriptor>
            }
        } else {
            quote! {
                fn #helper_name(
                    host: &#napi::bindgen_prelude::Object<'static>,
                    callback_type_id: u32,
                    callback_id: u32,
                    contract: #contract_type,
                    invoker: #invoker_type,
                ) -> ::std::result::Result<::std::sync::Arc<dyn #callback_path>, #error_descriptor>
            }
        };
        source.extend(quote! {
            #(#result_structs)*
            struct #inner_name {
                callback_type: u32,
                callback_id: u32,
                invoker: #invoker_type,
                #(#inner_fields)*
                #lease_field
            }
            struct #proxy_name {
                __uniffi_inner: ::std::sync::Arc<#inner_name>,
            }
            #helper_signature {
                let _ = &contract;
                let __uniffi_inner = #inner_name {
                    callback_type: callback_type_id,
                    callback_id,
                    invoker,
                    #(#inner_init)*
                    #lease_init
                };
                Ok(::std::sync::Arc::new(#proxy_name { __uniffi_inner: ::std::sync::Arc::new(__uniffi_inner) }))
            }
            #[async_trait::async_trait]
            impl #callback_path for #proxy_name {
                #(#impl_methods)*
            }
        });
    }
    Ok(source.to_string())
}

/// Build the owned `ErrorData` carrier used by the native engine's declared
/// error envelope.  Native operation shims run outside the JS owner thread,
/// so the error must be entirely Rust-owned; lowering a Rust error to a
/// string/backend descriptor would lose its canonical variant and payload.
fn declared_error_data_expr(
    package: &NormalizedPackage,
    flavor: NativeFlavor,
    binding: &RustValueBinding,
    expression: TokenStream,
) -> Result<TokenStream> {
    let engine_crate = engine_crate_path(flavor);
    let error_data = quote!(#engine_crate::ErrorData);
    let recurse = |binding: &RustValueBinding, expression: TokenStream| {
        declared_error_data_expr(package, flavor, binding, expression)
    };
    match &binding.conversion {
        ConversionRecipe::Identity => match &binding.rust_type {
            RustType::Unit => Ok(quote!(#error_data::Null)),
            RustType::Scalar(scalar) => {
                use uniffi_js_abi::ScalarType;
                Ok(match scalar {
                    ScalarType::Bool => quote!(#error_data::Boolean(#expression)),
                    ScalarType::I8
                    | ScalarType::U8
                    | ScalarType::I16
                    | ScalarType::U16
                    | ScalarType::I32
                    | ScalarType::U32
                    | ScalarType::F32
                    | ScalarType::F64 => quote!(#error_data::Number((#expression) as f64)),
                    ScalarType::I64 | ScalarType::U64 => quote!(
                        #error_data::BigInt(
                            #engine_crate::napi_family_core::BigIntWords::from(#expression)
                        )
                    ),
                    ScalarType::String => quote!(#error_data::String(#expression)),
                    ScalarType::Bytes => quote!(#error_data::Bytes(#expression)),
                })
            }
            // Named values always carry an explicit conversion recipe.  Keep
            // an owned null fallback for malformed/legacy plans rather than
            // reintroducing a display-string error path.
            _ => Ok(quote!({ let _ = #expression; #error_data::Null })),
        },
        ConversionRecipe::BigInt | ConversionRecipe::Bytes => recurse(
            &RustValueBinding {
                rust_type: binding.rust_type.clone(),
                carrier: binding.carrier,
                conversion: ConversionRecipe::Identity,
            },
            expression,
        ),
        ConversionRecipe::Timestamp => Ok(quote!({
            let __uniffi_timestamp = #expression;
            let __uniffi_timestamp_seconds = match __uniffi_timestamp.duration_since(::std::time::UNIX_EPOCH) {
                Ok(value) => value.as_secs_f64(),
                Err(error) => -error.duration().as_secs_f64(),
            };
            #error_data::Number(__uniffi_timestamp_seconds)
        })),
        ConversionRecipe::Duration => Ok(quote!(#error_data::Number(#expression.as_secs_f64()))),
        ConversionRecipe::Optional(inner) => {
            let inner_ty = match &binding.rust_type {
                RustType::Option(inner_ty) => inner_ty.as_ref().clone(),
                _ => RustType::Unit,
            };
            let inner_binding = RustValueBinding {
                rust_type: inner_ty,
                carrier: binding.carrier,
                conversion: *inner.clone(),
            };
            let inner_expression = recurse(&inner_binding, quote!(value))?;
            Ok(quote!(match #expression {
                Some(value) => #inner_expression,
                None => #error_data::Null,
            }))
        }
        ConversionRecipe::Sequence(inner) => {
            let inner_ty = match &binding.rust_type {
                RustType::Sequence(inner_ty) => inner_ty.as_ref().clone(),
                _ => RustType::Unit,
            };
            let inner_binding = RustValueBinding {
                rust_type: inner_ty,
                carrier: binding.carrier,
                conversion: *inner.clone(),
            };
            let inner_expression = recurse(&inner_binding, quote!(value))?;
            Ok(quote!(#error_data::Sequence(
                #expression.into_iter().map(|value| #inner_expression).collect()
            )))
        }
        ConversionRecipe::Set(inner) => {
            let inner_ty = match &binding.rust_type {
                RustType::Set(inner_ty) => inner_ty.as_ref().clone(),
                _ => RustType::Unit,
            };
            let inner_binding = RustValueBinding {
                rust_type: inner_ty,
                carrier: binding.carrier,
                conversion: *inner.clone(),
            };
            let inner_expression = recurse(&inner_binding, quote!(value))?;
            Ok(quote!(#error_data::Sequence(
                #expression.into_iter().map(|value| #inner_expression).collect()
            )))
        }
        ConversionRecipe::Map(key, value) => {
            let (key_ty, value_ty) = match &binding.rust_type {
                RustType::Map(key_ty, value_ty) => {
                    (key_ty.as_ref().clone(), value_ty.as_ref().clone())
                }
                _ => (RustType::Unit, RustType::Unit),
            };
            let key_binding = RustValueBinding {
                rust_type: key_ty,
                carrier: binding.carrier,
                conversion: *key.clone(),
            };
            let value_binding = RustValueBinding {
                rust_type: value_ty,
                carrier: binding.carrier,
                conversion: *value.clone(),
            };
            let key_expression = recurse(&key_binding, quote!(key))?;
            let value_expression = recurse(&value_binding, quote!(value))?;
            // ErrorData has no Map variant.  Preserve map entries as owned
            // `{key,value}` records so payloads never collapse to a display
            // string; the public error decoder can reject unsupported map
            // field shapes explicitly instead of accepting a lossy fallback.
            Ok(quote!(#error_data::Sequence(
                #expression.into_iter().map(|(key, value)| {
                    let mut __uniffi_entry = ::std::collections::BTreeMap::new();
                    __uniffi_entry.insert("key".to_owned(), #key_expression);
                    __uniffi_entry.insert("value".to_owned(), #value_expression);
                    #error_data::Record(__uniffi_entry)
                }).collect()
            )))
        }
        ConversionRecipe::Custom(_, inner) => {
            let inner_binding = RustValueBinding {
                rust_type: match &binding.rust_type {
                    RustType::Custom(inner_ty) => (**inner_ty).clone(),
                    _ => binding.rust_type.clone(),
                },
                carrier: binding.carrier,
                conversion: *inner.clone(),
            };
            recurse(&inner_binding, expression)
        }
        ConversionRecipe::Record(id) => {
            let named = package
                .rust
                .named_type(*id)
                .ok_or_else(|| anyhow!("missing error payload record {}", id.index()))?;
            let core = rust_path(&named.rust_path)?;
            let uniffi_js_engine_schema::RustNamedTypeKind::Record { fields } = &named.kind else {
                bail!("named type {} is not a record", id.index());
            };
            let names = fields
                .iter()
                .map(|field| rust_ident(&field.rust_name))
                .collect::<Vec<_>>();
            let entries = fields
                .iter()
                .zip(&names)
                .map(|(field, name)| {
                    let value = recurse(&field.binding, quote!(#name))?;
                    let public = &field.public_name;
                    Ok::<_, anyhow::Error>(quote! {
                        __uniffi_fields.insert(#public.to_owned(), #value);
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(quote!({
                let #core { #(#names),* } = #expression;
                let mut __uniffi_fields = ::std::collections::BTreeMap::new();
                #(#entries)*
                #error_data::Record(__uniffi_fields)
            }))
        }
        ConversionRecipe::Enum(id) | ConversionRecipe::Error(id) => {
            let named = package
                .rust
                .named_type(*id)
                .ok_or_else(|| anyhow!("missing error payload enum {}", id.index()))?;
            let core = rust_path(&named.rust_path)?;
            let variants = match &named.kind {
                uniffi_js_engine_schema::RustNamedTypeKind::Enum { variants }
                | uniffi_js_engine_schema::RustNamedTypeKind::Error { variants } => variants,
                _ => bail!("named type {} is not an enum/error", id.index()),
            };
            let arms = variants
                .iter()
                .map(|variant| {
                    let rust_variant = rust_ident(&variant.rust_name);
                    let public = &variant.public_name;
                    match &variant.payload {
                        uniffi_js_engine_schema::RustVariantPayload::Unit => Ok(quote! {
                            #core::#rust_variant => #error_data::String(#public.to_owned())
                        }),
                        uniffi_js_engine_schema::RustVariantPayload::Named(fields) => {
                            let names = fields
                                .iter()
                                .map(|field| rust_ident(&field.rust_name))
                                .collect::<Vec<_>>();
                            let entries = fields
                                .iter()
                                .zip(&names)
                                .map(|(field, name)| {
                                    let value = recurse(&field.binding, quote!(#name))?;
                                    let field_name = &field.public_name;
                                    Ok::<_, anyhow::Error>(quote! {
                                        __uniffi_fields.insert(#field_name.to_owned(), #value);
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok(quote! {
                                #core::#rust_variant { #(#names),* } => {
                                    let mut __uniffi_fields = ::std::collections::BTreeMap::new();
                                    __uniffi_fields.insert("tag".to_owned(), #error_data::String(#public.to_owned()));
                                    #(#entries)*
                                    #error_data::Record(__uniffi_fields)
                                }
                            })
                        }
                        uniffi_js_engine_schema::RustVariantPayload::Tuple(fields) => {
                            let names = fields
                                .iter()
                                .enumerate()
                                .map(|(index, _)| rust_ident(&format!("field{index}")))
                                .collect::<Vec<_>>();
                            let entries = fields
                                .iter()
                                .zip(&names)
                                .map(|(field, name)| {
                                    let value = recurse(&field.binding, quote!(#name))?;
                                    let field_name = &field.public_name;
                                    Ok::<_, anyhow::Error>(quote! {
                                        __uniffi_fields.insert(#field_name.to_owned(), #value);
                                    })
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok(quote! {
                                #core::#rust_variant ( #(#names),* ) => {
                                    let mut __uniffi_fields = ::std::collections::BTreeMap::new();
                                    __uniffi_fields.insert("tag".to_owned(), #error_data::String(#public.to_owned()));
                                    #(#entries)*
                                    #error_data::Record(__uniffi_fields)
                                }
                            })
                        }
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(quote!(match #expression { #(#arms),* }))
        }
        // Resource/callback values are not valid declared error payloads.
        // Keep the generated mapper total and owned if a malformed plan gets
        // this far, without exposing handles or JS values in the envelope.
        ConversionRecipe::Object(_)
        | ConversionRecipe::Callback(_)
        | ConversionRecipe::InputStream { .. }
        | ConversionRecipe::OutputStream { .. }
        | ConversionRecipe::StreamStep { .. } => {
            Ok(quote!({ let _ = #expression; #error_data::Null }))
        }
    }
}

fn render_operation_helpers_for(
    package: &NormalizedPackage,
    flavor: NativeFlavor,
    engine: EngineKind,
) -> Result<String> {
    let mut source = TokenStream::new();
    let error_descriptor = flavor.error_descriptor();
    let operations = &package.rust.engines[&engine].operations;
    for operation in operations {
        if let Some(error_id) = operation.throws {
            let error_type = package
                .rust
                .named_type(error_id)
                .ok_or_else(|| anyhow!("missing error type {}", error_id.index()))?;
            let error_path = rust_path(&error_type.rust_path)?;
            let error_name = ident_for_helper(operation.operation_id.index(), "error", "error");
            let error_variant = declared_error_data_expr(
                package,
                flavor,
                &RustValueBinding {
                    rust_type: RustType::Path(error_type.rust_path.clone()),
                    carrier: RustCarrier::LocalAdapter,
                    conversion: ConversionRecipe::Error(error_id),
                },
                quote!(value),
            )?;
            let engine_crate = engine_crate_path(flavor);
            let error_display_name = error_type
                .rust_path
                .segments
                .last()
                .cloned()
                .unwrap_or_else(|| "DeclaredError".to_owned());
            source.extend(quote! {
                fn #error_name(value: #error_path) -> #error_descriptor {
                    let __uniffi_data = #error_variant;
                    let __uniffi_variant = match &__uniffi_data {
                        #engine_crate::ErrorData::String(value) => Some(value.clone()),
                        #engine_crate::ErrorData::Record(fields) => fields.get("tag").and_then(|value| match value {
                            #engine_crate::ErrorData::String(value) => Some(value.clone()),
                            _ => None,
                        }),
                        _ => None,
                    };
                    #engine_crate::BridgeErrorDescriptor {
                        domain: #engine_crate::ErrorDomain::Declared,
                        error_name: #error_display_name.to_owned(),
                        variant: __uniffi_variant.clone(),
                        data: __uniffi_data,
                        message: __uniffi_variant
                            .map(|variant| format!("{}: {}", #error_display_name, variant))
                            .unwrap_or_else(|| #error_display_name.to_owned()),
                        native_stack: None,
                    }
                }
            });
        }
        if let Some(binding) = &operation.receiver {
            let value_binding = RustValueBinding {
                rust_type: binding.rust_type.clone(),
                carrier: binding.carrier,
                conversion: binding.conversion.clone(),
            };
            if let ConversionRecipe::Object(_) = binding.conversion {
                let core = core_type_for_binding(package, &value_binding)?;
                let name = ident_for_helper(
                    operation.operation_id.index(),
                    &operation.arguments.len().to_string(),
                    "lower",
                );
                let owned = binding.ownership == Ownership::Owned;
                source.extend(quote! {
                    fn #name(value: u32) -> ::std::result::Result<#core, #error_descriptor> {
                        crate::__uniffi_take_object::<#core>(value, #owned)
                            .map_err(|error| #error_descriptor::validation(error))
                    }
                });
            }
            if matches!(binding.carrier, RustCarrier::OutputStream)
                && matches!(binding.conversion, ConversionRecipe::Identity)
            {
                let name = ident_for_helper(
                    operation.operation_id.index(),
                    &operation.arguments.len().to_string(),
                    "lower",
                );
                source.extend(quote! {
                    fn #name(value: u32) -> ::std::result::Result<::uniffi::Handle, #error_descriptor> {
                        Ok(::uniffi::Handle::from_raw_unchecked(value as u64))
                    }
                });
            }
        }
        for (index, binding) in operation.arguments.iter().enumerate() {
            if let ConversionRecipe::Object(_) = binding.conversion {
                let value_binding = RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                };
                let core = core_type_for_binding(package, &value_binding)?;
                let name =
                    ident_for_helper(operation.operation_id.index(), &index.to_string(), "lower");
                let owned = binding.ownership == Ownership::Owned;
                let carrier = napi_object_lease_type();
                source.extend(quote! {
                    fn #name(value: #carrier) -> ::std::result::Result<#core, #error_descriptor> {
                        crate::__uniffi_take_object::<#core>(value.handle, #owned)
                            .map_err(|error| #error_descriptor::validation(error))
                    }
                });
                continue;
            }
            let uses_named_type_helper = binding.carrier == RustCarrier::LocalAdapter
                && matches!(
                    binding.conversion,
                    ConversionRecipe::Record(_)
                        | ConversionRecipe::Enum(_)
                        | ConversionRecipe::Error(_)
                        | ConversionRecipe::Custom(_, _)
                );
            let engine_lowers_without_operation_helper =
                matches!(
                    binding.carrier,
                    RustCarrier::CallbackProxy
                        | RustCarrier::InputStream
                        | RustCarrier::OpaqueHandle
                        | RustCarrier::OutputStream
                ) || matches!(binding.conversion, ConversionRecipe::BigInt)
                    || (binding.carrier == RustCarrier::Primitive
                        && matches!(binding.conversion, ConversionRecipe::Identity));
            if conversion_requires_host(&binding.conversion)
                || uses_named_type_helper
                || engine_lowers_without_operation_helper
            {
                continue;
            }
            let value_binding = RustValueBinding {
                rust_type: binding.rust_type.clone(),
                carrier: binding.carrier,
                conversion: binding.conversion.clone(),
            };
            let carrier =
                napi_carrier_type_for(&value_binding, package.rust.named_types(), flavor)?;
            let core = syn_type(&binding.rust_type, flavor.prefix())?;
            let expression = lower_expr(&value_binding, quote!(value), flavor)?;
            let name =
                ident_for_helper(operation.operation_id.index(), &index.to_string(), "lower");
            source.extend(quote! {
                fn #name(value: #carrier) -> ::std::result::Result<#core, #error_descriptor> {
                    Ok(#expression)
                }
            });
        }
        for resource in &operation.stream_resources {
            if resource.direction != uniffi_js_engine_schema::StreamDirection::Output {
                continue;
            }
            let item = core_type_for_binding(package, &resource.item)?;
            let error = core_type_for_binding(package, &resource.error)?;
            let registry =
                output_stream_registry_ident(operation.operation_id.index(), resource.id.index());
            let close =
                output_stream_close_ident(operation.operation_id.index(), resource.id.index());
            source.extend(quote! {
                #[doc(hidden)]
                static #registry: ::uniffi::RustStreamRegistry<#item, #error> =
                    ::uniffi::deps::once_cell::sync::Lazy::new(::std::default::Default::default);

                #[doc(hidden)]
                fn #close(handle: u32) {
                    let handle = ::uniffi::Handle::from_raw_unchecked(handle as u64);
                    ::uniffi::rust_stream_cancel::<#item, #error>(&#registry, handle);
                }
            });
        }
        if let Some(binding) = &operation.return_value {
            if let ConversionRecipe::StreamStep { .. } = &binding.conversion {
                let group = match &operation.call_target {
                    RustCallTarget::StreamHook {
                        parent,
                        use_site_id,
                        ..
                    } => {
                        let parent_operation = operations
                            .iter()
                            .find(|candidate| candidate.operation_id == *parent)
                            .ok_or_else(|| {
                                anyhow!(
                                    "stream-step operation {} references unknown parent {}",
                                    operation.operation_id.index(),
                                    parent.index()
                                )
                            })?;
                        parent_operation
                            .stream_resources
                            .iter()
                            .find(|resource| resource.id == *use_site_id)
                            .ok_or_else(|| {
                                anyhow!(
                                    "stream-step operation {} references unknown stream use-site {}",
                                    operation.operation_id.index(),
                                    use_site_id.index()
                                )
                            })?
                    }
                    _ => {
                        bail!(
                            "stream-step return operation {} is not a stream hook",
                            operation.operation_id.index()
                        )
                    }
                };
                let item_binding = group.item.clone();
                let error_binding = group.error.clone();
                let item_ty = core_type_for_binding(package, &item_binding)?;
                let error_ty = core_type_for_binding(package, &error_binding)?;
                let item_carrier =
                    napi_carrier_type_for(&item_binding, package.rust.named_types(), flavor)?;
                let error_carrier =
                    napi_carrier_type_for(&error_binding, package.rust.named_types(), flavor)?;
                let item_lift = lift_expr(&item_binding, quote!(value), flavor)?;
                let error_lift = lift_expr(&error_binding, quote!(value), flavor)?;
                let core: Type = syn::parse_quote!(::uniffi::UniFfiStreamStep<#item_ty, #error_ty>);
                let carrier = stream_step_carrier_type(operation.operation_id.index());
                let name = ident_for_helper(operation.operation_id.index(), "return", "lift");
                source.extend(quote! {
                    #[napi(discriminant = "kind", discriminant_case = "lowercase", use_nullable = true)]
                    #[derive(Clone, Debug)]
                    pub enum #carrier {
                        Item { value: #item_carrier },
                        Done,
                        Error { error: #error_carrier },
                    }

                    fn #name(value: #core) -> ::std::result::Result<#carrier, #error_descriptor> {
                        match value {
                            ::uniffi::UniFfiStreamStep::Item(value) => Ok(#carrier::Item {
                                value: { #item_lift },
                            }),
                            ::uniffi::UniFfiStreamStep::Done => Ok(#carrier::Done),
                            ::uniffi::UniFfiStreamStep::Error(value) => Ok(#carrier::Error {
                                error: { #error_lift },
                            }),
                        }
                    }
                });
                continue;
            }
            if let ConversionRecipe::Object(_) = binding.conversion {
                let value_binding = RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                };
                let core = core_type_for_binding(package, &value_binding)?;
                let name = ident_for_helper(operation.operation_id.index(), "return", "lift");
                let carrier = napi_object_lease_type();
                source.extend(quote! {
                    fn #name(value: #core) -> ::std::result::Result<#carrier, #error_descriptor> {
                        Ok(#carrier {
                            handle: crate::__uniffi_store_object(value),
                            surface_id: "base".to_owned(),
                        })
                    }
                });
                continue;
            }
            if let ConversionRecipe::OutputStream { .. } = binding.conversion {
                let value_binding = RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                };
                let core = core_type_for_binding(package, &value_binding)?;
                let name = ident_for_helper(operation.operation_id.index(), "return", "lift");
                let carrier = napi_output_stream_lease_type();
                let resource = operation
                    .stream_resources
                    .iter()
                    .find(|resource| {
                        resource.direction == uniffi_js_engine_schema::StreamDirection::Output
                    })
                    .ok_or_else(|| {
                        anyhow!(
                            "output stream return operation {} has no stream resource",
                            operation.operation_id.index()
                        )
                    })?;
                let registry = output_stream_registry_path(
                    operation.operation_id.index(),
                    resource.id.index(),
                );
                let close =
                    output_stream_close_ident(operation.operation_id.index(), resource.id.index());
                source.extend(quote! {
                    fn #name(value: #core) -> ::std::result::Result<#carrier, #error_descriptor> {
                        let handle = ::uniffi::rust_stream_new(&#registry, value);
                        let handle = u32::try_from(handle.as_raw())
                            .map_err(|_| #error_descriptor::validation("output stream handle overflow"))?;
                        crate::__uniffi_register_output_stream(handle, #close);
                        Ok(#carrier { handle })
                    }
                });
                continue;
            }
            // A callback-role return on a native operation is still a Rust
            // trait object (for example a TraitBoth returned by a function
            // which also accepts a callback).  The N-API family carries it as
            // a u32 resource handle; publish the core Arc into the same
            // generation-local registry used by object returns.  Callback
            // method (host-dispatch) operations never reach this branch
            // because their engine return binding is Unit.
            if let ConversionRecipe::Callback(_) = binding.conversion {
                let value_binding = RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                };
                let core = core_type_for_binding(package, &value_binding)?;
                let name = ident_for_helper(operation.operation_id.index(), "return", "lift");
                source.extend(quote! {
                    fn #name(value: #core) -> ::std::result::Result<u32, #error_descriptor> {
                        Ok(crate::__uniffi_store_object(value))
                    }
                });
                continue;
            }
            let uses_named_type_helper = matches!(
                binding.conversion,
                ConversionRecipe::Record(_)
                    | ConversionRecipe::Enum(_)
                    | ConversionRecipe::Error(_)
                    | ConversionRecipe::Custom(_, _)
            );
            let engine_lifts_without_operation_helper = matches!(
                binding.conversion,
                ConversionRecipe::BigInt | ConversionRecipe::Identity
            );
            if !conversion_requires_host(&binding.conversion)
                && !uses_named_type_helper
                && !engine_lifts_without_operation_helper
            {
                let value_binding = RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                };
                let carrier =
                    napi_carrier_type_for(&value_binding, package.rust.named_types(), flavor)?;
                let core = syn_type(&binding.rust_type, flavor.prefix())?;
                let expression = lift_expr(&value_binding, quote!(value), flavor)?;
                let name = ident_for_helper(operation.operation_id.index(), "return", "lift");
                source.extend(quote! {
                    fn #name(value: #core) -> ::std::result::Result<#carrier, #error_descriptor> {
                        Ok(#expression)
                    }
                });
            }
        }
    }
    Ok(source.to_string())
}

/// Build the host-dispatched foreign input-stream adapters expected by the
/// N-API family engine.  Input hooks remain `InputStreamHostPull/Cancel`
/// operations in the canonical plan; only this proxy is generated here so a
/// Rust `UniFfiInputStream` can poll the session Host from worker futures.
fn render_input_stream_helpers(
    package: &NormalizedPackage,
    flavor: NativeFlavor,
    engine: EngineKind,
) -> Result<String> {
    let mut source = TokenStream::new();
    let napi = napi_crate_path(flavor);
    let error_descriptor = flavor.error_descriptor();
    let operations = &package.rust.engines[&engine].operations;
    for operation in operations {
        for resource in &operation.stream_resources {
            if resource.direction != uniffi_js_engine_schema::StreamDirection::Input {
                continue;
            }
            let Some(ValuePathSegment::Argument(argument_index)) = resource.path.segments().first()
            else {
                continue;
            };
            let argument_index = *argument_index as usize;
            let Some(_argument) = operation.arguments.get(argument_index) else {
                bail!(
                    "input stream resource {} references missing argument {} on operation {}",
                    resource.id.index(),
                    argument_index,
                    operation.operation_id.index()
                );
            };
            let item = core_type_for_binding(package, &resource.item)?;
            let error = core_type_for_binding(package, &resource.error)?;
            let item_carrier =
                napi_carrier_type_for(&resource.item, package.rust.named_types(), flavor)?;
            let error_carrier =
                napi_carrier_type_for(&resource.error, package.rust.named_types(), flavor)?;
            let resource_id = resource.id.index();
            let step = input_stream_step_carrier_ident(operation.operation_id.index(), resource_id);
            let ops = rust_ident(&format!(
                "__UniffiNapiInputOps{}_{}",
                operation.operation_id.index(),
                resource_id
            ));
            let helper = input_stream_helper_ident(operation.operation_id.index(), resource_id);
            let lower_item = lower_expr(&resource.item, quote!(value), flavor)?;
            let lower_error = lower_expr(&resource.error, quote!(value), flavor)?;
            let pull_type = quote! {
                ::std::sync::Arc<#napi::threadsafe_function::ThreadsafeFunction<
                    u32,
                    #napi::bindgen_prelude::Promise<#step>,
                    u32,
                    #napi::Status,
                    false,
                >>
            };
            let cancel_type = quote! {
                ::std::sync::Arc<#napi::threadsafe_function::ThreadsafeFunction<
                    u32,
                    #napi::bindgen_prelude::Promise<#napi::bindgen_prelude::Unknown<'static>>,
                    u32,
                    #napi::Status,
                    false,
                >>
            };
            source.extend(quote! {
                #[napi(object)]
                #[derive(Clone, Debug)]
                pub struct #step {
                    pub kind: String,
                    pub value: Option<#item_carrier>,
                    pub error: Option<#error_carrier>,
                }

                struct #ops {
                    pull: #pull_type,
                    cancel: #cancel_type,
                }

                impl ::uniffi::ForeignInputStreamOps<#item, #error> for #ops {
                    fn next(&self, handle: ::uniffi::Handle) -> ::uniffi::ForeignInputStreamNextFuture<#item, #error> {
                        let pull = self.pull.clone();
                        Box::pin(async move {
                            let raw_handle = u32::try_from(handle.as_raw())
                                .unwrap_or_else(|_| panic!("input stream handle exceeds u32"));
                            let promise = pull
                                .call_async(raw_handle)
                                .await
                                .unwrap_or_else(|error| panic!("input stream pull dispatch failed: {error}"));
                            let step = promise
                                .await
                                .unwrap_or_else(|error| panic!("input stream pull promise failed: {error}"));
                            match step.kind.as_str() {
                                "done" => Ok(None),
                                "item" => {
                                    let value = step
                                        .value
                                        .unwrap_or_else(|| panic!("input stream item envelope missing value"));
                                    let value = (|| -> ::std::result::Result<#item, #error_descriptor> {
                                        Ok(#lower_item)
                                    })()
                                        .unwrap_or_else(|error| panic!("input stream item lowering failed: {error:?}"));
                                    Ok(Some(value))
                                }
                                "error" => {
                                    let value = step
                                        .error
                                        .unwrap_or_else(|| panic!("input stream error envelope missing error"));
                                    let value = (|| -> ::std::result::Result<#error, #error_descriptor> {
                                        Ok(#lower_error)
                                    })()
                                        .unwrap_or_else(|error| panic!("input stream error lowering failed: {error:?}"));
                                    Err(value)
                                }
                                other => panic!("input stream envelope has unknown kind: {other}"),
                            }
                        })
                    }

                    fn cancel(&self, handle: ::uniffi::Handle) {
                        let raw_handle = u32::try_from(handle.as_raw())
                            .unwrap_or_else(|_| panic!("input stream handle exceeds u32"));
                        let _ = self.cancel.call(
                            raw_handle,
                            #napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking,
                        );
                    }
                }

                fn #helper(
                    host: &#napi::bindgen_prelude::Object<'static>,
                    handle: u32,
                ) -> ::std::result::Result<::uniffi::UniFfiInputStream<#item, #error>, #error_descriptor> {
                    let pull = host
                        .get_named_property::<#napi::bindgen_prelude::Function<u32, #napi::bindgen_prelude::Promise<#step>>>("pullInputStream")
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?
                        // `pullInputStream` is an instance method on Host;
                        // retain its receiver when constructing the TSFN.
                        .bind(host)
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?
                        .build_threadsafe_function::<u32>()
                        .callee_handled::<false>()
                        .build_callback(|context| Ok(context.value))
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?;
                    let pull = ::std::sync::Arc::new(pull);
                    let cancel = host
                        .get_named_property::<#napi::bindgen_prelude::Function<u32, #napi::bindgen_prelude::Promise<#napi::bindgen_prelude::Unknown<'static>>>>("cancelInputStream")
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?
                        // See `pullInputStream` above: this method reads the
                        // Host's input registry through `this` as well.
                        .bind(host)
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?
                        .build_threadsafe_function::<u32>()
                        .callee_handled::<false>()
                        .build_callback(|context| Ok(context.value))
                        .map_err(|error| #error_descriptor::backend(error.to_string()))?;
                    let cancel = ::std::sync::Arc::new(cancel);
                    Ok(::uniffi::UniFfiInputStream::from_handle_and_ops(
                        ::uniffi::Handle::from_raw_unchecked(handle as u64),
                        ::std::sync::Arc::new(#ops { pull, cancel }),
                    ))
                }
            });
        }
    }
    Ok(source.to_string())
}

/// Lower an operation argument whose canonical resource path contains an
/// input stream below a record/enum/container/custom value.  The N-API
/// family passes the owning Host explicitly to `LowerWithHost`; keep that
/// Host on the call stack while recursively constructing the typed core
/// value instead of caching an engine object globally.
fn lower_host_expr(
    package: &NormalizedPackage,
    flavor: NativeFlavor,
    operation: &RustOperationPlan,
    binding: &RustValueBinding,
    expression: TokenStream,
    path: &[ValuePathSegment],
) -> Result<TokenStream> {
    let error_descriptor = flavor.error_descriptor();
    let child = |binding: &RustValueBinding, expression: TokenStream, path: &[ValuePathSegment]| {
        lower_host_expr(package, flavor, operation, binding, expression, path)
    };
    match &binding.conversion {
        ConversionRecipe::InputStream { .. } => {
            let resource = operation
                .stream_resources
                .iter()
                .find(|resource| resource.path.segments() == path)
                .ok_or_else(|| {
                    anyhow!(
                        "nested input stream path is missing resource group on operation {}",
                        operation.operation_id.index()
                    )
                })?;
            if resource.direction != uniffi_js_engine_schema::StreamDirection::Input {
                bail!(
                    "nested input stream path resolves to a non-input resource on operation {}",
                    operation.operation_id.index()
                );
            }
            let helper =
                input_stream_helper_ident(operation.operation_id.index(), resource.id.index());
            Ok(quote!(#helper(__uniffi_host, #expression)?))
        }
        ConversionRecipe::Record(id) => {
            let named = package
                .rust
                .named_type(*id)
                .ok_or_else(|| anyhow!("missing record type {}", id.index()))?;
            let core = rust_path(&named.rust_path)?;
            let uniffi_js_engine_schema::RustNamedTypeKind::Record { fields } = &named.kind else {
                bail!("named type {} is not a record", id.index());
            };
            let field_values = fields
                .iter()
                .map(|field| {
                    let public = rust_ident(&field.public_name);
                    let rust_name = rust_ident(&field.rust_name);
                    let mut field_path = path.to_vec();
                    field_path.push(ValuePathSegment::Field(field.public_name.clone()));
                    let value = child(&field.binding, quote!(#expression.#public), &field_path)?;
                    Ok::<_, anyhow::Error>(quote!(#rust_name: #value))
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(quote!(#core { #(#field_values,)* }))
        }
        ConversionRecipe::Enum(id) | ConversionRecipe::Error(id) => {
            let named = package
                .rust
                .named_type(*id)
                .ok_or_else(|| anyhow!("missing enum/error type {}", id.index()))?;
            let core = rust_path(&named.rust_path)?;
            let bridge = rust_ident(&format!("__UniffiNapiType{}", id.index()));
            let variants = match &named.kind {
                uniffi_js_engine_schema::RustNamedTypeKind::Enum { variants }
                | uniffi_js_engine_schema::RustNamedTypeKind::Error { variants } => variants,
                _ => bail!("named type {} is not an enum/error", id.index()),
            };
            let arms = variants
                .iter()
                .map(|variant| {
                    let variant_ident = rust_ident(&variant.public_name);
                    let core_variant = rust_ident(&variant.rust_name);
                    let mut variant_path = path.to_vec();
                    variant_path.push(ValuePathSegment::Variant(variant.public_name.clone()));
                    match &variant.payload {
                        uniffi_js_engine_schema::RustVariantPayload::Unit => {
                            Ok::<_, anyhow::Error>(quote!(
                                #bridge::#variant_ident => #core::#core_variant
                            ))
                        }
                        uniffi_js_engine_schema::RustVariantPayload::Named(fields) => {
                            let names = fields
                                .iter()
                                .map(|field| rust_ident(&field.public_name))
                                .collect::<Vec<_>>();
                            let values = fields
                                .iter()
                                .zip(&names)
                                .map(|(field, name)| {
                                    let mut field_path = variant_path.clone();
                                    field_path
                                        .push(ValuePathSegment::Field(field.public_name.clone()));
                                    let value = child(&field.binding, quote!(#name), &field_path)?;
                                    let rust_name = rust_ident(&field.rust_name);
                                    Ok::<_, anyhow::Error>(quote!(#rust_name: #value))
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok(quote!(
                                #bridge::#variant_ident { #(#names),* } =>
                                    #core::#core_variant { #(#values),* }
                            ))
                        }
                        uniffi_js_engine_schema::RustVariantPayload::Tuple(fields) => {
                            let names = fields
                                .iter()
                                .map(|field| rust_ident(&field.public_name))
                                .collect::<Vec<_>>();
                            let values = fields
                                .iter()
                                .zip(&names)
                                .map(|(field, name)| {
                                    let mut field_path = variant_path.clone();
                                    field_path
                                        .push(ValuePathSegment::Field(field.public_name.clone()));
                                    child(&field.binding, quote!(#name), &field_path)
                                })
                                .collect::<Result<Vec<_>>>()?;
                            Ok(quote!(
                                #bridge::#variant_ident { #(#names),* } =>
                                    #core::#core_variant ( #(#values),* )
                            ))
                        }
                    }
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(quote!(match #expression { #(#arms,)* }))
        }
        ConversionRecipe::Optional(inner) => {
            let inner_ty = match &binding.rust_type {
                RustType::Option(inner_ty) => (**inner_ty).clone(),
                _ => RustType::Unit,
            };
            let inner_binding = RustValueBinding {
                rust_type: inner_ty,
                carrier: binding.carrier,
                conversion: (**inner).clone(),
            };
            // Optional does not add a segment to the canonical value path;
            // recurse at the same selector when a value is present.
            let inner_value = child(&inner_binding, quote!(value), path)?;
            Ok(quote!(#expression
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#inner_value) })
                .transpose()?))
        }
        ConversionRecipe::Sequence(inner) => {
            let inner_ty = match &binding.rust_type {
                RustType::Sequence(inner_ty) => (**inner_ty).clone(),
                _ => RustType::Unit,
            };
            let inner_binding = RustValueBinding {
                rust_type: inner_ty,
                carrier: binding.carrier,
                conversion: (**inner).clone(),
            };
            let mut inner_path = path.to_vec();
            inner_path.push(ValuePathSegment::SequenceItem);
            let inner_value = child(&inner_binding, quote!(value), &inner_path)?;
            Ok(quote!(#expression
                .into_iter()
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#inner_value) })
                .collect::<::std::result::Result<Vec<_>, #error_descriptor>>()?))
        }
        ConversionRecipe::Set(inner) => {
            let inner_ty = match &binding.rust_type {
                RustType::Set(inner_ty) => (**inner_ty).clone(),
                _ => RustType::Unit,
            };
            let inner_binding = RustValueBinding {
                rust_type: inner_ty,
                carrier: binding.carrier,
                conversion: (**inner).clone(),
            };
            let mut inner_path = path.to_vec();
            inner_path.push(ValuePathSegment::SetItem);
            let inner_value = child(&inner_binding, quote!(value), &inner_path)?;
            Ok(quote!(#expression
                .into_iter()
                .map(|value| -> ::std::result::Result<_, #error_descriptor> { Ok(#inner_value) })
                .collect::<::std::result::Result<std::collections::HashSet<_>, #error_descriptor>>()?))
        }
        ConversionRecipe::Map(key, value) => {
            let (key_ty, value_ty) = match &binding.rust_type {
                RustType::Map(key_ty, value_ty) => ((**key_ty).clone(), (**value_ty).clone()),
                _ => (RustType::Unit, RustType::Unit),
            };
            let key_binding = RustValueBinding {
                rust_type: key_ty,
                carrier: binding.carrier,
                conversion: (**key).clone(),
            };
            let value_binding = RustValueBinding {
                rust_type: value_ty,
                carrier: binding.carrier,
                conversion: (**value).clone(),
            };
            let mut key_path = path.to_vec();
            key_path.push(ValuePathSegment::MapKey);
            let mut value_path = path.to_vec();
            value_path.push(ValuePathSegment::MapValue);
            let key_value = child(&key_binding, quote!(key), &key_path)?;
            let value_value = child(&value_binding, quote!(value), &value_path)?;
            Ok(quote!(#expression
                .into_iter()
                .map(|(key, value)| -> ::std::result::Result<_, #error_descriptor> {
                    Ok(({ #key_value }, { #value_value }))
                })
                .collect::<::std::result::Result<std::collections::HashMap<_, _>, #error_descriptor>>()?))
        }
        ConversionRecipe::Custom(id, _) => {
            let named = package
                .rust
                .named_type(*id)
                .ok_or_else(|| anyhow!("missing custom type {}", id.index()))?;
            if let uniffi_js_engine_schema::RustNamedTypeKind::Custom { inner, .. } = &named.kind {
                child(inner, expression, path)
            } else {
                lower_expr(binding, expression, flavor)
            }
        }
        _ => lower_expr(binding, expression, flavor),
    }
}

fn render_nested_input_lower_helpers_for(
    package: &NormalizedPackage,
    flavor: NativeFlavor,
    engine: EngineKind,
) -> Result<String> {
    let mut source = TokenStream::new();
    let napi = napi_crate_path(flavor);
    let error_descriptor = flavor.error_descriptor();
    let engine_crate = engine_crate_path(flavor);
    for operation in &package.rust.engines[&engine].operations {
        for (index, argument) in operation.arguments.iter().enumerate() {
            let has_nested_input = operation.stream_resources.iter().any(|resource| {
                resource.direction == uniffi_js_engine_schema::StreamDirection::Input
                    && resource.path.segments().len() > 1
                    && matches!(
                        resource.path.segments().first(),
                        Some(ValuePathSegment::Argument(argument_index))
                            if *argument_index == index as u32
                    )
            });
            if !has_nested_input {
                continue;
            }
            let binding = RustValueBinding {
                rust_type: argument.rust_type.clone(),
                carrier: argument.carrier,
                conversion: argument.conversion.clone(),
            };
            let carrier = napi_carrier_type_for(&binding, package.rust.named_types(), flavor)?;
            let core = core_type_for_binding(package, &binding)?;
            let path = vec![ValuePathSegment::Argument(index as u32)];
            let expression =
                lower_host_expr(package, flavor, operation, &binding, quote!(value), &path)?;
            let helper = operation_host_lower_ident(operation.operation_id.index(), index);
            source.extend(quote! {
                fn #helper(
                    __uniffi_host: & #napi::bindgen_prelude::Object<'static>,
                    value: #carrier,
                    _transfers: &#engine_crate::SessionCallbackTransfers,
                ) -> ::std::result::Result<#core, #error_descriptor> {
                    Ok(#expression)
                }
            });
        }
    }
    Ok(source.to_string())
}

fn native_operation_path(operation_id: u32) -> Path {
    syn::parse_str(&format!("crate::__uniffi_native_operation_{operation_id}"))
        .expect("generated native operation path is valid")
}

fn render_native_operation_wrappers(
    package: &NormalizedPackage,
    engine: EngineKind,
) -> Result<String> {
    let mut source = TokenStream::new();
    for operation in &package.rust.engines[&engine].operations {
        if let RustCallTarget::StreamHook {
            parent,
            use_site_id,
            hook,
        } = &operation.call_target
        {
            let parent_operation = package.rust.engines[&engine]
                .operations
                .iter()
                .find(|candidate| candidate.operation_id == *parent)
                .ok_or_else(|| {
                    anyhow!(
                        "stream hook {} references unknown parent operation {}",
                        operation.operation_id.index(),
                        parent.index()
                    )
                })?;
            let group = parent_operation
                .stream_resources
                .iter()
                .find(|resource| resource.id == *use_site_id)
                .ok_or_else(|| {
                    anyhow!(
                        "stream hook {} references unknown stream use-site {}",
                        operation.operation_id.index(),
                        use_site_id.index()
                    )
                })?;
            let registry = output_stream_registry_path(parent.index(), use_site_id.index());
            let item_ty = core_type_for_binding(package, &group.item)?;
            let error_ty = core_type_for_binding(package, &group.error)?;
            let wrapper = rust_ident(&format!(
                "__uniffi_native_operation_{}",
                operation.operation_id.index()
            ));
            let receiver_name = rust_ident("__uniffi_receiver");
            match *hook {
                RustResourceHook::PullOutputStream => {
                    source.extend(quote! {
                        #[doc(hidden)]
                        async fn #wrapper(#receiver_name: ::uniffi::Handle) -> ::uniffi::UniFfiStreamStep<#item_ty, #error_ty> {
                            ::uniffi::rust_stream_next_async::<#item_ty, #error_ty>(&#registry, #receiver_name)
                                .await
                                .unwrap_or_else(|error| panic!("output stream next failed: {error:?}"))
                        }
                    });
                }
                RustResourceHook::CancelOutputStream => {
                    source.extend(quote! {
                        #[doc(hidden)]
                        async fn #wrapper(#receiver_name: ::uniffi::Handle) {
                            ::uniffi::rust_stream_cancel::<#item_ty, #error_ty>(&#registry, #receiver_name);
                        }
                    });
                }
                RustResourceHook::PullInputStream | RustResourceHook::CancelInputStream => {}
                _ => {}
            }
            continue;
        }
        let (call, receiver_name) = match &operation.call_target {
            RustCallTarget::FreeFunction { module, item } => (
                rust_path(&RustPath {
                    segments: module
                        .segments
                        .iter()
                        .cloned()
                        .chain([item.clone()])
                        .collect(),
                })?,
                None,
            ),
            RustCallTarget::Constructor { object, item, .. } => (
                rust_path(&RustPath {
                    segments: object
                        .segments
                        .iter()
                        .cloned()
                        .chain([item.clone()])
                        .collect(),
                })?,
                None,
            ),
            RustCallTarget::Method { item, .. } => (
                syn::parse_str::<Path>(&format!("__uniffi_method_{item}"))?,
                Some(rust_ident("__uniffi_receiver")),
            ),
            RustCallTarget::CallbackMethod { .. } | RustCallTarget::StreamHook { .. } => continue,
        };
        let wrapper = rust_ident(&format!(
            "__uniffi_native_operation_{}",
            operation.operation_id.index()
        ));
        let mut parameters = Vec::new();
        let mut call_arguments = Vec::new();
        // Constructors may carry the object type in the normalized receiver
        // slot as their result carrier, but the Rust constructor itself has
        // no receiver argument.  Only method call targets lower an explicit
        // receiver into the native wrapper signature.
        if matches!(operation.call_target, RustCallTarget::Method { .. }) {
            let Some(receiver) = operation.receiver.as_ref() else {
                bail!(
                    "method operation {} is missing its normalized receiver",
                    operation.operation_id.index()
                );
            };
            let binding = RustValueBinding {
                rust_type: receiver.rust_type.clone(),
                carrier: receiver.carrier,
                conversion: receiver.conversion.clone(),
            };
            let ty = core_type_for_binding(package, &binding)?;
            let name = rust_ident("__uniffi_receiver");
            parameters.push(quote!(#name: #ty));
        }
        for argument in &operation.arguments {
            let binding = RustValueBinding {
                rust_type: argument.rust_type.clone(),
                carrier: argument.carrier,
                conversion: argument.conversion.clone(),
            };
            let ty = core_type_for_binding(package, &binding)?;
            let name = rust_ident(&argument.rust_name);
            parameters.push(quote!(#name: #ty));
            call_arguments.push(rust_call_argument(quote!(#name), argument.ownership));
        }
        let call_expression = if let Some(receiver_name) = receiver_name {
            let item = match &operation.call_target {
                RustCallTarget::Method { item, .. } => rust_ident(item),
                _ => unreachable!(),
            };
            quote!(#receiver_name.#item(#(#call_arguments),*))
        } else {
            quote!(#call(#(#call_arguments),*))
        };
        // The structured core operation retains the callable's async shape.
        // Await the native future in the generated wrapper before applying
        // any return/error carrier mapping.  Keeping this at the canonical
        // operation boundary covers free functions, constructors, and
        // methods (with or without throws) uniformly; synchronous operations
        // must never introduce an await.
        let call_expression = if operation.async_kind == AsyncKind::Async {
            quote!(#call_expression.await)
        } else {
            call_expression
        };
        let (return_type, return_expression) = match &operation.return_value {
            None => (syn::parse_quote!(()), call_expression),
            Some(binding) if matches!(binding.conversion, ConversionRecipe::Object(_)) => (
                core_type_for_binding(
                    package,
                    &RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    },
                )?,
                call_expression,
            ),
            Some(binding) => (
                core_type_for_binding(
                    package,
                    &RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    },
                )?,
                call_expression,
            ),
        };
        let return_type = if let Some(throws) = operation.throws {
            let error_type = package
                .rust
                .named_type(throws)
                .ok_or_else(|| anyhow!("missing error type {}", throws.index()))?;
            let error_path = rust_path(&error_type.rust_path)?;
            syn::parse_quote!(::std::result::Result<#return_type, #error_path>)
        } else {
            return_type
        };
        let async_token = (operation.async_kind == AsyncKind::Async).then(|| quote!(async));
        source.extend(quote! {
            #[doc(hidden)]
            #async_token fn #wrapper(#(#parameters),*) -> #return_type {
                #return_expression
            }
        });
    }
    Ok(source.to_string())
}

fn napi_argument(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
    index: usize,
    binding: &RustArgumentBinding,
    receiver: bool,
) -> Result<napi_uniffi_engine::RustArgumentPlan> {
    let name = rust_ident(&binding.rust_name);
    let value_binding = RustValueBinding {
        rust_type: binding.rust_type.clone(),
        carrier: binding.carrier,
        conversion: binding.conversion.clone(),
    };
    // Resource receivers are unwrapped by the N-API session immediately
    // before invoking the raw native function.  Ordinary resource
    // arguments, in contrast, stay as the engine-private `{ handle }`
    // carrier so nested/argument ownership is preserved by the session.
    let syn = if receiver
        && matches!(
            binding.carrier,
            RustCarrier::OpaqueHandle | RustCarrier::OutputStream
        ) {
        syn::parse_quote!(u32)
    } else {
        napi_carrier_type(&value_binding, package.rust.named_types())?
    };
    let type_helper = match &binding.conversion {
        ConversionRecipe::Record(id)
        | ConversionRecipe::Enum(id)
        | ConversionRecipe::Error(id)
        | ConversionRecipe::Custom(id, _) => Some(path_for_type_helper(id.index(), "lower")),
        _ => None,
    };
    let nested_input_resource = operation.stream_resources.iter().any(|resource| {
        resource.direction == uniffi_js_engine_schema::StreamDirection::Input
            && resource.path.segments().len() > 1
            && matches!(
                resource.path.segments().first(),
                Some(ValuePathSegment::Argument(argument_index))
                    if *argument_index == index as u32
            )
    });
    let generated = match binding.carrier {
        RustCarrier::BigInt if matches!(binding.conversion, ConversionRecipe::BigInt) => {
            if matches!(
                binding.rust_type,
                RustType::Scalar(uniffi_js_abi::ScalarType::I64)
            ) {
                napi_uniffi_engine::ArgumentBinding::I64BigInt
            } else {
                napi_uniffi_engine::ArgumentBinding::U64BigInt
            }
        }
        RustCarrier::CallbackProxy => napi_uniffi_engine::ArgumentBinding::CallbackProxy {
            rust_type: syn_type(&binding.rust_type, "napi")?,
            build: path_for_helper(
                operation.operation_id.index(),
                &index.to_string(),
                "callback",
            ),
        },
        RustCarrier::InputStream => napi_uniffi_engine::ArgumentBinding::InputStreamProxy {
            rust_type: core_type_for_binding(package, &value_binding)?,
            build: {
                let resource_id = operation
                    .stream_resources
                    .iter()
                    .find(|resource| {
                        resource.direction == uniffi_js_engine_schema::StreamDirection::Input
                            && resource.path.segments()
                                == [ValuePathSegment::Argument(index as u32)]
                    })
                    .map(|resource| resource.id.index())
                    .ok_or_else(|| {
                        anyhow!(
                            "input stream argument {} on operation {} has no canonical resource group",
                            index,
                            operation.operation_id.index()
                        )
                    })?;
                syn::parse_str(&format!(
                    "crate::__uniffi_input_stream_{}_{}",
                    operation.operation_id.index(),
                    resource_id
                ))?
            },
        },
        RustCarrier::OpaqueHandle => napi_uniffi_engine::ArgumentBinding::ObjectLease {
            carrier_type: syn,
            lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
            ownership: match binding.ownership {
                Ownership::Owned => napi_uniffi_engine::napi_family_core::ResourceOwnership::Owned,
                Ownership::Borrowed => {
                    napi_uniffi_engine::napi_family_core::ResourceOwnership::Borrowed
                }
            },
        },
        RustCarrier::OutputStream => napi_uniffi_engine::ArgumentBinding::OutputStreamLease {
            carrier_type: syn,
            lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
            ownership: match binding.ownership {
                Ownership::Owned => napi_uniffi_engine::napi_family_core::ResourceOwnership::Owned,
                Ownership::Borrowed => {
                    napi_uniffi_engine::napi_family_core::ResourceOwnership::Borrowed
                }
            },
        },
        RustCarrier::Primitive if matches!(binding.conversion, ConversionRecipe::Identity) => {
            napi_uniffi_engine::ArgumentBinding::Direct { carrier_type: syn }
        }
        RustCarrier::Timestamp | RustCarrier::Duration => {
            napi_uniffi_engine::ArgumentBinding::LowerWith {
                carrier_type: syn,
                lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
            }
        }
        RustCarrier::LocalAdapter if nested_input_resource => {
            napi_uniffi_engine::ArgumentBinding::LowerWithHost {
                carrier_type: syn,
                lower: syn::parse_str::<Path>(&format!(
                    "crate::__uniffi_lower_host_{}_{}",
                    operation.operation_id.index(),
                    index,
                ))?,
            }
        }
        RustCarrier::LocalAdapter if type_helper.is_some() => {
            napi_uniffi_engine::ArgumentBinding::LowerWith {
                carrier_type: syn,
                lower: type_helper.expect("type helper checked above"),
            }
        }
        _ if !conversion_requires_host(&binding.conversion) => {
            napi_uniffi_engine::ArgumentBinding::LowerWith {
                carrier_type: syn,
                lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
            }
        }
        _ => napi_uniffi_engine::ArgumentBinding::LowerWithHost {
            carrier_type: syn,
            lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
        },
    };
    Ok(napi_uniffi_engine::RustArgumentPlan {
        name,
        binding: generated,
    })
}

fn napi_operation(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
) -> Result<napi_uniffi_engine::RustOperationPlan> {
    let target = match &operation.call_target {
        RustCallTarget::FreeFunction { .. }
        | RustCallTarget::Constructor { .. }
        | RustCallTarget::Method { .. } => napi_uniffi_engine::RustOperationTarget::Native {
            call: native_operation_path(operation.operation_id.index()),
        },
        RustCallTarget::CallbackMethod { .. } => {
            napi_uniffi_engine::RustOperationTarget::CallbackHost
        }
        RustCallTarget::StreamHook { hook, .. } => match hook {
            RustResourceHook::PullInputStream => {
                napi_uniffi_engine::RustOperationTarget::InputStreamHostPull
            }
            RustResourceHook::CancelInputStream => {
                napi_uniffi_engine::RustOperationTarget::InputStreamHostCancel
            }
            RustResourceHook::PullOutputStream | RustResourceHook::CancelOutputStream => {
                napi_uniffi_engine::RustOperationTarget::Native {
                    call: native_operation_path(operation.operation_id.index()),
                }
            }
            _ => napi_uniffi_engine::RustOperationTarget::CallbackHost,
        },
    };
    let receiver = if matches!(
        target,
        napi_uniffi_engine::RustOperationTarget::Native { .. }
    ) && matches!(
        operation.kind,
        OperationKind::Method
            | OperationKind::InputStreamPull
            | OperationKind::InputStreamCancel
            | OperationKind::OutputStreamNext
            | OperationKind::OutputStreamCancel
    ) {
        if let Some(binding) = operation.receiver.as_ref() {
            let argument = RustArgumentBinding {
                public_name: "receiver".into(),
                rust_name: "__uniffi_receiver".into(),
                rust_type: binding.rust_type.clone(),
                carrier: binding.carrier,
                ownership: binding.ownership,
                conversion: binding.conversion.clone(),
            };
            let mut binding = napi_argument(
                package,
                operation,
                operation.arguments.len(),
                &argument,
                true,
            )?
            .binding;
            if let napi_uniffi_engine::ArgumentBinding::ObjectLease { ownership, .. } = &mut binding
            {
                if argument.ownership == Ownership::Owned {
                    *ownership = napi_uniffi_engine::napi_family_core::ResourceOwnership::ByArc;
                }
            }
            Some(napi_uniffi_engine::RustReceiverPlan {
                name: rust_ident("__uniffi_receiver"),
                binding,
            })
        } else {
            None
        }
    } else {
        None
    };
    let arguments = if matches!(
        target,
        napi_uniffi_engine::RustOperationTarget::Native { .. }
    ) {
        operation
            .arguments
            .iter()
            .enumerate()
            .map(|(index, binding)| napi_argument(package, operation, index, binding, false))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let return_type = operation
        .return_value
        .as_ref()
        .map(|binding| {
            if matches!(binding.conversion, ConversionRecipe::StreamStep { .. }) {
                return Ok(stream_step_carrier_type(operation.operation_id.index()));
            }
            napi_carrier_type(
                &RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                },
                package.rust.named_types(),
            )
        })
        .transpose()?;
    let return_helper =
        operation
            .return_value
            .as_ref()
            .and_then(|binding| match &binding.conversion {
                ConversionRecipe::Record(id)
                | ConversionRecipe::Enum(id)
                | ConversionRecipe::Error(id)
                | ConversionRecipe::Custom(id, _) => Some(path_for_type_helper(id.index(), "lift")),
                _ => None,
            });
    let return_binding = if !matches!(
        target,
        napi_uniffi_engine::RustOperationTarget::Native { .. }
    ) {
        napi_uniffi_engine::ReturnBinding::Unit
    } else {
        match operation.return_value.as_ref() {
            None => napi_uniffi_engine::ReturnBinding::Unit,
            Some(binding) if matches!(binding.conversion, ConversionRecipe::BigInt) => {
                if matches!(
                    binding.rust_type,
                    RustType::Scalar(uniffi_js_abi::ScalarType::I64)
                ) {
                    napi_uniffi_engine::ReturnBinding::I64BigInt
                } else {
                    napi_uniffi_engine::ReturnBinding::U64BigInt
                }
            }
            Some(binding) if matches!(binding.carrier, RustCarrier::OpaqueHandle) => {
                napi_uniffi_engine::ReturnBinding::ObjectLease {
                    carrier_type: return_type.expect("object carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(binding) if matches!(binding.carrier, RustCarrier::OutputStream) => {
                napi_uniffi_engine::ReturnBinding::OutputStreamLease {
                    carrier_type: return_type.expect("stream carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(binding) if matches!(binding.carrier, RustCarrier::CallbackProxy) => {
                napi_uniffi_engine::ReturnBinding::CallbackLease {
                    carrier_type: return_type.expect("callback carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(binding) if matches!(binding.conversion, ConversionRecipe::StreamStep { .. }) => {
                napi_uniffi_engine::ReturnBinding::LiftWith {
                    carrier_type: return_type.expect("stream-step carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(_binding) if return_helper.is_some() => {
                napi_uniffi_engine::ReturnBinding::LiftWith {
                    carrier_type: return_type.expect("named carrier type"),
                    lift: return_helper.expect("type helper checked above"),
                }
            }
            Some(binding) if matches!(binding.conversion, ConversionRecipe::Identity) => {
                napi_uniffi_engine::ReturnBinding::Direct {
                    carrier_type: return_type.expect("direct carrier type"),
                }
            }
            Some(_binding) => napi_uniffi_engine::ReturnBinding::LiftWith {
                carrier_type: return_type.expect("carrier type"),
                lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
            },
        }
    };
    let error_binding = if matches!(
        target,
        napi_uniffi_engine::RustOperationTarget::Native { .. }
    ) {
        operation
            .throws
            .map_or(napi_uniffi_engine::ErrorBinding::Infallible, |_| {
                napi_uniffi_engine::ErrorBinding::Descriptor {
                    map: path_for_helper(operation.operation_id.index(), "error", "error"),
                }
            })
    } else {
        napi_uniffi_engine::ErrorBinding::Infallible
    };
    Ok(napi_uniffi_engine::RustOperationPlan {
        operation_id: operation.operation_id.index(),
        target,
        receiver,
        arguments,
        return_binding,
        error_binding,
    })
}

pub(crate) fn napi_source(package: &NormalizedPackage) -> Result<String> {
    let family = family_plan(package, FamilyFlavor::Node)?;
    let operations = package.rust.engines[&EngineKind::Napi]
        .operations
        .iter()
        .map(|operation| napi_operation(package, operation))
        .collect::<Result<Vec<_>>>()?;
    let rust = napi_uniffi_engine::RustBridgePlan::build_with_resource_hooks(
        &family,
        operations,
        napi_uniffi_engine::RustResourceHooks {
            release_object: Some(napi_uniffi_engine::RustResourceHook {
                call: syn::parse_str("crate::__uniffi_release_object_impl")?,
                carrier_type: syn::parse_quote!(u32),
            }),
            cancel_output_stream: Some(napi_uniffi_engine::RustResourceHook {
                call: syn::parse_str("crate::__uniffi_cancel_output_stream_impl")?,
                carrier_type: syn::parse_quote!(u32),
            }),
            release_output_stream: Some(napi_uniffi_engine::RustResourceHook {
                call: syn::parse_str("crate::__uniffi_release_output_stream_impl")?,
                carrier_type: syn::parse_quote!(u32),
            }),
        },
    )
    .map_err(|error| anyhow!("N-API Rust plan: {error}"))?;
    let module = napi_uniffi_engine::generate_napi_module(&family, &rust)
        .map_err(|error| anyhow!("N-API source: {error}"))?;
    let temporal = render_temporal_helpers(NativeFlavor::Node)?;
    let helpers = render_named_type_helpers(package)?;
    let operation_helpers =
        render_operation_helpers_for(package, NativeFlavor::Node, EngineKind::Napi)?;
    let nested_input_helpers =
        render_nested_input_lower_helpers_for(package, NativeFlavor::Node, EngineKind::Napi)?;
    let input_stream_helpers =
        render_input_stream_helpers(package, NativeFlavor::Node, EngineKind::Napi)?;
    let callback_helpers =
        render_callback_proxy_helpers(package, NativeFlavor::Node, EngineKind::Napi)?;
    let registry = render_resource_registry(NativeFlavor::Node);
    let wrappers = render_native_operation_wrappers(package, EngineKind::Napi)?;
    let derive_import = format!("use {};\n", NativeFlavor::Node.derive_import());
    Ok(format!(
        "{derive_import}{registry}\n{temporal}\n{helpers}\n{operation_helpers}\n{nested_input_helpers}\n{input_stream_helpers}\n{callback_helpers}\n{wrappers}\n{}",
        module.source()
    ))
}

#[cfg(feature = "ohos")]
fn ohos_argument(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
    index: usize,
    binding: &RustArgumentBinding,
    receiver: bool,
) -> Result<napi_ohos_uniffi_engine::OhosArgumentPlan> {
    let name = rust_ident(&binding.rust_name);
    let value_binding = RustValueBinding {
        rust_type: binding.rust_type.clone(),
        carrier: binding.carrier,
        conversion: binding.conversion.clone(),
    };
    // The N-API session strips a resource receiver's `{ handle }` object to
    // its numeric handle before the generated raw operation is called.  A
    // normal object/output argument remains the private carrier object.
    let syn = if receiver
        && matches!(
            binding.carrier,
            RustCarrier::OpaqueHandle | RustCarrier::OutputStream
        ) {
        syn::parse_quote!(u32)
    } else {
        napi_carrier_type_for(
            &value_binding,
            package.rust.named_types(),
            NativeFlavor::Ohos,
        )?
    };
    let type_helper = match &binding.conversion {
        ConversionRecipe::Record(id)
        | ConversionRecipe::Enum(id)
        | ConversionRecipe::Error(id)
        | ConversionRecipe::Custom(id, _) => Some(path_for_type_helper(id.index(), "lower")),
        _ => None,
    };
    let ownership = match binding.ownership {
        Ownership::Owned => napi_ohos_uniffi_engine::napi_family_core::ResourceOwnership::Owned,
        Ownership::Borrowed => {
            napi_ohos_uniffi_engine::napi_family_core::ResourceOwnership::Borrowed
        }
    };
    let generated = match binding.carrier {
        RustCarrier::BigInt if matches!(binding.conversion, ConversionRecipe::BigInt) => {
            if matches!(
                binding.rust_type,
                RustType::Scalar(uniffi_js_abi::ScalarType::I64)
            ) {
                napi_ohos_uniffi_engine::OhosArgumentBinding::I64BigInt
            } else {
                napi_ohos_uniffi_engine::OhosArgumentBinding::U64BigInt
            }
        }
        RustCarrier::CallbackProxy => napi_ohos_uniffi_engine::OhosArgumentBinding::CallbackProxy {
            rust_type: syn_type(&binding.rust_type, "napi_ohos")?,
            build: path_for_helper(
                operation.operation_id.index(),
                &index.to_string(),
                "callback",
            ),
        },
        RustCarrier::InputStream => {
            napi_ohos_uniffi_engine::OhosArgumentBinding::InputStreamProxy {
                rust_type: core_type_for_binding(package, &value_binding)?,
                build: path_for_helper(
                    operation.operation_id.index(),
                    &index.to_string(),
                    "input_stream",
                ),
            }
        }
        RustCarrier::OpaqueHandle => napi_ohos_uniffi_engine::OhosArgumentBinding::ObjectLease {
            carrier_type: syn,
            lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
            ownership,
        },
        RustCarrier::OutputStream => {
            napi_ohos_uniffi_engine::OhosArgumentBinding::OutputStreamLease {
                carrier_type: syn,
                lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
                ownership,
            }
        }
        RustCarrier::Primitive if matches!(binding.conversion, ConversionRecipe::Identity) => {
            napi_ohos_uniffi_engine::OhosArgumentBinding::Direct { carrier_type: syn }
        }
        RustCarrier::Timestamp | RustCarrier::Duration => {
            napi_ohos_uniffi_engine::OhosArgumentBinding::LowerWith {
                carrier_type: syn,
                lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
            }
        }
        RustCarrier::LocalAdapter if type_helper.is_some() => {
            napi_ohos_uniffi_engine::OhosArgumentBinding::LowerWith {
                carrier_type: syn,
                lower: type_helper.expect("type helper checked above"),
            }
        }
        _ if !conversion_requires_host(&binding.conversion) => {
            napi_ohos_uniffi_engine::OhosArgumentBinding::LowerWith {
                carrier_type: syn,
                lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
            }
        }
        _ => napi_ohos_uniffi_engine::OhosArgumentBinding::LowerWithHost {
            carrier_type: syn,
            lower: path_for_helper(operation.operation_id.index(), &index.to_string(), "lower"),
        },
    };
    Ok(napi_ohos_uniffi_engine::OhosArgumentPlan {
        name,
        binding: generated,
    })
}

#[cfg(feature = "ohos")]
fn ohos_operation(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
) -> Result<napi_ohos_uniffi_engine::OhosOperationPlan> {
    let target = match &operation.call_target {
        RustCallTarget::FreeFunction { .. }
        | RustCallTarget::Constructor { .. }
        | RustCallTarget::Method { .. } => napi_ohos_uniffi_engine::OhosOperationTarget::Native {
            call: native_operation_path(operation.operation_id.index()),
        },
        RustCallTarget::CallbackMethod { .. } => {
            napi_ohos_uniffi_engine::OhosOperationTarget::CallbackHost
        }
        RustCallTarget::StreamHook { hook, .. } => match hook {
            RustResourceHook::PullInputStream => {
                napi_ohos_uniffi_engine::OhosOperationTarget::InputStreamHostPull
            }
            RustResourceHook::CancelInputStream => {
                napi_ohos_uniffi_engine::OhosOperationTarget::InputStreamHostCancel
            }
            RustResourceHook::PullOutputStream | RustResourceHook::CancelOutputStream => {
                napi_ohos_uniffi_engine::OhosOperationTarget::Native {
                    call: native_operation_path(operation.operation_id.index()),
                }
            }
            _ => napi_ohos_uniffi_engine::OhosOperationTarget::CallbackHost,
        },
    };
    let receiver = if matches!(
        target,
        napi_ohos_uniffi_engine::OhosOperationTarget::Native { .. }
    ) && matches!(
        operation.kind,
        OperationKind::Method
            | OperationKind::InputStreamPull
            | OperationKind::InputStreamCancel
            | OperationKind::OutputStreamNext
            | OperationKind::OutputStreamCancel
    ) {
        if let Some(binding) = operation.receiver.as_ref() {
            let argument = RustArgumentBinding {
                public_name: "receiver".into(),
                rust_name: "__uniffi_receiver".into(),
                rust_type: binding.rust_type.clone(),
                carrier: binding.carrier,
                ownership: binding.ownership,
                conversion: binding.conversion.clone(),
            };
            let mut binding = ohos_argument(
                package,
                operation,
                operation.arguments.len(),
                &argument,
                true,
            )?
            .binding;
            if let napi_ohos_uniffi_engine::OhosArgumentBinding::ObjectLease { ownership, .. } =
                &mut binding
            {
                if argument.ownership == Ownership::Owned {
                    *ownership =
                        napi_ohos_uniffi_engine::napi_family_core::ResourceOwnership::ByArc;
                }
            }
            Some(napi_ohos_uniffi_engine::OhosReceiverPlan {
                name: rust_ident("__uniffi_receiver"),
                binding,
            })
        } else {
            None
        }
    } else {
        None
    };
    let arguments = if matches!(
        target,
        napi_ohos_uniffi_engine::OhosOperationTarget::Native { .. }
    ) {
        operation
            .arguments
            .iter()
            .enumerate()
            .map(|(index, binding)| ohos_argument(package, operation, index, binding, false))
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let return_type = operation
        .return_value
        .as_ref()
        .map(|binding| {
            if matches!(binding.conversion, ConversionRecipe::StreamStep { .. }) {
                return Ok(stream_step_carrier_type(operation.operation_id.index()));
            }
            napi_carrier_type_for(
                &RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                },
                package.rust.named_types(),
                NativeFlavor::Ohos,
            )
        })
        .transpose()?;
    let return_helper =
        operation
            .return_value
            .as_ref()
            .and_then(|binding| match &binding.conversion {
                ConversionRecipe::Record(id)
                | ConversionRecipe::Enum(id)
                | ConversionRecipe::Error(id)
                | ConversionRecipe::Custom(id, _) => Some(path_for_type_helper(id.index(), "lift")),
                _ => None,
            });
    let return_binding = if !matches!(
        target,
        napi_ohos_uniffi_engine::OhosOperationTarget::Native { .. }
    ) {
        napi_ohos_uniffi_engine::OhosReturnBinding::Unit
    } else {
        match operation.return_value.as_ref() {
            None => napi_ohos_uniffi_engine::OhosReturnBinding::Unit,
            Some(binding) if matches!(binding.conversion, ConversionRecipe::BigInt) => {
                if matches!(
                    binding.rust_type,
                    RustType::Scalar(uniffi_js_abi::ScalarType::I64)
                ) {
                    napi_ohos_uniffi_engine::OhosReturnBinding::I64BigInt
                } else {
                    napi_ohos_uniffi_engine::OhosReturnBinding::U64BigInt
                }
            }
            Some(binding) if matches!(binding.carrier, RustCarrier::OpaqueHandle) => {
                napi_ohos_uniffi_engine::OhosReturnBinding::ObjectLease {
                    carrier_type: return_type.expect("object carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(binding) if matches!(binding.carrier, RustCarrier::OutputStream) => {
                napi_ohos_uniffi_engine::OhosReturnBinding::OutputStreamLease {
                    carrier_type: return_type.expect("stream carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(binding) if matches!(binding.carrier, RustCarrier::CallbackProxy) => {
                napi_ohos_uniffi_engine::OhosReturnBinding::CallbackLease {
                    carrier_type: return_type.expect("callback carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(binding) if matches!(binding.conversion, ConversionRecipe::StreamStep { .. }) => {
                napi_ohos_uniffi_engine::OhosReturnBinding::LiftWith {
                    carrier_type: return_type.expect("stream-step carrier type"),
                    lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
                }
            }
            Some(_) if return_helper.is_some() => {
                napi_ohos_uniffi_engine::OhosReturnBinding::LiftWith {
                    carrier_type: return_type.expect("named carrier type"),
                    lift: return_helper.expect("type helper checked above"),
                }
            }
            Some(binding) if matches!(binding.conversion, ConversionRecipe::Identity) => {
                napi_ohos_uniffi_engine::OhosReturnBinding::Direct {
                    carrier_type: return_type.expect("direct carrier type"),
                }
            }
            Some(_) => napi_ohos_uniffi_engine::OhosReturnBinding::LiftWith {
                carrier_type: return_type.expect("carrier type"),
                lift: path_for_helper(operation.operation_id.index(), "return", "lift"),
            },
        }
    };
    let error_binding = if matches!(
        target,
        napi_ohos_uniffi_engine::OhosOperationTarget::Native { .. }
    ) {
        operation.throws.map_or(
            napi_ohos_uniffi_engine::OhosErrorBinding::Infallible,
            |_| napi_ohos_uniffi_engine::OhosErrorBinding::Descriptor {
                map: path_for_helper(operation.operation_id.index(), "error", "error"),
            },
        )
    } else {
        napi_ohos_uniffi_engine::OhosErrorBinding::Infallible
    };
    Ok(napi_ohos_uniffi_engine::OhosOperationPlan {
        operation_id: operation.operation_id.index(),
        target,
        receiver,
        arguments,
        return_binding,
        error_binding,
    })
}

#[cfg(feature = "ohos")]
pub(crate) fn ohos_source(package: &NormalizedPackage) -> Result<String> {
    let family = family_plan(package, FamilyFlavor::Ohos)?;
    let operations = package.rust.engines[&EngineKind::OhosNapi]
        .operations
        .iter()
        .map(|operation| ohos_operation(package, operation))
        .collect::<Result<Vec<_>>>()?;
    let rust = napi_ohos_uniffi_engine::OhosBridgePlan::build_with_resource_hooks(
        &family,
        operations,
        napi_ohos_uniffi_engine::OhosResourceHooks {
            release_object: Some(napi_ohos_uniffi_engine::OhosResourceHook {
                call: syn::parse_str("crate::__uniffi_release_object_impl")?,
                carrier_type: syn::parse_quote!(u32),
            }),
            cancel_output_stream: Some(napi_ohos_uniffi_engine::OhosResourceHook {
                call: syn::parse_str("crate::__uniffi_cancel_output_stream_impl")?,
                carrier_type: syn::parse_quote!(u32),
            }),
            release_output_stream: Some(napi_ohos_uniffi_engine::OhosResourceHook {
                call: syn::parse_str("crate::__uniffi_release_output_stream_impl")?,
                carrier_type: syn::parse_quote!(u32),
            }),
        },
    )
    .map_err(|error| anyhow!("OHOS Rust plan: {error}"))?;
    let module = napi_ohos_uniffi_engine::generate_ohos_source(&family, rust)
        .map_err(|error| anyhow!("OHOS source: {error}"))?;
    let temporal = render_temporal_helpers(NativeFlavor::Ohos)?;
    let helpers = render_named_type_helpers_for(package, NativeFlavor::Ohos)?;
    let operation_helpers =
        render_operation_helpers_for(package, NativeFlavor::Ohos, EngineKind::OhosNapi)?;
    let nested_input_helpers =
        render_nested_input_lower_helpers_for(package, NativeFlavor::Ohos, EngineKind::OhosNapi)?;
    let input_stream_helpers =
        render_input_stream_helpers(package, NativeFlavor::Ohos, EngineKind::OhosNapi)?;
    let callback_helpers =
        render_callback_proxy_helpers(package, NativeFlavor::Ohos, EngineKind::OhosNapi)?;
    let registry = render_resource_registry(NativeFlavor::Ohos);
    let wrappers = render_native_operation_wrappers(package, EngineKind::OhosNapi)?;
    let derive_import = format!("use {};\n", NativeFlavor::Ohos.derive_import());
    Ok(format!(
        "{derive_import}{registry}\n{temporal}\n{helpers}\n{operation_helpers}\n{nested_input_helpers}\n{input_stream_helpers}\n{callback_helpers}\n{wrappers}\n{}",
        module.source()
    ))
}

#[cfg(not(feature = "ohos"))]
pub(crate) fn ohos_source(_package: &NormalizedPackage) -> Result<String> {
    bail!("Harmony generation requires the uniffi_bindgen_javascript `ohos` feature")
}

fn wasm_rust_path(path: &RustPath) -> Result<wasm_bindgen_uniffi_engine::RustPath> {
    wasm_bindgen_uniffi_engine::RustPath::new(path.segments.clone())
        .map_err(|error| anyhow!("invalid wasm Rust path: {error}"))
}

fn wasm_type(ty: &RustType) -> Result<wasm_bindgen_uniffi_engine::WasmRustType> {
    use wasm_bindgen_uniffi_engine::WasmRustType as W;
    Ok(match ty {
        RustType::Unit => W::Unit,
        RustType::Scalar(scalar) => W::Scalar(match scalar {
            ScalarType::Bool => wasm_bindgen_uniffi_engine::WasmScalarType::Bool,
            ScalarType::I8 => wasm_bindgen_uniffi_engine::WasmScalarType::I8,
            ScalarType::I16 => wasm_bindgen_uniffi_engine::WasmScalarType::I16,
            ScalarType::I32 => wasm_bindgen_uniffi_engine::WasmScalarType::I32,
            ScalarType::U8 => wasm_bindgen_uniffi_engine::WasmScalarType::U8,
            ScalarType::U16 => wasm_bindgen_uniffi_engine::WasmScalarType::U16,
            ScalarType::U32 => wasm_bindgen_uniffi_engine::WasmScalarType::U32,
            ScalarType::I64 => wasm_bindgen_uniffi_engine::WasmScalarType::I64,
            ScalarType::U64 => wasm_bindgen_uniffi_engine::WasmScalarType::U64,
            ScalarType::F32 => wasm_bindgen_uniffi_engine::WasmScalarType::F32,
            ScalarType::F64 => wasm_bindgen_uniffi_engine::WasmScalarType::F64,
            ScalarType::String => wasm_bindgen_uniffi_engine::WasmScalarType::String,
            ScalarType::Bytes => wasm_bindgen_uniffi_engine::WasmScalarType::Bytes,
        }),
        RustType::Timestamp => W::Timestamp,
        RustType::Duration => W::Duration,
        RustType::Path(path) => W::Path(wasm_rust_path(path)?),
        RustType::Option(inner) => W::Option(Box::new(wasm_type(inner)?)),
        RustType::Sequence(inner) => W::Sequence(Box::new(wasm_type(inner)?)),
        RustType::Map(key, value) => W::Map(Box::new(wasm_type(key)?), Box::new(wasm_type(value)?)),
        RustType::Set(inner) => W::Set(Box::new(wasm_type(inner)?)),
        RustType::Stream {
            item,
            error,
            is_send,
        } => W::Stream {
            item: Box::new(wasm_type(item)?),
            error: Box::new(wasm_type(error)?),
            is_send: *is_send,
        },
        RustType::InputStream {
            item,
            error,
            is_send,
        } => W::InputStream {
            item: Box::new(wasm_type(item)?),
            error: Box::new(wasm_type(error)?),
            is_send: *is_send,
        },
        RustType::StreamStep { item, error } => W::StreamStep {
            item: Box::new(wasm_type(item)?),
            error: Box::new(wasm_type(error)?),
        },
        RustType::Custom(inner) => W::Custom(Box::new(wasm_type(inner)?)),
    })
}

fn wasm_carrier(binding: &RustValueBinding) -> wasm_bindgen_uniffi_engine::WasmCarrier {
    use wasm_bindgen_uniffi_engine::WasmCarrier as C;
    match binding.carrier {
        RustCarrier::BigInt => match binding.rust_type {
            RustType::Scalar(ScalarType::I64) => C::I64,
            RustType::Scalar(ScalarType::U64) => C::U64,
            _ => C::JsValue,
        },
        RustCarrier::Bytes => C::Bytes,
        RustCarrier::OpaqueHandle | RustCarrier::InputStream | RustCarrier::OutputStream => C::U32,
        RustCarrier::Primitive => match binding.rust_type {
            RustType::Scalar(ScalarType::Bool) => C::Bool,
            RustType::Scalar(ScalarType::I8) => C::I8,
            RustType::Scalar(ScalarType::U8) => C::U8,
            RustType::Scalar(ScalarType::I16) => C::I16,
            RustType::Scalar(ScalarType::U16) => C::U16,
            RustType::Scalar(ScalarType::I32) => C::I32,
            RustType::Scalar(ScalarType::U32) => C::U32,
            RustType::Scalar(ScalarType::I64) => C::I64,
            RustType::Scalar(ScalarType::U64) => C::U64,
            RustType::Scalar(ScalarType::F32) => C::F32,
            RustType::Scalar(ScalarType::F64) => C::F64,
            RustType::Scalar(ScalarType::String) => C::String,
            RustType::Scalar(ScalarType::Bytes) => C::Bytes,
            _ => C::JsValue,
        },
        RustCarrier::Timestamp
        | RustCarrier::Duration
        | RustCarrier::LocalAdapter
        | RustCarrier::CallbackProxy
        | RustCarrier::StreamStep => C::JsValue,
    }
}

fn wasm_conversion(
    conversion: &ConversionRecipe,
) -> wasm_bindgen_uniffi_engine::WasmConversionRecipe {
    use wasm_bindgen_uniffi_engine::WasmConversionRecipe as C;
    match conversion {
        ConversionRecipe::Identity => C::Identity,
        ConversionRecipe::Timestamp => C::Timestamp,
        ConversionRecipe::Duration => C::Duration,
        ConversionRecipe::BigInt => C::BigInt,
        ConversionRecipe::Bytes => C::Bytes,
        ConversionRecipe::Optional(inner) => C::Optional(Box::new(wasm_conversion(inner))),
        ConversionRecipe::Sequence(inner) => C::Sequence(Box::new(wasm_conversion(inner))),
        ConversionRecipe::Map(key, value) => C::Map(
            Box::new(wasm_conversion(key)),
            Box::new(wasm_conversion(value)),
        ),
        ConversionRecipe::Set(inner) => C::Set(Box::new(wasm_conversion(inner))),
        ConversionRecipe::Record(id) => C::Record(id.index()),
        ConversionRecipe::Enum(id) => C::Enum(id.index()),
        ConversionRecipe::Error(id) => C::Error(id.index()),
        ConversionRecipe::Object(id) => C::Object(id.index()),
        ConversionRecipe::Custom(id, inner) => {
            C::Custom(id.index(), Box::new(wasm_conversion(inner)))
        }
        ConversionRecipe::Callback(id) => C::Callback(id.index()),
        ConversionRecipe::InputStream { item, error } => C::InputStream {
            item: Box::new(wasm_conversion(item)),
            error: Box::new(wasm_conversion(error)),
        },
        ConversionRecipe::OutputStream { item, error } => C::OutputStream {
            item: Box::new(wasm_conversion(item)),
            error: Box::new(wasm_conversion(error)),
        },
        ConversionRecipe::StreamStep { item, error } => C::StreamStep {
            item: Box::new(wasm_conversion(item)),
            error: Box::new(wasm_conversion(error)),
        },
    }
}

fn wasm_ownership(ownership: Ownership) -> wasm_bindgen_uniffi_engine::WasmOwnership {
    match ownership {
        Ownership::Owned => wasm_bindgen_uniffi_engine::WasmOwnership::Owned,
        Ownership::Borrowed => wasm_bindgen_uniffi_engine::WasmOwnership::Borrowed,
    }
}

/// The Wasm engine owns object leases only for values it returns.  Receivers
/// and arguments are borrowed views into an existing facade lease, even when
/// the canonical Rust plan records a different ownership required by another
/// backend (for example an `Arc<Self>` receiver).  Keep this normalization at
/// the engine DTO boundary; the canonical Rust plan remains unchanged and
/// callback values are deliberately not treated as object resources here.
fn wasm_binding_contains_object(
    package: &NormalizedPackage,
    binding: &RustValueBinding,
    visiting: &mut BTreeSet<TypeId>,
) -> bool {
    fn conversion_contains_object(
        package: &NormalizedPackage,
        rust_type: &RustType,
        conversion: &ConversionRecipe,
        visiting: &mut BTreeSet<TypeId>,
    ) -> bool {
        match conversion {
            ConversionRecipe::Object(_) => true,
            ConversionRecipe::Optional(inner)
            | ConversionRecipe::Sequence(inner)
            | ConversionRecipe::Set(inner)
            | ConversionRecipe::Custom(_, inner) => {
                let nested_type = match (conversion, rust_type) {
                    (ConversionRecipe::Optional(_), RustType::Option(inner))
                    | (ConversionRecipe::Sequence(_), RustType::Sequence(inner))
                    | (ConversionRecipe::Set(_), RustType::Set(inner))
                    | (ConversionRecipe::Custom(_, _), RustType::Custom(inner)) => inner.as_ref(),
                    _ => rust_type,
                };
                conversion_contains_object(package, nested_type, inner, visiting)
            }
            ConversionRecipe::Map(key, value) => {
                let (key_type, value_type) = match rust_type {
                    RustType::Map(key_type, value_type) => (key_type.as_ref(), value_type.as_ref()),
                    _ => (rust_type, rust_type),
                };
                conversion_contains_object(package, key_type, key, visiting)
                    || conversion_contains_object(package, value_type, value, visiting)
            }
            ConversionRecipe::InputStream { item, error }
            | ConversionRecipe::OutputStream { item, error }
            | ConversionRecipe::StreamStep { item, error } => {
                let (item_type, error_type) = match rust_type {
                    RustType::InputStream { item, error, .. }
                    | RustType::Stream { item, error, .. }
                    | RustType::StreamStep { item, error } => (item.as_ref(), error.as_ref()),
                    _ => (rust_type, rust_type),
                };
                conversion_contains_object(package, item_type, item, visiting)
                    || conversion_contains_object(package, error_type, error, visiting)
            }
            ConversionRecipe::Record(id)
            | ConversionRecipe::Enum(id)
            | ConversionRecipe::Error(id) => {
                if !visiting.insert(*id) {
                    return false;
                }
                let found = package
                    .rust
                    .named_type(*id)
                    .map(|named| match &named.kind {
                        RustNamedTypeKind::Record { fields } => fields.iter().any(|field| {
                            wasm_binding_contains_object(package, &field.binding, visiting)
                        }),
                        RustNamedTypeKind::Enum { variants } => {
                            variants.iter().any(|variant| match &variant.payload {
                                RustVariantPayload::Unit => false,
                                RustVariantPayload::Named(fields) => fields.iter().any(|field| {
                                    wasm_binding_contains_object(package, &field.binding, visiting)
                                }),
                                RustVariantPayload::Tuple(fields) => fields.iter().any(|field| {
                                    wasm_binding_contains_object(package, &field.binding, visiting)
                                }),
                            })
                        }
                        RustNamedTypeKind::Error { variants } => {
                            variants.iter().any(|variant| match &variant.payload {
                                RustVariantPayload::Unit => false,
                                RustVariantPayload::Named(fields) => fields.iter().any(|field| {
                                    wasm_binding_contains_object(package, &field.binding, visiting)
                                }),
                                RustVariantPayload::Tuple(fields) => fields.iter().any(|field| {
                                    wasm_binding_contains_object(package, &field.binding, visiting)
                                }),
                            })
                        }
                        _ => false,
                    })
                    .unwrap_or(false);
                visiting.remove(id);
                found
            }
            // Callback use-sites have their own callback contract and registry;
            // never fold them into object lease ownership normalization.
            ConversionRecipe::Callback(_)
            | ConversionRecipe::Identity
            | ConversionRecipe::Timestamp
            | ConversionRecipe::Duration
            | ConversionRecipe::BigInt
            | ConversionRecipe::Bytes => false,
        }
    }

    conversion_contains_object(package, &binding.rust_type, &binding.conversion, visiting)
}

fn wasm_binding_ownership(
    package: &NormalizedPackage,
    binding: &RustValueBinding,
    ownership: Ownership,
    return_value: bool,
) -> wasm_bindgen_uniffi_engine::WasmOwnership {
    let contains_object = wasm_binding_contains_object(package, binding, &mut BTreeSet::new());
    if contains_object {
        if return_value {
            wasm_bindgen_uniffi_engine::WasmOwnership::Owned
        } else {
            wasm_bindgen_uniffi_engine::WasmOwnership::Borrowed
        }
    } else {
        wasm_ownership(ownership)
    }
}

fn wasm_operation_kind(kind: OperationKind) -> wasm_bindgen_uniffi_engine::WasmOperationKind {
    use wasm_bindgen_uniffi_engine::WasmOperationKind as K;
    match kind {
        OperationKind::Function => K::Function,
        OperationKind::Constructor => K::Constructor,
        OperationKind::Method => K::Method,
        OperationKind::CallbackMethod => K::CallbackMethod,
        OperationKind::OutputStreamStart => K::OutputStreamStart,
        OperationKind::OutputStreamNext => K::OutputStreamNext,
        OperationKind::OutputStreamCancel => K::OutputStreamCancel,
        OperationKind::InputStreamPull => K::InputStreamPull,
        OperationKind::InputStreamCancel => K::InputStreamCancel,
    }
}

fn wasm_async(kind: AsyncKind) -> wasm_bindgen_uniffi_engine::WasmAsyncKind {
    match kind {
        AsyncKind::Sync => wasm_bindgen_uniffi_engine::WasmAsyncKind::Sync,
        AsyncKind::Async => wasm_bindgen_uniffi_engine::WasmAsyncKind::Async,
    }
}

fn wasm_object_kind(
    kind: uniffi_js_engine_schema::RustObjectKind,
) -> wasm_bindgen_uniffi_engine::WasmObjectKind {
    use wasm_bindgen_uniffi_engine::WasmObjectKind as K;
    match kind {
        uniffi_js_engine_schema::RustObjectKind::Struct => K::Struct,
        uniffi_js_engine_schema::RustObjectKind::TraitRustOnly => K::TraitRustOnly,
        uniffi_js_engine_schema::RustObjectKind::TraitBoth => K::TraitBoth,
        uniffi_js_engine_schema::RustObjectKind::TraitForeignOnly => K::TraitForeignOnly,
    }
}

fn wasm_type_source(
    key: &uniffi_js_abi::TypeSourceKey,
) -> wasm_bindgen_uniffi_engine::WasmTypeSourceKey {
    wasm_bindgen_uniffi_engine::WasmTypeSourceKey {
        component: key.component().namespace().to_owned(),
        name: key.name().to_owned(),
    }
}

fn wasm_owner(owner: &OperationOwner) -> wasm_bindgen_uniffi_engine::WasmOperationOwner {
    use wasm_bindgen_uniffi_engine::WasmOperationOwner as O;
    match owner {
        OperationOwner::Namespace => O::Namespace,
        OperationOwner::Object(key) => O::Object(wasm_type_source(key)),
        OperationOwner::Value(key) => O::Value(wasm_type_source(key)),
        OperationOwner::Callback(key) => O::Callback(wasm_type_source(key)),
    }
}

fn wasm_call_target(target: &RustCallTarget) -> Result<wasm_bindgen_uniffi_engine::WasmCallTarget> {
    use wasm_bindgen_uniffi_engine::WasmCallTarget as C;
    Ok(match target {
        RustCallTarget::FreeFunction { module, item } => C::FreeFunction {
            module: wasm_rust_path(module)?,
            item: item.clone(),
        },
        RustCallTarget::Constructor {
            object,
            object_kind,
            item,
        } => C::Constructor {
            object: wasm_rust_path(object)?,
            object_kind: wasm_object_kind(*object_kind),
            item: item.clone(),
        },
        RustCallTarget::Method {
            object,
            object_kind,
            callback_method_id,
            item,
        } => C::Method {
            object: wasm_rust_path(object)?,
            object_kind: wasm_object_kind(*object_kind),
            callback_method_id: *callback_method_id,
            item: item.clone(),
        },
        RustCallTarget::CallbackMethod {
            callback,
            callback_type,
            method_id,
            item,
        } => C::CallbackMethod {
            callback: wasm_rust_path(callback)?,
            callback_type_id: callback_type.index(),
            method_id: *method_id,
            item: item.clone(),
        },
        RustCallTarget::StreamHook {
            parent,
            use_site_id,
            hook,
        } => C::StreamHook {
            parent_operation_id: parent.index(),
            use_site_id: use_site_id.index(),
            hook: wasm_resource_hook(*hook),
        },
    })
}

fn wasm_resource_hook(hook: RustResourceHook) -> wasm_bindgen_uniffi_engine::WasmResourceHook {
    use wasm_bindgen_uniffi_engine::WasmResourceHook as H;
    match hook {
        RustResourceHook::None => H::None,
        RustResourceHook::AcquireObject => H::AcquireObject,
        RustResourceHook::ReleaseObject => H::ReleaseObject,
        RustResourceHook::StartInputStream => H::StartInputStream,
        RustResourceHook::PullInputStream => H::PullInputStream,
        RustResourceHook::CancelInputStream => H::CancelInputStream,
        RustResourceHook::CloseInputStream => H::CloseInputStream,
        RustResourceHook::StartOutputStream => H::StartOutputStream,
        RustResourceHook::PullOutputStream => H::PullOutputStream,
        RustResourceHook::CancelOutputStream => H::CancelOutputStream,
        RustResourceHook::CloseOutputStream => H::CloseOutputStream,
    }
}

fn wasm_value_path(path: &ValuePath) -> wasm_bindgen_uniffi_engine::WasmValuePath {
    wasm_resource_path(
        &path
            .segments()
            .iter()
            .map(|segment| match segment {
                ValuePathSegment::Argument(index) => ResourcePathSegment::Argument(*index),
                ValuePathSegment::Return => ResourcePathSegment::Return,
                ValuePathSegment::Field(name) => ResourcePathSegment::Field(name.clone()),
                ValuePathSegment::Variant(name) => ResourcePathSegment::Variant(name.clone()),
                ValuePathSegment::SequenceItem => ResourcePathSegment::SequenceItem,
                ValuePathSegment::SetItem => ResourcePathSegment::SetItem,
                ValuePathSegment::MapKey => ResourcePathSegment::MapKey,
                ValuePathSegment::MapValue => ResourcePathSegment::MapValue,
            })
            .collect::<Vec<_>>(),
    )
}

fn wasm_value_binding(
    binding: &RustValueBinding,
) -> Result<wasm_bindgen_uniffi_engine::WasmValueBinding> {
    Ok(wasm_bindgen_uniffi_engine::WasmValueBinding {
        rust_type: wasm_type(&binding.rust_type)?,
        carrier: match binding.carrier {
            RustCarrier::Primitive => wasm_bindgen_uniffi_engine::WasmRustCarrier::Primitive,
            RustCarrier::BigInt => wasm_bindgen_uniffi_engine::WasmRustCarrier::BigInt,
            RustCarrier::Bytes => wasm_bindgen_uniffi_engine::WasmRustCarrier::Bytes,
            RustCarrier::Timestamp => wasm_bindgen_uniffi_engine::WasmRustCarrier::Timestamp,
            RustCarrier::Duration => wasm_bindgen_uniffi_engine::WasmRustCarrier::Duration,
            RustCarrier::LocalAdapter => wasm_bindgen_uniffi_engine::WasmRustCarrier::LocalAdapter,
            RustCarrier::OpaqueHandle => wasm_bindgen_uniffi_engine::WasmRustCarrier::OpaqueHandle,
            RustCarrier::CallbackProxy => {
                wasm_bindgen_uniffi_engine::WasmRustCarrier::CallbackProxy
            }
            RustCarrier::InputStream => wasm_bindgen_uniffi_engine::WasmRustCarrier::InputStream,
            RustCarrier::OutputStream => wasm_bindgen_uniffi_engine::WasmRustCarrier::OutputStream,
            RustCarrier::StreamStep => wasm_bindgen_uniffi_engine::WasmRustCarrier::StreamStep,
        },
        abi_carrier: wasm_carrier(binding),
        conversion: wasm_conversion(&binding.conversion),
    })
}

fn wasm_resource_paths_for_binding(
    package: &NormalizedPackage,
    binding: &RustValueBinding,
    path: &[ResourcePathSegment],
    ownership: Ownership,
    output: &mut Vec<wasm_bindgen_uniffi_engine::WasmResourceUseSite>,
    visiting: &mut BTreeSet<TypeId>,
) {
    match &binding.conversion {
        ConversionRecipe::Object(type_id) => {
            output.push(wasm_bindgen_uniffi_engine::WasmResourceUseSite::object(
                wasm_resource_path(path),
                type_id.index(),
                wasm_ownership(ownership),
            ))
        }
        ConversionRecipe::Optional(inner)
        | ConversionRecipe::Sequence(inner)
        | ConversionRecipe::Set(inner) => {
            let segment = match &binding.conversion {
                ConversionRecipe::Optional(_) => ResourcePathSegment::Optional,
                ConversionRecipe::Sequence(_) => ResourcePathSegment::SequenceItem,
                ConversionRecipe::Set(_) => ResourcePathSegment::SetItem,
                _ => unreachable!(),
            };
            let mut nested = binding.clone();
            nested.conversion = (**inner).clone();
            nested.rust_type = match &binding.rust_type {
                RustType::Option(value) | RustType::Sequence(value) | RustType::Set(value) => {
                    (**value).clone()
                }
                _ => RustType::Unit,
            };
            let mut next = path.to_vec();
            next.push(segment);
            wasm_resource_paths_for_binding(package, &nested, &next, ownership, output, visiting);
        }
        ConversionRecipe::Map(key, value) => {
            if let RustType::Map(key_type, value_type) = &binding.rust_type {
                for (conversion, rust_type, segment) in [
                    (key.as_ref(), key_type.as_ref(), ResourcePathSegment::MapKey),
                    (
                        value.as_ref(),
                        value_type.as_ref(),
                        ResourcePathSegment::MapValue,
                    ),
                ] {
                    let mut nested = binding.clone();
                    nested.conversion = conversion.clone();
                    nested.rust_type = rust_type.clone();
                    let mut next = path.to_vec();
                    next.push(segment);
                    wasm_resource_paths_for_binding(
                        package, &nested, &next, ownership, output, visiting,
                    );
                }
            }
        }
        ConversionRecipe::Record(type_id)
        | ConversionRecipe::Enum(type_id)
        | ConversionRecipe::Error(type_id) => {
            if !visiting.insert(*type_id) {
                return;
            }
            if let Some(named) = package.rust.named_type(*type_id) {
                match &named.kind {
                    RustNamedTypeKind::Record { fields } => {
                        for field in fields {
                            let mut next = path.to_vec();
                            next.push(ResourcePathSegment::Field(field.public_name.clone()));
                            wasm_resource_paths_for_binding(
                                package,
                                &field.binding,
                                &next,
                                ownership,
                                output,
                                visiting,
                            );
                        }
                    }
                    RustNamedTypeKind::Enum { variants }
                    | RustNamedTypeKind::Error { variants } => {
                        for variant in variants {
                            match &variant.payload {
                                RustVariantPayload::Named(fields) => {
                                    for field in fields {
                                        let mut next = path.to_vec();
                                        next.push(ResourcePathSegment::Variant(
                                            variant.public_name.clone(),
                                        ));
                                        next.push(ResourcePathSegment::Field(
                                            field.public_name.clone(),
                                        ));
                                        wasm_resource_paths_for_binding(
                                            package,
                                            &field.binding,
                                            &next,
                                            ownership,
                                            output,
                                            visiting,
                                        );
                                    }
                                }
                                RustVariantPayload::Tuple(fields) => {
                                    for field in fields {
                                        let mut next = path.to_vec();
                                        next.push(ResourcePathSegment::Variant(
                                            variant.public_name.clone(),
                                        ));
                                        next.push(ResourcePathSegment::Field(
                                            field.public_name.clone(),
                                        ));
                                        wasm_resource_paths_for_binding(
                                            package,
                                            &field.binding,
                                            &next,
                                            ownership,
                                            output,
                                            visiting,
                                        );
                                    }
                                }
                                RustVariantPayload::Unit => {}
                            }
                        }
                    }
                    _ => {}
                }
            }
            visiting.remove(type_id);
        }
        ConversionRecipe::Custom(_, inner) => {
            let mut nested = binding.clone();
            nested.conversion = (**inner).clone();
            wasm_resource_paths_for_binding(package, &nested, path, ownership, output, visiting);
        }
        ConversionRecipe::StreamStep { item, error } => {
            let (item_ty, error_ty) = match &binding.rust_type {
                RustType::StreamStep { item, error } => (item.as_ref(), error.as_ref()),
                _ => (&binding.rust_type, &binding.rust_type),
            };
            let mut item_binding = binding.clone();
            item_binding.rust_type = item_ty.clone();
            item_binding.conversion = (**item).clone();
            let mut item_path = path.to_vec();
            item_path.push(ResourcePathSegment::StreamItem);
            wasm_resource_paths_for_binding(
                package,
                &item_binding,
                &item_path,
                ownership,
                output,
                visiting,
            );
            let mut error_binding = binding.clone();
            error_binding.rust_type = error_ty.clone();
            error_binding.conversion = (**error).clone();
            let mut error_path = path.to_vec();
            error_path.push(ResourcePathSegment::StreamError);
            wasm_resource_paths_for_binding(
                package,
                &error_binding,
                &error_path,
                ownership,
                output,
                visiting,
            );
        }
        ConversionRecipe::InputStream { .. }
        | ConversionRecipe::OutputStream { .. }
        | ConversionRecipe::Callback(_)
        | ConversionRecipe::Identity
        | ConversionRecipe::Timestamp
        | ConversionRecipe::Duration
        | ConversionRecipe::BigInt
        | ConversionRecipe::Bytes => {}
    }
}

fn wasm_resource_use_sites(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
) -> Vec<wasm_bindgen_uniffi_engine::WasmResourceUseSite> {
    let mut output = Vec::new();
    let mut visit =
        |binding: &RustValueBinding, path: Vec<ResourcePathSegment>, ownership: Ownership| {
            wasm_resource_paths_for_binding(
                package,
                binding,
                &path,
                ownership,
                &mut output,
                &mut BTreeSet::new(),
            );
        };
    if let Some(receiver) = &operation.receiver {
        let binding = RustValueBinding {
            rust_type: receiver.rust_type.clone(),
            carrier: receiver.carrier,
            conversion: receiver.conversion.clone(),
        };
        visit(
            &binding,
            vec![ResourcePathSegment::Receiver],
            if wasm_binding_contains_object(package, &binding, &mut BTreeSet::new()) {
                Ownership::Borrowed
            } else {
                receiver.ownership
            },
        );
    }
    for (index, argument) in operation.arguments.iter().enumerate() {
        let binding = RustValueBinding {
            rust_type: argument.rust_type.clone(),
            carrier: argument.carrier,
            conversion: argument.conversion.clone(),
        };
        visit(
            &binding,
            vec![ResourcePathSegment::Argument(index as u32)],
            if wasm_binding_contains_object(package, &binding, &mut BTreeSet::new()) {
                Ownership::Borrowed
            } else {
                argument.ownership
            },
        );
    }
    if let Some(return_value) = &operation.return_value {
        let binding = RustValueBinding {
            rust_type: return_value.rust_type.clone(),
            carrier: return_value.carrier,
            conversion: return_value.conversion.clone(),
        };
        visit(
            &binding,
            vec![ResourcePathSegment::Return],
            if wasm_binding_contains_object(package, &binding, &mut BTreeSet::new()) {
                Ownership::Owned
            } else {
                return_value.ownership
            },
        );
    }
    output
}

fn wasm_callback_contract(
    contract: uniffi_js_engine_schema::CallbackContract,
) -> wasm_bindgen_uniffi_engine::WasmCallbackContract {
    wasm_bindgen_uniffi_engine::WasmCallbackContract {
        retention: match contract.retention {
            uniffi_js_engine_schema::CallbackRetention::Scoped => {
                wasm_bindgen_uniffi_engine::WasmCallbackRetention::Scoped
            }
            uniffi_js_engine_schema::CallbackRetention::Retained => {
                wasm_bindgen_uniffi_engine::WasmCallbackRetention::Retained
            }
        },
        threading: match contract.threading {
            uniffi_js_engine_schema::CallbackThreading::CallingThread => {
                wasm_bindgen_uniffi_engine::WasmCallbackThreading::CallingThread
            }
            uniffi_js_engine_schema::CallbackThreading::MayCrossThread => {
                wasm_bindgen_uniffi_engine::WasmCallbackThreading::MayCrossThread
            }
        },
        reentrancy: match contract.reentrancy {
            uniffi_js_engine_schema::CallbackReentrancy::Forbidden => {
                wasm_bindgen_uniffi_engine::WasmCallbackReentrancy::Forbidden
            }
            uniffi_js_engine_schema::CallbackReentrancy::Allowed => {
                wasm_bindgen_uniffi_engine::WasmCallbackReentrancy::Allowed
            }
        },
    }
}

fn wasm_stream_contract(
    contract: uniffi_js_engine_schema::StreamContract,
) -> wasm_bindgen_uniffi_engine::WasmStreamContract {
    wasm_bindgen_uniffi_engine::WasmStreamContract {
        direction: match contract.direction {
            uniffi_js_engine_schema::StreamDirection::Input => {
                wasm_bindgen_uniffi_engine::WasmStreamDirection::Input
            }
            uniffi_js_engine_schema::StreamDirection::Output => {
                wasm_bindgen_uniffi_engine::WasmStreamDirection::Output
            }
        },
        lazy_start: contract.lazy_start,
        single_consumer: contract.single_consumer,
        serial_pull: contract.serial_pull,
        exactly_once_cleanup: contract.exactly_once_cleanup,
        explicit_cancel: contract.explicit_cancel,
        eof_is_distinct_from_item: contract.eof_is_distinct_from_item,
    }
}

fn wasm_stream_groups(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
) -> Result<Vec<wasm_bindgen_uniffi_engine::WasmStreamResourceGroup>> {
    operation
        .stream_resources
        .iter()
        .map(|group| {
            let site = package
                .bridge
                .streams()
                .iter()
                .find(|site| site.id == group.id && site.operation_id == operation.operation_id)
                .ok_or_else(|| anyhow!("missing stream use-site {}", group.id.index()))?;
            Ok(wasm_bindgen_uniffi_engine::WasmStreamResourceGroup {
                use_site: wasm_bindgen_uniffi_engine::WasmStreamUseSite {
                    id: site.id.index(),
                    operation_id: site.operation_id.index(),
                    path: wasm_value_path(&site.path),
                    contract: wasm_stream_contract(site.contract),
                },
                item: wasm_value_binding(&group.item)?,
                error: wasm_value_binding(&group.error)?,
                is_send: group.is_send,
                hooks: group
                    .hooks
                    .iter()
                    .copied()
                    .map(wasm_resource_hook)
                    .collect(),
                slot_operation_ids: group
                    .slot_operation_ids
                    .iter()
                    .map(|(kind, id)| (wasm_operation_kind(*kind), id.index()))
                    .collect(),
            })
        })
        .collect()
}

fn wasm_operation_plan(
    package: &NormalizedPackage,
    operation: &RustOperationPlan,
) -> Result<wasm_bindgen_uniffi_engine::WasmOperationPlan> {
    use wasm_bindgen_uniffi_engine::{
        WasmArgumentBinding, WasmOperationPlan, WasmOperationSourceKey, WasmReceiverBinding,
        WasmReturnBinding,
    };

    let source_key = WasmOperationSourceKey {
        component: operation.source_key.component().namespace().to_owned(),
        owner: wasm_owner(operation.source_key.owner()),
        kind: wasm_operation_kind(operation.source_key.kind()),
        name: operation.source_key.name().to_owned(),
    };
    let receiver = operation
        .receiver
        .as_ref()
        .map(|binding| -> Result<WasmReceiverBinding> {
            Ok(WasmReceiverBinding {
                rust_type: wasm_type(&binding.rust_type)?,
                carrier: match binding.carrier {
                    RustCarrier::Primitive => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::Primitive
                    }
                    RustCarrier::BigInt => wasm_bindgen_uniffi_engine::WasmRustCarrier::BigInt,
                    RustCarrier::Bytes => wasm_bindgen_uniffi_engine::WasmRustCarrier::Bytes,
                    RustCarrier::Timestamp => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::Timestamp
                    }
                    RustCarrier::Duration => wasm_bindgen_uniffi_engine::WasmRustCarrier::Duration,
                    RustCarrier::LocalAdapter => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::LocalAdapter
                    }
                    RustCarrier::OpaqueHandle => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::OpaqueHandle
                    }
                    RustCarrier::CallbackProxy => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::CallbackProxy
                    }
                    RustCarrier::InputStream => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::InputStream
                    }
                    RustCarrier::OutputStream => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::OutputStream
                    }
                    RustCarrier::StreamStep => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::StreamStep
                    }
                },
                abi_carrier: wasm_carrier(&RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                }),
                ownership: wasm_binding_ownership(
                    package,
                    &RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    },
                    binding.ownership,
                    false,
                ),
                conversion: wasm_conversion(&binding.conversion),
            })
        })
        .transpose()?;

    let arguments = operation
        .arguments
        .iter()
        .map(|binding| {
            Ok(WasmArgumentBinding {
                public_name: binding.public_name.clone(),
                rust_name: binding.rust_name.clone(),
                rust_type: wasm_type(&binding.rust_type)?,
                carrier: match binding.carrier {
                    RustCarrier::Primitive => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::Primitive
                    }
                    RustCarrier::BigInt => wasm_bindgen_uniffi_engine::WasmRustCarrier::BigInt,
                    RustCarrier::Bytes => wasm_bindgen_uniffi_engine::WasmRustCarrier::Bytes,
                    RustCarrier::Timestamp => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::Timestamp
                    }
                    RustCarrier::Duration => wasm_bindgen_uniffi_engine::WasmRustCarrier::Duration,
                    RustCarrier::LocalAdapter => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::LocalAdapter
                    }
                    RustCarrier::OpaqueHandle => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::OpaqueHandle
                    }
                    RustCarrier::CallbackProxy => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::CallbackProxy
                    }
                    RustCarrier::InputStream => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::InputStream
                    }
                    RustCarrier::OutputStream => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::OutputStream
                    }
                    RustCarrier::StreamStep => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::StreamStep
                    }
                },
                abi_carrier: wasm_carrier(&RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                }),
                ownership: wasm_binding_ownership(
                    package,
                    &RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    },
                    binding.ownership,
                    false,
                ),
                conversion: wasm_conversion(&binding.conversion),
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let return_value = operation
        .return_value
        .as_ref()
        .map(|binding| -> Result<WasmReturnBinding> {
            Ok(WasmReturnBinding {
                rust_type: wasm_type(&binding.rust_type)?,
                carrier: match binding.carrier {
                    RustCarrier::Primitive => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::Primitive
                    }
                    RustCarrier::BigInt => wasm_bindgen_uniffi_engine::WasmRustCarrier::BigInt,
                    RustCarrier::Bytes => wasm_bindgen_uniffi_engine::WasmRustCarrier::Bytes,
                    RustCarrier::Timestamp => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::Timestamp
                    }
                    RustCarrier::Duration => wasm_bindgen_uniffi_engine::WasmRustCarrier::Duration,
                    RustCarrier::LocalAdapter => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::LocalAdapter
                    }
                    RustCarrier::OpaqueHandle => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::OpaqueHandle
                    }
                    RustCarrier::CallbackProxy => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::CallbackProxy
                    }
                    RustCarrier::InputStream => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::InputStream
                    }
                    RustCarrier::OutputStream => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::OutputStream
                    }
                    RustCarrier::StreamStep => {
                        wasm_bindgen_uniffi_engine::WasmRustCarrier::StreamStep
                    }
                },
                abi_carrier: wasm_carrier(&RustValueBinding {
                    rust_type: binding.rust_type.clone(),
                    carrier: binding.carrier,
                    conversion: binding.conversion.clone(),
                }),
                ownership: wasm_binding_ownership(
                    package,
                    &RustValueBinding {
                        rust_type: binding.rust_type.clone(),
                        carrier: binding.carrier,
                        conversion: binding.conversion.clone(),
                    },
                    binding.ownership,
                    true,
                ),
                conversion: wasm_conversion(&binding.conversion),
            })
        })
        .transpose()?;

    let callback_use_sites = package
        .bridge
        .callbacks()
        .iter()
        .filter(|callback| callback.operation_id == operation.operation_id)
        .map(|callback| wasm_bindgen_uniffi_engine::WasmCallbackUseSite {
            operation_id: callback.operation_id.index(),
            callback_type_id: callback.callback_type.index(),
            path: wasm_value_path(&callback.path),
            contract: wasm_callback_contract(callback.contract),
        })
        .collect();

    let rust_call = wasm_rust_path(&RustPath::new([
        "crate".to_owned(),
        format!(
            "__uniffi_typed_wasm_operation_{}",
            operation.operation_id.index()
        ),
    ]))?;

    Ok(WasmOperationPlan {
        operation_id: operation.operation_id.index(),
        source_key,
        component_id: operation.component_id.index(),
        owner: wasm_owner(&operation.owner),
        kind: wasm_operation_kind(operation.kind),
        callback_method_id: operation.callback_method_id,
        call_target: wasm_call_target(&operation.call_target)?,
        rust_call,
        receiver,
        arguments,
        return_value,
        async_kind: wasm_async(operation.async_kind),
        throws: operation.throws.map(TypeId::index),
        callback_use_sites,
        resource_use_sites: wasm_resource_use_sites(package, operation),
        resource_hooks: operation
            .resource_hooks
            .iter()
            .copied()
            .map(wasm_resource_hook)
            .collect(),
        stream_resources: wasm_stream_groups(package, operation)?,
    })
}

fn wasm_resource_hooks() -> Result<wasm_bindgen_uniffi_engine::WasmEngineResourceHooks> {
    use wasm_bindgen_uniffi_engine::{
        WasmAsyncKind, WasmEngineResourceHook, WasmEngineResourceHooks,
    };
    Ok(WasmEngineResourceHooks {
        release_object: Some(WasmEngineResourceHook {
            rust_call: wasm_rust_path(&RustPath::new([
                "crate".to_owned(),
                "__uniffi_wasm_release_object_impl".to_owned(),
            ]))?,
            async_kind: WasmAsyncKind::Sync,
            fallible: true,
        }),
        close_output_stream: Some(WasmEngineResourceHook {
            rust_call: wasm_rust_path(&RustPath::new([
                "crate".to_owned(),
                "__uniffi_wasm_close_output_stream_impl".to_owned(),
            ]))?,
            async_kind: WasmAsyncKind::Sync,
            fallible: true,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontend::{normalize, BindingInput};
    use crate::JsConfig;
    use uniffi_bindgen::interface::ComponentInterface;
    use uniffi_bindgen::Component;

    fn fixture() -> (NormalizedPackage, RustOperationPlan, uniffi_js_abi::TypeId) {
        let ci = ComponentInterface::from_webidl(
            r#"
interface Counter {
  constructor();
  u32 get();
};

dictionary Envelope {
  Counter object;
};

[Enum]
interface Shape {
  Circle(Counter object);
};

namespace resource_paths {
  Envelope make();
};
"#,
            "resource_paths",
        )
        .expect("resource path fixture should parse");
        let mut component = Component {
            ci,
            config: JsConfig::default(),
        };
        component
            .ci
            .derive_ffi_funcs()
            .expect("resource path fixture derives ffi functions");
        let package =
            normalize(BindingInput::new(&[component])).expect("resource path fixture normalizes");
        let operation = package.rust.engines[&EngineKind::Napi]
            .operations
            .first()
            .expect("fixture has a native operation")
            .clone();
        let object_id = package
            .rust
            .named_types
            .iter()
            .find_map(|named| {
                matches!(
                    &named.kind,
                    uniffi_js_engine_schema::RustNamedTypeKind::Object { .. }
                )
                .then_some(named.id)
            })
            .expect("fixture has an object type");
        (package, operation, object_id)
    }

    fn paths_for(
        package: &NormalizedPackage,
        operation: &RustOperationPlan,
        rust_type: RustType,
        conversion: ConversionRecipe,
    ) -> Vec<Vec<ResourcePathSegment>> {
        let mut operation = operation.clone();
        operation.return_value = Some(uniffi_js_engine_schema::RustReturnBinding {
            rust_type,
            carrier: RustCarrier::LocalAdapter,
            ownership: Ownership::Owned,
            conversion,
        });
        operation_result_resource_paths(
            package,
            &operation,
            vec![ResourcePathSegment::Return],
            Ownership::Owned,
        )
        .expect("resource path expansion should succeed")
        .into_iter()
        .map(|resource| resource.path)
        .collect()
    }

    #[test]
    fn expanded_resource_paths_cover_nested_selectors_and_streams() {
        let (package, operation, object_id) = fixture();
        let object_ty = RustType::Path(RustPath {
            segments: vec!["resource_paths".into(), "Counter".into()],
        });
        let expected_root = vec![ResourcePathSegment::Return];
        assert_eq!(
            paths_for(
                &package,
                &operation,
                object_ty.clone(),
                ConversionRecipe::Object(object_id),
            ),
            vec![expected_root.clone()]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::Option(Box::new(object_ty.clone())),
                ConversionRecipe::Optional(Box::new(ConversionRecipe::Object(object_id))),
            ),
            vec![vec![
                ResourcePathSegment::Return,
                ResourcePathSegment::Optional,
            ]]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::Sequence(Box::new(object_ty.clone())),
                ConversionRecipe::Sequence(Box::new(ConversionRecipe::Object(object_id))),
            ),
            vec![vec![
                ResourcePathSegment::Return,
                ResourcePathSegment::SequenceItem,
            ]]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::Set(Box::new(object_ty.clone())),
                ConversionRecipe::Set(Box::new(ConversionRecipe::Object(object_id))),
            ),
            vec![vec![
                ResourcePathSegment::Return,
                ResourcePathSegment::SetItem
            ]]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::Map(Box::new(object_ty.clone()), Box::new(object_ty.clone())),
                ConversionRecipe::Map(
                    Box::new(ConversionRecipe::Object(object_id)),
                    Box::new(ConversionRecipe::Object(object_id)),
                ),
            ),
            vec![
                vec![ResourcePathSegment::Return, ResourcePathSegment::MapKey],
                vec![ResourcePathSegment::Return, ResourcePathSegment::MapValue],
            ]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::InputStream {
                    item: Box::new(object_ty.clone()),
                    error: Box::new(RustType::Scalar(uniffi_js_abi::ScalarType::String)),
                    is_send: false,
                },
                ConversionRecipe::InputStream {
                    item: Box::new(ConversionRecipe::Object(object_id)),
                    error: Box::new(ConversionRecipe::Identity),
                },
            ),
            vec![expected_root.clone()]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::Stream {
                    item: Box::new(object_ty.clone()),
                    error: Box::new(RustType::Scalar(uniffi_js_abi::ScalarType::String)),
                    is_send: false,
                },
                ConversionRecipe::OutputStream {
                    item: Box::new(ConversionRecipe::Object(object_id)),
                    error: Box::new(ConversionRecipe::Identity),
                },
            ),
            vec![expected_root]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::StreamStep {
                    item: Box::new(object_ty.clone()),
                    error: Box::new(object_ty.clone()),
                },
                ConversionRecipe::StreamStep {
                    item: Box::new(ConversionRecipe::Object(object_id)),
                    error: Box::new(ConversionRecipe::Object(object_id)),
                },
            ),
            vec![
                vec![ResourcePathSegment::Return, ResourcePathSegment::StreamItem],
                vec![
                    ResourcePathSegment::Return,
                    ResourcePathSegment::StreamError
                ],
            ]
        );
    }

    #[test]
    fn expanded_resource_paths_walk_record_and_enum_payload_fields() {
        let (package, operation, object_id) = fixture();
        let record_id = package
            .rust
            .named_types
            .iter()
            .find_map(|named| {
                matches!(
                    &named.kind,
                    uniffi_js_engine_schema::RustNamedTypeKind::Record { .. }
                )
                .then_some(named.id)
            })
            .expect("fixture has a record type");
        let enum_id = package
            .rust
            .named_types
            .iter()
            .find_map(|named| {
                matches!(
                    &named.kind,
                    uniffi_js_engine_schema::RustNamedTypeKind::Enum { .. }
                )
                .then_some(named.id)
            })
            .expect("fixture has an enum type");
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::Path(RustPath {
                    segments: vec!["resource_paths".into(), "Envelope".into()]
                }),
                ConversionRecipe::Record(record_id),
            ),
            vec![vec![
                ResourcePathSegment::Return,
                ResourcePathSegment::Field("object".into()),
            ]]
        );
        assert_eq!(
            paths_for(
                &package,
                &operation,
                RustType::Path(RustPath {
                    segments: vec!["resource_paths".into(), "Shape".into()]
                }),
                ConversionRecipe::Enum(enum_id),
            ),
            vec![vec![
                ResourcePathSegment::Return,
                ResourcePathSegment::Variant("Circle".into()),
                ResourcePathSegment::Field("object".into()),
            ]]
        );
        assert!(object_id.index() < package.rust.named_types.len() as u32);
    }

    #[test]
    fn wasm_plan_preserves_nested_return_resource_paths() {
        let (package, operation, object_id) = fixture();
        let mut wasm_operation = package.rust.engines[&EngineKind::WasmBindgen]
            .operations
            .iter()
            .find(|candidate| candidate.operation_id == operation.operation_id)
            .expect("fixture has the same operation in the Wasm engine")
            .clone();
        let object_ty = RustType::Path(RustPath {
            segments: vec!["resource_paths".into(), "Counter".into()],
        });
        wasm_operation.return_value = Some(uniffi_js_engine_schema::RustReturnBinding {
            rust_type: RustType::Sequence(Box::new(object_ty)),
            carrier: RustCarrier::LocalAdapter,
            ownership: Ownership::Owned,
            conversion: ConversionRecipe::Sequence(Box::new(ConversionRecipe::Object(object_id))),
        });
        let plan = wasm_operation_plan(&package, &wasm_operation)
            .expect("nested Wasm resource path should plan");
        assert!(plan.resource_use_sites.iter().any(|site| {
            site.path.segments()
                == [
                    wasm_bindgen_uniffi_engine::WasmValuePathSegment::Return,
                    wasm_bindgen_uniffi_engine::WasmValuePathSegment::SequenceItem,
                ]
                && site.type_id == object_id.index()
                && site.ownership == wasm_bindgen_uniffi_engine::WasmOwnership::Owned
        }));
    }

    #[test]
    fn native_optional_primitive_arguments_use_host_free_lowering() {
        let (mut package, operation, _) = fixture();
        let binding = RustArgumentBinding {
            public_name: "rootDir".into(),
            rust_name: "root_dir".into(),
            rust_type: RustType::Option(Box::new(RustType::Scalar(
                uniffi_js_abi::ScalarType::String,
            ))),
            carrier: RustCarrier::Primitive,
            ownership: Ownership::Owned,
            conversion: ConversionRecipe::Optional(Box::new(ConversionRecipe::Identity)),
        };

        let node = napi_argument(&package, &operation, 0, &binding, false)
            .expect("optional primitive Node argument should plan");
        assert!(matches!(
            node.binding,
            napi_uniffi_engine::ArgumentBinding::LowerWith { .. }
        ));
        let operation_id = operation.operation_id;
        package
            .rust
            .engines
            .get_mut(&EngineKind::Napi)
            .expect("fixture has a Node engine")
            .operations
            .iter_mut()
            .find(|candidate| candidate.operation_id == operation_id)
            .expect("fixture has the Node operation")
            .arguments = vec![binding.clone()];
        let node_helpers =
            render_operation_helpers_for(&package, NativeFlavor::Node, EngineKind::Napi)
                .expect("optional primitive Node helper should render");
        assert!(node_helpers.contains(&format!("fn __uniffi_lower_{}_0", operation_id.index())));

        #[cfg(feature = "ohos")]
        {
            let harmony = ohos_argument(&package, &operation, 0, &binding, false)
                .expect("optional primitive Harmony argument should plan");
            assert!(matches!(
                harmony.binding,
                napi_ohos_uniffi_engine::OhosArgumentBinding::LowerWith { .. }
            ));
            package
                .rust
                .engines
                .get_mut(&EngineKind::OhosNapi)
                .expect("fixture has a Harmony engine")
                .operations
                .iter_mut()
                .find(|candidate| candidate.operation_id == operation_id)
                .expect("fixture has the Harmony operation")
                .arguments = vec![binding];
            let harmony_helpers =
                render_operation_helpers_for(&package, NativeFlavor::Ohos, EngineKind::OhosNapi)
                    .expect("optional primitive Harmony helper should render");
            assert!(
                harmony_helpers.contains(&format!("fn __uniffi_lower_{}_0", operation_id.index()))
            );
        }
    }

    #[test]
    fn native_input_stream_arguments_use_the_typed_uniffi_stream() {
        let (package, mut operation, _) = fixture();
        let item = RustValueBinding {
            rust_type: RustType::Scalar(uniffi_js_abi::ScalarType::U32),
            carrier: RustCarrier::Primitive,
            conversion: ConversionRecipe::Identity,
        };
        let error = RustValueBinding {
            rust_type: RustType::Scalar(uniffi_js_abi::ScalarType::String),
            carrier: RustCarrier::Primitive,
            conversion: ConversionRecipe::Identity,
        };
        let binding = RustArgumentBinding {
            public_name: "points".into(),
            rust_name: "points".into(),
            rust_type: RustType::InputStream {
                item: Box::new(item.rust_type.clone()),
                error: Box::new(error.rust_type.clone()),
                is_send: true,
            },
            carrier: RustCarrier::InputStream,
            ownership: Ownership::Owned,
            conversion: ConversionRecipe::InputStream {
                item: Box::new(item.conversion.clone()),
                error: Box::new(error.conversion.clone()),
            },
        };
        operation
            .stream_resources
            .push(uniffi_js_engine_schema::RustStreamResourceGroup {
                id: uniffi_js_abi::StreamUseSiteId::new(0),
                path: uniffi_js_engine_schema::ValuePath::argument(0),
                direction: uniffi_js_engine_schema::StreamDirection::Input,
                item,
                error,
                is_send: true,
                hooks: Vec::new(),
                slot_operation_ids: Default::default(),
            });

        let node = napi_argument(&package, &operation, 0, &binding, false)
            .expect("Node input stream argument should plan");
        let napi_uniffi_engine::ArgumentBinding::InputStreamProxy { rust_type, .. } = node.binding
        else {
            panic!("Node input stream argument should use the stream proxy");
        };
        assert_eq!(
            rust_type.to_token_stream().to_string(),
            "uniffi :: UniFfiInputStream < u32 , String >"
        );

        #[cfg(feature = "ohos")]
        {
            let harmony = ohos_argument(&package, &operation, 0, &binding, false)
                .expect("Harmony input stream argument should plan");
            let napi_ohos_uniffi_engine::OhosArgumentBinding::InputStreamProxy {
                rust_type, ..
            } = harmony.binding
            else {
                panic!("Harmony input stream argument should use the stream proxy");
            };
            assert_eq!(
                rust_type.to_token_stream().to_string(),
                "uniffi :: UniFfiInputStream < u32 , String >"
            );
        }
    }

    #[test]
    fn native_bridge_escapes_rust_keyword_identifiers() {
        assert_eq!(rust_ident("type").to_string(), "r#type");
        assert_eq!(rust_ident("r#type").to_string(), "r#type");
        assert_eq!(rust_ident("self").to_string(), "__uniffi_self");
    }

    #[test]
    fn native_bigint_carriers_use_the_selected_engine_crate() {
        let binding = RustValueBinding {
            rust_type: RustType::Scalar(uniffi_js_abi::ScalarType::I64),
            carrier: RustCarrier::BigInt,
            conversion: ConversionRecipe::BigInt,
        };

        let node = napi_carrier_type_for(&binding, &[], NativeFlavor::Node)
            .expect("Node BigInt carrier should render");
        assert_eq!(
            node.to_token_stream().to_string(),
            "napi :: bindgen_prelude :: BigInt"
        );

        #[cfg(feature = "ohos")]
        {
            let harmony = napi_carrier_type_for(&binding, &[], NativeFlavor::Ohos)
                .expect("Harmony BigInt carrier should render");
            assert_eq!(
                harmony.to_token_stream().to_string(),
                "napi_ohos :: bindgen_prelude :: BigInt"
            );
        }
    }

    #[test]
    fn native_optional_bigints_use_typed_operation_helpers() {
        let (mut package, operation, _) = fixture();
        let argument = RustArgumentBinding {
            public_name: "botUserId".into(),
            rust_name: "bot_user_id".into(),
            rust_type: RustType::Option(Box::new(RustType::Scalar(uniffi_js_abi::ScalarType::U64))),
            carrier: RustCarrier::BigInt,
            ownership: Ownership::Owned,
            conversion: ConversionRecipe::Optional(Box::new(ConversionRecipe::BigInt)),
        };
        let return_value = uniffi_js_engine_schema::RustReturnBinding {
            rust_type: argument.rust_type.clone(),
            carrier: argument.carrier,
            ownership: argument.ownership,
            conversion: argument.conversion.clone(),
        };
        let operation_id = operation.operation_id;

        assert!(matches!(
            napi_argument(&package, &operation, 0, &argument, false)
                .expect("optional BigInt Node argument should plan")
                .binding,
            napi_uniffi_engine::ArgumentBinding::LowerWith { .. }
        ));
        let mut planned = operation.clone();
        planned.return_value = Some(return_value.clone());
        assert!(matches!(
            napi_operation(&package, &planned)
                .expect("optional BigInt Node return should plan")
                .return_binding,
            napi_uniffi_engine::ReturnBinding::LiftWith { .. }
        ));
        let node_operation = package
            .rust
            .engines
            .get_mut(&EngineKind::Napi)
            .expect("fixture has a Node engine")
            .operations
            .iter_mut()
            .find(|candidate| candidate.operation_id == operation_id)
            .expect("fixture has the Node operation");
        node_operation.arguments = vec![argument.clone()];
        node_operation.return_value = Some(return_value.clone());
        let node_helpers =
            render_operation_helpers_for(&package, NativeFlavor::Node, EngineKind::Napi)
                .expect("optional BigInt Node helpers should render");
        assert!(node_helpers.contains(&format!("fn __uniffi_lower_{}_0", operation_id.index())));
        assert!(node_helpers.contains(&format!("fn __uniffi_lift_{}_return", operation_id.index())));

        #[cfg(feature = "ohos")]
        {
            assert!(matches!(
                ohos_argument(&package, &operation, 0, &argument, false)
                    .expect("optional BigInt Harmony argument should plan")
                    .binding,
                napi_ohos_uniffi_engine::OhosArgumentBinding::LowerWith { .. }
            ));
            assert!(matches!(
                ohos_operation(&package, &planned)
                    .expect("optional BigInt Harmony return should plan")
                    .return_binding,
                napi_ohos_uniffi_engine::OhosReturnBinding::LiftWith { .. }
            ));
            let harmony_operation = package
                .rust
                .engines
                .get_mut(&EngineKind::OhosNapi)
                .expect("fixture has a Harmony engine")
                .operations
                .iter_mut()
                .find(|candidate| candidate.operation_id == operation_id)
                .expect("fixture has the Harmony operation");
            harmony_operation.arguments = vec![argument];
            harmony_operation.return_value = Some(return_value);
            let harmony_helpers =
                render_operation_helpers_for(&package, NativeFlavor::Ohos, EngineKind::OhosNapi)
                    .expect("optional BigInt Harmony helpers should render");
            assert!(
                harmony_helpers.contains(&format!("fn __uniffi_lower_{}_0", operation_id.index()))
            );
            assert!(harmony_helpers
                .contains(&format!("fn __uniffi_lift_{}_return", operation_id.index())));
        }
    }

    #[test]
    fn native_host_free_returns_render_operation_lift_helpers() {
        fn assert_helper(
            package: &mut NormalizedPackage,
            operation: &RustOperationPlan,
            binding: uniffi_js_engine_schema::RustReturnBinding,
        ) {
            let operation_id = operation.operation_id;
            package
                .rust
                .engines
                .get_mut(&EngineKind::Napi)
                .expect("fixture has a Node engine")
                .operations
                .iter_mut()
                .find(|candidate| candidate.operation_id == operation_id)
                .expect("fixture has the Node operation")
                .return_value = Some(binding.clone());
            let mut planned = operation.clone();
            planned.return_value = Some(binding.clone());
            assert!(matches!(
                napi_operation(package, &planned)
                    .expect("host-free Node return should plan")
                    .return_binding,
                napi_uniffi_engine::ReturnBinding::LiftWith { .. }
            ));
            let node_helpers =
                render_operation_helpers_for(package, NativeFlavor::Node, EngineKind::Napi)
                    .expect("host-free Node return helper should render");
            assert!(
                node_helpers.contains(&format!("fn __uniffi_lift_{}_return", operation_id.index()))
            );

            #[cfg(feature = "ohos")]
            {
                package
                    .rust
                    .engines
                    .get_mut(&EngineKind::OhosNapi)
                    .expect("fixture has a Harmony engine")
                    .operations
                    .iter_mut()
                    .find(|candidate| candidate.operation_id == operation_id)
                    .expect("fixture has the Harmony operation")
                    .return_value = Some(binding);
                assert!(matches!(
                    ohos_operation(package, &planned)
                        .expect("host-free Harmony return should plan")
                        .return_binding,
                    napi_ohos_uniffi_engine::OhosReturnBinding::LiftWith { .. }
                ));
                let harmony_helpers =
                    render_operation_helpers_for(package, NativeFlavor::Ohos, EngineKind::OhosNapi)
                        .expect("host-free Harmony return helper should render");
                assert!(harmony_helpers
                    .contains(&format!("fn __uniffi_lift_{}_return", operation_id.index())));
            }
        }

        let (mut package, operation, _) = fixture();
        assert_helper(
            &mut package,
            &operation,
            uniffi_js_engine_schema::RustReturnBinding {
                rust_type: RustType::Scalar(uniffi_js_abi::ScalarType::Bytes),
                carrier: RustCarrier::Bytes,
                ownership: Ownership::Owned,
                conversion: ConversionRecipe::Bytes,
            },
        );
        assert_helper(
            &mut package,
            &operation,
            uniffi_js_engine_schema::RustReturnBinding {
                rust_type: RustType::Option(Box::new(RustType::Scalar(
                    uniffi_js_abi::ScalarType::String,
                ))),
                carrier: RustCarrier::Primitive,
                ownership: Ownership::Owned,
                conversion: ConversionRecipe::Optional(Box::new(ConversionRecipe::Identity)),
            },
        );
    }

    #[cfg(feature = "ohos")]
    #[test]
    fn ohos_stream_step_returns_use_the_generated_step_carrier() {
        let (package, mut operation, _) = fixture();
        operation.return_value = Some(uniffi_js_engine_schema::RustReturnBinding {
            rust_type: RustType::StreamStep {
                item: Box::new(RustType::Scalar(uniffi_js_abi::ScalarType::String)),
                error: Box::new(RustType::Scalar(uniffi_js_abi::ScalarType::String)),
            },
            carrier: RustCarrier::StreamStep,
            ownership: Ownership::Owned,
            conversion: ConversionRecipe::StreamStep {
                item: Box::new(ConversionRecipe::Identity),
                error: Box::new(ConversionRecipe::Identity),
            },
        });

        let plan = ohos_operation(&package, &operation)
            .expect("Harmony stream-step operation should plan");
        let napi_ohos_uniffi_engine::OhosReturnBinding::LiftWith { carrier_type, .. } =
            plan.return_binding
        else {
            panic!("Harmony stream-step operation must use a typed lift helper");
        };
        assert_eq!(
            carrier_type.to_token_stream().to_string(),
            format!("__UniffiNapiStreamStep{}", operation.operation_id.index())
        );
    }
}
