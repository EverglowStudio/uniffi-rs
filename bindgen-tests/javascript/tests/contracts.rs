//! Generated JavaScript static and stub-runtime contracts.

mod support;

#[path = "support/shared.rs"]
mod shared;

#[path = "support/contract_corpus.rs"]
mod contract_corpus;

use contract_corpus::*;
use shared::*;
use support::*;

#[test]
fn engine_neutral_contract_corpus_is_deterministic() {
    use uniffi_js_abi::Capability;
    use uniffi_js_abi::{
        InputPresence, PresenceError, PublicSourceFamily, PublicTarget, ScalarType, ValueType,
    };

    let forward = unified_contract_corpus(false);
    let reverse = unified_contract_corpus(true);
    assert_eq!(forward, reverse, "source discovery order changed the plan");
    assert_eq!(forward.targets().len(), 3);
    assert!(forward
        .operations()
        .iter()
        .any(|operation| operation.required_capabilities.contains(Capability::BigInt)));
    assert!(forward
        .operations()
        .iter()
        .any(|operation| operation.required_capabilities.contains(Capability::Map)));
    assert!(forward
        .operations()
        .iter()
        .any(|operation| operation.required_capabilities.contains(Capability::Set)));

    let optional = ValueType::optional(ValueType::Scalar(ScalarType::String));
    assert_eq!(optional.validate_presence(InputPresence::Null), Ok(()));
    assert_eq!(
        optional.validate_presence(InputPresence::Undefined),
        Err(PresenceError::Undefined)
    );

    let node = PublicTarget::NodeNapi.output_layout();
    let web = PublicTarget::BrowserWasm.output_layout();
    let ohos = PublicTarget::OhosNapi.output_layout();
    assert_eq!(node, web, "Node and Web must share physical public source");
    assert_eq!(node.implementation_suffix, ".js");
    assert_eq!(node.declaration_suffix, ".d.ts");
    assert_eq!(ohos.implementation_suffix, ".ets");
    assert_eq!(ohos.declaration_suffix, ".d.ets");
    assert_eq!(ohos.source_family, PublicSourceFamily::ArkTs);
}

#[test]
fn callback_methods_keep_independent_async_and_error_signatures() {
    use uniffi_js_abi::Capability;
    use uniffi_js_abi::{AsyncKind, OperationOwner};

    let plan = unified_contract_corpus(false);
    let callback_methods: Vec<_> = plan
        .operations()
        .iter()
        .filter(|operation| {
            matches!(
                operation.operation.definition.source_key.owner(),
                OperationOwner::Callback(_)
            )
        })
        .collect();
    assert_eq!(callback_methods.len(), 4);
    assert!(callback_methods.iter().any(|method| {
        method.operation.definition.signature.async_kind == AsyncKind::Sync
            && method.operation.definition.signature.throws.is_none()
    }));
    assert!(callback_methods.iter().any(|method| {
        method.operation.definition.signature.async_kind == AsyncKind::Sync
            && method.operation.definition.signature.throws.is_some()
    }));
    assert!(callback_methods.iter().any(|method| {
        method.operation.definition.signature.async_kind == AsyncKind::Async
            && method.operation.definition.signature.throws.is_none()
    }));
    assert!(callback_methods.iter().any(|method| {
        method.operation.definition.signature.async_kind == AsyncKind::Async
            && method.operation.definition.signature.throws.is_some()
    }));

    let run_async = plan
        .operations()
        .iter()
        .find(|operation| operation.operation.definition.public_name == "runAsync")
        .unwrap();
    assert!(run_async
        .required_capabilities
        .contains(Capability::AsyncCallback));
    assert!(run_async
        .required_capabilities
        .contains(Capability::FallibleCallback));
}

