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
    pub error: ValueType,
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
        if matches!(operation.source_key.owner(), OperationOwner::Callback(_))
            && operation.callback_method_id.is_none()
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
        error: public_stream_value_type(&resource.error, types).ok_or_else(|| {
            FacadeError::InvalidStreamBinding {
                path: resource.path.to_string(),
            }
        })?,
        is_send: resource.is_send,
        slot_operation_ids: resource.slot_operation_ids.clone(),
    })
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
            | ConversionRecipe::Bytes => from_rust_type(rust_type, types),
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
            ConversionRecipe::InputStream(inner) => Some(ValueType::InputStream(Box::new(
                from_conversion(inner, rust_type_inner(rust_type), types)?,
            ))),
            ConversionRecipe::OutputStream(inner) => Some(ValueType::OutputStream(Box::new(
                from_conversion(inner, rust_type_inner(rust_type), types)?,
            ))),
            ConversionRecipe::StreamStep { .. } => None,
        }
    }

    fn rust_type_inner(rust_type: &RustType) -> &RustType {
        match rust_type {
            RustType::Option(inner)
            | RustType::Sequence(inner)
            | RustType::Set(inner)
            | RustType::Stream(inner)
            | RustType::InputStream(inner)
            | RustType::Custom(inner) => inner,
            RustType::Map(_, value) => value,
            _ => rust_type,
        }
    }

    fn from_rust_type(rust_type: &RustType, types: &[JsType]) -> Option<ValueType> {
        match rust_type {
            RustType::Scalar(scalar) => Some(ValueType::Scalar(*scalar)),
            RustType::Timestamp => Some(ValueType::Timestamp),
            RustType::Duration => Some(ValueType::Duration),
            RustType::Option(inner) => {
                Some(ValueType::Optional(Box::new(from_rust_type(inner, types)?)))
            }
            RustType::Sequence(inner) => {
                Some(ValueType::Sequence(Box::new(from_rust_type(inner, types)?)))
            }
            RustType::Map(key, value) => Some(ValueType::Map(
                Box::new(from_rust_type(key, types)?),
                Box::new(from_rust_type(value, types)?),
            )),
            RustType::Set(inner) => Some(ValueType::Set(Box::new(from_rust_type(inner, types)?))),
            RustType::Stream(inner) => Some(ValueType::OutputStream(Box::new(from_rust_type(
                inner, types,
            )?))),
            RustType::InputStream(inner) => Some(ValueType::InputStream(Box::new(from_rust_type(
                inner, types,
            )?))),
            RustType::Custom(inner) => from_rust_type(inner, types),
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
        files.push(PublicFile::new(
            implementation_path,
            implementation.into_bytes(),
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
    r#"export class BackendSession {
  constructor(backend: unknown, host?: Host);
  readonly backend: unknown;
  readonly host: Host;
  invokeSync(operationId: number, args: unknown[]): unknown;
  invokeAsync(operationId: number, args: unknown[]): Promise<unknown>;
  releaseObject(handle: unknown): void;
  registerCallback(callbackType: number, callback: unknown, contract?: unknown): number;
  retainCallback(callbackType: number, callbackId: number): { release(): void };
  releaseCallback(callbackType: number, callbackId: number): void;
  invokeCallbackSync(callbackType: number, callbackId: number, methodId: number, args?: unknown[]): unknown;
  invokeCallbackAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: unknown[]): Promise<unknown>;
  pullInputStream(handle: unknown): Promise<unknown>;
  cancelInputStream(handle: unknown): Promise<void>;
  releaseInputStream(handle: unknown): void;
  cancelOutputStream(handle: unknown): Promise<void>;
  releaseOutputStream(handle: unknown): void;
  createInputStream(source: unknown, options?: unknown): UniFfiInputStream<unknown>;
  createOutputStream(options?: unknown): UniFfiStream<unknown>;
  close(): Promise<void>;
}
export class Host {
  retainCallback(callbackType: number, callbackId: number): void;
  releaseCallback(callbackType: number, callbackId: number): void;
  invokeCallbackSync(callbackType: number, callbackId: number, methodId: number, args?: unknown[]): unknown;
  invokeCallbackAsync(callbackType: number, callbackId: number, methodId: number, invocationId: number, args?: unknown[]): Promise<unknown>;
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
export class ObjectLease<T = unknown> {
  private constructor();
  dispose(): void;
}
export interface UniFfiStream<T> { next(): Promise<IteratorResult<T>>; cancel(): Promise<void>; }
export interface UniFfiInputStream<T> { next(): Promise<IteratorResult<T>>; cancel?(): Promise<void>; release?(): void; }
export declare function createBackendSession(backend: unknown, host?: Host): BackendSession;
export declare function createFacade(session: BackendSession, descriptors: unknown): Record<string, unknown>;
export declare function invokeOperation(session: BackendSession, descriptor: unknown, args?: unknown[]): unknown;
export declare function invokeObjectOperation(lease: ObjectLease<unknown>, descriptor: unknown, args?: unknown[]): unknown;
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
        "import {{ createFacade, invokeOperation, invokeObjectOperation, ObjectLease, UniffiError }} from \"../../shared/uniffi_runtime.{runtime_extension}\";\n"
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
    out.push_str("\n");
    out.push_str("const __objectClasses = Object.create(null);\n");
    out.push_str("const __errorClasses = Object.create(null);\n");
    out.push_str("const __descriptors = ");
    out.push_str(&render_descriptors(ast, component, &all_operations));
    out.push_str(";\n");
    out.push_str("__descriptors.classes = __objectClasses;\n");
    out.push_str("__descriptors.errorClasses = __errorClasses;\n");
    out.push_str("\nexport function createNamespace(session) { return createFacade(session, __descriptors); }\n");
    out.push_str("\n");

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
                "{}class {} extends ObjectLease {{ constructor() {{ throw new UniffiError({{ errorName: \"UniffiObjectConstructor\", message: \"object wrappers are created by the facade\" }}); }}",
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
                out.push_str(&format!(
                    "{}({}) {{ return invokeObjectOperation(this, __descriptors.operations[{}], [{}]); }}\n",
                    operation.name,
                    args,
                    operation.id.index(),
                    args
                ));
            }
            out.push_str("}\n");
            out.push_str(&format!(
                "__objectClasses[{}] = {{ typeName: \"{}\", typeKey: \"{}\", public: {}, publicClass: {} }};\n",
                ty.id.index(),
                escape_js(&ty.name),
                escape_js(&ty.source_key.to_string()),
                local,
                class_name
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
            "\"{}\":{{componentId:{},name:\"{}\",id:{},kind:\"{:?}\",async:{},receiver:{},receiverType:{},receiverTypeId:{},surface:\"{}\",ownerSurface:\"{}\",ownerType:{},ownerTypeId:{},valueSelf:{},public:{},objectType:{},args:[",
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
                .unwrap_or_else(|| "null".to_owned())
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
        render_value_descriptor(ast, &resource.error),
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
            let is_method = matches!(operation.source_key.owner(), OperationOwner::Callback(key) if key == &ty.source_key)
                || matches!(operation.source_key.owner(), OperationOwner::Object(key) if key == &ty.source_key && operation.kind == OperationKind::Method);
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
                    "\"{}\":{{name:\"{}\",async:{},args:[{}],returnType:{},throws:{},callbackContracts:{}}}",
                    id,
                    escape_js(&operation.name),
                    operation.async_kind == AsyncKind::Async,
                    args,
                    return_type,
                    throws,
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

fn render_runtime_import_line(import: &str, extension: &str) -> Result<String, FacadeError> {
    let trimmed = import.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() {
        return Ok(String::new());
    }
    if trimmed.starts_with("import type ") {
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

fn render_component_declaration(
    ast: &PublicAst,
    component: &AstComponent,
) -> Result<String, FacadeError> {
    let mut out = String::new();
    let extension = "js";
    out.push_str("// AUTOGENERATED public declaration; no implementation .ts files.\n");
    out.push_str(&format!(
        "import {{ UniffiError }} from \"../../shared/uniffi_runtime.{extension}\";\nimport type {{ BackendSession, UniFfiInputStream, UniFfiStream }} from \"../../shared/uniffi_runtime.{extension}\";\n\n"
    ));
    for ty in ast
        .types
        .iter()
        .filter(|ty| ty.source_key.component().namespace() == component.namespace)
    {
        if let AstTypeKind::Custom { config, .. } = &ty.kind {
            for import in &config.imports {
                out.push_str(&render_import_line(import));
            }
        }
    }
    out.push('\n');
    out.push_str("export interface Namespace {\n");
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
                    "    {}({}): {};\n",
                    operation.name,
                    all_arguments.join(", "),
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
    out.push_str("export declare function createNamespace(session: BackendSession): Namespace;\n");
    for ty in ast
        .types
        .iter()
        .filter(|ty| ty.source_key.component().namespace() == component.namespace)
    {
        render_type_declaration(ast, ty, &mut out)?;
    }
    out.push_str("export { UniffiError };\n");
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
        AstTypeKind::Error { .. } => {
            out.push_str(&format!(
                "export declare class {} extends UniffiError {{\n  constructor(message?: string, variant?: string | null, data?: unknown);\n  readonly errorName: \"{}\";\n  readonly variant: string | null;\n  readonly data: unknown;\n}}\n",
                ty.name, ty.name
            ));
        }
        AstTypeKind::Custom { builtin, config } => {
            let public_type = if config.public_type_name.is_empty() {
                render_public_type(ast, builtin)
            } else {
                config.public_type_name.clone()
            };
            out.push_str(&format!("export type {} = {};\n", ty.name, public_type));
        }
        AstTypeKind::Object { .. } => {
            out.push_str(&format!(
                "export declare class {} {{ private constructor();\n  dispose(): void;\n",
                ty.name
            ));
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
                    let return_type = if operation.async_kind == AsyncKind::Async {
                        format!("Promise<{}>", ty.name)
                    } else {
                        ty.name.clone()
                    };
                    out.push_str(&format!(
                        "  {}({}): {};\n",
                        operation.name, args, return_type
                    ));
                }
                out.push_str("}\n");
            }
        }
        AstTypeKind::Callback => {
            out.push_str(&format!("export interface {} {{\n", ty.name));
            for operation in ast.operations.iter().filter(|operation| matches!(operation.source_key.owner(), OperationOwner::Callback(key) if key == &ty.source_key)) {
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
        ValueType::InputStream(inner) => {
            format!("UniFfiInputStream<{}>", render_public_type(ast, inner))
        }
        ValueType::OutputStream(inner) => {
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
        ValueType::InputStream(inner) => format!(
            "{{kind:\"inputStream\",inner:{}}}",
            render_value_descriptor(ast, inner)
        ),
        ValueType::OutputStream(inner) => format!(
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
                    Some(ValueType::OutputStream(item)) => item.as_ref().clone(),
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
                    error: ValueType::Scalar(ScalarType::String),
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
            Some(ValueType::OutputStream(Box::new(ValueType::Named(event)))),
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
            "import { convert } from \"./convert.ts\";".into(),
        ];
        let implementation =
            render_component_implementation(&ast, &ast.components[0], "js").unwrap();
        assert!(!implementation.contains("import type"));
        assert!(!implementation.contains(".ts\""));
        assert!(implementation.contains("./convert.js"));
    }

    #[test]
    fn scalar_shapes_are_canonical() {
        let empty = PublicAst {
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
                        callback_method_id: None,
                        private_ffi_symbol: None,
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
        };
        let api = JsApiIr {
            target_universe: vec![PublicTarget::NodeNapi],
            resolved_config: ResolvedJsConfig {
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
            vec!["Index.ets", "Index.d.ets"]
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
            vec!["Index.ets", "Index.d.ets"]
        );
        for file in &ark {
            let text = String::from_utf8(file.bytes.clone()).unwrap();
            if file.path.ends_with(".ets") && !file.path.ends_with(".d.ets") {
                assert!(text.contains("ClosePolicy"));
                assert!(text.contains("onDeadline: \"detach\""));
                assert!(text.contains("class __ArkDetachedMarker"));
                assert!(!text.contains("const __DETACHED = \"__uniffi_detached__\""));
                assert!(text.contains("if (closed || detached) return new ArkDoneStep();"));
                assert!(text.contains("return guarded as ArkValue"));
            } else {
                assert!(!text.contains("ClosePolicy"));
                assert!(!text.contains("configureClosePolicy"));
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
        let declaration = String::from_utf8(
            shared
                .iter()
                .find(|file| file.path.ends_with(".d.ts") && file.path.contains("components"))
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
    fn ark_composite_owner_and_stream_surface_is_canonical() {
        let implementation = String::from_utf8(
            render_ark_inventory(&corpus_ast())
                .unwrap()
                .into_iter()
                .find(|file| file.path == "Index.ets")
                .unwrap()
                .bytes,
        )
        .unwrap();
        assert!(implementation.contains("__invokeValue12(self: Profile, session: BackendSession)"));
        assert!(implementation.contains("__invokeObject13(self: Service, session: BackendSession"));
        assert!(implementation.contains("const __arkEnum_Event: EventValue"));
        assert!(implementation.contains("Event: __corpus_Event"));
        assert!(implementation.contains("Color: __corpus_Color"));
        assert!(implementation.contains("readonly error: (value: ArkValue) => UniffiError"));
        assert!(implementation
            .contains("release: (handle: ArkValue): void => session.releaseOutputStream(handle)"));
        assert!(!implementation.contains("fromHandle"));
        assert!(!implementation.contains("handleValue"));
        assert!(!implementation.contains("export const Event:"));
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
            NamedTypeKind::Callback,
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
                callback_method_id: operation.definition.callback_method_id,
                private_ffi_symbol: None,
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
        };
        let api = JsApiIr {
            target_universe: vec![PublicTarget::NodeNapi],
            resolved_config: ResolvedJsConfig {
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
                kind: JsTypeKind::Callback,
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
        let implementation = String::from_utf8(
            facade
                .shared_files()
                .iter()
                .find(|file| file.path == "components/callback_e2e/index.js")
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
            root.join("components/callback_e2e/index.js"),
            implementation,
        )
        .unwrap();
        let script = format!(
            r#"
import {{ createNamespace }} from "./components/callback_e2e/index.js";
import {{ BackendSession, Host, UniffiError, lowerValue }} from "./shared/uniffi_runtime.js";
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
session = new BackendSession(engine, host);
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
            error_type: Box::new(Type::String),
            is_send: false,
        };
        let output = Type::Stream {
            item_type: Box::new(Type::String),
            error_type: Box::new(Type::UInt32),
            is_send: false,
        };
        group.add_item(
            FnMetadata {
                module_path: "normalized_streams_crate".into(),
                name: "consume".into(),
                orig_name: None,
                is_async: false,
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
                inputs: vec![],
                return_type: Some(Type::Stream {
                    item_type: Box::new(Type::String),
                    error_type: Box::new(Type::UInt32),
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
                .find(|file| file.path == "components/normalized_streams/index.js")
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
                .find(|file| file.path == "Index.ets")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        assert!(ark_implementation.contains("graceMs: 7"));
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
            root.join("components/normalized_streams/index.js"),
            implementation,
        )
        .unwrap();
        let script = format!(
            r#"
import {{ createNamespace }} from "./components/normalized_streams/index.js";
import {{ BackendSession, Host }} from "./shared/uniffi_runtime.js";
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
const session = new BackendSession(backend, host);
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
                        by_ref: false,
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
                .find(|file| file.path == "components/normalized_alpha/index.js")
                .unwrap()
                .bytes
                .clone(),
        )
        .unwrap();
        let beta_impl = String::from_utf8(
            facade
                .shared_files()
                .iter()
                .find(|file| file.path == "components/normalized_beta/index.js")
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
            root.join("components/normalized_alpha/index.js"),
            alpha_impl,
        )
        .unwrap();
        fs::write(root.join("components/normalized_beta/index.js"), beta_impl).unwrap();
        let script = format!(
            r#"
import * as alpha from "./components/normalized_alpha/index.js";
import * as beta from "./components/normalized_beta/index.js";
import {{ BackendSession, Host }} from "./shared/uniffi_runtime.js";
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
const session = new BackendSession(backend, host);
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
    fn generated_ecmascript_executes_with_operation_id_mock() {
        let ast = corpus_ast();
        let component = &ast.components[0];
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
        fs::write(root.join("components/corpus/index.js"), implementation).unwrap();
        let script = r#"
import { createNamespace } from "./components/corpus/index.js";
import { BackendSession, Host, ObjectLease, createFacade } from "./shared/uniffi_runtime.js";
const installPolicy = (session) => { createFacade(session, {closePolicy:{graceMs:5000,onDeadline:"detach"}}); return session; };
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
    if (id === 10) return {kind:"error", error:{errorName:"Failure", variant:"Rejected", data:{message:"no"}, message:"no"}};
    return {kind:"value", value:null};
  },
  async invokeAsync(id, args) { if (id === 8) return {kind:"value", value:{kind:"done"}}; return {kind:"value", value: args[0]}; },
  releaseObject() {}, close() {},
};
const session = new BackendSession(engine, new Host());
const api = createNamespace(session);
let starts = 0;
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
returnedService.dispose();
const secondSession = new BackendSession({invokeSync(id) { if (id === 1) return {kind:"value", value:77}; if (id === 2) return {kind:"value", value:"second"}; if (id === 15) return {kind:"value", value:"accepted-second"}; return {kind:"value", value:null}; }, async invokeAsync() { return {kind:"value", value:null}; }, close() {}}, new Host());
const secondApi = createNamespace(secondSession);
const secondService = secondApi.Service.newService();
if (!(secondService instanceof secondApi.Service) || secondService.describe() !== "second") throw Error("session-bound object constructor");
try { secondApi.acceptService(service); throw Error("cross-session object accepted"); } catch (error) { if (error.errorName !== "UniffiObjectSession") throw error; }
secondService.dispose();
await secondSession.close();
service.dispose();
service.dispose();
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
const cancellingSession = installPolicy(new BackendSession({invokeSync() { return 1; }, async invokeAsync() { return {kind:"item", value:1}; }, cancelOutputStream() { outputCancelCalls += 1; return Promise.reject({errorName:"CancelFailed", message:"cancel failed"}); }, releaseOutputStream() { outputReleaseCalls += 1; }, close() {}}, new Host()));
const cancellingOutput = cancellingSession.createOutputStream({start: () => 1, next: () => ({kind:"item", value:1})});
await cancellingOutput.next();
try { await cancellingOutput.cancel(); throw Error("cancel rejection"); } catch (error) { if (error.errorName !== "CancelFailed") throw error; }
if (outputCancelCalls !== 1 || outputReleaseCalls !== 1 || Object.keys(cancellingOutput).includes("release")) throw Error("cancel cleanup");
await cancellingSession.close();
let leaseReleases = 0;
const leaseSession = new BackendSession({invokeSync() { return {kind:"value", value:7}; }, invokeAsync() { return {kind:"value", value:7}; }, releaseObject() { leaseReleases += 1; }, close() {}}, new Host());
const lease = createNamespace(leaseSession).getService();
await leaseSession.close();
lease.dispose();
if (leaseReleases !== 1) throw Error("object close/dispose idempotence");
const mixed = session.registerCallback(3, {syncMethod(value) { return value + 1; }, asyncMethod(value) { return Promise.resolve(value + 2); }}, {methods:{0:"syncMethod", 1:"asyncMethod"}});
if (session.invokeCallbackSync(3, mixed, 0, [1]) !== 2) throw Error("sync callback");
if (await session.invokeCallbackAsync(3, mixed, 1, 99, [1]) !== 3) throw Error("async callback");
const badCallback = session.registerCallback(4, () => Promise.resolve(1), { methods: { 0: { name: null } } });
try { session.invokeCallbackSync(4, badCallback, 0, []); throw Error("sync callback Promise"); } catch (error) { if (error.errorName !== "UniffiCallbackProtocol") throw error; }
const badBackend = installPolicy(new BackendSession({invokeSync() { return Promise.resolve(1); }, async invokeAsync() { return {kind:"value", value:1}; }, close() {}}, new Host()));
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
