//! Reusable engine-neutral contract corpus.
//!
//! Later facade and engine stages consume this same corpus instead of adding
//! target-specific lookalikes.

use uniffi_js_abi::{
    assign_component_ids, assign_operation_ids, assign_type_ids, ArgumentDefinition, AsyncKind,
    ComponentDefinition, ComponentKey, EnumVariant, FieldDefinition, NamedTypeKind,
    OperationDefinition, OperationKind, OperationOwner, OperationSignature, OperationSourceKey,
    Ownership, ScalarType, TypeDefinition, TypeSourceKey, ValueType,
};
use uniffi_js_engine_schema::{
    BridgePlan, BridgePlanInput, CallbackContract, CallbackReentrancy, CallbackRetention,
    CallbackThreading, CallbackUseSite, Capability, CapabilitySet, EngineCapabilities, EngineKind,
    PlannedOperation, StreamContract, StreamUseSite, ValuePath,
};

pub fn unified_contract_corpus(reverse_discovery_order: bool) -> BridgePlan {
    let component_key = ComponentKey::new("unified_contract_corpus").unwrap();
    let components = assign_component_ids([ComponentDefinition::new(
        component_key.clone(),
        "unifiedContractCorpus",
    )
    .unwrap()])
    .unwrap();

    let record_key = TypeSourceKey::new(component_key.clone(), "Payload").unwrap();
    let enum_key = TypeSourceKey::new(component_key.clone(), "Event").unwrap();
    let error_key = TypeSourceKey::new(component_key.clone(), "CorpusError").unwrap();
    let object_key = TypeSourceKey::new(component_key.clone(), "CorpusObject").unwrap();
    let callback_key = TypeSourceKey::new(component_key.clone(), "CorpusCallback").unwrap();
    let mut type_definitions = vec![
        TypeDefinition::new(
            record_key.clone(),
            "Payload",
            NamedTypeKind::Record {
                fields: vec![
                    FieldDefinition::new("flag", ValueType::Scalar(ScalarType::Bool)).unwrap(),
                    FieldDefinition::new("text", ValueType::Scalar(ScalarType::String)).unwrap(),
                    FieldDefinition::new("bytes", ValueType::Scalar(ScalarType::Bytes)).unwrap(),
                    FieldDefinition::new("wide", ValueType::Scalar(ScalarType::I64)).unwrap(),
                    FieldDefinition::new(
                        "optionalText",
                        ValueType::optional(ValueType::Scalar(ScalarType::String)),
                    )
                    .unwrap(),
                    FieldDefinition::new(
                        "lookup",
                        ValueType::map(
                            ValueType::Scalar(ScalarType::String),
                            ValueType::Scalar(ScalarType::U64),
                        ),
                    )
                    .unwrap(),
                    FieldDefinition::new(
                        "labels",
                        ValueType::set(ValueType::Scalar(ScalarType::String)),
                    )
                    .unwrap(),
                ],
            },
        )
        .unwrap(),
        TypeDefinition::new(
            enum_key.clone(),
            "Event",
            NamedTypeKind::Enum {
                variants: vec![
                    EnumVariant::new("Ready", vec![]).unwrap(),
                    EnumVariant::new(
                        "Data",
                        vec![
                            FieldDefinition::new("payload", ValueType::Named(record_key.clone()))
                                .unwrap(),
                        ],
                    )
                    .unwrap(),
                ],
            },
        )
        .unwrap(),
        TypeDefinition::new(
            error_key.clone(),
            "CorpusError",
            NamedTypeKind::Error {
                variants: vec![EnumVariant::new(
                    "Rejected",
                    vec![
                        FieldDefinition::new("reason", ValueType::Scalar(ScalarType::String))
                            .unwrap(),
                    ],
                )
                .unwrap()],
            },
        )
        .unwrap(),
        TypeDefinition::new(object_key.clone(), "CorpusObject", NamedTypeKind::Object).unwrap(),
        TypeDefinition::new(
            callback_key.clone(),
            "CorpusCallback",
            NamedTypeKind::Callback,
        )
        .unwrap(),
    ];
    if reverse_discovery_order {
        type_definitions.reverse();
    }
    let types = assign_type_ids(type_definitions).unwrap();
    let callback_type = types
        .iter()
        .find(|ty| ty.definition.source_key == callback_key)
        .unwrap()
        .id;

    let mut operation_definitions = vec![
        OperationDefinition::new(
            OperationSourceKey::new(
                component_key.clone(),
                OperationOwner::Namespace,
                OperationKind::Function,
                "run_sync",
            )
            .unwrap(),
            "runSync",
            "unified_contract_corpus.runSync",
            "__uniffi_unified_contract_corpus_run_sync",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "payload",
                    ValueType::Named(record_key.clone()),
                    Ownership::Owned,
                )
                .unwrap()],
                return_type: Some(ValueType::Named(enum_key.clone())),
                async_kind: AsyncKind::Sync,
                throws: Some(error_key.clone()),
            },
        )
        .unwrap(),
        OperationDefinition::new(
            OperationSourceKey::new(
                component_key.clone(),
                OperationOwner::Namespace,
                OperationKind::Function,
                "run_async",
            )
            .unwrap(),
            "runAsync",
            "unified_contract_corpus.runAsync",
            "__uniffi_unified_contract_corpus_run_async",
            OperationSignature {
                arguments: vec![
                    ArgumentDefinition::new(
                        "callback",
                        ValueType::Named(callback_key.clone()),
                        Ownership::Borrowed,
                    )
                    .unwrap(),
                    ArgumentDefinition::new(
                        "input",
                        ValueType::input_stream(ValueType::Named(record_key)),
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
                OperationKind::CallbackMethod,
                "on_ready",
            )
            .unwrap(),
            "onReady",
            "unified_contract_corpus.CorpusCallback.onReady",
            "__uniffi_unified_contract_corpus_callback_on_ready",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "event",
                    ValueType::Named(enum_key.clone()),
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
                OperationKind::CallbackMethod,
                "on_ready_checked",
            )
            .unwrap(),
            "onReadyChecked",
            "unified_contract_corpus.CorpusCallback.onReadyChecked",
            "__uniffi_unified_contract_corpus_callback_on_ready_checked",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "event",
                    ValueType::Named(enum_key.clone()),
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
                OperationKind::CallbackMethod,
                "on_event",
            )
            .unwrap(),
            "onEvent",
            "unified_contract_corpus.CorpusCallback.onEvent",
            "__uniffi_unified_contract_corpus_callback_on_event",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "event",
                    ValueType::Named(enum_key.clone()),
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
                OperationOwner::Callback(callback_key),
                OperationKind::CallbackMethod,
                "on_event_checked",
            )
            .unwrap(),
            "onEventChecked",
            "unified_contract_corpus.CorpusCallback.onEventChecked",
            "__uniffi_unified_contract_corpus_callback_on_event_checked",
            OperationSignature {
                arguments: vec![ArgumentDefinition::new(
                    "event",
                    ValueType::Named(enum_key.clone()),
                    Ownership::Borrowed,
                )
                .unwrap()],
                return_type: None,
                async_kind: AsyncKind::Async,
                throws: Some(error_key),
            },
        )
        .unwrap(),
        OperationDefinition::new(
            OperationSourceKey::new(
                component_key,
                OperationOwner::Namespace,
                OperationKind::OutputStreamStart,
                "events",
            )
            .unwrap(),
            "events",
            "unified_contract_corpus.events",
            "__uniffi_unified_contract_corpus_events",
            OperationSignature {
                arguments: vec![],
                return_type: Some(ValueType::output_stream(ValueType::Named(enum_key))),
                async_kind: AsyncKind::Sync,
                throws: None,
            },
        )
        .unwrap(),
    ];
    if reverse_discovery_order {
        operation_definitions.reverse();
    }
    let operations = assign_operation_ids(operation_definitions).unwrap();
    let run_async = operations
        .iter()
        .find(|operation| operation.definition.public_name == "runAsync")
        .unwrap()
        .id;
    let events = operations
        .iter()
        .find(|operation| operation.definition.public_name == "events")
        .unwrap()
        .id;

    let full_capabilities = CapabilitySet::new([
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
    ]);

    BridgePlan::build(BridgePlanInput {
        components,
        types,
        operations: operations.into_iter().map(PlannedOperation::new).collect(),
        callbacks: vec![CallbackUseSite {
            operation_id: run_async,
            callback_type,
            path: ValuePath::argument(0),
            contract: CallbackContract {
                retention: CallbackRetention::Retained,
                threading: CallbackThreading::CallingThread,
                reentrancy: CallbackReentrancy::Allowed,
            },
        }],
        streams: vec![
            StreamUseSite {
                operation_id: run_async,
                path: ValuePath::argument(1),
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
            supported: full_capabilities.clone(),
        })
        .collect(),
    })
    .unwrap()
}
