/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Engine-neutral JavaScript API contract used by UniFFI generators.
//!
//! This crate deliberately contains no `ComponentInterface`, renderer,
//! filesystem, serialization, or engine dependencies.  IDs are dense indices
//! assigned after sorting complete semantic source keys.  They are stable for
//! equivalent input regardless of discovery order, but are intentionally not
//! persisted artifact identities.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

macro_rules! dense_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u32);

        impl $name {
            pub const fn new(index: u32) -> Self {
                Self(index)
            }

            pub const fn index(self) -> u32 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

dense_id!(ComponentId);
dense_id!(TypeId);
dense_id!(OperationId);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum PublicTarget {
    NodeNapi,
    BrowserWasm,
    OhosNapi,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublicOutputLayout {
    pub implementation_suffix: &'static str,
    pub declaration_suffix: &'static str,
    /// Node and Web deliberately share this source family.  OHOS uses the
    /// same normalized API contract but an ArkTS policy printer.
    pub source_family: PublicSourceFamily,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicSourceFamily {
    SharedEcmaScript,
    ArkTs,
}

impl PublicTarget {
    pub const fn output_layout(self) -> PublicOutputLayout {
        match self {
            Self::NodeNapi | Self::BrowserWasm => PublicOutputLayout {
                implementation_suffix: ".js",
                declaration_suffix: ".d.ts",
                source_family: PublicSourceFamily::SharedEcmaScript,
            },
            Self::OhosNapi => PublicOutputLayout {
                implementation_suffix: ".ets",
                declaration_suffix: ".d.ets",
                source_family: PublicSourceFamily::ArkTs,
            },
        }
    }
}

/// The normalized source identity of one UniFFI component.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ComponentKey {
    namespace: String,
}

impl ComponentKey {
    pub fn new(namespace: impl Into<String>) -> Result<Self, ContractError> {
        Ok(Self {
            namespace: checked_name("component namespace", namespace.into())?,
        })
    }

    pub fn namespace(&self) -> &str {
        &self.namespace
    }
}

impl fmt::Display for ComponentKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.namespace)
    }
}

/// The normalized source identity of one named UniFFI type.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TypeSourceKey {
    component: ComponentKey,
    name: String,
}

impl TypeSourceKey {
    pub fn new(component: ComponentKey, name: impl Into<String>) -> Result<Self, ContractError> {
        Ok(Self {
            component,
            name: checked_name("type source name", name.into())?,
        })
    }

    pub fn component(&self) -> &ComponentKey {
        &self.component
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for TypeSourceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}::{}", self.component, self.name)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OperationOwner {
    Namespace,
    Object(TypeSourceKey),
    Callback(TypeSourceKey),
}

impl fmt::Display for OperationOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Namespace => formatter.write_str("namespace"),
            Self::Object(key) => write!(formatter, "object:{key}"),
            Self::Callback(key) => write!(formatter, "callback:{key}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum OperationKind {
    Function,
    Constructor,
    Method,
    CallbackMethod,
    OutputStreamStart,
    OutputStreamNext,
    OutputStreamCancel,
    InputStreamPull,
    InputStreamCancel,
}

/// The source location of an operation before dense IDs are allocated.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OperationSourceKey {
    component: ComponentKey,
    owner: OperationOwner,
    kind: OperationKind,
    name: String,
}

