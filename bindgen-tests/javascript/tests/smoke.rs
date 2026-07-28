//! File-layout smoke test + minimal execution test for the JavaScript
//! bindgen scaffolding.
//!
//! `emits_real_tree_for_all_flavors` stays as the cheap, always-on check:
//! file tree exists, key strings are present. It guards regressions in
//! the scaffolding without requiring a node runtime.
//!
//! `runs_common_api_under_node` is the execution-level check for the
//! high-level common API. It installs a pure-JS stub backend, imports
//! the generated `common/api.ts` via Node's `--experimental-strip-types`,
//! and exercises free functions, object lifecycle, error marshalling,
//! and the numeric conversion path. Requires Node >= 22.6; older/missing
//! nodes cause the test to be skipped with an eprintln rather than fail.

use camino::Utf8PathBuf;
use std::process::Command;
use uniffi_bindgen::{BindgenLoader, BindgenPaths, GlobalConfig};
use uniffi_bindgen_javascript::{generate, FlavorTarget, GenerateJsOptions, HostCrateOptions};

const EMPTY_GENERATED_FILES: &[(&str, &str)] = &[];

fn workspace_root() -> Utf8PathBuf {
    let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../..").canonicalize_utf8().unwrap()
}

fn contains_dynamic_type_word(source: &str) -> bool {
    source
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word == "any" || word == "unknown")
}

fn generate_arithmetic(out_dir: &Utf8PathBuf) {
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    assert!(source.exists(), "fixture UDL missing: {source}");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![
                FlavorTarget::Wasm,
                FlavorTarget::Napi,
                FlavorTarget::Electron,
                FlavorTarget::Harmony,
            ],
        },
    )
    .expect("generator should succeed");
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
crate-type = ["rlib"]
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
  [Async]
  Logger make_logger(string prefix);
};

namespace async_callback_return {
  [Async]
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
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("N-API/Electron should accept async callback-return callbacks");
    let api = std::fs::read_to_string(out_dir.join("common/api.ts")).unwrap();
    assert!(
        api.contains("callbackReturnMethods: { \"makeLogger\": true }"),
        "callback-return metadata should be emitted:\n{api}"
    );
    let napi_rs = std::fs::read_to_string(out_dir.join("node/async_callback_return.rs")).unwrap();
    assert!(
        napi_rs.contains("__UniffiCallbackHandle")
            && napi_rs.contains("__uniffi_from_callback_registry"),
        "napi bridge should emit callback-return registry support:\n{napi_rs}"
    );
    let backend = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        backend.contains("__uniffiStoreCallbackReturn")
            && backend.contains("__uniffiCallbackDispatcher"),
        "napi backend should store async callback returns:\n{backend}"
    );
}

#[test]
fn emits_real_tree_for_all_flavors() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    for name in [
        "common/api.ts",
        "common/records.ts",
        "common/enums.ts",
        "common/errors.ts",
        "common/objects.ts",
        "common/callbacks.ts",
        "common/runtime.ts",
        "browser/index.ts",
        "browser/backend-wasm.ts",
        "node/index.ts",
        "node/backend-napi.ts",
        "electron/index.ts",
        "electron/backend-napi.ts",
        "electron/preload.cjs",
        "electron/renderer.ts",
        "harmony/index.ts",
        "harmony/arithmetical.ohos-facade.json",
    ] {
        let p = out_dir.join(name);
        assert!(p.exists(), "expected output file missing: {p}");
    }

    let harmony_contract: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out_dir.join("harmony/arithmetical.ohos-facade.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(harmony_contract["schemaVersion"], 2);
    assert!(harmony_contract["outputStreams"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(harmony_contract["inputStreams"]
        .as_array()
        .unwrap()
        .is_empty());

    let api = std::fs::read_to_string(out_dir.join("common/api.ts")).unwrap();
    assert!(
        api.contains("export function add("),
        "common/api.ts should expose `add`, got:\n{api}"
    );
    // u64 returns are now bigint — no fromU64 wrapping.
    assert!(
        !api.contains("fromU64"),
        "common/api.ts should NOT wrap u64 returns via fromU64 (bigint contract), got:\n{api}"
    );
    // public-types.ts must exist and re-export key types.
    let pt_path = out_dir.join("common/public-types.ts");
    assert!(pt_path.exists(), "expected common/public-types.ts");
    let pt = std::fs::read_to_string(&pt_path).unwrap();
    assert!(
        pt.contains("ArithmeticError"),
        "public-types.ts should re-export ArithmeticError"
    );
    assert!(
        pt.contains("UniffiError"),
        "public-types.ts should re-export UniffiError"
    );
    assert!(
        pt.contains("./api.ts"),
        "public-types.ts should re-export free functions from api.ts"
    );
    assert!(
        pt.contains("export interface ArithmeticModule"),
        "public-types.ts should expose an explicit module API type:\n{pt}"
    );
    assert!(
        pt.contains("add: typeof import(\"./api.ts\").add;")
            && pt.contains("ArithmeticError: typeof import(\"./errors.ts\").ArithmeticError;")
            && pt.contains("UniffiError: typeof import(\"./runtime.ts\").UniffiError;"),
        "public-types.ts should describe runtime exports in the module API type:\n{pt}"
    );
    assert!(
        pt.contains("export type ArithmeticApi = ArithmeticModule;")
            && pt.contains("export type UniffiPublicApi = ArithmeticModule;"),
        "public-types.ts should expose stable component-specific and generic API aliases:\n{pt}"
    );
    // No `bigint | number` in public contract.
    assert!(
        !pt.contains("bigint | number"),
        "public-types.ts must not contain `bigint | number`"
    );

    // High-level api.ts: u64 args should be `bigint`, not `number`.
    assert!(
        api.contains(": bigint"),
        "common/api.ts should type u64 args/returns as `bigint`, got:\n{api}"
    );
    assert!(
        !api.contains("fromU64(") && !api.contains("fromI64("),
        "common/api.ts should not call fromU64/fromI64 (bigint-first)"
    );
    assert!(
        api.contains("toU64"),
        "common/api.ts should lower u64 args via toU64, got:\n{api}"
    );

    // The raw napi addon surface is bigint-native, so node/electron
    // adapters must not carry the old safe-integer compatibility layer.
    let napi_backend = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        !napi_backend.contains("__uniffiInt64ArgKinds")
            && !napi_backend.contains("__uniffiInt64ReturnKinds")
            && !napi_backend.contains("__uniffiLowerInt64ForNapi")
            && !napi_backend.contains("__uniffiLiftInt64FromNapi"),
        "node/backend-napi.ts must not carry the old int64 compat layer"
    );
    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    assert!(
        !preload.contains("__uniffiInt64ArgKinds")
            && !preload.contains("__uniffiInt64ReturnKinds")
            && !preload.contains("__uniffiLowerInt64ForNapi")
            && !preload.contains("__uniffiLiftInt64FromNapi"),
        "electron/preload.cjs must not carry the old int64 compat layer"
    );

    let errors = std::fs::read_to_string(out_dir.join("common/errors.ts")).unwrap();
    assert!(
        errors.contains("ArithmeticError"),
        "common/errors.ts should contain ArithmeticError subclass"
    );

    let runtime = std::fs::read_to_string(out_dir.join("common/runtime.ts")).unwrap();
    assert!(runtime.contains("UNIFFI_JS_CONTRACT_VERSION = 1"));
    assert!(runtime.contains("class UniffiError"));
    assert!(runtime.contains("class UniffiObjectHandle"));
    assert!(runtime.contains("function toU64"));

    // Both node and browser entries must auto-install the backend.
    let node_index = std::fs::read_to_string(out_dir.join("node/index.ts")).unwrap();
    assert!(
        node_index.contains("__installBackend(backend)"),
        "node/index.ts must auto-install backend, got:\n{node_index}"
    );
    let browser_index = std::fs::read_to_string(out_dir.join("browser/index.ts")).unwrap();
    assert!(
        browser_index.contains("__installBackend(backend)"),
        "browser/index.ts must auto-install backend, got:\n{browser_index}"
    );

    // Locate the per-crate napi bridge file.
    let rust_path = std::fs::read_dir(out_dir.join("node"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .expect("a .rs bridge file should exist under node/");
    let rust_bridge = std::fs::read_to_string(&rust_path).unwrap();
    assert!(
        rust_bridge.contains("#[napi]"),
        "node/*.rs should be real napi-rs bridge"
    );
    assert!(
        rust_bridge.contains("pub fn add") || rust_bridge.contains("fn add"),
        "node/*.rs should expose `add`"
    );

    // The wasm Rust shim must exist, must use #[wasm_bindgen], and must
    // actually wrap at least one of the arithmetic free functions.
    let wasm_rs_path = std::fs::read_dir(out_dir.join("browser"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .expect("a .rs wasm shim should exist under browser/");
    let wasm_rs = std::fs::read_to_string(&wasm_rs_path).unwrap();
    assert!(
        wasm_rs.contains("#[wasm_bindgen]"),
        "browser/*.rs should be a wasm-bindgen shim, got:\n{wasm_rs}"
    );
    assert!(
        wasm_rs.contains("pub fn add") || wasm_rs.contains("pub async fn add"),
        "browser/*.rs should wrap `add`"
    );

    let backend_wasm = std::fs::read_to_string(out_dir.join("browser/backend-wasm.ts")).unwrap();
    assert!(
        backend_wasm.contains("adaptWasmBindgenGlue"),
        "backend-wasm.ts must expose Path A adapter, got:\n{backend_wasm}"
    );
    assert!(
        backend_wasm.contains("WasmBindgenGlue"),
        "backend-wasm.ts must expose WasmBindgenGlue type"
    );
    assert!(
        !backend_wasm.contains("WebAssembly.compile"),
        "backend-wasm.ts must NOT load raw wasm bytes (Path A committed)"
    );
    assert!(
        browser_index.contains("initBackend"),
        "browser/index.ts must expose async initBackend"
    );

    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    assert!(preload.contains("contextBridge.exposeInMainWorld(\"__uniffi__\""));
    assert!(preload.contains(".node\""));
    // Preload must split sync and async — sync Rust exports break React
    // render if the preload async-wraps them.
    assert!(
        preload.contains("dispatchSync"),
        "electron preload must expose dispatchSync for sync Rust exports"
    );
    assert!(
        preload.contains("dispatchAsync"),
        "electron preload must expose dispatchAsync"
    );
    // Preload must unwrap the backend-agnostic callback marker before
    // forwarding to the napi addon (which expects raw `{ log: fn }`).
    assert!(
        preload.contains("__uniffiCallback"),
        "electron preload must unwrap callback-trait markers in resolveArg"
    );

    let renderer = std::fs::read_to_string(out_dir.join("electron/renderer.ts")).unwrap();
    assert!(renderer.contains("__installBackend"));
    assert!(renderer.contains("new Proxy"));
    // Renderer must preserve sync semantics: there must be a sync
    // callSync path, and the generator must emit an explicit
    // ASYNC_METHODS set. Without this, `core.greet(name)` returns a
    // Promise and crashes React render.
    assert!(
        renderer.contains("ASYNC_METHODS"),
        "electron renderer must declare an ASYNC_METHODS set so sync \
         Rust exports stay synchronous"
    );
    assert!(
        renderer.contains("callSync"),
        "electron renderer must expose a synchronous dispatch path"
    );
    assert!(
        !renderer.contains("(...args: unknown[]) => call(method, ...args)"),
        "electron renderer must not collapse every method into an async call"
    );

    // napi backend adapter must also unwrap the callback marker so the
    // Logger-style fixture works end-to-end through the napi addon.
    let backend_napi = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        backend_napi.contains("__uniffiCallback"),
        "napi backend adapter must unwrap callback-trait markers \
         before calling the native addon"
    );
}

/// Execution-level sanity check (see module doc). Skipped if Node is
/// missing or older than 22.6 (no `--experimental-strip-types`).
#[test]
fn runs_common_api_under_node() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping: node with --experimental-strip-types not available");
        return;
    };

    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    // Harness: install a pure-JS stub backend, call through common/api.ts.
    let harness = r#"
import {
    __installBackend,
    UniffiError,
} from "./common/runtime.ts";
import { add, sub, div, equal } from "./common/api.ts";
import { ArithmeticError } from "./common/errors.ts";

__installBackend({
    add(a, b) {
        // The generator lowers u64 args via toU64 into bigint.
        // Return a bigint — the high-level contract is bigint-first.
        if (typeof a !== "bigint" || typeof b !== "bigint") {
            throw new Error(`add expected bigint args, got ${typeof a}, ${typeof b}`);
        }
        return a + b;
    },
    sub(a, b) {
        if (a < b) {
            const err = new UniffiError({
                errorName: "ArithmeticError",
                variant: "IntegerOverflow",
                data: null,
                message: "sub underflow",
            });
            throw err;
        }
        return a - b;
    },
    div(a, b) { return a / b; },
    equal(a, b) { return a === b; },
});

const sum = add(2n, 3n);
if (sum !== 5n) throw new Error(`add failed: ${sum}`);
if (typeof sum !== "bigint") throw new Error(`u64 should be bigint, got ${typeof sum}`);

if (sub(10n, 4n) !== 6n) throw new Error("sub failed");
if (div(20n, 4n) !== 5n) throw new Error("div failed");
if (equal(3n, 3n) !== true) throw new Error("equal failed");

let threw = false;
try {
    sub(1n, 5n);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) throw new Error("error not wrapped as UniffiError");
    if (e.errorName !== "ArithmeticError") throw new Error(`wrong errorName: ${e.errorName}`);
}
if (!threw) throw new Error("sub(1,5) should have thrown");

// Exercise the ArithmeticError class to confirm it's a real subclass.
const hand = new ArithmeticError("manual", "IntegerOverflow");
if (!(hand instanceof UniffiError)) throw new Error("ArithmeticError not subclass");

console.log("ok");
"#;
    std::fs::write(out_dir.join("driver.ts"), harness).unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to invoke node");

    if !output.status.success() {
        panic!(
            "node driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ok"), "driver did not print ok:\n{stdout}");
}

/// Full Path-A end-to-end: takes the generated `browser/<crate>.rs`
/// shim, drops it into a synthesised `cargo` workspace alongside a
/// trivial business crate, compiles for `wasm32-unknown-unknown`, runs
/// `wasm-bindgen --target nodejs`, then loads the resulting JS glue
/// from a Node driver that calls into the generated `browser/index.ts`.
///
/// Covers one sync scalar function, one fallible scalar function, and
/// one async scalar function — the minimum the user asked for before
/// the public wasm path can be called complete.
///
/// Skipped gracefully when any piece of the toolchain is missing:
/// `node` ≥ 22.6, `cargo`, the `wasm32-unknown-unknown` target, or
/// `wasm-bindgen` CLI.
/// Static regression for strict TypeScript consumers:
///
/// 1. `common/api.ts` must explicitly import every named type it uses
///    in a free-function signature (records / enums / callback traits),
///    so names like `Event` do not silently bind to the DOM global.
/// 2. `common/api.ts` and `common/objects.ts` must not contain unused
///    runtime imports — strict TS refuses them.
/// 3. The `__call<_>` / `__callAsync<_>` generic must be `bigint` on
///    the i64/u64 return path so the bigint-first contract still
///    type-checks under strict mode.
#[test]
fn api_ts_has_explicit_imports_and_strict_safe_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    // Minimal UDL crate. Deliberately includes:
    // - `dictionary Shape` (record) used in a free-function signature
    // - `enum Event` (non-error enum) — the name collides with DOM global
    // - `dictionary GreetOptions` (record) used as an arg, with nested i64
    //   lowering that must import toI64 in common/api.ts, not records.ts
    // - `callback interface Logger` used as an arg
    // - a free function returning i64 (to exercise bigint return flow)
    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl = r#"
dictionary Shape { string label; u32 sides; };
dictionary GreetOptions { string prefix; boolean loud; sequence<i64> dims; };
enum Event { "Start", "Tick", "Stop" };
callback interface Logger { void log(string msg); };

namespace blockers {
    Shape describe(Event e);
    string greet(GreetOptions opts);
    void run_job(Logger logger);
    i64 signed_roundtrip(i64 input);
};
"#;
    let udl_path = biz.join("src/blockers.udl");
    std::fs::write(&udl_path, udl).unwrap();
    std::fs::write(
        biz.join("Cargo.toml"),
        r#"[package]
name = "blockers"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
"#,
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .expect("generator should succeed for blockers fixture");

    let api = std::fs::read_to_string(gen_dir.join("common/api.ts")).unwrap();

    // 1. Explicit imports for every type the signatures actually touch.
    for (name, from_file) in [
        ("Shape", "./records.ts"),
        ("GreetOptions", "./records.ts"),
        ("Event", "./enums.ts"),
        ("Logger", "./callbacks.ts"),
    ] {
        let import_line = format!(" {name}");
        let from_clause = format!("from \"{from_file}\"");
        let found = api.lines().any(|l| {
            l.starts_with("import") && l.contains(&import_line) && l.contains(&from_clause)
        });
        assert!(
            found,
            "common/api.ts must explicitly import `{name}` from `{from_file}`. Got:\n{api}"
        );
    }

    // 2. i64/u64 returns use `__call<bigint>` and pass through directly
    //    — no fromI64/fromU64 wrapping (bigint-first contract).
    assert!(
        !api.contains("fromI64(") && !api.contains("fromU64("),
        "common/api.ts should not call fromI64/fromU64 (bigint-first contract):\n{api}"
    );
    assert!(
        api.contains("__call<bigint>") || api.contains("__callAsync<bigint>"),
        "common/api.ts expected a `__call<bigint>` on the i64 return path, got:\n{api}"
    );

    let import_block: String = api
        .lines()
        .filter(|l| l.starts_with("import ") && l.contains("./runtime.ts"))
        .collect::<Vec<_>>()
        .join("\n");

    // 3. Nested i64 fields in a record argument must be lowered by api.ts,
    //    because record declarations are just TypeScript interfaces.
    assert!(
        import_block.contains("toI64"),
        "common/api.ts should import toI64 for nested i64 fields in record args:\n{import_block}\n\n{api}"
    );

    let records = std::fs::read_to_string(gen_dir.join("common/records.ts")).unwrap();
    let records_runtime_imports: String = records
        .lines()
        .filter(|l| l.starts_with("import ") && l.contains("./runtime.ts"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !records_runtime_imports.contains("toI64"),
        "common/records.ts must not import toI64 for plain interface fields:\n{records_runtime_imports}\n\n{records}"
    );

    // 4. No unused runtime imports. This fixture does not touch u64
    //    args, so `toU64` must not be imported. `fromI64`/`fromU64` are
    //    also not used any more (bigint-first contract).
    for unused in ["toU64", "fromU64", "fromI64"] {
        assert!(
            !import_block.contains(unused),
            "common/api.ts imports unused `{unused}`:\n{import_block}"
        );
    }
    // common/api.ts is now backend-agnostic: callback-trait args are
    // lowered to a tagged marker `{ __uniffiCallback: true, object }`
    // and each backend adapter (wasm/napi/electron) translates it.
    // common/api.ts itself must NOT import registerCallback.
    assert!(
        !import_block.contains("registerCallback"),
        "common/api.ts must be backend-agnostic and not import registerCallback:\n{import_block}"
    );
    let api_body = api.as_str();
    assert!(
        api_body.contains("__uniffiCallback: true"),
        "common/api.ts should lower callback-trait args as tagged marker:\n{api_body}"
    );

    // 5. `common/objects.ts` must not carry a dangling runtime import
    //    when there are no non-callback-trait objects at all.
    let objects = std::fs::read_to_string(gen_dir.join("common/objects.ts")).unwrap();
    assert!(
        !objects.contains("import") || !objects.contains("runtime.ts"),
        "common/objects.ts has unused runtime import (no objects in this fixture):\n{objects}"
    );

    // 6. Optional: if tsc is available, actually compile in strict
    //    mode. This is the real guardrail — the string assertions
    //    above are only a faster early signal.
    if let Some(tsc) = which_tool("tsc") {
        let tsconfig = gen_dir.join("tsconfig.json");
        std::fs::write(
            &tsconfig,
            r#"{
  "compilerOptions": {
    "target": "es2022",
    "module": "es2022",
    "moduleResolution": "bundler",
    "strict": true,
    "noUnusedLocals": true,
    "noUnusedParameters": true,
    "noEmit": true,
    "skipLibCheck": true,
    "allowImportingTsExtensions": true,
    "types": [],
    "lib": ["es2022", "dom"]
  },
  "include": ["common/*.ts"]
}
"#,
        )
        .unwrap();
        let out = Command::new(&tsc)
            .arg("--noEmit")
            .arg("-p")
            .arg(tsconfig.as_str())
            .output()
            .expect("failed to invoke tsc");
        if !out.status.success() {
            panic!(
                "tsc --strict failed on generated tree:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            );
        }
    } else {
        eprintln!("note: tsc not available, skipping strict TS compile check");
    }
}

#[test]
fn runs_generated_wasm_shim_end_to_end() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping wasm e2e: node 22.6+ unavailable");
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("skipping wasm e2e: cargo not found");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!("skipping wasm e2e: wasm32-unknown-unknown target not installed");
        return;
    }
    let Some(wasm_bindgen) = which_tool("wasm-bindgen") else {
        eprintln!("skipping wasm e2e: wasm-bindgen CLI not found");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    // 1. Business crate (written first so the UDL lives inside a real
    //    crate; the uniffi loader requires that). Plain Rust, no uniffi
    //    runtime dep — the generated shim calls pub fns directly.
    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl = r#"
[Error]
enum WasmScalarError { "Underflow" };

namespace wasm_scalar {
  u64 add(u64 a, u64 b);
  [Throws=WasmScalarError]
  u64 checked_sub(u64 a, u64 b);
  [Async]
  u64 async_add(u64 a, u64 b);
};
"#;
    let udl_path = biz.join("src/wasm_scalar.udl");
    std::fs::write(&udl_path, udl).unwrap();
    // Minimal Cargo.toml so the uniffi loader recognises this as a crate.
    std::fs::write(
        biz.join("Cargo.toml"),
        r#"[package]
name = "wasm_scalar"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]
"#,
    )
    .unwrap();
    std::fs::write(
        biz.join("src/lib.rs"),
        "// placeholder, overwritten below\n",
    )
    .unwrap();

    // 2. Generate JS bindings from the UDL into ./gen.
    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path.clone(),
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .expect("bindgen should succeed for wasm_scalar UDL");

    // 3. Finish the business crate (real lib.rs body).
    std::fs::write(
        biz.join("src/lib.rs"),
        r#"
pub fn add(a: u64, b: u64) -> u64 { a.wrapping_add(b) }

#[derive(Debug)]
pub enum WasmScalarError { Underflow }
impl core::fmt::Display for WasmScalarError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        write!(f, "{:?}", self)
    }
}
impl std::error::Error for WasmScalarError {}

pub fn checked_sub(a: u64, b: u64) -> Result<u64, WasmScalarError> {
    a.checked_sub(b).ok_or(WasmScalarError::Underflow)
}

pub async fn async_add(a: u64, b: u64) -> u64 { a.wrapping_add(b) }
"#,
    )
    .unwrap();

    // 4. Shim crate: cdylib with the generated wasm-bindgen Rust file.
    let shim = root.join("shim");
    std::fs::create_dir_all(shim.join("src")).unwrap();
    let gen_rs = gen_dir.join("browser/wasm_scalar.rs");
    let shim_src = std::fs::read_to_string(&gen_rs)
        .unwrap_or_else(|_| panic!("generated shim missing at {gen_rs}"));
    std::fs::write(shim.join("src/lib.rs"), shim_src).unwrap();
    std::fs::write(
        shim.join("Cargo.toml"),
        r#"[package]
name = "wasm_scalar_shim"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wasm-bindgen = "=0.2.117"
wasm-bindgen-futures = "0.4"
js-sys = "0.3"
wasm_scalar = { path = "../biz" }
"#,
    )
    .unwrap();
    // Isolate from any parent workspace so the temp crates build standalone.
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"biz\", \"shim\"]\nresolver = \"3\"\n",
    )
    .unwrap();

    // 5. cargo build --target wasm32-unknown-unknown --release
    let build = Command::new(&cargo)
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "-p",
            "wasm_scalar_shim",
        ])
        .current_dir(&root)
        .output()
        .expect("failed to invoke cargo");
    if !build.status.success() {
        eprintln!(
            "skipping wasm e2e: cargo build failed (likely offline/no deps):\nstderr:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
        return;
    }

    // 6. wasm-bindgen --target nodejs
    let wasm_file = root.join("target/wasm32-unknown-unknown/release/wasm_scalar_shim.wasm");
    assert!(wasm_file.exists(), "expected wasm artifact at {wasm_file}");
    let pkg = root.join("pkg");
    let bg = Command::new(&wasm_bindgen)
        .args(["--target", "nodejs", "--out-dir"])
        .arg(pkg.as_str())
        .arg(wasm_file.as_str())
        .output()
        .expect("failed to invoke wasm-bindgen");
    if !bg.status.success() {
        panic!(
            "wasm-bindgen failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&bg.stdout),
            String::from_utf8_lossy(&bg.stderr),
        );
    }

    // 7. Driver: import the CJS glue via createRequire, drive initBackend
    //    then exercise sync / fallible / async scalar paths.
    let driver = r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { add, checkedSub, asyncAdd } from "./gen/common/api.ts";
import { UniffiError } from "./gen/common/runtime.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_scalar_shim.js");
await initBackend(glue);

// sync scalar — u64 returns bigint
const s = add(2n, 3n);
if (s !== 5n) throw new Error(`sync add via wasm failed: ${s}`);
if (typeof s !== "bigint") throw new Error(`u64 should be bigint, got ${typeof s}`);

// fallible scalar — underflow must surface as UniffiError
let threw = false;
try {
    checkedSub(3n, 10n);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`fallible wrapped wrong type: ${e && (e as Error).message}`);
    }
}
if (!threw) throw new Error("checked_sub(3,10) should have thrown");

// async scalar — u64 returns bigint
const r = await asyncAdd(7n, 8n);
if (r !== 15n) throw new Error(`async add via wasm failed: ${r}`);
if (typeof r !== "bigint") throw new Error(`u64 should be bigint, got ${typeof r}`);

// initBackend idempotent
await initBackend(glue);

console.log("ok");
"#;
    std::fs::write(root.join("driver.ts"), driver).unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&root)
        .output()
        .expect("failed to invoke node");
    if !output.status.success() {
        panic!(
            "wasm e2e driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ok"),
        "wasm e2e driver did not print ok:\n{stdout}"
    );
}

/// Records + payload enum + error-with-data coverage. Extends the
/// scalar e2e with:
///   - a `User` record as both arg and return
///   - a unit enum (`Color`) as arg + return
///   - a payload enum (`Shape`) as return
///   - a payload error (`CheckoutError::OutOfStock { sku }`)
#[test]
fn runs_generated_wasm_shim_records_and_enums() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_rec",
        udl: r#"
dictionary User {
  string name;
  u32 age;
};

enum Color { "Red", "Green", "Blue" };

[Enum]
interface Shape {
  Circle(f64 radius);
  Square(f64 side);
};

[Error]
interface CheckoutError {
  OutOfStock(string sku);
  PaymentDeclined(string reason);
};

namespace wasm_rec {
  User make_user(string name, u32 age);
  string greet_user(User user);
  Color invert(Color c);
  Shape bigger(Shape s, f64 factor);
  [Throws=CheckoutError]
  u32 buy(string sku, u32 qty);
};
"#,
        // No `serde` dep anywhere — not on downstream biz, and not on
        // the shim. Explicit `__lower_`/`__lift_` helpers replace serde
        // entirely.
        biz_deps: r#""#,
        shim_deps: r#""#,
        biz_lib: r#"
// NB: no `serde::Serialize` / `serde::Deserialize` / `#[serde(tag)]` on
// any of these. The wasm shim lowers/lifts via `js_sys::Reflect` /
// `js_sys::Array`, so the downstream crate stays serde-free.
#[derive(Clone)]
pub struct User {
    pub name: String,
    pub age: u32,
}

#[derive(Clone)]
pub enum Color {
    Red,
    Green,
    Blue,
}

#[derive(Clone)]
pub enum Shape {
    Circle { radius: f64 },
    Square { side: f64 },
}

// Error with payload: the wasm shim wraps via Display+format; the TS
// runtime catches the thrown JsError and wraps it into `UniffiError`.
#[derive(Debug)]
pub enum CheckoutError {
    OutOfStock(String),
    PaymentDeclined(String),
}
impl std::fmt::Display for CheckoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::OutOfStock(s) => write!(f, "OutOfStock({s})"),
            Self::PaymentDeclined(r) => write!(f, "PaymentDeclined({r})"),
        }
    }
}
impl std::error::Error for CheckoutError {}

pub fn make_user(name: String, age: u32) -> User {
    User { name, age }
}

pub fn greet_user(user: User) -> String {
    format!("hello {}, you are {}", user.name, user.age)
}

pub fn invert(c: Color) -> Color {
    match c {
        Color::Red => Color::Blue,
        Color::Blue => Color::Red,
        Color::Green => Color::Green,
    }
}

pub fn bigger(s: Shape, factor: f64) -> Shape {
    match s {
        Shape::Circle { radius } => Shape::Circle { radius: radius * factor },
        Shape::Square { side } => Shape::Square { side: side * factor },
    }
}

pub fn buy(sku: String, qty: u32) -> Result<u32, CheckoutError> {
    if sku == "rare" {
        Err(CheckoutError::OutOfStock(sku))
    } else if qty == 0 {
        Err(CheckoutError::PaymentDeclined("zero quantity".into()))
    } else {
        Ok(qty * 10)
    }
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { makeUser, greetUser, invert, bigger, buy } from "./gen/common/api.ts";
import { UniffiError } from "./gen/common/runtime.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_rec_shim.js");
await initBackend(glue);

// records: make_user returns a plain object
const u = makeUser("alice", 30) as { name: string; age: number };
if (u.name !== "alice" || u.age !== 30) {
    throw new Error(`make_user shape wrong: ${JSON.stringify(u)}`);
}
const g = greetUser({ name: "bob", age: 25 });
if (g !== "hello bob, you are 25") throw new Error(`greet_user: ${g}`);

// unit enum: round-trips as string
const inv = invert("Red" as any);
if (inv !== "Blue") throw new Error(`invert(Red)=${inv}`);

// payload enum: tagged-union shape
const big = bigger({ tag: "Circle", radius: 2 } as any, 3) as any;
if (big.tag !== "Circle" || big.radius !== 6) {
    throw new Error(`bigger: ${JSON.stringify(big)}`);
}
const sq = bigger({ tag: "Square", side: 4 } as any, 0.5) as any;
if (sq.tag !== "Square" || sq.side !== 2) {
    throw new Error(`bigger square: ${JSON.stringify(sq)}`);
}

