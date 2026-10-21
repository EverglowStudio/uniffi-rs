//! Regression coverage for recursive DTOs through the production frontend and Ark printer.

use super::*;
use uniffi_meta::CallbackValuePathSegment;

const CRATE: &str = "recursive_dto";

fn record_type(name: &str) -> Type {
    Type::Record {
        module_path: CRATE.into(),
        name: name.into(),
    }
}

fn body_value_type() -> Type {
    Type::Enum {
        module_path: CRATE.into(),
        name: "ItemBodyValue".into(),
    }
}

fn optional(ty: Type) -> Type {
    Type::Optional {
        inner_type: Box::new(ty),
    }
}

fn sequence(ty: Type) -> Type {
    Type::Sequence {
        inner_type: Box::new(ty),
    }
}

fn map(ty: Type) -> Type {
    Type::Map {
        key_type: Box::new(Type::String),
        value_type: Box::new(ty),
    }
}

fn field(name: &str, ty: Type) -> FieldMetadata {
    FieldMetadata {
        name: name.into(),
        orig_name: None,
        ty,
        default: None,
        docstring: None,
    }
}

fn function(name: &str, inputs: Vec<Type>, return_type: Option<Type>) -> FnMetadata {
    FnMetadata {
        module_path: CRATE.into(),
        name: name.into(),
        orig_name: None,
        is_async: false,
        rust_async: false,
        rust_return_is_result: false,
        rust_stream_item_is_result: false,
        inputs: inputs
            .into_iter()
            .enumerate()
            .map(|(index, ty)| FnParamMetadata::simple(&format!("value{index}"), ty))
            .collect(),
        return_type,
        throws: None,
        checksum: None,
        docstring: None,
    }
}

