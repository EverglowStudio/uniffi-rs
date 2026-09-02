//! CLI orchestration smoke test for the JavaScript N-API build path.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::{
    shared_cargo_target_dir, shared_cargo_target_lock, workspace_root, CompositeFixture,
    CANONICAL_COMPONENTS,
};

use camino::Utf8PathBuf;
use std::process::Command;

fn write_cli_napi_fixture(root: &std::path::Path) -> Utf8PathBuf {
    let crate_dir = root.join("cli_napi_fixture");
    let src = crate_dir.join("src");
    std::fs::create_dir_all(&src).unwrap();

    let workspace = workspace_root();
    let uniffi_path = workspace.join("uniffi");
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\n\
             name = \"cli-napi-fixture\"\n\
             version = \"0.0.0\"\n\
             edition = \"2021\"\n\
             publish = false\n\n\
             [lib]\n\
             name = \"cli_napi_fixture\"\n\
             crate-type = [\"lib\", \"cdylib\"]\n\n\
             [dependencies]\n\
             uniffi = {{ path = \"{}\" }}\n\n\
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
        "fn main() {\n    uniffi::generate_scaffolding(\"src/cli_napi.udl\").unwrap();\n}\n",
    )
    .unwrap();
    std::fs::write(
        src.join("cli_napi.udl"),
        "namespace cli_napi {\n\
         \x20   u64 add(u64 a, u64 b);\n\
         };\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn add(a: u64, b: u64) -> u64 { a + b }\n\n\
         uniffi::include_scaffolding!(\"cli_napi\");\n",
    )
    .unwrap();

    Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap()
}