// error-with-data: message carries the variant + payload
let threw = false;
try {
    buy("rare", 1);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) throw new Error("not UniffiError");
    if (!(e as Error).message.includes("OutOfStock")) {
        throw new Error(`error message missing variant: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("buy(rare) should have thrown");

const ok = buy("common", 3);
if (ok !== 30) throw new Error(`buy ok: ${ok}`);

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_map() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_map",
        udl: r#"
dictionary User {
  string name;
  u32 age;
};

namespace wasm_map {
  record<string, u32> bump_counts(record<string, u32> input);
  record<string, User> rename_users(record<string, User> input);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::collections::HashMap;

#[derive(Clone)]
pub struct User {
    pub name: String,
    pub age: u32,
}

pub fn bump_counts(mut input: HashMap<String, u32>) -> HashMap<String, u32> {
    for value in input.values_mut() {
        *value += 1;
    }
    let total = input.values().copied().sum();
    input.insert("total".into(), total);
    input
}

pub fn rename_users(input: HashMap<String, User>) -> HashMap<String, User> {
    input
        .into_iter()
        .map(|(key, user)| {
            (
                key,
                User {
                    name: format!("{}!", user.name),
                    age: user.age + 1,
                },
            )
        })
        .collect()
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { bumpCounts, renameUsers } from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_map_shim.js");
await initBackend(glue);

const counts = bumpCounts({ a: 1, b: 2 }) as Record<string, number>;
if (counts.a !== 2 || counts.b !== 3 || counts.total !== 5) {
    throw new Error(`bumpCounts wrong: ${JSON.stringify(counts)}`);
}

const users = renameUsers({
    ada: { name: "Ada", age: 36 },
    bob: { name: "Bob", age: 41 },
}) as Record<string, { name: string; age: number }>;
if (users.ada.name !== "Ada!" || users.ada.age !== 37) {
    throw new Error(`renameUsers ada wrong: ${JSON.stringify(users)}`);
}
if (users.bob.name !== "Bob!" || users.bob.age !== 42) {
    throw new Error(`renameUsers bob wrong: ${JSON.stringify(users)}`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

/// Chronological builtins: `timestamp` -> `Date`, `duration` -> ms
/// number. Exercises round-trip, arithmetic, optional handling and the
/// two key error paths.
#[test]
fn runs_generated_wasm_shim_timestamp_duration() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_time",
        udl: r#"
[Error]
enum ChronologicalError {
  "TimeOverflow",
  "TimeDiffError",
};

namespace wasm_time {
  [Throws=ChronologicalError]
  timestamp return_timestamp(timestamp a);
  [Throws=ChronologicalError]
  duration return_duration(duration a);
  [Throws=ChronologicalError]
  timestamp add(timestamp a, duration b);
  [Throws=ChronologicalError]
  duration diff(timestamp a, timestamp b);
  boolean optional(timestamp? a, duration? b);
  timestamp get_far_future_timestamp();
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::time::{Duration, SystemTime};

#[derive(Debug)]
pub enum ChronologicalError {
    TimeOverflow,
    TimeDiffError,
}

impl std::fmt::Display for ChronologicalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimeOverflow => write!(f, "TimeOverflow"),
            Self::TimeDiffError => write!(f, "TimeDiffError"),
        }
    }
}

impl std::error::Error for ChronologicalError {}

pub fn return_timestamp(a: SystemTime) -> Result<SystemTime, ChronologicalError> {
    Ok(a)
}

pub fn return_duration(a: Duration) -> Result<Duration, ChronologicalError> {
    Ok(a)
}

pub fn add(a: SystemTime, b: Duration) -> Result<SystemTime, ChronologicalError> {
    a.checked_add(b).ok_or(ChronologicalError::TimeOverflow)
}

pub fn diff(a: SystemTime, b: SystemTime) -> Result<Duration, ChronologicalError> {
    a.duration_since(b)
        .map_err(|_| ChronologicalError::TimeDiffError)
}

pub fn optional(a: Option<SystemTime>, b: Option<Duration>) -> bool {
    a.is_some() && b.is_some()
}

pub fn get_far_future_timestamp() -> SystemTime {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(8_640_000_000_001))
        .unwrap()
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import {
    returnTimestamp,
    returnDuration,
    add,
    diff,
    optional,
    getFarFutureTimestamp,
} from "./gen/common/api.ts";
import { UniffiError } from "./gen/common/runtime.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_time_shim.js");
await initBackend(glue);

const ts = new Date("2024-01-02T03:04:05.283Z");
const tsRound = returnTimestamp(ts);
if (!(tsRound instanceof Date) || tsRound.getTime() !== ts.getTime()) {
    throw new Error(`timestamp round-trip failed: ${tsRound}`);
}

const dur = 1500.5;
const durRound = returnDuration(dur);
if (durRound !== dur) {
    throw new Error(`duration round-trip failed: ${durRound}`);
}

const added = add(new Date(1000), 2000);
if (!(added instanceof Date) || added.getTime() !== 3000) {
    throw new Error(`timestamp + duration failed: ${added}`);
}

const delta = diff(new Date(3000), new Date(1000));
if (delta !== 2000) {
    throw new Error(`timestamp - timestamp failed: ${delta}`);
}

if (!optional(ts, dur)) throw new Error("optional(Some, Some) should be true");
if (optional(null, dur)) throw new Error("optional(None, Some) should be false");
if (optional(ts, null)) throw new Error("optional(Some, None) should be false");

let threw = false;
try {
    returnDuration(-1);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`bad duration threw wrong type: ${e && (e as Error).message}`);
    }
    if (!/duration.*negative/i.test((e as Error).message)) {
        throw new Error(`bad duration message: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("returnDuration(-1) should throw");

threw = false;
try {
    getFarFutureTimestamp();
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`far future threw wrong type: ${e && (e as Error).message}`);
    }
    if (!(e as Error).message.includes("timestamp exceeds JS Date range")) {
        throw new Error(`far future message: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("getFarFutureTimestamp() should throw");

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_objects() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_obj",
        udl: r#"
interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
  // `get` intentionally collides with the registry helper name
  // (`counter_get`) to regression-guard the `__uniffi_` prefix fix.
  u32 get();
};

namespace wasm_obj {};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Mutex;

pub struct Counter { inner: Mutex<u32> }

impl Counter {
    pub fn new(initial: u32) -> Self { Self { inner: Mutex::new(initial) } }
    pub fn inc(&self) { *self.inner.lock().unwrap() += 1; }
    pub fn value(&self) -> u32 { *self.inner.lock().unwrap() }
    pub fn get(&self) -> u32 { *self.inner.lock().unwrap() }
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { Counter } from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_obj_shim.js");
await initBackend(glue);

const c = Counter.new(10);
c.inc();
c.inc();
const v = c.value();
if (v !== 12) throw new Error(`value=${v}`);
// Exercise the colliding method name.
const g = c.get();
if (g !== 12) throw new Error(`get=${g}`);
c.dispose();

let threw = false;
try { c.inc(); } catch { threw = true; }
if (!threw) throw new Error("expected throw after dispose");

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

/// Arc<Self> constructor — the `__Coerce` autoref trick must handle it
/// the same way as `-> Self`. Also covers proc-macro-style biz code.
#[test]
fn runs_generated_wasm_shim_arc_self_ctor() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_arc",
        udl: r#"
interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
};

namespace wasm_arc {};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::{Arc, Mutex};

pub struct Counter { inner: Mutex<u32> }

impl Counter {
    // Returns `Arc<Self>` rather than `Self` — the coercion logic must
    // support both constructor shapes.
    pub fn new(initial: u32) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(initial) })
    }
    pub fn inc(&self) { *self.inner.lock().unwrap() += 1; }
    pub fn value(&self) -> u32 { *self.inner.lock().unwrap() }
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { Counter } from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_arc_shim.js");
await initBackend(glue);

const c = Counter.new(7);
c.inc(); c.inc(); c.inc();
if (c.value() !== 10) throw new Error(`value=${c.value()}`);
c.dispose();

let threw = false;
try { c.inc(); } catch { threw = true; }
if (!threw) throw new Error("expected throw after dispose");

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

/// Trait object / free-function object handle — factory returns
/// `Arc<dyn Greeter>`, free function takes it as an argument.
#[test]
fn runs_generated_wasm_shim_trait_object() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_trait",
        udl: r#"
[Trait]
interface Greeter {
  string greet(string name);
};

namespace wasm_trait {
  Greeter english_greeter();
  Greeter chinese_greeter();
  string call_greeter(Greeter greeter, string name);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

pub trait Greeter: Send + Sync {
    fn greet(&self, name: String) -> String;
}

pub struct English;
impl Greeter for English {
    fn greet(&self, name: String) -> String { format!("Hello, {name}!") }
}

pub struct Chinese;
impl Greeter for Chinese {
    fn greet(&self, name: String) -> String { format!("Ni hao, {name}!") }
}

pub fn english_greeter() -> Arc<dyn Greeter> { Arc::new(English) }
pub fn chinese_greeter() -> Arc<dyn Greeter> { Arc::new(Chinese) }
pub fn call_greeter(greeter: Arc<dyn Greeter>, name: String) -> String {
    greeter.greet(name)
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { englishGreeter, chineseGreeter, callGreeter } from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_trait_shim.js");
await initBackend(glue);

// factory returns a wrapped object
const en = englishGreeter();
const hi1 = en.greet("world");
if (hi1 !== "Hello, world!") throw new Error(`en.greet=${hi1}`);

// method on the trait object directly
const cn = chineseGreeter();
const hi2 = cn.greet("shijie");
if (hi2 !== "Ni hao, shijie!") throw new Error(`cn.greet=${hi2}`);

// free function taking the handle back through __uniffi.raw
const viaFree = callGreeter(en, "alice");
if (viaFree !== "Hello, alice!") throw new Error(`callGreeter=${viaFree}`);

en.dispose();
cn.dispose();

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

/// Callback interface — JS object is registered, passed as handle, Rust
/// calls back into JS by handle through the thread-local invoker.
#[test]
fn runs_generated_wasm_shim_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_cb",
        udl: r#"
callback interface Logger {
  void log(string msg);
};

namespace wasm_cb {
  void run_job(Logger logger);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

pub trait Logger: Send + Sync {
    fn log(&self, msg: String);
}

pub fn run_job(logger: Arc<dyn Logger>) {
    logger.log("start".to_string());
    logger.log("progress".to_string());
    logger.log("done".to_string());
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { runJob } from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_cb_shim.js");
await initBackend(glue);

const received: string[] = [];
const logger = {
    log(msg: string) { received.push(msg); },
};

runJob(logger as any);

if (received.length !== 3) {
    throw new Error(`expected 3 log calls, got ${received.length}: ${JSON.stringify(received)}`);
}
if (received[0] !== "start" || received[1] !== "progress" || received[2] !== "done") {
    throw new Error(`unexpected log payload: ${JSON.stringify(received)}`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

/// Async callback-trait / `with_foreign` trait — JS async methods should be
/// awaited by the generated Rust wasm shim, and Promise-returning JS callback
/// methods must round-trip through the callback registry.
#[test]
fn runs_generated_wasm_shim_async_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_async_cb",
        udl: r#"
dictionary WorkRecord {
  u32 total;
};

[Trait, WithForeign]
interface AsyncWorker {
  [Async]
  void note(string msg);
  [Async]
  WorkRecord make_record(u32 a, u32 b);
};

namespace wasm_async_cb {
  [Async]
  WorkRecord run_async_worker(AsyncWorker worker);
};
"#,
        biz_deps: "async-trait = \"0.1\"\n",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkRecord {
    pub total: u32,
}

#[async_trait::async_trait(?Send)]
pub trait AsyncWorker: Send + Sync {
    async fn note(&self, msg: String);
    async fn make_record(&self, a: u32, b: u32) -> WorkRecord;
}

pub async fn run_async_worker(worker: Arc<dyn AsyncWorker>) -> WorkRecord {
    worker.note("start".to_string()).await;
    let record = worker.make_record(20, 22).await;
    worker.note("done".to_string()).await;
    record
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import { runAsyncWorker } from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_async_cb_shim.js");
await initBackend(glue);

const calls: string[] = [];
const worker = {
    async note(msg: string): Promise<void> {
        await new Promise((resolve) => setTimeout(resolve, 1));
        calls.push(msg);
    },
    async makeRecord(a: number, b: number): Promise<{ total: number }> {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return { total: a + b };
    },
};
const record = await runAsyncWorker(worker as any);
if (record.total !== 42) {
    throw new Error(`total=${record.total}`);
}
if (calls.join(",") !== "start,done") {
    throw new Error(`calls=${calls.join(",")}`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

/// Callback-return smoke — JS callback returns a normal UniFFI object
/// (`Counter`), a trait object (`Greeter`), plus callback trait /
/// callback interface values (`Logger`, `HostLogger`). The Rust consumer
/// immediately calls methods on the returned callback values, proving the
/// object and callback registry round-trips work in wasm.
#[test]
fn runs_generated_wasm_shim_callback_object_return() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_cb_object",
        udl: r#"
interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
};

[Trait]
interface Greeter {
  string greet(string name);
};

callback interface Logger {
  string log(string message);
};

[Trait, WithForeign]
interface HostLogger {
  string greet(string name);
};

[Error]
enum ProviderError {
  "BadValue",
};

callback interface Maker {
  Counter make_counter(u32 initial);
  Greeter make_greeter(string prefix);
  Logger make_logger(string prefix);
  HostLogger make_host_logger(string prefix);
  [Async]
  Logger make_async_logger(string prefix);
  [Async]
  HostLogger make_async_host_logger(string prefix);
  [Async, Throws=ProviderError]
  Logger checked_make_async_logger(string prefix, boolean fail);
};

namespace wasm_cb_object {
  Greeter english_greeter(string prefix);
  Counter invoke_maker_make_counter(Maker maker, u32 initial);
  Greeter invoke_maker_make_greeter(Maker maker, string prefix);
  string invoke_maker_run_logger(Maker maker, string prefix, string message);
  string invoke_maker_run_host_logger(Maker maker, string prefix, string name);
  [Async]
  string invoke_maker_run_async_logger(Maker maker, string prefix, string message);
  [Async]
  string invoke_maker_run_async_host_logger(Maker maker, string prefix, string name);
  [Async, Throws=ProviderError]
  string invoke_maker_checked_make_async_logger(Maker maker, string prefix, boolean fail, string message);
};
"#,
        biz_deps: "async-trait = \"0.1\"\n",
        shim_deps: "",
        biz_lib: r#"
use std::sync::{Arc, Mutex};

pub struct Counter {
    inner: Mutex<u32>,
}

impl Counter {
    pub fn new(initial: u32) -> Arc<Self> {
        Arc::new(Self { inner: Mutex::new(initial) })
    }
    pub fn inc(&self) {
        *self.inner.lock().unwrap() += 1;
    }
    pub fn value(&self) -> u32 {
        *self.inner.lock().unwrap()
    }
}

pub trait Greeter: Send + Sync {
    fn greet(&self, name: String) -> String;
}

pub trait Logger: Send + Sync {
    fn log(&self, message: String) -> String;
}

pub trait HostLogger: Send + Sync {
    fn greet(&self, name: String) -> String;
}

#[derive(Debug)]
pub enum ProviderError {
    BadValue,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadValue => write!(f, "BadValue"),
        }
    }
}

impl std::error::Error for ProviderError {}

pub struct English {
    prefix: String,
}

impl Greeter for English {
    fn greet(&self, name: String) -> String {
        format!("{}{}{}", self.prefix, if self.prefix.ends_with(' ') { "" } else { " " }, name)
    }
}

pub fn english_greeter(prefix: String) -> Arc<dyn Greeter> {
    Arc::new(English { prefix })
}

#[async_trait::async_trait(?Send)]
pub trait Maker: Send + Sync {
    fn make_counter(&self, initial: u32) -> Arc<Counter>;
    fn make_greeter(&self, prefix: String) -> Arc<dyn Greeter>;
    fn make_logger(&self, prefix: String) -> Arc<dyn Logger>;
    fn make_host_logger(&self, prefix: String) -> Arc<dyn HostLogger>;
    async fn make_async_logger(&self, prefix: String) -> Arc<dyn Logger>;
    async fn make_async_host_logger(&self, prefix: String) -> Arc<dyn HostLogger>;
    async fn checked_make_async_logger(
        &self,
        prefix: String,
        fail: bool,
    ) -> Result<Arc<dyn Logger>, ProviderError>;
}

pub fn invoke_maker_make_counter(maker: Arc<dyn Maker>, initial: u32) -> Arc<Counter> {
    maker.make_counter(initial)
}

pub fn invoke_maker_make_greeter(maker: Arc<dyn Maker>, prefix: String) -> Arc<dyn Greeter> {
    maker.make_greeter(prefix)
}

pub fn invoke_maker_run_logger(maker: Arc<dyn Maker>, prefix: String, message: String) -> String {
    maker.make_logger(prefix).log(message)
}

pub fn invoke_maker_run_host_logger(maker: Arc<dyn Maker>, prefix: String, name: String) -> String {
    maker.make_host_logger(prefix).greet(name)
}

pub async fn invoke_maker_run_async_logger(
    maker: Arc<dyn Maker>,
    prefix: String,
    message: String,
) -> String {
    maker.make_async_logger(prefix).await.log(message)
}

pub async fn invoke_maker_run_async_host_logger(
    maker: Arc<dyn Maker>,
    prefix: String,
    name: String,
) -> String {
    maker.make_async_host_logger(prefix).await.greet(name)
}

pub async fn invoke_maker_checked_make_async_logger(
    maker: Arc<dyn Maker>,
    prefix: String,
    fail: bool,
    message: String,
) -> Result<String, ProviderError> {
    Ok(maker
        .checked_make_async_logger(prefix, fail)
        .await?
        .log(message))
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import {
    Counter,
    ProviderError,
    englishGreeter,
    invokeMakerMakeCounter,
    invokeMakerMakeGreeter,
    invokeMakerRunAsyncLogger,
    invokeMakerRunAsyncHostLogger,
    invokeMakerCheckedMakeAsyncLogger,
    invokeMakerRunHostLogger,
    invokeMakerRunLogger,
} from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_cb_object_shim.js");
await initBackend(glue);

const maker = {
    makeCounter(initial: number) {
        return Counter.new(initial);
    },
    makeGreeter(prefix: string) {
        return englishGreeter(prefix);
    },
    makeLogger(prefix: string) {
        return {
            log(message: string) {
                return `${prefix}:${message}`;
            },
        };
    },
    makeHostLogger(prefix: string) {
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async makeAsyncLogger(prefix: string) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return {
            log(message: string) {
                return `${prefix}:${message}`;
            },
        };
    },
    async makeAsyncHostLogger(prefix: string) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async checkedMakeAsyncLogger(prefix: string, fail: boolean) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        if (fail) {
            throw new ProviderError("BadValue", "BadValue");
        }
        return {
            log(message: string) {
                return `${prefix}:${message}`;
            },
        };
    },
};

const counter = invokeMakerMakeCounter(maker as any, 10);
counter.inc();
if (counter.value() !== 11) {
    throw new Error(`counter.value()=${counter.value()}`);
}

const greeter = invokeMakerMakeGreeter(maker as any, "Hello");
if (greeter.greet("world") !== "Hello world") {
    throw new Error(`greeter.greet()=${greeter.greet("world")}`);
}

const loggerLog = invokeMakerRunLogger(maker as any, "Log", "world");
if (loggerLog !== "Log:world") {
    throw new Error(`loggerLog=${loggerLog}`);
}

const hostLoggerGreet = invokeMakerRunHostLogger(maker as any, "Host", "world");
if (hostLoggerGreet !== "Host world!") {
    throw new Error(`hostLoggerGreet=${hostLoggerGreet}`);
}

const asyncLogger = await invokeMakerRunAsyncLogger(maker as any, "Async", "world");
if (asyncLogger !== "Async:world") {
    throw new Error(`asyncLogger=${asyncLogger}`);
}

const asyncHostLogger = await invokeMakerRunAsyncHostLogger(maker as any, "AsyncHost", "world");
if (asyncHostLogger !== "AsyncHost world!") {
    throw new Error(`asyncHostLogger=${asyncHostLogger}`);
}

const checkedAsyncLogger = await invokeMakerCheckedMakeAsyncLogger(maker as any, "Checked", false, "world");
if (checkedAsyncLogger !== "Checked:world") {
    throw new Error(`checkedAsyncLogger=${checkedAsyncLogger}`);
}

let checkedAsyncLoggerFailed = false;
try {
    await invokeMakerCheckedMakeAsyncLogger(maker as any, "Checked", true, "world");
} catch (error) {
    checkedAsyncLoggerFailed = true;
    if (!(error instanceof Error) || !String(error.message).includes("BadValue")) {
        throw new Error(`checkedAsyncLogger(true) wrong error: ${String(error)}`);
    }
}
if (!checkedAsyncLoggerFailed) {
    throw new Error("checkedAsyncLogger(true) should throw");
}

counter.dispose();
greeter.dispose();

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_fallible_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_fallible_cb",
        udl: r#"
dictionary Payload {
  u32 left;
  u32 right;
};

[Error]
enum ProviderError {
  "BadValue",
};

callback interface ValueProvider {
  u32 get_value();
  Payload make_payload();
  [Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Throws=ProviderError]
  Payload checked_payload(boolean fail);
  [Throws=ProviderError]
  void checked_void(boolean fail);
};

namespace wasm_fallible_cb {
  u32 invoke_value_provider_get_value(ValueProvider provider);
  Payload invoke_value_provider_make_payload(ValueProvider provider);
  [Throws=ProviderError]
  u32 invoke_value_provider_checked_value(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
  Payload invoke_value_provider_checked_payload(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
  boolean invoke_value_provider_checked_void(ValueProvider provider, boolean fail);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub left: u32,
    pub right: u32,
}

#[derive(Debug)]
pub enum ProviderError {
    BadValue,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadValue => write!(f, "BadValue"),
        }
    }
}

impl std::error::Error for ProviderError {}

pub trait ValueProvider: Send + Sync {
    fn get_value(&self) -> u32;
    fn make_payload(&self) -> Payload;
    fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;
    fn checked_payload(&self, fail: bool) -> Result<Payload, ProviderError>;
    fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;
}

pub fn invoke_value_provider_get_value(provider: Arc<dyn ValueProvider>) -> u32 {
    provider.get_value()
}

pub fn invoke_value_provider_make_payload(provider: Arc<dyn ValueProvider>) -> Payload {
    provider.make_payload()
}

pub fn invoke_value_provider_checked_value(
    provider: Arc<dyn ValueProvider>,
    fail: bool,
) -> Result<u32, ProviderError> {
    provider.checked_value(fail)
}

pub fn invoke_value_provider_checked_payload(
    provider: Arc<dyn ValueProvider>,
    fail: bool,
) -> Result<Payload, ProviderError> {
    provider.checked_payload(fail)
}

pub fn invoke_value_provider_checked_void(
    provider: Arc<dyn ValueProvider>,
    fail: bool,
) -> Result<bool, ProviderError> {
    provider.checked_void(fail)?;
    Ok(true)
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import {
  ProviderError,
  invokeValueProviderCheckedPayload,
  invokeValueProviderCheckedValue,
  invokeValueProviderCheckedVoid,
  invokeValueProviderGetValue,
  invokeValueProviderMakePayload,
} from "./gen/common/api.ts";
import { UniffiError } from "./gen/common/runtime.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_fallible_cb_shim.js");
await initBackend(glue);

const provider = {
  getValue() {
    return 42;
  },
  makePayload() {
    return { left: 7, right: 11 };
  },
  checkedValue(fail: boolean) {
    if (fail) throw new ProviderError("BadValue", "BadValue");
    return 77;
  },
  checkedPayload(fail: boolean) {
    if (fail) throw new ProviderError("BadValue", "BadValue");
    return { left: 13, right: 17 };
  },
  checkedVoid(fail: boolean) {
    if (fail) throw new ProviderError("BadValue", "BadValue");
  },
};

if (invokeValueProviderGetValue(provider as any) !== 42) {
  throw new Error("getValue failed");
}
const payload = invokeValueProviderMakePayload(provider as any);
if (payload.left !== 7 || payload.right !== 11) {
  throw new Error(`makePayload failed: ${JSON.stringify(payload)}`);
}
if (invokeValueProviderCheckedValue(provider as any, false) !== 77) {
  throw new Error("checkedValue(false) failed");
}
const checkedPayload = invokeValueProviderCheckedPayload(provider as any, false);
if (checkedPayload.left !== 13 || checkedPayload.right !== 17) {
  throw new Error(`checkedPayload(false) failed: ${JSON.stringify(checkedPayload)}`);
}
if (invokeValueProviderCheckedVoid(provider as any, false) !== true) {
  throw new Error("checkedVoid(false) failed");
}

for (const [label, fn] of [
  ["checkedValue", () => invokeValueProviderCheckedValue(provider as any, true)],
  ["checkedPayload", () => invokeValueProviderCheckedPayload(provider as any, true)],
  ["checkedVoid", () => invokeValueProviderCheckedVoid(provider as any, true)],
] as const) {
  let threw = false;
  try {
    fn();
  } catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
      throw new Error(`${label} threw wrong type: ${e && (e as Error).message}`);
    }
    if (!String((e as Error).message).includes("BadValue")) {
      throw new Error(`${label} threw wrong message: ${(e as Error).message}`);
    }
  }
  if (!threw) throw new Error(`${label}(true) should throw`);
}

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_fallible_async_callback_trait() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_fallible_async_cb",
        udl: r#"
dictionary Payload {
  u32 left;
  u32 right;
};

[Error]
enum ProviderError {
  "BadValue",
};

[Trait, WithForeign]
interface CheckedWorker {
  [Async, Throws=ProviderError]
  void checked_void(boolean fail);
  [Async, Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Async, Throws=ProviderError]
  Payload checked_record(boolean fail);
};

namespace wasm_fallible_async_cb {
  [Async, Throws=ProviderError]
  boolean invoke_checked_void(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
  u32 invoke_checked_value(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
  Payload invoke_checked_record(CheckedWorker worker, boolean fail);
};
"#,
        biz_deps: "async-trait = \"0.1\"\n",
        shim_deps: "",
        biz_lib: r#"
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub left: u32,
    pub right: u32,
}

#[derive(Debug)]
pub enum ProviderError {
    BadValue,
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadValue => write!(f, "BadValue"),
        }
    }
}

impl std::error::Error for ProviderError {}

#[async_trait::async_trait(?Send)]
pub trait CheckedWorker: Send + Sync {
    async fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;
    async fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;
    async fn checked_record(&self, fail: bool) -> Result<Payload, ProviderError>;
}

pub async fn invoke_checked_void(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<bool, ProviderError> {
    worker.checked_void(fail).await?;
    Ok(true)
}

pub async fn invoke_checked_value(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<u32, ProviderError> {
    worker.checked_value(fail).await
}

pub async fn invoke_checked_record(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<Payload, ProviderError> {
    worker.checked_record(fail).await
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import {
  ProviderError,
  invokeCheckedRecord,
  invokeCheckedValue,
  invokeCheckedVoid,
} from "./gen/common/api.ts";
import { initBackend } from "./gen/browser/index.ts";
import { UniffiError } from "./gen/common/runtime.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_fallible_async_cb_shim.js");
await initBackend(glue);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

function delay(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 1));
}

function makeProvider() {
  const calls: string[] = [];
  return {
    calls,
    provider: {
      async checkedVoid(fail: boolean): Promise<void> {
        await delay();
        calls.push(`void:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
      },
      async checkedValue(fail: boolean): Promise<number> {
        await delay();
        calls.push(`value:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return 77;
      },
      async checkedRecord(fail: boolean): Promise<{ left: number; right: number }> {
        await delay();
        calls.push(`record:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return { left: 7, right: 11 };
      },
    },
  };
}

async function expectTypedError(label: string, fn: () => Promise<unknown>): Promise<void> {
  let threw = false;
  try {
    await fn();
  } catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
      throw new Error(`${label} threw wrong type: ${e && (e as Error).message}`);
    }
    if (!String((e as Error).message).includes("BadValue")) {
      throw new Error(`${label} threw wrong message: ${(e as Error).message}`);
    }
  }
  if (!threw) throw new Error(`${label}(true) should throw`);
}

const { calls, provider } = makeProvider();
assert(await invokeCheckedVoid(provider as any, false) === true, "checkedVoid(false)");
assert(await invokeCheckedValue(provider as any, false) === 77, "checkedValue(false)");
const record = await invokeCheckedRecord(provider as any, false);
assert(record.left === 7 && record.right === 11, `checkedRecord(false)=${JSON.stringify(record)}`);
await expectTypedError("checkedVoid", () => invokeCheckedVoid(provider as any, true));
await expectTypedError("checkedValue", () => invokeCheckedValue(provider as any, true));
await expectTypedError("checkedRecord", () => invokeCheckedRecord(provider as any, true));
assert(
    calls.join(",") === "void:false,value:false,record:false,void:true,value:true,record:true",
    `calls=${calls.join(",")}`
);

console.log("ok");
"#,
        config_toml: None,
        generated_files: EMPTY_GENERATED_FILES,
    });
}

#[test]
fn runs_generated_wasm_shim_custom_types() {
    run_wasm_e2e(WasmE2eSpec {
        name: "wasm_custom",
        udl: r#"
[Custom]
typedef string Email;

dictionary Contact {
  Email primary;
  sequence<Email> aliases;
};

[Trait, WithForeign]
interface EmailFormatter {
  Email format_email(Email value);
  Contact format_contact(Contact value);
};

namespace wasm_custom {
  Email normalize_email(Email value);
  Contact normalize_contact(Contact value);
  sequence<Email> normalize_many(sequence<Email> values);
  Email format_email_with(EmailFormatter formatter, Email value);
  Contact format_contact_with(EmailFormatter formatter, Contact value);
};
"#,
        biz_deps: "",
        shim_deps: "",
        biz_lib: r#"
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniFfiTag;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Email(pub String);
uniffi::custom_type!(Email, String, {
    lower: |value| value.0,
    try_lift: |value| Ok(Email(value)),
});

impl From<Email> for String {
    fn from(value: Email) -> Self {
        value.0
    }
}

impl From<String> for Email {
    fn from(value: String) -> Self {
        Email(value)
    }
}

#[derive(Clone)]
pub struct Contact {
    pub primary: Email,
    pub aliases: Vec<Email>,
}

impl Contact {
    fn normalize(self) -> Self {
        Self {
            primary: normalize_email(self.primary),
            aliases: self.aliases.into_iter().map(normalize_email).collect(),
        }
    }
}

pub fn normalize_email(value: Email) -> Email {
    Email(value.0.trim().to_ascii_lowercase())
}

pub fn normalize_contact(value: Contact) -> Contact {
    value.normalize()
}

pub fn normalize_many(values: Vec<Email>) -> Vec<Email> {
    values.into_iter().map(normalize_email).collect()
}

pub trait EmailFormatter: Send + Sync {
    fn format_email(&self, value: Email) -> Email;
    fn format_contact(&self, value: Contact) -> Contact;
}

pub fn format_email_with(formatter: std::sync::Arc<dyn EmailFormatter>, value: Email) -> Email {
    formatter.format_email(value)
}

pub fn format_contact_with(formatter: std::sync::Arc<dyn EmailFormatter>, value: Contact) -> Contact {
    formatter.format_contact(value).normalize()
}
"#,
        driver_ts: r#"
import { createRequire } from "node:module";
import { initBackend } from "./gen/browser/index.ts";
import {
  formatContactWith,
  formatEmailWith,
  normalizeContact,
  normalizeEmail,
  normalizeMany,
} from "./gen/common/api.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/wasm_custom_shim.js");
await initBackend(glue);

const one = normalizeEmail({ value: "  A@EXAMPLE.COM  " });
if (one.value !== "a@example.com") throw new Error(`normalizeEmail=${JSON.stringify(one)}`);

const contact = normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
if (contact.primary.value !== "root@example.com") throw new Error(`contact.primary=${contact.primary.value}`);
if (contact.aliases[0].value !== "alias@one.com" || contact.aliases[1].value !== "two@example.com") {
  throw new Error(`contact.aliases=${JSON.stringify(contact.aliases)}`);
}

const many = normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
if (many[0].value !== "x@y.com" || many[1].value !== "z@q.com") {
  throw new Error(`normalizeMany=${JSON.stringify(many)}`);
}

const formatter = {
  formatEmail(value: { value: string }) {
    return { value: `${value.value.trim().toUpperCase()}!` };
  },
  formatContact(value: { primary: { value: string }; aliases: Array<{ value: string }> }) {
    return {
      primary: { value: value.primary.value.trim().toUpperCase() },
      aliases: value.aliases.map((alias) => ({ value: alias.value.trim().toUpperCase() })),
    };
  },
};
const formatted = formatEmailWith(formatter, { value: " ada@example.com " });
if (formatted.value !== "ADA@EXAMPLE.COM!") {
  throw new Error(`formatEmailWith=${JSON.stringify(formatted)}`);
}
const formattedContact = formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
if (formattedContact.primary.value !== "root@example.com" || formattedContact.aliases[0].value !== "alias@one.com") {
  throw new Error(`formatContactWith=${JSON.stringify(formattedContact)}`);
}

console.log("ok");
"#,
        config_toml: Some(
            r#"
[bindings.javascript.customTypes.Email]
typeName = "EmailAddress"
imports = [
  "type { EmailAddress } from \"./email.ts\"",
  "{ emailAddressFromString, emailAddressToString } from \"./email.ts\"",
]
intoCustom = "emailAddressFromString({})"
fromCustom = "emailAddressToString({})"
"#,
        ),
        generated_files: &[(
            "common/email.ts",
            r#"
export type EmailAddress = { value: string };
export function emailAddressFromString(value: string): EmailAddress {
  return { value };
}
export function emailAddressToString(value: EmailAddress): string {
  return value.value;
}
"#,
        )],
    });
}