#[test]
fn napi_accepts_async_callback_return_callbacks() {
    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("async_callback_return");
    let src_dir = crate_dir.join("src");
    std::fs::create_dir_all(&src_dir).unwrap();
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        r#"[package]
name = "async-callback-return"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["lib", "cdylib"]
"#,
    )
    .unwrap();
    std::fs::write(src_dir.join("lib.rs"), "// placeholder\n").unwrap();
    let udl_path = src_dir.join("async_callback_return.udl");
    std::fs::write(
        &udl_path,
        r#"
[Trait, WithForeign]
interface Logger {
  string log(string message);
};

[Trait, WithForeign]
interface Maker {
  [Async, CallbackContract="return,retained,calling_thread,allowed"]
  Logger make_logger(string prefix);
};

namespace async_callback_return {
  [Async, CallbackContract="argument[0],retained,may_cross_thread,allowed"]
  string run(Maker maker);
};
"#,
    )
    .unwrap();

    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: Utf8PathBuf::from_path_buf(udl_path).unwrap(),
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap(),
                host_crates_dir: out_dir.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("N-API/Electron should accept async callback-return callbacks");
    let api =
        std::fs::read_to_string(out_dir.join("components/async_callback_return/index.js")).unwrap();
    for needle in [
        "callbackContracts:{\"return\":{",
        "retention:\"retained\"",
        "threading:\"callingThread\"",
        "reentrancy:\"allowed\"",
    ] {
        assert!(
            api.contains(needle),
            "callback-return canonical metadata is missing `{needle}`:\n{api}"
        );
    }
    let napi_rs = std::fs::read_to_string(out_dir.join("native/node.rs")).unwrap();
    for needle in [
        "SessionCallbackArgument",
        "SessionValuePathSegment :: Return",
        "SessionCallbackRetention :: Retained",
        "SessionCallbackThreading :: CallingThread",
        "SessionCallbackReentrancy :: Allowed",
        "CallbackHostAsync",
    ] {
        assert!(
            napi_rs.contains(needle),
            "N-API bridge is missing callback-return canonical metadata `{needle}`:\n{napi_rs}"
        );
    }
    assert!(
        api.contains("createNamespace"),
        "callback-return facade should expose the canonical namespace factory:\n{api}"
    );
    assert!(
        out_dir.join("electron/index.js").is_file(),
        "Electron target must publish the shared canonical entrypoint"
    );
}

