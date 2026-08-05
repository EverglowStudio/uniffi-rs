//! CLI orchestration smoke test for the JavaScript wasm build path.
//!
//! This keeps the productized `uniffi-bindgen javascript build-wasm`
//! flow covered without editing the larger `smoke.rs` harness.

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

fn write_cli_wasm_fixture(root: &std::path::Path) -> Utf8PathBuf {
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
        "fn main() {\n    uniffi::generate_scaffolding(\"src/cli_wasm.udl\").unwrap();\n}\n",
    )
    .unwrap();
    std::fs::write(
        src.join("cli_wasm.udl"),
        "namespace cli_wasm {\n\
         \x20   u64 add(u64 a, u64 b);\n\
         };\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn add(a: u64, b: u64) -> u64 { a + b }\n\n\
         uniffi::include_scaffolding!(\"cli_wasm\");\n",
    )
    .unwrap();

    Utf8PathBuf::from_path_buf(crate_dir.join("Cargo.toml")).unwrap()
}

#[test]
fn cli_build_wasm_orchestrates_synthetic_fixture() {
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let target_root = shared_cargo_target_dir("cli");
    let wasm_core_target_dir = target_root.join("wasm-core");
    let wasm_target_dir = target_root.join("wasm-host");
    let _target_lock = shared_cargo_target_lock("cli");

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated/native/hosts")).unwrap();
    let pkg_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated/browser/pkg")).unwrap();
    let manifest = write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
        .arg("javascript")
        .arg("build-wasm")
        .arg("--manifest-path")
        .arg(manifest.as_str())
        .arg("--out-dir")
        .arg(out_dir.as_str())
        .arg("--host-crates-dir")
        .arg(host_dir.as_str())
        .arg("--wasm-bindgen-out-dir")
        .arg(pkg_dir.as_str())
        .arg("--core-target-dir")
        .arg(wasm_core_target_dir.as_str())
        .arg("--target-dir")
        .arg(wasm_target_dir.as_str())
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
        "shared/uniffi_runtime.js",
        "shared/uniffi_runtime.d.ts",
        "browser/index.js",
        "browser/index.d.ts",
        "browser/backend.js",
        "components/cli_wasm/index.js",
        "components/cli_wasm/index.d.ts",
        "native/wasm.rs",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing generated JS file: {file}");
    }
    let browser_entry = std::fs::read_to_string(out_dir.join("browser/index.js")).unwrap();
    assert!(
        browser_entry
            .matches("import * as __backend from \"./backend.js\";")
            .count()
            == 1
            && browser_entry.contains("import * as __glue from ")
            && browser_entry
                .contains("export const ready = __backend.initWithGlue(__glue, undefined);")
            && browser_entry.contains(
                "export function init(input) { return __backend.initWithGlue(__glue, input); }",
            )
            && !browser_entry.contains("export function initWithGlue"),
        "Web index must own the planned glue loader and ready/init lifecycle:\n{browser_entry}"
    );
    let browser_backend = std::fs::read_to_string(out_dir.join("browser/backend.js")).unwrap();
    assert!(
        browser_backend.contains("let __bootPromise;")
            && browser_backend.contains("if (__bootPromise !== undefined) return __bootPromise;")
            && browser_backend.contains("__bootPromise = (async () => {")
            && browser_backend.contains("export function initWithGlue")
            && !browser_backend.contains("export const ready"),
        "browser backend must expose only the explicit idempotent coordinator:\n{browser_backend}"
    );
    assert!(
        !browser_backend.contains("import * as namespaces from \"./index.js\";")
            && !browser_backend.contains("export * from \"./index.js\";"),
        "browser backend must not self-import the canonical index:\n{browser_backend}"
    );
    let browser_rs_text = std::fs::read_to_string(out_dir.join("native/wasm.rs")).unwrap();
    assert!(
        browser_rs_text.contains("wasm_bindgen"),
        "native/wasm.rs should be a wasm-bindgen host adapter"
    );

    let host_manifest = host_dir.join("wasm/Cargo.toml");
    assert!(host_manifest.exists(), "missing generated wasm host crate");
    let host_lib = host_dir.join("wasm/src/lib.rs");
    let host_lib_text = std::fs::read_to_string(&host_lib).unwrap();
    assert!(
        host_lib_text.contains("include!("),
        "generated wasm host crate should include the package-level wasm adapter"
    );
    assert!(
        host_lib_text.contains("../../../wasm.rs"),
        "generated wasm host crate should include the package-level wasm adapter:\n{host_lib_text}"
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
    assert!(
        pkg_entries
            .iter()
            .any(|p| p.extension().and_then(|e| e.to_str()) == Some("js")),
        "wasm-bindgen output must include a JS glue file"
    );
}

