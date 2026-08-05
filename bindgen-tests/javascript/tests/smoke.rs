//! Fast JavaScript package smoke checks.
//!
//! Smoke intentionally exercises only the in-process generator, the
//! deterministic package tree, and one pure-JavaScript runtime call.  Native
//! host compilation, Wasm target compilation, and addon execution belong to
//! the dedicated integration targets.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::*;

#[test]
fn generates_minimal_package_tree() {
    let out = tempfile::tempdir().unwrap();
    let package_root = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&package_root);

    // Public source, declarations, platform entrypoints, and private native
    // adapters are all members of one package root.  This list is deliberately
    // small: detailed facade/engine contracts live in contracts.rs and the
    // target-specific E2E suites.
    for path in [
        "shared/uniffi_runtime.js",
        "shared/uniffi_runtime.d.ts",
        "components/arithmetic/index.js",
        "components/arithmetic/index.d.ts",
        "node/index.js",
        "node/index.d.ts",
        "browser/index.js",
        "browser/index.d.ts",
        "Index.ets",
        "Index.d.ets",
        "native/node.rs",
        "native/wasm.rs",
        "native/ohos.rs",
    ] {
        let path = package_root.join(path);
        assert!(
            path.is_file(),
            "expected generated package file missing: {path}"
        );
    }

    let component =
        std::fs::read_to_string(package_root.join("components/arithmetic/index.js")).unwrap();
    assert!(component.contains("createNamespace"));
    assert!(component.contains("add"));
    assert!(!component.contains(".ts"));

    let wasm = std::fs::read_to_string(package_root.join("native/wasm.rs")).unwrap();
    assert!(wasm.contains("wasm_bindgen"));
    let node = std::fs::read_to_string(package_root.join("native/node.rs")).unwrap();
    assert!(node.contains("napi_derive::napi"));
    let ohos = std::fs::read_to_string(package_root.join("native/ohos.rs")).unwrap();
    assert!(ohos.contains("napi_ohos"));

    // The deleted per-component sidecar/contract tree must not reappear.
    assert!(!package_root.join("components/arithmetic/harmony").exists());
    assert!(!package_root.join("components/arithmetic/node").exists());
    assert!(!package_root.join("components/arithmetic/browser").exists());
}

#[test]
fn runs_component_api_under_node() {
    let node = locate_node_with_strip_types();

    let out = tempfile::tempdir().unwrap();
    let package_root = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&package_root);

    // Use the generated facade with a pure-JS BackendSession.  This proves
    // the public package can be consumed without building a native addon or
    // invoking any nested Cargo command.
    let harness = r#"
import { createBackendSession } from "./shared/uniffi_runtime.js";
import { createNamespace } from "./components/arithmetic/index.js";

const session = createBackendSession(() => ({
  invokeSync(id, args) {
    if (id === 0) return args[0] + args[1];
    if (id === 3) {
      if (args[0] < args[1]) {
        throw { errorName: "ArithmeticError", variant: "IntegerOverflow", message: "sub underflow" };
      }
      return args[0] - args[1];
    }
    if (id === 1) return args[0] / args[1];
    if (id === 2) return args[0] === args[1];
    throw new Error(`unknown operation ${id}`);
  },
  async invokeAsync() { throw new Error("unexpected async operation"); },
  close() {},
}));

const api = createNamespace(session);
if (api.add(2n, 3n) !== 5n) throw new Error("add failed");
if (api.div(20n, 4n) !== 5n) throw new Error("div failed");
if (api.equal(3n, 3n) !== true) throw new Error("equal failed");
if (api.sub(10n, 4n) !== 6n) throw new Error("sub failed");
let threw = false;
try { api.sub(1n, 5n); } catch (error) {
  threw = error?.errorName === "ArithmeticError" && error?.variant === "IntegerOverflow";
}
if (!threw) throw new Error("sub underflow should preserve ArithmeticError");
await session.close();
console.log("ok");
"#;
    std::fs::write(package_root.join("driver.mjs"), harness).unwrap();

    let output = Command::new(&node)
        .arg("--no-warnings")
        .arg("driver.mjs")
        .current_dir(&package_root)
        .output()
        .expect("failed to invoke node");
    assert!(
        output.status.success(),
        "node driver failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("ok"),
        "node driver did not print ok"
    );
}
