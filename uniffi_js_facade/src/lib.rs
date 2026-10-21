//! The UniFFI public JavaScript facade.
//!
//! This crate is deliberately a small leaf.  It consumes the already
//! normalised [`JsApiIr`] and [`BridgePlan`] values and never looks at a
//! `ComponentInterface`, a parser, a serializer, or the filesystem.  The
//! [`PublicAst`] value is the only semantic representation used by the Node/Web
//! printers.  ArkTS has its own strict dialect printer in [`ark`], which emits
//! the package-root implementation/declaration pair from that same AST.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use uniffi_js_abi::{
    AsyncKind, ComponentId, DefaultValue, JsApiIr, JsArgument, JsCustomTypeConfig, JsOperation,
    JsType, JsTypeKind, OperationId, OperationKind, OperationOwner, PublicTarget, ScalarType,
    TypeId, TypeSourceKey, ValueType,
};
use uniffi_js_engine_schema::{
    BridgePlan, ClosePolicy, ConversionRecipe, RustBridgePlan, RustOperationPlan,
    RustStreamResourceGroup, RustType, RustValueBinding, StreamDirection,
};
use uniffi_js_engine_schema::{
    CallbackContract, CallbackReentrancy, CallbackRetention, CallbackThreading, ValuePath,
};

mod ark;

/// The runtime is a plain ECMAScript module, not TypeScript.  Keeping it in
/// this crate makes delivery independent of a bindgen package path.
pub const RUNTIME_SOURCE: &str = uniffi_runtime_javascript::RUNTIME_SOURCE;

/// A generated file held entirely in memory until the caller writes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub role: PublicFileRole,
}