impl OperationSourceKey {
    pub fn new(
        component: ComponentKey,
        owner: OperationOwner,
        kind: OperationKind,
        name: impl Into<String>,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            component,
            owner,
            kind,
            name: checked_name("operation source name", name.into())?,
        })
    }

    pub fn component(&self) -> &ComponentKey {
        &self.component
    }

    pub fn owner(&self) -> &OperationOwner {
        &self.owner
    }

    pub const fn kind(&self) -> OperationKind {
        self.kind
    }

    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for OperationSourceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}::{}::{:?}::{}",
            self.component, self.owner, self.kind, self.name
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ScalarType {
    Bool,
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    I64,
    U64,
    F32,
    F64,
    String,
    Bytes,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicScalarShape {
    Boolean,
    Number,
    BigInt,
    String,
    Uint8Array,
}

impl ScalarType {
    pub const fn public_shape(self) -> PublicScalarShape {
        match self {
            Self::Bool => PublicScalarShape::Boolean,
            Self::I8
            | Self::U8
            | Self::I16
            | Self::U16
            | Self::I32
            | Self::U32
            | Self::F32
            | Self::F64 => PublicScalarShape::Number,
            Self::I64 | Self::U64 => PublicScalarShape::BigInt,
            Self::String => PublicScalarShape::String,
            Self::Bytes => PublicScalarShape::Uint8Array,
        }
    }
}

/// The public JavaScript value shape.  Named types refer to semantic source
/// keys so the graph can be built before `TypeId` allocation.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ValueType {
    Scalar(ScalarType),
    Named(TypeSourceKey),
    Optional(Box<ValueType>),
    Sequence(Box<ValueType>),
    Map(Box<ValueType>, Box<ValueType>),
    Set(Box<ValueType>),
    /// A structural JavaScript async iterable consumed by native code.
    InputStream(Box<ValueType>),
    /// A pull-based JavaScript async iterable produced by native code.
    OutputStream(Box<ValueType>),
}

impl ValueType {
    pub fn optional(inner: Self) -> Self {
        Self::Optional(Box::new(inner))
    }

    pub fn sequence(inner: Self) -> Self {
        Self::Sequence(Box::new(inner))
    }

    pub fn map(key: Self, value: Self) -> Self {
        Self::Map(Box::new(key), Box::new(value))
    }

    pub fn set(inner: Self) -> Self {
        Self::Set(Box::new(inner))
    }

    pub fn input_stream(item: Self) -> Self {
        Self::InputStream(Box::new(item))
    }

    pub fn output_stream(item: Self) -> Self {
        Self::OutputStream(Box::new(item))
    }

    /// Validate only presence/nullability.  Concrete value conversion remains
    /// the generated facade's responsibility.
    pub fn validate_presence(&self, presence: InputPresence) -> Result<(), PresenceError> {
        match presence {
            InputPresence::Present => Ok(()),
            InputPresence::Null if matches!(self, Self::Optional(_)) => Ok(()),
            InputPresence::Null => Err(PresenceError::NullForRequired),
            InputPresence::Undefined => Err(PresenceError::Undefined),
        }
    }

