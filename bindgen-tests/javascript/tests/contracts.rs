//! Generated JavaScript static and stub-runtime contracts.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::*;

#[test]
fn engine_neutral_contract_corpus_is_deterministic() {
    use uniffi_js_abi::{
        InputPresence, PresenceError, PublicSourceFamily, PublicTarget, ScalarType, ValueType,
    };
    use uniffi_js_engine_schema::Capability;

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

fn contains_dynamic_type_word(source: &str) -> bool {
    source
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|word| word == "any" || word == "unknown")
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
    let api =
        std::fs::read_to_string(out_dir.join("components/async_callback_return/common/api.ts"))
            .unwrap();
    assert!(
        api.contains("callbackReturnMethods: { \"makeLogger\": true }"),
        "callback-return metadata should be emitted:\n{api}"
    );
    let napi_rs = std::fs::read_to_string(
        out_dir.join("components/async_callback_return/node/async_callback_return.rs"),
    )
    .unwrap();
    assert!(
        napi_rs.contains("UniffiCallbackHandle")
            && napi_rs.contains("__uniffi_from_callback_registry"),
        "napi bridge should emit callback-return registry support:\n{napi_rs}"
    );
    let backend = std::fs::read_to_string(
        out_dir.join("components/async_callback_return/node/backend-napi.ts"),
    )
    .unwrap();
    assert!(
        backend.contains("__uniffiStoreCallbackReturn")
            && backend.contains("__uniffiCallbackDispatcher"),
        "napi backend should store async callback returns:\n{backend}"
    );
}

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

    let api = std::fs::read_to_string(gen_dir.join("components/blockers/common/api.ts")).unwrap();

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

    let records =
        std::fs::read_to_string(gen_dir.join("components/blockers/common/records.ts")).unwrap();
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
    let objects =
        std::fs::read_to_string(gen_dir.join("components/blockers/common/objects.ts")).unwrap();
    assert!(
        !objects.contains("import") || !objects.contains("runtime.ts"),
        "common/objects.ts has unused runtime import (no objects in this fixture):\n{objects}"
    );
    // 6. Empty generated category files still need an explicit ES-module
    // marker. In particular, an error-free component must not import the
    // runtime's UniffiError merely to make an otherwise comment-only module.
    let errors =
        std::fs::read_to_string(gen_dir.join("components/blockers/common/errors.ts")).unwrap();
    let custom_types =
        std::fs::read_to_string(gen_dir.join("components/blockers/common/custom-types.ts"))
            .unwrap();
    for (module, source) in [
        ("custom-types.ts", custom_types.as_str()),
        ("errors.ts", errors.as_str()),
        ("objects.ts", objects.as_str()),
    ] {
        assert!(
            source.lines().last() == Some("export {};"),
            "empty {module} must end with exactly `export {{}};`:\n{source}"
        );
    }
    assert!(
        !errors.contains("UniffiError"),
        "errors.ts must not import UniffiError without generated error classes:\n{errors}"
    );

    // 7. The final matrix injects an explicit TypeScript 5.9.3 compiler.
    //    This is the real guardrail — the string assertions above are only a
    //    faster early signal. Missing or broken compiler configuration fails
    //    this required contract instead of silently skipping it.
    let tsc = required_typescript_compiler();
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
    "allowImportingTsExtensions": true,
    "types": [],
    "lib": ["es2022", "dom"]
  },
  "include": ["components/blockers/common/*.ts"]
}
"#,
    )
    .unwrap();
    let out = run_required_typescript_check(&tsc, tsconfig.as_std_path());
    assert!(
        out.status.success(),
        "required TypeScript 5.9.3 --strict check failed on generated tree:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
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
        gen_dir.join("components/custom_js_core/common/email.ts"),
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

    let api =
        std::fs::read_to_string(gen_dir.join("components/custom_js_core/common/api.ts")).unwrap();
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

    let public_types =
        std::fs::read_to_string(gen_dir.join("components/custom_js_core/common/public-types.ts"))
            .unwrap();
    assert!(
        public_types.contains("export type { Email } from \"./custom-types.ts\";"),
        "public-types.ts should re-export the configured custom type alias:\n{public_types}"
    );

    let custom_types =
        std::fs::read_to_string(gen_dir.join("components/custom_js_core/common/custom-types.ts"))
            .unwrap();
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

    let records =
        std::fs::read_to_string(gen_dir.join("components/custom_js_core/common/records.ts"))
            .unwrap();
    assert!(
        records.contains("import type { Email } from \"./custom-types.ts\";"),
        "records.ts should import configured custom types when used in records:\n{records}"
    );

    let wasm_rs_path = std::fs::read_dir(gen_dir.join("components/custom_js_core/browser"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .expect("a .rs wasm shim should exist under the custom_js_core browser component");
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

// Parameters for a full Path-A wasm e2e fixture. Shared by the scalar
// regression test and the non-scalar tests added in the records/enums/
// objects pass.
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
        ("greet_with", "ffi_mapping_greet_with"),
        ("counter_new", "ffi_mapping_counter_new"),
        ("counter_with_initial", "ffi_mapping_counter_with_initial"),
        ("counter_get", "ffi_mapping_counter_get"),
        ("run_job", "ffi_mapping_run_job"),
    ];

    let backend_napi =
        std::fs::read_to_string(gen_dir.join("components/mapping/node/backend-napi.ts")).unwrap();
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

    let preload =
        std::fs::read_to_string(gen_dir.join("components/mapping/electron/preload.cjs")).unwrap();
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

    let renderer =
        std::fs::read_to_string(gen_dir.join("components/mapping/electron/renderer.ts")).unwrap();
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

    let backend_napi =
        std::fs::read_to_string(gen_dir.join("components/compat/node/backend-napi.ts")).unwrap();
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

    let preload =
        std::fs::read_to_string(gen_dir.join("components/compat/electron/preload.cjs")).unwrap();
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
    let root = out_dir.clone();
    // Keep the runtime state-machine contract independent of generated API
    // strings.  It deliberately drives the final factory directly so every
    // state transition, including ABI refusal and finalizer ownership, is
    // observable without a native addon.
    std::fs::write(
        root.join("driver.ts"),
        r#"
class FakeFinalizationRegistry {
  static readonly registries: FakeFinalizationRegistry[] = [];
  private readonly callback: (heldValue: unknown) => void;
  private registrations: Array<{ target: object; heldValue: unknown; token?: object }> = [];
  constructor(callback: (heldValue: unknown) => void) {
    this.callback = callback;
    FakeFinalizationRegistry.registries.push(this);
  }
  register(target: object, heldValue: unknown, token?: object): void {
    this.registrations.push({ target, heldValue, token });
  }
  unregister(token: object): boolean {
    const before = this.registrations.length;
    this.registrations = this.registrations.filter((entry) => entry.token !== token);
    return this.registrations.length !== before;
  }
  static trigger(target: object): void {
    for (const registry of FakeFinalizationRegistry.registries) {
      const entries = registry.registrations.filter((entry) => entry.target === target);
      registry.registrations = registry.registrations.filter((entry) => entry.target !== target);
      for (const entry of entries) registry.callback(entry.heldValue);
    }
  }
}
(globalThis as { FinalizationRegistry?: unknown }).FinalizationRegistry = FakeFinalizationRegistry;

const { __installBackend, createUniFfiStream, UniffiError } =
  await import("./components/arithmetic/common/runtime.ts");

function assert(condition: boolean, label: string): void {
  if (!condition) throw new Error("FAIL " + label);
}
async function flush(): Promise<void> {
  await Promise.resolve();
  await new Promise<void>((resolve) => setTimeout(resolve, 0));
}
function errorName(error: unknown): string {
  return error instanceof UniffiError ? error.errorName : "";
}

for (const [backend, expectedGot] of [
  [{}, undefined],
  [{ __uniffiJsRuntimeAbiVersion: 1 }, 1],
  [{ __uniffiJsRuntimeAbiVersion: 3 }, 3],
] as const) {
  let rejected = false;
  try {
    __installBackend(backend);
  } catch (error) {
    const mismatch = (error as UniffiError).data as {
      jsRuntimeAbiVersion?: { expected?: number; got?: unknown };
    } | null;
    rejected = errorName(error) === "UniffiAbiMismatch"
      && mismatch?.jsRuntimeAbiVersion?.expected === 2
      && mismatch?.jsRuntimeAbiVersion?.got === expectedGot
      && (error as UniffiError).message.includes("jsRuntimeAbiVersion");
  }
  assert(rejected, "backend ABI mismatch is rejected before installation");
}
__installBackend({ __uniffiJsRuntimeAbiVersion: 2 });

let idleStarts = 0;
let idleCancels = 0;
const idle = createUniFfiStream({
  start: () => { idleStarts += 1; return "idle"; },
  next: async () => ({ kind: "done" as const }),
  cancel: () => { idleCancels += 1; },
});
assert(idleStarts === 0, "construction is lazy");
await idle.cancel();
assert(idleCancels === 0, "idle cancel stays local");
FakeFinalizationRegistry.trigger(idle);
await flush();
assert(idleCancels === 0, "idle stream has no native finalizer");
assert((await idle.next()).done === true && idleStarts === 0, "idle cancel is terminal");

let starts = 0;
let pulls = 0;
let cancels = 0;
const values: Array<number | null> = [1, null, 2];
const direct = createUniFfiStream({
  start: () => { starts += 1; return "direct"; },
  next: async () => {
    pulls += 1;
    return pulls > values.length
      ? { kind: "done" as const }
      : { kind: "item" as const, value: values[pulls - 1] };
  },
  cancel: () => { cancels += 1; },
});
assert(starts === 0, "stream does not start before first direct next");
assert((await direct.next()).value === 1 && starts === 1, "first direct next starts once");
assert((await direct.next()).value === null, "optional null is an item");
assert((await direct.next()).value === 2, "later optional item is retained");
assert((await direct.next()).done === true, "done is explicit");
const pullsAfterDone = pulls;
assert((await direct.next()).done === true && pulls === pullsAfterDone, "done is terminal");
let mixedRejected = false;
try {
  direct[Symbol.asyncIterator]();
} catch (error) {
  mixedRejected = errorName(error) === "UniffiStreamConsumed";
}
assert(mixedRejected, "direct consumer excludes iterator consumer");

const singleUse = createUniFfiStream({
  start: () => "single",
  next: async () => ({ kind: "done" as const }),
  cancel: () => {},
});
const iterator = singleUse[Symbol.asyncIterator]();
let secondIteratorRejected = false;
try {
  singleUse[Symbol.asyncIterator]();
} catch (error) {
  secondIteratorRejected = errorName(error) === "UniffiStreamConsumed";
}
assert(secondIteratorRejected, "second iterator is rejected before start");
assert((await iterator.next()).done === true, "first iterator remains valid");

let pendingResolve: ((step: unknown) => void) | null = null;
const pending = createUniFfiStream({
  start: () => "pending",
  next: () => new Promise((resolve) => { pendingResolve = resolve; }),
  cancel: () => { cancels += 1; },
});
const firstPending = pending.next();
let concurrentRejected = false;
try {
  await pending.next();
} catch (error) {
  concurrentRejected = errorName(error) === "UniffiStreamConcurrentNext";
}
assert(concurrentRejected, "concurrent next is rejected without ending the first pull");
pendingResolve?.({ kind: "item", value: 99 });
assert((await firstPending).value === 99, "first pending pull survives concurrent rejection");
await pending.cancel();
await pending.cancel();

const protocol = createUniFfiStream({
  start: () => "protocol",
  next: async () => ({ done: true } as unknown),
  cancel: () => { cancels += 1; },
});
let protocolRejected = false;
try {
  await protocol.next();
} catch (error) {
  protocolRejected = errorName(error) === "UniffiStreamProtocolError";
}
assert(protocolRejected, "old nullable/done envelope is rejected");
assert((await protocol.next()).done === true, "protocol failure is terminal");

const typed = new UniffiError({
  errorName: "StreamError",
  variant: "Boom",
  data: { code: 7 },
  message: "boom",
});
const failed = createUniFfiStream({
  start: () => "failed",
  next: async () => ({ kind: "error" as const, error: typed }),
  cancel: () => { cancels += 1; },
});
let typedPreserved = false;
try {
  await failed.next();
} catch (error) {
  typedPreserved = error === typed
    && errorName(error) === "StreamError"
    && (error as UniffiError).variant === "Boom"
    && (error as UniffiError).data !== null
    && (error as Error).message === "boom";
}
assert(typedPreserved, "typed stream error retains name variant data and message");
assert((await failed.next()).done === true, "error is terminal");

let finalizerCancels = 0;
const active = createUniFfiStream({
  start: () => "active",
  next: async () => ({ kind: "item" as const, value: 7 }),
  cancel: () => { finalizerCancels += 1; return Promise.reject(new Error("ignored")); },
});
assert((await active.next()).value === 7, "active direct stream yields item");
FakeFinalizationRegistry.trigger(active);
await flush();
assert(finalizerCancels === 1, "active stream finalizer cancels once");
FakeFinalizationRegistry.trigger(active);
await flush();
assert(finalizerCancels === 1, "active stream finalizer remains one-shot");

let iteratorCancels = 0;
const iteratorOwner = createUniFfiStream({
  start: () => "iterator-owner",
  next: async () => ({ kind: "item" as const, value: 8 }),
  cancel: () => { iteratorCancels += 1; },
});
const activeIterator = iteratorOwner[Symbol.asyncIterator]();
await activeIterator.next();
FakeFinalizationRegistry.trigger(iteratorOwner);
await flush();
assert(iteratorCancels === 0, "stream collection cannot cancel a live iterator");
FakeFinalizationRegistry.trigger(activeIterator);
await flush();
assert(iteratorCancels === 1, "active iterator finalizer cancels once");

let returnCancels = 0;
const returning = createUniFfiStream({
  start: () => "return",
  next: async () => ({ kind: "item" as const, value: 1 }),
  cancel: () => { returnCancels += 1; },
});
const returningIterator = returning[Symbol.asyncIterator]();
await returningIterator.return?.();
await returningIterator.return?.();
assert(returnCancels === 0, "iterator return before start stays local");

const throwing = createUniFfiStream({
  start: () => "throw",
  next: async () => ({ kind: "item" as const, value: 1 }),
  cancel: () => { returnCancels += 1; },
});
const throwingIterator = throwing[Symbol.asyncIterator]();
await throwingIterator.next();
const callerError = new Error("caller");
let callerPreserved = false;
try {
  await throwingIterator.throw?.(callerError);
} catch (error) {
  callerPreserved = error === callerError;
}
assert(callerPreserved && returnCancels === 1, "iterator throw preserves caller error after cleanup");

console.log("ok");
"#,
    )
    .unwrap();

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
    let runtime = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("driver.ts")
        .current_dir(&root)
        .output()
        .expect("failed to run runtime state-machine driver");
    assert!(
        runtime.status.success(),
        "runtime state-machine driver failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&runtime.stdout),
        String::from_utf8_lossy(&runtime.stderr),
    );
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

    let backend_napi =
        std::fs::read_to_string(out_dir.join("components/arithmetic/node/backend-napi.ts"))
            .unwrap();
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

    let preload =
        std::fs::read_to_string(out_dir.join("components/arithmetic/electron/preload.cjs"))
            .unwrap();
    for needle in [
        "__uniffiLowerShape",
        "__uniffiLiftShape",
        "__uniffiIsPlainObject",
        "__uniffiLowerShape(resolveArg(a))",
        "wrapResult(__uniffiLiftResult(msg.method, raw))",
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
const src = fs.readFileSync(path.resolve(__dirname, "components/arithmetic/electron/preload.cjs"), "utf8");
// Extract just the helper block without loading the generated addon bridge.
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

#[test]
fn js_async_iterable_runtime_stub_contract() {
    let Some(node) = locate_node_with_strip_types() else {
        eprintln!("skipping: node with --experimental-strip-types not available");
        return;
    };
    let tmp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(
        shared.join("runtime.ts"),
        include_str!("../../../uniffi_runtime_javascript/typescript/src/runtime.ts"),
    )
    .unwrap();
    std::fs::write(
        root.join("driver.ts"),
        r#"
class Finalizers {
  static all: Finalizers[] = [];
  private entries: Array<{ target: object; value: unknown; token?: object }> = [];
  private readonly callback: (value: unknown) => void;
  constructor(callback: (value: unknown) => void) { this.callback = callback; Finalizers.all.push(this); }
  register(target: object, value: unknown, token?: object): void { this.entries.push({ target, value, token }); }
  unregister(token: object): boolean {
    const count = this.entries.length;
    this.entries = this.entries.filter((entry) => entry.token !== token);
    return count !== this.entries.length;
  }
  static trigger(target: object): void {
    for (const registry of Finalizers.all) {
      const selected = registry.entries.filter((entry) => entry.target === target);
      registry.entries = registry.entries.filter((entry) => entry.target !== target);
      selected.forEach((entry) => registry.callback(entry.value));
    }
  }
}
(globalThis as { FinalizationRegistry?: unknown }).FinalizationRegistry = Finalizers;
const { createComponentRuntime, createUniFfiStream, UniffiError } = await import("./shared/runtime.ts");
const { __installBackend } = createComponentRuntime("runtime-stub-contract");
function assert(ok: boolean, label: string): void { if (!ok) throw new Error("FAIL " + label); }
function name(error: unknown): string { return error instanceof UniffiError ? error.errorName : ""; }
async function flush(): Promise<void> { await Promise.resolve(); await new Promise<void>((resolve) => setTimeout(resolve, 0)); }

for (const [backend, expectedGot] of [
  [{}, undefined],
  [{ __uniffiJsRuntimeAbiVersion: 1 }, 1],
  [{ __uniffiJsRuntimeAbiVersion: 3 }, 3],
] as const) {
  let rejected = false;
  try {
    __installBackend(backend);
  } catch (error) {
    const mismatch = (error as UniffiError).data as {
      jsRuntimeAbiVersion?: { expected?: number; got?: unknown };
    } | null;
    rejected = name(error) === "UniffiAbiMismatch"
      && mismatch?.jsRuntimeAbiVersion?.expected === 2
      && mismatch?.jsRuntimeAbiVersion?.got === expectedGot
      && (error as UniffiError).message.includes("jsRuntimeAbiVersion");
  }
  assert(rejected, "ABI mismatch rejects before install");
}
__installBackend({ __uniffiJsRuntimeAbiVersion: 2 });

let starts = 0;
let pulls = 0;
let cancels = 0;
const values: Array<number | null> = [1, null, 2];
const stream = createUniFfiStream({
  start: () => { starts += 1; return "stream"; },
  next: async () => {
    pulls += 1;
    return pulls > values.length
      ? { kind: "done" as const }
      : { kind: "item" as const, value: values[pulls - 1] };
  },
  cancel: () => { cancels += 1; },
});
assert(starts === 0, "construction is lazy");
assert((await stream.next()).value === 1 && starts === 1, "first pull starts exactly once");
assert((await stream.next()).value === null && (await stream.next()).value === 2, "optional null is an item");
assert((await stream.next()).done === true, "Done is explicit");
const terminalPulls = pulls;
assert((await stream.next()).done === true && pulls === terminalPulls, "done is terminal");
let mixed = false;
try { stream[Symbol.asyncIterator](); } catch (error) { mixed = name(error) === "UniffiStreamConsumed"; }
assert(mixed, "direct and iterator consumers cannot mix");

let idleStarts = 0;
let idleCancels = 0;
const idle = createUniFfiStream({
  start: () => { idleStarts += 1; return "idle"; },
  next: async () => ({ kind: "done" as const }),
  cancel: () => { idleCancels += 1; },
});
await idle.cancel();
Finalizers.trigger(idle);
await flush();
assert(idleStarts === 0 && idleCancels === 0 && (await idle.next()).done === true, "idle cancel has no handle or finalizer");

const single = createUniFfiStream({
  start: () => "single",
  next: async () => ({ kind: "done" as const }),
  cancel: () => {},
});
const singleIterator = single[Symbol.asyncIterator]();
let second = false;
try { single[Symbol.asyncIterator](); } catch (error) { second = name(error) === "UniffiStreamConsumed"; }
assert(second && (await singleIterator.next()).done === true, "second iterator fails before start");

let resolvePending: ((value: unknown) => void) | null = null;
const pending = createUniFfiStream({
  start: () => "pending",
  next: () => new Promise((resolve) => { resolvePending = resolve; }),
  cancel: () => { cancels += 1; },
});
const first = pending.next();
let concurrent = false;
try { await pending.next(); } catch (error) { concurrent = name(error) === "UniffiStreamConcurrentNext"; }
resolvePending?.({ kind: "item", value: 99 });
assert(concurrent && (await first).value === 99, "concurrent next leaves first pull alive");
await pending.cancel();
await pending.cancel();

const typed = new UniffiError({ errorName: "StreamError", variant: "Boom", data: { id: 7 }, message: "boom" });
const failed = createUniFfiStream({
  start: () => "failed",
  next: async () => ({ kind: "error" as const, error: typed }),
  cancel: () => { cancels += 1; },
});
let typedPreserved = false;
try { await failed.next(); } catch (error) {
  typedPreserved = error === typed && name(error) === "StreamError"
    && (error as UniffiError).variant === "Boom"
    && (error as Error).message === "boom";
}
assert(typedPreserved && (await failed.next()).done === true, "typed Error is preserved and terminal");

const malformed = createUniFfiStream({
  start: () => "malformed",
  next: async () => ({ done: true } as unknown),
  cancel: () => { cancels += 1; },
});
let malformedRejected = false;
try { await malformed.next(); } catch (error) { malformedRejected = name(error) === "UniffiStreamProtocolError"; }
assert(malformedRejected && (await malformed.next()).done === true, "old nullable envelope is rejected");

let finalizerCancels = 0;
const active = createUniFfiStream({
  start: () => "active",
  next: async () => ({ kind: "item" as const, value: 7 }),
  cancel: () => { finalizerCancels += 1; return Promise.reject(new Error("ignored")); },
});
await active.next();
Finalizers.trigger(active);
await flush();
assert(finalizerCancels === 1, "active finalizer is best effort and once only");

let iteratorCancels = 0;
const owner = createUniFfiStream({
  start: () => "owner",
  next: async () => ({ kind: "item" as const, value: 8 }),
  cancel: () => { iteratorCancels += 1; },
});
const ownerIterator = owner[Symbol.asyncIterator]();
await ownerIterator.next();
Finalizers.trigger(owner);
await flush();
assert(iteratorCancels === 0, "stream finalizer does not own active iterator");
Finalizers.trigger(ownerIterator);
await flush();
assert(iteratorCancels === 1, "iterator finalizer owns active handle");

let returnCancels = 0;
const throwing = createUniFfiStream({
  start: () => "throwing",
  next: async () => ({ kind: "item" as const, value: 1 }),
  cancel: () => { returnCancels += 1; },
});
const throwingIterator = throwing[Symbol.asyncIterator]();
await throwingIterator.next();
const caller = new Error("caller");
let callerPreserved = false;
try { await throwingIterator.throw?.(caller); } catch (error) { callerPreserved = error === caller; }
assert(callerPreserved && returnCancels === 1, "iterator throw keeps caller error after cleanup");
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

    let api =
        std::fs::read_to_string(out_dir.join("components/stream_core/common/api.ts")).unwrap();
    for needle in [
        "createUniFfiStream<StreamEvent, unknown>",
        "start: () => __call<any>(\"count_events\"",
        "__callAsync<any>(\"count_events_stream_next\"",
        "switch (__next.kind)",
        "return { kind: \"item\", value:",
        "return { kind: \"done\" };",
        "return { kind: \"error\", error:",
        "__call<void>(\"count_events_stream_cancel\"",
        "export type { UniFfiStream } from \"./runtime.ts\";",
        "import type { UniFfiStream } from \"./runtime.ts\";",
        "new StreamError(",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts should expose stream async iterable contract via `{needle}`:\n{api}"
        );
    }
    assert!(
        api.contains("export function optionalEvents(): UniFfiStream<number | null>"),
        "common/api.ts should retain an optional stream item type:\n{api}"
    );
    assert!(
        api.contains("new StreamError("),
        "common/api.ts should lift stream Error steps into the typed error class:\n{api}"
    );
    for removed in [
        "createUniffiAsyncIterable",
        "UniffiStreamNext",
        "done?: unknown",
        "if (__next.done)",
        "UNIFFI_JS_CONTRACT_VERSION",
        "__installBackend",
    ] {
        assert!(
            !api.contains(removed),
            "common/api.ts retained removed stream/backend surface {removed}:\n{api}"
        );
    }
    // This fixture contains real output-stream bindings. The local type import
    // is required in addition to the public re-export above, and the injected
    // TypeScript 5.9.3 compiler must resolve the stream return annotations
    // with strict unused-local/parameter checking enabled.
    let tsc = required_typescript_compiler();
    let tsconfig = out_dir.join("tsconfig.json");
    std::fs::write(
        &tsconfig,
        r#"{
  "compilerOptions": {
    "target": "es2022",
    "module": "es2022",
    "moduleResolution": "bundler",
    "allowImportingTsExtensions": true,
    "strict": true,
    "noUnusedLocals": true,
    "noUnusedParameters": true,
    "noEmit": true,
    "types": [],
    "lib": ["es2022", "dom"]
  },
  "include": ["components/stream_core/common/*.ts"]
}
"#,
    )
    .unwrap();
    let output = run_required_typescript_check(&tsc, tsconfig.as_std_path());
    assert!(
        output.status.success(),
        "required TypeScript 5.9.3 --strict check failed on generated output-stream tree:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let backend =
        std::fs::read_to_string(out_dir.join("components/stream_core/node/backend-napi.ts"))
            .unwrap();
    assert!(
        backend
            .contains("\"count_events_stream_next\": \"ffi_stream_core_count_events_stream_next\"")
            && backend.contains(
                "\"count_events_stream_cancel\": \"ffi_stream_core_count_events_stream_cancel\""
            )
            && backend.contains("__uniffiJsRuntimeAbiVersion")
            && backend.contains("__uniffiNormalizeStreamStep"),
        "napi backend name map should include stream next/cancel:\n{backend}"
    );
    let preload =
        std::fs::read_to_string(out_dir.join("components/stream_core/electron/preload.cjs"))
            .unwrap();
    assert!(
        preload.contains("__uniffiNormalizeStreamStep")
            && preload.contains("__uniffiLiftResult")
            && preload.contains("method.endsWith(\"_stream_next\")"),
        "electron preload must normalize N-API stream output to tagged ABI-v2 steps:\n{preload}"
    );
    let renderer =
        std::fs::read_to_string(out_dir.join("components/stream_core/electron/renderer.ts"))
            .unwrap();
    assert!(
        renderer.contains("\"count_events_stream_next\"")
            && !renderer.contains("\"count_events_stream_cancel\""),
        "electron renderer should dispatch next asynchronously and leave cancel sync:\n{renderer}"
    );
    let wasm_rs =
        std::fs::read_to_string(out_dir.join("components/stream_core/browser/stream_core.rs"))
            .unwrap();
    assert!(
        wasm_rs.contains("RustStreamRegistry")
            && wasm_rs.contains("pub async fn count_events_stream_next")
            && wasm_rs.contains("pub fn count_events_stream_cancel")
            && wasm_rs.contains("Reflect::set(&__obj, &JsValue::from_str(\"kind\")")
            && wasm_rs.contains("JsValue::from_str(\"item\")")
            && wasm_rs.contains("JsValue::from_str(\"done\")")
            && wasm_rs.contains("JsValue::from_str(\"error\")"),
        "wasm shim should emit stream start/next/cancel:\n{wasm_rs}"
    );
    let napi_rs =
        std::fs::read_to_string(out_dir.join("components/stream_core/node/stream_core.rs"))
            .unwrap();
    assert!(
        napi_rs.contains("RustStreamRegistry")
            && napi_rs.contains("pub async fn count_events_stream_next")
            && napi_rs.contains("pub fn count_events_stream_cancel")
            && napi_rs.contains("pub kind: ::std::string::String,")
            && napi_rs.contains("kind: ::std::string::String::from(\"item\")")
            && napi_rs.contains("kind: ::std::string::String::from(\"done\")")
            && napi_rs.contains("kind: ::std::string::String::from(\"error\")"),
        "napi shim should emit stream start/next/cancel:\n{napi_rs}"
    );

    std::fs::write(
        out_dir.join("driver.ts"),
        r#"
import { __installBackend, UniffiError } from "./components/stream_core/common/runtime.ts";
import { countEvents, emptyOptionalEvents, errorAfterOne, optionalEvents, singleOptionalEvent } from "./components/stream_core/common/api.ts";

function assert(cond: boolean, label: string): void {
  if (!cond) throw new Error(`FAIL ${label}`);
}

let cancelCount = 0;
let nextId = 1;
let errorNextCalls = 0;
const streams = new Map<string, { values: Array<unknown>; errorAt?: number; nextCalls: number }>();

__installBackend({
  __uniffiJsRuntimeAbiVersion: 2,
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
    if (!stream) return { kind: "done" };
    stream.nextCalls += 1;
    const value = stream.values.shift();
    return value === undefined ? { kind: "done" } : { kind: "item", value };
  },
  count_events_stream_cancel(handle: string) {
    cancelCount += 1;
    streams.delete(handle);
  },
  optional_events() {
    const handle = `o${nextId++}`;
    streams.set(handle, { values: [1, null, 2], nextCalls: 0 });
    return handle;
  },
  async optional_events_stream_next(handle: string) {
    const stream = streams.get(handle);
    if (!stream) return { kind: "done" };
    stream.nextCalls += 1;
    if (stream.values.length === 0) return { kind: "done" };
    return { kind: "item", value: stream.values.shift() };
  },
  optional_events_stream_cancel(handle: string) {
    cancelCount += 1;
    streams.delete(handle);
  },
  empty_optional_events() {
    const handle = `oe${nextId++}`;
    streams.set(handle, { values: [], nextCalls: 0 });
    return handle;
  },
  async empty_optional_events_stream_next(handle: string) {
    const stream = streams.get(handle);
    if (!stream || stream.values.length === 0) return { kind: "done" };
    stream.nextCalls += 1;
    return { kind: "item", value: stream.values.shift() };
  },
  empty_optional_events_stream_cancel(handle: string) {
    cancelCount += 1;
    streams.delete(handle);
  },
  single_optional_event() {
    const handle = `os${nextId++}`;
    streams.set(handle, { values: [null], nextCalls: 0 });
    return handle;
  },
  async single_optional_event_stream_next(handle: string) {
    const stream = streams.get(handle);
    if (!stream || stream.values.length === 0) return { kind: "done" };
    stream.nextCalls += 1;
    return { kind: "item", value: stream.values.shift() };
  },
  single_optional_event_stream_cancel(handle: string) {
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
    if (!stream) return { kind: "done" };
    errorNextCalls += 1;
    stream.nextCalls += 1;
    if (stream.errorAt === stream.nextCalls) {
      return { kind: "error", error: "Boom" };
    }
    const value = stream.values.shift();
    return value === undefined ? { kind: "done" } : { kind: "item", value };
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

const optionalValues: Array<number | null> = [];
for await (const value of optionalEvents()) {
  optionalValues.push(value);
}
assert(optionalValues.length === 3, `optional stream item count ${optionalValues.length}`);
assert(optionalValues[0] === 1 && optionalValues[1] === null && optionalValues[2] === 2,
  `optional stream values ${optionalValues}`);

const emptyOptionalValues: Array<number | null> = [];
for await (const value of emptyOptionalEvents()) {
  emptyOptionalValues.push(value);
}
assert(emptyOptionalValues.length === 0, `empty optional stream values ${emptyOptionalValues}`);

const singleOptionalValues: Array<number | null> = [];
for await (const value of singleOptionalEvent()) {
  singleOptionalValues.push(value);
}
assert(singleOptionalValues.length === 1 && singleOptionalValues[0] === null,
  `single optional stream values ${singleOptionalValues}`);

const errorIterator = errorAfterOne()[Symbol.asyncIterator]();
const errorFirst = await errorIterator.next();
assert(errorFirst.done === false && errorFirst.value.value === 7, "stream error first item");
const beforeError = cancelCount;
let threw = false;
try {
  await errorIterator.next();
} catch (error) {
  threw = true;
  assert(error instanceof UniffiError, "stream error should be wrapped");
  assert((error as UniffiError).errorName === "StreamError", "stream error name");
}
assert(threw, "stream error should throw");
assert(cancelCount === beforeError + 1, "stream error should cancel once");
assert((await errorIterator.next()).done === true, "error iterator should be terminal");
assert(errorNextCalls === 2, `error after terminal next calls ${errorNextCalls}`);

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

const thrown = countEvents(10)[Symbol.asyncIterator]();
const callerError = new Error("caller error");
const beforeThrow = cancelCount;
let callerErrorPreserved = false;
try {
  await thrown.throw?.(callerError);
} catch (error) {
  callerErrorPreserved = error === callerError;
}
assert(callerErrorPreserved, "throw should reject the caller error");
await thrown.return?.();
assert(cancelCount === beforeThrow, "throw before first pull should not start or cancel a native handle");

let resolvePending: ((value: unknown) => void) | null = null;
__installBackend({
  __uniffiJsRuntimeAbiVersion: 2,
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
resolvePending?.({ kind: "item", value: { value: 99 } });
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

const beforeMalformed = cancelCount;
__installBackend({
  __uniffiJsRuntimeAbiVersion: 2,
  optional_events() { return "malformed"; },
  async optional_events_stream_next() { return {}; },
  optional_events_stream_cancel() { cancelCount += 1; },
});
const malformed = optionalEvents()[Symbol.asyncIterator]();
let malformedRejected = false;
try {
  await malformed.next();
} catch (error) {
  malformedRejected = error instanceof UniffiError
    && (error as UniffiError).errorName === "UniffiStreamProtocolError";
}
assert(malformedRejected, "malformed envelope must reject rather than become Done");
assert(cancelCount === beforeMalformed + 1, "malformed envelope should clean up once");
assert((await malformed.next()).done === true, "malformed iterator should be terminal");

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
        &std::fs::read(out_dir.join("components/stream_core/harmony/stream_core.ohos-facade.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(contract["component"], "stream_core");
    assert_eq!(contract["namespace"], "stream_core");
    assert_eq!(contract["nativeExportPrefix"], "ffi_stream_core");
    assert_eq!(contract["outputStreams"].as_array().unwrap().len(), 6);
    for output in contract["outputStreams"].as_array().unwrap() {
        let fields = output
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            fields,
            [
                "arguments",
                "cancelFunction",
                "errorType",
                "function",
                "itemType",
                "nextFunction",
                "pullClass",
                "stepType",
                "streamFactory",
            ]
            .into_iter()
            .collect()
        );
    }
    assert_eq!(
        contract["outputStreams"][0]["stepType"],
        "ffi_stream_core_UniffiCountEventsStreamNext"
    );
    assert_eq!(
        contract["outputStreams"][0]["streamFactory"],
        "countEventsStream"
    );
    assert!(contract["inputStreams"].as_array().unwrap().is_empty());

    let optional_stream = contract["outputStreams"]
        .as_array()
        .unwrap()
        .iter()
        .find(|stream| stream["function"] == "ffi_stream_core_optional_events")
        .expect("facade contract must retain the optional stream item descriptor");
    assert_eq!(
        optional_stream["nextFunction"],
        "ffi_stream_core_optional_events_stream_next"
    );
    assert_eq!(
        optional_stream["cancelFunction"],
        "ffi_stream_core_optional_events_stream_cancel"
    );
    assert_eq!(
        optional_stream["itemType"],
        serde_json::json!({
            "kind": "optional",
            "inner": { "kind": "number" },
        })
    );
    assert_eq!(
        optional_stream["stepType"],
        "ffi_stream_core_UniffiOptionalEventsStreamNext"
    );

    let extra_types = std::fs::read_to_string(
        out_dir.join("components/stream_core/harmony/stream_core.ohos-extra-types.d.ts"),
    )
    .unwrap();
    let optional_step = extra_types
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(
                line.strip_prefix("type_def:")
                    .expect("OHOS sidecar must use only type_def records"),
            )
            .unwrap()
        })
        .find(|definition| definition["name"] == "ffi_stream_core_UniffiOptionalEventsStreamNext")
        .expect("OHOS sidecar must declare the optional stream envelope");
    assert_eq!(optional_step["kind"], "interface");
    assert_eq!(optional_step["typeParameters"], serde_json::json!([]));
    assert_eq!(
        optional_step["def"],
        "kind: string\nvalue?: number | undefined | null\nerror?: ffi_stream_core_StreamError"
    );

    let root_index = std::fs::read_to_string(out_dir.join("harmony/index.ts")).unwrap();
    assert!(
        root_index.contains("stream_core") && !root_index.contains("export * from"),
        "harmony root should expose only the stream_core namespace:\n{root_index}"
    );
    let index =
        std::fs::read_to_string(out_dir.join("components/stream_core/harmony/index.ts")).unwrap();
    assert!(
        index.contains("export * from \"./stream.ts\";"),
        "component harmony index should re-export stream fallback helpers:\n{index}"
    );

    let stream =
        std::fs::read_to_string(out_dir.join("components/stream_core/harmony/stream.ts")).unwrap();
    for needle in [
        "import type { UniFfiStream } from \"../common/runtime.ts\";",
        "export function countEventsStream(count: number): UniFfiStream<StreamEvent>",
        "return countEvents(count);",
        "export function errorAfterOneStream(): UniFfiStream<StreamEvent>",
        "return errorAfterOne();",
        "export function optionalEventsStream(): UniFfiStream<number | null>",
        "return optionalEvents();",
        "import type { StreamEvent } from \"../common/public-types.ts\";",
    ] {
        assert!(
            stream.contains(needle),
            "harmony stream helper missing `{needle}`:\n{stream}"
        );
    }
    assert!(
        !stream.contains("__call") && !stream.contains("_stream_next"),
        "harmony stream helper should not expose raw stream ABI keys:\n{stream}"
    );
    assert!(
        !stream.contains("toUniFfiStream")
            && !stream.contains("source[Symbol.asyncIterator]")
            && !stream.contains("returnFn.call"),
        "harmony stream helper must delegate to the canonical strict runtime:\n{stream}"
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
    let shared = root.join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(
        shared.join("runtime.ts"),
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
} from "./shared/runtime.ts";

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
        &std::fs::read(
            out_dir.join("components/input_stream_core/harmony/input_stream_core.ohos-facade.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(contract["component"], "input_stream_core");
    assert_eq!(contract["namespace"], "input_stream_core");
    assert_eq!(contract["nativeExportPrefix"], "ffi_input_stream_core");
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
    let input_stream_type = "ffi_input_stream_core_UniffiInputStream";
    assert!(factory.starts_with("createItemRecordCounterEventErrorEnumStreamErrorFingerprint"));
    assert!(factory.ends_with("InputChannel"));
    assert_eq!(contract["inputStreams"][0]["itemType"]["kind"], "named");
    assert_eq!(contract["inputStreams"][0]["errorType"]["kind"], "named");
    assert_eq!(
        input_next_type,
        format!("ffi_input_stream_core_UniffiInputStream{input_suffix}Next")
    );
    assert_eq!(
        contract["outputStreams"][0]["function"],
        "ffi_input_stream_core_running_sum"
    );
    assert_eq!(
        contract["outputStreams"][0]["nextFunction"],
        "ffi_input_stream_core_running_sum_stream_next"
    );
    assert_eq!(
        contract["outputStreams"][0]["cancelFunction"],
        "ffi_input_stream_core_running_sum_stream_cancel"
    );
    assert_eq!(
        contract["outputStreams"][0]["stepType"],
        "ffi_input_stream_core_UniffiRunningSumStreamNext"
    );
    let extra_types = std::fs::read_to_string(
        out_dir
            .join("components/input_stream_core/harmony/input_stream_core.ohos-extra-types.d.ts"),
    )
    .unwrap();
    let input_stream_def = extra_types
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(
                line.strip_prefix("type_def:")
                    .expect("OHOS sidecar must use only type_def records"),
            )
            .unwrap()
        })
        .find(|definition| definition["name"] == input_stream_type)
        .expect("OHOS sidecar must declare the canonical prefixed UniffiInputStream");
    assert_eq!(input_stream_def["kind"], "interface");
    assert_eq!(input_stream_def["typeParameters"], serde_json::json!(["T"]));
    assert!(input_stream_def["def"]
        .as_str()
        .unwrap()
        .contains("next(error: Error | null, handle: number): Promise<T>"));

    let api = std::fs::read_to_string(out_dir.join("components/input_stream_core/common/api.ts"))
        .unwrap();
    for needle in [
        "createUniffiInputStream",
        "export async function sumInputEvents(events: AsyncIterable<CounterEvent>): Promise<bigint>",
        "createUniffiInputStream(events, {",
        "lowerItem: (__uniffiInputValue0: any) => ({ value: __uniffiInputValue0.value })",
        "lowerError: (__uniffiInputError0: unknown) => __uniffiInputError0",
        "errorShape: \"flat\"",
        "export function runningSum(events: AsyncIterable<CounterEvent>): UniFfiStream<CounterEvent>",
        "return createUniFfiStream<CounterEvent, unknown>({",
        "start: () => __call<any>(\"running_sum\", createUniffiInputStream(events, {",
        "next: async (__streamHandle: unknown) =>",
        "switch (__next.kind)",
        "__call<void>(\"running_sum_stream_cancel\", __streamHandle);",
    ] {
        assert!(
            api.contains(needle),
            "common/api.ts missing input stream contract `{needle}`:\n{api}"
        );
    }

    let backend =
        std::fs::read_to_string(out_dir.join("components/input_stream_core/node/backend-napi.ts"))
            .unwrap();
    assert!(
        backend.contains("__uniffiInputStream")
            && backend.contains(
                "return { handle: marker.handle, next: marker.next, cancel: marker.cancel };"
            ),
        "napi backend should coerce input stream markers:\n{backend}"
    );
    let preload =
        std::fs::read_to_string(out_dir.join("components/input_stream_core/electron/preload.cjs"))
            .unwrap();
    assert!(
        preload.contains("__uniffiInputStream")
            && preload
                .contains("return { handle: arg.handle, next: arg.next, cancel: arg.cancel };"),
        "electron preload should forward input stream markers:\n{preload}"
    );
    let napi_rs = std::fs::read_to_string(
        out_dir.join("components/input_stream_core/node/input_stream_core.rs"),
    )
    .unwrap();
    let compact_napi_rs = napi_rs.split_whitespace().collect::<String>();
    assert!(
        compact_napi_rs.contains(
            "pubstruct__UniffiInputStream<NextResult:'static+napi::bindgen_prelude::FromNapiValue,>"
        ),
        "napi bridge must fully qualify the input-stream FromNapiValue bound:\n{napi_rs}"
    );
    for needle in [
        input_next_type.clone(),
        "impl ::uniffi::ForeignInputStreamOps".to_string(),
        "::uniffi::UniFfiInputStream::from_handle_and_ops".to_string(),
        "napi::threadsafe_function::ThreadsafeFunctionCallMode::NonBlocking".to_string(),
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
        napi_rs.contains(&format!("UniffiInputStream{input_suffix}Ops")),
        "napi bridge must use the canonical contract suffix `{input_suffix}`:\n{napi_rs}"
    );
    let wasm_rs = std::fs::read_to_string(
        out_dir.join("components/input_stream_core/browser/input_stream_core.rs"),
    )
    .unwrap();
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

    let backend =
        std::fs::read_to_string(out_dir.join("components/callback_shape/node/backend-napi.ts"))
            .unwrap();
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
  ffi_callback_shape_emit_event(sink) {
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
const root = await import("./node/index.ts");
const api = root.callback_shape;

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

    let preload =
        std::fs::read_to_string(out_dir.join("components/callback_shape/electron/preload.cjs"))
            .unwrap();
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
  ffi_callback_shape_emit_event(sink) {
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
const root = await import("./electron/index.ts");
const api = root.callback_shape;

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

    let preload =
        std::fs::read_to_string(out_dir.join("components/arithmetic/electron/preload.cjs"))
            .unwrap();
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
    path.resolve(__dirname, "components/arithmetic/electron/preload.cjs"),
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
        "shared/runtime.ts",
        "harmony/index.ts",
        "components/arithmetic/common/public-types.ts",
        "components/arithmetic/node/backend-napi.ts",
        "components/arithmetic/node/arithmetical.rs",
        "components/arithmetic/harmony/index.ts",
        "components/arithmetic/harmony/backend-ohos.ts",
        "components/arithmetic/harmony/arithmetical.ohos-extra-types.d.ts",
        "components/arithmetic/harmony/arithmetical.rs",
    ] {
        let p = out_dir.join(name);
        assert!(p.exists(), "expected output file missing: {p}");
    }

    let node_rs =
        std::fs::read_to_string(out_dir.join("components/arithmetic/node/arithmetical.rs"))
            .unwrap();
    assert!(
        !node_rs.contains("use napi::bindgen_prelude::*;")
            && node_rs.contains("use napi_derive::napi;")
            && node_rs.contains("napi::bindgen_prelude::Result"),
        "node bridge must use collision-safe qualified napi-rs paths:\n{node_rs}"
    );
    assert!(
        !node_rs.contains("napi_ohos"),
        "node bridge must not use ohos-rs imports:\n{node_rs}"
    );

    let ohos_rs =
        std::fs::read_to_string(out_dir.join("components/arithmetic/harmony/arithmetical.rs"))
            .unwrap();
    assert!(
        ohos_rs.contains("extern crate napi_ohos as napi;")
            && ohos_rs.contains("use napi_derive_ohos::napi;")
            && ohos_rs.contains("napi::bindgen_prelude::BigInt"),
        "harmony bridge must bind internal napi paths to ohos-rs directly:\n{ohos_rs}"
    );
    assert!(
        !ohos_rs.contains("use napi_derive::napi;"),
        "harmony bridge must not import the ordinary napi-rs derive crate:\n{ohos_rs}"
    );

    let backend =
        std::fs::read_to_string(out_dir.join("components/arithmetic/harmony/backend-ohos.ts"))
            .unwrap();
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
        "__uniffiJsRuntimeAbiVersion",
        "then(",
    ] {
        assert!(
            backend.contains(required),
            "harmony backend missing `{required}`:\n{backend}"
        );
    }
}

#[test]
fn custom_types_emit_public_contract() {
    let tmp = tempfile::tempdir().unwrap();
    let (generated, _manifest) = generate_custom_napi_tree(tmp.path());

    let custom_types =
        std::fs::read_to_string(generated.join("components/custom_js_core/common/custom-types.ts"))
            .unwrap();
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

    let api =
        std::fs::read_to_string(generated.join("components/custom_js_core/common/api.ts")).unwrap();
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
    let public_types =
        std::fs::read_to_string(generated.join("components/custom_js_core/common/public-types.ts"))
            .unwrap();
    assert!(
        public_types.contains("export type { Email } from \"./custom-types.ts\";"),
        "public-types.ts should re-export Email:\n{public_types}"
    );
}

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
import { toI64, toU64, UniffiError } from "./shared/runtime.ts";

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