#[test]
fn public_facade_types_and_names_are_static() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    std::fs::write(
        biz.join("src/blockers.udl"),
        r#"
dictionary Shape { string label; u32 sides; };
dictionary GreetOptions { string prefix; boolean loud; sequence<i64> dims; };
enum Event { "Start", "Tick", "Stop" };
callback interface Logger { void log(string msg); };
namespace blockers {
    Shape describe(Event e);
    string greet(GreetOptions opts);
    [CallbackContract="argument[0],scoped,calling_thread,forbidden"]
    void run_job(Logger logger);
    i64 signed_roundtrip(i64 input);
};
"#,
    )
    .unwrap();
    std::fs::write(
        biz.join("Cargo.toml"),
        r#"[package]
name = "blockers"
version = "0.0.0"
edition = "2021"
[lib]
crate-type = ["lib", "cdylib"]
[dependencies]
"#,
    )
    .unwrap();
    std::fs::write(
        biz.join("src/lib.rs"),
        "// placeholder
",
    )
    .unwrap();

    let package_root = root.join("generated");
    std::fs::create_dir_all(&package_root).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: biz.join("src/blockers.udl"),
            out_dir: package_root.clone(),
            package_root: package_root.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: biz.join("Cargo.toml"),
                host_crates_dir: package_root.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .expect("generator should succeed for blockers fixture");

    let implementation =
        std::fs::read_to_string(package_root.join("components/blockers/index.js")).unwrap();
    let declarations =
        std::fs::read_to_string(package_root.join("components/blockers/index.d.ts")).unwrap();
    for name in [
        "Shape",
        "GreetOptions",
        "Event",
        "Logger",
        "describe",
        "greet",
        "runJob",
        "signedRoundtrip",
    ] {
        assert!(
            declarations.contains(name) || implementation.contains(name),
            "public facade should retain {name}:\n{declarations}\n{implementation}"
        );
    }
    assert!(
        declarations.contains("bigint"),
        "i64 public declarations must use bigint:\n{declarations}"
    );
    assert!(
        implementation.contains("createNamespace") && implementation.contains("signedRoundtrip"),
        "component implementation must expose the canonical namespace factory and operation:\n{implementation}"
    );
    assert!(
        !implementation.contains(".ts"),
        "ECMAScript facade must not depend on removed TypeScript runtime paths:\n{implementation}"
    );
}
#[test]
fn custom_types_wasm_static_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let (udl, config, manifest) = write_custom_core_crate(tmp.path());

    let package_root = root.join("generated-wasm");
    std::fs::create_dir_all(&package_root).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: package_root.clone(),
            package_root: package_root.clone(),
            artifact_dir: None,
            config_override: Some(config),
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: package_root.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .expect("custom wasm generation should succeed");

    std::fs::write(
        package_root.join("components/custom_js_core/email.js"),
        r#"
export function emailAddressFromString(value) { return { value }; }
export function emailAddressToString(value) { return value.value; }
"#,
    )
    .unwrap();

    let implementation =
        std::fs::read_to_string(package_root.join("components/custom_js_core/index.js")).unwrap();
    let declarations =
        std::fs::read_to_string(package_root.join("components/custom_js_core/index.d.ts")).unwrap();
    for needle in [
        "EmailAddress",
        "emailAddressFromString",
        "emailAddressToString",
    ] {
        assert!(
            implementation.contains(needle),
            "custom implementation must contain {needle}:\n{implementation}"
        );
    }
    assert!(
        declarations.contains("Email") && declarations.contains("Contact"),
        "custom declarations must preserve public custom and record types:\n{declarations}"
    );
    let wasm_rs = std::fs::read_to_string(package_root.join("native/wasm.rs")).unwrap();
    assert!(
        !wasm_rs.contains("serde::") && !wasm_rs.contains("serde_wasm_bindgen"),
        "wasm shim must stay serde-free for custom types:\n{wasm_rs}"
    );
}
#[test]
fn harmony_stream_package_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let host_dir = out_dir.join("native/hosts");
    generate_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Harmony],
    );

    for path in [
        "Index.ets",
        "Index.d.ets",
        "native/ohos.rs",
        "native/hosts/ohos/Cargo.toml",
    ] {
        assert!(
            out_dir.join(path).is_file(),
            "missing stream package file {path}"
        );
    }
    let ark = std::fs::read_to_string(out_dir.join("Index.ets")).unwrap();
    for needle in ["countEvents", "errorAfterOne", "optionalEvents"] {
        assert!(
            ark.contains(needle),
            "Harmony stream facade must expose {needle}: {ark}"
        );
    }
    assert!(
        !out_dir.join("components/stream_core/harmony").exists(),
        "stream package must not recreate a per-component Harmony sidecar directory"
    );
}
#[test]
fn input_stream_package_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_input_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let host_dir = out_dir.join("native/hosts");
    generate_input_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![
            FlavorTarget::Wasm,
            FlavorTarget::Napi,
            FlavorTarget::Electron,
            FlavorTarget::Harmony,
        ],
    );

    for path in [
        "components/input_stream_core/index.js",
        "components/input_stream_core/index.d.ts",
        "Index.ets",
        "Index.d.ets",
        "native/node.rs",
        "native/wasm.rs",
        "native/ohos.rs",
        "native/hosts/napi/Cargo.toml",
        "native/hosts/wasm/Cargo.toml",
        "native/hosts/ohos/Cargo.toml",
    ] {
        assert!(
            out_dir.join(path).is_file(),
            "missing input-stream package file {path}"
        );
    }
    let component =
        std::fs::read_to_string(out_dir.join("components/input_stream_core/index.js")).unwrap();
    for needle in ["sumInputEvents", "runningSum"] {
        assert!(
            component.contains(needle),
            "input-stream facade must expose {needle}: {component}"
        );
    }
    let native_node = std::fs::read_to_string(out_dir.join("native/node.rs")).unwrap();
    assert!(
        native_node.contains("running_sum")
            && native_node.contains("SessionStreamDirection")
            && native_node.contains("InputStreamHostPull"),
        "N-API adapter must retain canonical input-stream operations: {native_node}"
    );
    let native_wasm = std::fs::read_to_string(out_dir.join("native/wasm.rs")).unwrap();
    assert!(
        native_wasm.contains("running_sum") && native_wasm.contains("ForeignInputStreamOps"),
        "Wasm adapter must retain input-stream operations: {native_wasm}"
    );
    assert!(
        !out_dir
            .join("components/input_stream_core/harmony")
            .exists(),
        "input-stream package must not recreate a per-component Harmony sidecar directory"
    );
}
#[test]
fn wasm_local_uniffi_stream_alias_cargo_checks() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP wasm_local_uniffi_stream_alias_cargo_checks: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP wasm_local_uniffi_stream_alias_cargo_checks: wasm32-unknown-unknown target unavailable"
        );
        return;
    }

    let tmp = tempfile::tempdir().unwrap();
    let crate_dir = tmp.path().join("local-stream-core");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let uniffi_dep = workspace_root().join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "local-stream-core"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