#[test]
fn cli_artifacts_wasm_and_mini_orchestrate_two_components_without_feature_flags() {
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);

    let tmp = tempfile::tempdir().unwrap();
    let fixture = CompositeFixture::write(tmp.path());
    fixture.build_cdylib();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated/native/hosts")).unwrap();
    let artifact_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated/artifacts")).unwrap();
    let pkg_dir = artifact_dir.join("browser/pkg");
    let target_root = shared_cargo_target_dir("cli");
    let wasm_target_dir = target_root.join("wasm-host");
    let _target_lock = shared_cargo_target_lock("cli");
    let output = Command::new(cli.as_std_path())
        .current_dir(root.as_std_path())
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
        .args([
            "artifacts",
            "build",
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
            "--wasm-bindgen-out-dir",
            pkg_dir.as_str(),
            "--wasm-target-dir",
            wasm_target_dir.as_str(),
            "--target",
            "wasm",
            "--target",
            "mini-program",
        ])
        .output()
        .expect("failed to invoke two-component uniffi-bindgen artifacts build");
    assert!(
        output.status.success(),
        "two-component wasm/Mini artifacts build failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let glue_files = std::fs::read_dir(pkg_dir.as_std_path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("js"))
        .collect::<Vec<_>>();
    assert_eq!(
        glue_files.len(),
        1,
        "the two components must share one wasm-bindgen glue module: {glue_files:?}"
    );
    let glue_stem = glue_files[0]
        .file_stem()
        .and_then(|stem| stem.to_str())
        .expect("wasm-bindgen glue path must have a UTF-8 file stem");
    let browser_entry = std::fs::read_to_string(out_dir.join("browser/index.js")).unwrap();
    assert!(
        browser_entry
            .matches("import * as __backend from \"./backend.js\";")
            .count()
            == 1
            && browser_entry
                .contains("export const ready = __backend.initWithGlue(__glue, undefined);")
            && browser_entry.contains(
                "export function init(input) { return __backend.initWithGlue(__glue, input); }",
            ),
        "two-component Browser index must load the planned backend exactly once:\n{browser_entry}"
    );
    let browser_backend = std::fs::read_to_string(out_dir.join("browser/backend.js")).unwrap();
    assert!(
        browser_backend.contains("let __bootPromise;")
            && browser_backend.contains("if (__bootPromise !== undefined) return __bootPromise;")
            && browser_backend.contains("__bootPromise = (async () => {")
            && !browser_backend.contains("export const ready"),
        "two-component Browser backend must guard one idempotent initialization:\n{browser_backend}"
    );
    for component in CANONICAL_COMPONENTS {
        assert!(
            browser_backend.contains(component.namespace)
                && browser_backend.contains(&format!(
                    "__{}Module.createNamespace(session)",
                    component.namespace
                )),
            "two-component Browser backend must expose namespace {}:\n{browser_backend}",
            component.namespace,
        );
    }
    let mini_entry =
        std::fs::read_to_string(out_dir.join("browser/index.mini-program.js")).unwrap();
    for (label, auto_entry) in [("Mini Program", mini_entry)] {
        assert!(
            auto_entry.contains("import * as __backend from \"./backend.js\";")
                && auto_entry
                    .contains("export { session, close, alpha, beta } from \"./backend.js\";",),
            "two-component {label} auto entry must compose the backend coordinator:\n{auto_entry}"
        );
        assert!(
            !auto_entry.contains("import * as namespaces from \"./index.js\";")
                && !auto_entry.contains("export * from \"./index.js\";"),
            "two-component {label} auto entry must not self-import index.js:\n{auto_entry}"
        );
        assert!(
            auto_entry.contains("let readyPromise = null;")
                && auto_entry.contains("readyPromise ??= installAll(customGlue, wasmPath);")
                && auto_entry.contains("return __backend.initWithGlue(customGlue, wasmPath);")
                && auto_entry.matches("return __backend.initWithGlue(customGlue, wasmPath);").count() == 1
                && auto_entry.contains("return initWithGlue(glue, wasmPath);")
                && auto_entry.contains(&format!("{glue_stem}.js")),
            "two-component {label} auto entry must use the one canonical glue module:\n{auto_entry}"
        );
    }
    assert!(
        out_dir.join("native/wasm.rs").exists(),
        "missing package wasm adapter"
    );
    let host_lib = std::fs::read_to_string(host_dir.join("wasm/src/lib.rs")).unwrap();
    assert!(
        host_lib.contains("../../../wasm.rs"),
        "one composite wasm host must include the package wasm adapter:\n{host_lib}"
    );
}

#[test]
fn cli_build_wasm_orchestrates_arithmetic_fixture() {
    let cargo = which_tool("cargo");
    assert_wasm32_target(&cargo);
    let root = workspace_root();
    let cli = build_uniffi_bindgen_cli(&cargo);
    let target_root = shared_cargo_target_dir("cli");
    let wasm_core_target_dir = target_root.join("wasm-core");
    let wasm_target_dir = target_root.join("wasm-host");
    let _target_lock = shared_cargo_target_lock("cli");
    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = out_dir.join("native/hosts");
    let pkg_dir = out_dir.join("browser/pkg");
    let (manifest, source) = shared::write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
        .env("CARGO_TARGET_DIR", target_root.as_std_path())
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
        .arg("--core-target-dir")
        .arg(wasm_core_target_dir.as_str())
        .arg("--target-dir")
        .arg(wasm_target_dir.as_str())
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
        "shared/uniffi_runtime.js",
        "shared/uniffi_runtime.d.ts",
        "browser/index.js",
        "browser/index.d.ts",
        "browser/backend.js",
        "components/cli_wasm/index.js",
        "components/cli_wasm/index.d.ts",
        "native/wasm.rs",
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
