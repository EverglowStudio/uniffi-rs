//! The one ComponentInterface -> canonical JavaScript normalization pass.
//!
//! This module is intentionally the only place in the JavaScript bindgen
//! that walks a `ComponentInterface`.  It produces owned values from
//! [`super::ir`]; renderers and engine adapters must not import the UniFFI
//! interface types again.

use std::collections::BTreeMap;
use std::fmt;

use heck::{ToLowerCamelCase, ToUpperCamelCase};
use uniffi_bindgen::{
    interface::{
        AsType, Callable, ComponentInterface, DefaultValue, Enum, ObjectImpl, TraitKind, Type,
    },
    Component,
};
use uniffi_js_abi::{
    assign_component_ids, assign_operation_ids, assign_type_ids, ArgumentDefinition, AsyncKind,
    Capability, CapabilitySet, ComponentDefinition, ComponentKey, EnumVariant, FieldDefinition,
    IdentifiedComponent, IdentifiedOperation, IdentifiedType, NamedTypeKind, ObjectKind,
    OperationDefinition, OperationKind, OperationOwner, OperationSignature, OperationSourceKey,
    Ownership, PublicTarget, ReceiverDefinition, TypeDefinition, TypeSourceKey, ValueType,
};
use uniffi_js_engine_schema::{
    callback_type_for_path, enumerate_stream_use_sites, BridgePlan, BridgePlanInput,
    CallbackContract, CallbackUseSite, EngineCapabilities, EngineKind, PlannedOperation,
    StreamDirection, StreamUseSite, ValuePath, ValuePathSegment,
};
use uniffi_meta::{CallbackOperationKind, CallbackUseSiteMetadata, CallbackValuePathSegment};

use super::ir::{
    ConversionRecipe, EnginePlan, EngineRustBridgePlan, HostPlan, JsApiIr, JsArgument, JsComponent,
    JsCustomTypeConfig, JsDefaultValue, JsField, JsOperation, JsReceiver, JsType, JsTypeKind,
    JsVariant, LayoutPlan, NormalizedPackage, ResolvedJsConfig, RustArgumentBinding,
    RustBridgePlan, RustCallTarget, RustCarrier, RustObjectKind, RustOperationPlan, RustPath,
    RustReceiverBinding, RustResourceHook, RustReturnBinding, RustStreamResourceGroup, RustType,
    UNIFIED_TARGET_UNIVERSE,
};
use crate::{CustomTypeConfig, JsConfig};

/// Errors raised before any renderer or writer is called.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FrontendError {
    EmptyPackage,
    DuplicateComponentNamespace(String),
    DuplicateCrateRoot {
        root: String,
        first_component: String,
        second_component: String,
    },
    NoBuildTargets,
    DuplicateBuildTarget(PublicTarget),
    UnknownTypeOwner(String),
    UnsupportedType(String),
    InvalidMetadata(String),
    Contract(String),
}

impl fmt::Display for FrontendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyPackage => formatter.write_str("JavaScript package has no components"),
            Self::DuplicateComponentNamespace(namespace) => {
                write!(formatter, "duplicate component namespace {namespace:?}")
            }
            Self::DuplicateCrateRoot {
                root,
                first_component,
                second_component,
            } => write!(
                formatter,
                "normalized crate root {root:?} is shared by components {first_component:?} and {second_component:?}"
            ),
            Self::NoBuildTargets => formatter.write_str("at least one JavaScript build target is required"),
            Self::DuplicateBuildTarget(target) => {
                write!(formatter, "duplicate JavaScript build target {target:?}")
            }
            Self::UnknownTypeOwner(ty) => write!(formatter, "cannot resolve owner of type {ty}"),
            Self::UnsupportedType(ty) => write!(formatter, "unsupported JavaScript type {ty}"),
            Self::InvalidMetadata(message) => formatter.write_str(message),
            Self::Contract(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for FrontendError {}

/// Owned component/config input for the canonical pass.  The references are
/// used only while normalizing; no interface or config value is retained in
/// [`NormalizedPackage`].
pub struct BindingInput<'a> {
    pub components: &'a [Component<JsConfig>],
    pub build_targets: Vec<PublicTarget>,
}

impl<'a> BindingInput<'a> {
    pub fn new(components: &'a [Component<JsConfig>]) -> Self {
        Self {
            components,
            build_targets: UNIFIED_TARGET_UNIVERSE.to_vec(),
        }
    }

    pub fn with_build_targets(mut self, targets: impl IntoIterator<Item = PublicTarget>) -> Self {
        self.build_targets = targets.into_iter().collect();
        self
    }
}