uniffi = {{ path = {:?}, features = ["wasm-unstable-single-threaded"] }}

[workspace]
resolver = "3"
"#,
            uniffi_dep.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
use std::{
    cell::Cell,
    fmt,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use uniffi::deps::futures_core::Stream;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LocalStreamEvent {
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum LocalStreamError {
    Boom,
}

impl fmt::Display for LocalStreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boom => write!(f, "boom"),
        }
    }
}

impl std::error::Error for LocalStreamError {}

struct LocalStream {
    cursor: Rc<Cell<u32>>,
    end: u32,
}

impl Stream for LocalStream {
    type Item = Result<LocalStreamEvent, LocalStreamError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let value = self.cursor.get();
        if value >= self.end {
            Poll::Ready(None)
        } else {
            self.cursor.set(value + 1);
            Poll::Ready(Some(Ok(LocalStreamEvent { value })))
        }
    }
}

#[uniffi::export]
pub fn local_events(count: u32) -> uniffi::UniFfiStream<LocalStreamEvent, LocalStreamError> {
    Box::pin(LocalStream {
        cursor: Rc::new(Cell::new(0)),
        end: count,
    })
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    let output = Command::new(&cargo)
        .args([
            "check",
            "--manifest-path",
            crate_dir.join("Cargo.toml").to_str().unwrap(),
            "--target",
            "wasm32-unknown-unknown",
        ])
        .env("CARGO_TARGET_DIR", tmp.path().join("target-local-stream"))
        .env("RUSTFLAGS", "-D warnings")
        .output()
        .expect("failed to invoke cargo for wasm local stream fixture");
    if !output.status.success() {
        panic!(
            "wasm local UniFfiStream fixture failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn emits_harmony_flavor_with_ohos_napi_surface() {
    let out = tempfile::tempdir().unwrap();
    let package_root = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    std::fs::create_dir_all(&package_root).unwrap();
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let manifest = workspace_root().join("examples/arithmetic/Cargo.toml");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: package_root.clone(),
            package_root: package_root.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: package_root.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Harmony],
        },
    )
    .expect("generator should emit both node and Harmony package targets");

    for path in [
        "shared/uniffi_runtime.js",
        "shared/uniffi_runtime.d.ts",
        "components/arithmetic/index.js",
        "components/arithmetic/index.d.ts",
        "Index.ets",
        "Index.d.ets",
        "native/node.rs",
        "native/ohos.rs",
        "native/hosts/napi/Cargo.toml",
        "native/hosts/ohos/Cargo.toml",
    ] {
        assert!(
            package_root.join(path).is_file(),
            "missing generated package file {path}"
        );
    }
    let node = std::fs::read_to_string(package_root.join("native/node.rs")).unwrap();
    assert!(node.contains("napi_derive::napi"));
    assert!(!node.contains("napi_ohos"));
    let ohos = std::fs::read_to_string(package_root.join("native/ohos.rs")).unwrap();
    assert!(ohos.contains("napi_ohos"));
    assert!(!ohos.contains("napi_derive::napi"));
    let ark = std::fs::read_to_string(package_root.join("Index.ets")).unwrap();
    assert!(
        ark.contains("add") && ark.contains("sub"),
        "ArkTS entry must preserve arithmetic operations: {ark}"
    );
}
#[test]
fn custom_types_emit_public_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let (generated, _host_dir) = generate_custom_napi_tree(tmp.path());

    let implementation =
        std::fs::read_to_string(generated.join("components/custom_js_core/index.js")).unwrap();
    let declarations =
        std::fs::read_to_string(generated.join("components/custom_js_core/index.d.ts")).unwrap();
    assert!(
        implementation.contains("emailAddressFromString")
            && implementation.contains("emailAddressToString"),
        "custom implementation should retain configured conversion functions:\n{implementation}"
    );
    assert!(
        declarations.contains("Email") && declarations.contains("Contact"),
        "custom declarations should retain public custom and record types:\n{declarations}"
    );
    assert!(
        generated.join("native/node.rs").is_file(),
        "custom package must publish its native adapter in the package root"
    );
    assert!(
        !implementation.contains(".ts"),
        "custom implementation must not reference removed TypeScript sidecar paths:\n{implementation}"
    );
}

