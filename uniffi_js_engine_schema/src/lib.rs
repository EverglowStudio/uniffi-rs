/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Pure in-memory planning contract shared by JavaScript engine adapters.
//!
//! The types in this crate are compiler IR, not an artifact format.  This
//! crate has no serializer, filesystem access, schema version, digest, or
//! manifest identity.  A [`BridgePlan`] is constructed only after all target
//! capabilities, names, symbols, callback contracts, and references validate.

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;

use uniffi_js_abi::{
    AsyncKind, ComponentId, ComponentKey, IdentifiedComponent, IdentifiedOperation, IdentifiedType,
    NamedTypeKind, OperationId, OperationKind, OperationOwner, ScalarType, TypeId, TypeSourceKey,
    ValueType,
};

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum EngineKind {
    Napi,
    WasmBindgen,
    OhosNapi,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Capability {
    Primitive,
    String,
    Bytes,
    BigInt,
    Optional,
    Sequence,
    Map,
    Set,
    Record,
    Enum,
    DeclaredError,
    ObjectLease,
    SyncCall,
    AsyncCall,
    Callback,
    RetainedCallback,
    AsyncCallback,
    FallibleCallback,
    CallbackReentrancy,
    CrossThreadAsyncCallback,
    InputStream,
    OutputStream,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    pub fn new(capabilities: impl IntoIterator<Item = Capability>) -> Self {
        Self(capabilities.into_iter().collect())
    }

    pub fn insert(&mut self, capability: Capability) -> bool {
        self.0.insert(capability)
    }

    pub fn contains(&self, capability: Capability) -> bool {
        self.0.contains(&capability)
    }

    pub fn iter(&self) -> impl Iterator<Item = Capability> + '_ {
        self.0.iter().copied()
    }

    pub fn is_superset(&self, other: &Self) -> bool {
        self.0.is_superset(&other.0)
    }

    pub fn extend(&mut self, other: impl IntoIterator<Item = Capability>) {
        self.0.extend(other);
    }
}

impl FromIterator<Capability> for CapabilitySet {
    fn from_iter<T: IntoIterator<Item = Capability>>(iter: T) -> Self {
        Self::new(iter)
    }
}