#[test]
fn cli_build_napi_orchestrates_synthetic_fixture() {
    let cargo = which_tool("cargo");

    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    let target_root = shared_cargo_target_dir("cli");
    let target_dir = target_root.join("napi");
    let _target_lock = shared_cargo_target_lock("cli");
    let manifest = write_cli_napi_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
        .arg("javascript")
        .arg("build-napi")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--out-dir")
        .arg(out_dir.as_str())
        .arg("--host-crates-dir")
        .arg(host_dir.as_str())
        .arg("--target-dir")
        .arg(target_dir.as_str())
        .output()
        .expect("failed to invoke uniffi-bindgen javascript build-napi");
    if !output.status.success() {
        panic!(
            "javascript build-napi failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    for path in [
        "shared/uniffi_runtime.js",
        "shared/uniffi_runtime.d.ts",
        "node/index.js",
        "node/index.d.ts",
        "components/cli_napi/index.js",
        "components/cli_napi/index.d.ts",
        "electron/index.js",
        "electron/preload.cjs",
        "electron/index.d.ts",
        "native/node.rs",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing generated N-API file: {file}");
    }
    // A generated package has one composite N-API host.  Component adapters
    // intentionally share the package-level artifact rather than each
    // receiving a copy with a namespace-derived filename.
    assert_single_node_addon(out_dir.join("node"));
    assert!(
        host_dir.join("napi/Cargo.toml").exists(),
        "missing generated napi host crate"
    );
    let backend_napi = std::fs::read_to_string(out_dir.join("node/index.js")).unwrap();
    assert!(
        backend_napi.contains("__uniffi_backend_factory")
            && backend_napi.contains("createRequire")
            && backend_napi.contains("new BackendSession"),
        "node entry should expose the generated native backend:\n{backend_napi}"
    );
    let electron_preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    for expected in [
        "function __napiCarrier(value)",
        "Object.prototype.toString.call(value) === \"[object Uint8Array]\"",
        "return __napiCarrier(__rendererHost[method](...args))",
        "Promise.resolve(__hostCall(method, args)).then(__napiCarrier)",
    ] {
        assert!(
            electron_preload.contains(expected),
            "Electron preload must localize callback carriers before N-API using `{expected}`:\n{electron_preload}"
        );
    }
    assert!(
        electron_preload.contains("return __backendMethod(name).apply(__backend, args)"),
        "Electron backend operation dispatch must remain unchanged:\n{electron_preload}"
    );

    let node = which_tool("node");
    assert_node_strip_types(&node);
    let driver = tmp.path().join("generated-adapter-driver.ts");
    std::fs::write(
        &driver,
        "import * as root from './generated/node/index.js';\n\
         const value = root.cli_napi.add(2n, 3n);\n\
         if (value !== 5n) throw new Error(`expected 5n, got ${value}`);\n\
         console.log('ok');\n",
    )
    .unwrap();
    let run = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(&driver)
        .output()
        .expect("failed to run generated N-API adapter driver");
    if !run.status.success() {
        panic!(
            "generated N-API adapter driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("ok"),
        "generated N-API adapter driver did not print ok"
    );

    assert!(single_node_addon(out_dir.join("node")).exists());
}

#[test]
fn cli_build_napi_orchestrates_two_components_without_feature_flags() {
    let cargo = which_tool("cargo");
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);

    let tmp = tempfile::tempdir().unwrap();
    let fixture = CompositeFixture::write(tmp.path());
    fixture.build_cdylib();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    let artifact_dir = out_dir.join("artifacts");
    let target_root = shared_cargo_target_dir("cli");
    let target_dir = target_root.join("napi");
    let _target_lock = shared_cargo_target_lock("cli");
    let output = Command::new(cli.as_std_path())
        .current_dir(root.as_std_path())
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
        .args([
            "javascript",
            "build-napi",
            "--manifest-path",
            fixture.manifest_path().as_str(),
            "--source",
            fixture.library_path().as_str(),
            "--out-dir",
            out_dir.as_str(),
            "--host-crates-dir",
            host_dir.as_str(),
            "--artifact-dir",
            artifact_dir.as_str(),
            "--target-dir",
            target_dir.as_str(),
        ])
        .output()
        .expect("failed to invoke two-component uniffi-bindgen javascript build-napi");
    assert!(
        output.status.success(),
        "two-component javascript build-napi failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let composite_addon = format!(
        "{}.node",
        uniffi_bindgen_javascript::host_crates::composite_host_lib_target("composite-core")
    );
    let addon = single_node_addon(artifact_dir.join("node"));
    assert_eq!(
        addon.file_name().and_then(|name| name.to_str()),
        Some(composite_addon.as_str()),
        "both generated namespaces must share the canonical composite addon"
    );
    let host_lib = std::fs::read_to_string(host_dir.join("napi/src/lib.rs")).unwrap();
    assert!(
        host_dir.join("napi/Cargo.toml").exists(),
        "two components must produce one N-API host crate"
    );
    assert!(
        host_lib.contains("../../../node.rs"),
        "one composite N-API host must include the package node adapter:\n{host_lib}"
    );
    let expected_addon_path = format!("../artifacts/node/{composite_addon}");
    let node_entry = std::fs::read_to_string(out_dir.join("node/index.js")).unwrap();
    let electron_preload = std::fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
    for (surface, source) in [
        ("Node entry", node_entry),
        ("Electron preload", electron_preload),
    ] {
        assert!(
            source.contains(&expected_addon_path),
            "{surface} must use the one canonical composite addon path `{expected_addon_path}`:\n{source}"
        );
    }
    for platform_entry in ["node/index.js", "electron/index.js"] {
        let source = std::fs::read_to_string(out_dir.join(platform_entry)).unwrap();
        for component in CANONICAL_COMPONENTS {
            assert!(
                source.contains(component.namespace),
                "{platform_entry} must expose namespace {}:\n{source}",
                component.namespace,
            );
        }
    }
}

fn assert_single_node_addon(dir: Utf8PathBuf) {
    let _ = single_node_addon(dir);
}

fn single_node_addon(dir: Utf8PathBuf) -> std::path::PathBuf {
    let addons = std::fs::read_dir(dir.as_std_path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("node"))
        .collect::<Vec<_>>();
    assert_eq!(
        addons.len(),
        1,
        "expected exactly one .node addon in {dir}: {addons:?}"
    );
    addons.into_iter().next().unwrap()
}