#[test]
fn custom_types_wasm_static_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let (udl, config, _manifest) = write_custom_core_crate(tmp.path());

    let gen_dir = root.join("generated-wasm");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override: Some(config),
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .expect("custom wasm generation should succeed");

    std::fs::write(
        gen_dir.join("common/email.ts"),
        r#"
export type EmailAddress = { value: string };
export function emailAddressFromString(value: string): EmailAddress {
  return { value };
}
export function emailAddressToString(value: EmailAddress): string {
  return value.value;
}
"#,
    )
    .unwrap();

    let api = std::fs::read_to_string(gen_dir.join("common/api.ts")).unwrap();
    assert!(
        api.contains("export type { Email } from \"./custom-types.ts\";"),
        "api.ts should re-export the configured custom type alias:\n{api}"
    );
    assert!(
        api.contains("import { __uniffiLiftCustomEmail, __uniffiLowerCustomEmail } from \"./custom-types.ts\";"),
        "api.ts should import the custom-type helpers:\n{api}"
    );
    assert!(
        !api.contains("EmailAddress"),
        "api.ts should surface the UDL custom type name, not the underlying type name:\n{api}"
    );

    let public_types = std::fs::read_to_string(gen_dir.join("common/public-types.ts")).unwrap();
    assert!(
        public_types.contains("export type { Email } from \"./custom-types.ts\";"),
        "public-types.ts should re-export the configured custom type alias:\n{public_types}"
    );

    let custom_types = std::fs::read_to_string(gen_dir.join("common/custom-types.ts")).unwrap();
    for needle in [
        "type { EmailAddress } from \"./email.ts\"",
        "emailAddressFromString",
        "emailAddressToString",
        "export type Email = EmailAddress;",
        "__uniffiLowerCustomEmail",
        "__uniffiLiftCustomEmail",
    ] {
        assert!(
            custom_types.contains(needle),
            "custom-types.ts missing `{needle}`:\n{custom_types}"
        );
    }

    let records = std::fs::read_to_string(gen_dir.join("common/records.ts")).unwrap();
    assert!(
        records.contains("import type { Email } from \"./custom-types.ts\";"),
        "records.ts should import configured custom types when used in records:\n{records}"
    );

    let wasm_rs_path = std::fs::read_dir(gen_dir.join("browser"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .expect("a .rs wasm shim should exist under browser/");
    let wasm_rs = std::fs::read_to_string(&wasm_rs_path).unwrap();
    assert!(
        !wasm_rs.contains("serde::") && !wasm_rs.contains("serde_wasm_bindgen"),
        "wasm shim must stay serde-free for custom types:\n{wasm_rs}"
    );
    assert!(
        wasm_rs.contains("::uniffi::Lift") && wasm_rs.contains("::uniffi::Lower"),
        "wasm shim should lower/lift custom types through builtin representation:\n{wasm_rs}"
    );
}

/// Parameters for a full Path-A wasm e2e fixture. Shared by the scalar
/// regression test and the non-scalar tests added in the records/enums/
/// objects pass.
struct WasmE2eSpec {
    /// Namespace = crate name = wasm module name.
    name: &'static str,
    /// UDL declaring the public uniffi surface.
    udl: &'static str,
    /// Content of the `biz` crate's `src/lib.rs`.
    biz_lib: &'static str,
    /// Extra lines inserted under the `biz` crate's `[dependencies]`
    /// section (e.g. `serde = ...`).
    biz_deps: &'static str,
    /// Extra lines inserted under the `shim` crate's `[dependencies]`
    /// section.
    shim_deps: &'static str,
    /// TypeScript driver executed under Node. Must print `ok`.
    driver_ts: &'static str,
    /// Optional config override consumed by GenerateJsOptions.config_override.
    config_toml: Option<&'static str>,
    /// Extra files written into the generated tree before the driver runs.
    generated_files: &'static [(&'static str, &'static str)],
}

/// Execute a Path-A wasm e2e run for the given spec. Skips gracefully
/// when any piece of the toolchain (node ≥ 22.6, cargo, wasm32 target,
/// wasm-bindgen CLI) is missing.
fn run_wasm_e2e(spec: WasmE2eSpec) {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping wasm e2e {}: node 22.6+ unavailable", spec.name);
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("skipping wasm e2e {}: cargo not found", spec.name);
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "skipping wasm e2e {}: wasm32-unknown-unknown target not installed",
            spec.name
        );
        return;
    }
    let Some(wasm_bindgen) = which_tool("wasm-bindgen") else {
        eprintln!(
            "skipping wasm e2e {}: wasm-bindgen CLI not found",
            spec.name
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let name = spec.name;
    let shim_name = format!("{name}_shim");
    let uniffi_dep = format!(
        "uniffi = {{ path = {:?} }}",
        workspace_root().join("uniffi").as_str()
    );

    // biz crate skeleton first (needed for UDL loader).
    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl_path = biz.join(format!("src/{name}.udl"));
    std::fs::write(&udl_path, spec.udl).unwrap();
    std::fs::write(
        biz.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{name}"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
{uniffi_dep}
{extra}
"#,
            extra = spec.biz_deps,
            uniffi_dep = uniffi_dep
        ),
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    // Generate JS bindings.
    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let config_override = spec.config_toml.map(|toml| {
        let path = root.join("uniffi.toml");
        std::fs::write(&path, toml).unwrap();
        path
    });
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path.clone(),
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .unwrap_or_else(|e| panic!("bindgen should succeed for {name}: {e:?}"));

    // Real biz lib.rs.
    std::fs::write(biz.join("src/lib.rs"), spec.biz_lib).unwrap();

    // Shim crate.
    let shim = root.join("shim");
    std::fs::create_dir_all(shim.join("src")).unwrap();
    let gen_rs = gen_dir.join(format!("browser/{name}.rs"));
    let shim_src = std::fs::read_to_string(&gen_rs)
        .unwrap_or_else(|_| panic!("generated shim missing at {gen_rs}"));
    // Regression: the wasm shim must NEVER pull in serde in any form.
    // Records/enums cross via explicit `__lower_` / `__lift_` helpers
    // built on `js_sys::Reflect` / `js_sys::Array`.
    for forbidden in [
        "serde_wasm_bindgen",
        "::serde::Serialize",
        "::serde::Deserialize",
        "#[serde(",
        "struct Wasm",
        "enum Wasm",
    ] {
        assert!(
            !shim_src.contains(forbidden),
            "generated wasm shim for `{name}` still contains forbidden pattern `{forbidden}`"
        );
    }
    std::fs::write(shim.join("src/lib.rs"), shim_src).unwrap();
    std::fs::write(
        shim.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{shim_name}"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wasm-bindgen = "=0.2.117"
wasm-bindgen-futures = "0.4"
js-sys = "0.3"
async-trait = "0.1"
{uniffi_dep}
{name} = {{ path = "../biz" }}
{extra}
"#,
            extra = spec.shim_deps,
            uniffi_dep = uniffi_dep
        ),
    )
    .unwrap();
    for (path, contents) in spec.generated_files {
        let full = gen_dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, contents).unwrap();
    }
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"biz\", \"shim\"]\nresolver = \"3\"\n",
    )
    .unwrap();

    // cargo build — deny warnings so cleanup regressions
    // (`non_shorthand_field_patterns`, `unused_parens`, …) fail loudly.
    let build = Command::new(&cargo)
        .args([
            "build",
            "--release",
            "--target",
            "wasm32-unknown-unknown",
            "-p",
            &shim_name,
        ])
        .env("RUSTFLAGS", "-D warnings")
        .current_dir(&root)
        .output()
        .expect("failed to invoke cargo");
    if !build.status.success() {
        panic!(
            "cargo build failed for {name}:\nstderr:\n{}",
            String::from_utf8_lossy(&build.stderr)
        );
    }

    // wasm-bindgen.
    let wasm_file = root.join(format!(
        "target/wasm32-unknown-unknown/release/{shim_name}.wasm"
    ));
    assert!(wasm_file.exists(), "expected wasm artifact at {wasm_file}");
    let pkg = root.join("pkg");
    let bg = Command::new(&wasm_bindgen)
        .args(["--target", "nodejs", "--out-dir"])
        .arg(pkg.as_str())
        .arg(wasm_file.as_str())
        .output()
        .expect("failed to invoke wasm-bindgen");
    if !bg.status.success() {
        panic!(
            "wasm-bindgen failed for {name}:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&bg.stdout),
            String::from_utf8_lossy(&bg.stderr),
        );
    }

    std::fs::write(root.join("driver.ts"), spec.driver_ts).unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&root)
        .output()
        .expect("failed to invoke node");
    if !output.status.success() {
        panic!(
            "wasm e2e {name} driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ok"),
        "wasm e2e {name} driver did not print ok:\n{stdout}"
    );
}

fn which_tool(name: &str) -> Option<std::path::PathBuf> {
    let output = Command::new("which").arg(name).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path.into())
    }
}

fn has_wasm32_target(cargo: &std::path::Path) -> bool {
    // Ask rustup first; fall back to a dry-run build probe if no rustup.
    if let Ok(out) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).contains("wasm32-unknown-unknown");
        }
    }
    // No rustup: optimistically report true and let the build surface
    // the real error; the test skips on build failure anyway.
    let _ = cargo;
    true
}

fn locate_node_with_strip_types() -> Option<std::path::PathBuf> {
    let node = which_node()?;
    let output = Command::new(&node).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let ver = String::from_utf8_lossy(&output.stdout);
    let ver = ver.trim().trim_start_matches('v');
    let mut parts = ver.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    if major > 22 || (major == 22 && minor >= 6) {
        Some(node)
    } else {
        None
    }
}

fn which_node() -> Option<std::path::PathBuf> {
    let output = Command::new("which").arg("node").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        None
    } else {
        Some(path.into())
    }
}

#[test]
fn napi_electron_dispatch_keys_are_mapped_to_camel_case() {
    // `common/api.ts` emits low-level `snake_case` dispatch keys, but
    // napi-rs actually exports them as `lowerCamelCase`. Both the
    // node backend and the electron preload must consume a
    // generator-emitted name map so they cannot drift.
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();

    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    // Cover free function, object constructor (default + named),
    // object method, and a callback-trait-driven function.
    let udl = r#"
interface Counter {
    constructor();
    [Name=with_initial] constructor(u32 value);
    u32 get();
};

namespace mapping {
    string greet_with(string prefix);
    string run_job(string name);
};
"#;
    let udl_path = biz.join("src/mapping.udl");
    std::fs::write(&udl_path, udl).unwrap();
    std::fs::write(
        biz.join("Cargo.toml"),
        r#"[package]
name = "mapping"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
"#,
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("generator should succeed for mapping fixture");

    // Representative dispatch keys. If any of these drift we want a
    // loud failure pointing at this exact file.
    let expected: &[(&str, &str)] = &[
        ("greet_with", "greetWith"),
        ("counter_new", "counterNew"),
        ("counter_with_initial", "counterWithInitial"),
        ("counter_get", "counterGet"),
        ("run_job", "runJob"),
    ];

    let backend_napi = std::fs::read_to_string(gen_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        backend_napi.contains("__uniffiNameMap"),
        "node/backend-napi.ts must carry the generator-emitted name map"
    );
    for (snake, camel) in expected {
        let needle = format!("\"{snake}\": \"{camel}\"");
        assert!(
            backend_napi.contains(&needle),
            "node/backend-napi.ts missing mapping `{snake}` -> `{camel}`:\n{backend_napi}"
        );
    }
    assert!(
        backend_napi.contains("__uniffiIsObjectFreeKey")
            && backend_napi.contains("destructor calls as idempotent no-ops"),
        "node/backend-napi.ts must handle wasm-style object_free keys as documented no-ops:\n{backend_napi}"
    );

    let preload = std::fs::read_to_string(gen_dir.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("__uniffiNameMap"),
        "electron/preload.cjs must carry the same name map"
    );
    for (snake, camel) in expected {
        let needle = format!("\"{snake}\": \"{camel}\"");
        assert!(
            preload.contains(&needle),
            "electron/preload.cjs missing mapping `{snake}` -> `{camel}`:\n{preload}"
        );
    }
    // Electron preload must look up via `resolveMethod`, not raw
    // `addon[msg.method]` — otherwise the map is inert.
    assert!(
        preload.contains("resolveMethod(msg.method)"),
        "electron/preload.cjs must dispatch through resolveMethod()"
    );

    let renderer = std::fs::read_to_string(gen_dir.join("electron/renderer.ts")).unwrap();
    assert!(
        renderer.contains("dropSync(handle: unknown)")
            && renderer.contains("kind: \"drop\"")
            && renderer.contains("method.endsWith(\"_object_free\")"),
        "electron/renderer.ts must translate object_free keys into preload drop messages:\n{renderer}"
    );
}

#[test]
fn napi_electron_do_not_emit_int64_compat_maps() {
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl = r#"
interface Counter {
    [Name=with_initial] constructor(i64 value);
    i64 get();
};

namespace compat {
    [Async]
    u32 slow_add(u32 a, u32 b, u64 delay_ms);
};
"#;
    let udl_path = biz.join("src/compat.udl");
    std::fs::write(&udl_path, udl).unwrap();
    std::fs::write(
        biz.join("Cargo.toml"),
        r#"[package]
name = "compat"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
"#,
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: gen_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("generator should succeed for compat fixture");

    let backend_napi = std::fs::read_to_string(gen_dir.join("node/backend-napi.ts")).unwrap();
    for needle in [
        "__uniffiInt64ArgKinds",
        "__uniffiInt64ReturnKinds",
        "__uniffiLowerInt64ForNapi",
        "__uniffiLiftInt64FromNapi",
    ] {
        assert!(
            !backend_napi.contains(needle),
            "node/backend-napi.ts must not carry `{needle}`:\n{backend_napi}"
        );
    }

    let preload = std::fs::read_to_string(gen_dir.join("electron/preload.cjs")).unwrap();
    for needle in [
        "__uniffiInt64ArgKinds",
        "__uniffiInt64ReturnKinds",
        "__uniffiLowerInt64ForNapi",
        "__uniffiLiftInt64FromNapi",
    ] {
        assert!(
            !preload.contains(needle),
            "electron/preload.cjs must not carry `{needle}`:\n{preload}"
        );
    }
}

#[test]
fn host_crates_napi_raw_addon_is_bigint_native() {
    let Some(node) = which_node() else {
        eprintln!("SKIP host_crates_napi_raw_addon_is_bigint_native: node unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-napi-raw");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_raw_addon_is_bigint_native: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build for raw napi addon failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let dylib = target_dir
        .join("debug")
        .join(cdylib_filename("napi-compat-core-napi"));
    assert!(dylib.exists(), "expected raw cdylib at {}", dylib.display());
    let addon = tmp.path().join("napi_compat.node");
    std::fs::copy(&dylib, &addon).unwrap();

    let driver = tmp.path().join("raw-addon-bigint.cjs");
    std::fs::write(
        &driver,
        format!(
            r#"
const addon = require({addon:?});

function expectBigint(label, value) {{
  if (typeof value !== "bigint") {{
    throw new Error(`${{label}}: expected bigint, got ${{typeof value}}`);
  }}
  return value;
}}

function expectThrow(label, fn_) {{
  let threw = false;
  try {{
    fn_();
  }} catch (e) {{
    threw = true;
    const msg = String((e && e.message) || e);
    if (!/fit into|cannot be converted|BigInt value/i.test(msg)) {{
      throw new Error(`${{label}}: unexpected error ${{msg}}`);
    }}
  }}
  if (!threw) {{
    throw new Error(`${{label}}: expected throw`);
  }}
}}

const u64Max = 18446744073709551615n;
const i64Min = -9223372036854775808n;
const i64Max = 9223372036854775807n;

if (expectBigint("roundtripU64", addon.roundtripU64(u64Max)) !== u64Max) {{
  throw new Error("roundtripU64 failed");
}}
if (expectBigint("roundtripI64(min)", addon.roundtripI64(i64Min)) !== i64Min) {{
  throw new Error("roundtripI64(min) failed");
}}
if (expectBigint("roundtripI64(max)", addon.roundtripI64(i64Max)) !== i64Max) {{
  throw new Error("roundtripI64(max) failed");
}}
if (expectBigint("addU64", addon.addU64(9007199254740993n, 2n)) !== 9007199254740995n) {{
  throw new Error("addU64 above safe integer failed");
}}

expectThrow("u64 overflow", () => addon.roundtripU64(18446744073709551616n));
expectThrow("i64 overflow", () => addon.roundtripI64(9223372036854775808n));

Promise.resolve(addon.asyncRoundtripU64(u64Max)).then((value) => {{
  if (expectBigint("asyncRoundtripU64", value) !== u64Max) {{
    throw new Error("asyncRoundtripU64 failed");
  }}
  console.log("ok");
}}, (err) => {{
  throw err;
}});
"#,
            addon = addon.display().to_string(),
        ),
    )
    .unwrap();
    let output = Command::new(&node)
        .arg(driver.as_path())
        .output()
        .expect("failed to run raw addon bigint driver");
    if !output.status.success() {
        panic!(
            "raw addon bigint driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "raw addon bigint driver did not print ok"
    );
}

#[test]
fn electron_preload_is_syntactically_valid_js() {
    // Keep a `node --check` gate here so template-string churn fails
    // loudly at generator-test time instead of in a downstream
    // Electron smoke.
    let Some(node) = which_node() else {
        eprintln!("skipping: node not available");
        return;
    };

    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    let preload = out_dir.join("electron/preload.cjs");
    let output = Command::new(&node)
        .arg("--check")
        .arg(preload.as_path())
        .output()
        .expect("failed to invoke node --check");
    if !output.status.success() {
        panic!(
            "node --check rejected generated preload.cjs:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn napi_electron_translate_enum_tag_to_type() {
    // common/enums.ts uses `tag`, but napi-rs exports enums with
    // `type`. Both the napi backend and the electron preload must
    // carry the `__uniffiLowerShape` / `__uniffiLiftShape` helpers and
    // actually apply them around the addon boundary.
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    let backend_napi = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    for needle in [
        "__uniffiLowerShape",
        "__uniffiLiftShape",
        "__uniffiIsPlainObject",
        "args.map((a) =>",
    ] {
        assert!(
            backend_napi.contains(needle),
            "node/backend-napi.ts missing `{needle}`:\n{backend_napi}"
        );
    }
    assert!(
        backend_napi.contains("\"type\"") && backend_napi.contains("\"tag\""),
        "backend-napi.ts must reference both `tag` and `type`"
    );

    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    for needle in [
        "__uniffiLowerShape",
        "__uniffiLiftShape",
        "__uniffiIsPlainObject",
        "__uniffiLowerShape(resolveArg(a))",
        "wrapResult(__uniffiLiftShape(raw))",
        "serializeError(__uniffiLiftShape(error))",
    ] {
        assert!(
            preload.contains(needle),
            "electron/preload.cjs missing `{needle}`:\n{preload}"
        );
    }

    // Dynamically load the helpers out of the generated preload and
    // exercise them against plain enum, nested inside a sequence,
    // inside an optional, and
    // an error-with-data shape. This is cheap: we extract the helper
    // block and `eval` it in a local scope.
    let Some(node) = which_node() else {
        eprintln!("skipping helper exec: node not available");
        return;
    };
    let driver = out_dir.join("enum-shape-driver.cjs");
    std::fs::write(
        &driver,
        r#"
const fs = require("node:fs");
const path = require("node:path");
const src = fs.readFileSync(path.resolve(__dirname, "electron/preload.cjs"), "utf8");
// Extract just the helper block — stops at the contextBridge line
// so we do not pull in `electron` at require time.
const start = src.indexOf("function __uniffiIsPlainObject");
const end = src.indexOf("// -------------------------------------------------------------------", start);
if (start < 0 || end < 0) {
    console.error("helper block not found");
    process.exit(2);
}
const block = src.slice(start, end);
const scope = {};
(new Function("scope", block + "\nscope.__uniffiLowerShape = __uniffiLowerShape; scope.__uniffiLiftShape = __uniffiLiftShape;"))(scope);
const { __uniffiLowerShape: lower, __uniffiLiftShape: lift } = scope;

function eq(a, b, label) {
    const ja = JSON.stringify(a);
    const jb = JSON.stringify(b);
    if (ja !== jb) {
        console.error(`FAIL ${label}: ${ja} !== ${jb}`);
        process.exit(1);
    }
}

// plain enum
eq(lower({ tag: "Click", x: 1 }), { type: "Click", x: 1 }, "lower plain enum");
eq(lift({ type: "Click", x: 1 }), { tag: "Click", x: 1 }, "lift plain enum");

// nested in sequence
eq(
    lower([{ tag: "Tick" }, { tag: "Stop" }]),
    [{ type: "Tick" }, { type: "Stop" }],
    "lower seq",
);
eq(
    lift([{ type: "Tick" }, { type: "Stop" }]),
    [{ tag: "Tick" }, { tag: "Stop" }],
    "lift seq",
);

// inside a record
eq(
    lower({ label: "x", event: { tag: "Start" } }),
    { label: "x", event: { type: "Start" } },
    "lower nested",
);

// error-enum payload
eq(
    lift({ type: "BadInput", message: "bad", cause: { type: "InnerFoo" } }),
    { tag: "BadInput", message: "bad", cause: { tag: "InnerFoo" } },
    "lift nested error",
);

// primitives / nulls untouched
eq(lower(null), null, "lower null");
eq(lower(42), 42, "lower num");
eq(lower("hi"), "hi", "lower str");

console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg(driver.as_path())
        .output()
        .expect("failed to run enum-shape driver");
    if !output.status.success() {
        panic!(
            "enum-shape driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "enum-shape driver did not print ok"
    );
}

fn generate_callback_shape_tree(out_dir: &Utf8PathBuf) {
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
crate-type = ["rlib"]

[dependencies]
"#,
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("generator should succeed for callback shape fixture");
}

struct StreamFixture {
    crate_dir: Utf8PathBuf,
    lib_path: Utf8PathBuf,
}

fn build_stream_fixture(root: &std::path::Path) -> Option<StreamFixture> {
    let cargo = match which_tool("cargo") {
        Some(cargo) => cargo,
        None => {
            eprintln!("SKIP stream fixture: cargo unavailable");
            return None;
        }
    };
    let root = Utf8PathBuf::from_path_buf(root.to_path_buf()).unwrap();
    let crate_dir = root.join("stream-core");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let uniffi_dep = workspace_root().join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "stream-core"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

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
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use uniffi::deps::futures_core::Stream;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StreamEvent {
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventId(pub u64);

uniffi::custom_type!(EventId, u64, {
    lower: |value| value.0,
    try_lift: |value| Ok(EventId(value)),
});

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct IdEnvelope {
    pub primary: EventId,
    pub others: Vec<EventId>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum StreamError {
    Boom,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boom => write!(f, "boom"),
        }
    }
}

impl std::error::Error for StreamError {}

struct CountStream {
    next: u32,
    end: u32,
}

impl Stream for CountStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.next >= self.end {
            Poll::Ready(None)
        } else {
            let value = self.next;
            self.next += 1;
            Poll::Ready(Some(Ok(StreamEvent { value })))
        }
    }
}

struct ErrorAfterOne {
    next: u32,
}

struct PendingStream;

impl Stream for PendingStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

impl Stream for ErrorAfterOne {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.next += 1;
        match self.next {
            1 => Poll::Ready(Some(Ok(StreamEvent { value: 7 }))),
            2 => Poll::Ready(Some(Err(StreamError::Boom))),
            _ => Poll::Ready(None),
        }
    }
}

#[uniffi::export]
pub fn count_events(count: u32) -> uniffi::UniFfiStream<StreamEvent, StreamError> {
    Box::pin(CountStream { next: 0, end: count })
}

#[uniffi::export]
pub fn error_after_one() -> Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send + 'static>> {
    Box::pin(ErrorAfterOne { next: 0 })
}

#[uniffi::export]
pub fn pending_events() -> uniffi::UniFfiStream<StreamEvent, StreamError> {
    Box::pin(PendingStream)
}

#[uniffi::export]
pub fn roundtrip_event_id(value: EventId) -> EventId {
    value
}

#[uniffi::export]
pub fn event_id_envelope(value: EventId) -> IdEnvelope {
    IdEnvelope {
        primary: value.clone(),
        others: vec![value, EventId(u64::MAX)],
    }
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    let target_dir = root.join("target-stream-core");
    let output = Command::new(&cargo)
        .args(["build", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml").as_std_path())
        .env("CARGO_TARGET_DIR", target_dir.as_str())
        .env_remove("RUSTFLAGS")
        .output()
        .expect("failed to invoke cargo for stream fixture");
    if !output.status.success() {
        panic!(
            "stream fixture core build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let lib_path = target_dir
        .join("debug")
        .join(cdylib_filename("stream-core"));
    assert!(
        lib_path.exists(),
        "expected stream fixture cdylib at {lib_path}"
    );
    Some(StreamFixture {
        crate_dir,
        lib_path,
    })
}

fn generate_stream_tree(
    fixture: &StreamFixture,
    out_dir: &Utf8PathBuf,
    host_crates: Option<Utf8PathBuf>,
    flavors: Vec<FlavorTarget>,
) {
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: fixture.lib_path.clone(),
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: host_crates.map(|host_crates_dir| HostCrateOptions {
                manifest_path: fixture.crate_dir.join("Cargo.toml"),
                host_crates_dir,
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors,
        },
    )
    .expect("generator should succeed for native stream fixture");
}

struct InputStreamFixture {
    crate_dir: Utf8PathBuf,
    lib_path: Utf8PathBuf,
}

fn build_input_stream_fixture(root: &std::path::Path) -> Option<InputStreamFixture> {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP input stream fixture: cargo unavailable");
        return None;
    };
    let crate_dir = Utf8PathBuf::from_path_buf(root.join("input-stream-core")).unwrap();
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let uniffi_dep = workspace_root().join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "input-stream-core"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib", "rlib"]

[dependencies]
uniffi = {{ path = {:?}, features = ["tokio", "default-async-runtime-tokio", "wasm-unstable-single-threaded"] }}

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
    fmt,
    pin::Pin,
    task::{Context, Poll},
};

use uniffi::deps::futures_core::Stream;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct CounterEvent {
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum StreamError {
    Boom,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boom => write!(f, "boom"),
        }
    }
}

impl std::error::Error for StreamError {}

async fn next_input(
    events: &mut uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Option<Result<CounterEvent, StreamError>> {
    std::future::poll_fn(|cx| Pin::new(&mut *events).poll_next(cx)).await
}

struct RunningSumStream {
    events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
    sum: u32,
    done: bool,
}

impl Stream for RunningSumStream {
    type Item = Result<CounterEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.events).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(event))) => {
                self.sum = self.sum.wrapping_add(event.value);
                Poll::Ready(Some(Ok(CounterEvent { value: self.sum })))
            }
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn sum_input_events(
    mut events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Result<u64, StreamError> {
    let mut sum = 0u64;
    while let Some(event) = next_input(&mut events).await {
        sum = sum.wrapping_add(u64::from(event?.value));
    }
    Ok(sum)
}

#[uniffi::export(async_runtime = "tokio")]
pub async fn take_one_input_event(
    mut events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Result<u32, StreamError> {
    match next_input(&mut events).await {
        Some(Ok(event)) => Ok(event.value),
        Some(Err(error)) => Err(error),
        None => Ok(0),
    }
}

#[uniffi::export]
pub fn running_sum(
    events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> uniffi::UniFfiStream<CounterEvent, StreamError> {
    Box::pin(RunningSumStream {
        events,
        sum: 0,
        done: false,
    })
}

uniffi::setup_scaffolding!();
"#,
    )
    .unwrap();

    let target_dir = root.join("target-input-stream-core");
    let output = Command::new(&cargo)
        .args(["build", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml").as_std_path())
        .env("CARGO_TARGET_DIR", target_dir.as_os_str())
        .env_remove("RUSTFLAGS")
        .output()
        .expect("failed to invoke cargo for input stream fixture");
    if !output.status.success() {
        panic!(
            "input stream fixture core build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let lib_path = Utf8PathBuf::from_path_buf(
        target_dir
            .join("debug")
            .join(cdylib_filename("input-stream-core")),
    )
    .unwrap();
    assert!(
        lib_path.exists(),
        "expected input stream fixture cdylib at {lib_path}"
    );
    Some(InputStreamFixture {
        crate_dir,
        lib_path,
    })
}

fn generate_input_stream_tree(
    fixture: &InputStreamFixture,
    out_dir: &Utf8PathBuf,
    host_crates: Option<Utf8PathBuf>,
    flavors: Vec<FlavorTarget>,
) {
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: fixture.lib_path.clone(),
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: host_crates.map(|host_crates_dir| HostCrateOptions {
                manifest_path: fixture.crate_dir.join("Cargo.toml"),
                host_crates_dir,
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors,
        },
    )
    .expect("generator should succeed for JavaScript input stream fixture");
}

#[test]
fn js_async_iterable_runtime_stub_contract() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping: node with --experimental-strip-types not available");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let common = root.join("common");
    std::fs::create_dir_all(&common).unwrap();
    std::fs::write(
        common.join("runtime.ts"),
        include_str!("../../../uniffi_runtime_javascript/typescript/src/runtime.ts"),
    )
    .unwrap();
    std::fs::write(
        common.join("api.ts"),
        r#"
import { __call, __callAsync, createUniffiAsyncIterable } from "./runtime.ts";

export interface StreamEvent {
  value: number;
}

export function countEvents(count: number): AsyncIterable<StreamEvent> {
  const __handle = __call<any>("count_events", count);
  return createUniffiAsyncIterable<StreamEvent>({
    handle: __handle,
    next: async (__streamHandle: unknown): Promise<StreamEvent | null> => {
      const __next = await __callAsync<{ done: boolean; value?: any }>("count_events_stream_next", __streamHandle);
      if (__next == null || __next.done === true) return null;
      return { value: __next.value.value } as StreamEvent;
    },
    cancel: (__streamHandle: unknown): void => {
      __call<void>("count_events_stream_cancel", __streamHandle);
    },
  });
}
"#,
    )
    .unwrap();
    std::fs::write(
        root.join("driver.ts"),
        r#"
import { __installBackend, UniffiError } from "./common/runtime.ts";
import { countEvents } from "./common/api.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

let cancelCount = 0;
let nextId = 1;
const streams = new Map<string, Array<unknown>>();

__installBackend({
  count_events(count: number) {
    const handle = `h${nextId++}`;
    streams.set(handle, Array.from({ length: count }, (_, value) => ({ value })));
    return handle;
  },
  async count_events_stream_next(handle: string) {
    const values = streams.get(handle);
    if (!values) return { done: true };
    const value = values.shift();
    return value === undefined ? { done: true } : { done: false, value };
  },
  count_events_stream_cancel(handle: string) {
    cancelCount += 1;
    streams.delete(handle);
  },
});

const values: number[] = [];
for await (const event of countEvents(3)) values.push(event.value);
assert(values.join(",") === "0,1,2", `for-await values ${values}`);

const beforeBreak = cancelCount;
for await (const event of countEvents(10)) {
  assert(event.value === 0, "break first value");
  break;
}
assert(cancelCount === beforeBreak + 1, "break should cancel once");

const manual = countEvents(10)[Symbol.asyncIterator]();
assert((await manual.next()).done === false, "manual first next");
await manual.return?.();
await manual.return?.();
assert(cancelCount === beforeBreak + 2, "manual return should be idempotent");
assert((await manual.next()).done === true, "next after return done");

__installBackend({
  count_events() { return "err"; },
  async count_events_stream_next() {
    throw new UniffiError({ errorName: "StreamError", variant: "Boom", message: "boom" });
  },
  count_events_stream_cancel() { cancelCount += 1; },
});
let threw = false;
try {
  for await (const _ of countEvents(1)) {}
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "stream error type");
  assert((error as UniffiError).errorName === "StreamError", "stream error name");
}
assert(threw, "stream error should throw");

let resolvePending: ((value: unknown) => void) | null = null;
__installBackend({
  count_events() { return "pending"; },
  count_events_stream_next() {
    return new Promise((resolve) => { resolvePending = resolve; });
  },
  count_events_stream_cancel() { cancelCount += 1; },
});
const concurrent = countEvents(1)[Symbol.asyncIterator]();
const pending = concurrent.next();
let concurrentRejected = false;
try {
  await concurrent.next();
} catch (error) {
  concurrentRejected = true;
  assert(error instanceof UniffiError, "concurrent next error type");
  assert((error as UniffiError).errorName === "UniffiStreamConcurrentNext", "concurrent next error name");
}
assert(concurrentRejected, "concurrent next should reject");
resolvePending?.({ done: false, value: { value: 99 } });
assert((await pending).value.value === 99, "pending value");
await concurrent.return?.();

const single = countEvents(1);
single[Symbol.asyncIterator]();
let consumedRejected = false;
try {
  single[Symbol.asyncIterator]();
} catch (error) {
  consumedRejected = true;
  assert(error instanceof UniffiError, "consumed error type");
  assert((error as UniffiError).errorName === "UniffiStreamConsumed", "consumed error name");
}
assert(consumedRejected, "second iterator should throw");

console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&root)
        .output()
        .expect("failed to run runtime stream driver");
    if !output.status.success() {
        panic!(
            "runtime stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "runtime stream driver did not print ok"
    );
}

#[test]
fn js_async_iterable_stream_stub_contract() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping: node with --experimental-strip-types not available");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_stream_tree(
        &fixture,
        &out_dir,
        None,
        vec![
            FlavorTarget::Wasm,
            FlavorTarget::Napi,
            FlavorTarget::Electron,
        ],
    );

    let api = std::fs::read_to_string(out_dir.join("common/api.ts")).unwrap();
    for needle in [
        "createUniffiAsyncIterable<StreamEvent>",
        "__call<any>(\"count_events\"",
        "__callAsync<{ done: boolean; value?: any }>(\"count_events_stream_next\"",
        "__call<void>(\"count_events_stream_cancel\"",
        "return { value: __next.value.value } as StreamEvent;",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts should expose stream async iterable contract via `{needle}`:\n{api}"
        );
    }
    assert!(
        !api.contains("StreamError"),
        "common/api.ts should not import unused stream error types:\n{api}"
    );
    let backend = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        backend.contains("\"count_events_stream_next\": \"countEventsStreamNext\"")
            && backend.contains("\"count_events_stream_cancel\": \"countEventsStreamCancel\""),
        "napi backend name map should include stream next/cancel:\n{backend}"
    );
    let renderer = std::fs::read_to_string(out_dir.join("electron/renderer.ts")).unwrap();
    assert!(
        renderer.contains("\"count_events_stream_next\"")
            && !renderer.contains("\"count_events_stream_cancel\""),
        "electron renderer should dispatch next asynchronously and leave cancel sync:\n{renderer}"
    );
    let wasm_rs = std::fs::read_to_string(out_dir.join("browser/stream_core.rs")).unwrap();
    assert!(
        wasm_rs.contains("RustStreamRegistry")
            && wasm_rs.contains("pub async fn count_events_stream_next")
            && wasm_rs.contains("pub fn count_events_stream_cancel"),
        "wasm shim should emit stream start/next/cancel:\n{wasm_rs}"
    );
    let napi_rs = std::fs::read_to_string(out_dir.join("node/stream_core.rs")).unwrap();
    assert!(
        napi_rs.contains("RustStreamRegistry")
            && napi_rs.contains("pub async fn count_events_stream_next")
            && napi_rs.contains("pub fn count_events_stream_cancel"),
        "napi shim should emit stream start/next/cancel:\n{napi_rs}"
    );

    std::fs::write(
        out_dir.join("driver.ts"),
        r#"
import { __installBackend, UniffiError } from "./common/runtime.ts";
import { countEvents, errorAfterOne } from "./common/api.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

let cancelCount = 0;
let nextId = 1;
const streams = new Map<string, { values: Array<unknown>; errorAt?: number; nextCalls: number }>();

__installBackend({
  count_events(count: number) {
    const handle = `h${nextId++}`;
    streams.set(handle, {
      values: Array.from({ length: count }, (_, value) => ({ value })),
      nextCalls: 0,
    });
    return handle;
  },
  async count_events_stream_next(handle: string) {
    const stream = streams.get(handle);
    if (!stream) return { done: true };
    stream.nextCalls += 1;
    const value = stream.values.shift();
    return value === undefined ? { done: true } : { done: false, value };
  },
  count_events_stream_cancel(handle: string) {
    cancelCount += 1;
    streams.delete(handle);
  },
  error_after_one() {
    const handle = `e${nextId++}`;
    streams.set(handle, { values: [{ value: 7 }], errorAt: 2, nextCalls: 0 });
    return handle;
  },
  async error_after_one_stream_next(handle: string) {
    const stream = streams.get(handle);
    if (!stream) return { done: true };
    stream.nextCalls += 1;
    if (stream.errorAt === stream.nextCalls) {
      throw new UniffiError({ errorName: "StreamError", variant: "Boom", message: "boom" });
    }
    const value = stream.values.shift();
    return value === undefined ? { done: true } : { done: false, value };
  },
  error_after_one_stream_cancel(handle: string) {
    cancelCount += 1;
    streams.delete(handle);
  },
});

const values: number[] = [];
for await (const event of countEvents(3)) {
  values.push(event.value);
}
assert(values.join(",") === "0,1,2", `for-await values ${values}`);

let threw = false;
try {
  for await (const event of errorAfterOne()) {
    values.push(event.value);
  }
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "stream error should be wrapped");
  assert((error as UniffiError).errorName === "StreamError", "stream error name");
}
assert(threw, "stream error should throw");

const beforeBreak = cancelCount;
for await (const event of countEvents(10)) {
  assert(event.value === 0, "break first value");
  break;
}
assert(cancelCount === beforeBreak + 1, "break should cancel once");

const manual = countEvents(10)[Symbol.asyncIterator]();
assert((await manual.next()).done === false, "manual first next");
await manual.return?.();
await manual.return?.();
assert(cancelCount === beforeBreak + 2, "manual return should be idempotent");
assert((await manual.next()).done === true, "next after return done");

let resolvePending: ((value: unknown) => void) | null = null;
__installBackend({
  count_events() { return "pending"; },
  count_events_stream_next() {
    return new Promise((resolve) => { resolvePending = resolve; });
  },
  count_events_stream_cancel() { cancelCount += 1; },
});
const concurrent = countEvents(1)[Symbol.asyncIterator]();
const pending = concurrent.next();
let concurrentRejected = false;
try {
  await concurrent.next();
} catch (error) {
  concurrentRejected = true;
  assert(error instanceof UniffiError, "concurrent next error type");
  assert((error as UniffiError).errorName === "UniffiStreamConcurrentNext", "concurrent next error name");
}
assert(concurrentRejected, "concurrent next should reject");
resolvePending?.({ done: false, value: { value: 99 } });
assert((await pending).value.value === 99, "pending value");
await concurrent.return?.();

const single = countEvents(1);
single[Symbol.asyncIterator]();
let consumedRejected = false;
try {
  single[Symbol.asyncIterator]();
} catch (error) {
  consumedRejected = true;
  assert(error instanceof UniffiError, "consumed error type");
  assert((error as UniffiError).errorName === "UniffiStreamConsumed", "consumed error name");
}
assert(consumedRejected, "second iterator should throw");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to run stream stub driver");
    if !output.status.success() {
        panic!(
            "stream stub driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "stream stub driver did not print ok"
    );
}

#[test]
fn harmony_stream_fallback_static_and_runtime_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_stream_tree(&fixture, &out_dir, None, vec![FlavorTarget::Harmony]);

    let contract: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out_dir.join("harmony/stream_core.ohos-facade.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(contract["schemaVersion"], 2);
    assert_eq!(contract["outputStreams"].as_array().unwrap().len(), 3);
    assert_eq!(
        contract["outputStreams"][0]["eventsFactory"],
        "countEventsEvents"
    );
    assert_eq!(
        contract["outputStreams"][0]["streamFactory"],
        "countEventsStream"
    );
    assert!(contract["inputStreams"].as_array().unwrap().is_empty());

    let index = std::fs::read_to_string(out_dir.join("harmony/index.ts")).unwrap();
    assert!(
        index.contains("export * from \"./stream.ts\";"),
        "harmony index should re-export stream fallback helpers:\n{index}"
    );

    let stream = std::fs::read_to_string(out_dir.join("harmony/stream.ts")).unwrap();
    for needle in [
        "export interface UniFfiStream<T>",
        "next(): Promise<IteratorResult<T>>;",
        "cancel(): Promise<void>;",
        "export function toUniFfiStream<T>(source: AsyncIterable<T>): UniFfiStream<T>",
        "source[Symbol.asyncIterator]()",
        "returnFn.call(iterator)",
        "export function countEventsStream(count: number): UniFfiStream<StreamEvent>",
        "return toUniFfiStream(countEvents(count));",
        "export function errorAfterOneStream(): UniFfiStream<StreamEvent>",
        "import type { StreamEvent } from \"../common/public-types.ts\";",
    ] {
        assert!(
            stream.contains(needle),
            "harmony stream helper missing `{needle}`:\n{stream}"
        );
    }
    assert!(
        !stream.contains("unknown") && !contains_dynamic_type_word(&stream),
        "harmony stream helper should avoid explicit any/unknown:\n{stream}"
    );
    assert!(
        !stream.contains("__call") && !stream.contains("_stream_next"),
        "harmony fallback should not expose raw stream ABI keys:\n{stream}"
    );

    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping harmony stream runtime driver: node with --experimental-strip-types not available");
        return;
    };
    std::fs::write(
        out_dir.join("harmony-stream-driver.ts"),
        r#"
import { toUniFfiStream } from "./harmony/stream.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

let returnCount = 0;
const source = {
  [Symbol.asyncIterator](): AsyncIterator<number> {
    let nextValue = 0;
    return {
      async next(): Promise<IteratorResult<number>> {
        if (nextValue >= 3) return { done: true, value: undefined as number };
        return { done: false, value: nextValue++ };
      },
      async return(): Promise<IteratorResult<number>> {
        returnCount += 1;
        return { done: true, value: undefined as number };
      },
    };
  },
};

const stream = toUniFfiStream(source);
const one = await stream.next();
assert(one.done === false && one.value === 0, "first next");
await stream.cancel();
await stream.cancel();
assert(returnCount === 1, `cancel idempotent ${returnCount}`);
const afterCancel = await stream.next();
assert(afterCancel.done === true, "next after cancel done");

let threw = false;
const bad = toUniFfiStream({
  [Symbol.asyncIterator](): AsyncIterator<number> {
    return {
      async next(): Promise<IteratorResult<number>> {
        throw new Error("boom");
      },
    };
  },
});
try {
  await bad.next();
} catch (error) {
  threw = error instanceof Error && error.message === "boom";
}
assert(threw, "error rejection propagates");

console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("harmony-stream-driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to run harmony stream driver");
    assert!(
        output.status.success(),
        "harmony stream driver failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("ok"),
        "harmony stream driver did not print ok"
    );
}

#[test]
fn input_stream_runtime_helper_contract() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping: node with --experimental-strip-types not available");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let common = root.join("common");
    std::fs::create_dir_all(&common).unwrap();
    std::fs::write(
        common.join("runtime.ts"),
        include_str!("../../../uniffi_runtime_javascript/typescript/src/runtime.ts"),
    )
    .unwrap();
    std::fs::write(
        root.join("driver.ts"),
        r#"
import {
  cancelUniffiInputStream,
  createUniffiInputStream,
  nextUniffiInputStream,
  UniffiError,
} from "./common/runtime.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const marker = createUniffiInputStream(
  (async function* () {
    yield { value: 1 };
    yield { value: 2 };
  })(),
  {
    lowerItem: (event: { value: number }) => ({ value: event.value }),
    lowerError: (error: unknown) => error,
    errorShape: "flat",
  },
);
const first = await marker.next(marker.handle);
assert(first.ok === true && first.done === false && first.value.value === 1, "first value");
const second = await nextUniffiInputStream(marker.handle);
assert(second.ok === true && second.done === false && second.value.value === 2, "second value");
const done = await marker.next(marker.handle);
assert(done.ok === true && done.done === true, "done");

let returnCount = 0;
const cancellable = createUniffiInputStream(
  {
    [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
      let sent = false;
      return {
        async next(): Promise<IteratorResult<{ value: number }>> {
          if (sent) return { done: true, value: undefined as any };
          sent = true;
          return { done: false, value: { value: 7 } };
        },
        async return(): Promise<IteratorResult<{ value: number }>> {
          returnCount += 1;
          return { done: true, value: undefined as any };
        },
      };
    },
  },
  {
    lowerItem: (event: { value: number }) => ({ value: event.value }),
    lowerError: (error: unknown) => error,
    errorShape: "flat",
  },
);
await cancellable.next(cancellable.handle);
cancellable.cancel(cancellable.handle);
cancellable.cancel(cancellable.handle);
await new Promise((resolve) => setTimeout(resolve, 0));
assert(returnCount === 1, `cancel should call return once, got ${returnCount}`);

const failing = createUniffiInputStream(
  {
    [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
      return {
        async next(): Promise<IteratorResult<{ value: number }>> {
          throw new UniffiError({ errorName: "StreamError", variant: "Boom", message: "boom" });
        },
      };
    },
  },
  {
    lowerItem: (event: { value: number }) => ({ value: event.value }),
    lowerError: (error: unknown) => error,
    errorShape: "flat",
  },
);
const failed = await failing.next(failing.handle);
assert(failed.ok === false && failed.error === "Boom", "typed flat error payload");

let resolvePending: ((value: IteratorResult<{ value: number }>) => void) | null = null;
const pending = createUniffiInputStream(
  {
    [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
      return {
        next(): Promise<IteratorResult<{ value: number }>> {
          return new Promise((resolve) => { resolvePending = resolve; });
        },
      };
    },
  },
  {
    lowerItem: (event: { value: number }) => ({ value: event.value }),
    lowerError: (error: unknown) => error,
    errorShape: "flat",
  },
);
const firstPending = pending.next(pending.handle);
let concurrentRejected = false;
try {
  await pending.next(pending.handle);
} catch (error) {
  concurrentRejected = true;
  assert(error instanceof UniffiError, "concurrent error type");
  assert((error as UniffiError).errorName === "UniffiInputStreamConcurrentNext", "concurrent error name");
}
assert(concurrentRejected, "concurrent next should reject");
resolvePending?.({ done: false, value: { value: 99 } });
const pendingValue = await firstPending;
assert(pendingValue.ok === true && pendingValue.done === false && pendingValue.value.value === 99, "pending value");
await cancelUniffiInputStream(pending.handle);

console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&root)
        .output()
        .expect("failed to run input stream runtime driver");
    if !output.status.success() {
        panic!(
            "input stream runtime driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "input stream runtime driver did not print ok"
    );
}

#[test]
fn input_stream_bidi_static_generation_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_input_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_input_stream_tree(
        &fixture,
        &out_dir,
        None,
        vec![
            FlavorTarget::Wasm,
            FlavorTarget::Napi,
            FlavorTarget::Electron,
            FlavorTarget::Harmony,
        ],
    );

    let contract: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out_dir.join("harmony/input_stream_core.ohos-facade.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(contract["schemaVersion"], 2);
    assert_eq!(contract["inputStreams"].as_array().unwrap().len(), 1);
    let factory = contract["inputStreams"][0]["factory"].as_str().unwrap();
    let input_suffix = contract["inputStreams"][0]["suffix"]
        .as_str()
        .unwrap()
        .to_string();
    let input_next_type = contract["inputStreams"][0]["nextType"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(factory.starts_with("createItemRecordCounterEventErrorEnumStreamErrorFingerprint"));
    assert!(factory.ends_with("InputChannel"));
    assert_eq!(contract["inputStreams"][0]["itemType"]["kind"], "named");
    assert_eq!(contract["inputStreams"][0]["errorType"]["kind"], "named");
    assert_eq!(
        contract["outputStreams"][0]["eventsFactory"],
        "runningSumEvents"
    );
    let extra_types =
        std::fs::read_to_string(out_dir.join("harmony/input_stream_core.ohos-extra-types.d.ts"))
            .unwrap();
    assert!(extra_types.contains("export interface __UniffiInputStream<T>"));
    assert!(extra_types.contains("next(error: Error | null, handle: number): Promise<T>"));

    let api = std::fs::read_to_string(out_dir.join("common/api.ts")).unwrap();
    for needle in [
        "createUniffiInputStream",
        "export async function sumInputEvents(events: AsyncIterable<CounterEvent>): Promise<bigint>",
        "createUniffiInputStream(events, {",
        "lowerItem: (__uniffiInputValue0: any) => ({ value: __uniffiInputValue0.value })",
        "lowerError: (__uniffiInputError0: unknown) => __uniffiInputError0",
        "errorShape: \"flat\"",
        "export function runningSum(events: AsyncIterable<CounterEvent>): AsyncIterable<CounterEvent>",
        "const __handle = __call<any>(\"running_sum\", createUniffiInputStream(events, {",
        "return createUniffiAsyncIterable<CounterEvent>({",
        "next: async (__streamHandle: unknown): Promise<CounterEvent | null> =>",
        "__call<void>(\"running_sum_stream_cancel\", __streamHandle);",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts missing input stream contract `{needle}`:\n{api}"
        );
    }

    let backend = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        backend.contains("__uniffiInputStream")
            && backend.contains(
                "return { handle: marker.handle, next: marker.next, cancel: marker.cancel };"
            ),
        "napi backend should coerce input stream markers:\n{backend}"
    );
    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("__uniffiInputStream")
            && preload
                .contains("return { handle: arg.handle, next: arg.next, cancel: arg.cancel };"),
        "electron preload should forward input stream markers:\n{preload}"
    );
    let napi_rs = std::fs::read_to_string(out_dir.join("node/input_stream_core.rs")).unwrap();
    for needle in [
        "pub struct __UniffiInputStream<NextResult: 'static + FromNapiValue>".to_string(),
        input_next_type.clone(),
        "impl ::uniffi::ForeignInputStreamOps".to_string(),
        "::uniffi::UniFfiInputStream::from_handle_and_ops".to_string(),
        "ThreadsafeFunctionCallMode::NonBlocking".to_string(),
        "pub fn running_sum(".to_string(),
        "events: __UniffiInputStream<".to_string(),
        "pub async fn running_sum_stream_next(".to_string(),
    ] {
        assert!(
            napi_rs.contains(&needle),
            "napi bridge missing input stream lowering `{needle}`:\n{napi_rs}"
        );
    }
    assert!(
        napi_rs.contains(&format!("__UniffiInputStream{input_suffix}Ops")),
        "napi bridge must use the canonical contract suffix `{input_suffix}`:\n{napi_rs}"
    );
    let wasm_rs = std::fs::read_to_string(out_dir.join("browser/input_stream_core.rs")).unwrap();
    for needle in [
        "__UniffiInputStreamCounterEventStreamErrorOps",
        "impl ::uniffi::ForeignInputStreamOps",
        "fn __lower_input_stream_counter_event_stream_error",
        "::uniffi::UniFfiInputStream::from_handle_and_ops",
        "::wasm_bindgen_futures::JsFuture::from(__promise).await",
        "pub fn running_sum(events: JsValue) -> Result<u64, JsError>",
        "pub async fn running_sum_stream_next(handle: u64) -> Result<JsValue, JsError>",
    ] {
        assert!(
            wasm_rs.contains(needle),
            "wasm bridge missing input stream lowering `{needle}`:\n{wasm_rs}"
        );
    }
    assert!(
        !wasm_rs.contains("input streams are not wired into wasm codegen yet"),
        "wasm shim should no longer emit the input stream unsupported stub:\n{wasm_rs}"
    );
}

#[test]
fn host_crates_napi_runs_stream_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_stream_fixture: node 22.6+ unavailable");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Napi, FlavorTarget::Electron],
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("target-napi-stream");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(output) => output,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_stream_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on stream napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let built_lib = target_dir
        .join("debug")
        .join(cdylib_filename("stream-core-napi"));
    assert!(
        built_lib.exists(),
        "expected built stream addon at {}",
        built_lib.display()
    );
    std::fs::copy(&built_lib, out_dir.join("node/stream_core.node")).unwrap();

    std::fs::write(
        out_dir.join("stream-driver.ts"),
        r#"
import { countEvents, errorAfterOne, eventIdEnvelope, pendingEvents, roundtripEventId, UniffiError } from "./node/index.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const values: number[] = [];
for await (const event of countEvents(3)) {
  values.push(event.value);
}
assert(values.join(",") === "0,1,2", `napi stream values ${values}`);

const manual = countEvents(2)[Symbol.asyncIterator]();
assert((await manual.next()).value.value === 0, "manual first");
await manual.return?.();
await manual.return?.();
assert((await manual.next()).done === true, "manual after return done");

let errorValues = 0;
let threw = false;
try {
  for await (const event of errorAfterOne()) {
    errorValues += event.value;
  }
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "napi stream error should be UniffiError");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `napi stream error message ${(error as Error).message}`);
}
assert(errorValues === 7, `napi stream error first value ${errorValues}`);
assert(threw, "napi stream error should throw");

const pendingManual = pendingEvents()[Symbol.asyncIterator]();
const pendingNext = pendingManual.next();
await pendingManual.return?.();
const pendingResult = await Promise.race([
  pendingNext,
  new Promise<string>((resolve): void => { setTimeout((): void => resolve("timeout"), 1000); })
]);
assert(pendingResult !== "timeout" && pendingResult.done === true, "napi pending next should settle after cancel");
assert((await pendingManual.next()).done === true, "napi pending registry should remain closed");

const aboveSafe = 9007199254740993n;
const u64Max = 18446744073709551615n;
assert(roundtripEventId(aboveSafe) === aboveSafe, "napi custom u64 above safe integer");
assert(roundtripEventId(u64Max) === u64Max, "napi custom u64 max");
const idEnvelope = eventIdEnvelope(aboveSafe);
assert(idEnvelope.primary === aboveSafe && idEnvelope.others[1] === u64Max, "napi composite custom u64");
let overflowRejected = false;
try { roundtripEventId(18446744073709551616n); } catch (_error) { overflowRejected = true; }
assert(overflowRejected, "napi custom u64 overflow");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("stream-driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to run napi stream driver");
    if !output.status.success() {
        panic!(
            "napi stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "napi stream driver did not print ok"
    );
}

#[test]
fn host_crates_napi_runs_input_stream_bidi_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_input_stream_fixture: node 22.6+ unavailable");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_input_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_input_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Napi, FlavorTarget::Electron],
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("target-napi-input-stream");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(output) => output,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_input_stream_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on input stream napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let built_lib = target_dir
        .join("debug")
        .join(cdylib_filename("input-stream-core-napi"));
    assert!(
        built_lib.exists(),
        "expected built input stream addon at {}",
        built_lib.display()
    );
    std::fs::copy(&built_lib, out_dir.join("node/input_stream_core.node")).unwrap();

    std::fs::write(
        out_dir.join("input-stream-driver.ts"),
        r#"
import { runningSum, sumInputEvents, takeOneInputEvent, StreamError, UniffiError } from "./node/index.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}

const sum = await sumInputEvents(events());
assert(sum === 6n, `input stream sum ${sum}`);

const sums: number[] = [];
for await (const event of runningSum(events())) {
  sums.push(event.value);
}
assert(sums.join(",") === "1,3,6", `bidi running sums ${sums}`);

let returnCount = 0;
const cancellable = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    let sent = false;
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        if (sent) return { done: true, value: undefined as any };
        sent = true;
        return { done: false, value: { value: 41 } };
      },
      async return(): Promise<IteratorResult<{ value: number }>> {
        returnCount += 1;
        return { done: true, value: undefined as any };
      },
    };
  },
};
const one = await takeOneInputEvent(cancellable);
assert(one === 41, `take one ${one}`);
await new Promise((resolve) => setTimeout(resolve, 20));
assert(returnCount === 1, `Rust drop should call iterator.return once, got ${returnCount}`);

