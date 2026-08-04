//! Fast JavaScript binding smoke checks.
//!
//! This target intentionally avoids nested Cargo, wasm32, native addon, and
//! release-profile work. Broader contract and runtime coverage lives in the
//! layered integration targets next to this file.

mod support;

use support::*;

fn locate_node_with_strip_types() -> Option<std::path::PathBuf> {
    let node = which_node()?;
    let output = Command::new(&node).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    (major > 22 || (major == 22 && minor >= 6)).then_some(node)
}

fn which_node() -> Option<std::path::PathBuf> {
    let output = Command::new("which").arg("node").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!path.is_empty()).then_some(path.into())
}
#[test]
fn emits_real_tree_for_all_flavors() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    for name in [
        "shared/runtime.ts",
        "browser/index.ts",
        "components/arithmetic/common/api.ts",
        "components/arithmetic/common/records.ts",
        "components/arithmetic/common/enums.ts",
        "components/arithmetic/common/errors.ts",
        "components/arithmetic/common/objects.ts",
        "components/arithmetic/common/callbacks.ts",
        "components/arithmetic/common/runtime.ts",
        "components/arithmetic/browser/index.ts",
        "components/arithmetic/browser/backend-wasm.ts",
        "node/index.ts",
        "components/arithmetic/node/index.ts",
        "components/arithmetic/node/backend-napi.ts",
        "electron/index.ts",
        "electron/preload.cjs",
        "components/arithmetic/electron/index.ts",
        "components/arithmetic/electron/backend-napi.ts",
        "components/arithmetic/electron/preload.cjs",
        "components/arithmetic/electron/renderer.ts",
        "harmony/index.ts",
        "components/arithmetic/harmony/index.ts",
        "components/arithmetic/harmony/arithmetical.ohos-facade.json",
    ] {
        let p = out_dir.join(name);
        assert!(p.exists(), "expected output file missing: {p}");
    }

    let harmony_contract: serde_json::Value = serde_json::from_slice(
        &std::fs::read(out_dir.join("components/arithmetic/harmony/arithmetical.ohos-facade.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(harmony_contract["component"], "arithmetical");
    assert_eq!(harmony_contract["namespace"], "arithmetic");
    assert_eq!(harmony_contract["nativeExportPrefix"], "ffi_arithmetical");
    assert!(harmony_contract.get("schemaVersion").is_none());
    assert!(harmony_contract["outputStreams"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(harmony_contract["inputStreams"]
        .as_array()
        .unwrap()
        .is_empty());

    let api = std::fs::read_to_string(out_dir.join("components/arithmetic/common/api.ts")).unwrap();
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
    let pt_path = out_dir.join("components/arithmetic/common/public-types.ts");
    assert!(pt_path.exists(), "expected component public-types.ts");
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
    let napi_backend =
        std::fs::read_to_string(out_dir.join("components/arithmetic/node/backend-napi.ts"))
            .unwrap();
    assert!(
        !napi_backend.contains("__uniffiInt64ArgKinds")
            && !napi_backend.contains("__uniffiInt64ReturnKinds")
            && !napi_backend.contains("__uniffiLowerInt64ForNapi")
            && !napi_backend.contains("__uniffiLiftInt64FromNapi"),
        "node/backend-napi.ts must not carry the old int64 compat layer"
    );
    let preload =
        std::fs::read_to_string(out_dir.join("components/arithmetic/electron/preload.cjs"))
            .unwrap();
    assert!(
        !preload.contains("__uniffiInt64ArgKinds")
            && !preload.contains("__uniffiInt64ReturnKinds")
            && !preload.contains("__uniffiLowerInt64ForNapi")
            && !preload.contains("__uniffiLiftInt64FromNapi"),
        "electron/preload.cjs must not carry the old int64 compat layer"
    );

    let errors =
        std::fs::read_to_string(out_dir.join("components/arithmetic/common/errors.ts")).unwrap();
    assert!(
        errors.contains("ArithmeticError"),
        "common/errors.ts should contain ArithmeticError subclass"
    );

    let runtime = std::fs::read_to_string(out_dir.join("shared/runtime.ts")).unwrap();
    assert!(runtime.contains("const JS_RUNTIME_ABI_VERSION = 2"));
    assert!(runtime.contains("__uniffiJsRuntimeAbiVersion"));
    assert!(runtime.contains("jsRuntimeAbiVersion"));
    assert!(runtime.contains("class UniffiError"));
    assert!(runtime.contains("class UniffiObjectHandle"));
    assert!(runtime.contains("function toU64"));

    // Platform roots are namespace-only; each component entry auto-installs
    // its own backend.
    let node_root = std::fs::read_to_string(out_dir.join("node/index.ts")).unwrap();
    assert!(
        node_root.contains("arithmetic") && !node_root.contains("__installBackend(backend)"),
        "node root must expose only the arithmetic namespace, got:\n{node_root}"
    );
    let node_index =
        std::fs::read_to_string(out_dir.join("components/arithmetic/node/index.ts")).unwrap();
    assert!(
        node_index.contains("__installBackend(backend)"),
        "component node entry must auto-install backend, got:\n{node_index}"
    );
    let browser_root = std::fs::read_to_string(out_dir.join("browser/index.ts")).unwrap();
    assert!(
        browser_root.contains("arithmetic") && !browser_root.contains("__installBackend(backend)"),
        "browser root must expose only the arithmetic namespace, got:\n{browser_root}"
    );
    let browser_index =
        std::fs::read_to_string(out_dir.join("components/arithmetic/browser/index.ts")).unwrap();
    assert!(
        browser_index.contains("__installBackend(backend)"),
        "component browser entry must auto-install backend, got:\n{browser_index}"
    );

    // Locate the per-crate napi bridge file.
    let rust_path = std::fs::read_dir(out_dir.join("components/arithmetic/node"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .expect("a .rs bridge file should exist under the arithmetic node component");
    let rust_bridge = std::fs::read_to_string(&rust_path).unwrap();
    assert!(
        rust_bridge.contains("#[napi"),
        "node/*.rs should be real napi-rs bridge"
    );
    assert!(
        rust_bridge.contains("pub fn add") || rust_bridge.contains("fn add"),
        "node/*.rs should expose `add`"
    );

    // The wasm Rust shim must exist, must use #[wasm_bindgen], and must
    // actually wrap at least one of the arithmetic free functions.
    let wasm_rs_path = std::fs::read_dir(out_dir.join("components/arithmetic/browser"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .expect("a .rs wasm shim should exist under the arithmetic browser component");
    let wasm_rs = std::fs::read_to_string(&wasm_rs_path).unwrap();
    assert!(
        wasm_rs.contains("#[wasm_bindgen"),
        "browser/*.rs should be a wasm-bindgen shim, got:\n{wasm_rs}"
    );
    assert!(
        wasm_rs.contains("pub fn add") || wasm_rs.contains("pub async fn add"),
        "browser/*.rs should wrap `add`"
    );

    let backend_wasm =
        std::fs::read_to_string(out_dir.join("components/arithmetic/browser/backend-wasm.ts"))
            .unwrap();
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

    let root_preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    assert!(
        root_preload.contains("contextBridge.exposeInMainWorld(\"__uniffi__\"")
            && root_preload.contains("components")
            && root_preload.contains("../components/arithmetic/electron/preload.cjs"),
        "root Electron preload must publish the namespaced component bridge once:\n{root_preload}"
    );

    let electron_root = std::fs::read_to_string(out_dir.join("electron/index.ts")).unwrap();
    assert!(
        electron_root.contains("export * as arithmetic")
            && electron_root.contains("main: () => import")
            && electron_root.contains("preload: new URL(\"./preload.cjs\", import.meta.url)"),
        "Electron root must expose the renderer API and a namespace-keyed root preload entrypoint:\n{electron_root}"
    );

    let preload =
        std::fs::read_to_string(out_dir.join("components/arithmetic/electron/preload.cjs"))
            .unwrap();
    assert!(
        preload.contains(".node\"")
            && preload.contains("module.exports = Object.freeze")
            && !preload.contains("contextBridge.exposeInMainWorld"),
        "component Electron preload must be a bridge module, not publish a global:\n{preload}"
    );
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

    let renderer =
        std::fs::read_to_string(out_dir.join("components/arithmetic/electron/renderer.ts"))
            .unwrap();
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
    let backend_napi =
        std::fs::read_to_string(out_dir.join("components/arithmetic/node/backend-napi.ts"))
            .unwrap();
    assert!(
        backend_napi.contains("__uniffiCallback"),
        "napi backend adapter must unwrap callback-trait markers \
         before calling the native addon"
    );
}

// Execution-level sanity check (see module doc). Requires Node 22.6+
// for `--experimental-strip-types` support.
#[test]
fn runs_common_api_under_node() {
    let node = locate_node_with_strip_types()
        .expect("node 22.6+ with --experimental-strip-types is required");

    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);

    // Harness: install a pure-JS stub backend, call through the arithmetic
    // component API.
    let harness = r#"
import {
    __installBackend,
    UniffiError,
} from "./components/arithmetic/common/runtime.ts";
import { add, sub, div, equal } from "./components/arithmetic/common/api.ts";
import { ArithmeticError } from "./components/arithmetic/common/errors.ts";

__installBackend({
    __uniffiJsRuntimeAbiVersion: 2,
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