/// Normalize all selected components exactly once.
pub fn normalize(input: BindingInput<'_>) -> Result<NormalizedPackage, FrontendError> {
    if input.components.is_empty() {
        return Err(FrontendError::EmptyPackage);
    }

    let mut components = input.components.iter().collect::<Vec<_>>();
    components.sort_by_key(|component| {
        (
            component.ci.namespace().to_owned(),
            component.ci.crate_name().to_owned(),
        )
    });

    let mut component_definitions = Vec::with_capacity(components.len());
    let mut component_keys = BTreeMap::<String, ComponentKey>::new();
    let mut crate_owners = BTreeMap::<String, ComponentKey>::new();
    for component in &components {
        let source_key = ComponentKey::new(component.ci.namespace().to_owned())
            .map_err(|error| FrontendError::Contract(error.to_string()))?;
        if component_keys
            .insert(component.ci.namespace().to_owned(), source_key.clone())
            .is_some()
        {
            return Err(FrontendError::DuplicateComponentNamespace(
                component.ci.namespace().to_owned(),
            ));
        }
        let crate_root = crate_root(component.ci.crate_name());
        if let Some(previous) = crate_owners.insert(crate_root.clone(), source_key.clone()) {
            if previous != source_key {
                return Err(FrontendError::DuplicateCrateRoot {
                    root: crate_root,
                    first_component: previous.namespace().to_owned(),
                    second_component: source_key.namespace().to_owned(),
                });
            }
        }
        component_definitions.push(
            ComponentDefinition::new(source_key, component.ci.namespace().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?,
        );
    }
    let identified_components = assign_component_ids(component_definitions)
        .map_err(|error| FrontendError::Contract(error.to_string()))?;
    let component_id_by_key = identified_components
        .iter()
        .map(|component| (component.definition.source_key.clone(), component.id))
        .collect::<BTreeMap<_, _>>();

    let (type_definitions, js_type_extras) = collect_type_definitions(&components, &crate_owners)?;
    let identified_types = assign_type_ids(type_definitions)
        .map_err(|error| FrontendError::Contract(error.to_string()))?;
    let type_id_by_key = identified_types
        .iter()
        .map(|ty| (ty.definition.source_key.clone(), ty.id))
        .collect::<BTreeMap<_, _>>();

    let (operation_defs, operation_extras) =
        collect_operation_definitions(&components, &crate_owners)?;
    let identified_operations = assign_operation_ids(operation_defs)
        .map_err(|error| FrontendError::Contract(error.to_string()))?;
    let mut js_components = identified_components
        .iter()
        .map(|component| JsComponent {
            id: component.id,
            source_key: component.definition.source_key.clone(),
            public_namespace: component.definition.public_namespace.clone(),
        })
        .collect::<Vec<_>>();
    js_components.sort_by_key(|component| component.id);

    let js_types = identified_types
        .iter()
        .map(|ty| {
            let extra = js_type_extras
                .get(&ty.definition.source_key)
                .expect("type extras are collected with every definition");
            JsType {
                id: ty.id,
                source_key: ty.definition.source_key.clone(),
                public_name: ty.definition.public_name.clone(),
                kind: extra.kind.clone(),
            }
        })
        .collect::<Vec<_>>();

    let js_operations = identified_operations
        .iter()
        .map(|operation| {
            let definition = &operation.definition;
            let component_id = component_id_by_key
                .get(definition.source_key.component())
                .copied()
                .expect("operation component was collected");
            let receiver = match definition.source_key.owner() {
                OperationOwner::Object(owner) | OperationOwner::Value(owner) => Some(JsReceiver {
                    object_type: *type_id_by_key
                        .get(owner)
                        .expect("object operation owner was collected"),
                }),
                _ => None,
            };
            JsOperation {
                id: operation.id,
                source_key: definition.source_key.clone(),
                component_id,
                public_name: definition.public_name.clone(),
                debug_name: definition.debug_name.clone(),
                kind: definition.source_key.kind(),
                arguments: definition
                    .signature
                    .arguments
                    .iter()
                    .map(|argument| JsArgument {
                        public_name: argument.public_name.clone(),
                        ty: argument.ty.clone(),
                        default: argument.default.clone(),
                    })
                    .collect(),
                return_type: definition.signature.return_type.clone(),
                async_kind: definition.signature.async_kind,
                throws: definition.signature.throws.clone(),
                receiver,
                callback_method_id: definition.callback_method_id,
            }
        })
        .collect::<Vec<_>>();

    let callback_use_sites = collect_callback_metadata(
        &components,
        &js_operations,
        &identified_operations,
        &identified_types,
        &js_type_extras,
        &crate_owners,
    )?;
    let stream_use_sites = enumerate_stream_use_sites(&identified_operations, &identified_types)
        .map_err(|report| FrontendError::Contract(report.to_string()))?;

    let bridge_input = build_bridge_input(
        &identified_components,
        &identified_types,
        &identified_operations,
        callback_use_sites.clone(),
        stream_use_sites.clone(),
    );
    let bridge = BridgePlan::build(bridge_input)
        .map_err(|report| FrontendError::Contract(report.to_string()))?;

    let required_capabilities = bridge
        .operations()
        .iter()
        .flat_map(|operation| operation.required_capabilities.iter())
        .collect::<CapabilitySet>();
    let target_universe = UNIFIED_TARGET_UNIVERSE.to_vec();
    let mut build_targets = input.build_targets;
    build_targets.sort();
    if build_targets.is_empty() {
        return Err(FrontendError::NoBuildTargets);
    }
    for pair in build_targets.windows(2) {
        if pair[0] == pair[1] {
            return Err(FrontendError::DuplicateBuildTarget(pair[0]));
        }
    }
    if build_targets
        .iter()
        .any(|target| !target_universe.contains(target))
    {
        return Err(FrontendError::Contract(
            "requested build target is outside the fixed JavaScript target universe".to_owned(),
        ));
    }
    let resolved_config = resolve_config(&components, &crate_owners, &type_id_by_key);
    let api = JsApiIr {
        target_universe,
        resolved_config,
        components: js_components,
        types: js_types,
        operations: js_operations,
        required_capabilities,
    };
    let rust = build_rust_bridge_plan(
        &api,
        &type_id_by_key,
        &js_type_extras,
        &operation_extras,
        bridge.callbacks(),
        &stream_use_sites,
        &build_targets,
    )?;
    let engines = build_targets
        .iter()
        .copied()
        .map(engine_for_target)
        .map(|engine| {
            let plan = rust.engines.get(&engine).ok_or_else(|| {
                FrontendError::Contract(format!(
                    "selected engine {engine:?} has no Rust bridge operation plan"
                ))
            })?;
            Ok(EnginePlan {
                engine,
                operation_ids: plan
                    .operations
                    .iter()
                    .map(|operation| operation.operation_id)
                    .collect(),
            })
        })
        .collect::<Result<Vec<_>, FrontendError>>()?;
    let host = HostPlan {
        component_ids: api
            .components
            .iter()
            .map(|component| component.id)
            .collect(),
    };
    Ok(NormalizedPackage {
        api,
        bridge,
        rust,
        build_targets,
        layout: LayoutPlan { files: Vec::new() },
        host,
        engines,
    })
}

#[derive(Clone)]
struct TypeExtra {
    kind: JsTypeKind,
    source_kind: SourceTypeKind,
    rust_path: RustPath,
}

#[derive(Clone)]
struct SourceField {
    public_name: String,
    rust_name: String,
    ty: ValueType,
    default: Option<JsDefaultValue>,
}

#[derive(Clone)]
struct SourceVariant {
    public_name: String,
    rust_name: String,
    fields: Vec<SourceField>,
}

#[derive(Clone)]
enum SourceTypeKind {
    Record { fields: Vec<SourceField> },
    Enum { variants: Vec<SourceVariant> },
    Error { variants: Vec<SourceVariant> },
    Custom { builtin: ValueType },
    Object,
    Callback,
}

fn collect_type_definitions(
    components: &[&Component<JsConfig>],
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<(Vec<TypeDefinition>, BTreeMap<TypeSourceKey, TypeExtra>), FrontendError> {
    let mut definitions = Vec::new();
    let mut extras = BTreeMap::new();
    for component in components {
        let component_key =
            owner_key_for_module(&component.ci, component.ci.crate_name(), crate_owners)?;
        for record in component.ci.record_definitions() {
            let key = TypeSourceKey::new(component_key.clone(), record.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let fields = record
                .fields()
                .iter()
                .map(|field| {
                    Ok(SourceField {
                        public_name: field.name().to_lower_camel_case(),
                        rust_name: field.rust_name().to_owned(),
                        ty: convert_type(&field.as_type(), crate_owners)?,
                        default: field.default_value().map(convert_default),
                    })
                })
                .collect::<Result<Vec<_>, FrontendError>>()?;
            let bridge_fields = fields
                .iter()
                .map(|field| {
                    FieldDefinition::new(field.public_name.clone(), field.ty.clone())
                        .map_err(|error| FrontendError::Contract(error.to_string()))
                })
                .collect::<Result<Vec<_>, _>>()?;
            definitions.push(
                TypeDefinition::new(
                    key.clone(),
                    record.name().to_owned(),
                    NamedTypeKind::Record {
                        fields: bridge_fields,
                    },
                )
                .map_err(|error| FrontendError::Contract(error.to_string()))?,
            );
            extras.insert(
                key,
                TypeExtra {
                    kind: JsTypeKind::Record {
                        fields: fields.iter().map(public_field).collect(),
                    },
                    source_kind: SourceTypeKind::Record { fields },
                    rust_path: record_rust_path(record, &component.ci),
                },
            );
        }
        for enum_ in component.ci.enum_definitions() {
            let key = TypeSourceKey::new(component_key.clone(), enum_.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let variants = enum_
                .variants()
                .iter()
                .map(|variant| convert_variant(variant, crate_owners))
                .collect::<Result<Vec<_>, _>>()?;
            let bridge_variants = variants
                .iter()
                .map(|variant| convert_bridge_variant(variant))
                .collect::<Result<Vec<_>, _>>()?;
            let kind = if component.ci.is_name_used_as_error(enum_.name()) {
                NamedTypeKind::Error {
                    variants: bridge_variants,
                }
            } else {
                NamedTypeKind::Enum {
                    variants: bridge_variants,
                }
            };
            definitions.push(
                TypeDefinition::new(key.clone(), enum_.name().to_owned(), kind)
                    .map_err(|error| FrontendError::Contract(error.to_string()))?,
            );
            extras.insert(
                key,
                TypeExtra {
                    kind: if component.ci.is_name_used_as_error(enum_.name()) {
                        JsTypeKind::Error {
                            variants: variants.iter().map(public_variant).collect(),
                        }
                    } else {
                        JsTypeKind::Enum {
                            variants: variants.iter().map(public_variant).collect(),
                        }
                    },
                    source_kind: if component.ci.is_name_used_as_error(enum_.name()) {
                        SourceTypeKind::Error { variants }
                    } else {
                        SourceTypeKind::Enum { variants }
                    },
                    rust_path: enum_rust_path(enum_, &component.ci),
                },
            );
        }
        for object in component.ci.object_definitions() {
            let key = TypeSourceKey::new(component_key.clone(), object.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let object_kind = abi_object_kind(object.imp());
            definitions.push(
                TypeDefinition::new(
                    key.clone(),
                    object.name().to_owned(),
                    NamedTypeKind::Object { kind: object_kind },
                )
                .map_err(|error| FrontendError::Contract(error.to_string()))?,
            );
            extras.insert(
                key,
                TypeExtra {
                    kind: JsTypeKind::Object { kind: object_kind },
                    source_kind: SourceTypeKind::Object,
                    rust_path: RustPath::from_module_path(
                        object
                            .as_type()
                            .module_path()
                            .unwrap_or(component.ci.crate_name()),
                        &rust_object_item_name(object),
                    ),
                },
            );
        }
        for callback in component.ci.callback_interface_definitions() {
            let key = TypeSourceKey::new(component_key.clone(), callback.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            definitions.push(
                TypeDefinition::new(
                    key.clone(),
                    callback.name().to_owned(),
                    NamedTypeKind::Callback,
                )
                .map_err(|error| FrontendError::Contract(error.to_string()))?,
            );
            extras.insert(
                key,
                TypeExtra {
                    kind: JsTypeKind::Callback,
                    source_kind: SourceTypeKind::Callback,
                    rust_path: RustPath::from_module_path(callback.module_path(), callback.name()),
                },
            );
        }
        for custom in component.ci.custom_type_definitions() {
            let key = TypeSourceKey::new(component_key.clone(), custom.name.to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let builtin = convert_type(&custom.builtin, crate_owners)?;
            let config =
                custom_type_config(component.config.custom_type(&custom.name), &custom.name);
            definitions.push(
                TypeDefinition::new(
                    key.clone(),
                    config.public_type_name.clone(),
                    NamedTypeKind::Custom {
                        builtin: builtin.clone(),
                    },
                )
                .map_err(|error| FrontendError::Contract(error.to_string()))?,
            );
            extras.insert(
                key,
                TypeExtra {
                    kind: JsTypeKind::Custom {
                        builtin: builtin.clone(),
                        config: config.clone(),
                    },
                    source_kind: SourceTypeKind::Custom { builtin },
                    rust_path: RustPath::from_module_path(&custom.module_path, &custom.name),
                },
            );
        }
    }
    Ok((definitions, extras))
}

fn public_field(field: &SourceField) -> JsField {
    JsField {
        public_name: field.public_name.clone(),
        ty: field.ty.clone(),
        default: field.default.clone(),
    }
}

fn public_variant(variant: &SourceVariant) -> JsVariant {
    JsVariant {
        public_name: variant.public_name.clone(),
        fields: variant.fields.iter().map(public_field).collect(),
    }
}

fn record_rust_path(
    record: &uniffi_bindgen::interface::Record,
    ci: &ComponentInterface,
) -> RustPath {
    match record.rust_path() {
        Some(path) => RustPath::from_relative_path(ci.crate_name(), path),
        None => RustPath::from_module_path(
            record.as_type().module_path().unwrap_or(ci.crate_name()),
            record.rust_name(),
        ),
    }
}

fn enum_rust_path(enum_: &Enum, ci: &ComponentInterface) -> RustPath {
    match enum_.rust_path() {
        Some(path) => RustPath::from_relative_path(ci.crate_name(), path),
        None => RustPath::from_module_path(&enum_module_path(enum_), enum_.rust_name()),
    }
}

fn rust_object_kind(implementation: &ObjectImpl) -> RustObjectKind {
    match implementation {
        ObjectImpl::Struct => RustObjectKind::Struct,
        ObjectImpl::Trait(TraitKind::RustOnly) => RustObjectKind::TraitRustOnly,
        ObjectImpl::Trait(TraitKind::Both) => RustObjectKind::TraitBoth,
        ObjectImpl::Trait(TraitKind::ForeignOnly) => RustObjectKind::TraitForeignOnly,
    }
}

/// `Object::rust_name` is intended for Rust type syntax and therefore prefixes
/// trait objects with `dyn`.  A bridge path identifies the underlying core
/// item, so keep the raw identifier as a path segment and represent the trait
/// object distinction separately in [`RustObjectKind`].
fn rust_object_item_name(object: &uniffi_bindgen::interface::Object) -> String {
    let name = object.rust_name();
    name.strip_prefix("dyn ").unwrap_or(&name).to_owned()
}

fn abi_object_kind(implementation: &ObjectImpl) -> ObjectKind {
    match implementation {
        ObjectImpl::Struct => ObjectKind::Struct,
        ObjectImpl::Trait(TraitKind::RustOnly) => ObjectKind::TraitRustOnly,
        ObjectImpl::Trait(TraitKind::Both) => ObjectKind::TraitBoth,
        ObjectImpl::Trait(TraitKind::ForeignOnly) => ObjectKind::TraitForeignOnly,
    }
}

#[derive(Clone)]
struct OperationExtra {
    module_path: RustPath,
    type_path: Option<RustPath>,
    object_kind: Option<RustObjectKind>,
    /// Source Rust item name used by the structured core call target.
    item_name: String,
    /// Generated private FFI symbol, kept outside the core call target.
    private_ffi_symbol: Option<String>,
    argument_rust_names: Vec<String>,
    argument_ownership: Vec<Ownership>,
    receiver_ownership: Option<Ownership>,
}

fn collect_operation_definitions(
    components: &[&Component<JsConfig>],
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<
    (
        Vec<OperationDefinition>,
        BTreeMap<OperationSourceKey, OperationExtra>,
    ),
    FrontendError,
> {
    let mut definitions = Vec::new();
    let mut extras = BTreeMap::new();
    let mut push = |result: Result<(OperationDefinition, OperationExtra), FrontendError>| {
        let (definition, extra) = result?;
        extras.insert(definition.source_key.clone(), extra);
        definitions.push(definition);
        Ok::<(), FrontendError>(())
    };
    for component in components {
        let component_key =
            owner_key_for_module(&component.ci, component.ci.crate_name(), crate_owners)?;
        let module_path = RustPath::from_module_path(component.ci.crate_name(), "");
        for function in component.ci.function_definitions() {
            push(operation_definition(
                &component.ci,
                &component_key,
                OperationOwner::Namespace,
                OperationKind::Function,
                function.name(),
                function,
                None,
                None,
                module_path.clone(),
                None,
                crate_owners,
            ))?;
        }
        for object in component.ci.object_definitions() {
            let owner = TypeSourceKey::new(component_key.clone(), object.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let type_path = RustPath::from_module_path(
                object
                    .as_type()
                    .module_path()
                    .unwrap_or(component.ci.crate_name()),
                &rust_object_item_name(object),
            );
            let object_kind = rust_object_kind(object.imp());
            for constructor in object.constructors() {
                push(operation_definition(
                    &component.ci,
                    &component_key,
                    OperationOwner::Object(owner.clone()),
                    OperationKind::Constructor,
                    constructor.name(),
                    constructor,
                    None,
                    None,
                    module_path.clone(),
                    Some((type_path.clone(), object_kind)),
                    crate_owners,
                ))?;
            }
            let callback_methods = matches!(
                object.imp(),
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
            );
            for (method_index, method) in object.methods().iter().enumerate() {
                if !callback_methods || matches!(object.imp(), ObjectImpl::Trait(TraitKind::Both)) {
                    push(operation_definition(
                        &component.ci,
                        &component_key,
                        OperationOwner::Object(owner.clone()),
                        OperationKind::Method,
                        method.name(),
                        method,
                        Some(if method.takes_self_by_arc() {
                            Ownership::Owned
                        } else {
                            Ownership::Borrowed
                        }),
                        None,
                        module_path.clone(),
                        Some((type_path.clone(), object_kind)),
                        crate_owners,
                    ))?;
                }
                if callback_methods {
                    let mut callback_operation = operation_definition(
                        &component.ci,
                        &component_key,
                        OperationOwner::Callback(owner.clone()),
                        OperationKind::CallbackMethod,
                        method.name(),
                        method,
                        None,
                        Some(method_index as u32),
                        module_path.clone(),
                        Some((type_path.clone(), object_kind)),
                        crate_owners,
                    )?;
                    let callback_symbol = object
                        .vtable_methods()
                        .get(method_index)
                        .map(|(callback, _)| callback.name().to_owned())
                        .unwrap_or_else(|| {
                            format!("{}__callback", callback_operation.0.private_symbol)
                        });
                    // The vtable callback is a distinct host slot even though
                    // it is derived from the same source method.  Keep its
                    // canonical symbol separate from the Object::Method FFI
                    // export so schema symbol validation cannot merge the two
                    // directions.
                    callback_operation.0.private_symbol = format!(
                        "{}::{}",
                        crate_root(component.ci.crate_name()),
                        callback_symbol
                    );
                    callback_operation.1.private_ffi_symbol = Some(callback_symbol);
                    push(Ok(callback_operation))?;
                }
            }
            let trait_methods = object.uniffi_trait_methods();
            for method in [
                trait_methods.debug_fmt.as_ref(),
                trait_methods.display_fmt.as_ref(),
                trait_methods.eq_eq.as_ref(),
                trait_methods.eq_ne.as_ref(),
                trait_methods.hash_hash.as_ref(),
                trait_methods.ord_cmp.as_ref(),
            ]
            .into_iter()
            .flatten()
            {
                push(operation_definition(
                    &component.ci,
                    &component_key,
                    OperationOwner::Object(owner.clone()),
                    OperationKind::Method,
                    method.name(),
                    method,
                    Some(if method.takes_self_by_arc() {
                        Ownership::Owned
                    } else {
                        Ownership::Borrowed
                    }),
                    None,
                    module_path.clone(),
                    Some((type_path.clone(), object_kind)),
                    crate_owners,
                ))?;
            }
        }
        for record in component.ci.record_definitions() {
            let owner = TypeSourceKey::new(component_key.clone(), record.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let type_path = record_rust_path(record, &component.ci);
            for constructor in record.constructors() {
                push(operation_definition(
                    &component.ci,
                    &component_key,
                    OperationOwner::Value(owner.clone()),
                    OperationKind::Constructor,
                    constructor.name(),
                    constructor,
                    None,
                    None,
                    module_path.clone(),
                    Some((type_path.clone(), RustObjectKind::Struct)),
                    crate_owners,
                ))?;
            }
            for method in record.methods() {
                push(operation_definition(
                    &component.ci,
                    &component_key,
                    OperationOwner::Value(owner.clone()),
                    OperationKind::Method,
                    method.name(),
                    method,
                    Some(if method.takes_self_by_arc() {
                        Ownership::Owned
                    } else {
                        Ownership::Borrowed
                    }),
                    None,
                    module_path.clone(),
                    Some((type_path.clone(), RustObjectKind::Struct)),
                    crate_owners,
                ))?;
            }
        }
        for enum_ in component.ci.enum_definitions() {
            let owner = TypeSourceKey::new(component_key.clone(), enum_.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let type_path = enum_rust_path(enum_, &component.ci);
            for constructor in enum_.constructors() {
                push(operation_definition(
                    &component.ci,
                    &component_key,
                    OperationOwner::Value(owner.clone()),
                    OperationKind::Constructor,
                    constructor.name(),
                    constructor,
                    None,
                    None,
                    module_path.clone(),
                    Some((type_path.clone(), RustObjectKind::Struct)),
                    crate_owners,
                ))?;
            }
            for method in enum_.methods() {
                push(operation_definition(
                    &component.ci,
                    &component_key,
                    OperationOwner::Value(owner.clone()),
                    OperationKind::Method,
                    method.name(),
                    method,
                    Some(if method.takes_self_by_arc() {
                        Ownership::Owned
                    } else {
                        Ownership::Borrowed
                    }),
                    None,
                    module_path.clone(),
                    Some((type_path.clone(), RustObjectKind::Struct)),
                    crate_owners,
                ))?;
            }
        }
        for callback in component.ci.callback_interface_definitions() {
            let owner = TypeSourceKey::new(component_key.clone(), callback.name().to_owned())
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
            let type_path = RustPath::from_module_path(callback.module_path(), callback.name());
            for (method_id, method) in callback.methods().into_iter().enumerate() {
                push(operation_definition(
                    &component.ci,
                    &component_key,
                    OperationOwner::Callback(owner.clone()),
                    OperationKind::CallbackMethod,
                    method.name(),
                    method,
                    None,
                    Some(method_id as u32),
                    module_path.clone(),
                    Some((type_path.clone(), RustObjectKind::TraitBoth)),
                    crate_owners,
                ))?;
            }
        }
    }
    Ok((definitions, extras))
}

fn operation_definition(
    ci: &ComponentInterface,
    component_key: &ComponentKey,
    owner: OperationOwner,
    kind: OperationKind,
    name: &str,
    callable: &dyn Callable,
    receiver_ownership: Option<Ownership>,
    callback_method_id: Option<u32>,
    _module_path: RustPath,
    type_target: Option<(RustPath, RustObjectKind)>,
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<(OperationDefinition, OperationExtra), FrontendError> {
    let source_key =
        OperationSourceKey::new(component_key.clone(), owner.clone(), kind, name.to_owned())
            .map_err(|error| FrontendError::Contract(error.to_string()))?;
    let callable_arguments = callable.arguments();
    let argument_rust_names = callable_arguments
        .iter()
        .map(|argument| argument.name().to_owned())
        .collect::<Vec<_>>();
    let argument_ownership = callable_arguments
        .iter()
        .map(|argument| {
            if argument.by_ref() {
                Ownership::Borrowed
            } else {
                Ownership::Owned
            }
        })
        .collect::<Vec<_>>();
    let arguments = callable_arguments
        .into_iter()
        .into_iter()
        .map(|argument| {
            let definition = ArgumentDefinition::new(
                argument.name().to_lower_camel_case(),
                convert_type(&argument.as_type(), crate_owners)?,
                if argument.by_ref() {
                    Ownership::Borrowed
                } else {
                    Ownership::Owned
                },
            )
            .map_err(|error| FrontendError::Contract(error.to_string()))?;
            Ok::<_, FrontendError>(match argument.default_value() {
                Some(default) => definition.with_default(convert_default(default)),
                None => definition,
            })
        })
        .collect::<Result<Vec<_>, FrontendError>>()?;
    let return_type = callable
        .return_type()
        .map(|ty| convert_type(ty, crate_owners))
        .transpose()?;
    let throws = callable
        .throws_type()
        .map(|ty| source_key_for_type(ty, crate_owners))
        .transpose()?;
    let private_symbol = callable.ffi_func().name().to_owned();
    let module_path =
        RustPath::from_module_path(callable.module_path().unwrap_or(ci.crate_name()), "");
    let debug_name = format!("{}.{name}", ci.namespace());
    let public_name = if kind == OperationKind::Constructor && name == "new" {
        "new".to_owned()
    } else {
        name.to_lower_camel_case()
    };
    let mut definition = OperationDefinition::new(
        source_key,
        public_name,
        debug_name,
        private_symbol.clone(),
        OperationSignature {
            arguments,
            return_type,
            async_kind: if callable.is_async() {
                AsyncKind::Async
            } else {
                AsyncKind::Sync
            },
            throws,
        },
    )
    .map_err(|error| FrontendError::Contract(error.to_string()))?;
    if let Some(ownership) = receiver_ownership {
        definition = definition.with_receiver(ReceiverDefinition { ownership });
    }
    if let Some(method_id) = callback_method_id {
        definition = definition.with_callback_method_id(method_id);
    }
    Ok((
        definition,
        OperationExtra {
            module_path,
            type_path: type_target.as_ref().map(|(path, _)| path.clone()),
            object_kind: type_target.map(|(_, kind)| kind),
            item_name: callable.rust_name().to_owned(),
            private_ffi_symbol: Some(private_symbol),
            argument_rust_names,
            argument_ownership,
            receiver_ownership,
        },
    ))
}

fn collect_callback_metadata(
    components: &[&Component<JsConfig>],
    js_operations: &[JsOperation],
    operations: &[IdentifiedOperation],
    types: &[IdentifiedType],
    type_extras: &BTreeMap<TypeSourceKey, TypeExtra>,
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<Vec<CallbackUseSite>, FrontendError> {
    let mut contracts = Vec::new();
    for component in components {
        for metadata in component.ci.callback_use_sites() {
            let component_key = crate_owners
                .get(&crate_root(&metadata.module_path))
                .cloned()
                .ok_or_else(|| FrontendError::InvalidMetadata(metadata.module_path.clone()))?;
            for (kind, owner) in metadata_operation_owners(metadata, &component_key, type_extras)? {
                let source_key = OperationSourceKey::new(
                    component_key.clone(),
                    owner,
                    kind,
                    metadata.operation_name.clone(),
                )
                .map_err(|error| FrontendError::InvalidMetadata(error.to_string()))?;
                let operation = operations
                    .iter()
                    .find(|operation| operation.definition.source_key == source_key)
                    .ok_or_else(|| {
                        FrontendError::InvalidMetadata(format!(
                            "callback contract references unknown operation {source_key}"
                        ))
                    })?;
                let js_operation = js_operations
                    .iter()
                    .find(|candidate| candidate.id == operation.id)
                    .ok_or_else(|| FrontendError::InvalidMetadata(source_key.to_string()))?;
                let path = map_callback_path(js_operation, &metadata.path, type_extras)?;
                let callback_type =
                    callback_type_for_path(operation, &path, types).ok_or_else(|| {
                        FrontendError::InvalidMetadata(format!(
                            "callback contract path {} does not resolve to a callback in {}",
                            metadata.path, source_key
                        ))
                    })?;
                contracts.push(CallbackUseSite {
                    operation_id: operation.id,
                    callback_type,
                    path,
                    contract: CallbackContract {
                        retention: match metadata.contract.retention {
                            uniffi_meta::CallbackRetention::Scoped => {
                                uniffi_js_engine_schema::CallbackRetention::Scoped
                            }
                            uniffi_meta::CallbackRetention::Retained => {
                                uniffi_js_engine_schema::CallbackRetention::Retained
                            }
                        },
                        threading: match metadata.contract.threading {
                            uniffi_meta::CallbackThreading::CallingThread => {
                                uniffi_js_engine_schema::CallbackThreading::CallingThread
                            }
                            uniffi_meta::CallbackThreading::MayCrossThread => {
                                uniffi_js_engine_schema::CallbackThreading::MayCrossThread
                            }
                        },
                        reentrancy: match metadata.contract.reentrancy {
                            uniffi_meta::CallbackReentrancy::Forbidden => {
                                uniffi_js_engine_schema::CallbackReentrancy::Forbidden
                            }
                            uniffi_meta::CallbackReentrancy::Allowed => {
                                uniffi_js_engine_schema::CallbackReentrancy::Allowed
                            }
                        },
                    },
                });
            }
        }
    }
    contracts.sort_by_key(|contract| (contract.operation_id, contract.path.clone()));
    Ok(contracts)
}

fn metadata_operation_owners(
    metadata: &CallbackUseSiteMetadata,
    component_key: &ComponentKey,
    type_extras: &BTreeMap<TypeSourceKey, TypeExtra>,
) -> Result<Vec<(OperationKind, OperationOwner)>, FrontendError> {
    Ok(match metadata.operation_kind {
        CallbackOperationKind::Function => {
            vec![(OperationKind::Function, OperationOwner::Namespace)]
        }
        CallbackOperationKind::Constructor | CallbackOperationKind::Method => {
            let owner_name = metadata.owner.as_deref().ok_or_else(|| {
                FrontendError::InvalidMetadata(format!(
                    "callback contract for {} is missing its owner",
                    metadata.operation_name
                ))
            })?;
            let owner = TypeSourceKey::new(component_key.clone(), owner_name.to_owned())
                .map_err(|error| FrontendError::InvalidMetadata(error.to_string()))?;
            if metadata.operation_kind == CallbackOperationKind::Constructor {
                vec![(OperationKind::Constructor, OperationOwner::Object(owner))]
            } else if matches!(
                type_extras.get(&owner).map(|extra| &extra.kind),
                Some(JsTypeKind::Object {
                    kind: ObjectKind::TraitBoth | ObjectKind::TraitForeignOnly,
                })
            ) {
                if matches!(
                    type_extras.get(&owner).map(|extra| &extra.kind),
                    Some(JsTypeKind::Object {
                        kind: ObjectKind::TraitBoth,
                    })
                ) {
                    // A Both trait has two canonical directions.  The same
                    // source contract validates both operation legs.
                    vec![
                        (OperationKind::Method, OperationOwner::Object(owner.clone())),
                        (
                            OperationKind::CallbackMethod,
                            OperationOwner::Callback(owner),
                        ),
                    ]
                } else {
                    // ForeignOnly has no JS -> Rust method leg.
                    vec![(
                        OperationKind::CallbackMethod,
                        OperationOwner::Callback(owner),
                    )]
                }
            } else {
                vec![(OperationKind::Method, OperationOwner::Object(owner))]
            }
        }
        CallbackOperationKind::CallbackMethod => {
            let owner_name = metadata.owner.as_deref().ok_or_else(|| {
                FrontendError::InvalidMetadata(format!(
                    "callback contract for {} is missing its owner",
                    metadata.operation_name
                ))
            })?;
            let owner = TypeSourceKey::new(component_key.clone(), owner_name.to_owned())
                .map_err(|error| FrontendError::InvalidMetadata(error.to_string()))?;
            vec![(
                OperationKind::CallbackMethod,
                OperationOwner::Callback(owner),
            )]
        }
    })
}

fn map_callback_path(
    operation: &JsOperation,
    metadata_path: &uniffi_meta::CallbackValuePath,
    type_extras: &BTreeMap<TypeSourceKey, TypeExtra>,
) -> Result<ValuePath, FrontendError> {
    let segments = metadata_path.segments();
    let (mut value, mut index, mut path) = match segments.first() {
        Some(CallbackValuePathSegment::Argument(argument_index)) => {
            let argument = operation
                .arguments
                .get(*argument_index as usize)
                .ok_or_else(|| {
                    FrontendError::InvalidMetadata(format!(
                        "callback contract path {} references missing argument",
                        metadata_path
                    ))
                })?;
            (
                argument.ty.clone(),
                1usize,
                ValuePath::argument(*argument_index),
            )
        }
        Some(CallbackValuePathSegment::Return) => (
            operation.return_type.clone().ok_or_else(|| {
                FrontendError::InvalidMetadata(format!(
                    "callback contract path {} references a missing return",
                    metadata_path
                ))
            })?,
            1usize,
            ValuePath::return_value(),
        ),
        _ => {
            return Err(FrontendError::InvalidMetadata(format!(
                "callback contract path {} must start at argument or return",
                metadata_path
            )))
        }
    };
    while index < segments.len() {
        let (next, canonical_segments, consumed) =
            map_callback_path_segment(&value, &segments[index..], type_extras, metadata_path)?;
        value = next;
        for segment in canonical_segments {
            path = path.then(segment);
        }
        index += consumed;
    }
    Ok(path)
}

fn map_callback_path_segment(
    value: &ValueType,
    segments: &[CallbackValuePathSegment],
    type_extras: &BTreeMap<TypeSourceKey, TypeExtra>,
    full_path: &uniffi_meta::CallbackValuePath,
) -> Result<(ValueType, Vec<ValuePathSegment>, usize), FrontendError> {
    let segment = segments.first().ok_or_else(|| {
        FrontendError::InvalidMetadata(format!("empty callback path {full_path}"))
    })?;
    match (value, segment) {
        (ValueType::Optional(inner), _) => {
            map_callback_path_segment(inner, segments, type_extras, full_path)
        }
        (ValueType::Sequence(inner), CallbackValuePathSegment::SequenceItem)
        | (ValueType::InputStream(inner), CallbackValuePathSegment::SequenceItem)
        | (ValueType::OutputStream(inner), CallbackValuePathSegment::SequenceItem) => {
            Ok(((*inner.clone()), vec![ValuePathSegment::SequenceItem], 1))
        }
        (ValueType::Set(inner), CallbackValuePathSegment::SetItem) => {
            Ok(((*inner.clone()), vec![ValuePathSegment::SetItem], 1))
        }
        (ValueType::Map(key, _), CallbackValuePathSegment::MapKey) => {
            Ok(((*key.clone()), vec![ValuePathSegment::MapKey], 1))
        }
        (ValueType::Map(_, value), CallbackValuePathSegment::MapValue) => {
            Ok(((*value.clone()), vec![ValuePathSegment::MapValue], 1))
        }
        (ValueType::Named(key), CallbackValuePathSegment::Field(name)) => {
            let ty = type_extras
                .get(key)
                .ok_or_else(|| FrontendError::InvalidMetadata(key.to_string()))?;
            let fields = match &ty.source_kind {
                SourceTypeKind::Record { fields } => fields,
                SourceTypeKind::Custom { builtin, .. } => {
                    return map_callback_path_segment(builtin, segments, type_extras, full_path)
                }
                _ => {
                    return Err(FrontendError::InvalidMetadata(format!(
                        "field {name:?} is not present at {full_path}"
                    )))
                }
            };
            let field = fields
                .iter()
                .find(|field| field.rust_name == *name)
                .ok_or_else(|| FrontendError::InvalidMetadata(format!("unknown field {name:?}")))?;
            Ok((
                field.ty.clone(),
                vec![ValuePathSegment::Field(field.public_name.clone())],
                1,
            ))
        }
        (ValueType::Named(key), CallbackValuePathSegment::Variant(name)) => {
            let ty = type_extras
                .get(key)
                .ok_or_else(|| FrontendError::InvalidMetadata(key.to_string()))?;
            let variants = match &ty.source_kind {
                SourceTypeKind::Enum { variants } | SourceTypeKind::Error { variants } => variants,
                SourceTypeKind::Custom { builtin, .. } => {
                    return map_callback_path_segment(builtin, segments, type_extras, full_path)
                }
                _ => {
                    return Err(FrontendError::InvalidMetadata(format!(
                        "variant {name:?} is not present at {full_path}"
                    )))
                }
            };
            let variant = variants
                .iter()
                .find(|variant| variant.rust_name == *name)
                .ok_or_else(|| {
                    FrontendError::InvalidMetadata(format!("unknown variant {name:?}"))
                })?;
            // A variant only changes the lookup context.  The following field
            // segment resolves against the variant in the next call.
            let field = segments.get(1).and_then(|segment| match segment {
                CallbackValuePathSegment::Field(name) => {
                    variant.fields.iter().find(|field| field.rust_name == *name)
                }
                _ => None,
            });
            let field = field.ok_or_else(|| {
                FrontendError::InvalidMetadata(format!(
                    "variant {name:?} must be followed by a field"
                ))
            })?;
            Ok((
                field.ty.clone(),
                vec![
                    ValuePathSegment::Variant(variant.public_name.clone()),
                    ValuePathSegment::Field(field.public_name.clone()),
                ],
                2,
            ))
        }
        _ => Err(FrontendError::InvalidMetadata(format!(
            "callback path segment {segment:?} does not match value at {full_path}"
        ))),
    }
}

fn build_bridge_input(
    components: &[IdentifiedComponent],
    types: &[IdentifiedType],
    operations: &[IdentifiedOperation],
    callbacks: Vec<CallbackUseSite>,
    streams: Vec<StreamUseSite>,
) -> BridgePlanInput {
    let full_capabilities = [
        Capability::Primitive,
        Capability::String,
        Capability::Bytes,
        Capability::BigInt,
        Capability::Optional,
        Capability::Sequence,
        Capability::Map,
        Capability::Set,
        Capability::Record,
        Capability::Enum,
        Capability::DeclaredError,
        Capability::ObjectLease,
        Capability::SyncCall,
        Capability::AsyncCall,
        Capability::Callback,
        Capability::RetainedCallback,
        Capability::AsyncCallback,
        Capability::FallibleCallback,
        Capability::CallbackReentrancy,
        Capability::CrossThreadAsyncCallback,
        Capability::InputStream,
        Capability::OutputStream,
    ];
    let targets = [
        EngineKind::Napi,
        EngineKind::WasmBindgen,
        EngineKind::OhosNapi,
    ]
    .into_iter()
    .map(|engine| EngineCapabilities::new(engine, full_capabilities))
    .collect();
    BridgePlanInput {
        components: components.to_vec(),
        types: types.to_vec(),
        operations: operations
            .iter()
            .cloned()
            .map(PlannedOperation::new)
            .collect(),
        callbacks,
        streams,
        targets,
    }
}

fn build_rust_bridge_plan(
    api: &JsApiIr,
    type_ids: &BTreeMap<TypeSourceKey, uniffi_js_abi::TypeId>,
    type_extras: &BTreeMap<TypeSourceKey, TypeExtra>,
    operation_extras: &BTreeMap<OperationSourceKey, OperationExtra>,
    callbacks: &[CallbackUseSite],
    streams: &[StreamUseSite],
    build_targets: &[PublicTarget],
) -> Result<RustBridgePlan, FrontendError> {
    let mut operations = Vec::with_capacity(api.operations.len());
    let mut synthetic_operations = Vec::new();
    let mut next_synthetic_id = api
        .operations
        .iter()
        .map(|operation| operation.id.index())
        .max()
        .map(|id| id.saturating_add(1))
        .unwrap_or(0);
    for operation in &api.operations {
        let receiver = operation.receiver.as_ref().map(|receiver| {
            let is_object = matches!(operation.source_key.owner(), OperationOwner::Object(_));
            RustReceiverBinding {
                rust_type: if is_object {
                    RustType::Scalar(uniffi_js_abi::ScalarType::U64)
                } else {
                    type_extras
                        .get(match operation.source_key.owner() {
                            OperationOwner::Value(key) => key,
                            OperationOwner::Object(key) => key,
                            _ => unreachable!("receiver only exists for value/object owners"),
                        })
                        .map(|extra| RustType::Path(extra.rust_path.clone()))
                        .unwrap_or(RustType::Unit)
                },
                carrier: if is_object {
                    RustCarrier::OpaqueHandle
                } else {
                    RustCarrier::LocalAdapter
                },
                ownership: operation_extras
                    .get(&operation.source_key)
                    .and_then(|extra| extra.receiver_ownership)
                    .unwrap_or(Ownership::Borrowed),
                conversion: if is_object {
                    ConversionRecipe::Object(receiver.object_type)
                } else {
                    match api
                        .types
                        .iter()
                        .find(|ty| ty.id == receiver.object_type)
                        .map(|ty| &ty.kind)
                    {
                        Some(JsTypeKind::Record { .. }) => {
                            ConversionRecipe::Record(receiver.object_type)
                        }
                        Some(JsTypeKind::Enum { .. }) => {
                            ConversionRecipe::Enum(receiver.object_type)
                        }
                        Some(JsTypeKind::Error { .. }) => {
                            ConversionRecipe::Error(receiver.object_type)
                        }
                        _ => ConversionRecipe::Identity,
                    }
                },
            }
        });
        let extra = operation_extras.get(&operation.source_key).ok_or_else(|| {
            FrontendError::Contract(format!(
                "missing Rust operation extra {}",
                operation.source_key
            ))
        })?;
        if extra.argument_rust_names.len() != operation.arguments.len()
            || extra.argument_ownership.len() != operation.arguments.len()
        {
            return Err(FrontendError::Contract(format!(
                "Rust argument metadata length mismatch for {}",
                operation.source_key
            )));
        }
        let callback_role = matches!(operation.source_key.owner(), OperationOwner::Callback(_))
            || callbacks
                .iter()
                .any(|contract| contract.operation_id == operation.id);
        let arguments = operation
            .arguments
            .iter()
            .enumerate()
            .map(|(index, argument)| {
                let (rust_type, carrier, conversion) =
                    rust_binding_for_type(&argument.ty, api, type_ids, type_extras, callback_role)?;
                Ok(RustArgumentBinding {
                    public_name: argument.public_name.clone(),
                    rust_name: extra.argument_rust_names[index].clone(),
                    rust_type,
                    carrier,
                    ownership: extra.argument_ownership[index],
                    conversion,
                })
            })
            .collect::<Result<Vec<_>, FrontendError>>()?;
        let return_value = operation
            .return_type
            .as_ref()
            .map(|ty| {
                let (rust_type, carrier, conversion) =
                    rust_binding_for_type(ty, api, type_ids, type_extras, callback_role)?;
                Ok(RustReturnBinding {
                    rust_type,
                    carrier,
                    ownership: Ownership::Owned,
                    conversion,
                })
            })
            .transpose()?;
        let throws = operation
            .throws
            .as_ref()
            .and_then(|key| type_ids.get(key).copied());
        let mut resource_hooks = Vec::new();
        if operation.receiver.is_some() {
            resource_hooks.push(RustResourceHook::AcquireObject);
        }
        let mut stream_resources = Vec::new();
        for stream in streams
            .iter()
            .filter(|stream| stream.operation_id == operation.id)
        {
            let hooks = match stream.contract.direction {
                StreamDirection::Input => vec![
                    RustResourceHook::StartInputStream,
                    RustResourceHook::PullInputStream,
                    RustResourceHook::CancelInputStream,
                    RustResourceHook::CloseInputStream,
                ],
                StreamDirection::Output => vec![
                    RustResourceHook::StartOutputStream,
                    RustResourceHook::PullOutputStream,
                    RustResourceHook::CancelOutputStream,
                    RustResourceHook::CloseOutputStream,
                ],
            };
            resource_hooks.extend(hooks.iter().copied());
            let slot_kinds = match stream.contract.direction {
                StreamDirection::Input => [
                    (
                        OperationKind::InputStreamPull,
                        RustResourceHook::PullInputStream,
                    ),
                    (
                        OperationKind::InputStreamCancel,
                        RustResourceHook::CancelInputStream,
                    ),
                ]
                .into_iter()
                .collect::<Vec<_>>(),
                StreamDirection::Output => [
                    (
                        OperationKind::OutputStreamStart,
                        RustResourceHook::StartOutputStream,
                    ),
                    (
                        OperationKind::OutputStreamNext,
                        RustResourceHook::PullOutputStream,
                    ),
                    (
                        OperationKind::OutputStreamCancel,
                        RustResourceHook::CancelOutputStream,
                    ),
                ]
                .into_iter()
                .collect::<Vec<_>>(),
            };
            let mut slot_operation_ids = BTreeMap::new();
            for (kind, hook) in slot_kinds {
                let operation_id = uniffi_js_abi::OperationId::new(next_synthetic_id);
                next_synthetic_id = next_synthetic_id.saturating_add(1);
                let slot_name = format!(
                    "{}__stream_{}_{}",
                    operation.source_key.name(),
                    stream.id.index(),
                    stream_operation_suffix(kind)
                );
                let source_key = OperationSourceKey::new(
                    operation.source_key.component().clone(),
                    operation.source_key.owner().clone(),
                    kind,
                    slot_name,
                )
                .map_err(|error| FrontendError::Contract(error.to_string()))?;
                slot_operation_ids.insert(kind, operation_id);
                synthetic_operations.push(RustOperationPlan {
                    operation_id,
                    source_key,
                    component_id: operation.component_id,
                    owner: operation.source_key.owner().clone(),
                    kind,
                    callback_method_id: None,
                    private_ffi_symbol: None,
                    call_target: RustCallTarget::StreamHook {
                        parent: operation.id,
                        hook,
                    },
                    receiver: None,
                    arguments: Vec::new(),
                    return_value: None,
                    throws: None,
                    resource_hooks: vec![hook],
                    stream_resources: Vec::new(),
                });
            }
            stream_resources.push(RustStreamResourceGroup {
                id: stream.id,
                path: stream.path.clone(),
                direction: stream.contract.direction,
                hooks,
                slot_operation_ids,
            });
        }
        resource_hooks.sort();
        resource_hooks.dedup();
        let call_target = match (operation.source_key.owner(), operation.kind) {
            (OperationOwner::Namespace, OperationKind::Function) => RustCallTarget::FreeFunction {
                module: extra.module_path.clone(),
                item: extra.item_name.clone(),
            },
            (OperationOwner::Object(_), OperationKind::Constructor)
            | (OperationOwner::Value(_), OperationKind::Constructor) => {
                RustCallTarget::Constructor {
                    object: extra.type_path.clone().ok_or_else(|| {
                        FrontendError::Contract("constructor missing Rust type path".to_owned())
                    })?,
                    object_kind: extra.object_kind.unwrap_or(RustObjectKind::Struct),
                    item: extra.item_name.clone(),
                }
            }
            (OperationOwner::Object(_), OperationKind::Method)
            | (OperationOwner::Value(_), OperationKind::Method) => RustCallTarget::Method {
                object: extra.type_path.clone().ok_or_else(|| {
                    FrontendError::Contract("method missing Rust type path".to_owned())
                })?,
                object_kind: extra.object_kind.unwrap_or(RustObjectKind::Struct),
                callback_method_id: operation.callback_method_id,
                item: extra.item_name.clone(),
            },
            (OperationOwner::Callback(owner), OperationKind::CallbackMethod) => {
                let callback_type = *type_ids
                    .get(owner)
                    .ok_or_else(|| FrontendError::UnknownTypeOwner(owner.to_string()))?;
                RustCallTarget::CallbackMethod {
                    callback: extra.type_path.clone().ok_or_else(|| {
                        FrontendError::Contract("callback method missing Rust type path".to_owned())
                    })?,
                    callback_type,
                    method_id: operation.callback_method_id.ok_or_else(|| {
                        FrontendError::Contract("callback method missing method ID".to_owned())
                    })?,
                    item: extra.item_name.clone(),
                }
            }
            _ => {
                return Err(FrontendError::Contract(format!(
                    "unsupported Rust operation target {}",
                    operation.source_key
                )))
            }
        };
        operations.push(RustOperationPlan {
            operation_id: operation.id,
            source_key: operation.source_key.clone(),
            component_id: operation.component_id,
            owner: operation.source_key.owner().clone(),
            kind: operation.kind,
            callback_method_id: operation.callback_method_id,
            private_ffi_symbol: extra.private_ffi_symbol.clone(),
            call_target,
            receiver,
            arguments,
            return_value,
            throws,
            resource_hooks,
            stream_resources,
        });
    }
    operations.extend(synthetic_operations);
    operations.sort_by_key(|operation| operation.operation_id);
    for (expected, operation) in operations.iter().enumerate() {
        if operation.operation_id.index() != expected as u32 {
            return Err(FrontendError::Contract(
                "Rust operation plan IDs must remain dense after stream slots are synthesized"
                    .to_owned(),
            ));
        }
    }
    let engines = build_targets
        .iter()
        .copied()
        .map(engine_for_target)
        .map(|engine| {
            (
                engine,
                EngineRustBridgePlan {
                    engine,
                    operations: operations.clone(),
                },
            )
        })
        .collect();
    Ok(RustBridgePlan { engines })
}

fn rust_binding_for_type(
    ty: &ValueType,
    api: &JsApiIr,
    type_ids: &BTreeMap<TypeSourceKey, uniffi_js_abi::TypeId>,
    type_extras: &BTreeMap<TypeSourceKey, TypeExtra>,
    callback_role: bool,
) -> Result<(RustType, RustCarrier, ConversionRecipe), FrontendError> {
    Ok(match ty {
        ValueType::Scalar(scalar) => {
            let (carrier, conversion) = match scalar {
                uniffi_js_abi::ScalarType::I64 | uniffi_js_abi::ScalarType::U64 => {
                    (RustCarrier::BigInt, ConversionRecipe::BigInt)
                }
                uniffi_js_abi::ScalarType::Bytes => (RustCarrier::Bytes, ConversionRecipe::Bytes),
                _ => (RustCarrier::Primitive, ConversionRecipe::Identity),
            };
            (RustType::Scalar(*scalar), carrier, conversion)
        }
        ValueType::Timestamp => (
            RustType::Timestamp,
            RustCarrier::Timestamp,
            ConversionRecipe::Timestamp,
        ),
        ValueType::Duration => (
            RustType::Duration,
            RustCarrier::Duration,
            ConversionRecipe::Duration,
        ),
        ValueType::Named(key) => {
            let id = *type_ids
                .get(key)
                .ok_or_else(|| FrontendError::UnknownTypeOwner(key.to_string()))?;
            let kind = api
                .types
                .iter()
                .find(|ty| ty.id == id)
                .map(|ty| &ty.kind)
                .ok_or_else(|| FrontendError::UnknownTypeOwner(key.to_string()))?;
            let path = type_extras
                .get(key)
                .ok_or_else(|| FrontendError::UnknownTypeOwner(key.to_string()))?
                .rust_path
                .clone();
            let (carrier, conversion) = match kind {
                JsTypeKind::Record { .. } => {
                    (RustCarrier::LocalAdapter, ConversionRecipe::Record(id))
                }
                JsTypeKind::Enum { .. } => (RustCarrier::LocalAdapter, ConversionRecipe::Enum(id)),
                JsTypeKind::Error { .. } => {
                    (RustCarrier::LocalAdapter, ConversionRecipe::Error(id))
                }
                JsTypeKind::Custom { builtin, .. } => {
                    let (_, _, inner) =
                        rust_binding_for_type(builtin, api, type_ids, type_extras, callback_role)?;
                    (
                        RustCarrier::LocalAdapter,
                        ConversionRecipe::Custom(id, Box::new(inner)),
                    )
                }
                JsTypeKind::Object {
                    kind: ObjectKind::TraitBoth | ObjectKind::TraitForeignOnly,
                } if callback_role => (RustCarrier::CallbackProxy, ConversionRecipe::Callback(id)),
                JsTypeKind::Object { .. } => {
                    (RustCarrier::OpaqueHandle, ConversionRecipe::Object(id))
                }
                JsTypeKind::Callback => {
                    (RustCarrier::CallbackProxy, ConversionRecipe::Callback(id))
                }
            };
            (RustType::Path(path), carrier, conversion)
        }
        ValueType::Optional(inner) => {
            let (rust, carrier, conversion) =
                rust_binding_for_type(inner, api, type_ids, type_extras, callback_role)?;
            (
                RustType::Option(Box::new(rust)),
                carrier,
                ConversionRecipe::Optional(Box::new(conversion)),
            )
        }
        ValueType::Sequence(inner) => {
            let (rust, _, conversion) =
                rust_binding_for_type(inner, api, type_ids, type_extras, callback_role)?;
            (
                RustType::Sequence(Box::new(rust)),
                RustCarrier::LocalAdapter,
                ConversionRecipe::Sequence(Box::new(conversion)),
            )
        }
        ValueType::Map(key, value) => {
            let (key_rust, _, key_conversion) =
                rust_binding_for_type(key, api, type_ids, type_extras, callback_role)?;
            let (value_rust, _, value_conversion) =
                rust_binding_for_type(value, api, type_ids, type_extras, callback_role)?;
            (
                RustType::Map(Box::new(key_rust), Box::new(value_rust)),
                RustCarrier::LocalAdapter,
                ConversionRecipe::Map(Box::new(key_conversion), Box::new(value_conversion)),
            )
        }
        ValueType::Set(inner) => {
            let (rust, _, conversion) =
                rust_binding_for_type(inner, api, type_ids, type_extras, callback_role)?;
            (
                RustType::Set(Box::new(rust)),
                RustCarrier::LocalAdapter,
                ConversionRecipe::Set(Box::new(conversion)),
            )
        }
        ValueType::InputStream(inner) => {
            let (rust, _, conversion) =
                rust_binding_for_type(inner, api, type_ids, type_extras, callback_role)?;
            (
                RustType::InputStream(Box::new(rust)),
                RustCarrier::InputStream,
                ConversionRecipe::InputStream(Box::new(conversion)),
            )
        }
        ValueType::OutputStream(inner) => {
            let (rust, _, conversion) =
                rust_binding_for_type(inner, api, type_ids, type_extras, callback_role)?;
            (
                RustType::Stream(Box::new(rust)),
                RustCarrier::OutputStream,
                ConversionRecipe::OutputStream(Box::new(conversion)),
            )
        }
    })
}

fn resolve_config(
    components: &[&Component<JsConfig>],
    crate_owners: &BTreeMap<String, ComponentKey>,
    _type_ids: &BTreeMap<TypeSourceKey, uniffi_js_abi::TypeId>,
) -> ResolvedJsConfig {
    let mut custom_types = BTreeMap::new();
    for component in components {
        let Ok(component_key) =
            owner_key_for_module(&component.ci, component.ci.crate_name(), crate_owners)
        else {
            continue;
        };
        for custom in component.ci.custom_type_definitions() {
            let key = TypeSourceKey::new(component_key.clone(), custom.name.clone()).unwrap();
            custom_types.insert(
                key,
                custom_type_config(component.config.custom_type(&custom.name), &custom.name),
            );
        }
    }
    ResolvedJsConfig { custom_types }
}

fn custom_type_config(config: Option<&CustomTypeConfig>, fallback: &str) -> JsCustomTypeConfig {
    JsCustomTypeConfig {
        public_type_name: config
            .map(|config| config.public_type(fallback).to_owned())
            .unwrap_or_else(|| fallback.to_owned()),
        imports: config
            .map(|config| config.imports.clone())
            .unwrap_or_default(),
        into_custom: config
            .map(|config| config.into_custom.clone())
            .unwrap_or_default(),
        from_custom: config
            .map(|config| config.from_custom.clone())
            .unwrap_or_default(),
    }
}

fn convert_variant(
    variant: &uniffi_bindgen::interface::Variant,
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<SourceVariant, FrontendError> {
    Ok(SourceVariant {
        public_name: variant.name().to_upper_camel_case(),
        rust_name: variant.rust_name().to_owned(),
        fields: variant
            .fields()
            .iter()
            .map(|field| {
                Ok(SourceField {
                    public_name: field.name().to_lower_camel_case(),
                    rust_name: field.rust_name().to_owned(),
                    ty: convert_type(&field.as_type(), crate_owners)?,
                    default: field.default_value().map(convert_default),
                })
            })
            .collect::<Result<Vec<_>, FrontendError>>()?,
    })
}

fn convert_bridge_variant(variant: &SourceVariant) -> Result<EnumVariant, FrontendError> {
    Ok(EnumVariant::new(
        variant.public_name.clone(),
        variant
            .fields
            .iter()
            .map(|field| {
                FieldDefinition::new(field.public_name.clone(), field.ty.clone())
                    .map_err(|error| FrontendError::Contract(error.to_string()))
            })
            .collect::<Result<Vec<_>, _>>()?,
    )
    .map_err(|error| FrontendError::Contract(error.to_string()))?)
}

fn convert_default(value: &DefaultValue) -> JsDefaultValue {
    use uniffi_bindgen::interface::Literal;
    match value {
        DefaultValue::Default => JsDefaultValue::Unspecified,
        DefaultValue::Literal(literal) => match literal {
            Literal::Boolean(value) => JsDefaultValue::Boolean(*value),
            Literal::String(value) => JsDefaultValue::String(value.clone()),
            Literal::UInt(value, _, _) => JsDefaultValue::Integer {
                value: *value as i128,
                unsigned: true,
            },
            Literal::Int(value, _, _) => JsDefaultValue::Integer {
                value: *value as i128,
                unsigned: false,
            },
            Literal::Float(value, _) => JsDefaultValue::Float(value.clone()),
            Literal::Enum(value, _) => JsDefaultValue::Enum(value.clone()),
            Literal::EmptySequence => JsDefaultValue::EmptySequence,
            Literal::EmptyMap => JsDefaultValue::EmptyMap,
            Literal::EmptySet => JsDefaultValue::EmptySet,
            Literal::None => JsDefaultValue::None,
            Literal::Some { inner } => JsDefaultValue::Some(Box::new(convert_default(inner))),
        },
    }
}

fn convert_type(
    ty: &Type,
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<ValueType, FrontendError> {
    Ok(match ty {
        Type::UInt8 => ValueType::Scalar(uniffi_js_abi::ScalarType::U8),
        Type::Int8 => ValueType::Scalar(uniffi_js_abi::ScalarType::I8),
        Type::UInt16 => ValueType::Scalar(uniffi_js_abi::ScalarType::U16),
        Type::Int16 => ValueType::Scalar(uniffi_js_abi::ScalarType::I16),
        Type::UInt32 => ValueType::Scalar(uniffi_js_abi::ScalarType::U32),
        Type::Int32 => ValueType::Scalar(uniffi_js_abi::ScalarType::I32),
        Type::UInt64 => ValueType::Scalar(uniffi_js_abi::ScalarType::U64),
        Type::Int64 => ValueType::Scalar(uniffi_js_abi::ScalarType::I64),
        Type::Float32 => ValueType::Scalar(uniffi_js_abi::ScalarType::F32),
        Type::Float64 => ValueType::Scalar(uniffi_js_abi::ScalarType::F64),
        Type::Boolean => ValueType::Scalar(uniffi_js_abi::ScalarType::Bool),
        Type::String => ValueType::Scalar(uniffi_js_abi::ScalarType::String),
        Type::Bytes => ValueType::Scalar(uniffi_js_abi::ScalarType::Bytes),
        Type::Timestamp => ValueType::Timestamp,
        Type::Duration => ValueType::Duration,
        Type::Object { .. }
        | Type::Record { .. }
        | Type::Enum { .. }
        | Type::CallbackInterface { .. }
        | Type::Custom { .. } => ValueType::Named(source_key_for_type(ty, crate_owners)?),
        Type::Box { inner_type } => convert_type(inner_type, crate_owners)?,
        Type::Optional { inner_type } => {
            ValueType::optional(convert_type(inner_type, crate_owners)?)
        }
        Type::Sequence { inner_type } => {
            ValueType::sequence(convert_type(inner_type, crate_owners)?)
        }
        Type::Map {
            key_type,
            value_type,
        } => ValueType::map(
            convert_type(key_type, crate_owners)?,
            convert_type(value_type, crate_owners)?,
        ),
        Type::Set { inner_type } => ValueType::set(convert_type(inner_type, crate_owners)?),
        Type::Stream { item_type, .. } => {
            ValueType::output_stream(convert_type(item_type, crate_owners)?)
        }
        Type::InputStream { item_type, .. } => {
            ValueType::input_stream(convert_type(item_type, crate_owners)?)
        }
    })
}

fn source_key_for_type(
    ty: &Type,
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<TypeSourceKey, FrontendError> {
    let name = ty
        .name()
        .ok_or_else(|| FrontendError::UnsupportedType(format!("{ty:?}")))?;
    let module_path = ty
        .module_path()
        .ok_or_else(|| FrontendError::UnsupportedType(format!("{ty:?}")))?;
    let component = crate_owners
        .get(&crate_root(module_path))
        .ok_or_else(|| FrontendError::UnknownTypeOwner(module_path.to_owned()))?;
    TypeSourceKey::new(component.clone(), name.to_owned())
        .map_err(|error| FrontendError::Contract(error.to_string()))
}

fn owner_key_for_module(
    _ci: &ComponentInterface,
    module_path: &str,
    crate_owners: &BTreeMap<String, ComponentKey>,
) -> Result<ComponentKey, FrontendError> {
    crate_owners
        .get(&crate_root(module_path))
        .cloned()
        .ok_or_else(|| FrontendError::UnknownTypeOwner(module_path.to_owned()))
}

fn crate_root(module_path: &str) -> String {
    module_path
        .split("::")
        .next()
        .unwrap_or(module_path)
        .replace('-', "_")
}

fn engine_for_target(target: PublicTarget) -> EngineKind {
    match target {
        PublicTarget::NodeNapi => EngineKind::Napi,
        PublicTarget::BrowserWasm => EngineKind::WasmBindgen,
        PublicTarget::OhosNapi => EngineKind::OhosNapi,
    }
}

fn stream_operation_suffix(kind: OperationKind) -> &'static str {
    match kind {
        OperationKind::InputStreamPull => "input_pull",
        OperationKind::InputStreamCancel => "input_cancel",
        OperationKind::OutputStreamStart => "output_start",
        OperationKind::OutputStreamNext => "output_next",
        OperationKind::OutputStreamCancel => "output_cancel",
        _ => "stream",
    }
}

fn enum_module_path(enum_: &Enum) -> String {
    enum_.as_type().module_path().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(crate_name: &str, namespace: &str, body: &str) -> Component<JsConfig> {
        Component {
            ci: ComponentInterface::from_webidl(
                &format!("namespace {namespace} {{ {body} }};"),
                crate_name,
            )
            .expect("test interface should parse"),
            config: JsConfig::default(),
        }
    }

    fn component_idl(crate_name: &str, idl: &str) -> Component<JsConfig> {
        Component {
            ci: ComponentInterface::from_webidl(idl, crate_name)
                .expect("test interface should parse"),
            config: JsConfig::default(),
        }
    }

    #[test]
    fn normalizing_reversed_component_discovery_is_deterministic() {
        let forward_components = [
            component("first_crate", "first", "string hello_world();"),
            component("second_crate", "second", "i64 count();"),
        ];
        let reverse_components = [
            component("second_crate", "second", "i64 count();"),
            component("first_crate", "first", "string hello_world();"),
        ];
        let forward = normalize(BindingInput::new(&forward_components)).unwrap();
        let reverse = normalize(BindingInput::new(&reverse_components)).unwrap();
        assert_eq!(forward.api, reverse.api);
        assert_eq!(forward.bridge, reverse.bridge);
        assert_eq!(forward.rust, reverse.rust);
        assert_eq!(forward.build_targets, UNIFIED_TARGET_UNIVERSE.to_vec());
    }

    #[test]
    fn crate_root_collision_is_a_stable_normalization_error() {
        let first = component("foo-bar", "alpha", "string alpha();");
        let second = component("foo_bar", "zeta", "string zeta();");
        let forward = normalize(BindingInput::new(&[first, second])).unwrap_err();

        let first = component("foo-bar", "alpha", "string alpha();");
        let second = component("foo_bar", "zeta", "string zeta();");
        let reverse = normalize(BindingInput::new(&[second, first])).unwrap_err();

        assert_eq!(forward, reverse);
        let message = forward.to_string();
        assert!(message.contains("foo_bar"));
        assert!(message.contains("alpha"));
        assert!(message.contains("zeta"));
    }

    #[test]
    fn rust_targets_keep_crate_paths_source_items_and_private_ffi_separate() {
        let mut group = uniffi_meta::MetadataGroup {
            namespace: uniffi_meta::NamespaceMetadata {
                crate_name: "core-crate".to_owned(),
                name: "public_namespace".to_owned(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        group.add_item(
            uniffi_meta::RecordMetadata {
                module_path: "core-crate::bindings".to_owned(),
                name: "PublicRecord".to_owned(),
                orig_name: Some("RustRecord".to_owned()),
                rust_path: Some("domain::nested::RustRecord".to_owned()),
                remote: false,
                fields: vec![],
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            uniffi_meta::FnMetadata {
                module_path: "core-crate::api".to_owned(),
                name: "fetch".to_owned(),
                orig_name: Some("rust_fetch".to_owned()),
                is_async: false,
                inputs: vec![uniffi_meta::FnParamMetadata::simple(
                    "record",
                    uniffi_meta::Type::Record {
                        module_path: "core-crate::bindings".to_owned(),
                        name: "PublicRecord".to_owned(),
                    },
                )],
                return_type: None,
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            uniffi_meta::ObjectMetadata {
                module_path: "core-crate::bindings".to_owned(),
                name: "PublicObject".to_owned(),
                orig_name: Some("RustObject".to_owned()),
                remote: false,
                imp: uniffi_meta::ObjectImpl::Struct,
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            uniffi_meta::MethodMetadata {
                module_path: "core-crate::bindings".to_owned(),
                self_name: "PublicObject".to_owned(),
                name: "public_method".to_owned(),
                orig_name: Some("rust_method".to_owned()),
                is_async: false,
                inputs: vec![],
                return_type: None,
                throws: None,
                takes_self_by_arc: false,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        let component = Component {
            ci: ComponentInterface::from_metadata(group).unwrap(),
            config: JsConfig::default(),
        };
        let package = normalize(BindingInput::new(&[component])).unwrap();
        let fetch = package
            .rust
            .engines
            .get(&EngineKind::Napi)
            .unwrap()
            .operations
            .iter()
            .find(|operation| operation.kind == OperationKind::Function)
            .unwrap();
        let RustCallTarget::FreeFunction { module, item } = &fetch.call_target else {
            panic!("expected free-function call target");
        };
        assert_eq!(module.segments, vec!["core_crate", "api"]);
        assert_eq!(item, "rust_fetch");
        let private_ffi_symbol = fetch.private_ffi_symbol.as_deref().unwrap();
        assert!(private_ffi_symbol.contains("fetch"));
        assert_ne!(item, private_ffi_symbol);

        let record = package
            .rust
            .engines
            .get(&EngineKind::Napi)
            .unwrap()
            .operations
            .iter()
            .find(|operation| operation.kind == OperationKind::Function)
            .unwrap();
        assert_eq!(
            record.arguments[0].rust_type,
            RustType::Path(RustPath::new([
                "core_crate",
                "domain",
                "nested",
                "RustRecord",
            ]))
        );
        let method = package
            .rust
            .engines
            .get(&EngineKind::Napi)
            .unwrap()
            .operations
            .iter()
            .find(|operation| operation.source_key.name() == "public_method")
            .unwrap();
        let RustCallTarget::Method { object, item, .. } = &method.call_target else {
            panic!("expected object method call target");
        };
        assert_eq!(
            object.segments,
            vec!["core_crate", "bindings", "r#RustObject"]
        );
        assert_eq!(item, "rust_method");
        assert_eq!(
            package.api.components[0].public_namespace,
            "public_namespace"
        );
    }

    #[test]
    fn empty_and_duplicate_targets_are_rejected_without_changing_universe() {
        let empty_component = component("targets_crate", "targets", "string ping();");
        let empty =
            normalize(BindingInput::new(&[empty_component]).with_build_targets([])).unwrap_err();
        assert_eq!(empty, FrontendError::NoBuildTargets);

        let duplicate_component = component("targets_crate", "targets", "string ping();");
        let duplicate = normalize(
            BindingInput::new(&[duplicate_component])
                .with_build_targets([PublicTarget::NodeNapi, PublicTarget::NodeNapi]),
        )
        .unwrap_err();
        assert_eq!(
            duplicate,
            FrontendError::DuplicateBuildTarget(PublicTarget::NodeNapi)
        );

        let selected_component = component("targets_crate", "targets", "string ping();");
        let package = normalize(
            BindingInput::new(&[selected_component])
                .with_build_targets([PublicTarget::BrowserWasm]),
        )
        .unwrap();
        assert_eq!(
            package.api.target_universe,
            UNIFIED_TARGET_UNIVERSE.to_vec()
        );
        assert_eq!(package.rust.engines.len(), 1);
    }

    #[test]
    fn callback_contracts_are_not_invented_by_the_frontend() {
        let callback = component_idl(
            "callback_crate",
            "callback interface Listener { void on_event(string value); }; namespace callback { void run(Listener listener); };",
        );
        let error = normalize(BindingInput::new(&[callback])).unwrap_err();
        assert!(error.to_string().contains("callback"));
    }

    #[test]
    fn callback_metadata_uses_source_paths_then_exposes_public_paths() {
        let callback = component_idl(
            "callback_nested_crate",
            r#"
                callback interface Listener { void on_event(string value); };
                dictionary Envelope { Listener listener_value; };
                namespace callback_nested {
                    [CallbackContract="argument[0].field[listener_value],retained,calling_thread,allowed"]
                    void run(Envelope envelope);
                };
            "#,
        );
        let package = normalize(BindingInput::new(&[callback])).unwrap();
        assert_eq!(package.bridge.callbacks().len(), 1);
        assert_eq!(
            package.bridge.callbacks()[0].path.to_string(),
            "argument[0].field[listenerValue]"
        );
    }

    #[test]
    fn callback_interface_method_ids_follow_declaration_order_not_name_order() {
        let callback = component_idl(
            "ordered_callback_crate",
            r#"
                callback interface Ordered {
                    void zeta();
                    void alpha();
                };
                namespace ordered {
                    void consume([CallbackContract="retained,calling_thread,forbidden"] Ordered value);
                };
            "#,
        );
        let package = normalize(BindingInput::new(&[callback])).unwrap();
        let ordered = package
            .api
            .types
            .iter()
            .find(|ty| ty.public_name == "Ordered")
            .unwrap();
        let method_id = |name: &str| {
            package
                .api
                .operations
                .iter()
                .find(|operation| {
                    operation.source_key.owner()
                        == &OperationOwner::Callback(ordered.source_key.clone())
                        && operation.public_name == name
                })
                .and_then(|operation| operation.callback_method_id)
        };
        assert_eq!(method_id("zeta"), Some(0));
        assert_eq!(method_id("alpha"), Some(1));
    }

    #[test]
    fn callback_trait_metadata_resolves_callback_owner_and_nested_crate_root() {
        let callback = component_idl(
            "nested::trait_callback_crate",
            r#"
                callback interface Payload { void ping(); };
                callback interface Listener {
                    [CallbackContract="argument[0],retained,calling_thread,allowed"]
                    void on_event(Payload payload);
                };
                namespace trait_callback {
                    void register([CallbackContract="retained,calling_thread,forbidden"] Listener listener);
                };
            "#,
        );
        let package = normalize(BindingInput::new(&[callback])).unwrap();
        let callback = package.bridge.callbacks();
        assert_eq!(callback.len(), 2);
        let trait_contract = callback
            .iter()
            .find(|contract| {
                package
                    .api
                    .operations
                    .iter()
                    .find(|operation| operation.id == contract.operation_id)
                    .is_some_and(|operation| operation.kind == OperationKind::CallbackMethod)
            })
            .expect("callback interface trait method contract should be retained");
        assert_eq!(trait_contract.path.to_string(), "argument[0]");
        let operation = package
            .api
            .operations
            .iter()
            .find(|operation| operation.id == trait_contract.operation_id)
            .unwrap();
        assert!(matches!(
            operation.source_key.owner(),
            OperationOwner::Callback(_)
        ));
    }

    #[test]
    fn callback_metadata_does_not_accept_public_field_aliases() {
        let callback = component_idl(
            "callback_alias_crate",
            r#"
                callback interface Listener { void on_event(string value); };
                dictionary Envelope { Listener listener_value; };
                namespace callback_alias {
                    [CallbackContract="argument[0].field[listenerValue],retained,calling_thread,allowed"]
                    void run(Envelope envelope);
                };
            "#,
        );
        let error = normalize(BindingInput::new(&[callback])).unwrap_err();
        assert!(error.to_string().contains("unknown field"));
    }

    #[test]
    fn foreign_trait_is_callback_capable_and_methods_have_dense_vtable_ids() {
        let trait_component = component_idl(
            "trait_callback_crate",
            r#"
                [Trait, WithForeign]
                interface Listener { void zeta(string value); void alpha(); };
                namespace traits {
                    [CallbackContract="argument[0],retained,calling_thread,forbidden"]
                    void consume(Listener listener);
                };
            "#,
        );
        let package = normalize(BindingInput::new(&[trait_component])).unwrap();
        let listener = package
            .api
            .types
            .iter()
            .find(|ty| ty.public_name == "Listener")
            .expect("foreign trait is represented in the public type graph");
        assert!(matches!(
            listener.kind,
            JsTypeKind::Object {
                kind: ObjectKind::TraitBoth
            }
        ));
        assert_eq!(package.bridge.callbacks().len(), 1);
        let methods = package
            .api
            .operations
            .iter()
            .filter(|operation| {
                operation.source_key.owner() == &OperationOwner::Object(listener.source_key.clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            methods
                .iter()
                .map(|operation| operation.callback_method_id)
                .collect::<Vec<_>>(),
            vec![None, None]
        );
        let callback_methods = package
            .api
            .operations
            .iter()
            .filter(|operation| {
                operation.source_key.owner()
                    == &OperationOwner::Callback(listener.source_key.clone())
            })
            .collect::<Vec<_>>();
        let callback_method_id = |name: &str| {
            callback_methods
                .iter()
                .find(|operation| operation.public_name == name)
                .and_then(|operation| operation.callback_method_id)
        };
        // `ComponentInterface::vtable_methods()` is the source of truth for
        // the generated callback table; operation IDs are independently
        // sorted by source key.  The reversed names make that distinction
        // observable here.
        assert_eq!(callback_method_id("alpha"), Some(0));
        assert_eq!(callback_method_id("zeta"), Some(1));
        let object_rust_methods = package.rust.engines[&EngineKind::Napi]
            .operations
            .iter()
            .filter(|operation| {
                operation.source_key.owner() == &OperationOwner::Object(listener.source_key.clone())
            })
            .collect::<Vec<_>>();
        assert!(object_rust_methods.iter().all(|operation| matches!(
            operation.call_target,
            RustCallTarget::Method {
                object_kind: RustObjectKind::TraitBoth,
                callback_method_id: None,
                ..
            }
        )));
        let rust_methods = package.rust.engines[&EngineKind::Napi]
            .operations
            .iter()
            .filter(|operation| {
                operation.source_key.owner()
                    == &OperationOwner::Callback(listener.source_key.clone())
            })
            .collect::<Vec<_>>();
        assert!(rust_methods.iter().all(|operation| matches!(
            operation.call_target,
            RustCallTarget::CallbackMethod {
                callback_type: _,
                method_id: _,
                ..
            }
        )));
        let consume = package
            .rust
            .engines
            .get(&EngineKind::Napi)
            .unwrap()
            .operations
            .iter()
            .find(|operation| operation.source_key.name() == "consume")
            .expect("trait-consuming function");
        assert!(matches!(
            consume.arguments[0].carrier,
            RustCarrier::CallbackProxy
        ));
        assert!(matches!(
            consume.arguments[0].conversion,
            ConversionRecipe::Callback(_)
        ));
    }

    #[test]
    fn operation_defaults_and_arc_receiver_ownership_are_preserved() {
        let component = component_idl(
            "defaults_receiver_crate",
            r#"
                interface Counter {
                    constructor();
                    [Self=ByArc]
                    boolean add(optional Counter? other = null);
                };
                namespace defaults {
                    void greet(Counter counter);
                };
            "#,
        );
        let package = normalize(BindingInput::new(&[component])).unwrap();
        let add = package
            .api
            .operations
            .iter()
            .find(|operation| operation.public_name == "add")
            .expect("object method");
        assert_eq!(
            package.rust.engines[&EngineKind::Napi]
                .operations
                .iter()
                .find(|operation| operation.operation_id == add.id)
                .and_then(|operation| operation.receiver.as_ref())
                .map(|receiver| receiver.ownership),
            Some(Ownership::Owned)
        );
        assert!(matches!(
            add.arguments[0].default,
            Some(JsDefaultValue::None)
        ));
        let rust_add = package.rust.engines[&EngineKind::Napi]
            .operations
            .iter()
            .find(|operation| operation.operation_id == add.id)
            .expect("Rust bridge method");
        assert_eq!(
            rust_add.arguments[0].rust_name, "other",
            "Rust bridge keeps source argument spelling separate from public camelCase"
        );
    }

    #[test]
    fn both_trait_method_callback_contract_is_projected_to_both_directions() {
        let trait_component = component_idl(
            "trait_contract_crate",
            r#"
                callback interface Payload { void ping(); };
                [Trait, WithForeign]
                interface Listener {
                    [CallbackContract="argument[0],retained,calling_thread,allowed"]
                    void zeta(Payload value);
                    void alpha();
                };
                namespace traits {
                    void consume([CallbackContract="retained,calling_thread,forbidden"] Listener listener);
                };
            "#,
        );
        let package = normalize(BindingInput::new(&[trait_component])).unwrap();
        let zeta_operations = package
            .api
            .operations
            .iter()
            .filter(|operation| {
                operation.public_name == "zeta"
                    && matches!(
                        operation.source_key.owner(),
                        OperationOwner::Object(_) | OperationOwner::Callback(_)
                    )
            })
            .collect::<Vec<_>>();
        assert_eq!(zeta_operations.len(), 2);
        for operation in zeta_operations {
            let contracts = package
                .bridge
                .callbacks()
                .iter()
                .filter(|contract| contract.operation_id == operation.id)
                .collect::<Vec<_>>();
            assert_eq!(contracts.len(), 1);
            assert_eq!(contracts[0].path.to_string(), "argument[0]");
        }
        let napi_operations = package.rust.engines[&EngineKind::Napi]
            .operations
            .iter()
            .filter(|operation| operation.source_key.name() == "zeta")
            .collect::<Vec<_>>();
        assert_eq!(napi_operations.len(), 2);
        let object_operation = napi_operations
            .iter()
            .find(|operation| matches!(operation.owner, OperationOwner::Object(_)))
            .unwrap();
        assert!(matches!(
            object_operation.arguments[0].carrier,
            RustCarrier::CallbackProxy
        ));
        let callback_operation = napi_operations
            .iter()
            .find(|operation| matches!(operation.owner, OperationOwner::Callback(_)))
            .unwrap();
        assert!(matches!(
            callback_operation.arguments[0].carrier,
            RustCarrier::CallbackProxy
        ));
    }

    #[test]
    fn output_streams_get_dense_canonical_operation_slots() {
        let mut group = uniffi_meta::MetadataGroup {
            namespace: uniffi_meta::NamespaceMetadata {
                crate_name: "stream_slots_crate".to_owned(),
                name: "stream_slots".to_owned(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        group.add_item(
            uniffi_meta::FnMetadata {
                module_path: "stream_slots_crate".to_owned(),
                name: "events".to_owned(),
                orig_name: None,
                is_async: false,
                inputs: vec![],
                return_type: Some(uniffi_meta::Type::Stream {
                    item_type: Box::new(uniffi_meta::Type::UInt32),
                    error_type: Box::new(uniffi_meta::Type::String),
                    is_send: true,
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
        let package = normalize(BindingInput::new(&[component])).unwrap();
        assert_eq!(package.api.operations.len(), 1);
        assert_eq!(package.bridge.streams().len(), 1);
        let rust_operations = &package.rust.engines[&EngineKind::WasmBindgen].operations;
        assert_eq!(rust_operations.len(), 4);
        assert_eq!(
            rust_operations
                .iter()
                .map(|operation| operation.kind)
                .collect::<Vec<_>>(),
            vec![
                OperationKind::Function,
                OperationKind::OutputStreamStart,
                OperationKind::OutputStreamNext,
                OperationKind::OutputStreamCancel,
            ]
        );
        let group = &rust_operations[0].stream_resources[0];
        assert_eq!(group.slot_operation_ids.len(), 3);
        assert_eq!(
            group
                .slot_operation_ids
                .values()
                .map(|id| id.index())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            package.engines[0]
                .operation_ids
                .iter()
                .map(|id| id.index())
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn named_map_nested_streams_have_complete_resource_groups() {
        let stream_item = |ty| uniffi_meta::Type::InputStream {
            item_type: Box::new(ty),
            error_type: Box::new(uniffi_meta::Type::String),
            is_send: true,
        };
        let mut group = uniffi_meta::MetadataGroup {
            namespace: uniffi_meta::NamespaceMetadata {
                crate_name: "nested_streams_crate".to_owned(),
                name: "nested_streams".to_owned(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        group.add_item(
            uniffi_meta::RecordMetadata {
                module_path: "nested_streams_crate".to_owned(),
                name: "Envelope".to_owned(),
                orig_name: None,
                rust_path: None,
                remote: false,
                fields: vec![uniffi_meta::FieldMetadata {
                    name: "streams".to_owned(),
                    orig_name: None,
                    ty: uniffi_meta::Type::Map {
                        key_type: Box::new(stream_item(uniffi_meta::Type::UInt32)),
                        value_type: Box::new(uniffi_meta::Type::Stream {
                            item_type: Box::new(uniffi_meta::Type::String),
                            error_type: Box::new(uniffi_meta::Type::String),
                            is_send: true,
                        }),
                    },
                    default: None,
                    docstring: None,
                }],
                docstring: None,
            }
            .into(),
        );
        group.add_item(
            uniffi_meta::FnMetadata {
                module_path: "nested_streams_crate".to_owned(),
                name: "consume".to_owned(),
                orig_name: None,
                is_async: false,
                inputs: vec![uniffi_meta::FnParamMetadata::simple(
                    "envelope",
                    uniffi_meta::Type::Record {
                        module_path: "nested_streams_crate".to_owned(),
                        name: "Envelope".to_owned(),
                    },
                )],
                return_type: None,
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
        let package = normalize(BindingInput::new(&[component])).unwrap();
        assert_eq!(package.bridge.streams().len(), 2);
        let consume = package
            .rust
            .engines
            .get(&EngineKind::Napi)
            .unwrap()
            .operations
            .iter()
            .find(|operation| operation.kind == OperationKind::Function)
            .unwrap();
        assert_eq!(consume.stream_resources.len(), 2);
        for resource in &consume.stream_resources {
            assert!(
                resource.hooks.contains(&RustResourceHook::StartInputStream)
                    || resource
                        .hooks
                        .contains(&RustResourceHook::StartOutputStream)
            );
            assert!(
                resource.hooks.contains(&RustResourceHook::PullInputStream)
                    || resource.hooks.contains(&RustResourceHook::PullOutputStream)
            );
            assert!(
                resource
                    .hooks
                    .contains(&RustResourceHook::CancelInputStream)
                    || resource
                        .hooks
                        .contains(&RustResourceHook::CancelOutputStream)
            );
            assert!(
                resource.hooks.contains(&RustResourceHook::CloseInputStream)
                    || resource
                        .hooks
                        .contains(&RustResourceHook::CloseOutputStream)
            );
            assert!(!resource.slot_operation_ids.is_empty());
        }
        let ids = consume
            .stream_resources
            .iter()
            .flat_map(|resource| resource.slot_operation_ids.values())
            .map(|id| id.index())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }
}