let sharedReturnCount = 0;
let sharedIssued = false;
let settleShared: ((value: IteratorResult<{ value: number }>) => void) | null = null;
const sharedIterator: AsyncIterator<{ value: number }> = {
  next(): Promise<IteratorResult<{ value: number }>> {
    if (!sharedIssued) {
      sharedIssued = true;
      return Promise.resolve({ done: false, value: { value: 61 } });
    }
    return new Promise<IteratorResult<{ value: number }>>((resolve): void => { settleShared = resolve; });
  },
  return(): Promise<IteratorResult<{ value: number }>> {
    sharedReturnCount += 1;
    if (settleShared !== null) {
      settleShared({ done: true, value: undefined as any });
      settleShared = null;
    }
    return Promise.resolve({ done: true, value: undefined as any });
  },
};
const sharedSource = { [Symbol.asyncIterator](): AsyncIterator<{ value: number }> { return sharedIterator; } };
const sharedA = takeOneInputEvent(sharedSource);
const sharedB = takeOneInputEvent(sharedSource);
const sharedResults = await Promise.race([
  Promise.all([sharedA, sharedB]),
  new Promise<string>((resolve): void => { setTimeout((): void => resolve('timeout'), 1000); })
]);
assert(sharedResults !== 'timeout', 'two real Rust input consumers did not settle');
assert((sharedResults as number[]).sort().join(',') === '0,61', `shared consumer results ${sharedResults}`);
assert(sharedReturnCount >= 1, `shared logical input was not closed ${sharedReturnCount}`);

let breakReturnCount = 0;
const breakable = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    let next = 1;
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        return { done: false, value: { value: next++ } };
      },
      async return(): Promise<IteratorResult<{ value: number }>> {
        breakReturnCount += 1;
        return { done: true, value: undefined as any };
      },
    };
  },
};
for await (const event of runningSum(breakable)) {
  assert(event.value === 1, `bidi first value before break ${event.value}`);
  break;
}
await new Promise((resolve) => setTimeout(resolve, 20));
assert(breakReturnCount === 1, `bidi output break should cancel input once, got ${breakReturnCount}`);

const failing = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        throw new StreamError("boom", "Boom");
      },
    };
  },
};
let threw = false;
try {
  await sumInputEvents(failing);
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `input stream error message ${(error as Error).message}`);
}
assert(threw, "input stream error should throw");

let streamThrew = false;
try {
  for await (const _ of runningSum(failing)) {}
} catch (error) {
  streamThrew = true;
  assert(error instanceof UniffiError, "bidi input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `bidi input stream error message ${(error as Error).message}`);
}
assert(streamThrew, "bidi input stream error should throw from output iterator");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("input-stream-driver.ts")
        .current_dir(&out_dir)
        .output()
        .expect("failed to run napi input stream driver");
    if !output.status.success() {
        panic!(
            "napi input stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "napi input stream driver did not print ok"
    );
}

#[test]
fn host_crates_wasm_input_stream_bidi_runs_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_wasm_runs_input_stream_fixture: node 22.6+ unavailable");
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP host_crates_wasm_runs_input_stream_fixture: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP host_crates_wasm_runs_input_stream_fixture: wasm32-unknown-unknown target unavailable"
        );
        return;
    }
    let Some(wasm_bindgen) = which_tool("wasm-bindgen") else {
        eprintln!("SKIP host_crates_wasm_runs_input_stream_fixture: wasm-bindgen CLI unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_input_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_input_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Wasm],
    );

    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("target-wasm-input-stream");
    let build = Command::new(&cargo)
        .args([
            "build",
            "--manifest-path",
            manifest.as_str(),
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", "-D warnings")
        .output()
        .expect("failed to invoke cargo for wasm input stream host");
    if !build.status.success() {
        panic!(
            "cargo build on input stream wasm host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let wasm_file = target_dir
        .join("wasm32-unknown-unknown/release")
        .join("input_stream_core_wasm.wasm");
    assert!(
        wasm_file.exists(),
        "expected built input stream wasm at {}",
        wasm_file.display()
    );
    let pkg = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let bg = Command::new(&wasm_bindgen)
        .args(["--target", "nodejs", "--out-dir"])
        .arg(pkg.as_str())
        .arg(wasm_file.as_path())
        .output()
        .expect("failed to invoke wasm-bindgen for input stream fixture");
    if !bg.status.success() {
        panic!(
            "wasm-bindgen input stream fixture failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&bg.stdout),
            String::from_utf8_lossy(&bg.stderr)
        );
    }

    std::fs::write(
        tmp.path().join("wasm-input-stream-driver.ts"),
        r#"
import { createRequire } from "node:module";
import { initBackend, runningSum, sumInputEvents, takeOneInputEvent, StreamError, UniffiError } from "./generated/browser/index.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/input_stream_core_wasm.js");
await initBackend(glue);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

async function* events(): AsyncIterable<{ value: number }> {
  yield { value: 1 };
  yield { value: 2 };
  yield { value: 3 };
}

const sum = await sumInputEvents(events());
assert(sum === 6n, `wasm input stream sum ${sum}`);

const sums: number[] = [];
for await (const event of runningSum(events())) {
  sums.push(event.value);
}
assert(sums.join(",") === "1,3,6", `wasm bidi running sums ${sums}`);

let returnCount = 0;
const cancellable = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    let sent = false;
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        if (sent) return { done: true, value: undefined as any };
        sent = true;
        return { done: false, value: { value: 41 } };
      },
      async return(): Promise<IteratorResult<{ value: number }>> {
        returnCount += 1;
        return { done: true, value: undefined as any };
      },
    };
  },
};
const one = await takeOneInputEvent(cancellable);
assert(one === 41, `wasm take one ${one}`);
await new Promise((resolve) => setTimeout(resolve, 20));
assert(returnCount === 1, `wasm Rust drop should call iterator.return once, got ${returnCount}`);

let sharedReturnCount = 0;
let sharedIssued = false;
let settleShared: ((value: IteratorResult<{ value: number }>) => void) | null = null;
const sharedIterator: AsyncIterator<{ value: number }> = {
  next(): Promise<IteratorResult<{ value: number }>> {
    if (!sharedIssued) {
      sharedIssued = true;
      return Promise.resolve({ done: false, value: { value: 61 } });
    }
    return new Promise<IteratorResult<{ value: number }>>((resolve): void => { settleShared = resolve; });
  },
  return(): Promise<IteratorResult<{ value: number }>> {
    sharedReturnCount += 1;
    if (settleShared !== null) {
      settleShared({ done: true, value: undefined as any });
      settleShared = null;
    }
    return Promise.resolve({ done: true, value: undefined as any });
  },
};
const sharedSource = { [Symbol.asyncIterator](): AsyncIterator<{ value: number }> { return sharedIterator; } };
const sharedA = takeOneInputEvent(sharedSource);
const sharedB = takeOneInputEvent(sharedSource);
const sharedResults = await Promise.race([
  Promise.all([sharedA, sharedB]),
  new Promise<string>((resolve): void => { setTimeout((): void => resolve('timeout'), 1000); })
]);
assert(sharedResults !== 'timeout', 'two real wasm Rust input consumers did not settle');
assert((sharedResults as number[]).sort().join(',') === '0,61', `wasm shared consumer results ${sharedResults}`);
assert(sharedReturnCount >= 1, `wasm shared logical input was not closed ${sharedReturnCount}`);

let breakReturnCount = 0;
const breakable = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    let next = 1;
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        return { done: false, value: { value: next++ } };
      },
      async return(): Promise<IteratorResult<{ value: number }>> {
        breakReturnCount += 1;
        return { done: true, value: undefined as any };
      },
    };
  },
};
for await (const event of runningSum(breakable)) {
  assert(event.value === 1, `wasm bidi first value before break ${event.value}`);
  break;
}
await new Promise((resolve) => setTimeout(resolve, 20));
assert(breakReturnCount === 1, `wasm bidi output break should cancel input once, got ${breakReturnCount}`);

const failing = {
  [Symbol.asyncIterator](): AsyncIterator<{ value: number }> {
    return {
      async next(): Promise<IteratorResult<{ value: number }>> {
        throw new StreamError("boom", "Boom");
      },
    };
  },
};
let threw = false;
try {
  await sumInputEvents(failing);
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "wasm input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `wasm input stream error message ${(error as Error).message}`);
}
assert(threw, "wasm input stream error should throw");

let streamThrew = false;
try {
  for await (const _ of runningSum(failing)) {}
} catch (error) {
  streamThrew = true;
  assert(error instanceof UniffiError, "wasm bidi input stream error should be wrapped");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `wasm bidi input stream error message ${(error as Error).message}`);
}
assert(streamThrew, "wasm bidi input stream error should throw from output iterator");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("wasm-input-stream-driver.ts")
        .current_dir(tmp.path())
        .output()
        .expect("failed to run wasm input stream driver");
    if !output.status.success() {
        panic!(
            "wasm input stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "wasm input stream driver did not print ok"
    );
}

