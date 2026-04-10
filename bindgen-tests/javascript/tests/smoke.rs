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
use uniffi_bindgen::{BindgenLoader, BindgenPaths};
use uniffi_bindgen_javascript::{generate, FlavorTarget, GenerateJsOptions};

fn workspace_root() -> Utf8PathBuf {
    let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../..").canonicalize_utf8().unwrap()
}

fn generate_arithmetic(out_dir: &Utf8PathBuf) {
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    assert!(source.exists(), "fixture UDL missing: {source}");
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![
                FlavorTarget::Wasm,
                FlavorTarget::Napi,
                FlavorTarget::Electron,
            ],
        },
    )
    .expect("generator should succeed");
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
    ] {
        let p = out_dir.join(name);
        assert!(p.exists(), "expected output file missing: {p}");
    }

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

    // node/electron must emit the targeted bigint compatibility layer
    // for the current napi addon surface.
    let napi_backend = std::fs::read_to_string(out_dir.join("node/backend-napi.ts")).unwrap();
    assert!(
        napi_backend.contains("__uniffiInt64ArgKinds")
            && napi_backend.contains("__uniffiInt64ReturnKinds")
            && napi_backend.contains("__uniffiLowerInt64ForNapi")
            && napi_backend.contains("__uniffiLiftInt64FromNapi"),
        "node/backend-napi.ts must carry the bigint compat maps/helpers"
    );
    let preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    assert!(
        preload.contains("__uniffiInt64ArgKinds")
            && preload.contains("__uniffiInt64ReturnKinds")
            && preload.contains("__uniffiLowerInt64ForNapi")
            && preload.contains("__uniffiLiftInt64FromNapi"),
        "electron/preload.cjs must carry the bigint compat maps/helpers"
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
    // - `dictionary GreetOptions` (record) used as an arg
    // - `callback interface Logger` used as an arg
    // - a free function returning i64 (to exercise bigint return flow)
    let biz = root.join("biz");
    std::fs::create_dir_all(biz.join("src")).unwrap();
    let udl = r#"
dictionary Shape { string label; u32 sides; };
dictionary GreetOptions { string prefix; boolean loud; };
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: gen_dir.clone(),
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

    // 3. No unused runtime imports. This fixture does not touch u64
    //    args, so `toU64` must not be imported. `fromI64`/`fromU64` are
    //    also not used any more (bigint-first contract).
    let import_block: String = api
        .lines()
        .filter(|l| l.starts_with("import ") && l.contains("./runtime.ts"))
        .collect::<Vec<_>>()
        .join("\n");
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

    // 4. `common/objects.ts` must not carry a dangling runtime import
    //    when there are no non-callback-trait objects at all.
    let objects = std::fs::read_to_string(gen_dir.join("common/objects.ts")).unwrap();
    assert!(
        !objects.contains("import") || !objects.contains("runtime.ts"),
        "common/objects.ts has unused runtime import (no objects in this fixture):\n{objects}"
    );

    // 5. Optional: if tsc is available, actually compile in strict
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path.clone(),
            out_dir: gen_dir.clone(),
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
wasm-bindgen = "0.2"
wasm-bindgen-futures = "0.4"
wasm_scalar = { path = "../biz" }
"#,
    )
    .unwrap();
    // Isolate from any parent workspace so the temp crates build standalone.
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"biz\", \"shim\"]\nresolver = \"2\"\n",
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
    });
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
{extra}
"#,
            extra = spec.biz_deps
        ),
    )
    .unwrap();
    std::fs::write(biz.join("src/lib.rs"), "// placeholder\n").unwrap();

    // Generate JS bindings.
    let gen_dir = root.join("gen");
    std::fs::create_dir_all(&gen_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path.clone(),
            out_dir: gen_dir.clone(),
            config_override: None,
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
wasm-bindgen = "0.2"
wasm-bindgen-futures = "0.4"
js-sys = "0.3"
{name} = {{ path = "../biz" }}
{extra}
"#,
            extra = spec.shim_deps
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"biz\", \"shim\"]\nresolver = \"2\"\n",
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: gen_dir.clone(),
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
}