    pub fn visit(&self, visitor: &mut impl FnMut(&ValueType)) {
        visitor(self);
        match self {
            Self::Optional(inner)
            | Self::Sequence(inner)
            | Self::Set(inner)
            | Self::InputStream(inner)
            | Self::OutputStream(inner) => inner.visit(visitor),
            Self::Map(key, value) => {
                key.visit(visitor);
                value.visit(visitor);
            }
            Self::Scalar(_) | Self::Named(_) => {}
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputPresence {
    Present,
    Null,
    Undefined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresenceError {
    NullForRequired,
    Undefined,
}

impl fmt::Display for PresenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NullForRequired => formatter.write_str("null is not valid for a required value"),
            Self::Undefined => formatter
                .write_str("undefined is not a UniFFI value; use null for an optional value"),
        }
    }
}

impl Error for PresenceError {}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Ownership {
    Borrowed,
    Owned,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct FieldDefinition {
    pub public_name: String,
    pub ty: ValueType,
}

impl FieldDefinition {
    pub fn new(public_name: impl Into<String>, ty: ValueType) -> Result<Self, ContractError> {
        Ok(Self {
            public_name: checked_name("record field", public_name.into())?,
            ty,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EnumVariant {
    pub public_name: String,
    pub fields: Vec<FieldDefinition>,
}

impl EnumVariant {
    pub fn new(
        public_name: impl Into<String>,
        fields: Vec<FieldDefinition>,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            public_name: checked_name("enum variant", public_name.into())?,
            fields,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum NamedTypeKind {
    Record { fields: Vec<FieldDefinition> },
    Enum { variants: Vec<EnumVariant> },
    Error { variants: Vec<EnumVariant> },
    Object,
    Callback,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ComponentDefinition {
    pub source_key: ComponentKey,
    pub public_namespace: String,
}

impl ComponentDefinition {
    pub fn new(
        source_key: ComponentKey,
        public_namespace: impl Into<String>,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            source_key,
            public_namespace: checked_name("public component namespace", public_namespace.into())?,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TypeDefinition {
    pub source_key: TypeSourceKey,
    pub public_name: String,
    pub kind: NamedTypeKind,
}

impl TypeDefinition {
    pub fn new(
        source_key: TypeSourceKey,
        public_name: impl Into<String>,
        kind: NamedTypeKind,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            source_key,
            public_name: checked_name("public type name", public_name.into())?,
            kind,
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct ArgumentDefinition {
    pub public_name: String,
    pub ty: ValueType,
    pub ownership: Ownership,
}

impl ArgumentDefinition {
    pub fn new(
        public_name: impl Into<String>,
        ty: ValueType,
        ownership: Ownership,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            public_name: checked_name("operation argument", public_name.into())?,
            ty,
            ownership,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum AsyncKind {
    Sync,
    Async,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OperationSignature {
    pub arguments: Vec<ArgumentDefinition>,
    pub return_type: Option<ValueType>,
    pub async_kind: AsyncKind,
    pub throws: Option<TypeSourceKey>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct OperationDefinition {
    pub source_key: OperationSourceKey,
    pub public_name: String,
    pub debug_name: String,
    pub private_symbol: String,
    pub signature: OperationSignature,
}

impl OperationDefinition {
    pub fn new(
        source_key: OperationSourceKey,
        public_name: impl Into<String>,
        debug_name: impl Into<String>,
        private_symbol: impl Into<String>,
        signature: OperationSignature,
    ) -> Result<Self, ContractError> {
        Ok(Self {
            source_key,
            public_name: checked_name("public operation name", public_name.into())?,
            debug_name: checked_name("operation debug name", debug_name.into())?,
            private_symbol: checked_name("private operation symbol", private_symbol.into())?,
            signature,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentifiedComponent {
    pub id: ComponentId,
    pub definition: ComponentDefinition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentifiedType {
    pub id: TypeId,
    pub definition: TypeDefinition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdentifiedOperation {
    pub id: OperationId,
    pub definition: OperationDefinition,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContractError {
    InvalidName {
        role: &'static str,
        value: String,
        reason: &'static str,
    },
    DuplicateSourceKey {
        kind: &'static str,
        key: String,
    },
    ConflictingSourceKey {
        kind: &'static str,
        key: String,
    },
    TooManyDefinitions {
        kind: &'static str,
    },
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName {
                role,
                value,
                reason,
            } => write!(formatter, "invalid {role} {value:?}: {reason}"),
            Self::DuplicateSourceKey { kind, key } => {
                write!(formatter, "duplicate {kind} source key {key}")
            }
            Self::ConflictingSourceKey { kind, key } => {
                write!(
                    formatter,
                    "conflicting definitions for {kind} source key {key}"
                )
            }
            Self::TooManyDefinitions { kind } => {
                write!(
                    formatter,
                    "too many {kind} definitions for a dense u32 ID table"
                )
            }
        }
    }
}

impl Error for ContractError {}

pub fn assign_component_ids(
    definitions: impl IntoIterator<Item = ComponentDefinition>,
) -> Result<Vec<IdentifiedComponent>, ContractError> {
    assign_dense(
        "component",
        definitions,
        |definition| definition.source_key.clone(),
        |key| key.to_string(),
        |id, definition| IdentifiedComponent {
            id: ComponentId::new(id),
            definition,
        },
    )
}

pub fn assign_type_ids(
    definitions: impl IntoIterator<Item = TypeDefinition>,
) -> Result<Vec<IdentifiedType>, ContractError> {
    assign_dense(
        "type",
        definitions,
        |definition| definition.source_key.clone(),
        |key| key.to_string(),
        |id, definition| IdentifiedType {
            id: TypeId::new(id),
            definition,
        },
    )
}

pub fn assign_operation_ids(
    definitions: impl IntoIterator<Item = OperationDefinition>,
) -> Result<Vec<IdentifiedOperation>, ContractError> {
    assign_dense(
        "operation",
        definitions,
        |definition| definition.source_key.clone(),
        |key| key.to_string(),
        |id, definition| IdentifiedOperation {
            id: OperationId::new(id),
            definition,
        },
    )
}

fn assign_dense<D, K, I>(
    kind: &'static str,
    definitions: impl IntoIterator<Item = D>,
    source_key: impl Fn(&D) -> K,
    display_key: impl Fn(&K) -> String,
    identify: impl Fn(u32, D) -> I,
) -> Result<Vec<I>, ContractError>
where
    D: Clone + Eq,
    K: Clone + Ord,
{
    let mut sorted = BTreeMap::<K, D>::new();
    for definition in definitions {
        let key = source_key(&definition);
        if let Some(existing) = sorted.get(&key) {
            return Err(if existing == &definition {
                ContractError::DuplicateSourceKey {
                    kind,
                    key: display_key(&key),
                }
            } else {
                ContractError::ConflictingSourceKey {
                    kind,
                    key: display_key(&key),
                }
            });
        }
        sorted.insert(key, definition);
    }

    sorted
        .into_values()
        .enumerate()
        .map(|(index, definition)| {
            let index =
                u32::try_from(index).map_err(|_| ContractError::TooManyDefinitions { kind })?;
            Ok(identify(index, definition))
        })
        .collect()
}

fn checked_name(role: &'static str, value: String) -> Result<String, ContractError> {
    if value.is_empty() {
        return Err(ContractError::InvalidName {
            role,
            value,
            reason: "must not be empty",
        });
    }
    if value.trim() != value {
        return Err(ContractError::InvalidName {
            role,
            value,
            reason: "must already be whitespace-normalized",
        });
    }
    if value.chars().any(char::is_control) {
        return Err(ContractError::InvalidName {
            role,
            value,
            reason: "must not contain control characters",
        });
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn component(name: &str, public: &str) -> ComponentDefinition {
        ComponentDefinition::new(ComponentKey::new(name).unwrap(), public).unwrap()
    }

    fn operation(component: &str, source_name: &str, public_name: &str) -> OperationDefinition {
        let component = ComponentKey::new(component).unwrap();
        OperationDefinition::new(
            OperationSourceKey::new(
                component,
                OperationOwner::Namespace,
                OperationKind::Function,
                source_name,
            )
            .unwrap(),
            public_name,
            format!("debug_{source_name}"),
            format!("__uniffi_{source_name}"),
            OperationSignature {
                arguments: vec![],
                return_type: None,
                async_kind: AsyncKind::Sync,
                throws: None,
            },
        )
        .unwrap()
    }

    #[test]
    fn dense_ids_are_independent_of_discovery_order() {
        let forward =
            assign_component_ids([component("zeta", "zeta"), component("alpha", "alpha")]).unwrap();
        let reverse =
            assign_component_ids([component("alpha", "alpha"), component("zeta", "zeta")]).unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(forward[0].id, ComponentId::new(0));
        assert_eq!(forward[0].definition.source_key.namespace(), "alpha");

        let forward = assign_operation_ids([
            operation("core", "zeta", "zeta"),
            operation("core", "alpha", "alpha"),
        ])
        .unwrap();
        let reverse = assign_operation_ids([
            operation("core", "alpha", "alpha"),
            operation("core", "zeta", "zeta"),
        ])
        .unwrap();
        assert_eq!(forward, reverse);
        assert_eq!(forward[0].id, OperationId::new(0));
        assert_eq!(forward[0].definition.source_key.name(), "alpha");
    }

    #[test]
    fn duplicate_and_conflicting_source_keys_are_errors() {
        let duplicate = component("core", "core");
        assert!(matches!(
            assign_component_ids([duplicate.clone(), duplicate]),
            Err(ContractError::DuplicateSourceKey { .. })
        ));

        assert!(matches!(
            assign_component_ids([component("core", "core"), component("core", "renamed")]),
            Err(ContractError::ConflictingSourceKey { .. })
        ));
    }

    #[test]
    fn public_scalar_shapes_preserve_bigint_and_bytes() {
        assert_eq!(ScalarType::I64.public_shape(), PublicScalarShape::BigInt);
        assert_eq!(ScalarType::U64.public_shape(), PublicScalarShape::BigInt);
        assert_eq!(
            ScalarType::Bytes.public_shape(),
            PublicScalarShape::Uint8Array
        );
        assert_eq!(ScalarType::String.public_shape(), PublicScalarShape::String);
    }

    #[test]
    fn output_layout_uses_shared_js_and_arkts_suffixes() {
        let node = PublicTarget::NodeNapi.output_layout();
        let web = PublicTarget::BrowserWasm.output_layout();
        let ohos = PublicTarget::OhosNapi.output_layout();
        assert_eq!(node, web);
        assert_eq!(node.implementation_suffix, ".js");
        assert_eq!(node.declaration_suffix, ".d.ts");
        assert_eq!(ohos.implementation_suffix, ".ets");
        assert_eq!(ohos.declaration_suffix, ".d.ets");
    }

    #[test]
    fn optional_accepts_null_but_never_undefined() {
        let required = ValueType::Scalar(ScalarType::String);
        let optional = ValueType::optional(required.clone());
        assert_eq!(
            required.validate_presence(InputPresence::Null),
            Err(PresenceError::NullForRequired)
        );
        assert_eq!(optional.validate_presence(InputPresence::Null), Ok(()));
        assert_eq!(
            optional.validate_presence(InputPresence::Undefined),
            Err(PresenceError::Undefined)
        );
    }

    #[test]
    fn input_and_output_streams_remain_distinct_in_signatures() {
        let item = ValueType::Scalar(ScalarType::Bytes);
        let input = ValueType::input_stream(item.clone());
        let output = ValueType::output_stream(item);
        assert!(matches!(input, ValueType::InputStream(_)));
        assert!(matches!(output, ValueType::OutputStream(_)));
        assert_ne!(input, output);
    }

    #[test]
    fn type_corpus_retains_record_enum_error_object_map_and_set() {
        let component = ComponentKey::new("corpus").unwrap();
        let profile = TypeSourceKey::new(component.clone(), "Profile").unwrap();
        let event = TypeSourceKey::new(component.clone(), "Event").unwrap();
        let failure = TypeSourceKey::new(component.clone(), "Failure").unwrap();
        let service = TypeSourceKey::new(component.clone(), "Service").unwrap();
        let types = assign_type_ids([
            TypeDefinition::new(
                profile,
                "Profile",
                NamedTypeKind::Record {
                    fields: vec![
                        FieldDefinition::new(
                            "tags",
                            ValueType::set(ValueType::Scalar(ScalarType::String)),
                        )
                        .unwrap(),
                        FieldDefinition::new(
                            "scores",
                            ValueType::map(
                                ValueType::Scalar(ScalarType::String),
                                ValueType::Scalar(ScalarType::I64),
                            ),
                        )
                        .unwrap(),
                    ],
                },
            )
            .unwrap(),
            TypeDefinition::new(
                event,
                "Event",
                NamedTypeKind::Enum {
                    variants: vec![EnumVariant::new("Ready", vec![]).unwrap()],
                },
            )
            .unwrap(),
            TypeDefinition::new(
                failure,
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
            TypeDefinition::new(service, "Service", NamedTypeKind::Object).unwrap(),
        ])
        .unwrap();

        assert_eq!(types.len(), 4);
        assert_eq!(types[0].id, TypeId::new(0));
    }
}