#[test]
fn host_crates_wasm_runs_stream_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_wasm_runs_stream_fixture: node 22.6+ unavailable");
        return;
    };
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP host_crates_wasm_runs_stream_fixture: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP host_crates_wasm_runs_stream_fixture: wasm32-unknown-unknown target unavailable"
        );
        return;
    }
    let Some(wasm_bindgen) = which_tool("wasm-bindgen") else {
        eprintln!("SKIP host_crates_wasm_runs_stream_fixture: wasm-bindgen CLI unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let Some(fixture) = build_stream_fixture(tmp.path()) else {
        return;
    };
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_stream_tree(
        &fixture,
        &out_dir,
        Some(host_dir.clone()),
        vec![FlavorTarget::Wasm],
    );

    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("target-wasm-stream");
    let build = Command::new(&cargo)
        .args([
            "build",
            "--manifest-path",
            manifest.as_str(),
            "--release",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .env("CARGO_TARGET_DIR", &target_dir)
        .env("RUSTFLAGS", "-D warnings")
        .output()
        .expect("failed to invoke cargo for wasm stream host");
    if !build.status.success() {
        panic!(
            "cargo build on stream wasm host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr)
        );
    }

    let wasm_file = target_dir
        .join("wasm32-unknown-unknown/release")
        .join("stream_core_wasm.wasm");
    assert!(
        wasm_file.exists(),
        "expected built stream wasm at {}",
        wasm_file.display()
    );
    let pkg = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let bg = Command::new(&wasm_bindgen)
        .args(["--target", "nodejs", "--out-dir"])
        .arg(pkg.as_str())
        .arg(wasm_file.as_path())
        .output()
        .expect("failed to invoke wasm-bindgen for stream fixture");
    if !bg.status.success() {
        panic!(
            "wasm-bindgen stream fixture failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&bg.stdout),
            String::from_utf8_lossy(&bg.stderr)
        );
    }

    std::fs::write(
        tmp.path().join("wasm-stream-driver.ts"),
        r#"
import { createRequire } from "node:module";
import { initBackend, countEvents, errorAfterOne, eventIdEnvelope, pendingEvents, roundtripEventId, UniffiError } from "./generated/browser/index.ts";

const require = createRequire(import.meta.url);
const glue = require("./pkg/stream_core_wasm.js");
await initBackend(glue);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const values: number[] = [];
for await (const event of countEvents(3)) {
  values.push(event.value);
}
assert(values.join(",") === "0,1,2", `wasm stream values ${values}`);

const manual = countEvents(2)[Symbol.asyncIterator]();
assert((await manual.next()).value.value === 0, "manual first");
await manual.return?.();
await manual.return?.();
assert((await manual.next()).done === true, "manual after return done");

let errorValues = 0;
let threw = false;
try {
  for await (const event of errorAfterOne()) {
    errorValues += event.value;
  }
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "wasm stream error should be UniffiError");
  assert(/boom|Boom|StreamError/i.test((error as Error).message), `wasm stream error message ${(error as Error).message}`);
}
assert(errorValues === 7, `wasm stream error first value ${errorValues}`);
assert(threw, "wasm stream error should throw");

const pendingManual = pendingEvents()[Symbol.asyncIterator]();
const pendingNext = pendingManual.next();
await pendingManual.return?.();
const pendingResult = await Promise.race([
  pendingNext,
  new Promise<string>((resolve): void => { setTimeout((): void => resolve("timeout"), 1000); })
]);
assert(pendingResult !== "timeout" && pendingResult.done === true, "wasm pending next should settle after cancel");
assert((await pendingManual.next()).done === true, "wasm pending registry should remain closed");

const aboveSafe = 9007199254740993n;
const u64Max = 18446744073709551615n;
assert(roundtripEventId(aboveSafe) === aboveSafe, "wasm custom u64 above safe integer");
assert(roundtripEventId(u64Max) === u64Max, "wasm custom u64 max");
const idEnvelope = eventIdEnvelope(aboveSafe);
assert(idEnvelope.primary === aboveSafe && idEnvelope.others[1] === u64Max, "wasm composite custom u64");
let overflowRejected = false;
try { roundtripEventId(18446744073709551616n); } catch (_error) { overflowRejected = true; }
assert(overflowRejected, "wasm custom u64 overflow");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("wasm-stream-driver.ts")
        .current_dir(tmp.path())
        .output()
        .expect("failed to run wasm stream driver");
    if !output.status.success() {
        panic!(
            "wasm stream driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "wasm stream driver did not print ok"
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
fn napi_callback_args_are_lifted_with_js_stub() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping: node with --experimental-strip-types not available");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_callback_shape_tree(&out_dir);

    let backend = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    for needle in [
        "args.slice(2).map(__uniffiLiftShape)",
        "const liftedArgs = callArgs.map(__uniffiLiftShape);",
        "Promise.resolve(fn(...liftedArgs))",
        "__uniffiLowerShape(fn(...liftedArgs))",
    ] {
        assert!(
            backend.contains(needle),
            "node/backend-napi.ts should preserve callback arg lift and return lower via `{needle}`:\n{backend}"
        );
    }

    std::fs::write(
        out_dir.join("stub-addon.cjs"),
        r#"
module.exports = {
  emitEvent(sink) {
    sink.onEvent({ type: "Started", messageId: "msg-3" });
  },
};
"#,
    )
    .unwrap();
    std::fs::write(
        out_dir.join("node-callback-shape-driver.ts"),
        r#"
import assert from "node:assert/strict";

process.env.UNIFFI_CALLBACK_SHAPE_NAPI_PATH = new URL("./stub-addon.cjs", import.meta.url).pathname;
const api = await import("./node/index.ts");

const events: unknown[] = [];
api.emitEvent({
  onEvent(event: unknown) {
    events.push(event);
  },
});

assert.deepEqual(events, [{ tag: "Started", messageId: "msg-3" }]);
console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(out_dir.join("node-callback-shape-driver.ts").as_path())
        .current_dir(&out_dir)
        .output()
        .expect("failed to run node callback shape driver");
    if !output.status.success() {
        panic!(
            "node callback shape driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "node callback shape driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn electron_preload_callback_args_are_lifted_with_js_stub() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping: node with --experimental-strip-types not available");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_callback_shape_tree(&out_dir);

    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    for needle in [
        "UNIFFI_CALLBACK_SHAPE_NAPI_PATH",
        "UNIFFI_NAPI_PATH",
        "args.slice(2).map(__uniffiLiftShape)",
        "const liftedArgs = callArgs.map(__uniffiLiftShape);",
        "Promise.resolve(v(...liftedArgs))",
        "__uniffiLowerShape(resolveArg(v(...liftedArgs)))",
    ] {
        assert!(
            preload.contains(needle),
            "electron/preload.cjs should preserve stub loading and callback arg lift via `{needle}`:\n{preload}"
        );
    }

    let electron_stub = out_dir.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"
exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();
    std::fs::write(
        out_dir.join("stub-addon.cjs"),
        r#"
module.exports = {
  emitEvent(sink) {
    sink.onEvent({ type: "Started", messageId: "msg-3" });
  },
};
"#,
    )
    .unwrap();
    std::fs::write(
        out_dir.join("electron-callback-shape-driver.ts"),
        r#"
import assert from "node:assert/strict";
import { createRequire } from "node:module";

process.env.UNIFFI_CALLBACK_SHAPE_NAPI_PATH = new URL("./stub-addon.cjs", import.meta.url).pathname;
(globalThis as { window?: unknown }).window = globalThis;
const require = createRequire(import.meta.url);
require("./electron/preload.cjs");
const api = await import("./electron/renderer.ts");

const events: unknown[] = [];
api.emitEvent({
  onEvent(event: unknown) {
    events.push(event);
  },
});

assert.deepEqual(events, [{ tag: "Started", messageId: "msg-3" }]);
console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(out_dir.join("electron-callback-shape-driver.ts").as_path())
        .current_dir(&out_dir)
        .output()
        .expect("failed to run electron callback shape driver");
    if !output.status.success() {
        panic!(
            "electron callback shape driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "electron callback shape driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn electron_wrap_result_does_not_wrap_arrays_or_plain_values() {
    // The old `wrapResult` used a loose `constructor.name !== \"Object\"`
    // heuristic that swept up `Array`, `Uint8Array`, `Date`, `Error`,
    // etc., so `sampleEvents()` came back to the renderer as a handle
    // stub and `.map(...)` crashed.
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    // The bad heuristic must be gone.
    assert!(
        !preload.contains(r#"value.constructor.name !== "Object""#),
        "electron/preload.cjs still carries the loose constructor.name heuristic:\n{preload}"
    );
    // The new tight checks must be present.
    for needle in [
        "Array.isArray(value)",
        "ArrayBuffer.isView(value)",
        "__uniffiIsHostPlainObject(value)",
    ] {
        assert!(
            preload.contains(needle),
            "electron/preload.cjs missing tightened wrapResult check `{needle}`:\n{preload}"
        );
    }

    // Dynamic exec: pull wrapResult (and its tiny dependencies) out
    // of the generated preload, exercise it against every relevant
    // plain-data shape, and against a fake napi class instance
    // to prove real opaque objects still get wrapped.
    let Some(node) = which_node() else {
        eprintln!("skipping wrapResult exec: node not available");
        return;
    };
    let driver = out_dir.join("wrap-result-driver.cjs");
    std::fs::write(
        &driver,
        r#"
const fs = require("node:fs");
const path = require("node:path");
const src = fs.readFileSync(
    path.resolve(__dirname, "electron/preload.cjs"),
    "utf8",
);
const helperStart = src.indexOf("function __uniffiIsHostPlainObject");
if (helperStart < 0) {
    console.error("__uniffiIsHostPlainObject not found");
    process.exit(2);
}
const wrapStart = src.indexOf("function wrapResult");
if (wrapStart < 0) {
    console.error("wrapResult not found");
    process.exit(2);
}
function extractFunction(start) {
    const bodyOpen = src.indexOf("{", start);
    let depth = 0;
    let end = -1;
    for (let i = bodyOpen; i < src.length; i++) {
        const c = src[i];
        if (c === "{") depth++;
        else if (c === "}") {
            depth--;
            if (depth === 0) { end = i + 1; break; }
        }
    }
    if (end < 0) return null;
    return src.slice(start, end);
}
const helperBody = extractFunction(helperStart);
const wrapBody = extractFunction(wrapStart);
if (!helperBody || !wrapBody) {
    console.error("failed to extract wrapResult helpers");
    process.exit(2);
}
// Provide the storeHandle dependency as a stub that proves it was
// called. Everything else (`Array`, `ArrayBuffer`, etc.) is global.
const scope = { stored: [] };
new Function(
    "scope",
    "function storeHandle(v) { scope.stored.push(v); return scope.stored.length; }\n" +
        helperBody +
        "\n" +
        wrapBody +
        "\nscope.wrapResult = wrapResult;",
)(scope);
const wrapResult = scope.wrapResult;

function assert(cond, label) {
    if (!cond) { console.error(`FAIL ${label}`); process.exit(1); }
}

// These must pass through untouched — no handle wrapping.
const arr = [{ tag: "Click" }, { tag: "Tick" }];
assert(wrapResult(arr) === arr, "array pass-through");
assert(
    wrapResult({ tag: "Click", x: 1 }).__uniffiHandle === undefined,
    "plain enum object pass-through",
);
assert(wrapResult(42) === 42, "number pass-through");
assert(wrapResult("hi") === "hi", "string pass-through");
assert(wrapResult(null) === null, "null pass-through");
assert(wrapResult(undefined) === undefined, "undefined pass-through");
const u8 = new Uint8Array([1, 2, 3]);
assert(wrapResult(u8) === u8, "Uint8Array pass-through");
const ab = new ArrayBuffer(8);
assert(wrapResult(ab) === ab, "ArrayBuffer pass-through");
const d = new Date();
assert(wrapResult(d) === d, "Date pass-through");
const m = new Map();
assert(wrapResult(m) === m, "Map pass-through");
const s = new Set();
assert(wrapResult(s) === s, "Set pass-through");
const e = new Error("bad");
assert(wrapResult(e) === e, "Error pass-through");
assert(scope.stored.length === 0, "nothing should have been handle-wrapped yet");

// A fake napi class instance: non-plain, non-builtin proto.
class Counter { constructor() { this.x = 0; } }
const counter = new Counter();
const wrapped = wrapResult(counter);
assert(
    wrapped && wrapped.__uniffiHandle === true && typeof wrapped.id === "number",
    "napi class instance must be wrapped as handle",
);
assert(
    scope.stored.length === 1 && scope.stored[0] === counter,
    "handle registry must carry the real instance",
);

console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg(driver.as_path())
        .output()
        .expect("failed to run wrapResult driver");
    if !output.status.success() {
        panic!(
            "wrapResult driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "wrapResult driver did not print ok"
    );
}

// ---------------------------------------------------------------------
// Host-crate emission (opt-in via `HostCrateOptions`).
// ---------------------------------------------------------------------

fn generate_arithmetic_with_host_crates(out_dir: &Utf8PathBuf, host_crates_dir: &Utf8PathBuf) {
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let manifest = workspace_root().join("examples/arithmetic/Cargo.toml");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_crates_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![
                FlavorTarget::Wasm,
                FlavorTarget::Napi,
                FlavorTarget::Electron,
            ],
        },
    )
    .expect("generator with host crates should succeed");
}

#[test]
fn emits_host_crate_tree_when_opted_in() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(out.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_arithmetic_with_host_crates(&out_dir, &host_dir);

    for name in [
        "wasm/Cargo.toml",
        "wasm/src/lib.rs",
        "napi/Cargo.toml",
        "napi/src/lib.rs",
        "napi/build.rs",
    ] {
        let p = host_dir.join(name);
        assert!(p.exists(), "missing host-crate file: {p}");
    }

    let wasm_toml = std::fs::read_to_string(host_dir.join("wasm/Cargo.toml")).unwrap();
    assert!(wasm_toml.contains("name = \"uniffi-example-arithmetic-wasm\""));
    assert!(wasm_toml.contains("crate-type = [\"cdylib\""));
    assert!(wasm_toml.contains("wasm-bindgen ="));
    assert!(wasm_toml.contains("wasm-bindgen-futures"));
    assert!(!wasm_toml.contains("serde-wasm-bindgen"));
    assert!(!wasm_toml.contains("serde ="));
    assert!(wasm_toml.contains("js-sys"));
    assert!(
        wasm_toml.contains("uniffi-example-arithmetic = { path ="),
        "wasm Cargo.toml should path-depend on core crate, got:\n{wasm_toml}"
    );
    assert!(
        wasm_toml.contains("[workspace]"),
        "wasm host crate must declare its own [workspace] so it doesn't \
         need the downstream workspace edited first"
    );

    let wasm_lib = std::fs::read_to_string(host_dir.join("wasm/src/lib.rs")).unwrap();
    assert!(
        wasm_lib.contains("include!(") && wasm_lib.contains("browser/arithmetical.rs"),
        "wasm lib.rs must include the generated browser/<crate>.rs, got:\n{wasm_lib}"
    );

    let napi_toml = std::fs::read_to_string(host_dir.join("napi/Cargo.toml")).unwrap();
    assert!(napi_toml.contains("name = \"uniffi-example-arithmetic-napi\""));
    assert!(napi_toml.contains("crate-type = [\"cdylib\"]"));
    assert!(napi_toml.contains("napi = "));
    assert!(napi_toml.contains("napi-derive"));
    assert!(napi_toml.contains("napi-build"));
    assert!(napi_toml.contains("async-trait = \"0.1\""));
    assert!(
        napi_toml.contains("uniffi-example-arithmetic = { path ="),
        "napi Cargo.toml should path-depend on core crate, got:\n{napi_toml}"
    );
    assert!(napi_toml.contains("[workspace]"));

    let napi_lib = std::fs::read_to_string(host_dir.join("napi/src/lib.rs")).unwrap();
    assert!(
        napi_lib.contains("include!(") && napi_lib.contains("node/arithmetical.rs"),
        "napi lib.rs must include the generated node/<crate>.rs, got:\n{napi_lib}"
    );

    let build_rs = std::fs::read_to_string(host_dir.join("napi/build.rs")).unwrap();
    assert!(build_rs.contains("napi_build::setup"));
}

#[test]
fn does_not_emit_host_crates_by_default() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);
    assert!(!out_dir.join("rust_modules").exists());
    assert!(!out_dir.join("wasm/Cargo.toml").exists());
    assert!(!out_dir.join("napi/Cargo.toml").exists());
}

#[test]
fn emits_harmony_flavor_with_ohos_napi_surface() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Harmony],
        },
    )
    .expect("generator should emit both node and harmony flavors");

    for name in [
        "common/public-types.ts",
        "node/backend-napi.ts",
        "node/arithmetical.rs",
        "harmony/backend-ohos.ts",
        "harmony/index.ts",
        "harmony/arithmetical.ohos-extra-types.d.ts",
        "harmony/arithmetical.rs",
    ] {
        let p = out_dir.join(name);
        assert!(p.exists(), "expected output file missing: {p}");
    }

    let node_rs = std::fs::read_to_string(out_dir.join("node/arithmetical.rs")).unwrap();
    assert!(
        node_rs.contains("use napi::bindgen_prelude::*;")
            && node_rs.contains("use napi_derive::napi;"),
        "node bridge must keep ordinary napi-rs imports:\n{node_rs}"
    );
    assert!(
        !node_rs.contains("napi_ohos"),
        "node bridge must not use ohos-rs imports:\n{node_rs}"
    );

    let ohos_rs = std::fs::read_to_string(out_dir.join("harmony/arithmetical.rs")).unwrap();
    assert!(
        ohos_rs.contains("use napi_ohos::bindgen_prelude::*;")
            && ohos_rs.contains("use napi_derive_ohos::napi;")
            && ohos_rs.contains("napi_ohos::bindgen_prelude::BigInt"),
        "harmony bridge must use ohos-rs imports:\n{ohos_rs}"
    );
    assert!(
        !ohos_rs.contains("napi::"),
        "harmony bridge must not reference ordinary napi-rs:\n{ohos_rs}"
    );

    let backend = std::fs::read_to_string(out_dir.join("harmony/backend-ohos.ts")).unwrap();
    assert!(
        !contains_dynamic_type_word(&backend),
        "harmony backend must not emit ArkTS-hostile dynamic type words `any`/`unknown`:\n{backend}"
    );
    for forbidden in ["node:module", "createRequire", "process.env", ".node"] {
        assert!(
            !backend.contains(forbidden),
            "harmony backend must not contain Node-only `{forbidden}`:\n{backend}"
        );
    }
    for required in [
        "import * as native from \"libarithmetic_ohos.so\"",
        "type UniffiValue = UniffiPrimitive | object",
        "__uniffiNameMap",
        "__uniffiLowerShape",
        "__uniffiLiftShape",
        "__uniffiCallback",
        "__uniffiBackendKind = \"ohos\"",
        "then(",
    ] {
        assert!(
            backend.contains(required),
            "harmony backend missing `{required}`:\n{backend}"
        );
    }
}

#[test]
fn emits_ohos_host_crate_when_harmony_is_requested() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(out.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let manifest = workspace_root().join("examples/arithmetic/Cargo.toml");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Harmony],
        },
    )
    .expect("harmony host-crate generation should succeed");

    assert!(host_dir.join("ohos/Cargo.toml").exists());
    assert!(host_dir.join("ohos/build.rs").exists());
    assert!(host_dir.join("ohos/src/lib.rs").exists());
    assert!(
        !host_dir.join("napi/Cargo.toml").exists(),
        "harmony-only generation must not emit ordinary napi host crate"
    );
    assert!(
        !host_dir.join("wasm/Cargo.toml").exists(),
        "harmony-only generation must not emit wasm host crate"
    );

    let toml = std::fs::read_to_string(host_dir.join("ohos/Cargo.toml")).unwrap();
    for required in [
        "name = \"uniffi-example-arithmetic-ohos\"",
        "name = \"arithmetic_ohos\"",
        "napi-ohos = { version = \"1.1.6\"",
        "napi-derive-ohos = { version = \"1.1.6\"",
        "napi-build-ohos = \"1.1.6\"",
        "features = [\"napi8\", \"tokio_rt\"]",
        "features = [\"type-def\"]",
        "[workspace]",
    ] {
        assert!(
            toml.contains(required),
            "OHOS Cargo.toml missing `{required}`:\n{toml}"
        );
    }
    for forbidden in ["/Users/frain/Developer/refer/uni/ohos-rs", "ohos-rs/crates"] {
        assert!(
            !toml.contains(forbidden),
            "default OHOS host crate must not use local ohos-rs path deps `{forbidden}`:\n{toml}"
        );
    }
    let build_rs = std::fs::read_to_string(host_dir.join("ohos/build.rs")).unwrap();
    assert!(build_rs.contains("napi_build_ohos::setup"));
    assert!(!build_rs.contains("std::fs::write"));
    assert!(!build_rs.contains("ohos-extra-types.d.ts"));
    let bundle: serde_json::Value = serde_json::from_slice(
        &std::fs::read(host_dir.join("ohos/uniffi-ohos-facade-bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle["schemaVersion"], 2);
    assert!(bundle["fingerprint"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    assert!(bundle["typeSidecars"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["file"] == "arithmetical.ohos-extra-types.d.ts"));
    let lib_rs = std::fs::read_to_string(host_dir.join("ohos/src/lib.rs")).unwrap();
    assert!(
        lib_rs.contains("include!(") && lib_rs.contains("harmony/arithmetical.rs"),
        "OHOS lib.rs must include generated harmony bridge:\n{lib_rs}"
    );
}

/// Build a tiny synthetic downstream core crate + UDL inside `root`
/// whose public function signatures match what the JS bridge codegen
/// emits. This lets the compile-level tests below run `cargo check`
/// without depending on any fixture that relies on uniffi scaffolding
/// macros or private helper fns.
fn write_synthetic_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("tiny_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"tiny-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\nname = \"tiny\"\ncrate-type = [\"lib\"]\n\n[dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn echo(s: String) -> String { s }\n",
    )
    .unwrap();
    let udl = src.join("tiny.udl");
    std::fs::write(&udl, "namespace tiny {\n    string echo(string s);\n};\n").unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_synthetic_with_host_crates(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let (udl, manifest) = write_synthetic_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Wasm, FlavorTarget::Napi],
        },
    )
    .expect("synthetic generator run should succeed");
    (out_dir, host_dir)
}

fn write_float32_record_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("float32_record_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"float32-record-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\nname = \"float32_record_core\"\ncrate-type = [\"lib\"]\n\n[dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "#[derive(Clone, Debug)]\npub struct Float32Record {\n    pub speed: f32,\n}\n\npub fn roundtrip_float32_record(value: Float32Record) -> Float32Record {\n    value\n}\n\npub struct AsyncService;\n\nimpl AsyncService {\n    pub fn new() -> std::sync::Arc<Self> {\n        std::sync::Arc::new(Self)\n    }\n\n    pub async fn greet(&self, message: String) -> String {\n        message\n    }\n}\n",
    )
    .unwrap();
    let udl = src.join("float32_record_core.udl");
    std::fs::write(
        &udl,
        "dictionary Float32Record {\n    float speed;\n};\n\ninterface AsyncService {\n    constructor();\n    [Async]\n    string greet(string message);\n};\n\nnamespace float32_record_core {\n    Float32Record roundtrip_float32_record(Float32Record value);\n};\n",
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_float32_record_hosts(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let (udl, manifest) = write_float32_record_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Harmony],
        },
    )
    .expect("float32 record host generation should succeed");
    (out_dir, host_dir)
}

fn run_cargo_check(
    manifest: &Utf8PathBuf,
    extra: &[&str],
    target_dir: &std::path::Path,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("cargo");
    cmd.args(["check", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(extra)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("RUSTFLAGS");
    cmd.output()
}

// `cargo` is selected by rustup using this test process's working directory,
// which is under the workspace `rust-toolchain.toml`.  Resolve the probe
// rustc through that same override (or Cargo's explicit RUSTC override), not
// through an arbitrary PATH `rustc`; otherwise a target installed for stable
// can incorrectly green-light a cargo check performed by the pinned toolchain.
fn cargo_target_libdir(target: &str) -> std::io::Result<Option<std::path::PathBuf>> {
    let rustc = match std::env::var_os("RUSTC") {
        Some(value) if !value.is_empty() => std::path::PathBuf::from(value),
        _ => {
            let output = Command::new("rustup").args(["which", "rustc"]).output()?;
            if !output.status.success() {
                return Err(std::io::Error::other(format!(
                    "rustup could not resolve the rustc used by cargo: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        }
    };
    let output = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", target])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let libdir = std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(libdir.is_dir().then_some(libdir))
}

fn run_cargo_build(
    manifest: &Utf8PathBuf,
    extra: &[&str],
    target_dir: &std::path::Path,
) -> std::io::Result<std::process::Output> {
    let mut cmd = Command::new("cargo");
    cmd.args(["build", "--manifest-path"])
        .arg(manifest.as_std_path())
        .args(extra)
        .env("CARGO_TARGET_DIR", target_dir)
        .env_remove("RUSTFLAGS");
    cmd.output()
}

fn cdylib_filename(package_name: &str) -> String {
    let base = package_name.replace('-', "_");
    let ext = std::env::consts::DLL_EXTENSION;
    if cfg!(target_os = "windows") {
        format!("{base}.{ext}")
    } else {
        format!("lib{base}.{ext}")
    }
}

#[test]
fn host_crates_napi_passes_cargo_check() {
    let tmp = tempfile::tempdir().unwrap();
    let (_out, host_dir) = generate_synthetic_with_host_crates(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-napi");
    let output = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_passes_cargo_check: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn host_crates_napi_and_ohos_compile_float32_record_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let (out_dir, host_dir) = generate_float32_record_hosts(tmp.path());

    for bridge in [
        out_dir.join("node/float32_record_core.rs"),
        out_dir.join("harmony/float32_record_core.rs"),
    ] {
        let source = std::fs::read_to_string(&bridge).unwrap();
        assert!(
            source.contains("pub speed: f64")
                && source.contains("speed: value.speed as f32")
                && source.contains("speed: value.speed as f64"),
            "float32 bridge must adapt JS number at the FFI boundary: {source}"
        );
        assert!(
            !source.contains("pub speed: f32"),
            "the host crate must not ask N-API to marshal f32 directly: {source}"
        );
        assert!(
            source.contains("pub fn async_service_greet(")
                && source.contains("__uniffi_env: Env,")
                && source.contains("handle: ClassInstance<'_, AsyncService>,")
                && source.contains("let __uniffi_core = (*(handle)).0.clone();")
                && source.contains("drop(handle);")
                && source.contains("spawn_future(__uniffi_future)"),
            "async object receivers must lower before entering the Send promise future: {source}"
        );
        assert!(
            !source.contains("pub async fn async_service_greet("),
            "an async N-API function would capture ClassInstance before its body can drop it: {source}"
        );
    }

    let napi_manifest = host_dir.join("napi/Cargo.toml");
    let napi_target = tmp.path().join("cargo-target-float32-napi");
    let napi_output = run_cargo_check(&napi_manifest, &[], &napi_target)
        .expect("cargo must be available for the N-API f32 host regression");
    assert!(
        napi_output.status.success(),
        "cargo check on f32 N-API host crate failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&napi_output.stdout),
        String::from_utf8_lossy(&napi_output.stderr),
    );

    let target = "aarch64-unknown-linux-ohos";
    let Some(target_libdir) = cargo_target_libdir(target)
        .expect("the rustc selected by cargo must be available for the OHOS f32 host regression")
    else {
        eprintln!(
            "SKIP host_crates_napi_and_ohos_compile_float32_record_fixture: {target} standard library is not installed for Cargo's rust toolchain"
        );
        return;
    };
    assert!(
        target_libdir.is_dir(),
        "Cargo's target libdir must exist before compiling the OHOS host: {}",
        target_libdir.display()
    );

    let ohos_manifest = host_dir.join("ohos/Cargo.toml");
    let ohos_target = tmp.path().join("cargo-target-float32-ohos");
    let ohos_output = run_cargo_check(&ohos_manifest, &["--target", target], &ohos_target)
        .expect("cargo must be available for the OHOS f32 host regression");
    assert!(
        ohos_output.status.success(),
        "cargo check on f32 OHOS host crate failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ohos_output.stdout),
        String::from_utf8_lossy(&ohos_output.stderr),
    );
}

#[test]
fn host_crates_wasm_passes_cargo_check() {
    // Skip if wasm32 target not installed.
    let probe = Command::new("rustc")
        .args([
            "--print",
            "target-libdir",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .output();
    match probe {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("SKIP host_crates_wasm_passes_cargo_check: wasm32-unknown-unknown target not installed");
            return;
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let (_out, host_dir) = generate_synthetic_with_host_crates(tmp.path());
    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-wasm");
    let output = match run_cargo_check(
        &manifest,
        &["--target", "wasm32-unknown-unknown"],
        &target_dir,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_wasm_passes_cargo_check: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on wasm host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

// ---------------------------------------------------------------------
// Host-crate flavor gating.
// ---------------------------------------------------------------------

fn generate_synthetic_gated(root: &std::path::Path, flavors: Vec<FlavorTarget>) -> Utf8PathBuf {
    let (udl, manifest) = write_synthetic_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors,
        },
    )
    .expect("gated generator run should succeed");
    host_dir
}

#[test]
fn host_crates_wasm_only_skips_napi() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Wasm]);
    assert!(host_dir.join("wasm/Cargo.toml").exists());
    assert!(host_dir.join("wasm/src/lib.rs").exists());
    assert!(
        !host_dir.join("napi").exists(),
        "napi host crate must not be emitted when only --flavor wasm is requested"
    );
}

#[test]
fn host_crates_napi_only_skips_wasm() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Napi]);
    assert!(host_dir.join("napi/Cargo.toml").exists());
    assert!(host_dir.join("napi/src/lib.rs").exists());
    assert!(
        !host_dir.join("wasm").exists(),
        "wasm host crate must not be emitted when only --flavor napi is requested"
    );
}

#[test]
fn host_crates_electron_only_emits_napi_and_skips_wasm() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Electron]);
    assert!(
        host_dir.join("napi/Cargo.toml").exists(),
        "electron must reuse the napi host crate"
    );
    assert!(
        !host_dir.join("wasm").exists(),
        "wasm host crate must not be emitted when only --flavor electron is requested"
    );
}

#[test]
fn host_crates_wasm_only_passes_cargo_check() {
    // Regression proof for the broken scenario before flavor gating:
    // `--flavor wasm --emit-host-crates` would also emit a napi
    // host crate that `include!`-ed a non-existent `out/node/*.rs`.
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Wasm]);
    assert!(!host_dir.join("napi").exists());

    // Skip if wasm32 target not installed.
    let probe = Command::new("rustc")
        .args([
            "--print",
            "target-libdir",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .output();
    match probe {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("SKIP host_crates_wasm_only_passes_cargo_check: wasm32 target not installed");
            return;
        }
    }

    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-wasm-only");
    let output = match run_cargo_check(
        &manifest,
        &["--target", "wasm32-unknown-unknown"],
        &target_dir,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_wasm_only_passes_cargo_check: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on wasm-only host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn cli_build_wasm_orchestrates_arithmetic_fixture() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_build_wasm_orchestrates_arithmetic_fixture: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_build_wasm_orchestrates_arithmetic_fixture: wasm32-unknown-unknown target not installed"
        );
        return;
    }
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let pkg_dir = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let (manifest, source) = write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .arg("javascript")
        .arg("build-wasm")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--source")
        .arg(source.as_str())
        .arg("--out-dir")
        .arg(out_dir.as_str())
        .arg("--host-crates-dir")
        .arg(host_dir.as_str())
        .arg("--wasm-bindgen-out-dir")
        .arg(pkg_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build-wasm");
    if !output.status.success() {
        panic!(
            "javascript build-wasm failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    for path in [
        "common/api.ts",
        "browser/index.ts",
        "browser/backend-wasm.ts",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing generated JS file: {file}");
    }
    assert!(
        host_dir.join("wasm/Cargo.toml").exists(),
        "missing generated wasm host crate"
    );
    assert!(
        pkg_dir.exists(),
        "missing wasm-bindgen output dir: {pkg_dir}"
    );
    let pkg_entries = std::fs::read_dir(pkg_dir.as_std_path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect::<Vec<_>>();
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("js")),
        "wasm-bindgen output dir should contain a .js glue file: {pkg_entries:?}"
    );
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("wasm")),
        "wasm-bindgen output dir should contain a .wasm artifact: {pkg_entries:?}"
    );
}

#[test]
fn cli_build_orchestrates_full_javascript_tree() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_build_orchestrates_full_javascript_tree: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_build_orchestrates_full_javascript_tree: wasm32-unknown-unknown target not installed"
        );
        return;
    }
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let artifact_dir = Utf8PathBuf::from_path_buf(tmp.path().join("artifacts")).unwrap();
    let target_dir = Utf8PathBuf::from_path_buf(tmp.path().join("cargo-target-napi")).unwrap();
    let (manifest, source) = write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .arg("javascript")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--source")
        .arg(source.as_str())
        .arg("--out-dir")
        .arg(out_dir.as_str())
        .arg("--host-crates-dir")
        .arg(host_dir.as_str())
        .arg("--artifact-dir")
        .arg(artifact_dir.as_str())
        .arg("--target-dir")
        .arg(target_dir.as_str())
        .arg("--wasm-bindgen-target")
        .arg("nodejs")
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build");
    if !output.status.success() {
        panic!(
            "javascript build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    for path in [
        "common/api.ts",
        "common/public-types.ts",
        "browser/index.ts",
        "browser/backend-wasm.ts",
        "node/index.ts",
        "node/backend-napi.ts",
        "electron/index.ts",
        "electron/backend-napi.ts",
        "electron/preload.cjs",
        "electron/renderer.ts",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing combined build artifact: {file}");
    }

    assert!(host_dir.join("wasm/Cargo.toml").exists());
    assert!(host_dir.join("napi/Cargo.toml").exists());
    assert!(
        !out_dir.join("node/cli_wasm.node").exists(),
        "--artifact-dir should keep node addon out of the generated source tree"
    );
    assert!(
        !out_dir.join("electron/cli_wasm.node").exists(),
        "--artifact-dir should keep electron addon out of the generated source tree"
    );
    assert!(
        artifact_dir.join("node/cli_wasm.node").exists(),
        "missing node addon in artifact dir"
    );
    assert!(
        artifact_dir.join("electron/cli_wasm.node").exists(),
        "missing electron addon in artifact dir"
    );

    let browser_pkg = artifact_dir.join("browser/pkg");
    let pkg_entries = std::fs::read_dir(browser_pkg.as_std_path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect::<Vec<_>>();
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("wasm")),
        "combined build should leave wasm-bindgen .wasm in browser/pkg: {pkg_entries:?}"
    );
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("js")),
        "combined build should leave wasm-bindgen JS glue in browser/pkg: {pkg_entries:?}"
    );
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("ts")),
        "combined build should leave wasm-bindgen TypeScript declarations in browser/pkg: {pkg_entries:?}"
    );

    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("dispatchSync") && preload.contains("dispatchAsync"),
        "combined build electron preload should expose sync and async dispatch:\n{preload}"
    );
    assert!(
        preload.contains("../artifacts/electron/cli_wasm.node"),
        "preload should load the addon from --artifact-dir:\n{preload}"
    );
    let node_backend = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        node_backend.contains("../artifacts/node/cli_wasm.node"),
        "node backend should load the addon from --artifact-dir:\n{node_backend}"
    );

    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP cli_build_orchestrates_full_javascript_tree runtime matrix: \
             node with --experimental-strip-types not available"
        );
        return;
    };

    let wasm_glue_js = pkg_entries
        .iter()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .expect("browser/pkg should contain a wasm-bindgen JS glue file")
        .to_string();

    // The renderer path can be exercised without launching Electron by
    // stubbing the preload-only `contextBridge` API. This keeps the test
    // focused on generated bridge semantics rather than Electron process
    // management.
    let electron_stub = out_dir.join("electron/node_modules/electron");
    std::fs::create_dir_all(electron_stub.as_std_path()).unwrap();
    std::fs::write(
        electron_stub.join("index.js").as_std_path(),
        r#"
exports.contextBridge = {
    exposeInMainWorld(name, value) {
        globalThis[name] = value;
    },
};
"#,
    )
    .unwrap();

    let driver = r#"
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);