impl IntoIterator for CapabilitySet {
    type Item = Capability;
    type IntoIter = std::collections::btree_set::IntoIter<Capability>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EngineCapabilities {
    pub engine: EngineKind,
    pub supported: CapabilitySet,
}

impl EngineCapabilities {
    pub fn new(engine: EngineKind, supported: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            engine,
            supported: CapabilitySet::new(supported),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CallbackRetention {
    Scoped,
    Retained,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CallbackThreading {
    CallingThread,
    MayCrossThread,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum CallbackReentrancy {
    Forbidden,
    Allowed,
}

/// Required callback semantics for one argument/return use-site.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct CallbackContract {
    pub retention: CallbackRetention,
    pub threading: CallbackThreading,
    pub reentrancy: CallbackReentrancy,
}

impl CallbackContract {
    pub fn required_capabilities(self) -> CapabilitySet {
        let mut capabilities = CapabilitySet::new([Capability::Callback]);
        if self.retention == CallbackRetention::Retained {
            capabilities.insert(Capability::RetainedCallback);
        }
        if self.reentrancy == CallbackReentrancy::Allowed {
            capabilities.insert(Capability::CallbackReentrancy);
        }
        capabilities
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ValuePathSegment {
    Argument(u32),
    Return,
    Field(String),
    Variant(String),
    SequenceItem,
    SetItem,
    MapKey,
    MapValue,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ValuePath(Vec<ValuePathSegment>);

impl ValuePath {
    pub fn argument(index: u32) -> Self {
        Self(vec![ValuePathSegment::Argument(index)])
    }

    pub fn return_value() -> Self {
        Self(vec![ValuePathSegment::Return])
    }

    pub fn then(mut self, segment: ValuePathSegment) -> Self {
        self.0.push(segment);
        self
    }

    pub fn segments(&self) -> &[ValuePathSegment] {
        &self.0
    }
}

impl fmt::Display for ValuePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, segment) in self.0.iter().enumerate() {
            if index != 0 {
                formatter.write_str(".")?;
            }
            match segment {
                ValuePathSegment::Argument(index) => write!(formatter, "argument[{index}]")?,
                ValuePathSegment::Return => formatter.write_str("return")?,
                ValuePathSegment::Field(name) => write!(formatter, "field[{name}]")?,
                ValuePathSegment::Variant(name) => write!(formatter, "variant[{name}]")?,
                ValuePathSegment::SequenceItem => formatter.write_str("item")?,
                ValuePathSegment::SetItem => formatter.write_str("set-item")?,
                ValuePathSegment::MapKey => formatter.write_str("key")?,
                ValuePathSegment::MapValue => formatter.write_str("value")?,
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallbackUseSite {
    pub operation_id: OperationId,
    pub callback_type: TypeId,
    pub path: ValuePath,
    pub contract: CallbackContract,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum StreamDirection {
    Input,
    Output,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StreamContract {
    pub direction: StreamDirection,
    pub lazy_start: bool,
    pub single_consumer: bool,
    pub serial_pull: bool,
    pub exactly_once_cleanup: bool,
    pub explicit_cancel: bool,
    pub eof_is_distinct_from_item: bool,
}

impl StreamContract {
    pub const fn input() -> Self {
        Self::standard(StreamDirection::Input)
    }

    pub const fn output() -> Self {
        Self::standard(StreamDirection::Output)
    }

    const fn standard(direction: StreamDirection) -> Self {
        Self {
            direction,
            lazy_start: true,
            single_consumer: true,
            serial_pull: true,
            exactly_once_cleanup: true,
            explicit_cancel: true,
            eof_is_distinct_from_item: true,
        }
    }

    fn is_standard(self) -> bool {
        self.lazy_start
            && self.single_consumer
            && self.serial_pull
            && self.exactly_once_cleanup
            && self.explicit_cancel
            && self.eof_is_distinct_from_item
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamUseSite {
    pub operation_id: OperationId,
    pub path: ValuePath,
    pub contract: StreamContract,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedOperation {
    pub operation: IdentifiedOperation,
    /// Requirements that are not inferable from the public value graph.
    pub extra_capabilities: CapabilitySet,
}

impl PlannedOperation {
    pub fn new(operation: IdentifiedOperation) -> Self {
        Self {
            operation,
            extra_capabilities: CapabilitySet::default(),
        }
    }

    pub fn requiring(mut self, capability: Capability) -> Self {
        self.extra_capabilities.insert(capability);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgePlanInput {
    pub components: Vec<IdentifiedComponent>,
    pub types: Vec<IdentifiedType>,
    pub operations: Vec<PlannedOperation>,
    pub callbacks: Vec<CallbackUseSite>,
    pub streams: Vec<StreamUseSite>,
    /// Every requested engine is validated.  An empty target set is rejected.
    pub targets: Vec<EngineCapabilities>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgeOperation {
    pub operation: IdentifiedOperation,
    pub required_capabilities: CapabilitySet,
}

/// Validated, deterministic, in-memory input for all engine generators.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BridgePlan {
    components: Vec<IdentifiedComponent>,
    types: Vec<IdentifiedType>,
    operations: Vec<BridgeOperation>,
    callbacks: Vec<CallbackUseSite>,
    streams: Vec<StreamUseSite>,
    targets: Vec<EngineCapabilities>,
}

type UseSiteKey = (OperationId, ValuePath);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedCallbackUseSite {
    callback_type: TypeId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedStreamUseSite {
    direction: StreamDirection,
}

impl BridgePlan {
    pub fn build(mut input: BridgePlanInput) -> Result<Self, ValidationReport> {
        let mut errors = Vec::new();

        validate_unique_ids(
            "component",
            input.components.iter().map(|value| value.id.index()),
            &mut errors,
        );
        validate_unique_ids(
            "type",
            input.types.iter().map(|value| value.id.index()),
            &mut errors,
        );
        validate_unique_ids(
            "operation",
            input
                .operations
                .iter()
                .map(|value| value.operation.id.index()),
            &mut errors,
        );

        input.components.sort_by_key(|value| value.id);
        input.types.sort_by_key(|value| value.id);
        input.operations.sort_by_key(|value| value.operation.id);
        input.callbacks.sort_by(|left, right| {
            (left.operation_id, &left.path).cmp(&(right.operation_id, &right.path))
        });
        input.streams.sort_by(|left, right| {
            (left.operation_id, &left.path).cmp(&(right.operation_id, &right.path))
        });
        input.targets.sort_by_key(|value| value.engine);

        validate_dense_ids(
            "component",
            input.components.iter().map(|value| value.id.index()),
            &mut errors,
        );
        validate_dense_ids(
            "type",
            input.types.iter().map(|value| value.id.index()),
            &mut errors,
        );
        validate_dense_ids(
            "operation",
            input
                .operations
                .iter()
                .map(|value| value.operation.id.index()),
            &mut errors,
        );

        let components_by_key: BTreeMap<_, _> = input
            .components
            .iter()
            .map(|value| (value.definition.source_key.clone(), value.id))
            .collect();
        let types_by_key: BTreeMap<_, _> = input
            .types
            .iter()
            .map(|value| (value.definition.source_key.clone(), value))
            .collect();
        let types_by_id: BTreeMap<_, _> =
            input.types.iter().map(|value| (value.id, value)).collect();
        let operation_ids: BTreeSet<_> = input
            .operations
            .iter()
            .map(|value| value.operation.id)
            .collect();

        validate_names_and_owners(
            &input.components,
            &input.types,
            &input.operations,
            &components_by_key,
            &mut errors,
        );

        if input.targets.is_empty() {
            errors.push(ValidationError::NoBuildTargets);
        }
        let mut seen_targets = BTreeSet::new();
        for target in &input.targets {
            if !seen_targets.insert(target.engine) {
                errors.push(ValidationError::DuplicateEngineTarget {
                    engine: target.engine,
                });
            }
        }

        let (expected_callbacks, expected_streams) =
            enumerate_expected_use_sites(&input.operations, &types_by_key, &mut errors);
        validate_callback_use_sites(
            &input.callbacks,
            &expected_callbacks,
            &operation_ids,
            &types_by_id,
            &types_by_key,
            &input.operations,
            &mut errors,
        );
        validate_stream_use_sites(
            &input.streams,
            &expected_streams,
            &operation_ids,
            &mut errors,
        );

        let callbacks_by_operation = group_callback_capabilities(
            &input.callbacks,
            &types_by_id,
            &types_by_key,
            &input.operations,
        );
        let streams_by_operation = group_stream_capabilities(&input.streams);
        let mut operations = Vec::with_capacity(input.operations.len());
        for planned in input.operations {
            let operation = &planned.operation;
            let mut required = planned.extra_capabilities;
            required.insert(match operation.definition.signature.async_kind {
                AsyncKind::Sync => Capability::SyncCall,
                AsyncKind::Async => Capability::AsyncCall,
            });
            for argument in &operation.definition.signature.arguments {
                infer_value_capabilities(
                    &argument.ty,
                    &types_by_key,
                    &mut BTreeSet::new(),
                    &mut required,
                    &mut errors,
                );
            }
            if let Some(return_type) = &operation.definition.signature.return_type {
                infer_value_capabilities(
                    return_type,
                    &types_by_key,
                    &mut BTreeSet::new(),
                    &mut required,
                    &mut errors,
                );
            }
            if let Some(error_type) = &operation.definition.signature.throws {
                required.insert(Capability::DeclaredError);
                infer_named_capabilities(
                    error_type,
                    &types_by_key,
                    &mut BTreeSet::new(),
                    &mut required,
                    &mut errors,
                );
            }
            if let Some(callback_capabilities) = callbacks_by_operation.get(&operation.id) {
                required.extend(callback_capabilities.iter());
            }
            if let Some(stream_capabilities) = streams_by_operation.get(&operation.id) {
                required.extend(stream_capabilities.iter());
            }

            for target in &input.targets {
                for capability in required.iter() {
                    if !target.supported.contains(capability) {
                        errors.push(ValidationError::UnsupportedCapability {
                            engine: target.engine,
                            operation_id: operation.id,
                            capability,
                        });
                    }
                }
            }
            operations.push(BridgeOperation {
                operation: planned.operation,
                required_capabilities: required,
            });
        }

        if !errors.is_empty() {
            errors.sort_by_key(ToString::to_string);
            errors.dedup();
            return Err(ValidationReport { errors });
        }

        Ok(Self {
            components: input.components,
            types: input.types,
            operations,
            callbacks: input.callbacks,
            streams: input.streams,
            targets: input.targets,
        })
    }

    pub fn components(&self) -> &[IdentifiedComponent] {
        &self.components
    }

    pub fn types(&self) -> &[IdentifiedType] {
        &self.types
    }

    pub fn operations(&self) -> &[BridgeOperation] {
        &self.operations
    }

    pub fn callbacks(&self) -> &[CallbackUseSite] {
        &self.callbacks
    }

    pub fn streams(&self) -> &[StreamUseSite] {
        &self.streams
    }

    pub fn targets(&self) -> &[EngineCapabilities] {
        &self.targets
    }
}

fn validate_unique_ids(
    table: &'static str,
    ids: impl IntoIterator<Item = u32>,
    errors: &mut Vec<ValidationError>,
) {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            errors.push(ValidationError::DuplicateId { table, id });
        }
    }
}

fn validate_dense_ids(
    table: &'static str,
    ids: impl IntoIterator<Item = u32>,
    errors: &mut Vec<ValidationError>,
) {
    for (expected, actual) in ids.into_iter().enumerate() {
        let Ok(expected) = u32::try_from(expected) else {
            break;
        };
        if expected != actual {
            errors.push(ValidationError::NonDenseId {
                table,
                expected,
                actual,
            });
        }
    }
}

fn validate_names_and_owners(
    components: &[IdentifiedComponent],
    types: &[IdentifiedType],
    operations: &[PlannedOperation],
    components_by_key: &BTreeMap<ComponentKey, ComponentId>,
    errors: &mut Vec<ValidationError>,
) {
    let types_by_key: BTreeMap<_, _> = types
        .iter()
        .map(|ty| (ty.definition.source_key.clone(), ty))
        .collect();
    let mut public_namespaces = BTreeMap::new();
    for component in components {
        if let Some(previous) =
            public_namespaces.insert(component.definition.public_namespace.clone(), component.id)
        {
            errors.push(ValidationError::PublicNameCollision {
                scope: "component namespace".to_owned(),
                name: component.definition.public_namespace.clone(),
                first_id: previous.index(),
                second_id: component.id.index(),
            });
        }
    }

    let mut public_types = BTreeMap::new();
    for ty in types {
        if !components_by_key.contains_key(ty.definition.source_key.component()) {
            errors.push(ValidationError::UnknownComponent {
                role: "type",
                component: ty.definition.source_key.component().clone(),
            });
        }
        let name_key = (
            ty.definition.source_key.component().clone(),
            ty.definition.public_name.clone(),
        );
        if let Some(previous) = public_types.insert(name_key, ty.id) {
            errors.push(ValidationError::PublicNameCollision {
                scope: format!("types in {}", ty.definition.source_key.component()),
                name: ty.definition.public_name.clone(),
                first_id: previous.index(),
                second_id: ty.id.index(),
            });
        }
        validate_type_member_names(ty, errors);
    }

    let mut public_operations = BTreeMap::new();
    let mut symbols = BTreeMap::new();
    for planned in operations {
        let operation = &planned.operation;
        let source = &operation.definition.source_key;
        if !components_by_key.contains_key(source.component()) {
            errors.push(ValidationError::UnknownComponent {
                role: "operation",
                component: source.component().clone(),
            });
        }
        validate_operation_owner(operation, &types_by_key, errors);
        validate_unique_public_names(
            format!("arguments of operation {}", operation.id),
            operation
                .definition
                .signature
                .arguments
                .iter()
                .map(|argument| argument.public_name.as_str()),
            errors,
        );
        let scope = (
            source.component().clone(),
            source.owner().clone(),
            operation.definition.public_name.clone(),
        );
        if let Some(previous) = public_operations.insert(scope, operation.id) {
            errors.push(ValidationError::PublicNameCollision {
                scope: format!("operations in {}::{}", source.component(), source.owner()),
                name: operation.definition.public_name.clone(),
                first_id: previous.index(),
                second_id: operation.id.index(),
            });
        }
        if let Some(previous) =
            symbols.insert(operation.definition.private_symbol.clone(), operation.id)
        {
            errors.push(ValidationError::PrivateSymbolCollision {
                symbol: operation.definition.private_symbol.clone(),
                first: previous,
                second: operation.id,
            });
        }
    }
}

fn validate_type_member_names(ty: &IdentifiedType, errors: &mut Vec<ValidationError>) {
    match &ty.definition.kind {
        NamedTypeKind::Record { fields } => validate_unique_public_names(
            format!("fields of record {}", ty.definition.source_key),
            fields.iter().map(|field| field.public_name.as_str()),
            errors,
        ),
        NamedTypeKind::Enum { variants } | NamedTypeKind::Error { variants } => {
            validate_unique_public_names(
                format!("variants of type {}", ty.definition.source_key),
                variants.iter().map(|variant| variant.public_name.as_str()),
                errors,
            );
            for variant in variants {
                validate_unique_public_names(
                    format!(
                        "fields of variant {}::{}",
                        ty.definition.source_key, variant.public_name
                    ),
                    variant
                        .fields
                        .iter()
                        .map(|field| field.public_name.as_str()),
                    errors,
                );
            }
        }
        NamedTypeKind::Object | NamedTypeKind::Callback => {}
    }
}

fn validate_unique_public_names<'a>(
    scope: String,
    names: impl IntoIterator<Item = &'a str>,
    errors: &mut Vec<ValidationError>,
) {
    let mut seen = BTreeSet::new();
    for name in names {
        if !seen.insert(name) {
            errors.push(ValidationError::DuplicateScopedPublicName {
                scope: scope.clone(),
                name: name.to_owned(),
            });
        }
    }
}

fn validate_operation_owner(
    operation: &IdentifiedOperation,
    types: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    errors: &mut Vec<ValidationError>,
) {
    let source = &operation.definition.source_key;
    let (owner_key, expected_kind) = match source.owner() {
        OperationOwner::Namespace => return,
        OperationOwner::Object(key) => (key, "object"),
        OperationOwner::Callback(key) => (key, "callback"),
    };

    if owner_key.component() != source.component() {
        errors.push(ValidationError::OperationOwnerComponentMismatch {
            operation_id: operation.id,
            operation_component: source.component().clone(),
            owner: owner_key.clone(),
        });
    }

    match types.get(owner_key) {
        None => errors.push(ValidationError::UnknownOperationOwnerType {
            operation_id: operation.id,
            owner: owner_key.clone(),
        }),
        Some(owner) => {
            let matches = matches!(
                (&owner.definition.kind, expected_kind),
                (NamedTypeKind::Object, "object") | (NamedTypeKind::Callback, "callback")
            );
            if !matches {
                errors.push(ValidationError::OperationOwnerKindMismatch {
                    operation_id: operation.id,
                    owner: owner_key.clone(),
                    expected_kind,
                    actual_kind: named_type_kind_name(&owner.definition.kind),
                });
            }
        }
    }
}

fn named_type_kind_name(kind: &NamedTypeKind) -> &'static str {
    match kind {
        NamedTypeKind::Record { .. } => "record",
        NamedTypeKind::Enum { .. } => "enum",
        NamedTypeKind::Error { .. } => "error",
        NamedTypeKind::Object => "object",
        NamedTypeKind::Callback => "callback",
    }
}

fn enumerate_expected_use_sites(
    operations: &[PlannedOperation],
    types: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    errors: &mut Vec<ValidationError>,
) -> (
    BTreeMap<UseSiteKey, ExpectedCallbackUseSite>,
    BTreeMap<UseSiteKey, ExpectedStreamUseSite>,
) {
    let mut callbacks = BTreeMap::new();
    let mut streams = BTreeMap::new();
    for planned in operations {
        let operation = &planned.operation;
        for (index, argument) in operation.definition.signature.arguments.iter().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                errors.push(ValidationError::TooManyOperationArguments {
                    operation_id: operation.id,
                });
                break;
            };
            enumerate_value_use_sites(
                operation.id,
                &argument.ty,
                ValuePath::argument(index),
                types,
                &mut BTreeSet::new(),
                &mut callbacks,
                &mut streams,
                errors,
            );
        }
        if let Some(return_type) = &operation.definition.signature.return_type {
            enumerate_value_use_sites(
                operation.id,
                return_type,
                ValuePath::return_value(),
                types,
                &mut BTreeSet::new(),
                &mut callbacks,
                &mut streams,
                errors,
            );
        }
    }
    (callbacks, streams)
}

#[allow(clippy::too_many_arguments)]
fn enumerate_value_use_sites(
    operation_id: OperationId,
    value: &ValueType,
    path: ValuePath,
    types: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    visiting: &mut BTreeSet<TypeSourceKey>,
    callbacks: &mut BTreeMap<UseSiteKey, ExpectedCallbackUseSite>,
    streams: &mut BTreeMap<UseSiteKey, ExpectedStreamUseSite>,
    errors: &mut Vec<ValidationError>,
) {
    match value {
        ValueType::Scalar(_) => {}
        ValueType::Named(key) => {
            let Some(ty) = types.get(key) else {
                errors.push(ValidationError::UnknownNamedType { key: key.clone() });
                return;
            };
            match &ty.definition.kind {
                NamedTypeKind::Callback => {
                    callbacks.insert(
                        (operation_id, path),
                        ExpectedCallbackUseSite {
                            callback_type: ty.id,
                        },
                    );
                }
                NamedTypeKind::Object => {}
                NamedTypeKind::Record { fields } => {
                    if !visiting.insert(key.clone()) {
                        return;
                    }
                    for field in fields {
                        enumerate_value_use_sites(
                            operation_id,
                            &field.ty,
                            path.clone()
                                .then(ValuePathSegment::Field(field.public_name.clone())),
                            types,
                            visiting,
                            callbacks,
                            streams,
                            errors,
                        );
                    }
                    visiting.remove(key);
                }
                NamedTypeKind::Enum { variants } | NamedTypeKind::Error { variants } => {
                    if !visiting.insert(key.clone()) {
                        return;
                    }
                    for variant in variants {
                        let variant_path = path
                            .clone()
                            .then(ValuePathSegment::Variant(variant.public_name.clone()));
                        for field in &variant.fields {
                            enumerate_value_use_sites(
                                operation_id,
                                &field.ty,
                                variant_path
                                    .clone()
                                    .then(ValuePathSegment::Field(field.public_name.clone())),
                                types,
                                visiting,
                                callbacks,
                                streams,
                                errors,
                            );
                        }
                    }
                    visiting.remove(key);
                }
            }
        }
        ValueType::Optional(inner) => enumerate_value_use_sites(
            operation_id,
            inner,
            path,
            types,
            visiting,
            callbacks,
            streams,
            errors,
        ),
        ValueType::Sequence(inner) => enumerate_value_use_sites(
            operation_id,
            inner,
            path.then(ValuePathSegment::SequenceItem),
            types,
            visiting,
            callbacks,
            streams,
            errors,
        ),
        ValueType::Set(inner) => enumerate_value_use_sites(
            operation_id,
            inner,
            path.then(ValuePathSegment::SetItem),
            types,
            visiting,
            callbacks,
            streams,
            errors,
        ),
        ValueType::Map(key, value) => {
            enumerate_value_use_sites(
                operation_id,
                key,
                path.clone().then(ValuePathSegment::MapKey),
                types,
                visiting,
                callbacks,
                streams,
                errors,
            );
            enumerate_value_use_sites(
                operation_id,
                value,
                path.then(ValuePathSegment::MapValue),
                types,
                visiting,
                callbacks,
                streams,
                errors,
            );
        }
        ValueType::InputStream(item) | ValueType::OutputStream(item) => {
            let direction = if matches!(value, ValueType::InputStream(_)) {
                StreamDirection::Input
            } else {
                StreamDirection::Output
            };
            streams.insert(
                (operation_id, path.clone()),
                ExpectedStreamUseSite { direction },
            );
            enumerate_value_use_sites(
                operation_id,
                item,
                path.then(ValuePathSegment::SequenceItem),
                types,
                visiting,
                callbacks,
                streams,
                errors,
            );
        }
    }
}

fn validate_callback_use_sites(
    callbacks: &[CallbackUseSite],
    expected: &BTreeMap<UseSiteKey, ExpectedCallbackUseSite>,
    operation_ids: &BTreeSet<OperationId>,
    types_by_id: &BTreeMap<TypeId, &IdentifiedType>,
    types_by_key: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    operations: &[PlannedOperation],
    errors: &mut Vec<ValidationError>,
) {
    let mut counts = BTreeMap::<UseSiteKey, usize>::new();
    for callback in callbacks {
        let key = (callback.operation_id, callback.path.clone());
        if !operation_ids.contains(&callback.operation_id) {
            errors.push(ValidationError::UnknownOperation {
                role: "callback use-site",
                operation_id: callback.operation_id,
            });
        }
        match types_by_id.get(&callback.callback_type) {
            Some(callback_type)
                if matches!(callback_type.definition.kind, NamedTypeKind::Callback) => {}
            Some(_) => errors.push(ValidationError::NotACallbackType {
                type_id: callback.callback_type,
                use_site: callback.path.to_string(),
            }),
            None => errors.push(ValidationError::UnknownTypeId {
                role: "callback use-site",
                type_id: callback.callback_type,
            }),
        }
        let Some(expected_site) = expected.get(&key) else {
            errors.push(ValidationError::UnexpectedCallbackContract {
                operation_id: callback.operation_id,
                use_site: callback.path.to_string(),
            });
            continue;
        };
        *counts.entry(key).or_default() += 1;
        if callback.callback_type != expected_site.callback_type {
            errors.push(ValidationError::CallbackTypeMismatch {
                operation_id: callback.operation_id,
                use_site: callback.path.to_string(),
                expected: expected_site.callback_type,
                actual: callback.callback_type,
            });
            continue;
        }
        validate_callback_method_contract(
            callback,
            expected_site.callback_type,
            types_by_id,
            types_by_key,
            operations,
            errors,
        );
    }

    for ((operation_id, path), expected_site) in expected {
        match counts
            .get(&(*operation_id, path.clone()))
            .copied()
            .unwrap_or(0)
        {
            0 => errors.push(ValidationError::MissingCallbackContract {
                operation_id: *operation_id,
                use_site: path.to_string(),
                callback_type: expected_site.callback_type,
            }),
            1 => {}
            count => errors.push(ValidationError::DuplicateCallbackContract {
                operation_id: *operation_id,
                use_site: path.to_string(),
                count,
            }),
        }
    }
}

fn validate_callback_method_contract(
    callback: &CallbackUseSite,
    callback_type: TypeId,
    types_by_id: &BTreeMap<TypeId, &IdentifiedType>,
    types_by_key: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    operations: &[PlannedOperation],
    errors: &mut Vec<ValidationError>,
) {
    let Some(callback_definition) = types_by_id.get(&callback_type) else {
        return;
    };
    let callback_key = &callback_definition.definition.source_key;
    if !types_by_key.contains_key(callback_key) {
        return;
    }
    let methods: Vec<_> = callback_methods(callback_key, operations);
    if methods.is_empty() {
        errors.push(ValidationError::CallbackTypeHasNoMethods {
            callback_type,
            use_site: callback.path.to_string(),
        });
        return;
    }

    if callback.contract.threading == CallbackThreading::MayCrossThread
        && methods
            .iter()
            .any(|method| method.operation.definition.signature.async_kind == AsyncKind::Sync)
    {
        errors.push(ValidationError::CrossThreadSyncCallback {
            use_site: callback.path.to_string(),
        });
    }
}

fn callback_methods<'a>(
    callback_key: &TypeSourceKey,
    operations: &'a [PlannedOperation],
) -> Vec<&'a PlannedOperation> {
    operations
        .iter()
        .filter(|planned| {
            matches!(
                planned.operation.definition.source_key.owner(),
                OperationOwner::Callback(owner) if owner == callback_key
            ) && planned.operation.definition.source_key.kind() == OperationKind::CallbackMethod
        })
        .collect()
}

fn validate_stream_use_sites(
    streams: &[StreamUseSite],
    expected: &BTreeMap<UseSiteKey, ExpectedStreamUseSite>,
    operation_ids: &BTreeSet<OperationId>,
    errors: &mut Vec<ValidationError>,
) {
    let mut counts = BTreeMap::<UseSiteKey, usize>::new();
    for stream in streams {
        let key = (stream.operation_id, stream.path.clone());
        if !operation_ids.contains(&stream.operation_id) {
            errors.push(ValidationError::UnknownOperation {
                role: "stream use-site",
                operation_id: stream.operation_id,
            });
        }
        if !stream.contract.is_standard() {
            errors.push(ValidationError::NonCanonicalStreamContract {
                use_site: stream.path.to_string(),
            });
        }
        let Some(expected_site) = expected.get(&key) else {
            errors.push(ValidationError::UnexpectedStreamContract {
                operation_id: stream.operation_id,
                use_site: stream.path.to_string(),
            });
            continue;
        };
        *counts.entry(key).or_default() += 1;
        if stream.contract.direction != expected_site.direction {
            errors.push(ValidationError::StreamDirectionMismatch {
                operation_id: stream.operation_id,
                use_site: stream.path.to_string(),
                signature: expected_site.direction,
                contract: stream.contract.direction,
            });
        }
    }

    for ((operation_id, path), expected_site) in expected {
        match counts
            .get(&(*operation_id, path.clone()))
            .copied()
            .unwrap_or(0)
        {
            0 => errors.push(ValidationError::MissingStreamContract {
                operation_id: *operation_id,
                use_site: path.to_string(),
                direction: expected_site.direction,
            }),
            1 => {}
            count => errors.push(ValidationError::DuplicateStreamContract {
                operation_id: *operation_id,
                use_site: path.to_string(),
                count,
            }),
        }
    }
}

fn group_callback_capabilities(
    callbacks: &[CallbackUseSite],
    types_by_id: &BTreeMap<TypeId, &IdentifiedType>,
    types_by_key: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    operations: &[PlannedOperation],
) -> BTreeMap<OperationId, CapabilitySet> {
    let mut grouped = BTreeMap::<OperationId, CapabilitySet>::new();
    for callback in callbacks {
        grouped
            .entry(callback.operation_id)
            .or_default()
            .extend(callback_method_capabilities(
                callback,
                types_by_id,
                types_by_key,
                operations,
            ));
    }
    grouped
}

fn callback_method_capabilities(
    callback: &CallbackUseSite,
    types_by_id: &BTreeMap<TypeId, &IdentifiedType>,
    types_by_key: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    operations: &[PlannedOperation],
) -> CapabilitySet {
    let mut capabilities = callback.contract.required_capabilities();
    let Some(callback_definition) = types_by_id.get(&callback.callback_type) else {
        return capabilities;
    };
    let callback_key = &callback_definition.definition.source_key;
    if !types_by_key.contains_key(callback_key) {
        return capabilities;
    }
    let methods = callback_methods(callback_key, operations);
    if methods
        .iter()
        .any(|method| method.operation.definition.signature.async_kind == AsyncKind::Async)
    {
        capabilities.insert(Capability::AsyncCallback);
    }
    if methods
        .iter()
        .any(|method| method.operation.definition.signature.throws.is_some())
    {
        capabilities.insert(Capability::FallibleCallback);
    }
    if callback.contract.threading == CallbackThreading::MayCrossThread
        && !methods.is_empty()
        && methods
            .iter()
            .all(|method| method.operation.definition.signature.async_kind == AsyncKind::Async)
    {
        capabilities.insert(Capability::CrossThreadAsyncCallback);
    }
    capabilities
}

fn group_stream_capabilities(streams: &[StreamUseSite]) -> BTreeMap<OperationId, CapabilitySet> {
    let mut grouped = BTreeMap::<OperationId, CapabilitySet>::new();
    for stream in streams {
        grouped
            .entry(stream.operation_id)
            .or_default()
            .insert(match stream.contract.direction {
                StreamDirection::Input => Capability::InputStream,
                StreamDirection::Output => Capability::OutputStream,
            });
    }
    grouped
}

fn infer_named_capabilities(
    key: &TypeSourceKey,
    types: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    visiting: &mut BTreeSet<TypeSourceKey>,
    required: &mut CapabilitySet,
    errors: &mut Vec<ValidationError>,
) {
    let Some(ty) = types.get(key) else {
        errors.push(ValidationError::UnknownNamedType { key: key.clone() });
        return;
    };
    if !visiting.insert(key.clone()) {
        return;
    }
    match &ty.definition.kind {
        NamedTypeKind::Record { fields } => {
            required.insert(Capability::Record);
            for field in fields {
                infer_value_capabilities(&field.ty, types, visiting, required, errors);
            }
        }
        NamedTypeKind::Enum { variants } => {
            required.insert(Capability::Enum);
            for variant in variants {
                for field in &variant.fields {
                    infer_value_capabilities(&field.ty, types, visiting, required, errors);
                }
            }
        }
        NamedTypeKind::Error { variants } => {
            required.insert(Capability::DeclaredError);
            for variant in variants {
                for field in &variant.fields {
                    infer_value_capabilities(&field.ty, types, visiting, required, errors);
                }
            }
        }
        NamedTypeKind::Object => {
            required.insert(Capability::ObjectLease);
        }
        NamedTypeKind::Callback => {
            required.insert(Capability::Callback);
        }
    }
    visiting.remove(key);
}

fn infer_value_capabilities(
    value: &ValueType,
    types: &BTreeMap<TypeSourceKey, &IdentifiedType>,
    visiting: &mut BTreeSet<TypeSourceKey>,
    required: &mut CapabilitySet,
    errors: &mut Vec<ValidationError>,
) {
    match value {
        ValueType::Scalar(scalar) => {
            required.insert(Capability::Primitive);
            required.insert(match scalar {
                ScalarType::String => Capability::String,
                ScalarType::Bytes => Capability::Bytes,
                ScalarType::I64 | ScalarType::U64 => Capability::BigInt,
                _ => return,
            });
        }
        ValueType::Named(key) => {
            infer_named_capabilities(key, types, visiting, required, errors);
        }
        ValueType::Optional(inner) => {
            required.insert(Capability::Optional);
            infer_value_capabilities(inner, types, visiting, required, errors);
        }
        ValueType::Sequence(inner) => {
            required.insert(Capability::Sequence);
            infer_value_capabilities(inner, types, visiting, required, errors);
        }
        ValueType::Map(key, value) => {
            required.insert(Capability::Map);
            infer_value_capabilities(key, types, visiting, required, errors);
            infer_value_capabilities(value, types, visiting, required, errors);
        }
        ValueType::Set(inner) => {
            required.insert(Capability::Set);
            infer_value_capabilities(inner, types, visiting, required, errors);
        }
        ValueType::InputStream(inner) => {
            required.insert(Capability::InputStream);
            infer_value_capabilities(inner, types, visiting, required, errors);
        }
        ValueType::OutputStream(inner) => {
            required.insert(Capability::OutputStream);
            infer_value_capabilities(inner, types, visiting, required, errors);
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ValidationError {
    NoBuildTargets,
    DuplicateEngineTarget {
        engine: EngineKind,
    },
    DuplicateId {
        table: &'static str,
        id: u32,
    },
    NonDenseId {
        table: &'static str,
        expected: u32,
        actual: u32,
    },
    PublicNameCollision {
        scope: String,
        name: String,
        first_id: u32,
        second_id: u32,
    },
    DuplicateScopedPublicName {
        scope: String,
        name: String,
    },
    PrivateSymbolCollision {
        symbol: String,
        first: OperationId,
        second: OperationId,
    },
    UnknownComponent {
        role: &'static str,
        component: ComponentKey,
    },
    UnknownNamedType {
        key: TypeSourceKey,
    },
    UnknownOperationOwnerType {
        operation_id: OperationId,
        owner: TypeSourceKey,
    },
    OperationOwnerComponentMismatch {
        operation_id: OperationId,
        operation_component: ComponentKey,
        owner: TypeSourceKey,
    },
    OperationOwnerKindMismatch {
        operation_id: OperationId,
        owner: TypeSourceKey,
        expected_kind: &'static str,
        actual_kind: &'static str,
    },
    TooManyOperationArguments {
        operation_id: OperationId,
    },
    UnknownTypeId {
        role: &'static str,
        type_id: TypeId,
    },
    NotACallbackType {
        type_id: TypeId,
        use_site: String,
    },
    UnknownOperation {
        role: &'static str,
        operation_id: OperationId,
    },
    CrossThreadSyncCallback {
        use_site: String,
    },
    MissingCallbackContract {
        operation_id: OperationId,
        use_site: String,
        callback_type: TypeId,
    },
    DuplicateCallbackContract {
        operation_id: OperationId,
        use_site: String,
        count: usize,
    },
    UnexpectedCallbackContract {
        operation_id: OperationId,
        use_site: String,
    },
    CallbackTypeMismatch {
        operation_id: OperationId,
        use_site: String,
        expected: TypeId,
        actual: TypeId,
    },
    CallbackTypeHasNoMethods {
        callback_type: TypeId,
        use_site: String,
    },
    NonCanonicalStreamContract {
        use_site: String,
    },
    MissingStreamContract {
        operation_id: OperationId,
        use_site: String,
        direction: StreamDirection,
    },
    DuplicateStreamContract {
        operation_id: OperationId,
        use_site: String,
        count: usize,
    },
    UnexpectedStreamContract {
        operation_id: OperationId,
        use_site: String,
    },
    StreamDirectionMismatch {
        operation_id: OperationId,
        use_site: String,
        signature: StreamDirection,
        contract: StreamDirection,
    },
    UnsupportedCapability {
        engine: EngineKind,
        operation_id: OperationId,
        capability: Capability,
    },
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoBuildTargets => formatter.write_str("bridge plan has no build targets"),
            Self::DuplicateEngineTarget { engine } => {
                write!(formatter, "duplicate engine target {engine:?}")
            }
            Self::DuplicateId { table, id } => write!(formatter, "duplicate {table} ID {id}"),
            Self::NonDenseId {
                table,
                expected,
                actual,
            } => write!(
                formatter,
                "non-dense {table} ID table: expected {expected}, found {actual}"
            ),
            Self::PublicNameCollision {
                scope,
                name,
                first_id,
                second_id,
            } => write!(
                formatter,
                "public name {name:?} collides in {scope} between IDs {first_id} and {second_id}"
            ),
            Self::DuplicateScopedPublicName { scope, name } => {
                write!(formatter, "duplicate public name {name:?} in {scope}")
            }
            Self::PrivateSymbolCollision {
                symbol,
                first,
                second,
            } => write!(
                formatter,
                "private symbol {symbol:?} collides between operations {first} and {second}"
            ),
            Self::UnknownComponent { role, component } => {
                write!(formatter, "{role} references unknown component {component}")
            }
            Self::UnknownNamedType { key } => write!(formatter, "unknown named type {key}"),
            Self::UnknownOperationOwnerType {
                operation_id,
                owner,
            } => write!(
                formatter,
                "operation {operation_id} references unknown owner type {owner}"
            ),
            Self::OperationOwnerComponentMismatch {
                operation_id,
                operation_component,
                owner,
            } => write!(
                formatter,
                "operation {operation_id} belongs to component {operation_component} but its owner type is {owner}"
            ),
            Self::OperationOwnerKindMismatch {
                operation_id,
                owner,
                expected_kind,
                actual_kind,
            } => write!(
                formatter,
                "operation {operation_id} requires a {expected_kind} owner, but {owner} is a {actual_kind}"
            ),
            Self::TooManyOperationArguments { operation_id } => write!(
                formatter,
                "operation {operation_id} has too many arguments for u32 value paths"
            ),
            Self::UnknownTypeId { role, type_id } => {
                write!(formatter, "{role} references unknown type ID {type_id}")
            }
            Self::NotACallbackType { type_id, use_site } => write!(
                formatter,
                "callback use-site {use_site} references non-callback type ID {type_id}"
            ),
            Self::UnknownOperation { role, operation_id } => {
                write!(formatter, "{role} references unknown operation ID {operation_id}")
            }
            Self::CrossThreadSyncCallback { use_site } => write!(
                formatter,
                "callback at {use_site} is synchronous and may cross threads; the unified base forbids this contract"
            ),
            Self::MissingCallbackContract {
                operation_id,
                use_site,
                callback_type,
            } => write!(
                formatter,
                "callback type {callback_type} at operation {operation_id} {use_site} has no callback contract"
            ),
            Self::DuplicateCallbackContract {
                operation_id,
                use_site,
                count,
            } => write!(
                formatter,
                "callback at operation {operation_id} {use_site} has {count} contracts; exactly one is required"
            ),
            Self::UnexpectedCallbackContract {
                operation_id,
                use_site,
            } => write!(
                formatter,
                "callback contract at operation {operation_id} {use_site} does not name a callback in the signature"
            ),
            Self::CallbackTypeMismatch {
                operation_id,
                use_site,
                expected,
                actual,
            } => write!(
                formatter,
                "callback contract at operation {operation_id} {use_site} references type {actual}, but the signature contains callback type {expected}"
            ),
            Self::CallbackTypeHasNoMethods {
                callback_type,
                use_site,
            } => write!(
                formatter,
                "callback type {callback_type} at {use_site} has no callback-method operations"
            ),
            Self::NonCanonicalStreamContract { use_site } => write!(
                formatter,
                "stream at {use_site} weakens the canonical lazy, serial, cancellable, exactly-once contract"
            ),
            Self::MissingStreamContract {
                operation_id,
                use_site,
                direction,
            } => write!(
                formatter,
                "{direction:?} stream at operation {operation_id} {use_site} has no stream contract"
            ),
            Self::DuplicateStreamContract {
                operation_id,
                use_site,
                count,
            } => write!(
                formatter,
                "stream at operation {operation_id} {use_site} has {count} contracts; exactly one is required"
            ),
            Self::UnexpectedStreamContract {
                operation_id,
                use_site,
            } => write!(
                formatter,
                "stream contract at operation {operation_id} {use_site} does not name a stream in the signature"
            ),
            Self::StreamDirectionMismatch {
                operation_id,
                use_site,
                signature,
                contract,
            } => write!(
                formatter,
                "stream contract at operation {operation_id} {use_site} is {contract:?}, but the signature declares {signature:?}"
            ),
            Self::UnsupportedCapability {
                engine,
                operation_id,
                capability,
            } => write!(
                formatter,
                "engine {engine:?} lacks {capability:?} required by operation {operation_id}"
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidationReport {
    errors: Vec<ValidationError>,
}

impl ValidationReport {
    pub fn errors(&self) -> &[ValidationError] {
        &self.errors
    }

    pub fn contains(&self, predicate: impl Fn(&ValidationError) -> bool) -> bool {
        self.errors.iter().any(predicate)
    }
}

impl fmt::Display for ValidationReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index != 0 {
                formatter.write_str("; ")?;
            }
            error.fmt(formatter)?;
        }
        Ok(())
    }
}

impl Error for ValidationReport {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseOutcome {
    Released,
    AlreadyReleased,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaseError {
    StaleGeneration { expected: u32, actual: u32 },
    Released,
}

impl fmt::Display for LeaseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeneration { expected, actual } => write!(
                formatter,
                "stale resource generation {actual}; current generation is {expected}"
            ),
            Self::Released => formatter.write_str("resource lease has already been released"),
        }
    }
}

impl Error for LeaseError {}

/// Session-local object lease state.  No artifact or cross-generation
/// identity is encoded here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectLease {
    pub session_id: u64,
    pub type_id: TypeId,
    pub lease_id: u64,
    generation: u32,
    released: bool,
}

impl ObjectLease {
    pub fn new(session_id: u64, type_id: TypeId, lease_id: u64, generation: u32) -> Self {
        Self {
            session_id,
            type_id,
            lease_id,
            generation,
            released: false,
        }
    }

    pub const fn generation(&self) -> u32 {
        self.generation
    }

    pub fn ensure_active(&self, generation: u32) -> Result<(), LeaseError> {
        self.check_generation(generation)?;
        if self.released {
            Err(LeaseError::Released)
        } else {
            Ok(())
        }
    }

    /// Release is non-throwing and idempotent after the generation has been
    /// checked by the backend.
    pub fn release(&mut self, generation: u32) -> Result<ReleaseOutcome, LeaseError> {
        self.check_generation(generation)?;
        if std::mem::replace(&mut self.released, true) {
            Ok(ReleaseOutcome::AlreadyReleased)
        } else {
            Ok(ReleaseOutcome::Released)
        }
    }

    fn check_generation(&self, generation: u32) -> Result<(), LeaseError> {
        if generation == self.generation {
            Ok(())
        } else {
            Err(LeaseError::StaleGeneration {
                expected: self.generation,
                actual: generation,
            })
        }
    }
}

/// Pull results use an explicit `Done` variant, so an optional `null` item can
/// never be mistaken for end-of-stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RawStreamStep<T, E> {
    Item(T),
    Done,
    Error(E),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamPhase {
    Idle,
    Active,
    Pulling,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullCompletion {
    Item,
    Done,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PullOutcome {
    ItemAccepted,
    Closed { cleanup: bool },
    LateResultIgnored,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CloseOutcome {
    pub cleanup: bool,
    pub pending_pull_observes_done: bool,
    pub call_iterator_return: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamError {
    ConsumerAlreadyClaimed,
    ConsumerNotClaimed,
    Closed,
    ConcurrentPull,
    NoPendingPull,
}

impl fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConsumerAlreadyClaimed => formatter.write_str("stream already has a consumer"),
            Self::ConsumerNotClaimed => formatter.write_str("stream has no consumer"),
            Self::Closed => formatter.write_str("stream is closed"),
            Self::ConcurrentPull => formatter.write_str("stream already has a pending pull"),
            Self::NoPendingPull => formatter.write_str("stream has no pending pull"),
        }
    }
}

impl Error for StreamError {}

/// Executable reference state machine for both input and output stream
/// adapters.  It owns no JS/native values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamLifecycle {
    direction: StreamDirection,
    phase: StreamPhase,
    consumer_claimed: bool,
    cleanup_claimed: bool,
    iterator_return_claimed: bool,
    ignore_late_pull: bool,
}

impl StreamLifecycle {
    pub const fn new(direction: StreamDirection) -> Self {
        Self {
            direction,
            phase: StreamPhase::Idle,
            consumer_claimed: false,
            cleanup_claimed: false,
            iterator_return_claimed: false,
            ignore_late_pull: false,
        }
    }

    pub const fn phase(&self) -> StreamPhase {
        self.phase
    }

    pub fn claim_consumer(&mut self) -> Result<(), StreamError> {
        if self.consumer_claimed {
            Err(StreamError::ConsumerAlreadyClaimed)
        } else {
            self.consumer_claimed = true;
            Ok(())
        }
    }

    /// First pull lazily starts the stream.  A second pending pull is rejected.
    pub fn begin_pull(&mut self) -> Result<(), StreamError> {
        if !self.consumer_claimed {
            return Err(StreamError::ConsumerNotClaimed);
        }
        match self.phase {
            StreamPhase::Idle | StreamPhase::Active => {
                self.phase = StreamPhase::Pulling;
                Ok(())
            }
            StreamPhase::Pulling => Err(StreamError::ConcurrentPull),
            StreamPhase::Closed => Err(StreamError::Closed),
        }
    }

    pub fn complete_pull(
        &mut self,
        completion: PullCompletion,
    ) -> Result<PullOutcome, StreamError> {
        if self.phase == StreamPhase::Closed && self.ignore_late_pull {
            self.ignore_late_pull = false;
            return Ok(PullOutcome::LateResultIgnored);
        }
        if self.phase != StreamPhase::Pulling {
            return Err(StreamError::NoPendingPull);
        }
        match completion {
            PullCompletion::Item => {
                self.phase = StreamPhase::Active;
                Ok(PullOutcome::ItemAccepted)
            }
            PullCompletion::Done | PullCompletion::Error => {
                self.phase = StreamPhase::Closed;
                Ok(PullOutcome::Closed {
                    cleanup: self.claim_cleanup(),
                })
            }
        }
    }

    pub fn cancel(&mut self) -> CloseOutcome {
        self.close(true)
    }

    pub fn drain(&mut self) -> CloseOutcome {
        self.close(false)
    }

    fn close(&mut self, explicit_cancel: bool) -> CloseOutcome {
        let was_closed = self.phase == StreamPhase::Closed;
        let pending = self.phase == StreamPhase::Pulling;
        if pending {
            self.ignore_late_pull = true;
        }
        self.phase = StreamPhase::Closed;
        let call_iterator_return = explicit_cancel
            && self.direction == StreamDirection::Input
            && !was_closed
            && !std::mem::replace(&mut self.iterator_return_claimed, true);
        CloseOutcome {
            cleanup: self.claim_cleanup(),
            pending_pull_observes_done: pending,
            call_iterator_return,
        }
    }

    fn claim_cleanup(&mut self) -> bool {
        !std::mem::replace(&mut self.cleanup_claimed, true)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionPhase {
    Open,
    Closing,
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeadlineAction {
    Detach,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClosePolicy {
    pub grace_ms: u32,
    pub on_deadline: DeadlineAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionLifecycle {
    phase: SessionPhase,
}

impl Default for SessionLifecycle {
    fn default() -> Self {
        Self {
            phase: SessionPhase::Open,
        }
    }
}

impl SessionLifecycle {
    pub const fn phase(self) -> SessionPhase {
        self.phase
    }

    pub fn begin_close(&mut self) -> bool {
        if self.phase == SessionPhase::Open {
            self.phase = SessionPhase::Closing;
            true
        } else {
            false
        }
    }

    pub fn finish_close(&mut self) {
        self.phase = SessionPhase::Closed;
    }

    pub const fn accepts_calls(self) -> bool {
        matches!(self.phase, SessionPhase::Open)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uniffi_js_abi::{
        assign_component_ids, assign_operation_ids, assign_type_ids, ArgumentDefinition,
        ComponentDefinition, EnumVariant, FieldDefinition, OperationDefinition, OperationOwner,
        OperationSignature, OperationSourceKey, Ownership, TypeDefinition,
    };

    fn full_capabilities() -> CapabilitySet {
        CapabilitySet::new([
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
        ])
    }

    fn corpus_input() -> BridgePlanInput {
        let component_key = ComponentKey::new("contract_corpus").unwrap();
        let components = assign_component_ids([ComponentDefinition::new(
            component_key.clone(),
            "contractCorpus",
        )
        .unwrap()])
        .unwrap();
        let profile_key = TypeSourceKey::new(component_key.clone(), "Profile").unwrap();
        let event_key = TypeSourceKey::new(component_key.clone(), "Event").unwrap();
        let error_key = TypeSourceKey::new(component_key.clone(), "Failure").unwrap();
        let object_key = TypeSourceKey::new(component_key.clone(), "Service").unwrap();
        let callback_key = TypeSourceKey::new(component_key.clone(), "Observer").unwrap();
        let types = assign_type_ids([
            TypeDefinition::new(
                profile_key.clone(),
                "Profile",
                NamedTypeKind::Record {
                    fields: vec![
                        FieldDefinition::new(
                            "scores",
                            ValueType::map(
                                ValueType::Scalar(ScalarType::String),
                                ValueType::Scalar(ScalarType::I64),
                            ),
                        )
                        .unwrap(),
                        FieldDefinition::new(
                            "tags",
                            ValueType::set(ValueType::Scalar(ScalarType::String)),
                        )
                        .unwrap(),
                        FieldDefinition::new(
                            "avatar",
                            ValueType::optional(ValueType::Scalar(ScalarType::Bytes)),
                        )
                        .unwrap(),
                        FieldDefinition::new(
                            "input",
                            ValueType::input_stream(ValueType::Scalar(ScalarType::Bytes)),
                        )
                        .unwrap(),
                    ],
                },
            )
            .unwrap(),
            TypeDefinition::new(
                event_key.clone(),
                "Event",
                NamedTypeKind::Enum {
                    variants: vec![EnumVariant::new("Ready", vec![]).unwrap()],
                },
            )
            .unwrap(),
            TypeDefinition::new(
                error_key.clone(),
                "Failure",
                NamedTypeKind::Error {
                    variants: vec![EnumVariant::new(
                        "Rejected",
                        vec![FieldDefinition::new(
                            "message",
                            ValueType::Scalar(ScalarType::String),
                        )
                        .unwrap()],
                    )
                    .unwrap()],
                },
            )
            .unwrap(),
            TypeDefinition::new(object_key.clone(), "Service", NamedTypeKind::Object).unwrap(),
            TypeDefinition::new(callback_key.clone(), "Observer", NamedTypeKind::Callback).unwrap(),
        ])
        .unwrap();
        let callback_type = types
            .iter()
            .find(|ty| ty.definition.source_key == callback_key)
            .unwrap()
            .id;
        let operations = assign_operation_ids([
            OperationDefinition::new(
                OperationSourceKey::new(
                    component_key.clone(),
                    OperationOwner::Namespace,
                    uniffi_js_abi::OperationKind::Function,
                    "observe",
                )
                .unwrap(),
                "observe",
                "contract_corpus.observe",
                "__uniffi_contract_corpus_observe",
                OperationSignature {
                    arguments: vec![
                        ArgumentDefinition::new(
                            "profile",
                            ValueType::Named(profile_key),
                            Ownership::Owned,
                        )
                        .unwrap(),
                        ArgumentDefinition::new(
                            "observer",
                            ValueType::Named(callback_key.clone()),
                            Ownership::Borrowed,
                        )
                        .unwrap(),
                    ],
                    return_type: Some(ValueType::Named(object_key)),
                    async_kind: AsyncKind::Async,
                    throws: Some(error_key.clone()),
                },
            )
            .unwrap(),
            OperationDefinition::new(
                OperationSourceKey::new(
                    component_key.clone(),
                    OperationOwner::Callback(callback_key.clone()),
                    uniffi_js_abi::OperationKind::CallbackMethod,
                    "on_ready",
                )
                .unwrap(),
                "onReady",
                "contract_corpus.Observer.onReady",
                "__uniffi_contract_corpus_observer_on_ready",
                OperationSignature {
                    arguments: vec![ArgumentDefinition::new(
                        "event",
                        ValueType::Named(event_key.clone()),
                        Ownership::Borrowed,
                    )
                    .unwrap()],
                    return_type: None,
                    async_kind: AsyncKind::Sync,
                    throws: None,
                },
            )
            .unwrap(),
            OperationDefinition::new(
                OperationSourceKey::new(
                    component_key.clone(),
                    OperationOwner::Callback(callback_key.clone()),
                    uniffi_js_abi::OperationKind::CallbackMethod,
                    "on_ready_checked",
                )
                .unwrap(),
                "onReadyChecked",
                "contract_corpus.Observer.onReadyChecked",
                "__uniffi_contract_corpus_observer_on_ready_checked",
                OperationSignature {
                    arguments: vec![ArgumentDefinition::new(
                        "event",
                        ValueType::Named(event_key.clone()),
                        Ownership::Borrowed,
                    )
                    .unwrap()],
                    return_type: None,
                    async_kind: AsyncKind::Sync,
                    throws: Some(error_key.clone()),
                },
            )
            .unwrap(),
            OperationDefinition::new(
                OperationSourceKey::new(
                    component_key.clone(),
                    OperationOwner::Callback(callback_key.clone()),
                    uniffi_js_abi::OperationKind::CallbackMethod,
                    "on_event",
                )
                .unwrap(),
                "onEvent",
                "contract_corpus.Observer.onEvent",
                "__uniffi_contract_corpus_observer_on_event",
                OperationSignature {
                    arguments: vec![ArgumentDefinition::new(
                        "event",
                        ValueType::Named(event_key.clone()),
                        Ownership::Borrowed,
                    )
                    .unwrap()],
                    return_type: None,
                    async_kind: AsyncKind::Async,
                    throws: None,
                },
            )
            .unwrap(),
            OperationDefinition::new(
                OperationSourceKey::new(
                    component_key.clone(),
                    OperationOwner::Callback(callback_key.clone()),
                    uniffi_js_abi::OperationKind::CallbackMethod,
                    "on_event_checked",
                )
                .unwrap(),
                "onEventChecked",
                "contract_corpus.Observer.onEventChecked",
                "__uniffi_contract_corpus_observer_on_event_checked",
                OperationSignature {
                    arguments: vec![ArgumentDefinition::new(
                        "event",
                        ValueType::Named(event_key.clone()),
                        Ownership::Borrowed,
                    )
                    .unwrap()],
                    return_type: None,
                    async_kind: AsyncKind::Async,
                    throws: Some(error_key.clone()),
                },
            )
            .unwrap(),
            OperationDefinition::new(
                OperationSourceKey::new(
                    component_key,
                    OperationOwner::Namespace,
                    uniffi_js_abi::OperationKind::Function,
                    "events",
                )
                .unwrap(),
                "events",
                "contract_corpus.events",
                "__uniffi_contract_corpus_events",
                OperationSignature {
                    arguments: vec![],
                    return_type: Some(ValueType::output_stream(ValueType::Named(event_key))),
                    async_kind: AsyncKind::Sync,
                    throws: None,
                },
            )
            .unwrap(),
        ])
        .unwrap();
        let observe = operations
            .iter()
            .find(|operation| operation.definition.public_name == "observe")
            .unwrap()
            .id;
        let events = operations
            .iter()
            .find(|operation| operation.definition.public_name == "events")
            .unwrap()
            .id;
        BridgePlanInput {
            components,
            types,
            operations: operations.into_iter().map(PlannedOperation::new).collect(),
            callbacks: vec![CallbackUseSite {
                operation_id: observe,
                callback_type,
                path: ValuePath::argument(1),
                contract: CallbackContract {
                    retention: CallbackRetention::Retained,
                    threading: CallbackThreading::CallingThread,
                    reentrancy: CallbackReentrancy::Allowed,
                },
            }],
            streams: vec![
                StreamUseSite {
                    operation_id: observe,
                    path: ValuePath::argument(0).then(ValuePathSegment::Field("input".to_owned())),
                    contract: StreamContract::input(),
                },
                StreamUseSite {
                    operation_id: events,
                    path: ValuePath::return_value(),
                    contract: StreamContract::output(),
                },
            ],
            targets: [
                EngineKind::Napi,
                EngineKind::WasmBindgen,
                EngineKind::OhosNapi,
            ]
            .into_iter()
            .map(|engine| EngineCapabilities {
                engine,
                supported: full_capabilities(),
            })
            .collect(),
        }
    }

    #[test]
    fn complete_contract_corpus_builds_for_all_engines() {
        let plan = BridgePlan::build(corpus_input()).unwrap();
        assert_eq!(plan.targets().len(), 3);
        assert_eq!(plan.callbacks().len(), 1);
        assert_eq!(plan.streams().len(), 2);
        let observe = plan
            .operations()
            .iter()
            .find(|operation| operation.operation.definition.public_name == "observe")
            .unwrap();
        for capability in [
            Capability::BigInt,
            Capability::Bytes,
            Capability::Map,
            Capability::Set,
            Capability::Record,
            Capability::DeclaredError,
            Capability::ObjectLease,
            Capability::AsyncCall,
            Capability::RetainedCallback,
            Capability::AsyncCallback,
            Capability::FallibleCallback,
            Capability::InputStream,
        ] {
            assert!(observe.required_capabilities.contains(capability));
        }
        assert_eq!(
            plan.operations()
                .iter()
                .filter(|operation| {
                    matches!(
                        operation.operation.definition.source_key.owner(),
                        OperationOwner::Callback(_)
                    )
                })
                .count(),
            4
        );
    }

    #[test]
    fn unsupported_capabilities_fail_before_a_plan_exists() {
        let mut input = corpus_input();
        input.targets[1].supported = CapabilitySet::new([Capability::Primitive]);
        let report = BridgePlan::build(input).unwrap_err();
        assert!(report.contains(|error| matches!(
            error,
            ValidationError::UnsupportedCapability {
                engine: EngineKind::WasmBindgen,
                capability: Capability::BigInt,
                ..
            }
        )));
    }

    #[test]
    fn duplicate_ids_names_and_symbols_are_diagnosed() {
        let mut input = corpus_input();
        let duplicate = input.operations[0].clone();
        input.operations.push(duplicate);
        let mut conflicting = input.operations[1].clone();
        conflicting.operation.id = OperationId::new(9);
        conflicting.operation.definition.public_name =
            input.operations[0].operation.definition.public_name.clone();
        conflicting.operation.definition.private_symbol = input.operations[0]
            .operation
            .definition
            .private_symbol
            .clone();
        input.operations.push(conflicting);

        let report = BridgePlan::build(input).unwrap_err();
        assert!(report.contains(|error| matches!(
            error,
            ValidationError::DuplicateId {
                table: "operation",
                ..
            }
        )));
        assert!(
            report.contains(|error| matches!(error, ValidationError::PublicNameCollision { .. }))
        );
        assert!(report
            .contains(|error| matches!(error, ValidationError::PrivateSymbolCollision { .. })));
    }

    #[test]
    fn duplicate_public_names_inside_signatures_are_rejected() {
        let mut arguments = corpus_input();
        let observe = arguments
            .operations
            .iter_mut()
            .find(|planned| planned.operation.definition.public_name == "observe")
            .unwrap();
        observe.operation.definition.signature.arguments[1].public_name =
            observe.operation.definition.signature.arguments[0]
                .public_name
                .clone();
        assert!(BridgePlan::build(arguments).unwrap_err().contains(|error| {
            matches!(error, ValidationError::DuplicateScopedPublicName { scope, .. } if scope.contains("arguments"))
        }));

        let mut record_fields = corpus_input();
        let profile = record_fields
            .types
            .iter_mut()
            .find(|ty| ty.definition.public_name == "Profile")
            .unwrap();
        let NamedTypeKind::Record { fields } = &mut profile.definition.kind else {
            unreachable!();
        };
        fields[1].public_name = fields[0].public_name.clone();
        assert!(BridgePlan::build(record_fields).unwrap_err().contains(|error| {
            matches!(error, ValidationError::DuplicateScopedPublicName { scope, .. } if scope.contains("record"))
        }));

        let mut enum_variants = corpus_input();
        let event = enum_variants
            .types
            .iter_mut()
            .find(|ty| ty.definition.public_name == "Event")
            .unwrap();
        let NamedTypeKind::Enum { variants } = &mut event.definition.kind else {
            unreachable!();
        };
        variants.push(variants[0].clone());
        assert!(BridgePlan::build(enum_variants).unwrap_err().contains(|error| {
            matches!(error, ValidationError::DuplicateScopedPublicName { scope, .. } if scope.contains("variants"))
        }));

        let mut variant_fields = corpus_input();
        let failure = variant_fields
            .types
            .iter_mut()
            .find(|ty| ty.definition.public_name == "Failure")
            .unwrap();
        let NamedTypeKind::Error { variants } = &mut failure.definition.kind else {
            unreachable!();
        };
        let duplicate_field = variants[0].fields[0].clone();
        variants[0].fields.push(duplicate_field);
        assert!(BridgePlan::build(variant_fields).unwrap_err().contains(|error| {
            matches!(error, ValidationError::DuplicateScopedPublicName { scope, .. } if scope.contains("fields of variant"))
        }));
    }

    fn replace_observe_owner(input: &mut BridgePlanInput, owner: OperationOwner) {
        let observe = input
            .operations
            .iter_mut()
            .find(|planned| planned.operation.definition.public_name == "observe")
            .unwrap();
        let source = &observe.operation.definition.source_key;
        observe.operation.definition.source_key = OperationSourceKey::new(
            source.component().clone(),
            owner,
            source.kind(),
            source.name(),
        )
        .unwrap();
    }

    #[test]
    fn operation_owner_type_must_exist_match_component_and_kind() {
        let component = ComponentKey::new("contract_corpus").unwrap();

        let mut unknown = corpus_input();
        replace_observe_owner(
            &mut unknown,
            OperationOwner::Object(TypeSourceKey::new(component.clone(), "MissingObject").unwrap()),
        );
        assert!(BridgePlan::build(unknown)
            .unwrap_err()
            .contains(|error| matches!(error, ValidationError::UnknownOperationOwnerType { .. })));

        let mut wrong_kind = corpus_input();
        let callback = wrong_kind
            .types
            .iter()
            .find(|ty| matches!(ty.definition.kind, NamedTypeKind::Callback))
            .unwrap()
            .definition
            .source_key
            .clone();
        replace_observe_owner(&mut wrong_kind, OperationOwner::Object(callback));
        assert!(BridgePlan::build(wrong_kind)
            .unwrap_err()
            .contains(|error| matches!(error, ValidationError::OperationOwnerKindMismatch { .. })));

        let mut foreign = corpus_input();
        let foreign_component = ComponentKey::new("foreign_component").unwrap();
        let mut component_definition = foreign.components[0].clone();
        component_definition.id = ComponentId::new(foreign.components.len() as u32);
        component_definition.definition.source_key = foreign_component.clone();
        component_definition.definition.public_namespace = "foreignComponent".to_owned();
        foreign.components.push(component_definition);
        let foreign_key = TypeSourceKey::new(foreign_component, "ForeignObject").unwrap();
        let mut foreign_type = foreign
            .types
            .iter()
            .find(|ty| matches!(ty.definition.kind, NamedTypeKind::Object))
            .unwrap()
            .clone();
        foreign_type.id = TypeId::new(foreign.types.len() as u32);
        foreign_type.definition.source_key = foreign_key.clone();
        foreign_type.definition.public_name = "ForeignObject".to_owned();
        foreign.types.push(foreign_type);
        replace_observe_owner(&mut foreign, OperationOwner::Object(foreign_key));
        assert!(BridgePlan::build(foreign)
            .unwrap_err()
            .contains(|error| matches!(
                error,
                ValidationError::OperationOwnerComponentMismatch { .. }
            )));
    }

    #[test]
    fn cross_thread_sync_callback_is_rejected() {
        let mut input = corpus_input();
        input.callbacks[0].contract.threading = CallbackThreading::MayCrossThread;
        let report = BridgePlan::build(input).unwrap_err();
        assert!(report
            .contains(|error| matches!(error, ValidationError::CrossThreadSyncCallback { .. })));
    }

    #[test]
    fn cross_thread_async_callback_capability_comes_from_async_methods() {
        let mut input = corpus_input();
        input.callbacks[0].contract.threading = CallbackThreading::MayCrossThread;
        for planned in &mut input.operations {
            if matches!(
                planned.operation.definition.source_key.owner(),
                OperationOwner::Callback(_)
            ) {
                planned.operation.definition.signature.async_kind = AsyncKind::Async;
            }
        }
        let plan = BridgePlan::build(input).unwrap();
        let observe = plan
            .operations()
            .iter()
            .find(|operation| operation.operation.definition.public_name == "observe")
            .unwrap();
        assert!(observe
            .required_capabilities
            .contains(Capability::CrossThreadAsyncCallback));
    }

    #[test]
    fn callback_contract_is_required_for_every_signature_use_site() {
        let mut input = corpus_input();
        input.callbacks.clear();
        let report = BridgePlan::build(input).unwrap_err();
        assert!(report
            .contains(|error| matches!(error, ValidationError::MissingCallbackContract { .. })));
    }

    #[test]
    fn callback_use_sites_are_enumerated_through_named_type_fields() {
        let mut input = corpus_input();
        let callback = input
            .types
            .iter()
            .find(|ty| matches!(ty.definition.kind, NamedTypeKind::Callback))
            .unwrap()
            .clone();
        let profile = input
            .types
            .iter_mut()
            .find(|ty| ty.definition.public_name == "Profile")
            .unwrap();
        let NamedTypeKind::Record { fields } = &mut profile.definition.kind else {
            unreachable!();
        };
        fields.push(
            FieldDefinition::new(
                "nestedObserver",
                ValueType::Named(callback.definition.source_key.clone()),
            )
            .unwrap(),
        );

        let report = BridgePlan::build(input.clone()).unwrap_err();
        assert!(report.contains(|error| matches!(
            error,
            ValidationError::MissingCallbackContract { use_site, .. }
                if use_site == "argument[0].field[nestedObserver]"
        )));

        let mut nested_contract = input.callbacks[0].clone();
        nested_contract.path =
            ValuePath::argument(0).then(ValuePathSegment::Field("nestedObserver".to_owned()));
        input.callbacks.push(nested_contract);
        BridgePlan::build(input).unwrap();
    }

    #[test]
    fn duplicate_callback_contracts_are_rejected() {
        let mut input = corpus_input();
        input.callbacks.push(input.callbacks[0].clone());
        let report = BridgePlan::build(input).unwrap_err();
        assert!(report.contains(|error| matches!(
            error,
            ValidationError::DuplicateCallbackContract { count: 2, .. }
        )));
    }

    #[test]
    fn callback_contract_path_and_type_must_match_the_signature() {
        let mut wrong_path = corpus_input();
        wrong_path.callbacks[0].path = ValuePath::argument(0);
        let report = BridgePlan::build(wrong_path).unwrap_err();
        assert!(report
            .contains(|error| matches!(error, ValidationError::UnexpectedCallbackContract { .. })));
        assert!(report
            .contains(|error| matches!(error, ValidationError::MissingCallbackContract { .. })));

        let mut wrong_type = corpus_input();
        wrong_type.callbacks[0].callback_type = wrong_type
            .types
            .iter()
            .find(|ty| matches!(ty.definition.kind, NamedTypeKind::Object))
            .unwrap()
            .id;
        let report = BridgePlan::build(wrong_type).unwrap_err();
        assert!(
            report.contains(|error| matches!(error, ValidationError::CallbackTypeMismatch { .. }))
        );
    }

    #[test]
    fn mixed_callback_methods_derive_capabilities_from_each_signature() {
        let plan = BridgePlan::build(corpus_input()).unwrap();
        let observe = plan
            .operations()
            .iter()
            .find(|operation| operation.operation.definition.public_name == "observe")
            .unwrap();
        assert!(observe
            .required_capabilities
            .contains(Capability::AsyncCallback));
        assert!(observe
            .required_capabilities
            .contains(Capability::FallibleCallback));
    }

    #[test]
    fn stream_contract_is_required_once_at_each_real_signature_path() {
        let mut missing = corpus_input();
        missing.streams.clear();
        let report = BridgePlan::build(missing).unwrap_err();
        assert_eq!(
            report
                .errors()
                .iter()
                .filter(|error| matches!(error, ValidationError::MissingStreamContract { .. }))
                .count(),
            2
        );

        let mut duplicate = corpus_input();
        duplicate.streams.push(duplicate.streams[0].clone());
        let report = BridgePlan::build(duplicate).unwrap_err();
        assert!(report.contains(|error| matches!(
            error,
            ValidationError::DuplicateStreamContract { count: 2, .. }
        )));
    }

    #[test]
    fn stream_contract_path_and_direction_must_match_the_signature() {
        let mut wrong_path = corpus_input();
        wrong_path.streams[0].path =
            ValuePath::argument(0).then(ValuePathSegment::Field("missing".to_owned()));
        let report = BridgePlan::build(wrong_path).unwrap_err();
        assert!(report
            .contains(|error| matches!(error, ValidationError::UnexpectedStreamContract { .. })));
        assert!(
            report.contains(|error| matches!(error, ValidationError::MissingStreamContract { .. }))
        );

        let mut wrong_direction = corpus_input();
        wrong_direction.streams[0].contract = StreamContract::output();
        let report = BridgePlan::build(wrong_direction).unwrap_err();
        assert!(report.contains(|error| matches!(
            error,
            ValidationError::StreamDirectionMismatch {
                signature: StreamDirection::Input,
                contract: StreamDirection::Output,
                ..
            }
        )));
    }

    #[test]
    fn object_lease_is_generation_checked_and_released_once() {
        let mut lease = ObjectLease::new(7, TypeId::new(3), 42, 5);
        assert_eq!(lease.ensure_active(5), Ok(()));
        assert!(matches!(
            lease.ensure_active(4),
            Err(LeaseError::StaleGeneration { .. })
        ));
        assert_eq!(lease.release(5), Ok(ReleaseOutcome::Released));
        assert_eq!(lease.release(5), Ok(ReleaseOutcome::AlreadyReleased));
        assert_eq!(lease.ensure_active(5), Err(LeaseError::Released));
    }

    #[test]
    fn stream_pull_cancel_close_and_drain_are_exactly_once() {
        let mut input = StreamLifecycle::new(StreamDirection::Input);
        assert_eq!(input.phase(), StreamPhase::Idle);
        assert_eq!(input.claim_consumer(), Ok(()));
        assert_eq!(
            input.claim_consumer(),
            Err(StreamError::ConsumerAlreadyClaimed)
        );
        assert_eq!(input.begin_pull(), Ok(()));
        assert_eq!(input.begin_pull(), Err(StreamError::ConcurrentPull));
        let cancelled = input.cancel();
        assert!(cancelled.cleanup);
        assert!(cancelled.pending_pull_observes_done);
        assert!(cancelled.call_iterator_return);
        let repeated_while_pull_is_late = input.cancel();
        assert!(!repeated_while_pull_is_late.cleanup);
        assert!(!repeated_while_pull_is_late.pending_pull_observes_done);
        assert!(!repeated_while_pull_is_late.call_iterator_return);
        assert_eq!(
            input.complete_pull(PullCompletion::Item),
            Ok(PullOutcome::LateResultIgnored)
        );
        let repeated_cancel = input.cancel();
        assert!(!repeated_cancel.cleanup);
        assert!(!repeated_cancel.pending_pull_observes_done);
        assert!(!repeated_cancel.call_iterator_return);
        let drained_after_cancel = input.drain();
        assert!(!drained_after_cancel.cleanup);
        assert!(!drained_after_cancel.call_iterator_return);

        let mut idle_input = StreamLifecycle::new(StreamDirection::Input);
        idle_input.claim_consumer().unwrap();
        let first_cancel = idle_input.cancel();
        assert!(first_cancel.cleanup);
        assert!(first_cancel.call_iterator_return);
        assert!(!first_cancel.pending_pull_observes_done);
        let second_cancel = idle_input.cancel();
        assert!(!second_cancel.cleanup);
        assert!(!second_cancel.call_iterator_return);
        assert!(!idle_input.drain().call_iterator_return);

        let mut output = StreamLifecycle::new(StreamDirection::Output);
        output.claim_consumer().unwrap();
        output.begin_pull().unwrap();
        assert_eq!(
            output.complete_pull(PullCompletion::Item),
            Ok(PullOutcome::ItemAccepted)
        );
        output.begin_pull().unwrap();
        assert_eq!(
            output.complete_pull(PullCompletion::Done),
            Ok(PullOutcome::Closed { cleanup: true })
        );
        assert!(!output.drain().cleanup);

        let optional_null_item: RawStreamStep<Option<String>, ()> = RawStreamStep::Item(None);
        assert_ne!(optional_null_item, RawStreamStep::Done);
    }

    #[test]
    fn session_close_rejects_new_calls_and_is_idempotent() {
        let mut session = SessionLifecycle::default();
        assert!(session.accepts_calls());
        assert!(session.begin_close());
        assert!(!session.accepts_calls());
        assert!(!session.begin_close());
        session.finish_close();
        assert_eq!(session.phase(), SessionPhase::Closed);
    }
}
