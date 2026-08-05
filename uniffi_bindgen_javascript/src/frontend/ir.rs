//! Canonical JavaScript binding IR.
//!
//! This module intentionally contains only owned data.  It does not import
//! parser internals, a renderer, or a filesystem API.  The
//! normalization pass in [`super::normalize`] is the only code that turns
//! UniFFI interfaces into these values; all later stages consume this module.

use uniffi_js_abi::{ComponentId, OperationId, PublicTarget};
use uniffi_js_engine_schema::{BridgePlan, EngineKind};

// Public semantic DTOs are owned by the leaf ABI crate.  The frontend exposes
// them as its assembly surface without defining a second representation.
pub use uniffi_js_abi::{
    Capability, CapabilitySet, DefaultValue as JsDefaultValue, JsApiIr, JsArgument, JsComponent,
    JsCustomTypeConfig, JsField, JsOperation, JsReceiver, JsType, JsTypeKind, JsVariant,
    ResolvedJsConfig,
};

/// The compiler's complete public target universe.  Requested build targets
/// select output legs only; they never alter the base API IR.
pub const UNIFIED_TARGET_UNIVERSE: [PublicTarget; 3] = [
    PublicTarget::NodeNapi,
    PublicTarget::BrowserWasm,
    PublicTarget::OhosNapi,
];

// Rust bridge plan DTOs are owned by `uniffi_js_engine_schema`; the frontend
// exposes those exact types rather than defining a parallel plan.
pub use uniffi_js_engine_schema::{
    ConversionRecipe, EngineRustBridgePlan, RustArgumentBinding, RustBridgePlan, RustCallTarget,
    RustCarrier, RustEnumVariant, RustNamedTypeKind, RustNamedTypePlan, RustObjectKind,
    RustOperationPlan, RustPath, RustReceiverBinding, RustRecordField, RustResourceHook,
    RustReturnBinding, RustStreamResourceGroup, RustTupleField, RustType, RustValueBinding,
    RustVariantPayload,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RelativeFile {
    pub path: String,
    pub role: RelativeFileRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelativeFileRole {
    Source,
    Declaration,
    NativeHost,
    PlatformConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LayoutPlan {
    pub files: Vec<RelativeFile>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostPlan {
    pub component_ids: Vec<ComponentId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnginePlan {
    pub engine: EngineKind,
    pub operation_ids: Vec<OperationId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NormalizedPackage {
    pub api: JsApiIr,
    pub bridge: BridgePlan,
    pub rust: RustBridgePlan,
    pub build_targets: Vec<PublicTarget>,
    pub layout: LayoutPlan,
    pub host: HostPlan,
    pub engines: Vec<EnginePlan>,
}