fn component() -> Component<JsConfig> {
    let mut group = MetadataGroup {
        namespace: NamespaceMetadata {
            crate_name: CRATE.into(),
            name: CRATE.into(),
        },
        namespace_docstring: None,
        items: Default::default(),
    };
    // This is the exported demo S02 shape, including both recursive containers.
    group.add_item(
        EnumMetadata {
            module_path: CRATE.into(),
            name: "ItemBodyValue".into(),
            orig_name: None,
            rust_path: None,
            shape: EnumShape::Enum,
            remote: false,
            variants: [
                ("Null", vec![]),
                ("Bool", vec![field("value", Type::Boolean)]),
                ("Integer", vec![field("value", Type::Int64)]),
                ("Text", vec![field("value", Type::String)]),
                ("Map", vec![field("entries", map(body_value_type()))]),
                ("List", vec![field("values", sequence(body_value_type()))]),
            ]
            .into_iter()
            .map(|(name, fields)| VariantMetadata {
                name: name.into(),
                orig_name: None,
                discr: None,
                fields,
                docstring: None,
            })
            .collect(),
            discr_type: None,
            non_exhaustive: false,
            docstring: None,
        }
        .into(),
    );
    let listener = Type::CallbackInterface {
        module_path: CRATE.into(),
        name: "Listener".into(),
    };
    for (name, fields) in [
        (
            "ItemBodySnapshot",
            vec![
                field("text", optional(Type::String)),
                field("structured", optional(body_value_type())),
            ],
        ),
        (
            "BranchA",
            vec![field("next", optional(record_type("BranchB")))],
        ),
        (
            "BranchB",
            vec![
                field("children", sequence(record_type("BranchA"))),
                field("by_name", map(sequence(optional(record_type("BranchA"))))),
            ],
        ),
        (
            "CallbackNode",
            vec![
                field("listener", optional(listener)),
                field("next", optional(record_type("CallbackNode"))),
            ],
        ),
    ] {
        group.add_item(
            RecordMetadata {
                module_path: CRATE.into(),
                name: name.into(),
                orig_name: None,
                rust_path: None,
                remote: false,
                fields,
                docstring: None,
            }
            .into(),
        );
    }
    group.add_item(
        CallbackInterfaceMetadata {
            module_path: CRATE.into(),
            name: "Listener".into(),
            docstring: None,
        }
        .into(),
    );
    group.add_item(
        TraitMethodMetadata {
            module_path: CRATE.into(),
            trait_name: "Listener".into(),
            index: 0,
            name: "value".into(),
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
    for (name, ty) in [
        ("echo_value", body_value_type()),
        ("echo_snapshot", record_type("ItemBodySnapshot")),
        ("echo_branch", record_type("BranchA")),
    ] {
        group.add_item(function(name, vec![ty.clone()], Some(ty)).into());
    }
    for (name, retained_first) in [("bind_callbacks", false), ("bind_reversed", true)] {
        group.add_item(function(name, vec![record_type("CallbackNode"); 2], None).into());
        for index in 0..2 {
            group.add_item(
                CallbackUseSiteMetadata {
                    module_path: CRATE.into(),
                    operation_kind: CallbackOperationKind::Function,
                    owner: None,
                    operation_name: name.into(),
                    path: CallbackValuePath::argument(index)
                        .then(CallbackValuePathSegment::Field("listener".into())),
                    contract: MetaCallbackContract {
                        retention: if (index == 0) == retained_first {
                            MetaCallbackRetention::Retained
                        } else {
                            MetaCallbackRetention::Scoped
                        },
                        threading: MetaCallbackThreading::CallingThread,
                        reentrancy: MetaCallbackReentrancy::Forbidden,
                    },
                }
                .into(),
            );
        }
    }
    group.add_item(
        function(
            "snapshots",
            vec![Type::InputStream {
                item_type: Box::new(record_type("ItemBodySnapshot")),
                error_type: None,
                is_send: true,
            }],
            Some(Type::Stream {
                item_type: Box::new(record_type("ItemBodySnapshot")),
                error_type: None,
                is_send: true,
            }),
        )
        .into(),
    );
    Component {
        ci: ComponentInterface::from_metadata(group).unwrap(),
        config: JsConfig::default(),
    }
}

fn facade() -> PublicFacade {
    let normalized =
        normalize(BindingInput::new(&[component()]).with_build_targets([PublicTarget::NodeNapi]))
            .unwrap();
    assert_eq!(normalized.build_targets, vec![PublicTarget::NodeNapi]);
    build(&normalized.api, &normalized.bridge, &normalized.rust).unwrap()
}

#[test]
fn recursive_metadata_builds_both_facades_for_node() {
    let facade = facade();
    assert!(facade.ast.target_universe.contains(&PublicTarget::NodeNapi));
    assert!(facade
        .shared_files()
        .iter()
        .any(|file| file.path.ends_with("index.js")));
    assert_eq!(
        facade
            .ark_files()
            .iter()
            .map(|file| file.path.as_str())
            .collect::<Vec<_>>(),
        ["platform.ets", "platform.d.ets", "Index.ets", "Index.d.ets"]
    );
    assert_eq!(
        render_ark_inventory(&facade.ast).unwrap(),
        facade.ark_files()
    );
    let source = String::from_utf8(facade.ark_files()[0].bytes.clone()).unwrap();
    assert!(source.contains("Map<string, ItemBodyValue>"));
    assert!(source.contains("Array<ItemBodyValue>"));
    assert!(source.contains("structured: ItemBodyValue | null"));
    assert!(source.contains("next: BranchB | null"));
    assert!(source.contains("Map<string, Array<BranchA | null>>"));
    let names = source
        .lines()
        .filter_map(|line| line.strip_prefix("function __ark_"))
        .filter_map(|line| line.split_once('(').map(|(name, _)| name))
        .collect::<Vec<_>>();
    assert_eq!(
        names.iter().copied().collect::<BTreeSet<_>>().len(),
        names.len()
    );
    // Bound the generated program for this fixed graph, including resource helpers.
    assert!(
        names.len() < 256,
        "recursive types must not expand with value-path depth"
    );
}

#[test]
fn recursive_resource_paths_and_slot_errors_are_preserved() {
    let facade = facade();
    assert_eq!(facade.ast.callbacks.len(), 4);
    for name in ["bindCallbacks", "bindReversed"] {
        let operation = facade
            .ast
            .operations
            .iter()
            .find(|op| op.name == name)
            .unwrap();
        let sites = facade
            .ast
            .callbacks
            .iter()
            .filter(|site| site.operation_id == operation.id)
            .collect::<Vec<_>>();
        assert_eq!(sites.len(), 2);
        assert_eq!(
            sites
                .iter()
                .map(|site| site.path.to_string())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "argument[0].field[listener]".into(),
                "argument[1].field[listener]".into()
            ])
        );
        assert_ne!(sites[0].contract.retention, sites[1].contract.retention);
    }
    let stream = facade
        .ast
        .operations
        .iter()
        .find(|op| op.name == "snapshots" && op.kind == OperationKind::OutputStreamStart)
        .unwrap();
    assert_eq!(stream.stream_resources.len(), 2);
    assert_eq!(
        stream
            .stream_resources
            .iter()
            .map(|site| site.path.to_string())
            .collect::<BTreeSet<_>>(),
        BTreeSet::from(["argument[0]".into(), "return".into()])
    );
    let mut invalid = facade.ast.clone();
    let output = invalid
        .operations
        .iter_mut()
        .find(|op| op.id == stream.id)
        .unwrap()
        .stream_resources
        .iter_mut()
        .find(|site| site.direction == StreamDirection::Output)
        .unwrap();
    assert!(output
        .slot_operation_ids
        .remove(&OperationKind::OutputStreamCancel)
        .is_some());
    assert!(
        matches!(render_ark_inventory(&invalid), Err(FacadeError::MissingStreamSlot { operation }) if operation == stream.id)
    );
}