impl PublicFile {
    pub(crate) fn new(
        path: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
        role: PublicFileRole,
    ) -> Self {
        Self {
            path: path.into(),
            bytes: bytes.into(),
            role,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicFileRole {
    Implementation,
    Declaration,
    Runtime,
}

/// A target-neutral public AST.  All source names and dense IDs originate in
/// `JsApiIr`; the bridge/engine plans are retained only as validated routing
/// metadata so a printer never infers an ID or a resource slot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicAst {
    pub lifecycle: uniffi_js_abi::JsLifecycleConfig,
    /// Canonical close policy copied mechanically from `BridgePlan`.
    pub close_policy: ClosePolicy,
    pub target_universe: Vec<PublicTarget>,
    pub components: Vec<AstComponent>,
    pub types: Vec<AstType>,
    pub operations: Vec<AstOperation>,
    pub streams: Vec<AstStream>,
    pub callbacks: Vec<AstCallbackUseSite>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstComponent {
    pub id: ComponentId,
    pub namespace: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstType {
    pub id: TypeId,
    pub source_key: TypeSourceKey,
    pub name: String,
    pub kind: AstTypeKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AstTypeKind {
    Record {
        fields: Vec<AstField>,
    },
    Enum {
        variants: Vec<AstVariant>,
    },
    Error {
        variants: Vec<AstVariant>,
    },
    Custom {
        builtin: ValueType,
        config: JsCustomTypeConfig,
    },
    Object {
        kind: uniffi_js_abi::ObjectKind,
    },
    Callback,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstField {
    pub name: String,
    pub ty: ValueType,
    pub default: Option<DefaultValue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstVariant {
    pub name: String,
    pub fields: Vec<AstField>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstOperation {
    pub id: OperationId,
    pub source_key: uniffi_js_abi::OperationSourceKey,
    pub component_id: ComponentId,
    pub name: String,
    pub debug_name: String,
    pub kind: OperationKind,
    pub arguments: Vec<AstArgument>,
    pub return_type: Option<ValueType>,
    pub async_kind: AsyncKind,
    pub throws: Option<TypeSourceKey>,
    pub receiver_type: Option<TypeId>,
    /// Callback method ordinal is a canonical metadata value.  It is not
    /// recomputed from the name or from declaration order here.
    pub callback_method_id: Option<u32>,
    /// Stream resource groups and operation slots copied from the Rust plan.
    pub stream_slots: Vec<AstStreamSlot>,
    /// Canonical stream groups copied from the Rust plan, including typed
    /// item/error payloads and every lifecycle slot.  The Ark printer must
    /// consume these groups verbatim rather than discovering slots itself.
    pub stream_resources: Vec<AstStreamResource>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstArgument {
    pub name: String,
    pub ty: ValueType,
    pub default: Option<DefaultValue>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstStream {
    pub id: uniffi_js_abi::StreamUseSiteId,
    pub operation_id: OperationId,
    pub path: String,
    pub direction: uniffi_js_engine_schema::StreamDirection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstStreamSlot {
    pub kind: OperationKind,
    pub operation_id: OperationId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstStreamResource {
    pub id: uniffi_js_abi::StreamUseSiteId,
    pub path: ValuePath,
    pub direction: StreamDirection,
    pub item: ValueType,
    pub error: Option<ValueType>,
    pub is_send: bool,
    pub slot_operation_ids: BTreeMap<OperationKind, OperationId>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AstCallbackUseSite {
    pub operation_id: OperationId,
    pub callback_type: TypeId,
    pub path: ValuePath,
    pub contract: CallbackContract,
}

/// The complete output from one facade build.  `shared_inventory` is the
/// canonical Node/Web pair; both targets return these same path/byte values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicFacade {
    pub ast: PublicAst,
    pub shared_inventory: Vec<PublicFile>,
    pub ark_inventory: Vec<PublicFile>,
}

impl PublicFacade {
    pub fn files(&self, target: PublicTarget) -> &[PublicFile] {
        match target {
            PublicTarget::NodeNapi | PublicTarget::BrowserWasm => &self.shared_inventory,
            PublicTarget::OhosNapi => &self.ark_inventory,
        }
    }

    /// Alias useful to callers that treat Node and browser as one source
    /// family.  No rendering occurs here: bytes were created at build time.
    pub fn shared_files(&self) -> &[PublicFile] {
        &self.shared_inventory
    }

    pub fn ark_files(&self) -> &[PublicFile] {
        &self.ark_inventory
    }
}

/// Build the public facade from canonical normalized inputs.
pub fn build(
    api: &JsApiIr,
    bridge: &BridgePlan,
    rust: &RustBridgePlan,
) -> Result<PublicFacade, FacadeError> {
    FacadeBuilder::new(api, bridge, rust).build()
}

pub struct FacadeBuilder<'a> {
    api: &'a JsApiIr,
    bridge: &'a BridgePlan,
    rust: &'a RustBridgePlan,
}

impl<'a> FacadeBuilder<'a> {
    pub fn new(api: &'a JsApiIr, bridge: &'a BridgePlan, rust: &'a RustBridgePlan) -> Self {
        Self { api, bridge, rust }
    }

    pub fn build(&self) -> Result<PublicFacade, FacadeError> {
        let ast = self.build_ast()?;
        let shared_inventory = render_inventory(&ast)?;
        let ark_inventory = render_ark_inventory(&ast)?;
        Ok(PublicFacade {
            ast,
            shared_inventory,
            ark_inventory,
        })
    }

    pub fn build_ast(&self) -> Result<PublicAst, FacadeError> {
        validate_unique_ids(self.api)?;

        let api_operation_ids: BTreeSet<_> = self.api.operations.iter().map(|op| op.id).collect();
        let bridge_operation_ids: BTreeSet<_> = self
            .bridge
            .operations()
            .iter()
            .map(|op| op.operation.id)
            .collect();
        for id in &api_operation_ids {
            if !bridge_operation_ids.contains(id) {
                return Err(FacadeError::MissingBridgeOperation { id: *id });
            }
        }

        let components = self
            .api
            .components
            .iter()
            .map(|component| {
                ensure_path_segment(&component.public_namespace, "component namespace")?;
                Ok(AstComponent {
                    id: component.id,
                    namespace: component.public_namespace.clone(),
                })
            })
            .collect::<Result<Vec<_>, FacadeError>>()?;

        let types = self
            .api
            .types
            .iter()
            .map(|ty| self.ast_type(ty))
            .collect::<Result<Vec<_>, FacadeError>>()?;

        let rust_operations = rust_operation_index(self.rust)?;
        let operations = self
            .api
            .operations
            .iter()
            .map(|operation| self.ast_operation(operation, &rust_operations))
            .collect::<Result<Vec<_>, FacadeError>>()?;

        let streams = self
            .bridge
            .streams()
            .iter()
            .map(|stream| AstStream {
                id: stream.id,
                operation_id: stream.operation_id,
                path: stream.path.to_string(),
                direction: stream.contract.direction,
            })
            .collect();

        let callbacks = self
            .bridge
            .callbacks()
            .iter()
            .map(|callback| AstCallbackUseSite {
                operation_id: callback.operation_id,
                callback_type: callback.callback_type,
                path: callback.path.clone(),
                contract: callback.contract,
            })
            .collect();

        Ok(PublicAst {
            lifecycle: self.api.resolved_config.lifecycle.clone(),
            close_policy: self.bridge.close_policy(),
            target_universe: self.api.target_universe.clone(),
            components,
            types,
            operations,
            streams,
            callbacks,
        })
    }

    fn ast_type(&self, ty: &JsType) -> Result<AstType, FacadeError> {
        ensure_identifier(&ty.public_name, "public type name")?;
        let kind = match &ty.kind {
            JsTypeKind::Record { fields } => AstTypeKind::Record {
                fields: fields.iter().map(ast_field).collect::<Result<_, _>>()?,
            },
            JsTypeKind::Enum { variants } => AstTypeKind::Enum {
                variants: variants.iter().map(ast_variant).collect::<Result<_, _>>()?,
            },
            JsTypeKind::Error { variants } => AstTypeKind::Error {
                variants: variants.iter().map(ast_variant).collect::<Result<_, _>>()?,
            },
            JsTypeKind::Custom { builtin, config } => AstTypeKind::Custom {
                builtin: builtin.clone(),
                config: config.clone(),
            },
            JsTypeKind::Object { kind } => AstTypeKind::Object { kind: *kind },
            JsTypeKind::Callback => AstTypeKind::Callback,
        };
        Ok(AstType {
            id: ty.id,
            source_key: ty.source_key.clone(),
            name: ty.public_name.clone(),
            kind,
        })
    }

    fn ast_operation(
        &self,
        operation: &JsOperation,
        rust_operations: &BTreeMap<OperationId, &RustOperationPlan>,
    ) -> Result<AstOperation, FacadeError> {
        ensure_identifier(&operation.public_name, "public operation name")?;
        if operation.kind == OperationKind::CallbackMethod && operation.callback_method_id.is_none()
        {
            return Err(FacadeError::MissingCallbackMethodId { id: operation.id });
        }
        let rust = rust_operations
            .get(&operation.id)
            .ok_or(FacadeError::MissingRustOperation { id: operation.id })?;
        let stream_slots = rust
            .stream_resources
            .iter()
            .flat_map(|resource| {
                resource
                    .slot_operation_ids
                    .iter()
                    .map(|(kind, id)| AstStreamSlot {
                        kind: *kind,
                        operation_id: *id,
                    })
            })
            .collect();
        let stream_resources = rust
            .stream_resources
            .iter()
            .map(|resource| ast_stream_resource(resource, &self.api.types))
            .collect::<Result<Vec<_>, _>>()?;
        if operation.callback_method_id != rust.callback_method_id {
            return Err(FacadeError::CallbackMethodIdMismatch { id: operation.id });
        }
        Ok(AstOperation {
            id: operation.id,
            source_key: operation.source_key.clone(),
            component_id: operation.component_id,
            name: operation.public_name.clone(),
            debug_name: operation.debug_name.clone(),
            kind: operation.kind,
            arguments: operation
                .arguments
                .iter()
                .map(ast_argument)
                .collect::<Result<_, _>>()?,
            return_type: operation.return_type.clone(),
            async_kind: rust.async_kind,
            throws: operation.throws.clone(),
            receiver_type: if operation.kind == OperationKind::Constructor {
                None
            } else {
                operation
                    .receiver
                    .as_ref()
                    .map(|receiver| receiver.object_type)
            },
            callback_method_id: rust.callback_method_id,
            stream_slots,
            stream_resources,
        })
    }
}

fn ast_stream_resource(
    resource: &RustStreamResourceGroup,
    types: &[JsType],
) -> Result<AstStreamResource, FacadeError> {
    Ok(AstStreamResource {
        id: resource.id,
        path: resource.path.clone(),
        direction: resource.direction,
        item: public_stream_value_type(&resource.item, types).ok_or_else(|| {
            FacadeError::InvalidStreamBinding {
                path: resource.path.to_string(),
            }
        })?,
        error: if is_infallible_rust_type(&resource.error.rust_type) {
            None
        } else {
            Some(
                public_stream_value_type(&resource.error, types).ok_or_else(|| {
                    FacadeError::InvalidStreamBinding {
                        path: resource.path.to_string(),
                    }
                })?,
            )
        },
        is_send: resource.is_send,
        slot_operation_ids: resource.slot_operation_ids.clone(),
    })
}

fn is_infallible_rust_type(ty: &RustType) -> bool {
    matches!(ty, RustType::Path(path) if path.segments == ["std", "convert", "Infallible"])
}

fn public_stream_value_type(binding: &RustValueBinding, types: &[JsType]) -> Option<ValueType> {
    fn named_type(types: &[JsType], id: TypeId) -> Option<ValueType> {
        types
            .iter()
            .find(|ty| ty.id == id)
            .map(|ty| ValueType::Named(ty.source_key.clone()))
    }

    fn from_conversion(
        conversion: &ConversionRecipe,
        rust_type: &RustType,
        types: &[JsType],
    ) -> Option<ValueType> {
        match conversion {
            ConversionRecipe::Identity
            | ConversionRecipe::Timestamp
            | ConversionRecipe::Duration
            | ConversionRecipe::BigInt
            | ConversionRecipe::Bytes => from_rust_type(rust_type),
            ConversionRecipe::Optional(inner) => Some(ValueType::Optional(Box::new(
                from_conversion(inner, rust_type_inner(rust_type), types)?,
            ))),
            ConversionRecipe::Sequence(inner) => Some(ValueType::Sequence(Box::new(
                from_conversion(inner, rust_type_inner(rust_type), types)?,
            ))),
            ConversionRecipe::Map(key, value) => {
                let (rust_key, rust_value) = match rust_type {
                    RustType::Map(rust_key, rust_value) => (rust_key.as_ref(), rust_value.as_ref()),
                    _ => (rust_type, rust_type),
                };
                Some(ValueType::Map(
                    Box::new(from_conversion(key, rust_key, types)?),
                    Box::new(from_conversion(value, rust_value, types)?),
                ))
            }
            ConversionRecipe::Set(inner) => Some(ValueType::Set(Box::new(from_conversion(
                inner,
                rust_type_inner(rust_type),
                types,
            )?))),
            ConversionRecipe::Record(id)
            | ConversionRecipe::Enum(id)
            | ConversionRecipe::Error(id)
            | ConversionRecipe::Object(id)
            | ConversionRecipe::Callback(id)
            | ConversionRecipe::Custom(id, _) => named_type(types, *id),
            ConversionRecipe::InputStream { item, error } => {
                let RustType::InputStream {
                    item: rust_item,
                    error: rust_error,
                    is_send,
                } = rust_type
                else {
                    return None;
                };
                Some(ValueType::InputStream {
                    item: Box::new(from_conversion(item, rust_item, types)?),
                    error: if is_infallible_rust_type(rust_error) {
                        None
                    } else {
                        Some(Box::new(from_conversion(error, rust_error, types)?))
                    },
                    is_send: *is_send,
                })
            }
            ConversionRecipe::OutputStream { item, error } => {
                let RustType::Stream {
                    item: rust_item,
                    error: rust_error,
                    is_send,
                } = rust_type
                else {
                    return None;
                };
                Some(ValueType::OutputStream {
                    item: Box::new(from_conversion(item, rust_item, types)?),
                    error: if is_infallible_rust_type(rust_error) {
                        None
                    } else {
                        Some(Box::new(from_conversion(error, rust_error, types)?))
                    },
                    is_send: *is_send,
                })
            }
            ConversionRecipe::StreamStep { .. } => None,
        }
    }

    fn rust_type_inner(rust_type: &RustType) -> &RustType {
        match rust_type {
            RustType::Option(inner)
            | RustType::Sequence(inner)
            | RustType::Set(inner)
            | RustType::Custom(inner) => inner,
            RustType::Stream { item, .. } | RustType::InputStream { item, .. } => item,
            RustType::Map(_, value) => value,
            _ => rust_type,
        }
    }

    fn from_rust_type(rust_type: &RustType) -> Option<ValueType> {
        match rust_type {
            RustType::Scalar(scalar) => Some(ValueType::Scalar(*scalar)),
            RustType::Timestamp => Some(ValueType::Timestamp),
            RustType::Duration => Some(ValueType::Duration),
            RustType::Option(inner) => Some(ValueType::Optional(Box::new(from_rust_type(inner)?))),
            RustType::Sequence(inner) => {
                Some(ValueType::Sequence(Box::new(from_rust_type(inner)?)))
            }
            RustType::Map(key, value) => Some(ValueType::Map(
                Box::new(from_rust_type(key)?),
                Box::new(from_rust_type(value)?),
            )),
            RustType::Set(inner) => Some(ValueType::Set(Box::new(from_rust_type(inner)?))),
            RustType::Stream {
                item,
                error,
                is_send,
            } => Some(ValueType::OutputStream {
                item: Box::new(from_rust_type(item)?),
                error: if is_infallible_rust_type(error) {
                    None
                } else {
                    Some(Box::new(from_rust_type(error)?))
                },
                is_send: *is_send,
            }),
            RustType::InputStream {
                item,
                error,
                is_send,
            } => Some(ValueType::InputStream {
                item: Box::new(from_rust_type(item)?),
                error: if is_infallible_rust_type(error) {
                    None
                } else {
                    Some(Box::new(from_rust_type(error)?))
                },
                is_send: *is_send,
            }),
            RustType::Custom(inner) => from_rust_type(inner),
            RustType::Unit | RustType::Path(_) | RustType::StreamStep { .. } => None,
        }
    }

    from_conversion(&binding.conversion, &binding.rust_type, types)
}

fn ast_field(field: &uniffi_js_abi::JsField) -> Result<AstField, FacadeError> {
    ensure_identifier(&field.public_name, "field name")?;
    Ok(AstField {
        name: field.public_name.clone(),
        ty: field.ty.clone(),
        default: field.default.clone(),
    })
}

fn ast_variant(variant: &uniffi_js_abi::JsVariant) -> Result<AstVariant, FacadeError> {
    ensure_identifier(&variant.public_name, "variant name")?;
    Ok(AstVariant {
        name: variant.public_name.clone(),
        fields: variant
            .fields
            .iter()
            .map(ast_field)
            .collect::<Result<_, _>>()?,
    })
}

fn ast_argument(argument: &JsArgument) -> Result<AstArgument, FacadeError> {
    ensure_identifier(&argument.public_name, "argument name")?;
    Ok(AstArgument {
        name: argument.public_name.clone(),
        ty: argument.ty.clone(),
        default: argument.default.clone(),
    })
}

fn rust_operation_index(
    rust: &RustBridgePlan,
) -> Result<BTreeMap<OperationId, &RustOperationPlan>, FacadeError> {
    let mut result: BTreeMap<OperationId, &RustOperationPlan> = BTreeMap::new();
    let mut expected_ids: Option<BTreeSet<OperationId>> = None;
    for engine in rust.engines.values() {
        let current_ids: BTreeSet<_> = engine
            .operations
            .iter()
            .map(|operation| operation.operation_id)
            .collect();
        if let Some(expected) = &expected_ids {
            if expected != &current_ids {
                return Err(FacadeError::InconsistentEngineStreamProjection {
                    id: current_ids
                        .symmetric_difference(expected)
                        .next()
                        .copied()
                        .unwrap_or(OperationId::new(0)),
                });
            }
        } else {
            expected_ids = Some(current_ids);
        }
        for operation in &engine.operations {
            if let Some(previous) = result.get(&operation.operation_id) {
                if previous.stream_resources != operation.stream_resources {
                    return Err(FacadeError::InconsistentEngineStreamProjection {
                        id: operation.operation_id,
                    });
                }
            } else {
                result.insert(operation.operation_id, operation);
            }
        }
    }
    Ok(result)
}

fn validate_unique_ids(api: &JsApiIr) -> Result<(), FacadeError> {
    let mut components = BTreeSet::new();
    for component in &api.components {
        if !components.insert(component.id) {
            return Err(FacadeError::DuplicateId {
                table: "component",
                id: component.id.index(),
            });
        }
    }
    let mut types = BTreeSet::new();
    for ty in &api.types {
        if !types.insert(ty.id) {
            return Err(FacadeError::DuplicateId {
                table: "type",
                id: ty.id.index(),
            });
        }
    }
    let mut operations = BTreeSet::new();
    for operation in &api.operations {
        if !operations.insert(operation.id) {
            return Err(FacadeError::DuplicateId {
                table: "operation",
                id: operation.id.index(),
            });
        }
    }
    Ok(())
}

fn ensure_identifier(value: &str, role: &'static str) -> Result<(), FacadeError> {
    if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || value.chars().any(|character| character.is_whitespace())
    {
        return Err(FacadeError::UnsafeName {
            role,
            value: value.to_owned(),
        });
    }
    Ok(())
}

fn ensure_path_segment(value: &str, role: &'static str) -> Result<(), FacadeError> {
    ensure_identifier(value, role)?;
    if value == "." || value == ".." {
        return Err(FacadeError::UnsafeName {
            role,
            value: value.to_owned(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FacadeError {
    DuplicateId { table: &'static str, id: u32 },
    MissingBridgeOperation { id: OperationId },
    MissingRustOperation { id: OperationId },
    InconsistentEngineStreamProjection { id: OperationId },
    MissingCallbackMethodId { id: OperationId },
    CallbackMethodIdMismatch { id: OperationId },
    DuplicateCallbackMethodId { id: OperationId },
    InvalidStreamBinding { path: String },
    MissingStreamSlot { operation: OperationId },
    AmbiguousStreamResource { operation: OperationId },
    UnknownType { key: TypeSourceKey },
    UnsafeName { role: &'static str, value: String },
    DuplicatePath { path: String },
    UnsupportedArkImport { import: String },
}

impl fmt::Display for FacadeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateId { table, id } => write!(formatter, "duplicate {table} id {id}"),
            Self::MissingBridgeOperation { id } => {
                write!(formatter, "operation {id} is absent from BridgePlan")
            }
            Self::MissingRustOperation { id } => {
                write!(formatter, "operation {id} is absent from RustBridgePlan")
            }
            Self::InconsistentEngineStreamProjection { id } => write!(
                formatter,
                "operation {id} has inconsistent stream projection across engines"
            ),
            Self::MissingCallbackMethodId { id } => {
                write!(
                    formatter,
                    "callback method operation {id} has no canonical method ID"
                )
            }
            Self::CallbackMethodIdMismatch { id } => {
                write!(
                    formatter,
                    "operation {id} has mismatched callback method IDs"
                )
            }
            Self::DuplicateCallbackMethodId { id } => {
                write!(
                    formatter,
                    "callback method operation {id} reuses a method ID"
                )
            }
            Self::InvalidStreamBinding { path } => {
                write!(
                    formatter,
                    "stream use site {path} has no public value binding"
                )
            }
            Self::MissingStreamSlot { operation } => {
                write!(
                    formatter,
                    "stream operation {operation} has no canonical resource"
                )
            }
            Self::AmbiguousStreamResource { operation } => {
                write!(
                    formatter,
                    "stream operation {operation} has ambiguous resources"
                )
            }
            Self::UnknownType { key } => write!(formatter, "unknown named type {key}"),
            Self::UnsafeName { role, value } => {
                write!(formatter, "unsafe {role} {value:?}")
            }
            Self::DuplicatePath { path } => write!(formatter, "duplicate generated path {path}"),
            Self::UnsupportedArkImport { import } => {
                write!(
                    formatter,
                    "ArkTS custom import cannot be delivered in package root: {import:?}"
                )
            }
        }
    }
}

impl Error for FacadeError {}

fn render_inventory(ast: &PublicAst) -> Result<Vec<PublicFile>, FacadeError> {
    let mut files = Vec::new();
    let runtime_extension = "js";
    let runtime_declaration_extension = "d.ts";
    files.push(PublicFile::new(
        format!("shared/uniffi_runtime.{runtime_extension}"),
        RUNTIME_SOURCE.as_bytes().to_vec(),
        PublicFileRole::Runtime,
    ));
    files.push(PublicFile::new(
        format!("shared/uniffi_runtime.{runtime_declaration_extension}"),
        runtime_declaration_source().into_bytes(),
        PublicFileRole::Declaration,
    ));
    let mut paths = BTreeSet::new();
    for component in &ast.components {
        let implementation_extension = "js";
        let declaration_extension = "d.ts";
        let base = format!("components/{}/index", component.namespace);
        let implementation = render_component_implementation(ast, component, runtime_extension)?;
        let declaration = render_component_declaration(ast, component)?;
        let implementation_path = format!("{base}.{implementation_extension}");
        let declaration_path = format!("{base}.{declaration_extension}");
        if !paths.insert(implementation_path.clone()) {
            return Err(FacadeError::DuplicatePath {
                path: implementation_path,
            });
        }
        if !paths.insert(declaration_path.clone()) {
            return Err(FacadeError::DuplicatePath {
                path: declaration_path,
            });
        }
        let exports = std::iter::once("UniffiError".to_owned())
            .chain(
                ast.types
                    .iter()
                    .filter(|ty| ty.source_key.component().namespace() == component.namespace)
                    .filter(|ty| {
                        matches!(
                            ty.kind,
                            AstTypeKind::Object { .. }
                                | AstTypeKind::Error { .. }
                                | AstTypeKind::Enum { .. }
                        )
                    })
                    .map(|ty| ty.name.clone()),
            )
            .collect::<Vec<_>>()
            .join(", ");
        files.push(PublicFile::new(
            format!("components/{}/internal.js", component.namespace),
            implementation.into_bytes(),
            PublicFileRole::Implementation,
        ));
        files.push(PublicFile::new(
            format!("components/{}/internal.d.ts", component.namespace),
            b"export * from \"./index.js\";\nexport declare function createNamespace(session: unknown): import(\"./index.js\").Namespace;\n".to_vec(),
            PublicFileRole::Declaration,
        ));
        files.push(PublicFile::new(
            implementation_path,
            format!("export {{ {exports} }} from \"./internal.js\";\n").into_bytes(),
            PublicFileRole::Implementation,
        ));
        files.push(PublicFile::new(
            declaration_path,
            declaration.into_bytes(),
            PublicFileRole::Declaration,
        ));
    }
    Ok(files)
}

/// ArkTS is emitted by the dedicated strict printer in [`ark`].
fn render_ark_inventory(ast: &PublicAst) -> Result<Vec<PublicFile>, FacadeError> {
    ark::render_inventory(ast)
}

fn runtime_declaration_source() -> String {
    r#"export interface CallOptions { signal?: AbortSignal; }
export interface BackendSessionOptions { callControl: { create: number; cancel: number; release: number; operations: number[] }; }
export class BackendSession {
  constructor(backend: unknown, host: Host | undefined, options: BackendSessionOptions);
  readonly backend: unknown;
  readonly host: Host;
  invokeSync(operationId: number, args: unknown[]): unknown;
  invokeAsync(operationId: number, args: unknown[], options?: CallOptions): Promise<unknown>;
  releaseObject(handle: unknown): void;
  registerCallback(callbackType: number, callback: unknown, contract?: unknown): number;
  retainCallback(callbackType: number, callbackId: number): { release(): void };
  releaseCallback(callbackType: number, callbackId: number): void;
  invokeCallbackSync(callbackType: number, callbackId: number, methodId: number, args?: unknown[]): unknown;
  invokeCallbackAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: unknown[]): Promise<unknown>;
  pullInputStream(handle: unknown): Promise<unknown>;
  cancelInputStream(handle: unknown): Promise<void>;
  releaseInputStream(handle: unknown): Promise<void>;
  cancelOutputStream(handle: unknown): Promise<void>;
  releaseOutputStream(handle: unknown): void;
  createInputStream(source: unknown, options?: unknown): UniFfiInputStream<unknown>;
  createOutputStream(options?: unknown): UniFfiStream<unknown>;
  close(): Promise<void>;
  environmentDestroyed(): void;
}
export class Host {
  retainCallback(callbackType: number, callbackId: number): void;
  releaseCallback(callbackType: number, callbackId: number): void;
  invokeCallbackSync(callbackType: number, callbackId: number, methodId: number, args?: unknown[]): unknown;
  invokeCallbackAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: unknown[]): Promise<unknown>;
  invokeCallbackSyncResult(callbackType: number, callbackId: number, methodId: number, args?: unknown[]): { ok: boolean; value?: unknown; error?: unknown };
  invokeCallbackAsyncResult(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: unknown[]): Promise<{ ok: boolean; value?: unknown; error?: unknown }>;
  pullInputStream(handle: unknown): Promise<unknown>;
  cancelInputStream(handle: unknown): Promise<void>;
  releaseInputStream(handle: unknown): void;
  cancelOutputStream(handle: unknown): Promise<void>;
  releaseOutputStream(handle: unknown): void;
  registerInputStream(source: unknown, options?: unknown): UniFfiInputStream<unknown>;
  releaseObject(handle: unknown): void;
}
export class CallbackRegistry {
  register(callbackType: number, callback: unknown, contract?: unknown): number;
  retain(callbackType: number, callbackId: number): { release(): void };
  release(callbackType: number, callbackId: number): void;
  invokeSync(callbackType: number, callbackId: number, methodId: number, args?: unknown[]): unknown;
  invokeAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: unknown[]): Promise<unknown>;
}
export class UniffiError extends Error {
  readonly errorName: string;
  readonly variant: string | null;
  readonly data: unknown;
  readonly descriptor: unknown;
}
export interface UniFfiStream<T> { next(): Promise<IteratorResult<T>>; cancel(): Promise<void>; [Symbol.asyncIterator](): AsyncIterator<T>; }
export interface UniFfiInputStream<T> { next(): Promise<IteratorResult<T>>; cancel?(): Promise<void>; release?(): void; [Symbol.asyncIterator](): AsyncIterator<T>; }
export declare function createBackendSession(factory: (host: Host) => unknown, host: Host | undefined, options: BackendSessionOptions): BackendSession;
export declare function disposeObject(object: object): void;
export declare function createFacade(session: BackendSession, descriptors: unknown): Record<string, unknown>;
export declare function invokeOperation(session: BackendSession, descriptor: unknown, args?: unknown[]): unknown;
export declare function invokeObjectOperation(lease: object, descriptor: unknown, args?: unknown[]): unknown;
export declare function errorDescriptor(raw: unknown): unknown;
export declare function asUniffiError(raw: unknown): UniffiError;
export declare function lowerValue(value: unknown, descriptor: unknown, context?: unknown, session?: BackendSession | null, operation?: unknown, path?: string | null): unknown;
export declare function liftValue(value: unknown, descriptor: unknown, session?: BackendSession | null, context?: unknown, operation?: unknown): unknown;
"#
    .to_owned()
}

fn render_component_implementation(
    ast: &PublicAst,
    component: &AstComponent,
    runtime_extension: &str,
) -> Result<String, FacadeError> {
    // The descriptor registry is package-wide.  Keep every operation in the
    // descriptor table so a foreign component can reuse its owner's object
    // class/factory instead of synthesising a shadow method table.  Namespace
    // and value surfaces are still filtered by component in createFacade.
    let all_operations: Vec<_> = ast.operations.iter().collect();
    let mut out = String::new();
    out.push_str("// AUTOGENERATED UniFFI public facade; ECMAScript, never TypeScript.\n");
    out.push_str(&format!(
        "import {{ createFacade, invokeOperation, invokeObjectOperation, UniffiError }} from \"../../shared/uniffi_runtime.{runtime_extension}\";\n"
    ));
    let mut runtime_imports = BTreeSet::new();
    for ty in ast.types.iter() {
        if let AstTypeKind::Custom { config, .. } = &ty.kind {
            for import in &config.imports {
                runtime_imports.insert(render_runtime_import_line(import, runtime_extension)?);
            }
        }
    }
    for import in runtime_imports {
        out.push_str(&import);
    }
    out.push('\n');
    out.push_str("const __objectClasses = Object.create(null);\n");
    out.push_str("const __errorClasses = Object.create(null);\n");
    out.push_str("const __descriptors = ");
    out.push_str(&render_descriptors(ast, component, &all_operations));
    out.push_str(";\n");
    out.push_str("__descriptors.classes = __objectClasses;\n");
    out.push_str("__descriptors.errorClasses = __errorClasses;\n");
    out.push_str("\nexport function createNamespace(session) { return createFacade(session, __descriptors); }\n");
    out.push('\n');

    for ty in ast.types.iter() {
        if let AstTypeKind::Object { .. } = ty.kind {
            let local = ty.source_key.component().namespace() == component.namespace;
            let class_name = if local {
                ty.name.clone()
            } else {
                format!("__Object{}", ty.id.index())
            };
            let export_prefix = if local { "export " } else { "" };
            out.push_str(&format!(
                "{}class {} {{ constructor() {{ throw new UniffiError({{ errorName: \"UniffiObjectConstructor\", message: \"object wrappers are created by the facade\" }}); }}",
                export_prefix,
                class_name
            ));
            for operation in ast
                .operations
                .iter()
                .filter(|operation| operation.receiver_type == Some(ty.id))
            {
                let args = operation
                    .arguments
                    .iter()
                    .map(|argument| argument.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                let args = append_component_call_options(ast, operation, args, false);
                out.push_str(&format!(
                    "{}({}) {{ return invokeObjectOperation(this, __descriptors.operations[{}], [{}]); }}\n",
                    operation.name,
                    args,
                    operation.id.index(),
                    args
                ));
            }
            out.push_str("}\n");
            if let Some(close_id) = ast.lifecycle.close_methods.get(&ty.id) {
                let method = &ast
                    .operations
                    .iter()
                    .find(|op| op.id == *close_id)
                    .expect("close operation")
                    .name;
                out.push_str(&format!("if (Symbol.asyncDispose) Object.defineProperty({class_name}.prototype, Symbol.asyncDispose, {{ value: {class_name}.prototype.{method} }});\n"));
            }
            out.push_str(&format!(
                "__objectClasses[{}] = {{ typeName: \"{}\", typeKey: \"{}\", public: {}, publicClass: {}, resource: {} }};\n",
                ty.id.index(),
                escape_js(&ty.name),
                escape_js(&ty.source_key.to_string()),
                local,
                class_name,
                ast.lifecycle.close_methods.contains_key(&ty.id)
            ));
        } else if let AstTypeKind::Error { .. } = ty.kind {
            let local = ty.source_key.component().namespace() == component.namespace;
            let class_name = if local {
                ty.name.clone()
            } else {
                format!("__Error{}", ty.id.index())
            };
            out.push_str(&format!(
                "{}class {} extends UniffiError {{ constructor(message = \"\", variant = null, data = null) {{ super({{ message, errorName: \"{}\", variant, data }}); this.name = \"{}\"; }} }}\n__errorClasses[{}] = {};\n",
                if local { "export " } else { "" },
                class_name,
                escape_js(&ty.name),
                escape_js(&ty.name),
                ty.id.index(),
                class_name
            ));
        } else if ty.source_key.component().namespace() == component.namespace {
            if let AstTypeKind::Enum { ref variants } = ty.kind {
                out.push_str(&format!("export const {} = Object.freeze({{", ty.name));
                if variants.iter().all(|variant| variant.fields.is_empty()) {
                    for variant in variants {
                        out.push_str(&format!("{}: \"{}\",", variant.name, variant.name));
                    }
                } else {
                    for variant in variants {
                        let fields = variant
                            .fields
                            .iter()
                            .map(|field| format!("{}: value.{}", field.name, field.name))
                            .collect::<Vec<_>>()
                            .join(", ");
                        out.push_str(&format!(
                            "{}: (value) => {{ if (!value || typeof value !== \"object\" || Array.isArray(value)) throw new UniffiError({{ errorName: \"UniffiEnumPayload\", message: \"enum variant payload is required\" }}); return {{ tag: \"{}\"{} }}; }},",
                            variant.name,
                            variant.name,
                            if fields.is_empty() {
                                String::new()
                            } else {
                                format!(", {fields}")
                            }
                        ));
                    }
                }
                out.push_str("});\n");
            }
        }
    }
    out.push_str("export { UniffiError };\n");
    Ok(out)
}

fn render_descriptors(
    ast: &PublicAst,
    component: &AstComponent,
    operations: &[&AstOperation],
) -> String {
    let mut out = format!(
        "{{ closePolicy:{{graceMs:{},onDeadline:\"detach\"}}, componentId:{}, types: {{",
        ast.close_policy.grace_ms,
        component.id.index()
    );
    for ty in &ast.types {
        out.push_str(&format!(
            "\"{}\":{},",
            ty.id.index(),
            render_type_descriptor(ast, ty)
        ));
    }
    out.push_str("}, typeNames:{");
    out.push_str(
        &ast.types
            .iter()
            .filter(|ty| ty.source_key.component().namespace() == component.namespace)
            .map(|ty| format!("\"{}\":{}", escape_js(&ty.name), ty.id.index()))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push_str("}, publicTypes:[");
    out.push_str(
        &ast.types
            .iter()
            .filter(|ty| ty.source_key.component().namespace() == component.namespace)
            .map(|ty| format!("\"{}\"", escape_js(&ty.name)))
            .collect::<Vec<_>>()
            .join(","),
    );
    out.push_str("], values: {");
    for ty in &ast.types {
        let methods = operations
            .iter()
            .filter(|operation| {
                matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
                    && operation.receiver_type == Some(ty.id)
                    || matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
                        && operation.kind == OperationKind::Constructor
            })
            .map(|operation| format!("\"{}\":{}", escape_js(&operation.name), operation.id.index()))
            .collect::<Vec<_>>();
        if !methods.is_empty() {
            out.push_str(&format!(
                "\"{}\":{{{}}},",
                escape_js(&ty.name),
                methods.join(",")
            ));
        }
    }
    out.push_str("}, operations: {");
    for operation in operations {
        let owner_surface = operation_owner_surface(operation);
        let surface = operation_surface(operation);
        let owner_type = operation_owner_type_key(operation.source_key.owner())
            .and_then(|key| {
                ast.types
                    .iter()
                    .find(|candidate| candidate.source_key == *key)
            })
            .map(|candidate| format!("\"{}\"", escape_js(&candidate.name)))
            .unwrap_or_else(|| "null".to_owned());
        let owner_type_id = operation_owner_type_key(operation.source_key.owner())
            .and_then(|key| {
                ast.types
                    .iter()
                    .find(|candidate| candidate.source_key == *key)
            })
            .map(|candidate| candidate.id.index().to_string())
            .unwrap_or_else(|| "null".to_owned());
        out.push_str(&format!(
            "\"{}\":{{componentId:{},name:\"{}\",id:{},kind:\"{:?}\",async:{},receiver:{},receiverType:{},receiverTypeId:{},surface:\"{}\",ownerSurface:\"{}\",ownerType:{},ownerTypeId:{},valueSelf:{},public:{},objectType:{},resourceClose:{},owned:{},cached:{},args:[",
            operation.id.index(),
            operation.component_id.index(),
            escape_js(&operation.name),
            operation.id.index(),
            operation.kind,
            operation.async_kind == AsyncKind::Async,
            operation.receiver_type.is_some(),
            operation
                .receiver_type
                .and_then(|receiver| ast.types.iter().find(|candidate| candidate.id == receiver))
                .map(|candidate| format!("\"{}\"", escape_js(&candidate.name)))
                .unwrap_or_else(|| "null".to_owned()),
            operation
                .receiver_type
                .map(|receiver| receiver.index().to_string())
                .unwrap_or_else(|| "null".to_owned()),
            surface,
            owner_surface,
            owner_type,
            owner_type_id,
            owner_surface == "value" && operation.kind != OperationKind::Constructor,
            !matches!(operation.source_key.owner(), OperationOwner::Callback(_)),
            operation
                .return_type
                .as_ref()
                .and_then(|ty| match ty {
                    ValueType::Named(key) => ast
                        .types
                        .iter()
                        .find(|candidate| candidate.source_key == *key
                            && matches!(candidate.kind, AstTypeKind::Object { .. }))
                        .map(|candidate| candidate.id.index().to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "null".to_owned()),
            ast.lifecycle.close_methods.values().any(|id| *id == operation.id),
            ast.lifecycle.owned_methods.contains(&operation.id),
            ast.lifecycle.cached_methods.contains(&operation.id)
        ));
        for (index, argument) in operation.arguments.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{name:\"{}\",type:{},default:{},rustDefault:{}}}",
                escape_js(&argument.name),
                render_value_descriptor(ast, &argument.ty),
                render_argument_default(argument.default.as_ref(), &argument.ty).0,
                render_argument_default(argument.default.as_ref(), &argument.ty).1
            ));
        }
        out.push_str("],returnType:");
        if let Some(return_type) = &operation.return_type {
            out.push_str(&render_value_descriptor(ast, return_type));
        } else {
            out.push_str("null");
        }
        out.push_str(",throws:");
        if let Some(error) = &operation.throws {
            if let Some(error_type) = ast.types.iter().find(|ty| ty.source_key == *error) {
                out.push_str(&format!(
                    "{{name:\"{}\",typeId:{}}}",
                    escape_js(&error_type.name),
                    error_type.id.index()
                ));
            } else {
                out.push_str("null");
            }
        } else {
            out.push_str("null");
        }
        out.push_str(",streamResources:[");
        for (index, resource) in operation.stream_resources.iter().enumerate() {
            if index != 0 {
                out.push(',');
            }
            out.push_str(&render_stream_resource_descriptor(ast, resource));
        }
        out.push_str("],callbackContracts:");
        out.push_str(&render_callback_contracts(ast, operation.id));
        out.push_str("},");
    }
    out.push_str("}}");
    out
}

fn operation_owner_surface(operation: &AstOperation) -> &'static str {
    match operation.source_key.owner() {
        OperationOwner::Namespace => "namespace",
        OperationOwner::Object(_) => "object",
        OperationOwner::Value(_) => "value",
        OperationOwner::Callback(_) => "callback",
    }
}

fn render_stream_resource_descriptor(ast: &PublicAst, resource: &AstStreamResource) -> String {
    let slots = resource
        .slot_operation_ids
        .iter()
        .map(|(kind, id)| format!("{:?}:{}", kind, id.index()))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{{id:{},path:\"{}\",direction:\"{:?}\",item:{},error:{},isSend:{},slots:{{{}}}}}",
        resource.id.index(),
        escape_js(&resource.path.to_string()),
        resource.direction,
        render_value_descriptor(ast, &resource.item),
        resource
            .error
            .as_ref()
            .map(|error| render_value_descriptor(ast, error))
            .unwrap_or_else(|| "null".to_owned()),
        resource.is_send,
        slots
    )
}

fn operation_owner_type_key(owner: &OperationOwner) -> Option<&TypeSourceKey> {
    match owner {
        OperationOwner::Object(key)
        | OperationOwner::Value(key)
        | OperationOwner::Callback(key) => Some(key),
        OperationOwner::Namespace => None,
    }
}

fn operation_surface(operation: &AstOperation) -> &'static str {
    operation_owner_surface(operation)
}

fn render_type_descriptor(ast: &PublicAst, ty: &AstType) -> String {
    match &ty.kind {
        AstTypeKind::Record { fields } => format!(
            "{{kind:\"record\",fields:{{{}}}}}",
            fields
                .iter()
                .map(|field| {
                    format!(
                        "\"{}\":{{type:{},default:{},rustDefault:{}}}",
                        escape_js(&field.name),
                        render_value_descriptor(ast, &field.ty),
                        render_argument_default(field.default.as_ref(), &field.ty).0,
                        render_argument_default(field.default.as_ref(), &field.ty).1
                    )
                })
                .collect::<Vec<_>>()
                .join(",")
        ),
        AstTypeKind::Enum { variants } => format!(
            "{{kind:\"enum\",unit:{},variants:{{{}}}}}",
            variants.iter().all(|variant| variant.fields.is_empty()),
            variants
                .iter()
                .map(|variant| format!(
                    "\"{}\":{{fields:{{{}}}}}",
                    escape_js(&variant.name),
                    variant
                        .fields
                        .iter()
                        .map(|field| format!(
                            "\"{}\":{}",
                            escape_js(&field.name),
                            render_value_descriptor(ast, &field.ty)
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        AstTypeKind::Error { variants } => format!(
            "{{kind:\"enum\",error:true,unit:{},variants:{{{}}}}}",
            variants.iter().all(|variant| variant.fields.is_empty()),
            variants
                .iter()
                .map(|variant| format!(
                    "\"{}\":{{fields:{{{}}}}}",
                    escape_js(&variant.name),
                    variant
                        .fields
                        .iter()
                        .map(|field| format!(
                            "\"{}\":{}",
                            escape_js(&field.name),
                            render_value_descriptor(ast, &field.ty)
                        ))
                        .collect::<Vec<_>>()
                        .join(",")
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        AstTypeKind::Custom { builtin, config } => format!(
            "{{kind:\"custom\",builtin:{},publicType:\"{}\",intoCustom:{},fromCustom:{}}}",
            render_value_descriptor(ast, builtin),
            escape_js(&config.public_type_name),
            render_custom_function(&config.into_custom),
            render_custom_function(&config.from_custom)
        ),
        AstTypeKind::Object { kind }
            if matches!(
                kind,
                uniffi_js_abi::ObjectKind::TraitBoth | uniffi_js_abi::ObjectKind::TraitForeignOnly
            ) =>
        {
            format!(
                "{{kind:\"object\",callback:true,callbackOnly:{},methods:{{{}}}}}",
                matches!(kind, uniffi_js_abi::ObjectKind::TraitForeignOnly),
                render_callback_methods(ast, ty)
            )
        }
        AstTypeKind::Object { .. } => "{kind:\"object\"}".to_owned(),
        AstTypeKind::Callback => {
            format!(
                "{{kind:\"callback\",methods:{{{}}}}}",
                render_callback_methods(ast, ty)
            )
        }
    }
}

fn render_callback_methods(ast: &PublicAst, ty: &AstType) -> String {
    ast.operations
        .iter()
        .filter_map(|operation| {
            let is_method = matches!(operation.source_key.owner(), OperationOwner::Callback(key) if key == &ty.source_key && operation.kind == OperationKind::CallbackMethod)
                || matches!(operation.source_key.owner(), OperationOwner::Object(key) if key == &ty.source_key && matches!(operation.kind, OperationKind::Method | OperationKind::OutputStreamStart));
            if !is_method {
                return None;
            }
            operation.callback_method_id.map(|id| {
                let args = operation
                    .arguments
                    .iter()
                    .map(|argument| render_value_descriptor(ast, &argument.ty))
                    .collect::<Vec<_>>()
                    .join(",");
                let return_type = operation
                    .return_type
                    .as_ref()
                    .map(|ty| render_value_descriptor(ast, ty))
                    .unwrap_or_else(|| "null".to_owned());
                let throws = operation
                    .throws
                    .as_ref()
                    .and_then(|key| ast.types.iter().find(|candidate| candidate.source_key == *key))
                    .map(|ty| format!("{{name:\"{}\",typeId:{}}}", escape_js(&ty.name), ty.id.index()))
                    .unwrap_or_else(|| "null".to_owned());
                format!(
                    "\"{}\":{{name:\"{}\",async:{},args:[{}],returnType:{},throws:{},streamResources:[{}],callbackContracts:{}}}",
                    id,
                    escape_js(&operation.name),
                    operation.async_kind == AsyncKind::Async,
                    args,
                    return_type,
                    throws,
                    operation.stream_resources.iter().map(|resource| render_stream_resource_descriptor(ast, resource)).collect::<Vec<_>>().join(","),
                    render_callback_contracts(ast, operation.id)
                )
            })
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn render_callback_contracts(ast: &PublicAst, operation_id: OperationId) -> String {
    let mut out = String::from("{");
    for (index, callback) in ast
        .callbacks
        .iter()
        .filter(|callback| callback.operation_id == operation_id)
        .enumerate()
    {
        if index != 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "\"{}\":{{callbackTypeId:{},retention:\"{}\",threading:\"{}\",reentrancy:\"{}\"}}",
            escape_js(&callback.path.to_string()),
            callback.callback_type.index(),
            match callback.contract.retention {
                CallbackRetention::Scoped => "scoped",
                CallbackRetention::Retained => "retained",
            },
            match callback.contract.threading {
                CallbackThreading::CallingThread => "callingThread",
                CallbackThreading::MayCrossThread => "mayCrossThread",
            },
            match callback.contract.reentrancy {
                CallbackReentrancy::Forbidden => "forbidden",
                CallbackReentrancy::Allowed => "allowed",
            }
        ));
    }
    out.push('}');
    out
}

fn render_custom_function(expression: &str) -> String {
    let expression = if expression.is_empty() {
        "value".to_owned()
    } else {
        expression.replace("{}", "value")
    };
    format!("(value) => ({expression})")
}

fn render_import_line(import: &str) -> String {
    let trimmed = import.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        String::new()
    } else if trimmed.starts_with("import ") {
        format!("{trimmed};\n")
    } else {
        format!("import {trimmed};\n")
    }
}

fn custom_type_import_alias(ty: &AstType) -> String {
    format!("__UniffiCustomType{}", ty.id.index())
}

/// Rewrite the local binding of one configured named import to an
/// engine-private name.  A custom type commonly uses the same public name for
/// both its configured import and its generated export; importing the public
/// name verbatim would make `export type Name = Name` self-referential.
fn rewrite_custom_type_import(
    rendered_import: &str,
    public_name: &str,
    private_name: &str,
) -> Option<String> {
    if public_name.is_empty() {
        return None;
    }
    let open = rendered_import.find('{')?;
    let relative_close = rendered_import[open + 1..].find('}')?;
    let close = open + 1 + relative_close;
    let mut changed = false;
    let bindings = rendered_import[open + 1..close]
        .split(',')
        .map(|binding| {
            let trimmed = binding.trim();
            let words = trimmed.split_whitespace().collect::<Vec<_>>();
            let rewritten = match words.as_slice() {
                [local] if *local == public_name => {
                    Some(format!("{public_name} as {private_name}"))
                }
                [source, "as", local] if *local == public_name => {
                    Some(format!("{source} as {private_name}"))
                }
                ["type", local] if *local == public_name => {
                    Some(format!("type {public_name} as {private_name}"))
                }
                ["type", source, "as", local] if *local == public_name => {
                    Some(format!("type {source} as {private_name}"))
                }
                _ => None,
            };
            if rewritten.is_some() {
                changed = true;
            }
            rewritten.unwrap_or_else(|| trimmed.to_owned())
        })
        .collect::<Vec<_>>()
        .join(", ");
    changed.then(|| {
        format!(
            "{} {} {}",
            &rendered_import[..open + 1],
            bindings,
            &rendered_import[close..]
        )
    })
}

fn custom_type_uses_import_alias(ty: &AstType, config: &JsCustomTypeConfig) -> bool {
    let alias = custom_type_import_alias(ty);
    config.imports.iter().any(|import| {
        rewrite_custom_type_import(
            &render_import_line(import),
            &config.public_type_name,
            &alias,
        )
        .is_some()
    })
}

fn render_runtime_import_line(import: &str, extension: &str) -> Result<String, FacadeError> {
    let trimmed = import.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    // TOML custom-type configs may use either `import type { ... }` or the
    // shorthand `type { ... }`.  Both are declaration-only dependencies and
    // must never be emitted into executable `.js` component modules.
    if trimmed.starts_with("import type ") || trimmed.starts_with("type ") {
        return Ok(String::new());
    }
    let normalized = if extension == "js" {
        trimmed.replace(".ts\"", ".js\"").replace(".ts'", ".js'")
    } else {
        trimmed.replace(".ts\"", ".ets\"").replace(".ts'", ".ets'")
    };
    if normalized.starts_with("import ") {
        Ok(format!("{normalized};\n"))
    } else {
        Ok(format!("import {normalized};\n"))
    }
}

fn render_default(value: &DefaultValue) -> String {
    match value {
        DefaultValue::Unspecified => "undefined".to_owned(),
        DefaultValue::Boolean(value) => value.to_string(),
        DefaultValue::String(value) | DefaultValue::Enum(value) => {
            format!("\"{}\"", escape_js(value))
        }
        DefaultValue::Integer { value, .. } => value.to_string(),
        DefaultValue::Float(value) => value.clone(),
        DefaultValue::EmptySequence => "[]".to_owned(),
        DefaultValue::EmptyMap => "new Map()".to_owned(),
        DefaultValue::EmptySet => "new Set()".to_owned(),
        DefaultValue::None => "null".to_owned(),
        DefaultValue::Some(inner) => render_default(inner),
    }
}

fn render_default_for_type(value: &DefaultValue, ty: &ValueType) -> String {
    if let DefaultValue::Some(inner) = value {
        // `Some` is a Rust wrapper and has no JavaScript representation.  The
        // contained literal must still be rendered against the optional's
        // inner type (not the `Optional` wrapper), e.g. an i64 default is a
        // bigint literal rather than a number.
        let inner_type = match ty {
            ValueType::Optional(inner_type) => inner_type.as_ref(),
            _ => ty,
        };
        return render_default_for_type(inner, inner_type);
    }
    let rendered = render_default(value);
    if matches!(ty, ValueType::Scalar(ScalarType::I64 | ScalarType::U64))
        && rendered != "null"
        && rendered != "undefined"
        && !rendered.ends_with('n')
    {
        format!("{rendered}n")
    } else {
        rendered
    }
}

/// Render the two pieces of default metadata consumed by the JavaScript
/// runtime.  A `DefaultValue::Unspecified` is Rust's `Default::default()` and
/// therefore must be represented as an omission (rather than a JavaScript
/// value that can be lowered).  Literal/default values are lowered in the
/// generated facade and carry `rustDefault: false`.
fn render_argument_default(value: Option<&DefaultValue>, ty: &ValueType) -> (String, bool) {
    match value {
        Some(DefaultValue::Unspecified) => ("undefined".to_owned(), true),
        Some(value) => (render_default_for_type(value, ty), false),
        None => ("undefined".to_owned(), false),
    }
}

fn call_options_name(operation: &AstOperation) -> Option<String> {
    if operation.async_kind != AsyncKind::Async
        || matches!(operation.source_key.owner(), OperationOwner::Callback(_))
        || !matches!(
            operation.kind,
            OperationKind::Function
                | OperationKind::Method
                | OperationKind::Constructor
                | OperationKind::OutputStreamStart
        )
    {
        return None;
    }
    let mut name = "options".to_owned();
    while operation
        .arguments
        .iter()
        .any(|argument| argument.name == name)
    {
        name.insert(0, '_');
    }
    Some(name)
}

fn call_options_type_name(ast: &PublicAst, component_id: ComponentId) -> String {
    let namespace = &ast
        .components
        .iter()
        .find(|component| component.id == component_id)
        .expect("component")
        .namespace;
    let mut name = "CallOptions".to_owned();
    while ast
        .types
        .iter()
        .any(|ty| ty.source_key.component().namespace() == namespace && ty.name == name)
    {
        name.insert(0, '_');
    }
    name
}

fn append_component_call_options(
    ast: &PublicAst,
    operation: &AstOperation,
    args: String,
    declaration: bool,
) -> String {
    append_named_call_options(
        operation,
        args,
        declaration,
        &call_options_type_name(ast, operation.component_id),
    )
}

fn append_call_options(operation: &AstOperation, args: String, declaration: bool) -> String {
    append_named_call_options(operation, args, declaration, "CallOptions")
}

fn append_named_call_options(
    operation: &AstOperation,
    mut args: String,
    declaration: bool,
    options_type: &str,
) -> String {
    if let Some(name) = call_options_name(operation) {
        if !args.is_empty() {
            args.push_str(", ");
        }
        args.push_str(&name);
        if declaration {
            args.push_str("?: ");
            args.push_str(options_type);
        }
    }
    args
}

fn render_component_declaration(
    ast: &PublicAst,
    component: &AstComponent,
) -> Result<String, FacadeError> {
    let mut out = String::new();
    let extension = "js";
    let options_type = call_options_type_name(ast, component.id);
    let options_import = if options_type == "CallOptions" {
        options_type.clone()
    } else {
        format!("CallOptions as {options_type}")
    };
    out.push_str("// AUTOGENERATED public declaration; no implementation .ts files.\n");
    if !ast.lifecycle.close_methods.is_empty() {
        out.push_str("/// <reference lib=\"esnext.disposable\" />\n");
    }
    out.push_str(&format!(
        "import {{ UniffiError }} from \"../../shared/uniffi_runtime.{extension}\";\nimport type {{ {options_import}, UniFfiInputStream, UniFfiStream }} from \"../../shared/uniffi_runtime.{extension}\";\n\n"
    ));
    for ty in ast
        .types
        .iter()
        .filter(|ty| ty.source_key.component().namespace() == component.namespace)
    {
        if let AstTypeKind::Custom { config, .. } = &ty.kind {
            let private_name = custom_type_import_alias(ty);
            for import in &config.imports {
                let rendered = render_import_line(import);
                out.push_str(
                    &rewrite_custom_type_import(&rendered, &config.public_type_name, &private_name)
                        .unwrap_or(rendered),
                );
            }
        }
    }
    out.push('\n');
    out.push_str("export interface Namespace {\n");
    out.push_str("  readonly UniffiError: typeof UniffiError;\n");
    let operations: Vec<_> = ast
        .operations
        .iter()
        .filter(|operation| operation.component_id == component.id)
        .collect();
    for operation in &operations {
        let args = operation
            .arguments
            .iter()
            .map(|argument| {
                let optional = if argument.default.is_some() { "?" } else { "" };
                format!(
                    "{}{}: {}",
                    argument.name,
                    optional,
                    render_public_type(ast, &argument.ty)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let args = append_component_call_options(ast, operation, args, true);
        let return_type = operation
            .return_type
            .as_ref()
            .map(|ty| render_public_type(ast, ty))
            .unwrap_or_else(|| "void".to_owned());
        let return_type = if operation.async_kind == AsyncKind::Async {
            format!("Promise<{return_type}>")
        } else {
            return_type
        };
        if operation.receiver_type.is_none() && operation_surface(operation) == "namespace" {
            out.push_str(&format!(
                "  {}({}): {};\n",
                operation.name, args, return_type
            ));
        }
    }
    for ty in ast
        .types
        .iter()
        .filter(|ty| ty.source_key.component().namespace() == component.namespace)
    {
        if matches!(ty.kind, AstTypeKind::Error { .. }) {
            out.push_str(&format!("  readonly {}: typeof {};\n", ty.name, ty.name));
        }
        if !value_operations(ast, ty).is_empty() {
            out.push_str(&format!("  readonly {}: {{\n", ty.name));
            for operation in value_operations(ast, ty) {
                let self_argument = if operation.kind == OperationKind::Constructor {
                    String::new()
                } else {
                    format!("self_: {}", ty.name)
                };
                let operation_arguments = operation
                    .arguments
                    .iter()
                    .map(|argument| {
                        format!(
                            "{}{}: {}",
                            argument.name,
                            if argument.default.is_some() { "?" } else { "" },
                            render_public_type(ast, &argument.ty)
                        )
                    })
                    .collect::<Vec<_>>();
                let mut all_arguments = Vec::new();
                if !self_argument.is_empty() {
                    all_arguments.push(self_argument);
                }
                all_arguments.extend(operation_arguments);
                let return_type = operation
                    .return_type
                    .as_ref()
                    .map(|ty| render_public_type(ast, ty))
                    .unwrap_or_else(|| "void".to_owned());
                let return_type = if operation.async_kind == AsyncKind::Async {
                    format!("Promise<{return_type}>")
                } else {
                    return_type
                };
                out.push_str(&format!(
                    "    readonly {}: ({}) => {};\n",
                    operation.name,
                    append_component_call_options(ast, operation, all_arguments.join(", "), true),
                    return_type
                ));
            }
            out.push_str("  };\n");
        }
    }
    for ty in ast
        .types
        .iter()
        .filter(|ty| ty.source_key.component().namespace() == component.namespace)
    {
        if let AstTypeKind::Object { .. } = ty.kind {
            let constructors: Vec<_> = operations
                .iter()
                .filter(|operation| {
                    operation.kind == OperationKind::Constructor
                        && operation.source_key.owner()
                            == &OperationOwner::Object(ty.source_key.clone())
                })
                .collect();
            if !constructors.is_empty() {
                out.push_str(&format!(
                    "  readonly {}: {}Constructor;\n",
                    ty.name, ty.name
                ));
            }
        }
    }
    out.push_str("}\n");
    for ty in ast
        .types
        .iter()
        .filter(|ty| ty.source_key.component().namespace() == component.namespace)
    {
        render_type_declaration(ast, ty, &mut out)?;
    }
    out.push_str(&format!("export {{ UniffiError }};\nexport type {{ {options_import} }} from \"../../shared/uniffi_runtime.js\";\n"));
    Ok(out)
}

fn value_operations<'a>(ast: &'a PublicAst, ty: &AstType) -> Vec<&'a AstOperation> {
    ast.operations
        .iter()
        .filter(|operation| {
            matches!(operation.source_key.owner(), OperationOwner::Value(key) if key == &ty.source_key)
        })
        .collect()
}

fn render_type_declaration(
    ast: &PublicAst,
    ty: &AstType,
    out: &mut String,
) -> Result<(), FacadeError> {
    match &ty.kind {
        AstTypeKind::Record { fields } => {
            out.push_str(&format!("export interface {} {{\n", ty.name));
            for field in fields {
                out.push_str(&format!(
                    "  {}{}: {};\n",
                    field.name,
                    if field.default.is_some() { "?" } else { "" },
                    render_public_type(ast, &field.ty)
                ));
            }
            out.push_str("}\n");
        }
        AstTypeKind::Enum { variants } => {
            if variants.iter().all(|variant| variant.fields.is_empty()) {
                out.push_str(&format!(
                    "export type {} = {};\n",
                    ty.name,
                    variants
                        .iter()
                        .map(|variant| format!("\"{}\"", variant.name))
                        .collect::<Vec<_>>()
                        .join(" | ")
                ));
            } else {
                out.push_str(&format!("export type {} =\n", ty.name));
                for (index, variant) in variants.iter().enumerate() {
                    let fields = if variant.fields.is_empty() {
                        format!("{{ readonly tag: \"{}\" }}", variant.name)
                    } else {
                        let fields = variant
                            .fields
                            .iter()
                            .map(|field| {
                                format!(
                                    "readonly {}: {}",
                                    field.name,
                                    render_public_type(ast, &field.ty)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        format!("{{ readonly tag: \"{}\"; {} }}", variant.name, fields)
                    };
                    out.push_str(&format!(
                        "  {}{}\n",
                        fields,
                        if index + 1 == variants.len() {
                            ";"
                        } else {
                            " |"
                        }
                    ));
                }
            }
            out.push_str(&format!("export declare const {}: {{\n", ty.name));
            for variant in variants {
                if variant.fields.is_empty() {
                    out.push_str(&format!(
                        "  readonly {}: \"{}\";\n",
                        variant.name, variant.name
                    ));
                } else {
                    let fields = variant
                        .fields
                        .iter()
                        .map(|field| {
                            format!("{}: {}", field.name, render_public_type(ast, &field.ty))
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!(
                        "  {}(value: {{ {} }}): {};\n",
                        variant.name, fields, ty.name
                    ));
                }
            }
            out.push_str("};\n");
        }
        AstTypeKind::Error { variants } => {
            // A class with union-valued properties loses the relationship between
            // variant and data. Keep the real JS constructor in the value namespace,
            // and use a discriminated union for its instances (including instanceof).
            out.push_str(&format!(
                "export type {} = UniffiError & {{ readonly errorName: \"{}\" }} & (\n",
                ty.name, ty.name
            ));
            let payloads: Vec<_> = variants
                .iter()
                .map(|variant| {
                    if variant.fields.is_empty() {
                        format!("\"{}\"", variant.name)
                    } else {
                        let fields = variant
                            .fields
                            .iter()
                            .map(|field| {
                                format!(
                                    "readonly {}: {}",
                                    field.name,
                                    render_public_type(ast, &field.ty)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("; ");
                        // Rust-lifted payloads include tag; manually constructed
                        // errors only need the variant argument and declared fields.
                        format!("{{ readonly tag?: \"{}\"; {} }}", variant.name, fields)
                    }
                })
                .collect();
            for (index, (variant, payload)) in variants.iter().zip(&payloads).enumerate() {
                out.push_str(&format!(
                    "  {}{{ readonly variant: \"{}\"; readonly data: {} }}\n",
                    if index == 0 { "" } else { "| " },
                    variant.name,
                    payload
                ));
            }
            out.push_str(");\n");
            out.push_str(&format!(
                "export declare const {}: {{\n  readonly prototype: {};\n",
                ty.name, ty.name
            ));
            for (variant, payload) in variants.iter().zip(&payloads) {
                out.push_str(&format!(
                    "  new(message: string | undefined, variant: \"{}\", data: {}): {} & {{ readonly variant: \"{}\" }};\n",
                    variant.name, payload, ty.name, variant.name
                ));
            }
            out.push_str("};\n");
        }
        AstTypeKind::Custom { builtin, config } => {
            let public_type = if custom_type_uses_import_alias(ty, config) {
                custom_type_import_alias(ty)
            } else if config.public_type_name.is_empty() {
                render_public_type(ast, builtin)
            } else {
                config.public_type_name.clone()
            };
            out.push_str(&format!("export type {} = {};\n", ty.name, public_type));
        }
        AstTypeKind::Object {
            kind: uniffi_js_abi::ObjectKind::TraitForeignOnly,
        } => {
            out.push_str(&format!("export interface {} {{\n", ty.name));
            for operation in ast.operations.iter().filter(|operation| {
                matches!(
                    operation.source_key.owner(),
                    OperationOwner::Callback(key)
                        if key == &ty.source_key
                            && operation.kind == OperationKind::CallbackMethod
                )
            }) {
                let args = operation
                    .arguments
                    .iter()
                    .map(|argument| {
                        format!(
                            "{}: {}",
                            argument.name,
                            render_public_type(ast, &argument.ty)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let return_type = operation
                    .return_type
                    .as_ref()
                    .map(|value| render_public_type(ast, value))
                    .unwrap_or_else(|| "void".to_owned());
                let return_type = if operation.async_kind == AsyncKind::Async {
                    format!("Promise<{return_type}>")
                } else {
                    return_type
                };
                out.push_str(&format!(
                    "  {}({}): {};\n",
                    operation.name, args, return_type
                ));
            }
            out.push_str("}\n");
        }
        AstTypeKind::Object { .. } => {
            out.push_str(&format!(
                "export declare class {} {{ private constructor();\n",
                ty.name
            ));
            if ast.lifecycle.close_methods.contains_key(&ty.id) {
                out.push_str("  [Symbol.asyncDispose](): Promise<void>;\n");
            }
            for operation in ast
                .operations
                .iter()
                .filter(|operation| operation.receiver_type == Some(ty.id))
            {
                let args = operation
                    .arguments
                    .iter()
                    .map(|argument| {
                        format!(
                            "{}{}: {}",
                            argument.name,
                            if argument.default.is_some() { "?" } else { "" },
                            render_public_type(ast, &argument.ty)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let args = append_component_call_options(ast, operation, args, true);
                let return_type = operation
                    .return_type
                    .as_ref()
                    .map(|value| render_public_type(ast, value))
                    .unwrap_or_else(|| "void".to_owned());
                let return_type = if operation.async_kind == AsyncKind::Async {
                    format!("Promise<{return_type}>")
                } else {
                    return_type
                };
                out.push_str(&format!(
                    "  {}({}): {};\n",
                    operation.name, args, return_type
                ));
            }
            out.push_str("}\n");
            let constructors = ast
                .operations
                .iter()
                .filter(|operation| {
                    operation.kind == OperationKind::Constructor
                        && operation.source_key.owner()
                            == &OperationOwner::Object(ty.source_key.clone())
                })
                .collect::<Vec<_>>();
            if !constructors.is_empty() {
                out.push_str(&format!(
                    "export interface {}Constructor {{\n  readonly prototype: {};\n",
                    ty.name, ty.name
                ));
                for operation in constructors {
                    let args = operation
                        .arguments
                        .iter()
                        .map(|argument| {
                            format!(
                                "{}{}: {}",
                                argument.name,
                                if argument.default.is_some() { "?" } else { "" },
                                render_public_type(ast, &argument.ty)
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    let args = append_component_call_options(ast, operation, args, true);
                    let return_type = if operation.async_kind == AsyncKind::Async {
                        format!("Promise<{}>", ty.name)
                    } else {
                        ty.name.clone()
                    };
                    out.push_str(&format!(
                        "  readonly {}: ({}) => {};\n",
                        operation.name, args, return_type
                    ));
                }
                out.push_str("}\n");
            }
        }
        AstTypeKind::Callback => {
            out.push_str(&format!("export interface {} {{\n", ty.name));
            for operation in ast.operations.iter().filter(|operation| matches!(operation.source_key.owner(), OperationOwner::Callback(key) if key == &ty.source_key && operation.kind == OperationKind::CallbackMethod)) {
                let args = operation.arguments.iter().map(|argument| format!("{}: {}", argument.name, render_public_type(ast, &argument.ty))).collect::<Vec<_>>().join(", ");
                let return_type = operation.return_type.as_ref().map(|value| render_public_type(ast, value)).unwrap_or_else(|| "void".to_owned());
                let return_type = if operation.async_kind == AsyncKind::Async { format!("Promise<{return_type}>") } else { return_type };
                out.push_str(&format!("  {}({}): {};\n", operation.name, args, return_type));
            }
            out.push_str("}\n");
        }
    }
    Ok(())
}

fn render_public_type(ast: &PublicAst, ty: &ValueType) -> String {
    match ty {
        ValueType::Scalar(scalar) => match scalar {
            ScalarType::Bool => "boolean".to_owned(),
            ScalarType::I64 | ScalarType::U64 => "bigint".to_owned(),
            ScalarType::Bytes => "Uint8Array".to_owned(),
            ScalarType::String => "string".to_owned(),
            _ => "number".to_owned(),
        },
        ValueType::Timestamp => "Date".to_owned(),
        ValueType::Duration => "number".to_owned(),
        ValueType::Named(key) => ast
            .types
            .iter()
            .find(|candidate| candidate.source_key == *key)
            .map(|candidate| candidate.name.clone())
            .unwrap_or_else(|| "never".to_owned()),
        ValueType::Optional(inner) => format!("{} | null", render_public_type(ast, inner)),
        ValueType::Sequence(inner) => format!("Array<{}>", render_public_type(ast, inner)),
        ValueType::Map(key, value) => format!(
            "Map<{}, {}>",
            render_public_type(ast, key),
            render_public_type(ast, value)
        ),
        ValueType::Set(inner) => format!("Set<{}>", render_public_type(ast, inner)),
        ValueType::InputStream { item: inner, .. } => {
            format!("UniFfiInputStream<{}>", render_public_type(ast, inner))
        }
        ValueType::OutputStream { item: inner, .. } => {
            format!("UniFfiStream<{}>", render_public_type(ast, inner))
        }
    }
}

fn render_value_descriptor(ast: &PublicAst, ty: &ValueType) -> String {
    match ty {
        ValueType::Scalar(scalar) => format!("{{kind:\"scalar\",name:\"{:?}\"}}", scalar),
        ValueType::Timestamp => "{kind:\"timestamp\"}".to_owned(),
        ValueType::Duration => "{kind:\"duration\"}".to_owned(),
        ValueType::Named(key) => {
            let name = ast
                .types
                .iter()
                .find(|candidate| candidate.source_key == *key)
                .map(|candidate| candidate.name.as_str())
                .unwrap_or("Unknown");
            format!(
                "{{kind:\"named\",name:\"{}\",typeId:{}}}",
                escape_js(name),
                ast.types
                    .iter()
                    .find(|candidate| candidate.source_key == *key)
                    .map(|candidate| candidate.id.index())
                    .unwrap_or(0)
            )
        }
        ValueType::Optional(inner) => format!(
            "{{kind:\"optional\",inner:{}}}",
            render_value_descriptor(ast, inner)
        ),
        ValueType::Sequence(inner) => format!(
            "{{kind:\"sequence\",inner:{}}}",
            render_value_descriptor(ast, inner)
        ),
        ValueType::Map(key, value) => format!(
            "{{kind:\"map\",key:{},value:{}}}",
            render_value_descriptor(ast, key),
            render_value_descriptor(ast, value)
        ),
        ValueType::Set(inner) => format!(
            "{{kind:\"set\",inner:{}}}",
            render_value_descriptor(ast, inner)
        ),
        ValueType::InputStream { item: inner, .. } => format!(
            "{{kind:\"inputStream\",inner:{}}}",
            render_value_descriptor(ast, inner)
        ),
        ValueType::OutputStream { item: inner, .. } => format!(
            "{{kind:\"outputStream\",inner:{}}}",
            render_value_descriptor(ast, inner)
        ),
    }
}

fn escape_js(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Expose the self-contained runtime without asking callers to know where an
/// asset lives in the source tree.
pub fn runtime_source() -> &'static str {
    RUNTIME_SOURCE
}

#[cfg(test)]
mod tests {
    mod recursive;

    use super::*;
    use std::fs;
    use std::process::Command;
    use std::time::{SystemTime, UNIX_EPOCH};
    use uniffi_bindgen::interface::ComponentInterface;
    use uniffi_bindgen::Component;
    use uniffi_bindgen_javascript::frontend::{normalize, BindingInput};
    use uniffi_bindgen_javascript::{CustomTypeConfig, JsConfig};
    use uniffi_js_abi::{
        assign_component_ids, assign_operation_ids, assign_type_ids, ArgumentDefinition,
        Capability, CapabilitySet, ComponentDefinition, ComponentKey, JsApiIr, JsComponent,
        JsOperation, JsType, JsTypeKind, NamedTypeKind, OperationDefinition, OperationSignature,
        ResolvedJsConfig, TypeDefinition,
    };
    use uniffi_js_engine_schema::{
        BridgePlan, BridgePlanInput, CallbackContract, CallbackReentrancy, CallbackRetention,
        CallbackThreading, CallbackUseSite, EngineCapabilities, EngineKind, EngineRustBridgePlan,
        PlannedOperation, RustCallTarget, RustOperationPlan, RustPath, StreamDirection, ValuePath,
    };
    use uniffi_meta::{
        CallbackContract as MetaCallbackContract, CallbackInterfaceMetadata, CallbackOperationKind,
        CallbackReentrancy as MetaCallbackReentrancy, CallbackRetention as MetaCallbackRetention,
        CallbackThreading as MetaCallbackThreading, CallbackUseSiteMetadata, CallbackValuePath,
        ConstructorMetadata, EnumMetadata, EnumShape, FieldMetadata, FnMetadata, FnParamMetadata,
        LiteralMetadata, MetadataGroup, MethodMetadata, NamespaceMetadata, ObjectImpl,
        ObjectMetadata, RecordMetadata, TraitMethodMetadata, Type, VariantMetadata,
    };

    fn key(component: &ComponentKey, name: &str) -> TypeSourceKey {
        TypeSourceKey::new(component.clone(), name).unwrap()
    }

    fn operation_key(
        component: &ComponentKey,
        owner: OperationOwner,
        kind: OperationKind,
        name: &str,
    ) -> uniffi_js_abi::OperationSourceKey {
        uniffi_js_abi::OperationSourceKey::new(component.clone(), owner, kind, name).unwrap()
    }

    // Type-check ordinary business imports against both actual Ark printer
    // implementations and their declarations, with no hand-written API stubs.
    fn check_ark_business_types(ast: &PublicAst, consumer: &str, support: &[(&str, &str)]) {
        let root = std::env::temp_dir().join(format!(
            "uniffi-ark-business-types-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let files = render_ark_inventory(ast).unwrap();
        let compiler =
            std::env::var("UNIFFI_TEST_TYPESCRIPT_COMPILER").unwrap_or_else(|_| "tsc".into());
        for (part, declaration) in [("implementation", false), ("declaration", true)] {
            let directory = root.join(part);
            fs::create_dir_all(&directory).unwrap();
            fs::write(directory.join("package.json"), "{\"type\":\"commonjs\"}").unwrap();
            for file in &files {
                if file.path.ends_with(".d.ets") == declaration {
                    fs::write(
                        directory.join(file.path.replace(".ets", ".ts")),
                        &file.bytes,
                    )
                    .unwrap();
                }
            }
            for (path, contents) in support {
                let path = directory.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, contents).unwrap();
            }
            fs::write(directory.join("check.ts"), consumer).unwrap();
            let output = Command::new(&compiler)
                .args([
                    "--strict",
                    "--noEmit",
                    "--useUnknownInCatchVariables",
                    "false",
                    "--target",
                    "ES2022",
                    "--module",
                    "NodeNext",
                    "--moduleResolution",
                    "NodeNext",
                    "check.ts",
                ])
                .current_dir(&directory)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "Ark business types in {}: {}{}",
                directory.display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn call_options_type_collision_preserves_business_type_and_strict_calls() {
        let mut ast = corpus_ast();
        let profile = ast
            .types
            .iter_mut()
            .find(|ty| ty.name == "Profile")
            .unwrap();
        profile.name = "CallOptions".into();
        let profile_key = profile.source_key.clone();
        ast.types
            .iter_mut()
            .find(|ty| ty.name == "Event")
            .unwrap()
            .name = "_CallOptions".into();
        let operation = ast
            .operations
            .iter_mut()
            .find(|op| op.id.index() == 0)
            .unwrap();
        operation.async_kind = AsyncKind::Async;
        operation.arguments = vec![AstArgument {
            name: "options".into(),
            ty: ValueType::Named(profile_key),
            default: None,
        }];
        let declaration = render_component_declaration(&ast, &ast.components[0]).unwrap();
        assert!(declaration.contains("CallOptions as __CallOptions"));
        assert!(declaration.contains(
            "echo(options: CallOptions, _options?: __CallOptions): Promise<CallOptions>"
        ));
        let root = std::env::temp_dir().join(format!(
            "uniffi-call-options-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        for file in render_inventory(&ast).unwrap() {
            let path = root.join(file.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.bytes).unwrap();
        }
        fs::write(root.join("check.mts"), r#"
import type {CallOptions, __CallOptions as InvocationOptions, Namespace} from './components/corpus/index.js';
declare const api: Namespace;
const business: CallOptions = { scores: new Map(), avatar: null };
const invocation: InvocationOptions = { signal: new AbortController().signal };
const result: Promise<CallOptions> = api.echo(business, invocation);
// @ts-expect-error business data cannot be passed as cancellation options
api.echo(business, business);
"#).unwrap();
        let compiler =
            std::env::var("UNIFFI_TEST_TYPESCRIPT_COMPILER").unwrap_or_else(|_| "tsc".into());
        let output = Command::new(compiler)
            .args([
                "--strict",
                "--noEmit",
                "--target",
                "ES2022",
                "--module",
                "NodeNext",
                "--moduleResolution",
                "NodeNext",
                "check.mts",
            ])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "strict declaration failure in {}: {} {}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn async_call_options_preserve_business_arguments_and_host_signatures() {
        let mut ast = corpus_ast();
        for id in [0, 1, 2, 12] {
            let operation = ast
                .operations
                .iter_mut()
                .find(|op| op.id.index() == id)
                .unwrap();
            operation.async_kind = AsyncKind::Async;
            operation.arguments = vec![AstArgument {
                name: "value".into(),
                ty: ValueType::Scalar(ScalarType::String),
                default: Some(DefaultValue::Unspecified),
            }];
        }
        let declaration = render_component_declaration(&ast, &ast.components[0]).unwrap();
        assert!(
            declaration.contains("echo(value?: string, options?: CallOptions): Promise<Profile>")
        );
        assert!(declaration.contains(
            "readonly newService: (value?: string, options?: CallOptions) => Promise<Service>"
        ));
        assert!(declaration
            .contains("describe(value?: string, options?: CallOptions): Promise<string>"));
        assert!(declaration.contains("readonly collision: (self_: Profile, value?: string, options?: CallOptions) => Promise<string>"));
        assert!(declaration.contains("collision(): string"));
        assert!(declaration.contains("export type { CallOptions }"));
        let implementation =
            render_component_implementation(&ast, &ast.components[0], "js").unwrap();
        assert!(implementation.contains("describe(value, options) { return invokeObjectOperation(this, __descriptors.operations[2], [value, options]); }"));
        for operation in &ast.operations {
            if matches!(operation.source_key.owner(), OperationOwner::Callback(_)) {
                assert_eq!(call_options_name(operation), None);
            }
        }
        let mut host_operation = ast.operations[0].clone();
        for kind in [
            OperationKind::CallbackMethod,
            OperationKind::InputStreamPull,
            OperationKind::InputStreamCancel,
        ] {
            host_operation.kind = kind;
            assert_eq!(call_options_name(&host_operation), None);
        }
        host_operation.kind = OperationKind::Function;
        host_operation.arguments[0].name = "options".into();
        assert_eq!(
            call_options_name(&host_operation).as_deref(),
            Some("_options")
        );
        let runtime = runtime_declaration_source();
        assert!(runtime.contains(
            "constructor(backend: unknown, host: Host | undefined, options: BackendSessionOptions)"
        ));
        assert!(runtime
            .contains("invokeAsync(operationId: number, args: unknown[], options?: CallOptions)"));
        assert!(runtime.contains("environmentDestroyed(): void"));
        assert!(runtime.contains("createBackendSession(factory: (host: Host) => unknown, host: Host | undefined, options: BackendSessionOptions)"));
    }

    #[test]
    fn foreign_trait_distinguishes_native_methods_from_host_callback_signatures() {
        let components = [Component {
            ci: ComponentInterface::from_webidl("[Trait, WithForeign] interface Worker { [Async] u32 compute(u32 value); }; namespace trait_options {};", "trait_options").unwrap(),
            config: JsConfig::default(),
        }];
        let package = normalize(BindingInput::new(&components)).unwrap();
        let facade = build(&package.api, &package.bridge, &package.rust).unwrap();
        for operation in &facade.ast.operations {
            let target = &package
                .rust
                .engines
                .values()
                .next()
                .unwrap()
                .operations
                .iter()
                .find(|rust| rust.operation_id == operation.id)
                .unwrap()
                .call_target;
            match target {
                RustCallTarget::Method { .. } => {
                    assert_eq!(call_options_name(operation).as_deref(), Some("options"))
                }
                RustCallTarget::CallbackMethod { .. } => {
                    assert_eq!(call_options_name(operation), None)
                }
                _ => panic!("unexpected trait operation"),
            }
        }
        let declaration =
            render_component_declaration(&facade.ast, &facade.ast.components[0]).unwrap();
        assert!(
            declaration.contains("compute(value: number, options?: CallOptions): Promise<number>")
        );
        let ark =
            String::from_utf8(render_ark_inventory(&facade.ast).unwrap().remove(0).bytes).unwrap();
        assert!(ark.contains("compute(value: number, options?: CallOptions)"));
    }

    fn corpus_ast() -> PublicAst {
        let component = ComponentKey::new("corpus").unwrap();
        let profile = key(&component, "Profile");
        let event = key(&component, "Event");
        let failure = key(&component, "Failure");
        let service = key(&component, "Service");
        let observer = key(&component, "Observer");
        let color = key(&component, "Color");
        let profile_id = TypeId::new(0);
        let event_id = TypeId::new(1);
        let failure_id = TypeId::new(2);
        let service_id = TypeId::new(3);
        let observer_id = TypeId::new(4);
        let types = vec![
            AstType {
                id: profile_id,
                source_key: profile.clone(),
                name: "Profile".into(),
                kind: AstTypeKind::Record {
                    fields: vec![
                        AstField {
                            name: "scores".into(),
                            ty: ValueType::Map(
                                Box::new(ValueType::Scalar(ScalarType::String)),
                                Box::new(ValueType::Scalar(ScalarType::I64)),
                            ),
                            default: None,
                        },
                        AstField {
                            name: "avatar".into(),
                            ty: ValueType::Optional(Box::new(ValueType::Scalar(ScalarType::Bytes))),
                            default: Some(DefaultValue::None),
                        },
                    ],
                },
            },
            AstType {
                id: event_id,
                source_key: event.clone(),
                name: "Event".into(),
                kind: AstTypeKind::Enum {
                    variants: vec![
                        AstVariant {
                            name: "Ready".into(),
                            fields: vec![],
                        },
                        AstVariant {
                            name: "Message".into(),
                            fields: vec![AstField {
                                name: "message".into(),
                                ty: ValueType::Scalar(ScalarType::String),
                                default: None,
                            }],
                        },
                    ],
                },
            },
            AstType {
                id: failure_id,
                source_key: failure.clone(),
                name: "Failure".into(),
                kind: AstTypeKind::Error {
                    variants: vec![AstVariant {
                        name: "Rejected".into(),
                        fields: vec![AstField {
                            name: "message".into(),
                            ty: ValueType::Scalar(ScalarType::String),
                            default: None,
                        }],
                    }],
                },
            },
            AstType {
                id: service_id,
                source_key: service.clone(),
                name: "Service".into(),
                kind: AstTypeKind::Object {
                    kind: uniffi_js_abi::ObjectKind::Struct,
                },
            },
            AstType {
                id: observer_id,
                source_key: observer.clone(),
                name: "Observer".into(),
                kind: AstTypeKind::Callback,
            },
            AstType {
                id: TypeId::new(5),
                source_key: key(&component, "CustomName"),
                name: "CustomName".into(),
                kind: AstTypeKind::Custom {
                    builtin: ValueType::Scalar(ScalarType::String),
                    config: JsCustomTypeConfig {
                        public_type_name: "string".into(),
                        imports: vec![],
                        into_custom: "String({})".into(),
                        from_custom: "String({})".into(),
                    },
                },
            },
            AstType {
                id: TypeId::new(6),
                source_key: color,
                name: "Color".into(),
                kind: AstTypeKind::Enum {
                    variants: vec![
                        AstVariant {
                            name: "Red".into(),
                            fields: vec![],
                        },
                        AstVariant {
                            name: "Blue".into(),
                            fields: vec![],
                        },
                    ],
                },
            },
        ];
        let mut operations = Vec::new();
        let mut add = |id: u32,
                       owner: OperationOwner,
                       kind: OperationKind,
                       name: &str,
                       public: &str,
                       args: Vec<AstArgument>,
                       ret: Option<ValueType>,
                       async_kind: AsyncKind,
                       receiver: Option<TypeId>,
                       callback_method_id: Option<u32>,
                       stream_slots: Vec<AstStreamSlot>| {
            let stream_resources = if kind == OperationKind::OutputStreamStart {
                let item = match ret.as_ref() {
                    Some(ValueType::OutputStream { item, .. }) => item.as_ref().clone(),
                    _ => ValueType::Scalar(ScalarType::String),
                };
                let mut slots = BTreeMap::new();
                slots.insert(OperationKind::OutputStreamStart, OperationId::new(id));
                for slot in &stream_slots {
                    slots.insert(slot.kind, slot.operation_id);
                }
                vec![AstStreamResource {
                    id: uniffi_js_abi::StreamUseSiteId::new(id),
                    path: ValuePath::return_value(),
                    direction: uniffi_js_engine_schema::StreamDirection::Output,
                    item,
                    error: Some(ValueType::Scalar(ScalarType::String)),
                    is_send: false,
                    slot_operation_ids: slots,
                }]
            } else {
                Vec::new()
            };
            operations.push(AstOperation {
                id: OperationId::new(id),
                source_key: operation_key(&component, owner, kind, name),
                component_id: ComponentId::new(0),
                name: public.into(),
                debug_name: public.into(),
                kind,
                arguments: args,
                return_type: ret,
                async_kind,
                throws: None,
                receiver_type: receiver,
                callback_method_id,
                stream_slots,
                stream_resources,
            });
        };
        add(
            0,
            OperationOwner::Namespace,
            OperationKind::Function,
            "echo",
            "echo",
            vec![AstArgument {
                name: "profile".into(),
                ty: ValueType::Named(profile.clone()),
                default: None,
            }],
            Some(ValueType::Named(profile.clone())),
            AsyncKind::Sync,
            None,
            None,
            vec![],
        );
        add(
            1,
            OperationOwner::Object(service.clone()),
            OperationKind::Constructor,
            "new_service",
            "newService",
            vec![],
            Some(ValueType::Named(service.clone())),
            AsyncKind::Sync,
            None,
            None,
            vec![],
        );
        add(
            2,
            OperationOwner::Object(service.clone()),
            OperationKind::Method,
            "describe",
            "describe",
            vec![],
            Some(ValueType::Scalar(ScalarType::String)),
            AsyncKind::Sync,
            Some(service_id),
            None,
            vec![],
        );
        for (id, method, async_kind) in [
            (3, "onReady", AsyncKind::Sync),
            (4, "onChecked", AsyncKind::Sync),
            (5, "onEvent", AsyncKind::Async),
            (6, "onEventChecked", AsyncKind::Async),
        ] {
            add(
                id,
                OperationOwner::Callback(observer.clone()),
                OperationKind::CallbackMethod,
                method,
                method,
                vec![AstArgument {
                    name: "event".into(),
                    ty: ValueType::Named(event.clone()),
                    default: None,
                }],
                None,
                async_kind,
                None,
                Some(id - 3),
                vec![],
            );
        }
        add(
            7,
            OperationOwner::Namespace,
            OperationKind::OutputStreamStart,
            "events",
            "events",
            vec![],
            Some(ValueType::OutputStream {
                item: Box::new(ValueType::Named(event)),
                error: Some(Box::new(ValueType::Scalar(ScalarType::String))),
                is_send: false,
            }),
            AsyncKind::Sync,
            None,
            None,
            vec![
                AstStreamSlot {
                    kind: OperationKind::OutputStreamNext,
                    operation_id: OperationId::new(8),
                },
                AstStreamSlot {
                    kind: OperationKind::OutputStreamCancel,
                    operation_id: OperationId::new(9),
                },
            ],
        );
        add(
            10,
            OperationOwner::Namespace,
            OperationKind::Function,
            "fail",
            "fail",
            vec![],
            None,
            AsyncKind::Sync,
            None,
            None,
            vec![],
        );
        add(
            11,
            OperationOwner::Namespace,
            OperationKind::Function,
            "namespace_collision",
            "collision",
            vec![],
            Some(ValueType::Scalar(ScalarType::String)),
            AsyncKind::Sync,
            None,
            None,
            vec![],
        );
        add(
            12,
            OperationOwner::Value(profile.clone()),
            OperationKind::Method,
            "value_collision",
            "collision",
            vec![],
            Some(ValueType::Scalar(ScalarType::String)),
            AsyncKind::Sync,
            Some(profile_id),
            None,
            vec![],
        );
        add(
            13,
            OperationOwner::Object(service.clone()),
            OperationKind::Method,
            "object_collision",
            "collision",
            vec![],
            Some(ValueType::Scalar(ScalarType::String)),
            AsyncKind::Sync,
            Some(service_id),
            None,
            vec![],
        );
        add(
            14,
            OperationOwner::Namespace,
            OperationKind::Function,
            "get_service",
            "getService",
            vec![],
            Some(ValueType::Named(service.clone())),
            AsyncKind::Sync,
            None,
            None,
            vec![],
        );
        add(
            15,
            OperationOwner::Namespace,
            OperationKind::Function,
            "accept_service",
            "acceptService",
            vec![AstArgument {
                name: "service".into(),
                ty: ValueType::Named(service.clone()),
                default: None,
            }],
            Some(ValueType::Scalar(ScalarType::String)),
            AsyncKind::Sync,
            None,
            None,
            vec![],
        );
        operations
            .iter_mut()
            .find(|operation| operation.id == OperationId::new(10))
            .unwrap()
            .throws = Some(failure.clone());
        PublicAst {
            lifecycle: Default::default(),
            close_policy: ClosePolicy::default(),
            target_universe: vec![
                PublicTarget::NodeNapi,
                PublicTarget::BrowserWasm,
                PublicTarget::OhosNapi,
            ],
            components: vec![AstComponent {
                id: ComponentId::new(0),
                namespace: "corpus".into(),
            }],
            types,
            operations,
            streams: vec![AstStream {
                id: uniffi_js_abi::StreamUseSiteId::new(0),
                operation_id: OperationId::new(7),
                path: "return".into(),
                direction: uniffi_js_engine_schema::StreamDirection::Output,
            }],
            callbacks: vec![],
        }
    }

    #[test]
    fn runtime_is_plain_ecmascript_and_self_contained() {
        assert!(RUNTIME_SOURCE.contains("export class UniffiError"));
        assert!(!RUNTIME_SOURCE.contains("typescript/src"));
        assert!(!RUNTIME_SOURCE.contains("../../../"));
        assert!(!RUNTIME_SOURCE.contains("Function("));
        assert!(!RUNTIME_SOURCE.contains("eval("));
    }

    #[test]
    fn runtime_imports_split_type_only_and_normalize_typescript_specifiers() {
        let mut ast = corpus_ast();
        let custom = ast
            .types
            .iter_mut()
            .find_map(|ty| match &mut ty.kind {
                AstTypeKind::Custom { config, .. } => Some(config),
                _ => None,
            })
            .unwrap();
        custom.imports = vec![
            "import type { TypeOnly } from \"./types.ts\";".into(),
            "type { ShorthandOnly } from \"./shorthand.ts\";".into(),
            "import { convert } from \"./convert.ts\";".into(),
        ];
        let implementation =
            render_component_implementation(&ast, &ast.components[0], "js").unwrap();
        assert!(!implementation.contains("import type"));
        assert!(!implementation.contains("ShorthandOnly"));
        assert!(!implementation.contains(".ts\""));
        assert!(implementation.contains("./convert.js"));
    }

    #[test]
    fn scalar_shapes_are_canonical() {
        let empty = PublicAst {
            lifecycle: Default::default(),
            close_policy: ClosePolicy::default(),
            target_universe: vec![],
            components: vec![],
            types: vec![],
            operations: vec![],
            streams: vec![],
            callbacks: vec![],
        };
        assert_eq!(
            render_public_type(&empty, &ValueType::Scalar(ScalarType::I64)),
            "bigint"
        );
        assert_eq!(
            render_public_type(&empty, &ValueType::Scalar(ScalarType::Bytes)),
            "Uint8Array"
        );
        assert_eq!(
            render_public_type(
                &empty,
                &ValueType::Optional(Box::new(ValueType::Scalar(ScalarType::String)))
            ),
            "string | null"
        );
    }

    #[test]
    fn builder_consumes_real_minimal_ir_and_bridge_plans() {
        let component_key = ComponentKey::new("minimal").unwrap();
        let component_definition =
            ComponentDefinition::new(component_key.clone(), "minimal").unwrap();
        let identified_components = assign_component_ids([component_definition.clone()]).unwrap();
        let operation_definition = OperationDefinition::new(
            operation_key(
                &component_key,
                OperationOwner::Namespace,
                OperationKind::Function,
                "echo",
            ),
            "echo",
            "echo",
            "echo",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "value",
                    ValueType::Scalar(ScalarType::String),
                    uniffi_js_abi::Ownership::Borrowed,
                )
                .unwrap()],
                return_type: Some(ValueType::Scalar(ScalarType::String)),
                async_kind: AsyncKind::Sync,
                throws: None,
            },
        )
        .unwrap();
        let identified_operations = assign_operation_ids([operation_definition.clone()]).unwrap();
        let full_capabilities = [
            Capability::Primitive,
            Capability::String,
            Capability::SyncCall,
        ];
        let bridge = BridgePlan::build(BridgePlanInput {
            components: identified_components.clone(),
            types: vec![],
            operations: identified_operations
                .iter()
                .cloned()
                .map(PlannedOperation::new)
                .collect(),
            callbacks: vec![],
            streams: vec![],
            targets: vec![EngineCapabilities::new(EngineKind::Napi, full_capabilities)],
            close_policy: ClosePolicy::default(),
        })
        .unwrap();
        let operation_id = identified_operations[0].id;
        let rust = RustBridgePlan {
            engines: BTreeMap::from([(
                EngineKind::Napi,
                EngineRustBridgePlan {
                    engine: EngineKind::Napi,
                    operations: vec![RustOperationPlan {
                        operation_id,
                        source_key: operation_definition.source_key.clone(),
                        component_id: ComponentId::new(0),
                        owner: OperationOwner::Namespace,
                        kind: OperationKind::Function,
                        async_kind: AsyncKind::Sync,
                        rust_async: false,
                        rust_return_is_result: false,
                        rust_stream_item_is_result: false,
                        callback_method_id: None,
                        call_target: RustCallTarget::FreeFunction {
                            module: RustPath::new(["minimal"]),
                            item: "echo".into(),
                        },
                        receiver: None,
                        arguments: vec![],
                        return_value: None,
                        throws: None,
                        resource_hooks: vec![],
                        stream_resources: vec![],
                    }],
                },
            )]),
            named_types: vec![],
        };
        let api = JsApiIr {
            target_universe: vec![PublicTarget::NodeNapi],
            resolved_config: ResolvedJsConfig {
                wasm_platform_exports: Default::default(),
                lifecycle: Default::default(),
                custom_types: BTreeMap::new(),
            },
            components: vec![JsComponent {
                id: ComponentId::new(0),
                source_key: component_key.clone(),
                public_namespace: component_definition.public_namespace,
            }],
            types: vec![],
            operations: vec![JsOperation {
                id: operation_id,
                source_key: operation_definition.source_key,
                component_id: ComponentId::new(0),
                public_name: "echo".into(),
                debug_name: "echo".into(),
                kind: OperationKind::Function,
                arguments: vec![JsArgument {
                    public_name: "value".into(),
                    ty: ValueType::Scalar(ScalarType::String),
                    default: None,
                }],
                return_type: Some(ValueType::Scalar(ScalarType::String)),
                async_kind: AsyncKind::Sync,
                throws: None,
                receiver: None,
                callback_method_id: None,
            }],
            required_capabilities: CapabilitySet::new(full_capabilities),
        };
        let facade = FacadeBuilder::new(&api, &bridge, &rust).build().unwrap();
        assert_eq!(facade.ast.components.len(), 1);
        assert!(facade
            .shared_files()
            .iter()
            .any(|file| file.path == "components/minimal/index.js"));
        assert_eq!(
            facade
                .ark_files()
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["platform.ets", "platform.d.ets", "Index.ets", "Index.d.ets"]
        );
        assert_ne!(facade.shared_files()[0].bytes, facade.ark_files()[0].bytes);
    }

    #[test]
    fn ark_inventory_is_an_explicit_package_pair_with_static_surface_checks() {
        let ast = corpus_ast();
        let shared = render_inventory(&ast).unwrap();
        let ark = render_ark_inventory(&ast).unwrap();
        assert_eq!(shared[0].bytes, RUNTIME_SOURCE.as_bytes());
        assert!(shared.iter().any(|file| file.path.ends_with(".js")));
        assert!(shared.iter().any(|file| file.path.ends_with(".d.ts")));
        assert_eq!(
            ark.iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["platform.ets", "platform.d.ets", "Index.ets", "Index.d.ets"]
        );
        for file in &ark {
            let text = String::from_utf8(file.bytes.clone()).unwrap();
            if file.path == "platform.ets" {
                assert!(!text.contains("ClosePolicy"));
                assert!(!text.contains("deadlineTimer"));
                assert!(text.contains("class __ArkDetachedMarker"));
                assert!(text.contains("environmentDestroyed(): void"));
                assert!(!text.contains("Object.freeze({"));
                assert!(!text.contains("const __DETACHED = \"__uniffi_detached__\""));
                assert!(
                    text.contains("if (closed) { await closePromise; return new ArkDoneStep(); }")
                );
                assert!(text.contains("return guarded as ArkValue"));
                assert!(text.contains(
                    "new Promise<void>((resolve): void => { this.idleWaiters.add(resolve); })"
                ));
                assert!(text.contains("function __arkCallbackFailure(error: Error)"));
                assert!(text.contains("function __arkAsUniffiError(error: Error)"));
                assert!(text.contains(
                    "function __arkLowerBytes(value: Uint8Array): ArkValue { return value; }"
                ));
                assert!(text.contains("readonly error?: ArkValue"));
                assert!(text.contains("function __arkCallbackErrorCarrier(failure: ArkFailure)"));
                assert!(text.contains("if (error instanceof __ArkWireError)"));
                assert!(text.contains("const candidate: UniffiError = error as UniffiError"));
                assert!(!text.contains("error as ArkValue"));
                assert!(!text.contains(".catch((error: ArkValue)"));
                assert!(text.contains("new ArkHostErrorStep(lowerError(step.error))"));
                assert!(text.contains("export class Failure extends UniffiError"));
                assert!(!text.contains("export interface FailureConstructor"));
                assert!(!text.contains("Failure_RejectedInput"));
                assert!(!text.contains("readonly Failure: FailureConstructor"));
            } else if file.path == "platform.d.ets" {
                assert!(!text.contains("ClosePolicy"));
                assert!(!text.contains("configureClosePolicy"));
                assert!(text.contains("export declare class Failure extends UniffiError"));
                assert!(!text.contains("export interface FailureConstructor"));
                assert!(!text.contains("Failure_RejectedInput"));
            }
            for forbidden in [
                "unknown",
                "Record<",
                "typeof ",
                "IteratorResult<",
                "Object.prototype",
                ".call(",
                "Object.create",
                "Symbol",
            ] {
                assert!(
                    !text.contains(forbidden),
                    "Ark file {} contains forbidden {}",
                    file.path,
                    forbidden
                );
            }
        }
        let ark_declaration = String::from_utf8(
            ark.iter()
                .find(|file| file.path == "platform.d.ets")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(ark_declaration.contains("export interface ArkCallbackResult"));
        assert!(ark_declaration.contains("readonly error_message?: string"));
        let declaration = String::from_utf8(
            shared
                .iter()
                .find(|file| file.path == "components/corpus/index.d.ts")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(declaration.contains("readonly message"));
        assert!(declaration.contains("onReady"));
        assert!(declaration.contains("private constructor"));
        assert!(declaration.contains("../../shared/uniffi_runtime.js"));
        assert!(!declaration.contains("uniffi_runtime.d.js"));
        assert!(
            !declaration.contains("export interface Namespace {\nexport declare const createApi")
        );
    }

    #[test]
    fn declarations_keep_named_constructors_and_error_values_callable() {
        let mut shared_ast = corpus_ast();
        shared_ast
            .operations
            .iter_mut()
            .find(|operation| operation.id == OperationId::new(1))
            .expect("corpus has an object constructor")
            .name = "new".into();
        let shared = render_component_declaration(&shared_ast, &shared_ast.components[0]).unwrap();
        assert!(shared.contains("readonly new: () => Service;"));
        assert!(!shared.contains("\n  new(): Service;"));
        assert!(shared.contains("readonly UniffiError: typeof UniffiError;"));
        assert!(shared.contains("readonly Failure: typeof Failure;"));

        let mut ark_ast = corpus_ast();
        let component = ComponentKey::new("corpus").unwrap();
        let profile = key(&component, "Profile");
        let constructor = ark_ast
            .operations
            .iter_mut()
            .find(|operation| operation.id == OperationId::new(12))
            .expect("corpus has a value operation");
        constructor.kind = OperationKind::Constructor;
        constructor.source_key = operation_key(
            &component,
            OperationOwner::Value(profile.clone()),
            OperationKind::Constructor,
            "new",
        );
        constructor.name = "new".into();
        constructor.return_type = Some(ValueType::Named(profile));
        constructor.receiver_type = None;
        let declaration = String::from_utf8(
            render_ark_inventory(&ark_ast)
                .unwrap()
                .into_iter()
                .find(|file| file.path == "platform.d.ets")
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(declaration.contains("readonly new: () => Profile;"));
        assert!(!declaration.contains("new: (self_: Profile"));
    }

    #[test]
    fn custom_type_imports_use_private_aliases_before_public_reexport() {
        let mut ast = corpus_ast();
        let custom = ast
            .types
            .iter_mut()
            .find(|ty| matches!(ty.kind, AstTypeKind::Custom { .. }))
            .expect("corpus has a custom type");
        custom.name = "EmailAddress".into();
        let AstTypeKind::Custom { config, .. } = &mut custom.kind else {
            unreachable!();
        };
        config.public_type_name = "EmailAddress".into();
        config.imports = vec![
            "type { EmailAddress } from \"../../support/email.js\"".into(),
            "{ emailAddressFromString } from \"../../support/email.js\"".into(),
        ];

        let shared = render_component_declaration(&ast, &ast.components[0])
            .expect("custom declaration should render");
        assert!(shared.contains(
            "import type { EmailAddress as __UniffiCustomType5 } from \"../../support/email.js\";"
        ));
        assert!(shared.contains("export type EmailAddress = __UniffiCustomType5;"));
        assert!(!shared.contains("export type EmailAddress = EmailAddress;"));

        let ark = ark::render_inventory(&ast).expect("custom ArkTS inventory should render");
        for file in ark {
            let source = String::from_utf8(file.bytes).expect("ArkTS output is UTF-8");
            if file.path == "platform.ets" || file.path == "platform.d.ets" {
                assert!(source.contains(
                    "import type { EmailAddress as __UniffiCustomType5 } from \"./support/email.js\";"
                ));
                assert!(source.contains("export type EmailAddress = __UniffiCustomType5;"));
            } else {
                assert!(source.contains("EmailAddress,"));
                assert!(source.contains("} from \"./platform\";"));
                assert!(!source.contains("__UniffiCustomType5"));
            }
            assert!(!source.contains("export type EmailAddress = EmailAddress;"));
        }
        check_ark_business_types(&ast, r#"
import type { EmailAddress } from './Index';
import type { EmailAddress as ImportedEmailAddress } from './support/email.js';
const exported: EmailAddress = {value:'person@example.test'};
const original: ImportedEmailAddress = exported;
const sameType: EmailAddress = original;
// @ts-expect-error the public alias preserves the custom domain type
const invalid: EmailAddress = 'person@example.test';
// @ts-expect-error private import aliases stay private
import type { __UniffiCustomType5 } from './Index';
// @ts-expect-error business imports do not expose host state
import { BackendSession } from './Index';
"#, &[("support/email.ts", "export interface EmailAddress { readonly value: string; }\nexport function emailAddressFromString(value: string): EmailAddress { return {value}; }\n")]);
    }

    #[test]
    fn ark_custom_conversion_uses_runtime_direction() {
        let component = ComponentKey::new("corpus").unwrap();
        let custom = key(&component, "CustomName");
        let mut ast = corpus_ast();
        let custom_type = ast
            .types
            .iter_mut()
            .find(|ty| ty.source_key == custom)
            .expect("corpus has a custom type");
        let AstTypeKind::Custom { config, .. } = &mut custom_type.kind else {
            panic!("corpus custom type kind changed");
        };
        config.into_custom = "intoCustom({})".into();
        config.from_custom = "fromCustom({})".into();
        ast.operations.push(AstOperation {
            id: OperationId::new(16),
            source_key: operation_key(
                &component,
                OperationOwner::Namespace,
                OperationKind::Function,
                "roundtrip_custom",
            ),
            component_id: ComponentId::new(0),
            name: "roundtripCustom".into(),
            debug_name: "roundtripCustom".into(),
            kind: OperationKind::Function,
            arguments: vec![AstArgument {
                name: "value".into(),
                ty: ValueType::Named(custom.clone()),
                default: None,
            }],
            return_type: Some(ValueType::Named(custom)),
            async_kind: AsyncKind::Sync,
            throws: None,
            receiver_type: None,
            callback_method_id: None,
            stream_slots: vec![],
            stream_resources: vec![],
        });
        let source = String::from_utf8(
            render_ark_inventory(&ast)
                .unwrap()
                .into_iter()
                .find(|file| file.path == "platform.ets")
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(source.contains("fromCustom("));
        assert!(source.contains("intoCustom(__ark_nested_lift"));
        assert!(!source.contains("intoCustom(value as string"));
        assert!(!source.contains("fromCustom(__ark_nested_lift"));
    }

    #[test]
    fn ark_callback_error_payload_lowers_through_typed_fields() {
        let mut ast = corpus_ast();
        let component = ComponentKey::new("corpus").unwrap();
        let failure = key(&component, "Failure");
        ast.operations
            .iter_mut()
            .find(|operation| operation.id == OperationId::new(3))
            .expect("corpus has a callback method")
            .throws = Some(failure);
        let source = String::from_utf8(
            render_ark_inventory(&ast)
                .unwrap()
                .into_iter()
                .find(|file| file.path == "platform.ets")
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(source.contains("const payload: Failure_RejectedData = declared.data"));
        assert!(source.contains("const result: __ArkRecordCarrier2 = {}"));
        assert!(source.contains("result.message = "));
        assert!(source.contains("payload.message as string"));
        assert!(source.contains("return new __ArkWireError(\"Failure\", declared.message"));
        assert!(!source.contains("const payloadValue: ArkValue = declared.data"));
        assert!(!source.contains("__arkRecordField2(payloadValue"));
        assert!(!source.contains("Failure_RejectedInput"));
    }

    fn ark_error_payload_ast() -> PublicAst {
        let mut ast = corpus_ast();
        let component = ComponentKey::new("corpus").unwrap();
        let failure = ValueType::Named(key(&component, "Failure"));
        let profile = ValueType::Named(key(&component, "Profile"));
        let color = ValueType::Named(key(&component, "Color"));
        let field = |name: &str, ty: ValueType| AstField {
            name: name.into(),
            ty,
            default: None,
        };
        if let AstTypeKind::Record { fields } = &mut ast.types[0].kind {
            fields.push(field("code", color.clone()));
        }
        ast.types[2].kind = AstTypeKind::Error {
            variants: vec![
                AstVariant {
                    name: "Rejected".into(),
                    fields: vec![field("code", color), field("context", profile)],
                },
                AstVariant {
                    name: "Closed".into(),
                    fields: vec![],
                },
            ],
        };
        ast.operations[0].arguments[0].ty = failure.clone();
        ast.operations[0].return_type = Some(failure.clone());
        for operation in &mut ast.operations {
            if [3, 5, 11].contains(&operation.id.index()) {
                operation.throws = Some(key(&component, "Failure"));
            }
            if operation.id.index() == 11 {
                operation.async_kind = AsyncKind::Async;
            }
            if operation.id.index() == 7 {
                if let Some(ValueType::OutputStream { error, .. }) = &mut operation.return_type {
                    *error = Some(Box::new(failure.clone()));
                }
                operation.stream_resources[0].error = Some(failure.clone());
            }
            if operation.id.index() == 15 {
                operation.arguments = vec![
                    AstArgument {
                        name: "observer".into(),
                        ty: ValueType::Named(key(&component, "Observer")),
                        default: None,
                    },
                    AstArgument {
                        name: "source".into(),
                        ty: ValueType::InputStream {
                            item: Box::new(ValueType::Scalar(ScalarType::String)),
                            error: Some(Box::new(failure.clone())),
                            is_send: false,
                        },
                        default: None,
                    },
                ];
                operation.async_kind = AsyncKind::Async;
            }
        }
        ast.callbacks.push(AstCallbackUseSite {
            operation_id: OperationId::new(15),
            callback_type: TypeId::new(4),
            path: ValuePath::argument(0),
            contract: CallbackContract {
                retention: CallbackRetention::Scoped,
                threading: CallbackThreading::CallingThread,
                reentrancy: CallbackReentrancy::Allowed,
            },
        });
        ast
    }

    #[test]
    fn ark_error_payloads_keep_domain_types_separate_from_wire_values() {
        let ast = ark_error_payload_ast();
        for file in render_ark_inventory(&ast).unwrap() {
            let source = String::from_utf8(file.bytes).unwrap();
            if file.path == "platform.ets" || file.path == "platform.d.ets" {
                assert!(source.contains("export interface Failure_RejectedData"));
                assert!(source.contains("readonly code: Color;"));
                assert!(source.contains("readonly context: Profile;"));
                assert!(source.contains("readonly data: Failure_RejectedData | null;"));
                assert!(source.contains("export type ArkValue = ArkPrimitive | Uint8Array | Date | ArkRecord | Array<ArkValue> | Map<ArkValue, ArkValue> | Set<ArkValue> | null;"));
            } else {
                assert!(source.contains("Failure, Failure_RejectedData,"));
                assert!(source.contains("} from \"./platform\";"));
                assert!(!source.contains("ArkValue"));
            }
            assert!(!source.contains("data.set(\"code\""));
            assert!(!source.contains("const payloadValue: ArkValue = declared.data"));
        }
        check_ark_business_types(
            &ast,
            r#"
import { Failure } from './Index';
import type { Color, Failure_RejectedData, Profile } from './Index';
import { ArkRecord } from './platform';
declare const context: Profile;
declare const code: Color;
const data: Failure_RejectedData = {code, context};
const failure = new Failure('failed', 'Rejected', data);
if (failure.data !== null) {
  const actual: Color = failure.data.code;
  const nested: Color = failure.data.context.code;
}
// @ts-expect-error enum fields cannot be replaced by wire strings
new Failure('failed', 'Rejected', {code:'Red', context});
// @ts-expect-error domain enums do not belong in wire records
new ArkRecord().set('code', code);
// @ts-expect-error typed payloads have no legacy get method
data.get('code');
// @ts-expect-error wire types stay in the platform entry
import { ArkRecord as LeakedWireRecord } from './Index';
"#,
            &[],
        );
    }

    #[test]
    #[ignore = "requires UNIFFI_ARK_TYPESCRIPT pointing to the DevEco SDK TypeScript module"]
    fn ark_error_payloads_compile_and_round_trip() {
        let compiler = std::env::var("UNIFFI_ARK_TYPESCRIPT").expect("ArkTS compiler module path");
        let root = std::env::temp_dir().join(format!(
            "uniffi-ark-errors-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        for file in render_ark_inventory(&ark_error_payload_ast()).unwrap() {
            fs::write(root.join(file.path), file.bytes).unwrap();
        }
        fs::write(
            root.join("check.cjs"),
            include_str!("../tests/ark_error_payloads.cjs"),
        )
        .unwrap();
        let output = Command::new("node")
            .arg("check.cjs")
            .env("UNIFFI_ARK_TYPESCRIPT", compiler)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ArkTS error payload regression failed in {}: {}\n{}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        println!("{}", String::from_utf8_lossy(&output.stdout));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn ark_composite_owner_and_stream_surface_is_canonical() {
        let mut ast = corpus_ast();
        let component = ComponentKey::new("corpus").unwrap();
        let color = key(&component, "Color");
        ast.operations.push(AstOperation {
            id: OperationId::new(16),
            source_key: operation_key(
                &component,
                OperationOwner::Namespace,
                OperationKind::Function,
                "roundtrip_color",
            ),
            component_id: ComponentId::new(0),
            name: "roundtripColor".into(),
            debug_name: "roundtripColor".into(),
            kind: OperationKind::Function,
            arguments: vec![AstArgument {
                name: "value".into(),
                ty: ValueType::Named(color.clone()),
                default: None,
            }],
            return_type: Some(ValueType::Named(color)),
            async_kind: AsyncKind::Sync,
            throws: None,
            receiver_type: None,
            callback_method_id: None,
            stream_slots: vec![],
            stream_resources: vec![],
        });
        ast.operations.push(AstOperation {
            id: OperationId::new(17),
            source_key: operation_key(
                &component,
                OperationOwner::Namespace,
                OperationKind::Function,
                "flush_async",
            ),
            component_id: ComponentId::new(0),
            name: "flushAsync".into(),
            debug_name: "flushAsync".into(),
            kind: OperationKind::Function,
            arguments: vec![],
            return_type: None,
            async_kind: AsyncKind::Async,
            throws: None,
            receiver_type: None,
            callback_method_id: None,
            stream_slots: vec![],
            stream_resources: vec![],
        });
        let implementation = String::from_utf8(
            render_ark_inventory(&ast)
                .unwrap()
                .into_iter()
                .find(|file| file.path == "platform.ets")
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(implementation.contains("__invokeValue12(self: Profile, session: BackendSession)"));
        assert!(implementation.contains("__invokeObject13(self: Service, session: BackendSession"));
        assert!(implementation.contains("const __arkEnum_Event: __ArkEnumEventVariants"));
        assert!(implementation.contains("Event: __corpus_Event"));
        assert!(implementation.contains("Color: __corpus_Color"));
        assert!(implementation.contains("readonly error: (value: ArkValue) => UniffiError"));
        assert!(implementation
            .contains("release: (handle: ArkValue): void => session.releaseOutputStream(handle)"));
        assert!(!implementation.contains("fromHandle"));
        assert!(!implementation.contains("handleValue"));
        assert!(!implementation.contains("export const Event:"));
        assert!(implementation.contains("function __arkStreamStepField"));
        assert!(implementation.contains("error_message: failure.message"));
        assert!(implementation
            .contains("if (!(source instanceof Map)) throw new UniffiError(\"UniffiMapType\""));
        assert!(implementation.contains("if (frame.ended) return frame.cleanup;"));
        assert!(implementation.contains("return __arkLiftVoid(value);"));
        assert!(!implementation.contains("__arkLiftVoid(value, session)"));
        assert!(implementation.contains("if (value.tag === \"Red\") return \"Red\" as ArkValue;"));
        assert!(implementation
            .contains("const rawCarrier: __ArkRecordCarrier6 = rawValue as __ArkRecordCarrier6"));
        assert!(!implementation.contains(".then((value: ArkValue): void => { session.endCallFrame"));
        assert!(!implementation.contains("}, (error: Error):"));
    }

    #[test]
    fn bridge_callback_contract_lowers_through_generated_operation() {
        let component_key = ComponentKey::new("callback_e2e").unwrap();
        let component_definition =
            ComponentDefinition::new(component_key.clone(), "callback_e2e").unwrap();
        let components = assign_component_ids([component_definition]).unwrap();
        let listener_key = key(&component_key, "Listener");
        let types = assign_type_ids([TypeDefinition::new(
            listener_key.clone(),
            "Listener",
            NamedTypeKind::Object {
                kind: uniffi_js_abi::ObjectKind::TraitForeignOnly,
            },
        )
        .unwrap()])
        .unwrap();
        let listener_id = types[0].id;
        let run_definition = OperationDefinition::new(
            operation_key(
                &component_key,
                OperationOwner::Namespace,
                OperationKind::Function,
                "run",
            ),
            "run",
            "run",
            "run",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "listener",
                    ValueType::Named(listener_key.clone()),
                    uniffi_js_abi::Ownership::Borrowed,
                )
                .unwrap()],
                return_type: Some(ValueType::Scalar(ScalarType::String)),
                async_kind: AsyncKind::Sync,
                throws: None,
            },
        )
        .unwrap();
        let method_definition = OperationDefinition::new(
            operation_key(
                &component_key,
                OperationOwner::Callback(listener_key.clone()),
                OperationKind::CallbackMethod,
                "on_event",
            ),
            "onEvent",
            "onEvent",
            "on_event",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "value",
                    ValueType::Scalar(ScalarType::String),
                    uniffi_js_abi::Ownership::Borrowed,
                )
                .unwrap()],
                return_type: Some(ValueType::Scalar(ScalarType::String)),
                async_kind: AsyncKind::Sync,
                throws: None,
            },
        )
        .unwrap()
        .with_callback_method_id(0);
        let identified_operations =
            assign_operation_ids([run_definition.clone(), method_definition.clone()]).unwrap();
        let run = identified_operations
            .iter()
            .find(|operation| operation.definition.public_name == "run")
            .unwrap();
        let method = identified_operations
            .iter()
            .find(|operation| operation.definition.public_name == "onEvent")
            .unwrap();
        let capabilities = [
            Capability::Primitive,
            Capability::String,
            Capability::Callback,
            Capability::RetainedCallback,
            Capability::ObjectLease,
            Capability::SyncCall,
        ];
        let bridge = BridgePlan::build(BridgePlanInput {
            components: components.clone(),
            types: types.clone(),
            operations: identified_operations
                .iter()
                .cloned()
                .map(PlannedOperation::new)
                .collect(),
            callbacks: vec![CallbackUseSite {
                operation_id: run.id,
                callback_type: listener_id,
                path: ValuePath::argument(0),
                contract: CallbackContract {
                    retention: CallbackRetention::Retained,
                    threading: CallbackThreading::CallingThread,
                    reentrancy: CallbackReentrancy::Forbidden,
                },
            }],
            streams: vec![],
            targets: vec![EngineCapabilities::new(EngineKind::Napi, capabilities)],
            close_policy: ClosePolicy::default(),
        })
        .unwrap();
        assert_eq!(bridge.callbacks().len(), 1);
        let rust_operations = identified_operations
            .iter()
            .map(|operation| RustOperationPlan {
                operation_id: operation.id,
                source_key: operation.definition.source_key.clone(),
                component_id: components[0].id,
                owner: operation.definition.source_key.owner().clone(),
                kind: operation.definition.source_key.kind(),
                async_kind: operation.definition.signature.async_kind,
                rust_async: operation.definition.signature.async_kind == AsyncKind::Async,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                callback_method_id: operation.definition.callback_method_id,
                call_target: if matches!(
                    operation.definition.source_key.owner(),
                    OperationOwner::Callback(_)
                ) {
                    RustCallTarget::CallbackMethod {
                        callback: RustPath::new(["callback_e2e", "Listener"]),
                        callback_type: listener_id,
                        method_id: operation.definition.callback_method_id.unwrap(),
                        item: operation.definition.public_name.clone(),
                    }
                } else {
                    RustCallTarget::FreeFunction {
                        module: RustPath::new(["callback_e2e"]),
                        item: operation.definition.public_name.clone(),
                    }
                },
                receiver: None,
                arguments: vec![],
                return_value: None,
                throws: None,
                resource_hooks: vec![],
                stream_resources: vec![],
            })
            .collect();
        let rust = RustBridgePlan {
            engines: BTreeMap::from([(
                EngineKind::Napi,
                EngineRustBridgePlan {
                    engine: EngineKind::Napi,
                    operations: rust_operations,
                },
            )]),
            named_types: vec![],
        };
        let api = JsApiIr {
            target_universe: vec![PublicTarget::NodeNapi],
            resolved_config: ResolvedJsConfig {
                wasm_platform_exports: Default::default(),
                lifecycle: Default::default(),
                custom_types: BTreeMap::new(),
            },
            components: vec![JsComponent {
                id: components[0].id,
                source_key: component_key.clone(),
                public_namespace: "callback_e2e".into(),
            }],
            types: vec![JsType {
                id: listener_id,
                source_key: listener_key.clone(),
                public_name: "Listener".into(),
                kind: JsTypeKind::Object {
                    kind: uniffi_js_abi::ObjectKind::TraitForeignOnly,
                },
            }],
            operations: identified_operations
                .iter()
                .map(|operation| JsOperation {
                    id: operation.id,
                    source_key: operation.definition.source_key.clone(),
                    component_id: components[0].id,
                    public_name: operation.definition.public_name.clone(),
                    debug_name: operation.definition.debug_name.clone(),
                    kind: operation.definition.source_key.kind(),
                    arguments: operation
                        .definition
                        .signature
                        .arguments
                        .iter()
                        .map(|argument| uniffi_js_abi::JsArgument {
                            public_name: argument.public_name.clone(),
                            ty: argument.ty.clone(),
                            default: argument.default.clone(),
                        })
                        .collect(),
                    return_type: operation.definition.signature.return_type.clone(),
                    async_kind: operation.definition.signature.async_kind,
                    throws: None,
                    receiver: None,
                    callback_method_id: operation.definition.callback_method_id,
                })
                .collect(),
            required_capabilities: CapabilitySet::new(capabilities),
        };
        let facade = FacadeBuilder::new(&api, &bridge, &rust).build().unwrap();
        let declaration = String::from_utf8(
            facade
                .shared_files()
                .iter()
                .find(|file| file.path == "components/callback_e2e/index.d.ts")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(declaration
            .contains("export interface Listener {\n  onEvent(value: string): string;\n}"));
        assert!(!declaration.contains("export declare class Listener"));
        for file in facade.ark_files() {
            let ark = String::from_utf8(file.bytes.clone()).unwrap();
            if file.path == "platform.ets" || file.path == "platform.d.ets" {
                assert!(ark.contains("export interface Listener"));
            } else {
                assert!(ark.contains("Listener,"));
                assert!(ark.contains("} from \"./platform\";"));
            }
            assert!(!ark.contains("class Listener extends ObjectLease"));
            assert!(!ark.contains("Listener.__arkCreate"));
        }
        check_ark_business_types(
            &facade.ast,
            r#"
import type { Listener } from './Index';
const listener: Listener = {onEvent(value: string): string { return value; }};
const result: string = listener.onEvent('event');
// @ts-expect-error the business callback retains its argument type
listener.onEvent(1);
"#,
            &[],
        );
        let implementation = String::from_utf8(
            facade
                .shared_files()
                .iter()
                .find(|file| file.path == "components/callback_e2e/internal.js")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        let root = std::env::temp_dir().join(format!(
            "uniffi-js-callback-e2e-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("components/callback_e2e")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::write(root.join("package.json"), "{\"type\":\"module\"}").unwrap();
        fs::write(root.join("shared/uniffi_runtime.js"), RUNTIME_SOURCE).unwrap();
        fs::write(
            root.join("components/callback_e2e/internal.js"),
            implementation,
        )
        .unwrap();
        let script = format!(
            r#"
import {{ createNamespace }} from "./components/callback_e2e/internal.js";
import {{ BackendSession, Host, UniffiError, lowerValue }} from "./shared/uniffi_runtime.js";
function makeSession(backend, host = new Host()) {{
  const callControl = {{create: 10000, cancel: 10001, release: 10002, operations: []}};
  let nextCallId = 1;
  const calls = new Set();
  return new BackendSession({{...backend, invokeSync(id, args) {{
    if (id === callControl.create) {{ const callId = nextCallId++; calls.add(callId); return {{kind:"value", value:callId}}; }}
    if (id === callControl.cancel || id === callControl.release) {{
      if (!calls.has(args[0])) throw Error("unknown call ID");
      if (id === callControl.release) calls.delete(args[0]);
      return {{kind:"value", value:null}};
    }}
    return backend.invokeSync(id, args);
  }}}}, host, {{callControl}});
}}
const host = new Host();
let session;
let retainedCallback = null;
const engine = {{
  invokeSync(id, args) {{
    if (id !== {run_id}) return {{kind:"value", value:null}};
    host.retainCallback({listener_id}, args[0]);
    retainedCallback = args[0];
    const result = host.invokeCallbackSync({listener_id}, args[0], 0, ["hello"]);
    return {{kind:"value", value:result}};
  }},
  async invokeAsync() {{ return {{kind:"value", value:null}}; }},
  close() {{}},
}};
session = makeSession(engine, host);
const api = createNamespace(session);
if (api.run({{ onEvent(value) {{ return value + "!"; }} }}) !== "hello!") throw Error("callback operation");
if (session.callbacks.callbacks.size !== 1) throw Error("retained callback lease");
host.releaseCallback({listener_id}, retainedCallback);
if (session.callbacks.callbacks.size !== 0) throw Error("retained callback release");
try {{ host.invokeCallbackSync({listener_id}, 999, 0, []); throw Error("unknown callback"); }} catch (error) {{ if (error.errorName !== "UniffiCallbackMissing") throw error; }}
const recordContext = {{types: {{
  100: {{kind:"record", fields: {{count: {{type: {{kind:"scalar", name:"I64"}}}}}}}},
  101: {{kind:"enum", error:true, unit:false, variants: {{Failed: {{fields: {{count: {{type: {{kind:"scalar", name:"I64"}}}}}}}}}}}},
  22: {{kind:"callback", methods: {{}}}},
  110: {{kind:"record", fields: {{listener: {{type: {{kind:"named", name:"Listener", typeId:22}}}}}}}},
}}}};
const recordType = {{kind:"named", name:"Record", typeId:100}};
const mixedRecord = session.registerCallback(22, {{
  sync(value) {{ if (value.count !== 3n) throw Error("record lift"); return {{count:value.count + 1n}}; }},
  async(value) {{ return {{count:value.count + 2n}}; }},
  fail() {{ throw new UniffiError({{errorName:"Failure", variant:"Failed", data:{{count:4n}}}}); }},
}}, {{context: recordContext, retention:"scoped", reentrancy:"allowed", methods: {{
  0: {{name:"sync", async:false, args:[recordType], returnType:recordType}},
  1: {{name:"async", async:true, args:[recordType], returnType:recordType}},
  2: {{name:"fail", async:false, args:[], returnType:null, throws:{{name:"Failure", typeId:101}}}},
}}}});
const syncRecord = session.invokeCallbackSync(22, mixedRecord, 0, [{{count:3n}}]);
if (syncRecord.count !== 4n) throw Error("record bigint conversion");
const overlap = await Promise.all([
  session.invokeCallbackAsync(22, mixedRecord, 1, 100, [{{count:3n}}]),
  session.invokeCallbackAsync(22, mixedRecord, 1, 101, [{{count:5n}}]),
]);
if (overlap[0].count !== 5n || overlap[1].count !== 7n) throw Error("allowed overlap");
try {{ session.invokeCallbackSync(22, mixedRecord, 2, []); throw Error("fallible callback"); }} catch (error) {{ if (error.errorName !== "Failure" || error.variant !== "Failed" || error.data.count !== 4n) throw error; }}
const scopedFrame = session.beginCallFrame();
const scoped = session.registerCallback(23, () => 1, {{retention:"scoped", methods:{{0:{{name:null}}}}}});
session.endCallFrame(scopedFrame);
if (session.callbacks.callbacks.has(`23:${{scoped}}`)) throw Error("scoped cleanup");
let forbiddenId = 0;
forbiddenId = session.registerCallback(24, {{reenter() {{ try {{ session.invokeCallbackSync(24, forbiddenId, 0, []); }} catch (error) {{ if (error.errorName !== "UniffiCallbackReentrancy") throw error; return "blocked"; }} return "bad"; }}}}, {{reentrancy:"forbidden", methods:{{0:{{name:"reenter", async:false}}}}}});
if (session.invokeCallbackSync(24, forbiddenId, 0, []) !== "blocked") throw Error("forbidden reentrancy");
const nestedFrame = session.beginCallFrame();
const nested = lowerValue({{listener: () => "nested"}}, {{kind:"named", name:"RecordWithListener", typeId:110}}, recordContext, session, {{callbackContracts: {{"argument[0].field[listener]": {{callbackTypeId:22, retention:"scoped", threading:"callingThread", reentrancy:"forbidden"}}}}}}, "argument[0]");
if (typeof nested.listener !== "number") throw Error("nested callback contract");
session.endCallFrame(nestedFrame);
try {{ lowerValue(() => 1, {{kind:"named", name:"Listener", typeId:22}}, recordContext, session, {{callbackContracts:{{}}}}, "argument[0]"); throw Error("missing callback contract"); }} catch (error) {{ if (error.errorName !== "UniffiCallbackContract") throw error; }}
try {{ lowerValue(() => 1, {{kind:"named", name:"Listener", typeId:22}}, recordContext, session, {{callbackContracts:{{"argument[0]":{{callbackTypeId:99, retention:"scoped", threading:"callingThread", reentrancy:"forbidden"}}}}}}, "argument[0]"); throw Error("callback type mismatch"); }} catch (error) {{ if (error.errorName !== "UniffiCallbackContract") throw error; }}
await session.close();
"ok";
"#,
            run_id = run.id.index(),
            listener_id = listener_id.index(),
        );
        fs::write(root.join("check.mjs"), script).unwrap();
        let output = Command::new("node")
            .arg("check.mjs")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "node callback facade failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(root);
        let _ = method;
    }

    #[test]
    fn normalized_u3s_composite_stream_fixture_executes_canonical_slots() {
        // Build the fixture through the real frontend normalization pass.  In
        // particular, the stream groups and synthetic pull/cancel operations
        // below come from U3S metadata; this test does not hand-author a
        // substitute AstStreamResource or operation-slot table.
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: "normalized_streams_crate".into(),
                name: "normalized_streams".into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        let input = Type::InputStream {
            item_type: Box::new(Type::UInt32),
            error_type: Some(Box::new(Type::String)),
            is_send: false,
        };
        let output = Type::Stream {
            item_type: Box::new(Type::String),
            error_type: Some(Box::new(Type::UInt32)),
            is_send: false,
        };
        group.add_item(
            FnMetadata {
                module_path: "normalized_streams_crate".into(),
                name: "consume".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![FnParamMetadata::simple("source", input.clone())],
                return_type: None,
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            FnMetadata {
                module_path: "normalized_streams_crate".into(),
                name: "events".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: true,
                inputs: vec![FnParamMetadata::simple("source", input)],
                return_type: Some(output),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            FnMetadata {
                module_path: "normalized_streams_crate".into(),
                name: "produce".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: true,
                inputs: vec![],
                return_type: Some(Type::Stream {
                    item_type: Box::new(Type::String),
                    error_type: Some(Box::new(Type::UInt32)),
                    is_send: false,
                }),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        let component = Component {
            ci: ComponentInterface::from_metadata(group).unwrap(),
            config: JsConfig::default(),
        };
        let normalized = normalize(
            BindingInput::new(&[component])
                .with_close_policy(ClosePolicy::new(7))
                .with_build_targets([uniffi_js_abi::PublicTarget::NodeNapi]),
        )
        .unwrap();
        let facade = build(&normalized.api, &normalized.bridge, &normalized.rust).unwrap();
        assert_eq!(facade.ast.close_policy, ClosePolicy::new(7));
        let events = facade
            .ast
            .operations
            .iter()
            .find(|operation| operation.kind == OperationKind::OutputStreamStart)
            .unwrap();
        assert_eq!(events.stream_resources.len(), 2);
        let input_resource = events
            .stream_resources
            .iter()
            .find(|resource| resource.direction == StreamDirection::Input)
            .unwrap();
        let output_resource = events
            .stream_resources
            .iter()
            .find(|resource| resource.direction == StreamDirection::Output)
            .unwrap();
        assert_eq!(input_resource.slot_operation_ids.len(), 2);
        assert_eq!(output_resource.slot_operation_ids.len(), 3);
        let consume = facade
            .ast
            .operations
            .iter()
            .find(|operation| operation.name == "consume")
            .unwrap();
        assert_eq!(consume.stream_resources.len(), 1);
        assert_eq!(
            consume.stream_resources[0].direction,
            StreamDirection::Input
        );
        assert_eq!(consume.stream_resources[0].slot_operation_ids.len(), 2);
        let produce = facade
            .ast
            .operations
            .iter()
            .find(|operation| operation.name == "produce")
            .unwrap();
        assert_eq!(produce.stream_resources.len(), 1);
        assert_eq!(
            produce.stream_resources[0].direction,
            StreamDirection::Output
        );
        assert_eq!(produce.stream_resources[0].slot_operation_ids.len(), 3);
        let input_pull = input_resource.slot_operation_ids[&OperationKind::InputStreamPull].index();
        let output_next =
            output_resource.slot_operation_ids[&OperationKind::OutputStreamNext].index();
        let output_cancel =
            output_resource.slot_operation_ids[&OperationKind::OutputStreamCancel].index();
        let events_id = events.id.index();
        let implementation = String::from_utf8(
            facade
                .shared_files()
                .iter()
                .find(|file| file.path == "components/normalized_streams/internal.js")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(implementation.contains("closePolicy:{graceMs:7,onDeadline:\"detach\"}"));
        let ark_implementation = String::from_utf8(
            facade
                .ark_files()
                .iter()
                .find(|file| file.path == "platform.ets")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(!ark_implementation.contains("deadlineTimer"));
        let root = std::env::temp_dir().join(format!(
            "uniffi-js-normalized-u3s-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("components/normalized_streams")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::write(root.join("package.json"), "{\"type\":\"module\"}").unwrap();
        fs::write(root.join("shared/uniffi_runtime.js"), RUNTIME_SOURCE).unwrap();
        fs::write(
            root.join("components/normalized_streams/internal.js"),
            implementation,
        )
        .unwrap();
        let script = format!(
            r#"
import {{ createNamespace }} from "./components/normalized_streams/internal.js";
import {{ BackendSession, Host }} from "./shared/uniffi_runtime.js";
function makeSession(backend, host = new Host()) {{
  const callControl = {{create: 10000, cancel: 10001, release: 10002, operations: []}};
  let nextCallId = 1;
  const calls = new Set();
  return new BackendSession({{...backend, invokeSync(id, args) {{
    if (id === callControl.create) {{ const callId = nextCallId++; calls.add(callId); return {{kind:"value", value:callId}}; }}
    if (id === callControl.cancel || id === callControl.release) {{
      if (!calls.has(args[0])) throw Error("unknown call ID");
      if (id === callControl.release) calls.delete(args[0]);
      return {{kind:"value", value:null}};
    }}
    return backend.invokeSync(id, args);
  }}}}, host, {{callControl}});
}}
const host = new Host();
let seenInput;
let cancelCalls = 0;
const backend = {{
  invokeSync(id, args) {{
    if (id === {events_id}) {{ seenInput = args[0]; return {{kind:"value", value:41}}; }}
    return {{kind:"value", value:null}};
  }},
  async invokeAsync(id, args) {{
    if (id === {output_next}) return {{kind:"item", value:"hello"}};
    if (id === {output_cancel}) {{ cancelCalls += 1; return {{kind:"value", value:null}}; }}
    if (id === {input_pull}) return {{kind:"done"}};
    return {{kind:"value", value:null}};
  }},
  close() {{}}
}};
const session = makeSession(backend, host);
const api = createNamespace(session);
const source = {{ next: async () => ({{value:7, done:false}}) }};
const stream = api.events(source);
const item = await stream.next();
if (!item || item.value !== "hello" || item.done) throw Error("normalized output stream next");
const pulled = await host.pullInputStream(seenInput);
if (pulled.kind !== "item" || pulled.value !== 7) throw Error("normalized input stream pull");
await stream.cancel();
if (cancelCalls !== 1) throw Error("normalized output stream cancel");
await session.close();
"ok";
"#,
            events_id = events_id,
            input_pull = input_pull,
            output_next = output_next,
            output_cancel = output_cancel,
        );
        fs::write(root.join("check.mjs"), script).unwrap();
        let output = Command::new("node")
            .arg("check.mjs")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "normalized U3S Node fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn normalized_multicomponent_owner_registry_fixture_executes_foreign_objects_and_values() {
        let alpha_crate = "normalized_alpha_crate";
        let beta_crate = "normalized_beta_crate";
        let field = |name: &str, ty: Type, default| FieldMetadata {
            name: name.into(),
            orig_name: None,
            ty,
            default,
            docstring: None,
        };
        let mut alpha_group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: alpha_crate.into(),
                name: "normalized_alpha".into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        let shared_ty = || Type::Record {
            module_path: alpha_crate.into(),
            name: "Shared".into(),
        };
        let alias_ty = || Type::Custom {
            module_path: alpha_crate.into(),
            name: "Alias".into(),
            builtin: Box::new(Type::String),
        };
        let service_ty = || Type::Object {
            module_path: alpha_crate.into(),
            name: "Service".into(),
            imp: ObjectImpl::Struct,
        };
        let listener_ty = || Type::CallbackInterface {
            module_path: alpha_crate.into(),
            name: "Listener".into(),
        };
        alpha_group.add_item(
            RecordMetadata {
                module_path: alpha_crate.into(),
                name: "Shared".into(),
                orig_name: None,
                rust_path: None,
                remote: false,
                fields: vec![
                    field(
                        "count",
                        Type::Int64,
                        Some(uniffi_meta::DefaultValueMetadata::Literal(
                            LiteralMetadata::new_int(4),
                        )),
                    ),
                    field(
                        "values",
                        Type::Map {
                            key_type: Box::new(Type::String),
                            value_type: Box::new(Type::UInt32),
                        },
                        None,
                    ),
                    field(
                        "tags",
                        Type::Set {
                            inner_type: Box::new(Type::String),
                        },
                        None,
                    ),
                ],
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            EnumMetadata {
                module_path: alpha_crate.into(),
                name: "Failure".into(),
                orig_name: None,
                rust_path: None,
                shape: EnumShape::Error { flat: false },
                remote: false,
                variants: vec![VariantMetadata {
                    name: "Rejected".into(),
                    orig_name: None,
                    discr: None,
                    fields: vec![field("message", Type::String, None)],
                    docstring: None,
                }],
                discr_type: None,
                non_exhaustive: false,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            uniffi_meta::CustomTypeMetadata {
                module_path: alpha_crate.into(),
                name: "Alias".into(),
                orig_name: None,
                builtin: Type::String,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            ObjectMetadata {
                module_path: alpha_crate.into(),
                name: "Service".into(),
                orig_name: None,
                remote: false,
                imp: ObjectImpl::Struct,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            ConstructorMetadata {
                module_path: alpha_crate.into(),
                self_name: "Service".into(),
                self_type: Some(service_ty()),
                name: "new".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![],
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            MethodMetadata {
                module_path: alpha_crate.into(),
                self_name: "Service".into(),
                name: "describe".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![],
                return_type: Some(Type::String),
                throws: None,
                takes_self_by_arc: false,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            CallbackInterfaceMetadata {
                module_path: alpha_crate.into(),
                name: "Listener".into(),
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            TraitMethodMetadata {
                module_path: alpha_crate.into(),
                trait_name: "Listener".into(),
                index: 0,
                name: "on_event".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![FnParamMetadata::simple("value", Type::Int64)],
                return_type: Some(Type::String),
                throws: None,
                takes_self_by_arc: false,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            FnMetadata {
                module_path: alpha_crate.into(),
                name: "process".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: true,
                rust_stream_item_is_result: false,
                inputs: vec![
                    FnParamMetadata::simple("value", shared_ty()),
                    FnParamMetadata::simple("alias", alias_ty()),
                    FnParamMetadata::simple(
                        "values",
                        Type::Map {
                            key_type: Box::new(Type::String),
                            value_type: Box::new(Type::UInt32),
                        },
                    ),
                    FnParamMetadata::simple(
                        "tags",
                        Type::Set {
                            inner_type: Box::new(Type::String),
                        },
                    ),
                    FnParamMetadata {
                        name: "count".into(),
                        ty: Type::Int64,
                        pass_by: uniffi_meta::PassBy::Value,
                        optional: false,
                        default: Some(uniffi_meta::DefaultValueMetadata::Literal(
                            LiteralMetadata::new_int(4),
                        )),
                    },
                ],
                return_type: Some(shared_ty()),
                throws: Some(Type::Enum {
                    module_path: alpha_crate.into(),
                    name: "Failure".into(),
                }),
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            FnMetadata {
                module_path: alpha_crate.into(),
                name: "echo_alias".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![FnParamMetadata::simple("value", alias_ty())],
                return_type: Some(alias_ty()),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            FnMetadata {
                module_path: alpha_crate.into(),
                name: "register".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![FnParamMetadata::simple("listener", listener_ty())],
                return_type: None,
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        alpha_group.add_item(
            CallbackUseSiteMetadata {
                module_path: alpha_crate.into(),
                operation_kind: CallbackOperationKind::Function,
                owner: None,
                operation_name: "register".into(),
                path: CallbackValuePath::argument(0),
                contract: MetaCallbackContract {
                    retention: MetaCallbackRetention::Retained,
                    threading: MetaCallbackThreading::CallingThread,
                    reentrancy: MetaCallbackReentrancy::Forbidden,
                },
            }
            .into(),
        );
        let mut beta_group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: beta_crate.into(),
                name: "normalized_beta".into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        beta_group.add_item(
            RecordMetadata {
                module_path: beta_crate.into(),
                name: "Shared".into(),
                orig_name: None,
                rust_path: None,
                remote: false,
                fields: vec![field("label", Type::String, None)],
                docstring: None,
            }
            .into(),
        );
        beta_group.add_item(
            FnMetadata {
                module_path: beta_crate.into(),
                name: "accept_service".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![FnParamMetadata::simple("service", service_ty())],
                return_type: Some(service_ty()),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        beta_group.add_item(
            FnMetadata {
                module_path: beta_crate.into(),
                name: "reverse_register".into(),
                orig_name: None,
                is_async: false,
                rust_async: false,
                rust_return_is_result: false,
                rust_stream_item_is_result: false,
                inputs: vec![FnParamMetadata::simple("listener", listener_ty())],
                return_type: None,
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        beta_group.add_item(
            CallbackUseSiteMetadata {
                module_path: beta_crate.into(),
                operation_kind: CallbackOperationKind::Function,
                owner: None,
                operation_name: "reverse_register".into(),
                path: CallbackValuePath::argument(0),
                contract: MetaCallbackContract {
                    retention: MetaCallbackRetention::Scoped,
                    threading: MetaCallbackThreading::CallingThread,
                    reentrancy: MetaCallbackReentrancy::Allowed,
                },
            }
            .into(),
        );
        let alpha_config = JsConfig {
            wasm_platform_exports: Default::default(),
            lifecycle: Default::default(),
            custom_types: std::collections::BTreeMap::from([(
                "Alias".into(),
                CustomTypeConfig {
                    imports: vec![],
                    type_name: Some("AliasPublic".into()),
                    into_custom: "({ public: value })".into(),
                    from_custom: "value.public".into(),
                },
            )]),
        };
        let alpha = Component {
            ci: ComponentInterface::from_metadata(alpha_group).unwrap(),
            config: alpha_config,
        };
        let beta = Component {
            ci: ComponentInterface::from_metadata(beta_group).unwrap(),
            config: JsConfig::default(),
        };
        let normalized = normalize(
            BindingInput::new(&[alpha, beta])
                .with_build_targets([uniffi_js_abi::PublicTarget::NodeNapi]),
        )
        .unwrap();
        assert_eq!(normalized.api.components.len(), 2);
        assert!(
            normalized
                .api
                .types
                .iter()
                .filter(|ty| ty.public_name == "Shared")
                .count()
                == 2
        );
        assert!(normalized.bridge.callbacks().len() >= 2);
        let facade = build(&normalized.api, &normalized.bridge, &normalized.rust).unwrap();
        let alpha_impl = String::from_utf8(
            facade
                .shared_files()
                .iter()
                .find(|file| file.path == "components/normalized_alpha/internal.js")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        let beta_impl = String::from_utf8(
            facade
                .shared_files()
                .iter()
                .find(|file| file.path == "components/normalized_beta/internal.js")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        let alpha_component = normalized
            .api
            .components
            .iter()
            .find(|component| component.public_namespace == "normalized_alpha")
            .unwrap();
        let beta_component = normalized
            .api
            .components
            .iter()
            .find(|component| component.public_namespace == "normalized_beta")
            .unwrap();
        let alpha_process = normalized
            .api
            .operations
            .iter()
            .find(|operation| {
                operation.component_id == alpha_component.id && operation.public_name == "process"
            })
            .unwrap()
            .id
            .index();
        let alpha_alias = normalized
            .api
            .operations
            .iter()
            .find(|operation| {
                operation.component_id == alpha_component.id && operation.public_name == "echoAlias"
            })
            .unwrap()
            .id
            .index();
        let alpha_new = normalized
            .api
            .operations
            .iter()
            .find(|operation| {
                operation.component_id == alpha_component.id
                    && operation.kind == OperationKind::Constructor
            })
            .unwrap()
            .id
            .index();
        let beta_accept = normalized
            .api
            .operations
            .iter()
            .find(|operation| {
                operation.component_id == beta_component.id
                    && operation.public_name == "acceptService"
            })
            .unwrap()
            .id
            .index();
        let alpha_describe = normalized
            .api
            .operations
            .iter()
            .find(|operation| {
                operation.component_id == alpha_component.id && operation.public_name == "describe"
            })
            .unwrap()
            .id
            .index();
        let beta_reverse = normalized
            .api
            .operations
            .iter()
            .find(|operation| {
                operation.component_id == beta_component.id
                    && operation.public_name == "reverseRegister"
            })
            .unwrap()
            .id
            .index();
        let callback_type = normalized
            .bridge
            .callbacks()
            .iter()
            .find(|callback| {
                callback.operation_id
                    == normalized
                        .api
                        .operations
                        .iter()
                        .find(|operation| operation.public_name == "register")
                        .unwrap()
                        .id
            })
            .unwrap()
            .callback_type
            .index();
        let root = std::env::temp_dir().join(format!(
            "uniffi-js-normalized-composite-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("components/normalized_alpha")).unwrap();
        fs::create_dir_all(root.join("components/normalized_beta")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::write(root.join("package.json"), "{\"type\":\"module\"}").unwrap();
        fs::write(root.join("shared/uniffi_runtime.js"), RUNTIME_SOURCE).unwrap();
        fs::write(
            root.join("components/normalized_alpha/internal.js"),
            alpha_impl,
        )
        .unwrap();
        fs::write(
            root.join("components/normalized_beta/internal.js"),
            beta_impl,
        )
        .unwrap();
        let script = format!(
            r#"
import * as alpha from "./components/normalized_alpha/internal.js";
import * as beta from "./components/normalized_beta/internal.js";
import {{ BackendSession, Host }} from "./shared/uniffi_runtime.js";
function makeSession(backend, host = new Host()) {{
  const callControl = {{create: 10000, cancel: 10001, release: 10002, operations: []}};
  let nextCallId = 1;
  const calls = new Set();
  return new BackendSession({{...backend, invokeSync(id, args) {{
    if (id === callControl.create) {{ const callId = nextCallId++; calls.add(callId); return {{kind:"value", value:callId}}; }}
    if (id === callControl.cancel || id === callControl.release) {{
      if (!calls.has(args[0])) throw Error("unknown call ID");
      if (id === callControl.release) calls.delete(args[0]);
      return {{kind:"value", value:null}};
    }}
    return backend.invokeSync(id, args);
  }}}}, host, {{callControl}});
}}
const host = new Host();
let callbackId;
let processCalls = 0;
const backend = {{
  invokeSync(id, args) {{
    if (id === {alpha_process}) {{
      const record = args[0];
      if (!record || record.count !== 4n || !(record.values instanceof Map) || record.values.get("x") !== 1 || !(record.tags instanceof Set) || !record.tags.has("tag")) throw Error("record lower");
      if (args[1] !== "input" || !(args[2] instanceof Map) || !(args[3] instanceof Set) || args[4] !== 4n) throw Error("argument lower/default");
      processCalls += 1;
      if (processCalls === 1) return {{kind:"value", value:{{count:record.count, values:record.values, tags:record.tags}}}};
      return {{kind:"error", error:{{errorName:"Failure", variant:"Rejected", data:{{message:"bad"}}}}}};
    }}
    if (id === {alpha_alias}) return {{kind:"value", value:"native"}};
    if (id === {alpha_new}) return {{kind:"value", value:17}};
    if (id === {beta_accept}) return {{kind:"value", value:17}};
    if (id === {alpha_describe}) return {{kind:"value", value:"owner"}};
    if (id === {beta_reverse}) {{
      callbackId = args[0];
      host.retainCallback({callback_type}, callbackId);
      const result = host.invokeCallbackSync({callback_type}, callbackId, 0, [9n]);
      if (result !== "event:9") throw Error("reverse callback");
      host.releaseCallback({callback_type}, callbackId);
      return {{kind:"value", value:null}};
    }}
    return {{kind:"value", value:null}};
  }},
  async invokeAsync() {{ return {{kind:"value", value:null}}; }},
  close() {{}}
}};
const session = makeSession(backend, host);
const alphaApi = alpha.createNamespace(session);
const betaApi = beta.createNamespace(session);
if ("Service" in betaApi) throw Error("foreign object shadow export");
const service = alphaApi.Service.new();
const accepted = betaApi.acceptService(service);
if (!(accepted instanceof alphaApi.Service) || accepted.describe() !== "owner") throw Error("foreign owner instanceof");
const alias = alphaApi.echoAlias({{public:"input"}});
if (!alias || alias.public !== "native") throw Error("custom conversion round trip");
const processed = alphaApi.process({{count:4n, values:new Map([["x", 1]]), tags:new Set(["tag"]) }}, {{public:"input"}}, new Map(), new Set());
if (processed.count !== 4n || !(processed.values instanceof Map) || processed.values.get("x") !== 1 || !(processed.tags instanceof Set) || !processed.tags.has("tag")) throw Error("record/map/set round trip");
try {{ alphaApi.process({{count:4n, values:new Map([["x", 1]]), tags:new Set(["tag"]) }}, {{public:"input"}}, new Map(), new Set()); throw Error("typed error missing"); }} catch (error) {{ if (!(error instanceof alpha.Failure) || error.errorName !== "Failure" || error.variant !== "Rejected") throw error; }}
const listener = {{ onEvent(value) {{ return `event:${{value}}`; }} }};
betaApi.reverseRegister(listener);
try {{ host.invokeCallbackSync({callback_type}, callbackId, 0, [9n]); throw Error("reverse callback leaked"); }} catch (error) {{ if (error.errorName !== "UniffiCallbackMissing") throw error; }}
if (session.callbacks.callbacks.size !== 0) throw Error("reverse callback registry leak");
await session.close();
"ok";
"#,
            alpha_process = alpha_process,
            alpha_alias = alpha_alias,
            alpha_new = alpha_new,
            alpha_describe = alpha_describe,
            beta_accept = beta_accept,
            beta_reverse = beta_reverse,
            callback_type = callback_type,
        );
        fs::write(root.join("check.mjs"), script).unwrap();
        let output = Command::new("node")
            .arg("check.mjs")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "normalized composite Node fixture failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn generated_async_facade_forwards_fixed_options_and_waits_for_cancellation() {
        let mut ast = corpus_ast();
        for id in [0, 1, 2, 12, 13] {
            let operation = ast
                .operations
                .iter_mut()
                .find(|op| op.id.index() == id)
                .unwrap();
            operation.async_kind = AsyncKind::Async;
            operation.arguments = vec![AstArgument {
                name: if id == 13 { "options" } else { "value" }.into(),
                ty: ValueType::Scalar(ScalarType::String),
                default: Some(if id == 2 {
                    DefaultValue::Unspecified
                } else {
                    DefaultValue::String("default".into())
                }),
            }];
            if id != 1 {
                operation.return_type = Some(ValueType::Scalar(ScalarType::String));
            }
        }
        let root = std::env::temp_dir().join(format!(
            "uniffi-js-async-options-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        for file in render_inventory(&ast).unwrap() {
            let path = root.join(file.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.bytes).unwrap();
        }
        fs::write(root.join("package.json"), "{\"type\":\"module\"}").unwrap();
        let script = r#"
import assert from "node:assert/strict";
import { createNamespace } from "./components/corpus/internal.js";
import { BackendSession, Host, invokeOperation, disposeObject } from "./shared/uniffi_runtime.js";
const callControl = {create:10000, cancel:10001, release:10002, operations:[0,1,2,12,13,16]};
const calls = new Map();
const seen = [];
let nextCallId = 1;
let cancelCount = 0;
let releaseCount = 0;
let completeCancellation;
function makeSession(backend) {
  return new BackendSession({...backend, invokeSync(id, args) {
    if (id === callControl.create) { const first = nextCallId; nextCallId += 4096; return {kind:"value", value:first}; }
    if (id === callControl.cancel) { assert.ok(calls.has(args[0])); cancelCount++; return {kind:"value", value:null}; }
    if (id === callControl.release) { assert.ok(calls.delete(args[0])); releaseCount++; return {kind:"value", value:null}; }
    return backend.invokeSync(id, args);
  }}, new Host(), {callControl});
}
const session = makeSession({
  invokeSync(id) { if (id === 11) return {kind:"value", value:"sync"}; throw Error("unexpected sync operation"); },
  invokeAsync(id, args) {
    const callId = args.at(-1);
    assert.ok(Number.isSafeInteger(callId) && callId > 0 && callId < nextCallId, "native call must carry a reserved ID at the end");
    assert.ok(!calls.has(callId), "each call ID is used once");
    calls.set(callId, {});
    const business = args.slice(0, -1);
    seen.push([id, business]);
    if (business.includes("pending")) return new Promise(resolve => { completeCancellation = () => resolve({kind:"error", error:{errorName:"UniffiCancelled", message:"cleaned up"}}); });
    calls.delete(callId); // Normal completion unregisters in Rust, without an FFI release call.
    return Promise.resolve({kind:"value", value:id === 1 ? 42 : "ok"});
  },
  releaseObject() {},
  close() { assert.equal(calls.size, 0); },
});
const api = createNamespace(session);
const options = {signal:new AbortController().signal};
assert.equal(await api.echo(undefined, options), "ok");
assert.deepEqual(seen.at(-1), [0, ["default"]]);
const service = await api.Service.newService(undefined, options);
assert.deepEqual(seen.at(-1), [1, ["default"]]);
assert.equal(await service.describe(undefined, options), "ok");
assert.deepEqual(seen.at(-1), [2, [42, undefined]]);
await service.describe();
assert.deepEqual(seen.at(-1), [2, [42, undefined]]);
const profile = {scores:new Map([["x", 2n]])};
await api.Profile.collision(profile, undefined, options);
assert.deepEqual(seen.at(-1), [12, [{scores:new Map([["x", 2n]]), avatar:null}, "default"]]);
await invokeOperation(session, {id:16, name:"withoutReceiver", async:true, kind:"Method", valueSelf:true, args:[{name:"value",type:{kind:"scalar",name:"String"},default:"default"}],returnType:{kind:"scalar",name:"String"}}, [profile, undefined, options]);
assert.equal(seen.at(-1)[0], 16);
assert.equal(seen.at(-1)[1][1], "default");
await service.collision("business-options", options);
assert.deepEqual(seen.at(-1), [13, [42, "business-options"]]);
assert.equal(api.collision(), "sync");
assert.throws(() => api.collision(options), {errorName:"UniffiArgumentCount"});
await assert.rejects(api.echo(options), {errorName:"UniffiValueType"});
const preAborted = new AbortController();
preAborted.abort();
const before = nextCallId;
await assert.rejects(api.echo(undefined, {signal:preAborted.signal}), {errorName:"UniffiCancelled"});
assert.equal(nextCallId, before);
const controller = new AbortController();
let settled = false;
const pending = service.describe("pending", {signal:controller.signal});
const rejection = assert.rejects(pending, {errorName:"UniffiCancelled"}).then(() => { settled = true; });
await Promise.resolve();
controller.abort();
await Promise.resolve();
assert.equal(cancelCount, 1);
assert.equal(settled, false, "abort must wait for native cleanup");
completeCancellation();
await rejection;
assert.equal(calls.size, 0);
assert.equal(releaseCount, 1, "only cancellation needs a final release acknowledgement");
disposeObject(service);
await session.close();
"#;
        fs::write(root.join("check.mjs"), script).unwrap();
        let output = Command::new("node")
            .arg("check.mjs")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "generated async facade failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    #[ignore = "requires UNIFFI_ARK_TYPESCRIPT pointing to the DevEco SDK TypeScript module"]
    fn ark_async_protocol_compiles_and_executes() {
        let compiler = std::env::var("UNIFFI_ARK_TYPESCRIPT").expect("ArkTS compiler module path");
        let mut ast = corpus_ast();
        for id in [0, 1, 2, 12, 13] {
            let operation = ast
                .operations
                .iter_mut()
                .find(|op| op.id.index() == id)
                .unwrap();
            operation.async_kind = AsyncKind::Async;
            operation.arguments = vec![AstArgument {
                name: if id == 13 { "options" } else { "value" }.into(),
                ty: ValueType::Scalar(ScalarType::String),
                default: Some(if id == 2 {
                    DefaultValue::Unspecified
                } else {
                    DefaultValue::String("default".into())
                }),
            }];
            if id != 1 {
                operation.return_type = Some(ValueType::Scalar(ScalarType::String));
            }
        }
        ast.operations
            .iter_mut()
            .find(|op| op.id.index() == 7)
            .unwrap()
            .async_kind = AsyncKind::Async;
        let root = std::env::temp_dir().join(format!(
            "uniffi-ark-async-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        for file in render_ark_inventory(&ast).unwrap() {
            fs::write(root.join(file.path), file.bytes).unwrap();
        }
        for file in render_inventory(&ast).unwrap() {
            let path = root.join(file.path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, file.bytes).unwrap();
        }
        fs::write(root.join("check-types.ts"), r#"
import {BackendSession, BackendSessionOptions, CallOptions, createBackendSession, UniFfiStream} from "./shared/uniffi_runtime.js";
import type {Namespace, CallOptions as PublicCallOptions, Event} from "./components/corpus/index.js";
declare const api: Namespace;
const options: CallOptions = {signal:new AbortController().signal};
const publicOptions: PublicCallOptions = options;
const config: BackendSessionOptions = {callControl:{create:20,cancel:21,release:22,operations:[0,1,2,7,12,13]}};
const session = new BackendSession({}, undefined, config);
createBackendSession(() => ({}), undefined, config);
session.environmentDestroyed();
session.invokeAsync(0, [], publicOptions);
api.echo(undefined, publicOptions);
api.Service.newService(undefined, options).then(service => service.describe(undefined, options));
const stream: Promise<UniFfiStream<Event>> = api.events(options);
// @ts-expect-error the protocol argument is mandatory
new BackendSession({}, undefined);
// @ts-expect-error the factory protocol argument is mandatory
createBackendSession(() => ({}), undefined);
// @ts-expect-error synchronous calls do not accept cancellation options
api.collision(options);
"#).unwrap();
        let script = r#"
const assert = require("node:assert/strict");
const path = require("node:path");
const ts = require(process.env.UNIFFI_ARK_TYPESCRIPT);
const options = {target:ts.ScriptTarget.ES2021, module:ts.ModuleKind.CommonJS, strict:true, useUnknownInCatchVariables:false, lib:["lib.es2021.d.ts"], outDir:"out", allowNonTsExtensions:true};
const host = ts.createIncrementalCompilerHost(options);
const builder = ts.createIncrementalProgram({rootNames:[path.resolve("platform.ets")], options, host});
const program = builder.getProgram();
const diagnostics = ts.getPreEmitDiagnostics(program).concat(ts.ArkTSLinter_1_1.runArkTSLinter(builder, undefined, undefined, "ArkTS_1_1"));
for (const diagnostic of diagnostics) {
  const pos = diagnostic.file && diagnostic.file.getLineAndCharacterOfPosition(diagnostic.start || 0);
  console.log(`${diagnostic.file?.fileName || ""}:${pos ? pos.line + 1 : ""}: ${ts.flattenDiagnosticMessageText(diagnostic.messageText, "\n")}`);
}
assert.equal(diagnostics.filter(d => d.category === ts.DiagnosticCategory.Error).length, 0, "ArkTS compiler/linter errors");
const declarations = ts.createProgram([path.resolve("platform.d.ets")], options);
const publicTypes = ts.createProgram([path.resolve("check-types.ts")], {...options, lib:["lib.es2021.d.ts","lib.dom.d.ts"], noEmit:true});
const typeDiagnostics = ts.getPreEmitDiagnostics(declarations).concat(ts.getPreEmitDiagnostics(publicTypes));
for (const diagnostic of typeDiagnostics) console.log(ts.flattenDiagnosticMessageText(diagnostic.messageText,"\n"));
assert.equal(typeDiagnostics.filter(d => d.category === ts.DiagnosticCategory.Error).length,0,"public declarations must compile");
assert.equal(program.emit().emitSkipped, false);
const ark = require("./out/platform.js");
const callControl = {create:10000,cancel:10001,release:10002,operations:[0,1,2,7,12,13]};
let nextBlockStart = 1;
let creates = 0;
let cancels = 0;
let releases = 0;
let backendClosed = false;
const calls = new Map();
const cancelMarkers = new Set();
const seen = [];
let finish;
let finishBackend;
const backend = {
  invokeSync(id, args) {
    if (id === callControl.create) { const blockStart = nextBlockStart; nextBlockStart += 4096; creates++; return new ark.ArkValueResult(blockStart); }
    assert.ok(Number.isSafeInteger(args[0]) && args[0] > 0 && args[0] < nextBlockStart);
    if (id === callControl.cancel) {
      cancels++;
      const call = calls.get(args[0]);
      if (call) call.cancelled = true; else cancelMarkers.add(args[0]);
      return new ark.ArkValueResult(null);
    }
    if (id === callControl.release) {
      assert.equal(calls.has(args[0]), false, "Rust cleanup must finish before clearing late cancellation");
      cancelMarkers.delete(args[0]); releases++; return new ark.ArkValueResult(null);
    }
    throw Error("unexpected sync call");
  },
  invokeAsync(id, args) {
    seen.push([id, args]);
    if (id === callControl.create + 3) return Promise.resolve(new ark.ArkValueResult(1));
    if (!callControl.operations.includes(id)) return Promise.resolve(new ark.ArkValueResult("ok"));
    const callId = args.at(-1);
    assert.ok(Number.isSafeInteger(callId) && callId > 0 && callId < nextBlockStart);
    assert.equal(calls.has(callId), false);
    const call = {cancelled:cancelMarkers.delete(callId)};
    calls.set(callId, call); // typed wrapper's first poll owns the context
    const result = args.includes("pending")
      ? new Promise(resolve => { finish = () => resolve(new ark.ArkErrorResult({errorName:"UniffiCancelled",message:"cleaned",variant:null,data:null})); })
      : args.includes("business-failure")
        ? Promise.resolve(new ark.ArkErrorResult({errorName:"BusinessFailure",message:"business failed",variant:null,data:null}))
        : Promise.resolve(new ark.ArkValueResult(id === 1 ? 42 : id === 7 ? 77 : "ok"));
    return result.finally(() => { calls.delete(callId); }); // Rust RAII, independent of release
  },
  releaseObject() {}, releaseOutputStream() {},
  close() { assert.equal(calls.size, 0); assert.equal(cancelMarkers.size, 0); return new Promise(resolve => { finishBackend = () => { backendClosed = true; resolve(); }; }); },
};
const watchdog = setTimeout(() => { throw Error("ArkTS protocol test did not complete"); }, 10000);
(async () => {
  const session = ark.createBackendSession(() => backend, undefined, {callControl});
  const api = ark.createNamespace(session).corpus;
  const controller = new ark.CallController();
  const options = {signal:controller.signal};
  await api.echo(undefined, options);
  assert.deepEqual(seen.at(-1)[1].slice(0,-1), ["default"]);
  const service = await api.Service.newService(undefined, options);
  await service.describe(undefined, options);
  assert.deepEqual(Array.from(seen.at(-1)[1].slice(0,-1)), [42,undefined], "omitted default must reserve its slot before callId");
  await service.collision("business", options);
  assert.deepEqual(seen.at(-1)[1].slice(0,-1), [42,"business"]);
  await api.Profile.collision({scores:new Map(),avatar:null}, undefined, options);
  assert.equal(seen.at(-1)[0], 12);
  await session.invokeAsync(8, [77]);
  assert.deepEqual(seen.at(-1), [8, [77]], "stream slot cannot receive hidden call ID");
  const stream = await api.events(options);
  assert.equal(typeof stream.next, "function");
  assert.equal(seen.at(-1)[0], 7, "async factory must start before its promise resolves");
  await stream.cancel();
  assert.deepEqual(seen.slice(-2), [[9, [77]], [callControl.create + 3, [77]]], "stream cancellation requests its slot, then awaits N+3 acknowledgement without a call ID");
  let finishNext;
  let nextStarted;
  const startedNext = new Promise(resolve => { nextStarted = resolve; });
  let releasedStream = false;
  const activeStream = session.createOutputStream({handle:99,start:()=>99,
    next:()=>new Promise(resolve=>{finishNext=resolve;nextStarted();}),cancel:async()=>{},lift:value=>value,error:value=>value,
    release:()=>{releasedStream=true;}});
  const reading = activeStream.next();
  await startedNext;
  const cancelling = activeStream.cancel();
  assert.equal(activeStream.cancel(), cancelling, "repeated cancellation must share completion");
  let cancelFinished = false;
  cancelling.then(()=>{cancelFinished=true;});
  await Promise.resolve(); await Promise.resolve();
  assert.equal(cancelFinished,false); assert.equal(releasedStream,false);
  const done = new ark.ArkRecord(); done.set("kind","done");
  finishNext(new ark.ArkValueResult(done));
  const [cancelledNext] = await Promise.all([reading,cancelling]);
  assert.equal(cancelledNext.kind,"error");
  assert.equal(cancelledNext.error.errorName,"UniffiCancelled");
  assert.equal(releasedStream,true);
  await assert.rejects(api.echo("business-failure"), {errorName:"BusinessFailure"});
  assert.equal(releases, 0, "ordinary success and business failure must use Rust RAII without release FFI");
  assert.equal(creates, 1);
  const before = seen.length;
  controller.abort();
  await assert.rejects(api.echo(undefined, options), {errorName:"UniffiCancelled"});
  assert.equal(seen.length, before);
  const aborter = new ark.CallController();
  let settled = false;
  const pending = service.describe("pending", {signal:aborter.signal});
  const callback = {invokeSync:()=>"outside-frame",invokeAsync:async()=>"outside-frame"};
  const contract = {retention:"scoped",threading:"callingThread",reentrancy:"allowed"};
  const outsideId = session.registerCallback(99, callback, contract);
  const rejected = assert.rejects(pending, {errorName:"UniffiCancelled"}).then(() => { settled = true; });
  await Promise.resolve();
  aborter.abort(); aborter.abort();
  assert.equal(cancels, 1);
  await Promise.resolve(); assert.equal(settled, false);
  finish(); await rejected;
  assert.equal(session.host.invokeCallbackSync(99,outsideId,0,[]),"outside-frame", "pending async operation must not own later callback registrations");
  session.releaseCallback(99,outsideId);
  const explicitFrame = session.beginCallFrame();
  const scopedId = session.registerCallback(99,callback,contract);
  session.leaveCallFrame(explicitFrame);
  assert.equal(session.host.invokeCallbackSync(99,scopedId,0,[]),"outside-frame", "leave must preserve registrations until end");
  session.endCallFrame(explicitFrame);
  assert.throws(()=>session.host.invokeCallbackSync(99,scopedId,0,[]),{errorName:"UniffiCallbackMissing"});
  assert.equal(calls.size, 0);
  const closingCall = api.echo("pending");
  const cancelled = assert.rejects(closingCall, {errorName:"UniffiCancelled"});
  await Promise.resolve();
  let closed = false;
  const closing = session.close().then(() => { closed = true; });
  await Promise.resolve();
  assert.equal(cancels, 2); assert.equal(closed, false); assert.equal(backendClosed, false);
  finish(); await cancelled;
  for (let i=0;i<30 && !finishBackend;i++) await Promise.resolve();
  assert.equal(closed, false);
  assert.equal(typeof finishBackend, "function");
  finishBackend(); await closing;
  assert.equal(releases, 2, "only explicitly cancelled calls clear their late markers");
  assert.equal(cancelMarkers.size, 0);
  assert.equal(creates, 1, "one block serves every call in the session");
  assert.equal(backendClosed, true);
  let commitFactory;
  let finishSlotCancel;
  let finishFactoryAck;
  let factoryAckStarted;
  const factoryAck = new Promise(resolve => { factoryAckStarted = resolve; });
  let releasedHandle = false;
  let factoryEngineClosed = false;
  const factoryBackend = {...backend,
    invokeAsync(id,args) {
      if (id === callControl.create + 3) {
        assert.deepEqual(args,[88]);
        factoryAckStarted();
        return new Promise(resolve => { finishFactoryAck=()=>resolve(new ark.ArkValueResult(1)); });
      }
      if (id === 7) {
        const callId = args.at(-1);
        calls.set(callId,{cancelled:cancelMarkers.delete(callId)});
        return new Promise(resolve=>{commitFactory=()=>resolve(new ark.ArkValueResult(88));}).finally(()=>{calls.delete(callId);});
      }
      assert.deepEqual([id,args],[9,[88]]);
      return new Promise(resolve=>{finishSlotCancel=()=>resolve(new ark.ArkValueResult(null));});
    },
    releaseOutputStream(handle) { assert.equal(handle,88); releasedHandle=true; },
    close() { assert.equal(releasedHandle,true,"engine close must follow committed factory handle cleanup"); factoryEngineClosed=true; }
  };
  const factorySession = new ark.BackendSession(factoryBackend,new ark.Host(),{callControl});
  const factoryCall = ark.createNamespace(factorySession).corpus.events();
  const factoryRejected = assert.rejects(factoryCall,{errorName:"UniffiSessionClosed"});
  await Promise.resolve();
  const factoryClose = factorySession.close();
  commitFactory();
  for (let i=0;i<30 && !finishSlotCancel;i++) await Promise.resolve();
  assert.equal(typeof finishSlotCancel,"function");
  assert.equal(factoryEngineClosed,false);
  finishSlotCancel();
  await factoryAck;
  assert.equal(releasedHandle,false,"cancel is a request; release must wait for N+3 confirmation");
  assert.equal(factoryEngineClosed,false);
  finishFactoryAck();
  await Promise.all([factoryRejected,factoryClose]);
  assert.equal(factoryEngineClosed,true);
  // One failed producer cannot let frame or session close settle while another
  // producer still owns asynchronous cleanup.
  for (const sessionClose of [false,true]) {
    const cleanupSession = new ark.BackendSession({...backend,close(){}},new ark.Host(),{callControl});
    const frame = cleanupSession.beginCallFrame();
    let startCleanup, finishCleanup, cleanupDone=false, settled=false;
    const started = new Promise(resolve => { startCleanup=resolve; });
    const cleanup = new Promise(resolve => { finishCleanup=resolve; });
    const lowerError = value => value;
    cleanupSession.createInputStream({next:async()=>new ark.ArkDoneStep(),cancel:async()=>{throw new Error("producer cleanup failed");}}, value=>value, lowerError);
    cleanupSession.createInputStream({next:async()=>new ark.ArkDoneStep(),cancel:async()=>{startCleanup();await cleanup;cleanupDone=true;}}, value=>value, lowerError);
    cleanupSession.leaveCallFrame(frame);
    const closing = sessionClose ? cleanupSession.close() : cleanupSession.endCallFrame(frame);
    const rejected = assert.rejects(closing,/producer cleanup failed/).then(()=>{settled=true;});
    await started;
    assert.equal(settled,false); assert.equal(cleanupDone,false);
    finishCleanup(); await rejected; assert.equal(cleanupDone,true);
    if (!sessionClose) await cleanupSession.close();
  }
  const lateAborter = new ark.CallController();
  const lateCancels = cancels;
  const lateReleases = releases;
  const lateBackend = {...backend,
    invokeAsync(id,args) {
      const callId = args.at(-1);
      calls.set(callId,{cancelled:false});
      return Promise.resolve(new ark.ArkValueResult("committed")).finally(()=>{
        calls.delete(callId);
        lateAborter.abort(); // completed Rust context is gone, JS still owns its signal listener
        assert.equal(cancelMarkers.has(callId),true);
        assert.equal(releases,lateReleases);
      });
    },close() { assert.equal(cancelMarkers.size,0); }
  };
  const lateSession = new ark.BackendSession(lateBackend,new ark.Host(),{callControl});
  assert.equal(await lateSession.invokeAsync(0,[],{signal:lateAborter.signal}),"committed", "late abort must preserve the Rust completion decision");
  assert.equal(cancels,lateCancels+1); assert.equal(releases,lateReleases+1);
  assert.equal(cancelMarkers.size,0);
  await lateSession.close();

  const earlyAborter = new ark.CallController();
  const earlyCancels = cancels;
  const earlyReleases = releases;
  const earlyBackend = {...backend,invokeAsync(id,args) {
    const callId = args.at(-1);
    assert.equal(cancelMarkers.delete(callId),true,"first poll must observe pre-poll cancellation");
    calls.set(callId,{cancelled:true});
    return Promise.resolve(new ark.ArkErrorResult({errorName:"UniffiCancelled",message:"pre-poll cancellation",variant:null,data:null})).finally(()=>{calls.delete(callId);});
  },close(){assert.equal(cancelMarkers.size,0);}};
  const earlySession = new ark.BackendSession(earlyBackend,new ark.Host(),{callControl});
  const beforePoll = earlySession.invokeAsync(0,[],{signal:earlyAborter.signal});
  earlyAborter.abort();
  await assert.rejects(beforePoll,{errorName:"UniffiCancelled"});
  assert.equal(cancels,earlyCancels+1); assert.equal(releases,earlyReleases+1);
  await earlySession.close();

  function blockProbe(firstBlock) {
    let nextBlock = firstBlock;
    const probe = {starts:[],ids:[],polls:0,active:0};
    const backend = {
      invokeSync(id,args) {
        assert.equal(id,callControl.create,"uncancelled calls cannot invoke cancel or release");
        assert.deepEqual(args,[]);
        const start = nextBlock; nextBlock += 4096; probe.starts.push(start);
        return new ark.ArkValueResult(start);
      },
      invokeAsync(id,args) {
        assert.equal(id,0);
        const callId = args.at(-1);
        probe.ids.push(callId); probe.polls++; probe.active++;
        return Promise.resolve(new ark.ArkValueResult(callId)).finally(()=>{probe.active--;});
      }, releaseObject(){},releaseOutputStream(){},close(){assert.equal(probe.active,0);}
    };
    return {...probe,backend,get starts(){return probe.starts;},get ids(){return probe.ids;},get polls(){return probe.polls;}};
  }
  const probe = blockProbe(1);
  const blockSession = new ark.BackendSession(probe.backend,new ark.Host(),{callControl});
  const values = await Promise.all(Array.from({length:4097},()=>blockSession.invokeAsync(0,[])));
  assert.deepEqual(probe.starts,[1,4097]);
  assert.deepEqual(values,Array.from({length:4097},(_,index)=>index+1));
  assert.deepEqual(probe.ids,values);
  assert.equal(probe.polls,4097,"every no-signal call still enters the typed wrapper");
  await blockSession.close();
  const upper = blockProbe(Number.MAX_SAFE_INTEGER-4095);
  const upperSession = new ark.BackendSession(upper.backend,new ark.Host(),{callControl});
  const upperValues = await Promise.all(Array.from({length:4096},()=>upperSession.invokeAsync(0,[])));
  assert.equal(upperValues[0],Number.MAX_SAFE_INTEGER-4095);
  assert.equal(upperValues.at(-1),Number.MAX_SAFE_INTEGER);
  assert.equal(upperValues.every(Number.isSafeInteger),true);
  await assert.rejects(upperSession.invokeAsync(0,[]),{errorName:"UniffiCallControl"});
  assert.equal(upper.polls,4096,"overflow must never reach Rust");
  await upperSession.close();
  for (const start of [0,-1,1.5,NaN,Infinity,"1",Number.MAX_SAFE_INTEGER-4094]) {
    let invoked = false;
    const invalid = new ark.BackendSession({...probe.backend,
      invokeSync(){return new ark.ArkValueResult(start);},invokeAsync(){invoked=true;return Promise.resolve(new ark.ArkValueResult(null));}
    },new ark.Host(),{callControl});
    await assert.rejects(invalid.invokeAsync(0,[]),{errorName:"UniffiCallControl"});
    assert.equal(invoked,false);
    await invalid.close();
  }
  console.log("4097 no-signal calls: 2 create FFI, 0 cancel/release FFI; safe-integer boundary and early/late cancellation passed");
  const destroyed = new ark.BackendSession(backend, new ark.Host(), {callControl});
  const destroyedCall = destroyed.invokeAsync(0,["pending"]);
  const destruction = assert.rejects(destroyedCall,{errorName:"UniffiEnvironmentDestroyed"});
  await Promise.resolve();
  const destroyedClose = assert.rejects(destroyed.close(),{errorName:"UniffiEnvironmentDestroyed"});
  destroyed.environmentDestroyed();
  await Promise.all([destruction,destroyedClose]);
  console.log("ArkTS compile/linter + TS declarations + block IDs/cancel-only release/defaults/stream slots/close/environment destruction passed");
})().catch(error => { console.error(error); process.exitCode = 1; }).finally(() => clearTimeout(watchdog));
"#;
        fs::write(root.join("check.cjs"), script).unwrap();
        let output = Command::new("node")
            .arg("check.cjs")
            .env("UNIFFI_ARK_TYPESCRIPT", compiler)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ArkTS compile/runtime failed in {}: {}\n{}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        println!("{}", String::from_utf8_lossy(&output.stdout));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn generated_ecmascript_executes_with_operation_id_mock() {
        let mut ast = corpus_ast();
        let component_key = ComponentKey::new("corpus").unwrap();
        let unit_failure = key(&component_key, "UnitFailure");
        let error_event = key(&component_key, "ErrorEvent");
        let error_state = key(&component_key, "ErrorState");
        let optional_error = ValueType::Optional(Box::new(ValueType::Named(unit_failure.clone())));
        ast.types.extend([
            AstType {
                id: TypeId::new(7),
                source_key: unit_failure.clone(),
                name: "UnitFailure".into(),
                kind: AstTypeKind::Error {
                    variants: vec![AstVariant {
                        name: "Io".into(),
                        fields: vec![],
                    }],
                },
            },
            AstType {
                id: TypeId::new(8),
                source_key: error_event.clone(),
                name: "ErrorEvent".into(),
                kind: AstTypeKind::Enum {
                    variants: vec![
                        AstVariant {
                            name: "Failed".into(),
                            fields: vec![AstField {
                                name: "error".into(),
                                ty: ValueType::Named(unit_failure),
                                default: None,
                            }],
                        },
                        AstVariant {
                            name: "Closed".into(),
                            fields: vec![AstField {
                                name: "error".into(),
                                ty: optional_error.clone(),
                                default: None,
                            }],
                        },
                    ],
                },
            },
            AstType {
                id: TypeId::new(9),
                source_key: error_state.clone(),
                name: "ErrorState".into(),
                kind: AstTypeKind::Record {
                    fields: vec![
                        AstField {
                            name: "error".into(),
                            ty: optional_error,
                            default: None,
                        },
                        AstField {
                            name: "detail".into(),
                            ty: ValueType::Named(key(&component_key, "Failure")),
                            default: None,
                        },
                        AstField {
                            name: "reports".into(),
                            ty: ValueType::Sequence(Box::new(ValueType::Named(error_event))),
                            default: None,
                        },
                    ],
                },
            },
        ]);
        ast.operations.push(AstOperation {
            id: OperationId::new(16),
            source_key: operation_key(
                &component_key,
                OperationOwner::Namespace,
                OperationKind::Function,
                "echo_error_state",
            ),
            component_id: ComponentId::new(0),
            name: "echoErrorState".into(),
            debug_name: "echoErrorState".into(),
            kind: OperationKind::Function,
            arguments: vec![AstArgument {
                name: "value".into(),
                ty: ValueType::Named(error_state.clone()),
                default: None,
            }],
            return_type: Some(ValueType::Named(error_state)),
            async_kind: AsyncKind::Sync,
            throws: None,
            receiver_type: None,
            callback_method_id: None,
            stream_slots: vec![],
            stream_resources: vec![],
        });
        let component = &ast.components[0];
        let declaration = render_component_declaration(&ast, component).unwrap();
        assert!(declaration.contains("export type UnitFailure = UniffiError &"));
        assert!(declaration.contains("error: UnitFailure | null"));
        assert!(declaration.contains("detail: Failure"));
        assert!(declaration.contains("echoErrorState(value: ErrorState): ErrorState"));
        let implementation = render_component_implementation(&ast, component, "js").unwrap();
        let root = std::env::temp_dir().join(format!(
            "uniffi-js-facade-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("components/corpus")).unwrap();
        fs::create_dir_all(root.join("shared")).unwrap();
        fs::write(root.join("package.json"), "{\"type\":\"module\"}").unwrap();
        fs::write(root.join("shared/uniffi_runtime.js"), RUNTIME_SOURCE).unwrap();
        fs::write(root.join("components/corpus/internal.js"), implementation).unwrap();
        let script = r#"
import assert from "node:assert/strict";
import { createNamespace } from "./components/corpus/internal.js";
import { BackendSession, Host, disposeObject, createFacade } from "./shared/uniffi_runtime.js";
function makeSession(backend, host = new Host()) {
  const callControl = {create: 10000, cancel: 10001, release: 10002, operations: []};
  let nextCallId = 1;
  const calls = new Set();
  return new BackendSession({...backend, invokeSync(id, args) {
    if (id === callControl.create) { const callId = nextCallId++; calls.add(callId); return {kind:"value", value:callId}; }
    if (id === callControl.cancel || id === callControl.release) {
      if (!calls.has(args[0])) throw Error("unknown call ID");
      if (id === callControl.release) calls.delete(args[0]);
      return {kind:"value", value:null};
    }
    return backend.invokeSync(id, args);
  }}, host, {callControl});
}
const engine = {
  invokeSync(id, args) {
    if (id === 0) return {kind:"value", value: args[0]};
    if (id === 1) return {kind:"value", value: 42};
    if (id === 2) return {kind:"value", value: "ok"};
    if (id === 7) { starts += 1; return {kind:"value", value: 99}; }
    if (id === 11) return {kind:"value", value: "namespace-collision"};
    if (id === 12) return {kind:"value", value: "value-collision"};
    if (id === 13) return {kind:"value", value: "object-collision"};
    if (id === 14) return {kind:"value", value: 42};
    if (id === 15) return {kind:"value", value: "accepted"};
    if (id === 16) {
      assert.deepEqual(args[0], {error:"Io", detail:{tag:"Rejected",message:"payload"}, reports:[{tag:"Failed",error:"Io"},{tag:"Closed",error:null}]});
      return {kind:"value",value:args[0]};
    }
    if (id === 10) return {kind:"error", error:{errorName:"Failure", variant:"Rejected", data:{message:"no"}, message:"no"}};
    return {kind:"value", value:null};
  },
  async invokeAsync(id, args) { if (id === 8) return {kind:"value", value:{kind:"done"}}; return {kind:"value", value: args[0]}; },
  releaseObject() {}, close() {},
};
const session = makeSession(engine, new Host());
const api = createNamespace(session);
let starts = 0;
const errorState = api.echoErrorState({
  error:new api.UnitFailure("host message", "Io"),
  detail:new api.Failure("host message", "Rejected", {message:"payload"}),
  reports:[{tag:"Failed",error:new api.UnitFailure("host message", "Io")},{tag:"Closed",error:null}],
});
assert.ok(errorState.error instanceof api.UnitFailure);
assert.equal(errorState.error.variant,"Io");
assert.equal(errorState.error.data,"Io");
assert.ok(errorState.detail instanceof api.Failure);
assert.deepEqual(errorState.detail.data,{tag:"Rejected",message:"payload"});
assert.ok(errorState.reports[0].error instanceof api.UnitFailure);
assert.equal(errorState.reports[1].error,null);
api.echoErrorState(errorState);
const messageEvent = api.Event.Message({message:"hello"});
if (messageEvent.tag !== "Message" || messageEvent.message !== "hello") throw Error("enum variant surface");
if (api.Color.Red !== "Red" || api.Color.Blue !== "Blue") throw Error("unit enum surface");
const profile = {scores:new Map([["x", 2n]])};
const echoed = api.echo(profile);
if (!(echoed.scores instanceof Map) || echoed.scores.get("x") !== 2n) throw Error("record/map/bigint");
const service = api.Service.newService();
if (!(service instanceof api.Service && !Object.prototype.hasOwnProperty.call(service, "handle") && !Object.prototype.hasOwnProperty.call(service, "raw") && !Object.prototype.hasOwnProperty.call(service, "_handle") && !Object.prototype.hasOwnProperty.call(service, "_session"))) throw Error("constructor/object");
try { new api.Service(); throw Error("public object constructor"); } catch (error) { if (error.errorName !== "UniffiObjectConstructor") throw error; }
if (service.describe() !== "ok") throw Error("object method");
if (api.collision() !== "namespace-collision") throw Error("namespace owner collision");
if (api.Profile.collision(profile) !== "value-collision") throw Error("value owner collision");
if (service.collision() !== "object-collision") throw Error("object owner collision");
const returnedService = api.getService();
if (!(returnedService instanceof api.Service) || returnedService.describe() !== "ok") throw Error("returned object wrapper");
disposeObject(returnedService);
const secondSession = makeSession({invokeSync(id) { if (id === 1) return {kind:"value", value:77}; if (id === 2) return {kind:"value", value:"second"}; if (id === 15) return {kind:"value", value:"accepted-second"}; return {kind:"value", value:null}; }, async invokeAsync() { return {kind:"value", value:null}; }, close() {}}, new Host());
const secondApi = createNamespace(secondSession);
const secondService = secondApi.Service.newService();
if (!(secondService instanceof secondApi.Service) || secondService.describe() !== "second") throw Error("session-bound object constructor");
try { secondApi.acceptService(service); throw Error("cross-session object accepted"); } catch (error) { if (error.errorName !== "UniffiObjectSession") throw error; }
disposeObject(secondService);
await secondSession.close();
disposeObject(service);
disposeObject(service);
if (Object.keys(api).some((key) => key.startsWith("__") || ["registerCallback", "retainCallback", "releaseCallback", "invokeCallbackSync", "invokeCallbackAsync", "close"].includes(key))) throw Error("namespace internals");
const output = api.events();
if (!(output && typeof output.next === "function" && typeof output.cancel === "function")) throw Error("output stream");
if (starts !== 0) throw Error("output stream eager start");
if (!(await output.next()).done || starts !== 1) throw Error("output stream lazy start");
let pulled = false;
const input = session.createInputStream({next: async () => pulled ? {kind:"done"} : (pulled = true, {kind:"item", value:3}), cancel() {}}, {lowerItem: (value) => value + 1});
const inputItem = await session.host.pullInputStream(input.handle);
if (inputItem.kind !== "item" || inputItem.value !== 4) throw Error("input stream host/lower");
if ((await session.host.pullInputStream(input.handle)).kind !== "done") throw Error("input stream eof");
await input.cancel();
input.release();
let outputCancelCalls = 0;
let outputReleaseCalls = 0;
const cancellingSession = makeSession({invokeSync() { return 1; }, async invokeAsync() { return {kind:"item", value:1}; }, cancelOutputStream() { outputCancelCalls += 1; return Promise.reject({errorName:"CancelFailed", message:"cancel failed"}); }, releaseOutputStream() { outputReleaseCalls += 1; }, close() {}}, new Host());
const cancellingOutput = cancellingSession.createOutputStream({start: () => 1, next: () => ({kind:"item", value:1})});
await cancellingOutput.next();
try { await cancellingOutput.cancel(); throw Error("cancel rejection"); } catch (error) { if (error.errorName !== "CancelFailed") throw error; }
if (outputCancelCalls !== 1 || outputReleaseCalls !== 1 || Object.keys(cancellingOutput).includes("release")) throw Error("cancel cleanup");
await cancellingSession.close();
let leaseReleases = 0;
const leaseSession = makeSession({invokeSync() { return {kind:"value", value:7}; }, invokeAsync() { return {kind:"value", value:7}; }, releaseObject() { leaseReleases += 1; }, close() {}}, new Host());
const lease = createNamespace(leaseSession).getService();
await leaseSession.close();
disposeObject(lease);
if (leaseReleases !== 1) throw Error("object close/dispose idempotence");
const mixed = session.registerCallback(3, {syncMethod(value) { return value + 1; }, asyncMethod(value) { return Promise.resolve(value + 2); }}, {methods:{0:{name:"syncMethod", async:false}, 1:{name:"asyncMethod", async:true}}});
if (session.invokeCallbackSync(3, mixed, 0, [1]) !== 2) throw Error("sync callback");
if (await session.invokeCallbackAsync(3, mixed, 1, 99, [1]) !== 3) throw Error("async callback");
const badCallback = session.registerCallback(4, () => Promise.resolve(1), { methods: { 0: { name: null } } });
try { session.invokeCallbackSync(4, badCallback, 0, []); throw Error("sync callback Promise"); } catch (error) { if (error.errorName !== "UniffiCallbackProtocol") throw error; }
const badBackend = makeSession({invokeSync() { return Promise.resolve(1); }, async invokeAsync() { return {kind:"value", value:1}; }, close() {}}, new Host());
try { badBackend.invokeSync(0, []); throw Error("sync invoke Promise"); } catch (error) { if (error.errorName !== "UniffiBackendProtocol") throw error; }
await badBackend.close();
try { api.fail(); throw Error("error envelope"); } catch (error) { if (error.errorName !== "Failure" || error.variant !== "Rejected" || error.data?.message !== "no") throw error; }
await session.close();
await session.close();
try { api.echo(profile); throw Error("closed session"); } catch (error) { if (error.errorName !== "UniffiSessionClosed") throw error; }
"ok";
"#;
        fs::write(root.join("check.mjs"), script).unwrap();
        let output = Command::new("node")
            .arg("check.mjs")
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "node generated facade failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = fs::remove_dir_all(root);
    }
}