function assertEq(actual: unknown, expected: unknown, label: string): void {
    if (actual !== expected) {
        throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
    }
}

async function expectThrown(label: string, call: () => unknown): Promise<void> {
    try {
        await call();
    } catch (_e) {
        return;
    }
    throw new Error(`${label}: expected an error`);
}

const glue = require("__WASM_PKG__/__WASM_GLUE__");
const browser = await import("./browser/index.ts");
await browser.initBackend(glue);
assertEq(browser.add(2n, 3n), 5n, "browser.add");
assertEq(browser.slowAdd(20n, 22n), 42n, "browser.slowAdd name mapping");
assertEq(await browser.asyncAdd(30n, 12n), 42n, "browser.asyncAdd");
assertEq(browser.sub(9n, 4n), 5n, "browser.sub");
assertEq(browser.equal(8n, 8n), true, "browser.equal");
const browserEvent = browser.makeEvent(true) as { tag?: string; x?: number; y?: number };
assertEq(browserEvent.tag, "Moved", "browser.makeEvent tag");
assertEq(browserEvent.x, 3, "browser.makeEvent x");
assertEq(browserEvent.y, 4, "browser.makeEvent y");
assertEq(browser.describeEvent({ tag: "Moved", x: 5, y: 6 }), "moved:5,6", "browser.describeEvent");
await expectThrown("browser.sub underflow", () => browser.sub(1n, 2n));

const nodeApi = await import("./node/index.ts");
assertEq(nodeApi.add(4n, 6n), 10n, "node.add");
assertEq(nodeApi.slowAdd(20n, 22n), 42n, "node.slowAdd name mapping");
assertEq(await nodeApi.asyncAdd(30n, 12n), 42n, "node.asyncAdd");
assertEq(nodeApi.sub(9n, 4n), 5n, "node.sub");
assertEq(nodeApi.equal(8n, 9n), false, "node.equal");
const nodeEvent = nodeApi.makeEvent(true) as { tag?: string; x?: number; y?: number };
assertEq(nodeEvent.tag, "Moved", "node.makeEvent tag");
assertEq(nodeEvent.x, 3, "node.makeEvent x");
assertEq(nodeEvent.y, 4, "node.makeEvent y");
assertEq(nodeApi.describeEvent({ tag: "Moved", x: 5, y: 6 }), "moved:5,6", "node.describeEvent");
await expectThrown("node.sub underflow", () => nodeApi.sub(1n, 2n));

(globalThis as { window?: unknown }).window = globalThis;
require("./electron/preload.cjs");
const electronApi = await import("./electron/renderer.ts");
assertEq(electronApi.add(10n, 11n), 21n, "electron.add");
assertEq(electronApi.slowAdd(20n, 22n), 42n, "electron.slowAdd name mapping");
assertEq(await electronApi.asyncAdd(30n, 12n), 42n, "electron.asyncAdd");
assertEq(electronApi.sub(9n, 4n), 5n, "electron.sub");
assertEq(electronApi.equal(8n, 8n), true, "electron.equal");
const electronEvent = electronApi.makeEvent(true) as { tag?: string; x?: number; y?: number };
assertEq(electronEvent.tag, "Moved", "electron.makeEvent tag");
assertEq(electronEvent.x, 3, "electron.makeEvent x");
assertEq(electronEvent.y, 4, "electron.makeEvent y");
assertEq(electronApi.describeEvent({ tag: "Moved", x: 5, y: 6 }), "moved:5,6", "electron.describeEvent");
await expectThrown("electron.sub underflow", () => electronApi.sub(1n, 2n));

console.log("combined build runtime ok");
"#
    .replace(
        "__WASM_PKG__",
        &artifact_dir.join("browser/pkg").to_string().replace('\\', "/"),
    )
    .replace("__WASM_GLUE__", &wasm_glue_js);
    let driver_path = out_dir.join("combined-build-driver.ts");
    std::fs::write(driver_path.as_std_path(), driver).unwrap();
    let runtime = Command::new(&node)
        .current_dir(out_dir.as_std_path())
        .args([
            "--experimental-strip-types",
            "--no-warnings",
            "combined-build-driver.ts",
        ])
        .output()
        .expect("failed to run combined build runtime driver");
    if !runtime.status.success() {
        panic!(
            "combined build runtime driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&runtime.stdout),
            String::from_utf8_lossy(&runtime.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&runtime.stdout).contains("combined build runtime ok"),
        "combined build runtime driver did not report success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&runtime.stdout),
        String::from_utf8_lossy(&runtime.stderr),
    );
}