#[test]
#[ignore = "requires UNIFFI_ARK_TYPESCRIPT pointing to the DevEco SDK TypeScript module"]
fn recursive_ark_compiles_and_roundtrips_finite_values() {
    let compiler = std::env::var("UNIFFI_ARK_TYPESCRIPT").expect("ArkTS compiler module path");
    let facade = facade();
    let root = std::env::temp_dir().join(format!(
        "uniffi-ark-recursive-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    for file in facade.ark_files() {
        fs::write(root.join(&file.path), &file.bytes).unwrap();
    }
    let ids = facade
        .ast
        .operations
        .iter()
        .filter(|op| matches!(op.source_key.owner(), OperationOwner::Namespace))
        .map(|op| format!("{:?}:{}", op.name, op.id.index()))
        .collect::<Vec<_>>()
        .join(",");
    let listener_id = facade
        .ast
        .types
        .iter()
        .find(|ty| ty.name == "Listener")
        .unwrap()
        .id
        .index();
    let script = format!("const ids = {{{ids}}};\nconst listenerType = {listener_id};\n")
        + r#"
const assert = require('node:assert/strict');
const path = require('node:path');
const ts = require(process.env.UNIFFI_ARK_TYPESCRIPT);
const options = {target:ts.ScriptTarget.ES2021, module:ts.ModuleKind.CommonJS, strict:true,
  useUnknownInCatchVariables:false, lib:['lib.es2021.d.ts'], outDir:'out', allowNonTsExtensions:true};
const host = ts.createIncrementalCompilerHost(options);
const builder = ts.createIncrementalProgram({rootNames:[path.resolve('platform.ets')], options, host});
const program = builder.getProgram();
const diagnostics = ts.getPreEmitDiagnostics(program)
  .concat(ts.ArkTSLinter_1_1.runArkTSLinter(builder, undefined, undefined, 'ArkTS_1_1'))
  .concat(ts.getPreEmitDiagnostics(ts.createProgram([path.resolve('platform.d.ets')], options)));
for (const diagnostic of diagnostics) console.log(ts.flattenDiagnosticMessageText(diagnostic.messageText, '\n'));
assert.equal(diagnostics.filter(d => d.category === ts.DiagnosticCategory.Error).length, 0, 'strict ArkTS/declaration errors');
assert.equal(program.emit().emitSkipped, false);
const ark = require('./out/platform.js');
const hostApi = new ark.Host();
let backendCalls = 0;
let returned;
let overrideReturn = false;
let callbacks;
let closed = false;
const backend = {
  invokeSync(id, args) {
    backendCalls++;
    if (id === ids.bindCallbacks || id === ids.bindReversed) {
      callbacks = args.map(value => value.listener);
      assert.ok(callbacks.every(id => Number.isSafeInteger(id) && id > 0));
      return new ark.ArkValueResult(null);
    }
    assert.ok([ids.echoValue, ids.echoSnapshot, ids.echoBranch].includes(id));
    return new ark.ArkValueResult(overrideReturn ? returned : args[0]);
  },
  close() { closed = true; }
};
const session = new ark.BackendSession(backend, hostApi, {callControl:{create:10000,cancel:10001,release:10002,operations:[]}});
const api = ark.createNamespace(session).recursive_dto;
const value = api.ItemBodyValue;
const leaf = value.List({values:[value.Null, value.Bool({value:true}), value.Integer({value:9223372036854775807n}), value.Text({value:'正文'})]});
const tree = value.Map({entries:new Map([['root',value.List({values:[leaf,value.Map({entries:new Map([['empty',value.Null]])})]})]])});
for (const item of [value.Null, ...leaf.values, leaf, tree]) assert.deepEqual(api.echoValue(item), item);
for (const snapshot of [{text:null,structured:tree},{text:'',structured:null}]) assert.deepEqual(api.echoSnapshot(snapshot), snapshot);
const branch = {next:{children:[{next:null}],byName:new Map([['none',[null]],['nested',[{next:{children:[],byName:new Map()}}]]])}};
assert.deepEqual(api.echoBranch(branch), branch, 'mutually recursive Map/List/Option');
let nested = value.Null;
for (let i=0;i<24;i++) nested = i % 2 ? value.Map({entries:new Map([['child',nested]])}) : value.List({values:[nested]});
assert.deepEqual(api.echoValue(nested), nested, 'finite recursion is evaluated at runtime');
const fails = (body, name) => assert.throws(body, error => error.errorName === name);
const callsBeforeInvalid = backendCalls;
fails(() => api.echoValue({tag:'Unknown'}), 'UniffiEnumVariant');
fails(() => api.echoValue(value.Map({entries:{}})), 'UniffiMapType');
fails(() => api.echoBranch({next:{children:[{}],byName:new Map()}}), 'UniffiRecordField');
fails(() => api.echoSnapshot({text:null,structured:undefined}), 'UniffiRecordField');
assert.equal(backendCalls, callsBeforeInvalid, 'invalid inputs must not reach backend');
overrideReturn = true;
for (const [bad, error] of [[{tag:'Unknown'},'UniffiEnumVariant'],[{tag:'Map',entries:{}},'UniffiMapType'],[{tag:'List',values:[{tag:'Unknown'}]},'UniffiEnumVariant']]) {
  returned = bad; fails(() => api.echoValue(value.Null), error);
}
returned = {text:null,structured:undefined}; fails(() => api.echoSnapshot({text:null,structured:null}), 'UniffiRecordField');
overrideReturn = false;
const first = {listener:{value(){return 'first';}}, next:{listener:null,next:null}};
const second = {listener:{value(){return 'second';}}, next:null};
for (const [method, retained] of [['bindCallbacks',1],['bindReversed',0]]) {
  api[method](first, second);
  fails(() => hostApi.invokeCallbackSync(listenerType, callbacks[1-retained],0,[]),'UniffiCallbackMissing');
  hostApi.retainCallback(listenerType,callbacks[retained]);
  assert.equal(hostApi.invokeCallbackSync(listenerType,callbacks[retained],0,[]),retained ? 'second' : 'first');
  hostApi.releaseCallback(listenerType,callbacks[retained]);
  fails(() => hostApi.invokeCallbackSync(listenerType,callbacks[retained],0,[]),'UniffiCallbackMissing');
}
const callsBeforeMissing = backendCalls;
fails(() => api.bindCallbacks({listener:null,next:first},second),'UniffiCallbackContract');
assert.equal(backendCalls,callsBeforeMissing,'recursive children must not inherit an ancestor callback contract');
session.close().then(() => {
  assert.equal(closed,true);
  console.log('recursive ItemBodyValue, mutual containers, validation and operation/path callback lifetime contracts passed');
}).catch(error => { console.error(error); process.exitCode=1; });
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
        "recursive ArkTS failed in {}: {}\n{}",
        root.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains(
        "recursive ItemBodyValue, mutual containers, validation and operation/path callback lifetime contracts passed"
    ), "runtime must finish its roundtrip and close assertions");
    println!("{}", String::from_utf8_lossy(&output.stdout));
    fs::remove_dir_all(root).unwrap();
}