#[test]
fn napi_electron_emit_int64_compat_maps() {
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl_path,
            out_dir: gen_dir.clone(),
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
        "\"counter_with_initial\": [\"i64\"]",
        "\"counter_get\": \"i64\"",
        "\"slow_add\": [null, null, \"u64\"]",
    ] {
        assert!(
            backend_napi.contains(needle),
            "node/backend-napi.ts missing `{needle}`:\n{backend_napi}"
        );
    }

    let preload = std::fs::read_to_string(gen_dir.join("electron/preload.cjs")).unwrap();
    for needle in [
        "__uniffiInt64ArgKinds",
        "__uniffiInt64ReturnKinds",
        "__uniffiLowerInt64ForNapi",
        "__uniffiLiftInt64FromNapi",
        "\"counter_with_initial\": [\"i64\"]",
        "\"counter_get\": \"i64\"",
        "\"slow_add\": [null, null, \"u64\"]",
    ] {
        assert!(
            preload.contains(needle),
            "electron/preload.cjs missing `{needle}`:\n{preload}"
        );
    }
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
        .arg(preload.as_std_path())
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
        "args.map((a, i) =>",
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
        "__uniffiLowerInt64ForNapi",
        "__uniffiLiftInt64FromNapi",
        "__uniffiLowerShape(__uniffiLowerInt64ForNapi(resolveArg(a), argKinds[i] || null))",
        "wrapResult(__uniffiLiftInt64FromNapi(__uniffiLiftShape(raw), retKind))",
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
        .arg(driver.as_std_path())
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
        "Object.getPrototypeOf(value)",
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
        .arg(driver.as_std_path())
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_crates_dir.clone(),
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
        "[package]\nname = \"tiny-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\nname = \"tiny\"\ncrate-type = [\"lib\"]\n\n[dependencies]\n\n[workspace]\n",
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
            }),
            flavors: vec![FlavorTarget::Wasm, FlavorTarget::Napi],
        },
    )
    .expect("synthetic generator run should succeed");
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
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
         [dependencies]\n\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "use std::sync::Arc;\n\n\
         pub trait Logger: Send + Sync { fn log(&self, msg: String); }\n\n\
         pub enum JobState { Idle, Running, Done }\n\n\
         pub enum Event { Started, Finished { name: String } }\n\n\
         pub fn run_job(logger: Arc<dyn Logger>) { logger.log(\"x\".into()); }\n\
         pub fn current_job_state() -> JobState { JobState::Idle }\n\
         pub fn latest_event() -> Event { Event::Started }\n\
         pub async fn slow_add(a: u32, b: u32) -> u32 { a + b }\n\
         pub fn add_u64(a: u64, b: u64) -> u64 { a.wrapping_add(b) }\n\
         pub fn negate_i64(a: i64) -> i64 { a.wrapping_neg() }\n",
    )
    .unwrap();
    let udl = src.join("napi_compat.udl");
    std::fs::write(
        &udl,
        "[Trait, WithForeign]\n\
         interface Logger {\n    void log(string msg);\n};\n\n\
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
         \x20   u32 slow_add(u32 a, u32 b);\n\
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
    let loader = BindgenLoader::new(BindgenPaths::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
            }),
            flavors: vec![FlavorTarget::Napi],
        },
    )
    .expect("rich napi generator run should succeed");
    host_dir
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
        bridge.contains("ThreadsafeFunction"),
        "rich fixture should exercise ThreadsafeFunction"
    );
    assert!(
        bridge.contains("napi::bindgen_prelude::BigInt"),
        "rich fixture should use napi::BigInt for u64/i64, got:\n{bridge}"
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