#[test]
fn runtime_numeric_lossless_or_reject() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP runtime_numeric_lossless_or_reject: node 22.6+ unavailable");
        return;
    };

    // Generate the arithmetic package so we can import its plain-ECMAScript
    // runtime helpers directly.
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    let driver = r#"
import { lowerValue, UniffiError } from "./shared/uniffi_runtime.js";

const i64 = { kind: "scalar", name: "I64" };
const u64 = { kind: "scalar", name: "U64" };
const toI64 = (value) => lowerValue(value, i64);
const toU64 = (value) => lowerValue(value, u64);

function expectBigint(label, fn_) {
    const result = fn_();
    if (typeof result !== "bigint") throw new Error(`${label}: expected bigint, got ${typeof result}`);
    return result;
}

function expectThrow(label, fn_) {
    try {
        fn_();
        throw new Error(`${label}: expected throw, got success`);
    } catch (e) {
        if (!(e instanceof UniffiError) || e.errorName !== "UniffiNumericError") {
            throw new Error(`${label}: expected UniffiNumericError, got ${e}`);
        }
    }
}

// 1. Safe integer number → bigint (lossless)
const r1 = expectBigint("safe int", () => toI64(42));
if (r1 !== 42n) throw new Error(`safe int: got ${r1}`);

// 2. Unsafe integer number → reject
expectThrow("unsafe int", () => toI64(2**53));
expectThrow("unsafe int neg", () => toI64(-(2**53)));

// 3. Non-integer number → reject
expectThrow("float", () => toI64(3.14));
expectThrow("NaN", () => toI64(NaN));
expectThrow("Infinity", () => toI64(Infinity));
expectThrow("-Infinity", () => toI64(-Infinity));

// 4. String → bigint
const r4 = expectBigint("string", () => toI64("9007199254740993"));
if (r4 !== 9007199254740993n) throw new Error(`string: got ${r4}`);

// 5. I64 bigint boundaries remain lossless.
const r5Min = expectBigint("i64 min", () => toI64(-9223372036854775808n));
if (r5Min !== -9223372036854775808n) throw new Error(`i64 min: got ${r5Min}`);
const r5Max = expectBigint("i64 max", () => toI64(9223372036854775807n));
if (r5Max !== 9223372036854775807n) throw new Error(`i64 max: got ${r5Max}`);

// 6. toU64 rejects negative
expectThrow("u64 neg bigint", () => toU64(-1n));
expectThrow("u64 neg number", () => toU64(-1));

// 7. Large u64 beyond i64::MAX round-trips
const big = toU64(18446744073709551615n);
if (big !== 18446744073709551615n) throw new Error(`large u64: got ${big}`);

console.log("ok");
"#;
    std::fs::write(out_dir.join("numeric_driver.mjs"), driver).unwrap();

    let output = Command::new(&node)
        .arg("--no-warnings")
        .arg("numeric_driver.mjs")
        .current_dir(&out_dir)
        .output()
        .expect("failed to invoke node");

    if !output.status.success() {
        panic!(
            "numeric driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ok"),
        "numeric driver did not print ok:\n{stdout}"
    );
}