#[test]
fn cli_managed_layout_emits_package_entries_manifest_and_bench_smoke() {
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_managed_layout_emits_package_entries_manifest_and_bench_smoke: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_managed_layout_emits_package_entries_manifest_and_bench_smoke: wasm32-unknown-unknown target not installed"
        );
        return;
    }
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP cli_managed_layout_emits_package_entries_manifest_and_bench_smoke: node with --experimental-strip-types not available"
        );
        return;
    };

    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let tmp = tempfile::tempdir().unwrap();
    let package_dir = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let target_dir =
        Utf8PathBuf::from_path_buf(tmp.path().join("managed-cargo-target-napi")).unwrap();
    let (manifest, source) = write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .arg("artifacts")
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--source")
        .arg(source.as_str())
        .arg("--target")
        .arg("wasm")
        .arg("--target")
        .arg("mini-program")
        .arg("--target")
        .arg("node")
        .arg("--managed-layout")
        .arg("--package-dir")
        .arg(package_dir.as_str())
        .arg("--napi-target-dir")
        .arg(target_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen artifacts build --managed-layout");
    if !output.status.success() {
        panic!(
            "managed artifacts build failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    for path in [
        "src/index.web.ts",
        "src/index.mini-program.ts",
        "src/index.node.ts",
        "src/ffi/common/public-types.ts",
        "src/ffi/browser/index.web.ts",
        "src/ffi/browser/index.mini-program.ts",
        "src/ffi/node/index.ts",
        "artifacts/rust/wasm/Cargo.toml",
        "artifacts/rust/napi/Cargo.toml",
        "artifacts/browser/pkg/cli_wasm_fixture_wasm.js",
        "artifacts/browser/pkg/cli_wasm_fixture_wasm_bg.wasm",
        "artifacts/mini-program/cli_wasm_fixture_wasm.js",
        "artifacts/mini-program/cli_wasm_fixture_wasm_bg.wasm",
        "artifacts/node/cli_wasm.node",
        "artifact-manifest.json",
        ".gitignore",
    ] {
        let file = package_dir.join(path);
        assert!(file.exists(), "missing managed layout file: {file}");
    }

    let web_entry = std::fs::read_to_string(package_dir.join("src/index.web.ts")).unwrap();
    assert!(
        web_entry.contains("export * from \"./ffi/browser/index.web.ts\";"),
        "managed web entry must re-export generated browser auto entry:\n{web_entry}"
    );
    assert!(
        web_entry.contains("export type * from \"./ffi/common/public-types.ts\";"),
        "managed web entry must re-export public types:\n{web_entry}"
    );
    assert!(
        !web_entry.contains(package_dir.as_str()) && !web_entry.contains("artifacts/"),
        "managed web entry must not contain absolute paths or artifact internals:\n{web_entry}"
    );

    let mini_entry =
        std::fs::read_to_string(package_dir.join("src/index.mini-program.ts")).unwrap();
    assert!(
        mini_entry.contains("export * from \"./ffi/browser/index.mini-program.ts\";"),
        "managed Mini Program entry must re-export generated Mini Program entry:\n{mini_entry}"
    );
    assert!(
        mini_entry.contains("export type * from \"./ffi/common/public-types.ts\";"),
        "managed Mini Program entry must re-export public types:\n{mini_entry}"
    );

    let mini_runtime =
        std::fs::read_to_string(package_dir.join("src/ffi/browser/index.mini-program.ts")).unwrap();
    for forbidden in [
        "?url",
        "fetch(",
        "import.meta.url",
        "window",
        "document",
        "node:",
    ] {
        assert!(
            !mini_runtime.contains(forbidden),
            "Mini Program entry must not contain web/Node-only token `{forbidden}`:\n{mini_runtime}"
        );
    }
    assert!(
        mini_runtime.contains("WXWebAssembly.instantiate"),
        "Mini Program entry should validate WXWebAssembly.instantiate:\n{mini_runtime}"
    );

    let mini_glue = std::fs::read_to_string(
        package_dir.join("artifacts/mini-program/cli_wasm_fixture_wasm.js"),
    )
    .unwrap();
    for forbidden in ["fetch(", "import.meta.url", "?url", "window", "document"] {
        assert!(
            !mini_glue.contains(forbidden),
            "patched Mini Program glue must not contain web-only token `{forbidden}`:\n{mini_glue}"
        );
    }
    assert!(
        mini_glue.contains("WXWebAssembly.instantiate(wasmPath, imports)"),
        "patched Mini Program glue must load through WXWebAssembly.instantiate:\n{mini_glue}"
    );
    assert!(
        mini_glue.contains("__uniffiTextDecoder")
            && mini_glue.contains("__uniffiTextEncoder")
            && !mini_glue.contains("new TextDecoder(")
            && !mini_glue.contains("new TextEncoder("),
        "patched Mini Program glue must not require TextDecoder/TextEncoder globals at module evaluation:\n{mini_glue}"
    );

    let node_entry = std::fs::read_to_string(package_dir.join("src/index.node.ts")).unwrap();
    assert!(
        node_entry.contains("export * from \"./ffi/node/index.ts\";"),
        "managed node entry must re-export generated node entry:\n{node_entry}"
    );
    assert!(
        node_entry.contains("export type * from \"./ffi/common/public-types.ts\";"),
        "managed node entry must re-export public types:\n{node_entry}"
    );

    let gitignore = std::fs::read_to_string(package_dir.join(".gitignore")).unwrap();
    assert!(gitignore.contains("/artifacts/"));
    assert!(
        !gitignore.contains("src/ffi"),
        "managed gitignore must not hide reviewable FFI source:\n{gitignore}"
    );

    let manifest_text =
        std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap();
    assert!(
        !manifest_text.contains(package_dir.as_str()),
        "managed manifest must be relative-only:\n{manifest_text}"
    );
    let manifest_json: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    assert_eq!(manifest_json["schemaVersion"], 3);
    assert_eq!(
        manifest_json["targets"],
        serde_json::json!(["wasm", "mini-program", "node"])
    );
    assert_eq!(manifest_json["source"]["root"], "src/ffi");
    assert_eq!(manifest_json["source"]["common"], "src/ffi/common");
    assert_eq!(manifest_json["entrypoints"]["web"], "src/index.web.ts");
    assert_eq!(
        manifest_json["entrypoints"]["miniProgram"],
        "src/index.mini-program.ts"
    );
    assert_eq!(manifest_json["entrypoints"]["node"], "src/index.node.ts");
    assert_eq!(
        manifest_json["artifacts"]["wasm"]["wasm"],
        "artifacts/browser/pkg/cli_wasm_fixture_wasm_bg.wasm"
    );
    assert_eq!(
        manifest_json["artifacts"]["miniProgram"]["glue"],
        "artifacts/mini-program/cli_wasm_fixture_wasm.js"
    );
    assert_eq!(
        manifest_json["artifacts"]["miniProgram"]["wasm"],
        "artifacts/mini-program/cli_wasm_fixture_wasm_bg.wasm"
    );
    assert_eq!(
        manifest_json["artifacts"]["miniProgram"]["defaultWasmPath"],
        "/assets/cli_wasm_fixture_wasm_bg.wasm"
    );
    assert_eq!(
        manifest_json["artifacts"]["node"]["addon"],
        "artifacts/node/cli_wasm.node"
    );
    assert!(manifest_json["artifacts"]["harmony"].is_null());

    std::fs::write(
        package_dir.join("package.json").as_std_path(),
        r#"{ "type": "module" }"#,
    )
    .unwrap();

    let mini_driver = r#"
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

function assertEq(actual: unknown, expected: unknown, label: string): void {
    if (actual !== expected) {
        throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
    }
}

const calls: string[] = [];
(globalThis as { TextDecoder?: unknown; TextEncoder?: unknown }).TextDecoder = undefined;
(globalThis as { TextDecoder?: unknown; TextEncoder?: unknown }).TextEncoder = undefined;
(globalThis as { WXWebAssembly?: unknown }).WXWebAssembly = {
    async instantiate(path: string, imports: WebAssembly.Imports): Promise<WebAssembly.WebAssemblyInstantiatedSource> {
        calls.push(path);
        const localPath = path.startsWith("/assets/")
            ? `artifacts/mini-program/${path.slice("/assets/".length)}`
            : path;
        const bytes = await readFile(resolve(localPath));
        return WebAssembly.instantiate(bytes, imports);
    },
};

const mini = await import("./src/index.mini-program.ts");
await mini.init("/assets/cli_wasm_fixture_wasm_bg.wasm");
assertEq(calls[0], "/assets/cli_wasm_fixture_wasm_bg.wasm", "WXWebAssembly path");
assertEq(mini.add(2n, 3n), 5n, "mini.add");
assertEq(mini.slowAdd(20n, 22n), 42n, "mini.slowAdd");
assertEq(await mini.asyncAdd(30n, 12n), 42n, "mini.asyncAdd");
await mini.init("/assets/ignored.wasm");
assertEq(calls.length, 1, "mini init idempotent");
console.log("mini-program managed runtime ok");
"#;
    std::fs::write(
        package_dir.join("mini-program-smoke.ts").as_std_path(),
        mini_driver,
    )
    .unwrap();
    let mini_runtime = Command::new(&node)
        .current_dir(package_dir.as_std_path())
        .args([
            "--experimental-strip-types",
            "--no-warnings",
            "mini-program-smoke.ts",
        ])
        .output()
        .expect("failed to run Mini Program smoke");
    if !mini_runtime.status.success() {
        panic!(
            "Mini Program smoke failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&mini_runtime.stdout),
            String::from_utf8_lossy(&mini_runtime.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&mini_runtime.stdout).contains("mini-program managed runtime ok"),
        "Mini Program smoke did not report success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&mini_runtime.stdout),
        String::from_utf8_lossy(&mini_runtime.stderr),
    );

    let bench_driver = r#"
import { performance } from "node:perf_hooks";

function assertEq(actual: unknown, expected: unknown, label: string): void {
    if (actual !== expected) {
        throw new Error(`${label}: expected ${String(expected)}, got ${String(actual)}`);
    }
}

function run(label: string, fn: (a: bigint, b: bigint) => bigint): { elapsed: number; acc: bigint } {
    const started = performance.now();
    let acc = 0n;
    for (let i = 0; i < 5000; i += 1) {
        acc += fn(1n, 2n);
    }
    return { elapsed: performance.now() - started, acc };
}

const managed = await import("./src/index.node.ts");
const direct = await import("./src/ffi/node/index.ts");
assertEq(managed.add(2n, 3n), 5n, "managed.add");
assertEq(direct.add(2n, 3n), 5n, "direct.add");

const managedBench = run("managed", managed.add);
const directBench = run("direct", direct.add);
assertEq(managedBench.acc, directBench.acc, "bench accumulator");
const ratio = managedBench.elapsed / Math.max(directBench.elapsed, 0.001);
if (ratio > 100) {
    throw new Error(`managed entrypoint unexpectedly slower: managed=${managedBench.elapsed}ms direct=${directBench.elapsed}ms ratio=${ratio}`);
}
console.log(`managed entry bench-smoke ok managed=${managedBench.elapsed.toFixed(3)}ms direct=${directBench.elapsed.toFixed(3)}ms ratio=${ratio.toFixed(3)}`);
"#;
    std::fs::write(
        package_dir.join("managed-bench-smoke.ts").as_std_path(),
        bench_driver,
    )
    .unwrap();
    let runtime = Command::new(&node)
        .current_dir(package_dir.as_std_path())
        .args([
            "--experimental-strip-types",
            "--no-warnings",
            "managed-bench-smoke.ts",
        ])
        .output()
        .expect("failed to run managed entry bench-smoke");
    if !runtime.status.success() {
        panic!(
            "managed entry bench-smoke failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&runtime.stdout),
            String::from_utf8_lossy(&runtime.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&runtime.stdout).contains("managed entry bench-smoke ok"),
        "managed entry bench-smoke did not report success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&runtime.stdout),
        String::from_utf8_lossy(&runtime.stderr),
    );
}

fn build_uniffi_bindgen_cli(cargo: &std::path::Path) -> Utf8PathBuf {
    let root = workspace_root();
    let build = Command::new(cargo)
        .current_dir(&root)
        .args([
            "build",
            "-p",
            "uniffi",
            "--features",
            "cli",
            "--bin",
            "uniffi-bindgen",
        ])
        .output()
        .expect("failed to build uniffi-bindgen");
    if !build.status.success() {
        panic!(
            "building uniffi-bindgen failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let cli = root.join(if cfg!(windows) {
        "target/debug/uniffi-bindgen.exe"
    } else {
        "target/debug/uniffi-bindgen"
    });
    assert!(cli.exists(), "expected built CLI at {cli}");
    cli
}

fn write_cli_wasm_fixture(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let crate_dir = root.join("cli_wasm_fixture");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let workspace = workspace_root();
    let uniffi_path = workspace.join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"cli-wasm-fixture\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n\
             publish = false\n\n\
             [lib]\n\
             name = \"cli_wasm_fixture\"\n\
             crate-type = [\"lib\", \"cdylib\"]\n\n\
             [dependencies]\n\
             uniffi = {{ path = \"{}\" }}\n\
             thiserror = \"2\"\n\n\
             [build-dependencies]\n\
             uniffi = {{ path = \"{}\", features = [\"build\"] }}\n\n\
             [workspace]\n\
             resolver = \"3\"\n",
            uniffi_path, uniffi_path
        ),
    )
    .unwrap();
    std::fs::write(
        crate_dir.join("build.rs"),
        "fn main() {\n    uniffi::generate_scaffolding(\"src/cli_wasm.udl\").unwrap();\n}\n",
    )
    .unwrap();
    let udl_path = src.join("cli_wasm.udl");
    std::fs::write(
        &udl_path,
        "[Error]\n\
         enum ArithmeticError {\n\
         \x20   \"IntegerOverflow\",\n\
         };\n\n\
         [Enum]\n\
         interface CliEvent {\n\
         \x20   Started();\n\
         \x20   Moved(u32 x, u32 y);\n\
         };\n\n\
         namespace cli_wasm {\n\
         \x20   [Throws=ArithmeticError]\n\
         \x20   u64 add(u64 a, u64 b);\n\
         \x20   u64 slow_add(u64 a, u64 b);\n\
         \x20   [Async]\n\
         \x20   u64 async_add(u64 a, u64 b);\n\
         \x20   [Throws=ArithmeticError]\n\
         \x20   u64 sub(u64 a, u64 b);\n\
         \x20   u64 div(u64 dividend, u64 divisor);\n\
         \x20   boolean equal(u64 a, u64 b);\n\
         \x20   CliEvent make_event(boolean moved);\n\
         \x20   string describe_event(CliEvent event);\n\
         };\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use thiserror::Error;\n\n\
         #[derive(Debug, Error)]\n\
         pub enum ArithmeticError {\n\
         \x20   #[error(\"Integer overflow\")]\n\
         \x20   IntegerOverflow,\n\
         }\n\n\
         pub enum CliEvent {\n\
         \x20   Started,\n\
         \x20   Moved { x: u32, y: u32 },\n\
         }\n\n\
         pub fn add(a: u64, b: u64) -> Result<u64, ArithmeticError> {\n\
         \x20   a.checked_add(b).ok_or(ArithmeticError::IntegerOverflow)\n\
         }\n\n\
         pub fn slow_add(a: u64, b: u64) -> u64 { a + b }\n\n\
         pub async fn async_add(a: u64, b: u64) -> u64 { a + b }\n\n\
         pub fn sub(a: u64, b: u64) -> Result<u64, ArithmeticError> {\n\
         \x20   a.checked_sub(b).ok_or(ArithmeticError::IntegerOverflow)\n\
         }\n\n\
         pub fn div(dividend: u64, divisor: u64) -> u64 {\n\
         \x20   if divisor == 0 { panic!(\"divide by zero\"); }\n\
         \x20   dividend / divisor\n\
         }\n\n\
         pub fn equal(a: u64, b: u64) -> bool { a == b }\n\n\
         pub fn make_event(moved: bool) -> CliEvent {\n\
         \x20   if moved { CliEvent::Moved { x: 3, y: 4 } } else { CliEvent::Started }\n\
         }\n\n\
         pub fn describe_event(event: CliEvent) -> String {\n\
         \x20   match event {\n\
         \x20       CliEvent::Started => \"started\".to_string(),\n\
         \x20       CliEvent::Moved { x, y } => format!(\"moved:{x},{y}\"),\n\
         \x20   }\n\
         }\n\n\
         uniffi::include_scaffolding!(\"cli_wasm\");\n",
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap(),
        Utf8PathBuf::from_path_buf(udl_path).unwrap(),
    )
}

// ---------------------------------------------------------------------
// napi host-crate compatibility regression — enum + with_foreign
// callback trait + async fn. Guards the template default versions so
// the generated napi bridge (discriminant / FnArgs / ThreadsafeFunction)
// actually compiles against them.
// ---------------------------------------------------------------------

fn write_rich_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("rich_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"napi-compat-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [lib]\nname = \"napi_compat\"\ncrate-type = [\"lib\"]\n\n\
         [dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::sync::Arc;\n\n\
         pub trait Logger: Send + Sync { fn log(&self, msg: String); }\n\n\
         pub struct Counter(i64);\n\n\
         pub enum JobState { Idle, Running, Done }\n\n\
         pub enum Event { Started, Finished { name: String } }\n\n\
         impl Counter {\n\
         \x20   pub fn with_initial(value: i64) -> Arc<Self> { Arc::new(Self(value)) }\n\
         \x20   pub fn get(&self) -> i64 { self.0 }\n\
         }\n\n\
         pub fn run_job(logger: Arc<dyn Logger>) { logger.log(\"x\".into()); }\n\
         pub fn current_job_state() -> JobState { JobState::Idle }\n\
         pub fn latest_event() -> Event { Event::Started }\n\
         pub async fn slow_add(a: u32, b: u32, _delay_ms: u64) -> u32 { a + b }\n\
         pub async fn async_counter_value(counter: Arc<Counter>) -> i64 { counter.get() }\n\
         pub fn roundtrip_u64(a: u64) -> u64 { a }\n\
         pub fn roundtrip_i64(a: i64) -> i64 { a }\n\
         pub async fn async_roundtrip_u64(a: u64) -> u64 { a }\n\
         pub fn add_u64(a: u64, b: u64) -> u64 { a.wrapping_add(b) }\n\
         pub fn negate_i64(a: i64) -> i64 { a.wrapping_neg() }\n",
    )
    .unwrap();
    let udl = src.join("napi_compat.udl");
    std::fs::write(
        &udl,
        "[Trait, WithForeign]\n\
         interface Logger {\n    void log(string msg);\n};\n\n\
         interface Counter {\n\
         \x20   [Name=with_initial] constructor(i64 value);\n\
         \x20   i64 get();\n\
         };\n\n\
         enum JobState { \"Idle\", \"Running\", \"Done\" };\n\n\
         [Enum]\n\
         interface Event {\n\
         \x20   Started();\n\
         \x20   Finished(string name);\n\
         };\n\n\
         namespace napi_compat {\n\
         \x20   void run_job(Logger logger);\n\
         \x20   JobState current_job_state();\n\
         \x20   Event latest_event();\n\
         \x20   [Async]\n\
         \x20   u32 slow_add(u32 a, u32 b, u64 delay_ms);\n\
         \x20   [Async]\n\
         \x20   i64 async_counter_value(Counter counter);\n\
         \x20   u64 roundtrip_u64(u64 a);\n\
         \x20   i64 roundtrip_i64(i64 a);\n\
         \x20   [Async]\n\
         \x20   u64 async_roundtrip_u64(u64 a);\n\
         \x20   u64 add_u64(u64 a, u64 b);\n\
         \x20   i64 negate_i64(i64 a);\n\
         };\n",
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_rich_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_rich_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("rich napi generator run should succeed");
    host_dir
}

fn write_callback_return_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("callback_return_core");
    let src = core.join("src");
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            "[package]\nname = \"napi-callback-return-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [lib]\nname = \"napi_callback_return_core\"\ncrate-type = [\"lib\"]\n\n\
             [dependencies]\nasync-trait = \"0.1\"\nuniffi = {{ path = {:?}, default-features = false }}\n\n[workspace]\nresolver = \"3\"\n",
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "#[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct Payload {\n\
         \x20   pub left: u32,\n\
         \x20   pub right: u32,\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub struct Counter {\n\
         \x20   inner: std::sync::Mutex<u32>,\n\
         }\n\n\
         impl Counter {\n\
         \x20   pub fn new(initial: u32) -> std::sync::Arc<Self> {\n\
         \x20       std::sync::Arc::new(Self { inner: std::sync::Mutex::new(initial) })\n\
         \x20   }\n\
         \x20   pub fn inc(&self) {\n\
         \x20       *self.inner.lock().unwrap() += 1;\n\
         \x20   }\n\
         \x20   pub fn value(&self) -> u32 {\n\
         \x20       *self.inner.lock().unwrap()\n\
         \x20   }\n\
         }\n\n\
         pub trait Greeter: Send + Sync {\n\
         \x20   fn greet(&self, name: String) -> String;\n\
         }\n\n\
         pub trait HostLogger: Send + Sync {\n\
         \x20   fn greet(&self, name: String) -> String;\n\
         }\n\n\
         pub struct English {\n\
         \x20   prefix: String,\n\
         }\n\n\
         impl Greeter for English {\n\
         \x20   fn greet(&self, name: String) -> String {\n\
         \x20       format!(\"{}{}{}!\", self.prefix, if self.prefix.ends_with(' ') { \"\" } else { \" \" }, name)\n\
         \x20   }\n\
         }\n\n\
         pub fn english_greeter(prefix: String) -> std::sync::Arc<dyn Greeter> {\n\
         \x20   std::sync::Arc::new(English { prefix })\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub enum ProviderError {\n\
         \x20   BadValue,\n\
         }\n\n\
         impl std::fmt::Display for ProviderError {\n\
         \x20   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n\
         \x20       match self {\n\
         \x20           Self::BadValue => write!(f, \"BadValue\"),\n\
         \x20       }\n\
         \x20   }\n\
         }\n\n\
         impl std::error::Error for ProviderError {}\n\n\
         #[async_trait::async_trait]\n\
         pub trait ValueProvider: Send + Sync {\n\
         \x20   fn get_value(&self) -> u32;\n\
         \x20   fn make_payload(&self) -> Payload;\n\
         \x20   fn make_counter(&self, initial: u32) -> std::sync::Arc<Counter>;\n\
         \x20   fn make_greeter(&self, prefix: String) -> std::sync::Arc<dyn Greeter>;\n\
         \x20   fn make_host_logger(&self, prefix: String) -> std::sync::Arc<dyn HostLogger>;\n\
         \x20   async fn make_async_host_logger(&self, prefix: String) -> std::sync::Arc<dyn HostLogger>;\n\
         \x20   async fn checked_make_async_host_logger(&self, prefix: String, fail: bool) -> Result<std::sync::Arc<dyn HostLogger>, ProviderError>;\n\
         \x20   fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;\n\
         \x20   fn checked_payload(&self, fail: bool) -> Result<Payload, ProviderError>;\n\
         \x20   fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;\n\
         }\n\n\
         pub fn invoke_value_provider_get_value(provider: std::sync::Arc<dyn ValueProvider>) -> u32 {\n\
         \x20   provider.get_value()\n\
         }\n\n\
         pub fn invoke_value_provider_make_payload(provider: std::sync::Arc<dyn ValueProvider>) -> Payload {\n\
         \x20   provider.make_payload()\n\
         }\n\n\
         pub fn invoke_value_provider_make_counter(provider: std::sync::Arc<dyn ValueProvider>, initial: u32) -> std::sync::Arc<Counter> {\n\
         \x20   provider.make_counter(initial)\n\
         }\n\n\
         pub fn invoke_value_provider_make_greeter(provider: std::sync::Arc<dyn ValueProvider>, prefix: String) -> std::sync::Arc<dyn Greeter> {\n\
         \x20   provider.make_greeter(prefix)\n\
         }\n\n\
         pub fn invoke_value_provider_run_host_logger(provider: std::sync::Arc<dyn ValueProvider>, prefix: String, name: String) -> String {\n\
         \x20   provider.make_host_logger(prefix).greet(name)\n\
         }\n\n\
         pub async fn invoke_value_provider_run_async_host_logger(provider: std::sync::Arc<dyn ValueProvider>, prefix: String, name: String) -> String {\n\
         \x20   provider.make_async_host_logger(prefix).await.greet(name)\n\
         }\n\n\
         pub async fn invoke_value_provider_run_checked_async_host_logger(provider: std::sync::Arc<dyn ValueProvider>, prefix: String, fail: bool, name: String) -> Result<String, ProviderError> {\n\
         \x20   Ok(provider.checked_make_async_host_logger(prefix, fail).await?.greet(name))\n\
         }\n\n\
         pub fn invoke_value_provider_checked_value(provider: std::sync::Arc<dyn ValueProvider>, fail: bool) -> Result<u32, ProviderError> {\n\
         \x20   provider.checked_value(fail)\n\
         }\n\n\
         pub fn invoke_value_provider_checked_payload(provider: std::sync::Arc<dyn ValueProvider>, fail: bool) -> Result<Payload, ProviderError> {\n\
         \x20   provider.checked_payload(fail)\n\
         }\n\n\
         pub fn invoke_value_provider_checked_void(provider: std::sync::Arc<dyn ValueProvider>, fail: bool) -> Result<bool, ProviderError> {\n\
         \x20   provider.checked_void(fail)?;\n\
         \x20   Ok(true)\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/callback_return.udl");
    std::fs::write(
        &udl,
        r#"
dictionary Payload {
  u32 left;
  u32 right;
};

interface Counter {
  constructor(u32 initial);
  void inc();
  u32 value();
};

[Trait]
interface Greeter {
  string greet(string name);
};

[Trait, WithForeign]
interface HostLogger {
  string greet(string name);
};

[Error]
enum ProviderError {
  "BadValue",
};

[Trait, WithForeign]
interface ValueProvider {
  u32 get_value();
  Payload make_payload();
  Counter make_counter(u32 initial);
  Greeter make_greeter(string prefix);
  HostLogger make_host_logger(string prefix);
  [Async]
  HostLogger make_async_host_logger(string prefix);
  [Async, Throws=ProviderError]
  HostLogger checked_make_async_host_logger(string prefix, boolean fail);
  [Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Throws=ProviderError]
  Payload checked_payload(boolean fail);
  [Throws=ProviderError]
  void checked_void(boolean fail);
};

namespace callback_return {
  u32 invoke_value_provider_get_value(ValueProvider provider);
  Payload invoke_value_provider_make_payload(ValueProvider provider);
  Counter invoke_value_provider_make_counter(ValueProvider provider, u32 initial);
  Greeter invoke_value_provider_make_greeter(ValueProvider provider, string prefix);
  string invoke_value_provider_run_host_logger(ValueProvider provider, string prefix, string name);
  [Async]
  string invoke_value_provider_run_async_host_logger(ValueProvider provider, string prefix, string name);
  [Async, Throws=ProviderError]
  string invoke_value_provider_run_checked_async_host_logger(ValueProvider provider, string prefix, boolean fail, string name);
  Greeter english_greeter(string prefix);
  [Throws=ProviderError]
  u32 invoke_value_provider_checked_value(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
  Payload invoke_value_provider_checked_payload(ValueProvider provider, boolean fail);
  [Throws=ProviderError]
  boolean invoke_value_provider_checked_void(ValueProvider provider, boolean fail);
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_callback_return_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_callback_return_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("callback-return napi generator run should succeed");
    host_dir
}

fn write_async_callback_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("async_callback_core");
    let src = core.join("src");
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            "[package]\nname = \"napi-async-callback-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [lib]\nname = \"napi_async_callback_core\"\ncrate-type = [\"lib\"]\n\n\
             [dependencies]\nasync-trait = \"0.1\"\nuniffi = {{ path = {:?}, default-features = false }}\n\n[workspace]\nresolver = \"3\"\n",
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::sync::Arc;\n\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct WorkRecord {\n\
         \x20   pub total: u32,\n\
         }\n\n\
         #[async_trait::async_trait]\n\
         pub trait AsyncWorker: Send + Sync {\n\
         \x20   async fn note(&self, msg: String);\n\
         \x20   async fn compute(&self, a: u32, b: u32) -> u32;\n\
         \x20   async fn make_record(&self, a: u32, b: u32) -> WorkRecord;\n\
         }\n\n\
         pub async fn run_async_worker(worker: Arc<dyn AsyncWorker>) -> WorkRecord {\n\
         \x20   worker.note(\"start\".to_string()).await;\n\
         \x20   let value = worker.compute(20, 22).await;\n\
         \x20   let record = worker.make_record(value, 1).await;\n\
         \x20   worker.note(\"done\".to_string()).await;\n\
         \x20   record\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/async_callback.udl");
    std::fs::write(
        &udl,
        r#"
dictionary WorkRecord {
  u32 total;
};

[Trait, WithForeign]
interface AsyncWorker {
  [Async]
  void note(string msg);
  [Async]
  u32 compute(u32 a, u32 b);
  [Async]
  WorkRecord make_record(u32 a, u32 b);
};

namespace async_callback {
  [Async]
  WorkRecord run_async_worker(AsyncWorker worker);
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_async_callback_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_async_callback_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("async-callback napi generator run should succeed");
    host_dir
}

fn write_fallible_async_callback_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("fallible_async_callback_core");
    let src = core.join("src");
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            "[package]\nname = \"napi-fallible-async-callback-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
             [lib]\nname = \"napi_fallible_async_callback_core\"\ncrate-type = [\"lib\"]\n\n\
             [dependencies]\nasync-trait = \"0.1\"\nuniffi = {{ path = {:?}, default-features = false }}\n\n[workspace]\nresolver = \"3\"\n",
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::sync::Arc;\n\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct Payload {\n\
         \x20   pub left: u32,\n\
         \x20   pub right: u32,\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub enum ProviderError {\n\
         \x20   BadValue,\n\
         }\n\n\
         impl std::fmt::Display for ProviderError {\n\
         \x20   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n\
         \x20       match self {\n\
         \x20           Self::BadValue => write!(f, \"BadValue\"),\n\
         \x20       }\n\
         \x20   }\n\
         }\n\n\
         impl std::error::Error for ProviderError {}\n\n\
         #[async_trait::async_trait]\n\
         pub trait CheckedWorker: Send + Sync {\n\
         \x20   async fn checked_void(&self, fail: bool) -> Result<(), ProviderError>;\n\
         \x20   async fn checked_value(&self, fail: bool) -> Result<u32, ProviderError>;\n\
         \x20   async fn checked_record(&self, fail: bool) -> Result<Payload, ProviderError>;\n\
         }\n\n\
         pub async fn invoke_checked_void(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<bool, ProviderError> {\n\
         \x20   worker.checked_void(fail).await?;\n\
         \x20   Ok(true)\n\
         }\n\n\
         pub async fn invoke_checked_value(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<u32, ProviderError> {\n\
         \x20   worker.checked_value(fail).await\n\
         }\n\n\
         pub async fn invoke_checked_record(worker: Arc<dyn CheckedWorker>, fail: bool) -> Result<Payload, ProviderError> {\n\
         \x20   worker.checked_record(fail).await\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/fallible_async_callback.udl");
    std::fs::write(
        &udl,
        r#"
dictionary Payload {
  u32 left;
  u32 right;
};

[Error]
enum ProviderError {
  "BadValue",
};

[Trait, WithForeign]
interface CheckedWorker {
  [Async, Throws=ProviderError]
  void checked_void(boolean fail);
  [Async, Throws=ProviderError]
  u32 checked_value(boolean fail);
  [Async, Throws=ProviderError]
  Payload checked_record(boolean fail);
};

namespace fallible_async_callback {
  [Async, Throws=ProviderError]
  boolean invoke_checked_void(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
  u32 invoke_checked_value(CheckedWorker worker, boolean fail);
  [Async, Throws=ProviderError]
  Payload invoke_checked_record(CheckedWorker worker, boolean fail);
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_fallible_async_callback_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_fallible_async_callback_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("fallible async callback napi generator run should succeed");
    host_dir
}

fn write_temporal_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("temporal_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"napi-temporal-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n\
         [lib]\nname = \"napi_temporal_core\"\ncrate-type = [\"lib\"]\n\n\
         [dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::time::{Duration, SystemTime};\n\n\
         #[derive(Clone)]\n\
         pub enum TimeEvent {\n\
         \x20   Point { when: SystemTime },\n\
         \x20   Gap { gap: Duration },\n\
         }\n\n\
         #[derive(Clone)]\n\
         pub struct TimeBundle {\n\
         \x20   pub start: SystemTime,\n\
         \x20   pub gap: Duration,\n\
         \x20   pub maybe_end: Option<SystemTime>,\n\
         \x20   pub checkpoints: Vec<SystemTime>,\n\
         \x20   pub segments: Vec<Duration>,\n\
         \x20   pub event: TimeEvent,\n\
         }\n\n\
         #[derive(Debug)]\n\
         pub enum ChronologicalError {\n\
         \x20   TimeOverflow,\n\
         \x20   TimeDiffError,\n\
         }\n\n\
         impl std::fmt::Display for ChronologicalError {\n\
         \x20   fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {\n\
         \x20       match self {\n\
         \x20           Self::TimeOverflow => write!(f, \"TimeOverflow\"),\n\
         \x20           Self::TimeDiffError => write!(f, \"TimeDiffError\"),\n\
         \x20       }\n\
         \x20   }\n\
         }\n\n\
         impl std::error::Error for ChronologicalError {}\n\n\
         pub fn return_timestamp(a: SystemTime) -> Result<SystemTime, ChronologicalError> {\n\
         \x20   Ok(a)\n\
         }\n\n\
         pub fn return_duration(a: Duration) -> Result<Duration, ChronologicalError> {\n\
         \x20   Ok(a)\n\
         }\n\n\
         pub fn add(a: SystemTime, b: Duration) -> Result<SystemTime, ChronologicalError> {\n\
         \x20   a.checked_add(b).ok_or(ChronologicalError::TimeOverflow)\n\
         }\n\n\
         pub fn diff(a: SystemTime, b: SystemTime) -> Result<Duration, ChronologicalError> {\n\
         \x20   a.duration_since(b).map_err(|_| ChronologicalError::TimeDiffError)\n\
         }\n\n\
         pub fn optional(a: Option<SystemTime>, b: Option<Duration>) -> bool {\n\
         \x20   a.is_some() && b.is_some()\n\
         }\n\n\
         pub fn make_bundle(start: SystemTime, gap: Duration) -> TimeBundle {\n\
         \x20   TimeBundle {\n\
         \x20       start,\n\
         \x20       gap,\n\
         \x20       maybe_end: None,\n\
         \x20       checkpoints: vec![start],\n\
         \x20       segments: vec![gap],\n\
         \x20       event: TimeEvent::Gap { gap },\n\
         \x20   }\n\
         }\n\n\
         pub fn roundtrip_bundle(value: TimeBundle) -> TimeBundle {\n\
         \x20   value\n\
         }\n\n\
         pub fn roundtrip_event(value: TimeEvent) -> TimeEvent {\n\
         \x20   value\n\
         }\n\n\
         pub fn get_far_future_timestamp() -> SystemTime {\n\
         \x20   SystemTime::UNIX_EPOCH\n\
         \x20       .checked_add(Duration::from_secs(8_640_000_000_001))\n\
         \x20       .unwrap()\n\
         }\n",
    )
    .unwrap();
    let udl = core.join("src/napi_temporal_core.udl");
    std::fs::write(
        &udl,
        r#"
[Error]
enum ChronologicalError {
  "TimeOverflow",
  "TimeDiffError",
};

dictionary TimeBundle {
  timestamp start;
  duration gap;
  timestamp? maybe_end;
  sequence<timestamp> checkpoints;
  sequence<duration> segments;
  TimeEvent event;
};

[Enum]
interface TimeEvent {
  Point(timestamp when);
  Gap(duration gap);
};

namespace napi_temporal_core {
  [Throws=ChronologicalError]
  timestamp return_timestamp(timestamp a);
  [Throws=ChronologicalError]
  duration return_duration(duration a);
  [Throws=ChronologicalError]
  timestamp add(timestamp a, duration b);
  [Throws=ChronologicalError]
  duration diff(timestamp a, timestamp b);
  boolean optional(timestamp? a, duration? b);
  TimeBundle make_bundle(timestamp start, duration gap);
  TimeBundle roundtrip_bundle(TimeBundle value);
  TimeEvent roundtrip_event(TimeEvent value);
  timestamp get_far_future_timestamp();
};
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_temporal_napi_host(root: &std::path::Path) -> Utf8PathBuf {
    let (udl, manifest) = write_temporal_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("temporal napi generator run should succeed");
    host_dir
}

#[allow(dead_code)]
fn build_temporal_napi_addon(
    root: &std::path::Path,
    generated: &Utf8PathBuf,
    manifest: &Utf8PathBuf,
) -> Utf8PathBuf {
    let shim = root.join("temporal-napi-shim");
    std::fs::create_dir_all(shim.join("src")).unwrap();
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::write(
        shim.join("Cargo.toml"),
        format!(
            r#"[package]
name = "napi-temporal-core-napi"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
napi_temporal_core = {{ path = {:?} }}
uniffi = {{ path = {:?}, default-features = false }}
napi = {{ version = "3.8.4", default-features = false, features = ["napi8", "tokio_rt"] }}
napi-derive = {{ version = "3.5.3", features = ["type-def"] }}

[build-dependencies]
napi-build = "2.3.1"

[workspace]
resolver = "3"
"#,
            manifest.parent().unwrap().as_str(),
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        shim.join("build.rs"),
        "extern crate napi_build;\nfn main() { napi_build::setup(); }\n",
    )
    .unwrap();
    let bridge = std::fs::read_to_string(generated.join("node/napi_temporal_core.rs")).unwrap();
    std::fs::write(shim.join("src/lib.rs"), bridge).unwrap();

    let target_dir = root.join("cargo-target-temporal-napi");
    let output = run_cargo_build(
        &Utf8PathBuf::from_path_buf(shim.join("Cargo.toml")).unwrap(),
        &[],
        &target_dir,
    )
    .expect("cargo should be available for temporal napi build");
    if !output.status.success() {
        panic!(
            "cargo build on temporal napi shim failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let dylib = target_dir
        .join("debug")
        .join(cdylib_filename("napi-temporal-core-napi"));
    assert!(
        dylib.exists(),
        "expected built cdylib at {}",
        dylib.display()
    );
    let addon = generated.join("node/napi_temporal_core.node");
    std::fs::copy(&dylib, &addon).unwrap();
    addon
}

fn write_custom_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("custom-core");
    std::fs::create_dir_all(core.join("src")).unwrap();
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::write(
        core.join("Cargo.toml"),
        format!(
            r#"[package]
name = "custom_js_core"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["rlib"]

[dependencies]
uniffi = {{ path = {:?}, default-features = false }}
"#,
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        core.join("src/lib.rs"),
        r#"
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UniFfiTag;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Email(pub String);
uniffi::custom_type!(Email, String, {
    lower: |value| value.0,
    try_lift: |value| Ok(Email(value)),
});

impl From<Email> for String {
    fn from(value: Email) -> Self {
        value.0
    }
}

impl From<String> for Email {
    fn from(value: String) -> Self {
        Email(value)
    }
}

#[derive(Clone)]
pub struct Contact {
    pub primary: Email,
    pub aliases: Vec<Email>,
}

impl Contact {
    fn normalize(self) -> Self {
        Self {
            primary: normalize_email(self.primary),
            aliases: self.aliases.into_iter().map(normalize_email).collect(),
        }
    }
}

pub fn normalize_email(value: Email) -> Email {
    Email(value.0.trim().to_ascii_lowercase())
}

pub fn normalize_contact(value: Contact) -> Contact {
    value.normalize()
}

pub fn normalize_many(values: Vec<Email>) -> Vec<Email> {
    values.into_iter().map(normalize_email).collect()
}

pub trait EmailFormatter: Send + Sync {
    fn format_email(&self, value: Email) -> Email;
    fn format_contact(&self, value: Contact) -> Contact;
}

pub fn format_email_with(formatter: std::sync::Arc<dyn EmailFormatter>, value: Email) -> Email {
    formatter.format_email(value)
}

pub fn format_contact_with(formatter: std::sync::Arc<dyn EmailFormatter>, value: Contact) -> Contact {
    formatter.format_contact(value).normalize()
}
"#,
    )
    .unwrap();
    let udl = core.join("src/custom_js_core.udl");
    std::fs::write(
        &udl,
        r#"
[Custom]
typedef string Email;

dictionary Contact {
  Email primary;
  sequence<Email> aliases;
};

[Trait, WithForeign]
interface EmailFormatter {
  Email format_email(Email value);
  Contact format_contact(Contact value);
};

namespace custom_js_core {
  Email normalize_email(Email value);
  Contact normalize_contact(Contact value);
  sequence<Email> normalize_many(sequence<Email> values);
  Email format_email_with(EmailFormatter formatter, Email value);
  Contact format_contact_with(EmailFormatter formatter, Contact value);
};
"#,
    )
    .unwrap();
    let config = root.join("uniffi.toml");
    std::fs::write(
        &config,
        r#"
[bindings.javascript.customTypes.Email]
typeName = "EmailAddress"
imports = [
  "type { EmailAddress } from \"./email.ts\"",
  "{ emailAddressFromString, emailAddressToString } from \"./email.ts\"",
]
intoCustom = "emailAddressFromString({})"
fromCustom = "emailAddressToString({})"
"#,
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(config).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

fn generate_custom_napi_tree(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let (udl, config, manifest) = write_custom_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: Some(config),
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Electron],
        },
    )
    .expect("custom napi generator run should succeed");
    std::fs::write(
        out_dir.join("common/email.ts"),
        r#"
export type EmailAddress = { value: string };
export function emailAddressFromString(value: string): EmailAddress {
  return { value };
}
export function emailAddressToString(value: EmailAddress): string {
  return value.value;
}
"#,
    )
    .unwrap();
    (out_dir, manifest)
}

fn build_custom_napi_addon(
    root: &std::path::Path,
    generated: &Utf8PathBuf,
    manifest: &Utf8PathBuf,
) -> Utf8PathBuf {
    let shim = root.join("custom-napi-shim");
    std::fs::create_dir_all(shim.join("src")).unwrap();
    let uniffi_path = workspace_root().join("uniffi");
    std::fs::write(
        shim.join("Cargo.toml"),
        format!(
            r#"[package]
name = "custom_js_core_napi"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
custom_js_core = {{ path = {:?} }}
uniffi = {{ path = {:?}, default-features = false }}
napi = {{ version = "3.8.4", default-features = false, features = ["napi8", "tokio_rt"] }}
napi-derive = {{ version = "3.5.3", features = ["type-def"] }}

[build-dependencies]
napi-build = "2.3.1"

[workspace]
resolver = "3"
"#,
            manifest.parent().unwrap().as_str(),
            uniffi_path.as_str()
        ),
    )
    .unwrap();
    std::fs::write(
        shim.join("build.rs"),
        "extern crate napi_build;\nfn main() { napi_build::setup(); }\n",
    )
    .unwrap();
    let bridge = std::fs::read_to_string(generated.join("node/custom_js_core.rs")).unwrap();
    std::fs::write(shim.join("src/lib.rs"), bridge).unwrap();

    let target_dir = root.join("cargo-target-custom-napi");
    let output = run_cargo_build(
        &Utf8PathBuf::from_path_buf(shim.join("Cargo.toml")).unwrap(),
        &[],
        &target_dir,
    )
    .expect("cargo should be available for custom napi build");
    if !output.status.success() {
        panic!(
            "cargo build on custom napi shim failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    let dylib = target_dir
        .join("debug")
        .join(cdylib_filename("custom_js_core_napi"));
    assert!(
        dylib.exists(),
        "expected built cdylib at {}",
        dylib.display()
    );
    let addon = generated.join("node/custom_js_core.node");
    std::fs::copy(&dylib, &addon).unwrap();
    addon
}

#[test]
fn custom_types_emit_public_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let (generated, _manifest) = generate_custom_napi_tree(tmp.path());

    let custom_types = std::fs::read_to_string(generated.join("common/custom-types.ts")).unwrap();
    assert!(
        custom_types.contains("import type { EmailAddress } from \"./email.ts\";"),
        "custom-types.ts should emit configured type import:\n{custom_types}"
    );
    assert!(
        custom_types.contains(
            "import { emailAddressFromString, emailAddressToString } from \"./email.ts\";"
        ),
        "custom-types.ts should emit configured value import:\n{custom_types}"
    );
    assert!(
        custom_types.contains("export type Email = EmailAddress;"),
        "custom-types.ts should alias Email to EmailAddress:\n{custom_types}"
    );
    assert!(
        custom_types.contains("__uniffiLowerCustomEmail")
            && custom_types.contains("__uniffiLiftCustomEmail"),
        "custom-types.ts should emit lower/lift helpers:\n{custom_types}"
    );

    let api = std::fs::read_to_string(generated.join("common/api.ts")).unwrap();
    assert!(
        !api.contains("unknown /* custom:"),
        "common/api.ts must not leave custom types as unknown:\n{api}"
    );
    assert!(
        api.contains("export type { Email } from \"./custom-types.ts\";"),
        "common/api.ts should re-export Email:\n{api}"
    );
    assert!(
        api.contains("import { __uniffiLiftCustomEmail, __uniffiLowerCustomEmail } from \"./custom-types.ts\";"),
        "common/api.ts should import the custom-type helpers:\n{api}"
    );
    let public_types = std::fs::read_to_string(generated.join("common/public-types.ts")).unwrap();
    assert!(
        public_types.contains("export type { Email } from \"./custom-types.ts\";"),
        "public-types.ts should re-export Email:\n{public_types}"
    );
}

#[test]
fn generated_node_adapter_runs_custom_types_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP generated_node_adapter_runs_custom_types_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let _addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let driver = generated.join("custom-driver.ts");
    std::fs::write(
        &driver,
        r#"
import {
  formatContactWith,
  formatEmailWith,
  normalizeContact,
  normalizeEmail,
  normalizeMany,
} from "./node/index.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const one = normalizeEmail({ value: "  A@EXAMPLE.COM  " });
assert(one.value === "a@example.com", `normalizeEmail=${JSON.stringify(one)}`);

const contact = normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
assert(contact.primary.value === "root@example.com", `contact.primary=${contact.primary.value}`);
assert(
  contact.aliases[0].value === "alias@one.com" &&
    contact.aliases[1].value === "two@example.com",
  `contact.aliases=${JSON.stringify(contact.aliases)}`,
);

const many = normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
assert(
  many[0].value === "x@y.com" && many[1].value === "z@q.com",
  `normalizeMany=${JSON.stringify(many)}`,
);

const formatter = {
  formatEmail(value: { value: string }) {
    return { value: `${value.value.trim().toUpperCase()}!` };
  },
  formatContact(value: { primary: { value: string }; aliases: Array<{ value: string }> }) {
    return {
      primary: { value: value.primary.value.trim().toUpperCase() },
      aliases: value.aliases.map((alias) => ({ value: alias.value.trim().toUpperCase() })),
    };
  },
};
const formatted = formatEmailWith(formatter, { value: " ada@example.com " });
assert(formatted.value === "ADA@EXAMPLE.COM!", `formatEmailWith=${JSON.stringify(formatted)}`);
const formattedContact = formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
assert(formattedContact.primary.value === "root@example.com", `formattedContact.primary=${formattedContact.primary.value}`);
assert(formattedContact.aliases[0].value === "alias@one.com", `formattedContact.aliases=${JSON.stringify(formattedContact.aliases)}`);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to invoke node for custom adapter driver");
    if !output.status.success() {
        panic!(
            "custom node adapter driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "custom node adapter driver did not print ok"
    );
}

#[test]
fn host_crates_napi_compiles_temporal_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_temporal_napi_host(tmp.path());

    let bridge = std::fs::read_to_string(
        Utf8PathBuf::from_path_buf(tmp.path().join("generated/node/napi_temporal_core.rs"))
            .unwrap(),
    )
    .unwrap();
    assert!(
        bridge.contains("__UniffiTimestamp") && bridge.contains("__UniffiDuration"),
        "temporal napi bridge should emit explicit wrappers, got:\n{bridge}"
    );
    assert!(
        !bridge.contains("timestamp/duration are not supported"),
        "temporal napi bridge must not reject timestamp/duration anymore:\n{bridge}"
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-temporal-napi-check");
    let output = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_compiles_temporal_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on temporal napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let electron_preload =
        std::fs::read_to_string(tmp.path().join("generated/electron/preload.cjs")).unwrap();
    assert!(
        !electron_preload.contains("unsupported"),
        "electron preload should remain on the supported temporal path:\n{electron_preload}"
    );
}

#[test]
fn generated_node_adapter_runs_temporal_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP generated_node_adapter_runs_temporal_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_temporal_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-temporal-napi-runtime");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP generated_node_adapter_runs_temporal_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on temporal napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-temporal-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/napi_temporal_core.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("napi_temporal_core.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();

    let electron_stub = electron_dir.join("node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"const state = { api: null };
exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
    state.api = value;
  },
};
exports.__state = state;
"#,
    )
    .unwrap();

    let driver = generated.join("temporal-driver.ts");
    std::fs::write(
        &driver,
        r#"
import {
    returnTimestamp,
    returnDuration,
    add,
    diff,
    optional,
    makeBundle,
    roundtripBundle,
    roundtripEvent,
    getFarFutureTimestamp,
} from "./node/index.ts";
import { UniffiError } from "./common/runtime.ts";

const ts = new Date("2024-01-02T03:04:05.283Z");
const tsRound = returnTimestamp(ts);
if (!(tsRound instanceof Date) || tsRound.getTime() !== ts.getTime()) {
    throw new Error(`timestamp round-trip failed: ${tsRound}`);
}

const dur = 1500.5;
const durRound = returnDuration(dur);
if (durRound !== dur) {
    throw new Error(`duration round-trip failed: ${durRound}`);
}

const added = add(new Date(1000), 2000);
if (!(added instanceof Date) || added.getTime() !== 3000) {
    throw new Error(`timestamp + duration failed: ${added}`);
}

const delta = diff(new Date(3000), new Date(1000));
if (delta !== 2000) {
    throw new Error(`timestamp - timestamp failed: ${delta}`);
}

if (!optional(ts, dur)) throw new Error("optional(Some, Some) should be true");
if (optional(null, dur)) throw new Error("optional(None, Some) should be false");
if (optional(ts, null)) throw new Error("optional(Some, None) should be false");

const bundle = makeBundle(ts, dur);
if (!(bundle.start instanceof Date) || bundle.start.getTime() !== ts.getTime()) {
    throw new Error(`bundle.start failed: ${bundle.start}`);
}
if (bundle.maybe_end != null) {
    throw new Error(`bundle.maybe_end should be nullish: ${bundle.maybe_end}`);
}
if (!(bundle.checkpoints[0] instanceof Date) || bundle.checkpoints[0].getTime() !== ts.getTime()) {
    throw new Error(`bundle.checkpoints failed: ${bundle.checkpoints[0]}`);
}
if (bundle.segments[0] !== dur) {
    throw new Error(`bundle.segments failed: ${bundle.segments[0]}`);
}
if (bundle.event.tag !== "Gap" || bundle.event.gap !== dur) {
    throw new Error(`bundle.event failed: ${JSON.stringify(bundle.event)}`);
}

const bundleRound = roundtripBundle(bundle);
if (bundleRound.event.tag !== "Gap" || bundleRound.event.gap !== dur) {
    throw new Error(`bundle round-trip failed: ${JSON.stringify(bundleRound)}`);
}
if (bundleRound.checkpoints.length !== 1 || bundleRound.checkpoints[0].getTime() !== ts.getTime()) {
    throw new Error(`bundle checkpoints round-trip failed: ${JSON.stringify(bundleRound.checkpoints)}`);
}

const eventRound = roundtripEvent({ tag: "Point", when: ts });
if (eventRound.tag !== "Point" || !(eventRound.when instanceof Date) || eventRound.when.getTime() !== ts.getTime()) {
    throw new Error(`enum payload round-trip failed: ${JSON.stringify(eventRound)}`);
}

let threw = false;
try {
    returnDuration(-1);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`bad duration threw wrong type: ${e && (e as Error).message}`);
    }
    if (!/duration.*non-negative/i.test((e as Error).message)) {
        throw new Error(`bad duration message: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("returnDuration(-1) should throw");

threw = false;
try {
    returnDuration(Number.POSITIVE_INFINITY);
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`non-finite duration threw wrong type: ${e && (e as Error).message}`);
    }
}
if (!threw) throw new Error("returnDuration(Infinity) should throw");

threw = false;
try {
    returnTimestamp(new Date(8.64e15 + 1));
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`bad timestamp threw wrong type: ${e && (e as Error).message}`);
    }
    const message = (e as Error).message;
    if (!/invalid Date|timestamp exceeds JS Date range/i.test(message)) {
        throw new Error(`bad timestamp message: ${message}`);
    }
}
if (!threw) throw new Error("returnTimestamp(out of range) should throw");

threw = false;
try {
    getFarFutureTimestamp();
} catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
        throw new Error(`far future threw wrong type: ${e && (e as Error).message}`);
    }
    if (!(e as Error).message.includes("timestamp exceeds JS Date range")) {
        throw new Error(`far future message: ${(e as Error).message}`);
    }
}
if (!threw) throw new Error("getFarFutureTimestamp() should throw");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to invoke node for temporal driver");
    if !output.status.success() {
        panic!(
            "temporal node adapter driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "temporal node adapter driver did not print ok"
    );
}

#[test]
fn host_crates_napi_compiles_enum_callback_async_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());

    // Sanity check: the generated bridge actually uses the newer
    // napi-rs surface whose compatibility is the point of this test.
    let bridge = std::fs::read_to_string(
        Utf8PathBuf::from_path_buf(tmp.path().join("generated/node/napi_compat.rs")).unwrap(),
    )
    .unwrap();
    assert!(
        bridge.contains("discriminant = \"type\""),
        "rich fixture should exercise #[napi(discriminant = \"type\")]"
    );
    assert!(
        bridge.contains("string_enum"),
        "rich fixture should exercise #[napi(string_enum)] for flat enums"
    );
    assert!(
        bridge.contains("ThreadsafeFunction"),
        "rich fixture should exercise ThreadsafeFunction"
    );
    assert!(
        bridge.contains("napi::bindgen_prelude::BigInt"),
        "rich fixture should use napi::BigInt for u64/i64, got:\n{bridge}"
    );
    assert!(
        bridge.contains("pub fn async_counter_value(")
            && bridge.contains("__uniffi_env: Env,")
            && bridge.contains("counter: ClassInstance<'_, Counter>,")
            && bridge.contains("Result<PromiseRaw<'static, napi::bindgen_prelude::BigInt>>")
            && bridge.contains("let __uniffi_counter = (*(counter)).0.clone();")
            && bridge.contains(".spawn_future(async move"),
        "async function with object args should lower ClassInstance before spawning a Promise:\n{bridge}"
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("napi = { version = \"3"),
        "napi host crate template must default to napi 3.x, got:\n{cargo_toml}"
    );
    assert!(
        cargo_toml.contains("napi-derive = { version = \"3") && cargo_toml.contains("type-def"),
        "napi-derive must default to 3.x with type-def, got:\n{cargo_toml}"
    );

    let target_dir = tmp.path().join("cargo-target-napi-rich");
    let output = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_compiles_enum_callback_async_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on rich napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn host_crates_napi_runs_bigint_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_bigint_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-napi-runtime");
    let output = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_bigint_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo build on rich napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-compat-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/napi_compat.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("napi_compat.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"const state = { api: null };
exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
    state.api = value;
  },
};
exports.__state = state;
"#,
    )
    .unwrap();

    let driver = generated.join("bigint-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const raw = require("./node/napi_compat.node");

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

function expectBigint(value: unknown, expected: bigint, label: string): void {
  assert(typeof value === "bigint", `${label}: expected bigint, got ${typeof value}`);
  assert(value === expected, `${label}: expected ${expected}, got ${String(value)}`);
}

function expectThrow(label: string, fn: () => unknown, re: RegExp): void {
  try {
    fn();
    throw new Error(`${label}: expected throw`);
  } catch (e) {
    const message = e instanceof Error ? e.message : String(e);
    if (!re.test(message)) {
      throw new Error(`${label}: unexpected error ${message}`);
    }
  }
}

expectBigint(raw.roundtripU64(18446744073709551615n), 18446744073709551615n, "raw roundtripU64");
expectBigint(raw.roundtripI64(9223372036854775807n), 9223372036854775807n, "raw roundtripI64 max");
expectBigint(raw.roundtripI64(-9223372036854775808n), -9223372036854775808n, "raw roundtripI64 min");
expectBigint(await raw.asyncRoundtripU64(18446744073709551615n), 18446744073709551615n, "raw asyncRoundtripU64");
const rawCounter = raw.counterWithInitial(3n);
expectBigint(raw.counterGet(rawCounter), 3n, "raw counterGet");
assert(await raw.slowAdd(20, 22, 300n) === 42, "raw slowAdd mixed args");
expectThrow("raw u64 overflow", () => raw.roundtripU64(18446744073709551616n), /u64/i);
expectThrow("raw i64 overflow", () => raw.roundtripI64(9223372036854775808n), /i64/i);
expectThrow("raw i64 underflow", () => raw.roundtripI64(-9223372036854775809n), /i64/i);

const nodeApi = await import("./node/index.ts");
expectBigint(nodeApi.roundtripU64(18446744073709551615n), 18446744073709551615n, "node api roundtripU64");
expectBigint(nodeApi.roundtripI64(-9223372036854775808n), -9223372036854775808n, "node api roundtripI64");
expectBigint(await nodeApi.asyncRoundtripU64(18446744073709551615n), 18446744073709551615n, "node api asyncRoundtripU64");
const nodeCounter = nodeApi.Counter.withInitial(3n);
expectBigint(nodeCounter.get(), 3n, "node api counter.get");
nodeCounter.dispose();
nodeCounter.dispose();
expectThrow("node api counter use-after-dispose", () => nodeCounter.get(), /dispose|UniffiUseAfterDispose/i);
assert(await nodeApi.slowAdd(20, 22, 300n) === 42, "node api slowAdd mixed args");

require("./electron/preload.cjs");
const bridge = (globalThis as any).__uniffi__;
assert(bridge && typeof bridge.dispatchSync === "function", "electron preload bridge");
let res = bridge.dispatchSync({ kind: "call", id: 1, method: "roundtrip_u64", args: [18446744073709551615n] });
assert(res.kind === "ok", `electron sync response kind ${res.kind}`);
expectBigint(res.value, 18446744073709551615n, "electron sync roundtripU64");
res = bridge.dispatchSync({ kind: "call", id: 2, method: "roundtrip_i64", args: [-9223372036854775808n] });
assert(res.kind === "ok", `electron sync i64 response kind ${res.kind}`);
expectBigint(res.value, -9223372036854775808n, "electron sync roundtripI64");
res = bridge.dispatchSync({ kind: "call", id: 3, method: "counter_with_initial", args: [3n] });
assert(res.kind === "ok", `electron counter ctor kind ${res.kind}`);
const counterHandle = res.value;
res = bridge.dispatchSync({ kind: "call", id: 4, method: "counter_get", args: [counterHandle] });
assert(res.kind === "ok", `electron counter get kind ${res.kind}`);
expectBigint(res.value, 3n, "electron counterGet");
const asyncRes = await bridge.dispatchAsync({ kind: "call", id: 5, method: "async_roundtrip_u64", args: [18446744073709551615n] });
assert(asyncRes.kind === "ok", `electron async response kind ${asyncRes.kind}`);
expectBigint(asyncRes.value, 18446744073709551615n, "electron async roundtripU64");
const slowAddRes = await bridge.dispatchAsync({ kind: "call", id: 6, method: "slow_add", args: [20, 22, 300n] });
assert(slowAddRes.kind === "ok", `electron slow_add kind ${slowAddRes.kind}`);
assert(slowAddRes.value === 42, `electron slow_add result ${slowAddRes.value}`);
const overflowRes = bridge.dispatchSync({ kind: "call", id: 7, method: "roundtrip_u64", args: [18446744073709551616n] });
assert(overflowRes.kind === "err", "electron overflow should error");
assert(/u64/i.test(String(overflowRes.error?.message ?? "")), "electron overflow message");

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run bigint driver");
    if !output.status.success() {
        panic!(
            "bigint driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "bigint driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn host_crates_napi_runs_callback_return_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP host_crates_napi_runs_callback_return_fixture: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_callback_return_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-callback-return");

    let check = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_callback_return_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !check.status.success() {
        panic!(
            "cargo check on callback-return napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr),
        );
    }

    let build = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_callback_return_fixture: cargo unavailable during build: {e}");
            return;
        }
    };
    if !build.status.success() {
        panic!(
            "cargo build on callback-return napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-callback-return-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built callback-return cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/callback_return.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("callback_return.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"const state = { api: null };
exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
    state.api = value;
  },
};
exports.__state = state;
"#,
    )
    .unwrap();

    let callbacks = std::fs::read_to_string(generated.join("common/callbacks.ts")).unwrap();
    assert!(
        callbacks.contains("interface ValueProvider")
            && callbacks.contains("makePayload(): Payload")
            && callbacks.contains("makeCounter(initial: number): Counter")
            && callbacks.contains("makeGreeter(prefix: string): Greeter")
            && callbacks.contains("makeHostLogger(prefix: string): HostLogger"),
        "common/callbacks.ts should expose a return-capable callback interface:\n{callbacks}"
    );

    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("__uniffiCallback"),
        "electron preload must keep unwrapping callback markers for callback returns"
    );
    let renderer = std::fs::read_to_string(generated.join("electron/renderer.ts")).unwrap();
    assert!(
        renderer.contains("__installBackend"),
        "electron renderer must still install the backend"
    );

    let driver = generated.join("callback-return-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { invokeValueProviderGetValue, invokeValueProviderMakePayload } from "./node/index.ts";
import {
    Counter,
    ProviderError,
    englishGreeter,
    invokeValueProviderCheckedPayload,
    invokeValueProviderCheckedValue,
    invokeValueProviderCheckedVoid,
    invokeValueProviderMakeCounter,
    invokeValueProviderMakeGreeter,
    invokeValueProviderRunAsyncHostLogger,
    invokeValueProviderRunCheckedAsyncHostLogger,
    invokeValueProviderRunHostLogger,
} from "./node/index.ts";
import { UniffiError } from "./common/runtime.ts";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
// Simulate the renderer global before importing the electron entry.
(globalThis as any).window = globalThis as any;
const electronApi = await import("./electron/renderer.ts");
require("./electron/preload.cjs");

const provider = {
    getValue() {
        return 42;
    },
    makePayload() {
        return { left: 7, right: 11 };
    },
    makeCounter(initial: number) {
        return Counter.new(initial);
    },
    makeGreeter(prefix: string) {
        return englishGreeter(prefix);
    },
    makeHostLogger(prefix: string) {
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async makeAsyncHostLogger(prefix: string) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async checkedMakeAsyncHostLogger(prefix: string, fail: boolean) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    checkedValue(fail: boolean) {
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return 77;
    },
    checkedPayload(fail: boolean) {
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return { left: 13, right: 17 };
    },
    checkedVoid(fail: boolean) {
        if (fail) throw new ProviderError("BadValue", "BadValue");
    },
};

const scalar = invokeValueProviderGetValue(provider as any);
if (scalar !== 42) {
    throw new Error(`getValue failed: ${scalar}`);
}
const payload = invokeValueProviderMakePayload(provider as any);
if (payload.left !== 7 || payload.right !== 11) {
    throw new Error(`makePayload failed: ${JSON.stringify(payload)}`);
}
const returnedCounter = invokeValueProviderMakeCounter(provider as any, 10);
returnedCounter.inc();
if (returnedCounter.value() !== 11) {
    throw new Error(`node makeCounter failed: ${returnedCounter.value()}`);
}
const returnedGreeter = invokeValueProviderMakeGreeter(provider as any, "Hi");
if (returnedGreeter.greet("Ada") !== "Hi Ada!") {
    throw new Error(`node makeGreeter failed: ${returnedGreeter.greet("Ada")}`);
}
const returnedHostLogger = invokeValueProviderRunHostLogger(provider as any, "Host", "Ada");
if (returnedHostLogger !== "Host Ada!") {
    throw new Error(`node runHostLogger failed: ${returnedHostLogger}`);
}
const returnedAsyncHostLogger = await invokeValueProviderRunAsyncHostLogger(provider as any, "AsyncHost", "Ada");
if (returnedAsyncHostLogger !== "AsyncHost Ada!") {
    throw new Error(`node runAsyncHostLogger failed: ${returnedAsyncHostLogger}`);
}
const checkedAsyncHostLogger = await invokeValueProviderRunCheckedAsyncHostLogger(provider as any, "CheckedHost", false, "Ada");
if (checkedAsyncHostLogger !== "CheckedHost Ada!") {
    throw new Error(`node checked async host logger failed: ${checkedAsyncHostLogger}`);
}
let checkedAsyncHostLoggerFailed = false;
try {
    await invokeValueProviderRunCheckedAsyncHostLogger(provider as any, "CheckedHost", true, "Ada");
} catch (e) {
    checkedAsyncHostLoggerFailed = true;
    if (!(e instanceof UniffiError) || !String((e as Error).message).includes("BadValue")) {
        throw new Error(`checked async host logger threw wrong error: ${e && (e as Error).message}`);
    }
}
if (!checkedAsyncHostLoggerFailed) {
    throw new Error("checked async host logger should throw");
}
if (invokeValueProviderCheckedValue(provider as any, false) !== 77) {
    throw new Error("checkedValue(false) failed");
}
const checkedPayload = invokeValueProviderCheckedPayload(provider as any, false);
if (checkedPayload.left !== 13 || checkedPayload.right !== 17) {
    throw new Error(`checkedPayload(false) failed: ${JSON.stringify(checkedPayload)}`);
}
if (invokeValueProviderCheckedVoid(provider as any, false) !== true) {
    throw new Error("checkedVoid(false) failed");
}

for (const [label, fn] of [
    ["checkedValue", () => invokeValueProviderCheckedValue(provider as any, true)],
    ["checkedPayload", () => invokeValueProviderCheckedPayload(provider as any, true)],
    ["checkedVoid", () => invokeValueProviderCheckedVoid(provider as any, true)],
] as const) {
    let threw = false;
    try {
        fn();
    } catch (e) {
        threw = true;
        if (!(e instanceof UniffiError)) {
            throw new Error(`${label} threw wrong type: ${e && (e as Error).message}`);
        }
        if (!String((e as Error).message).includes("BadValue")) {
            throw new Error(`${label} threw wrong message: ${(e as Error).message}`);
        }
    }
    if (!threw) throw new Error(`${label}(true) should throw`);
}

(globalThis as any).window = globalThis as any;
require("./electron/preload.cjs");
const bridge = (globalThis as any).__uniffi__;
if (!bridge || typeof bridge.dispatchSync !== "function") {
    throw new Error("missing electron preload bridge");
}
const electronProvider = {
    ...provider,
    makeCounter(initial: number) {
        return electronApi.Counter.new(initial);
    },
    makeGreeter(prefix: string) {
        return electronApi.englishGreeter(prefix);
    },
    makeHostLogger(prefix: string) {
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async makeAsyncHostLogger(prefix: string) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
    async checkedMakeAsyncHostLogger(prefix: string, fail: boolean) {
        await new Promise((resolve) => setTimeout(resolve, 1));
        if (fail) throw new electronApi.ProviderError("BadValue", "BadValue");
        return {
            greet(name: string) {
                return `${prefix} ${name}!`;
            },
        };
    },
};
const electronCounter = electronApi.invokeValueProviderMakeCounter(electronProvider as any, 12);
electronCounter.inc();
if (electronCounter.value() !== 13) {
    throw new Error(`electron makeCounter failed: ${electronCounter.value()}`);
}
electronCounter.dispose();
electronCounter.dispose();
let electronCounterUseAfterDispose = false;
try {
    electronCounter.value();
} catch (e) {
    electronCounterUseAfterDispose = true;
    if (!/dispose|UniffiUseAfterDispose/i.test(String((e as Error).message ?? e))) {
        throw new Error(`electron counter use-after-dispose wrong error: ${e && (e as Error).message}`);
    }
}
if (!electronCounterUseAfterDispose) {
    throw new Error("electron counter use-after-dispose should throw");
}
const electronGreeter = electronApi.invokeValueProviderMakeGreeter(electronProvider as any, "Yo");
if (electronGreeter.greet("Ada") !== "Yo Ada!") {
    throw new Error(`electron makeGreeter failed: ${electronGreeter.greet("Ada")}`);
}
const electronHostLogger = electronApi.invokeValueProviderRunHostLogger(electronProvider as any, "EH", "Ada");
if (electronHostLogger !== "EH Ada!") {
    throw new Error(`electron runHostLogger failed: ${electronHostLogger}`);
}
const electronAsyncHostLogger = await electronApi.invokeValueProviderRunAsyncHostLogger(electronProvider as any, "EAH", "Ada");
if (electronAsyncHostLogger !== "EAH Ada!") {
    throw new Error(`electron runAsyncHostLogger failed: ${electronAsyncHostLogger}`);
}
const electronCheckedAsyncHostLogger = await electronApi.invokeValueProviderRunCheckedAsyncHostLogger(electronProvider as any, "EChecked", false, "Ada");
if (electronCheckedAsyncHostLogger !== "EChecked Ada!") {
    throw new Error(`electron checked async host logger failed: ${electronCheckedAsyncHostLogger}`);
}
let electronCheckedAsyncHostLoggerFailed = false;
try {
    await electronApi.invokeValueProviderRunCheckedAsyncHostLogger(electronProvider as any, "EChecked", true, "Ada");
} catch (e) {
    electronCheckedAsyncHostLoggerFailed = true;
    if (!(e instanceof electronApi.UniffiError) || !String((e as Error).message).includes("BadValue")) {
        throw new Error(`electron checked async host logger wrong error: ${e && (e as Error).message}`);
    }
}
if (!electronCheckedAsyncHostLoggerFailed) {
    throw new Error("electron checked async host logger should throw");
}
const electronMarker = {
    __uniffiCallback: true,
    object: provider,
    fallibleMethods: {
        checkedValue: "flat",
        checkedPayload: "flat",
        checkedVoid: "flat",
        checkedMakeAsyncHostLogger: "flat",
    },
    asyncMethods: {
        makeAsyncHostLogger: true,
        checkedMakeAsyncHostLogger: true,
    },
    callbackReturnMethods: {
        makeAsyncHostLogger: true,
        checkedMakeAsyncHostLogger: true,
    },
};
let electronRes = bridge.dispatchSync({
    kind: "call",
    id: 1,
    method: "invoke_value_provider_checked_value",
    args: [{ ...electronMarker, object: electronProvider }, false],
});
if (electronRes.kind !== "ok" || electronRes.value !== 77) {
    throw new Error(`electron checked value failed: ${JSON.stringify(electronRes)}`);
}
electronRes = bridge.dispatchSync({
    kind: "call",
    id: 2,
    method: "invoke_value_provider_checked_value",
    args: [{ ...electronMarker, object: electronProvider }, true],
});
if (electronRes.kind !== "err" || !String(electronRes.error?.message ?? "").includes("BadValue")) {
    throw new Error(`electron checked value error failed: ${JSON.stringify(electronRes)}`);
}

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run callback-return driver");
    if !output.status.success() {
        panic!(
            "callback-return driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "callback-return driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn host_crates_napi_runs_async_callback_trait_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP host_crates_napi_runs_async_callback_trait_fixture: node 22.6+ unavailable"
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_async_callback_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-async-callback");

    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("async-trait = \"0.1\""),
        "napi host crate must include async-trait for async callback impls:\n{cargo_toml}"
    );

    let build = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_async_callback_trait_fixture: cargo unavailable during build: {e}");
            return;
        }
    };
    if !build.status.success() {
        panic!(
            "cargo build on async-callback napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-async-callback-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built async-callback cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/async_callback.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("async_callback.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();

    let callbacks = std::fs::read_to_string(generated.join("common/callbacks.ts")).unwrap();
    assert!(
        callbacks.contains("note(msg: string): void | Promise<void>;")
            && callbacks.contains("compute(a: number, b: number): number | Promise<number>;")
            && callbacks
                .contains("makeRecord(a: number, b: number): WorkRecord | Promise<WorkRecord>;")
            && callbacks.contains("Promise.resolve(__uniffiCallbackObject.compute"),
        "common/callbacks.ts should expose and lower async callback methods:\n{callbacks}"
    );
    let api = std::fs::read_to_string(generated.join("common/api.ts")).unwrap();
    for needle in [
        "asyncMethods: {",
        "\"note\": true",
        "\"compute\": true",
        "\"makeRecord\": true",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts should mark async callback methods with `{needle}`:\n{api}"
        );
    }
    let bridge_path = std::fs::read_dir(generated.join("node"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .expect("generated node bridge should contain a Rust shim");
    let bridge = std::fs::read_to_string(bridge_path).unwrap();
    assert!(
        bridge.contains("#[async_trait::async_trait]")
            && bridge.contains("napi::bindgen_prelude::Promise<WorkRecord>")
            && bridge.contains(".call_async(Ok"),
        "napi bridge should implement async callback methods through TSFN Promise:\n{bridge}"
    );
    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("asyncMethods")
            && preload.contains("const liftedArgs = callArgs.map(__uniffiLiftShape);")
            && preload.contains("Promise.resolve(v(...liftedArgs))"),
        "electron preload should preserve async callback marker behavior:\n{preload}"
    );

    let driver = generated.join("async-callback-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { runAsyncWorker } from "./node/index.ts";
import { createRequire } from "node:module";

globalThis.window = globalThis;
const require = createRequire(import.meta.url);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

function delay(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 1));
}

function makeWorker(label: string) {
  const calls: string[] = [];
  return {
    calls,
    worker: {
      async note(msg: string): Promise<void> {
        await delay();
        calls.push(`${label}:${msg}`);
      },
      async compute(a: number, b: number): Promise<number> {
        await delay();
        return a + b;
      },
      async makeRecord(a: number, b: number): Promise<{ total: number }> {
        await delay();
        return { total: a + b };
      },
    },
  };
}

const nodeCase = makeWorker("node");
const nodeRecord = await runAsyncWorker(nodeCase.worker as any);
assert(nodeRecord.total === 43, `node total=${nodeRecord.total}`);
assert(nodeCase.calls.join(",") === "node:start,node:done", `node calls=${nodeCase.calls.join(",")}`);

require("./electron/preload.cjs");
const electronApi = await import("./electron/renderer.ts");
const electronCase = makeWorker("electron");
const electronRecord = await electronApi.runAsyncWorker(electronCase.worker as any);
assert(electronRecord.total === 43, `electron total=${electronRecord.total}`);
assert(electronCase.calls.join(",") === "electron:start,electron:done", `electron calls=${electronCase.calls.join(",")}`);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run async-callback driver");
    if !output.status.success() {
        panic!(
            "async-callback driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "async-callback driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn host_crates_napi_runs_fallible_async_callback_fixture() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!(
            "SKIP host_crates_napi_runs_fallible_async_callback_fixture: node 22.6+ unavailable"
        );
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_fallible_async_callback_napi_host(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-fallible-async-callback");

    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("async-trait = \"0.1\"")
            && cargo_toml.contains("napi = { version = \"3.8.4\""),
        "napi host crate template should keep async-trait + napi 3.x defaults:\n{cargo_toml}"
    );

    let build = match run_cargo_build(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_runs_fallible_async_callback_fixture: cargo unavailable during build: {e}");
            return;
        }
    };
    if !build.status.success() {
        panic!(
            "cargo build on fallible-async napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&build.stdout),
            String::from_utf8_lossy(&build.stderr),
        );
    }

    let lib_name = cdylib_filename("napi-fallible-async-callback-core-napi");
    let built_lib = target_dir.join("debug").join(lib_name);
    assert!(
        built_lib.exists(),
        "expected built fallible-async cdylib at {}",
        built_lib.display()
    );

    let generated = tmp.path().join("generated");
    let node_addon = generated.join("node/fallible_async_callback.node");
    std::fs::copy(&built_lib, &node_addon).unwrap();

    let electron_dir = generated.join("electron");
    let electron_addon = electron_dir.join("fallible_async_callback.node");
    std::fs::copy(&built_lib, &electron_addon).unwrap();
    let electron_stub = electron_dir.join("node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();

    let callbacks = std::fs::read_to_string(generated.join("common/callbacks.ts")).unwrap();
    for needle in [
        "checkedVoid(fail: boolean): void | Promise<void>;",
        "checkedValue(fail: boolean): number | Promise<number>;",
        "checkedRecord(fail: boolean): Payload | Promise<Payload>;",
        "Promise.resolve(__uniffiCallbackObject.checkedValue",
    ] {
        assert!(
            callbacks.contains(needle),
            "common/callbacks.ts should expose async fallible callbacks via `{needle}`:\n{callbacks}"
        );
    }
    let api = std::fs::read_to_string(generated.join("common/api.ts")).unwrap();
    for needle in [
        "fallibleMethods: {",
        "\"checkedVoid\": \"flat\"",
        "\"checkedValue\": \"flat\"",
        "\"checkedRecord\": \"flat\"",
        "asyncMethods: {",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts should mark async fallible callback methods with `{needle}`:\n{api}"
        );
    }
    let bridge_path = std::fs::read_dir(generated.join("node"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .expect("generated node bridge should contain a Rust shim");
    let bridge = std::fs::read_to_string(bridge_path).unwrap();
    assert!(
        bridge.contains("__UniffiCheckedWorkerCheckedVoidCallbackResult")
            && bridge.contains("napi::bindgen_prelude::Promise")
            && bridge.contains(".call_async(Ok"),
        "napi bridge should implement fallible async callback methods through TSFN Promise:\n{bridge}"
    );
    let preload = std::fs::read_to_string(generated.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("fallibleMethods")
            && preload.contains("asyncMethods")
            && preload.contains("const liftedArgs = callArgs.map(__uniffiLiftShape);")
            && preload.contains("Promise.resolve(v(...liftedArgs)).then("),
        "electron preload should preserve async fallible callback marker behavior:\n{preload}"
    );

    let driver = generated.join("fallible-async-callback-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { createRequire } from "node:module";
import {
  ProviderError,
  invokeCheckedRecord,
  invokeCheckedValue,
  invokeCheckedVoid,
} from "./node/index.ts";
import { UniffiError } from "./common/runtime.ts";

globalThis.window = globalThis;
const require = createRequire(import.meta.url);

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

function delay(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 1));
}

function makeProvider(label: string) {
  const calls: string[] = [];
  return {
    calls,
    provider: {
      async checkedVoid(fail: boolean): Promise<void> {
        await delay();
        calls.push(`${label}:void:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
      },
      async checkedValue(fail: boolean): Promise<number> {
        await delay();
        calls.push(`${label}:value:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return 77;
      },
      async checkedRecord(fail: boolean): Promise<{ left: number; right: number }> {
        await delay();
        calls.push(`${label}:record:${fail}`);
        if (fail) throw new ProviderError("BadValue", "BadValue");
        return { left: 7, right: 11 };
      },
    },
  };
}

async function expectTypedError(label: string, fn: () => Promise<unknown>): Promise<void> {
  let threw = false;
  try {
    await fn();
  } catch (e) {
    threw = true;
    if (!(e instanceof UniffiError)) {
      throw new Error(`${label} threw wrong type: ${e && (e as Error).message}`);
    }
    if (!String((e as Error).message).includes("BadValue")) {
      throw new Error(`${label} threw wrong message: ${(e as Error).message}`);
    }
  }
  if (!threw) throw new Error(`${label}(true) should throw`);
}

const nodeCase = makeProvider("node");
assert(await invokeCheckedVoid(nodeCase.provider as any, false) === true, "node checkedVoid(false)");
assert(await invokeCheckedValue(nodeCase.provider as any, false) === 77, "node checkedValue(false)");
const nodeRecord = await invokeCheckedRecord(nodeCase.provider as any, false);
assert(nodeRecord.left === 7 && nodeRecord.right === 11, `node checkedRecord(false)=${JSON.stringify(nodeRecord)}`);
await expectTypedError("node checkedVoid", () => invokeCheckedVoid(nodeCase.provider as any, true));
await expectTypedError("node checkedValue", () => invokeCheckedValue(nodeCase.provider as any, true));
await expectTypedError("node checkedRecord", () => invokeCheckedRecord(nodeCase.provider as any, true));
assert(nodeCase.calls.join(",") === "node:void:false,node:value:false,node:record:false,node:void:true,node:value:true,node:record:true", `node calls=${nodeCase.calls.join(",")}`);

require("./electron/preload.cjs");
const electronApi = await import("./electron/renderer.ts");
const electronCase = makeProvider("electron");
assert(await electronApi.invokeCheckedVoid(electronCase.provider as any, false) === true, "electron checkedVoid(false)");
assert(await electronApi.invokeCheckedValue(electronCase.provider as any, false) === 77, "electron checkedValue(false)");
const electronRecord = await electronApi.invokeCheckedRecord(electronCase.provider as any, false);
assert(electronRecord.left === 7 && electronRecord.right === 11, `electron checkedRecord(false)=${JSON.stringify(electronRecord)}`);
await expectTypedError("electron checkedVoid", () => electronApi.invokeCheckedVoid(electronCase.provider as any, true));
await expectTypedError("electron checkedValue", () => electronApi.invokeCheckedValue(electronCase.provider as any, true));
await expectTypedError("electron checkedRecord", () => electronApi.invokeCheckedRecord(electronCase.provider as any, true));
assert(
    electronCase.calls.join(",") === "electron:void:false,electron:value:false,electron:record:false,electron:void:true,electron:value:true,electron:record:true",
    `electron calls=${electronCase.calls.join(",")}`
);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(&generated)
        .output()
        .expect("failed to run fallible async callback driver");
    if !output.status.success() {
        panic!(
            "fallible async callback driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "fallible async callback driver did not print ok:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn custom_types_surface_and_raw_napi_execute() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP custom_types_surface_and_raw_napi_execute: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let public_types = std::fs::read_to_string(generated.join("common/public-types.ts")).unwrap();
    assert!(
        public_types.contains("export type { Email } from \"./custom-types.ts\";"),
        "public-types.ts should re-export custom types:\n{public_types}"
    );
    let custom_types = std::fs::read_to_string(generated.join("common/custom-types.ts")).unwrap();
    for needle in [
        "type { EmailAddress } from \"./email.ts\"",
        "emailAddressFromString",
        "emailAddressToString",
        "export type Email = EmailAddress;",
        "__uniffiLowerCustomEmail",
        "__uniffiLiftCustomEmail",
    ] {
        assert!(
            custom_types.contains(needle),
            "custom-types.ts missing `{needle}`:\n{custom_types}"
        );
    }
    assert!(
        !custom_types.contains("unknown /* custom"),
        "custom-types.ts must not leave custom types as unknown:\n{custom_types}"
    );
    let bridge = std::fs::read_to_string(generated.join("node/custom_js_core.rs")).unwrap();
    assert!(
        bridge.contains("::uniffi::Lift") && bridge.contains("::uniffi::Lower"),
        "napi bridge should use uniffi Lift/Lower for custom types:\n{bridge}"
    );

    let driver = tmp.path().join("raw-custom-addon.cjs");
    std::fs::write(
        &driver,
        format!(
            r#"
const addon = require({addon:?});

function eq(actual, expected, label) {{
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {{
    throw new Error(`${{label}}: ${{JSON.stringify(actual)}} !== ${{JSON.stringify(expected)}}`);
  }}
}}

eq(addon.normalizeEmail("  A@EXAMPLE.COM  "), "a@example.com", "normalizeEmail");
eq(
  addon.normalizeContact({{ primary: " ROOT@EXAMPLE.COM ", aliases: [" Alias@One.Com ", "TWO@EXAMPLE.COM"] }}),
  {{ primary: "root@example.com", aliases: ["alias@one.com", "two@example.com"] }},
  "normalizeContact",
);
eq(
  addon.normalizeMany([" X@Y.COM ", "Z@Q.COM"]),
  ["x@y.com", "z@q.com"],
  "normalizeMany",
);
eq(
  addon.formatEmailWith({{ formatEmail(value) {{ return `${{value.trim().toUpperCase()}}!`; }}, formatContact(value) {{ return value; }} }}, " ada@example.com "),
  "ADA@EXAMPLE.COM!",
  "formatEmailWith",
);
eq(
  addon.formatContactWith({{
    formatEmail(value) {{ return value; }},
    formatContact(value) {{
      return {{
        primary: value.primary.trim().toUpperCase(),
        aliases: value.aliases.map((alias) => alias.trim().toUpperCase()),
      }};
    }},
  }}, {{ primary: " Root@Example.Com ", aliases: [" Alias@One.Com "] }}),
  {{ primary: "root@example.com", aliases: ["alias@one.com"] }},
  "formatContactWith",
);

console.log("ok");
"#,
            addon = addon.as_str()
        ),
    )
    .unwrap();
    let output = Command::new(&node)
        .arg(driver.as_path())
        .output()
        .expect("failed to run raw custom addon driver");
    if !output.status.success() {
        panic!(
            "raw custom addon driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "raw custom addon driver did not print ok"
    );
}

#[test]
fn custom_types_generated_node_adapter_executes() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP custom_types_generated_node_adapter_executes: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let _addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);

    let driver = tmp.path().join("custom-node-driver.ts");
    std::fs::write(
        &driver,
        r#"
import * as api from "./generated/node/index.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const one = api.normalizeEmail({ value: "  A@EXAMPLE.COM  " });
assert(one.value === "a@example.com", `normalizeEmail=${JSON.stringify(one)}`);

const contact = api.normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
assert(contact.primary.value === "root@example.com", `primary=${contact.primary.value}`);
assert(contact.aliases[0].value === "alias@one.com", `alias0=${contact.aliases[0].value}`);
assert(contact.aliases[1].value === "two@example.com", `alias1=${contact.aliases[1].value}`);

const many = api.normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
assert(many[0].value === "x@y.com", `many0=${many[0].value}`);
assert(many[1].value === "z@q.com", `many1=${many[1].value}`);

const formatter = {
  formatEmail(value: { value: string }) {
    return { value: `${value.value.trim().toUpperCase()}!` };
  },
  formatContact(value: { primary: { value: string }; aliases: Array<{ value: string }> }) {
    return {
      primary: { value: value.primary.value.trim().toUpperCase() },
      aliases: value.aliases.map((alias) => ({ value: alias.value.trim().toUpperCase() })),
    };
  },
};
const formatted = api.formatEmailWith(formatter, { value: " ada@example.com " });
assert(formatted.value === "ADA@EXAMPLE.COM!", `formatEmailWith=${JSON.stringify(formatted)}`);
const formattedContact = api.formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
assert(formattedContact.primary.value === "root@example.com", `formattedContact.primary=${formattedContact.primary.value}`);
assert(formattedContact.aliases[0].value === "alias@one.com", `formattedContact.aliases=${JSON.stringify(formattedContact.aliases)}`);

console.log("ok");
"#,
    )
    .unwrap();
    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(tmp.path())
        .output()
        .expect("failed to run custom node adapter driver");
    if !output.status.success() {
        panic!(
            "custom node adapter driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "custom node adapter driver did not print ok"
    );
}

#[test]
fn custom_types_generated_electron_renderer_executes() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP custom_types_generated_electron_renderer_executes: node 22.6+ unavailable");
        return;
    };

    let tmp = tempfile::tempdir().unwrap();
    let (generated, manifest) = generate_custom_napi_tree(tmp.path());
    let addon = build_custom_napi_addon(tmp.path(), &generated, &manifest);
    std::fs::copy(&addon, generated.join("electron/custom_js_core.node")).unwrap();
    let electron_stub = generated.join("electron/node_modules/electron");
    std::fs::create_dir_all(&electron_stub).unwrap();
    std::fs::write(
        electron_stub.join("index.js"),
        r#"exports.contextBridge = {
  exposeInMainWorld(name, value) {
    globalThis[name] = value;
  },
};
"#,
    )
    .unwrap();

    let driver = tmp.path().join("custom-electron-driver.ts");
    std::fs::write(
        &driver,
        r#"
import { createRequire } from "node:module";

globalThis.window = globalThis;

const require = createRequire(import.meta.url);
require("./generated/electron/preload.cjs");
const api = await import("./generated/electron/renderer.ts");

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

const one = api.normalizeEmail({ value: "  A@EXAMPLE.COM  " });
assert(one.value === "a@example.com", `normalizeEmail=${JSON.stringify(one)}`);

const contact = api.normalizeContact({
  primary: { value: " ROOT@EXAMPLE.COM " },
  aliases: [{ value: " Alias@One.Com " }, { value: "TWO@EXAMPLE.COM" }],
});
assert(contact.primary.value === "root@example.com", `primary=${contact.primary.value}`);
assert(contact.aliases[0].value === "alias@one.com", `alias0=${contact.aliases[0].value}`);
assert(contact.aliases[1].value === "two@example.com", `alias1=${contact.aliases[1].value}`);

const many = api.normalizeMany([{ value: " X@Y.COM " }, { value: "Z@Q.COM" }]);
assert(many[0].value === "x@y.com", `many0=${many[0].value}`);
assert(many[1].value === "z@q.com", `many1=${many[1].value}`);

const formatter = {
  formatEmail(value: { value: string }) {
    return { value: `${value.value.trim().toUpperCase()}!` };
  },
  formatContact(value: { primary: { value: string }; aliases: Array<{ value: string }> }) {
    return {
      primary: { value: value.primary.value.trim().toUpperCase() },
      aliases: value.aliases.map((alias) => ({ value: alias.value.trim().toUpperCase() })),
    };
  },
};
const formatted = api.formatEmailWith(formatter, { value: " ada@example.com " });
assert(formatted.value === "ADA@EXAMPLE.COM!", `formatEmailWith=${JSON.stringify(formatted)}`);
const formattedContact = api.formatContactWith(formatter, {
  primary: { value: " Root@Example.Com " },
  aliases: [{ value: " Alias@One.Com " }],
});
assert(formattedContact.primary.value === "root@example.com", `formattedContact.primary=${formattedContact.primary.value}`);
assert(formattedContact.aliases[0].value === "alias@one.com", `formattedContact.aliases=${JSON.stringify(formattedContact.aliases)}`);

console.log("ok");
"#,
    )
    .unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(driver.as_path())
        .current_dir(tmp.path())
        .output()
        .expect("failed to run custom electron driver");
    if !output.status.success() {
        panic!(
            "custom electron driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "custom electron driver did not print ok"
    );
}

// ---------------------------------------------------------------------
// Runtime numeric regression — toI64 / toU64 must be lossless-or-reject
// and large integers must round-trip without silent narrowing.
// ---------------------------------------------------------------------

#[test]
fn runtime_numeric_lossless_or_reject() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("SKIP runtime_numeric_lossless_or_reject: node 22.6+ unavailable");
        return;
    };

    // Generate the arithmetic fixture (which pulls in runtime.ts via
    // include_str!) so we can import its toI64/toU64 directly.
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    let driver = r#"
import { toI64, toU64, UniffiError } from "./common/runtime.ts";

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

// 5. Bigint → bigint (pass-through)
const r5 = expectBigint("bigint", () => toI64(18446744073709551615n));
if (r5 !== 18446744073709551615n) throw new Error(`bigint: got ${r5}`);

// 6. toU64 rejects negative
expectThrow("u64 neg bigint", () => toU64(-1n));
expectThrow("u64 neg number", () => toU64(-1));

// 7. Large u64 beyond i64::MAX round-trips
const big = toU64(18446744073709551615n);
if (big !== 18446744073709551615n) throw new Error(`large u64: got ${big}`);

console.log("ok");
"#;
    std::fs::write(out_dir.join("numeric_driver.ts"), driver).unwrap();

    let output = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("numeric_driver.ts")
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