#[test]
fn runtime_input_stream_error_lowering_is_canonical() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP runtime_input_stream_error_lowering_is_canonical: node 22.6+ unavailable");
        return;
    };

    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);
    std::fs::write(
        out_dir.join("stream_error_driver.mjs"),
        r#"
import { BackendSession, UniffiError, lowerValue } from "./shared/uniffi_runtime.js";

const unitError = {
  kind: "enum", name: "UnitError", error: true, unit: true,
  variants: { Boom: { fields: {} } },
};
const payloadError = {
  kind: "enum", name: "PayloadError", error: true, unit: false,
  variants: {
    Bad: { fields: { values: { kind: "sequence", inner: { kind: "scalar", name: "U32" } } } },
  },
};
const context = { types: { 7: unitError, 8: payloadError } };
const input = (typeId) => ({ kind: "inputStream", item: { kind: "scalar", name: "U32" }, error: { kind: "named", name: typeId === 7 ? "UnitError" : "PayloadError", typeId } });
const operation = (error) => ({
  streamResources: [{ path: "argument[0]", direction: "Input", item: { kind: "scalar", name: "U32" }, error, slots: { InputStreamPull: 1, InputStreamCancel: 2 } }],
});

const session = new BackendSession({});
async function pull(source, descriptor, op) {
  const handle = lowerValue(source, descriptor, context, session, op, "argument[0]");
  return session.host.pullInputStream(handle);
}

const unit = await pull(
  { async next() { throw new UniffiError({ errorName: "UnitError", variant: "Boom", data: "Boom" }); } },
  input(7), operation({ kind: "named", name: "UnitError", typeId: 7 }),
);
if (unit.kind !== "error" || unit.error !== "Boom") throw new Error(`unit error was not canonical: ${JSON.stringify(unit)}`);

const payload = await pull(
  { async next() { throw new UniffiError({ errorName: "PayloadError", variant: "Bad", data: { values: [1, 2, 3] } }); } },
  input(8), operation({ kind: "named", name: "PayloadError", typeId: 8 }),
);
if (payload.kind !== "error" || payload.error?.tag !== "Bad" || payload.error?.values?.join(",") !== "1,2,3") {
  throw new Error(`payload error was not canonical: ${JSON.stringify(payload)}`);
}

async function expectFailure(label, raw) {
  try {
    await pull({ async next() { throw raw; } }, input(8), operation({ kind: "named", name: "PayloadError", typeId: 8 }));
    throw new Error(`${label} unexpectedly succeeded`);
  } catch (error) {
    if (!(error instanceof UniffiError)) throw new Error(`${label} wrong error: ${error}`);
  }
}
await expectFailure("unknown variant", new UniffiError({ variant: "Missing", data: {} }));
await expectFailure("missing payload", new UniffiError({ variant: "Bad", data: {} }));
console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--no-warnings")
        .arg("stream_error_driver.mjs")
        .current_dir(&out_dir)
        .output()
        .expect("failed to invoke node");
    if !output.status.success() {
        panic!(
            "stream error driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "stream error driver did not print ok"
    );
}

pub fn generate_callback_shape_tree(out_dir: &Utf8PathBuf) {
    let root = out_dir.parent().expect("temp output has parent");
    let biz = root.join("callback_shape");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl = r#"
[Enum]
interface StreamEvent {
    Started(string message_id);
    Delta(string message_id, string content_delta);
};

[Trait, WithForeign]
interface StreamSink {
    void on_event(StreamEvent event);
};

namespace callback_shape {
    [CallbackContract="argument[0],scoped,calling_thread,forbidden"]
    void emit_event(StreamSink sink);
};
"#;
    let udl_path = biz.join("src/callback_shape.udl");
    std::fs::write(&udl_path, udl).unwrap();
    std::fs::write(
        biz.join("Cargo.toml"),
        r#"[package]
name = "callback-shape"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["lib", "cdylib"]

[dependencies]
"#,
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let manifest = biz.join("Cargo.toml");
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: out_dir.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![FlavorTarget::Napi],
        },
    )
    .expect("generator should succeed for callback shape fixture");
}
