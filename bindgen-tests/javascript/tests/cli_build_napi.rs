//! CLI orchestration smoke test for the JavaScript N-API build path.

mod support;

use support::{CompositeFixture, CANONICAL_COMPONENTS};

use camino::Utf8PathBuf;
use std::process::Command;

fn workspace_root() -> Utf8PathBuf {
    let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../..").canonicalize_utf8().unwrap()
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

fn node_supports_strip_types(node: &std::path::Path) -> bool {
    let Ok(output) = Command::new(node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg("-e")
        .arg("console.log('ok')")
        .output()
    else {
        return false;
    };
    output.status.success()
}

fn build_uniffi_bindgen(root: &Utf8PathBuf, cargo: &std::path::Path) {
    let output = Command::new(cargo)
        .current_dir(root.as_std_path())
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
    if !output.status.success() {
        panic!(
            "building uniffi-bindgen failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

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
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_build_napi_orchestrates_synthetic_fixture: cargo unavailable");
        return;
    };

    let root = workspace_root();
    build_uniffi_bindgen(&root, &cargo);

    let cli = root.join(if cfg!(windows) {
        "target/debug/uniffi-bindgen.exe"
    } else {
        "target/debug/uniffi-bindgen"
    });
    assert!(cli.exists(), "expected built CLI at {cli}");

    let tmp = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let target_dir = Utf8PathBuf::from_path_buf(tmp.path().join("cargo-target")).unwrap();
    let manifest = write_cli_napi_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
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
        "shared/runtime.ts",
        "node/index.ts",
        "components/cli_napi/common/api.ts",
        "components/cli_napi/node/index.ts",
        "components/cli_napi/node/backend-napi.ts",
        "electron/index.ts",
        "electron/preload.cjs",
        "components/cli_napi/electron/index.ts",
        "components/cli_napi/electron/preload.cjs",
        "components/cli_napi/electron/renderer.ts",
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
    let backend_napi =
        std::fs::read_to_string(out_dir.join("components/cli_napi/node/backend-napi.ts")).unwrap();
    assert!(
        backend_napi.contains("UNIFFI_CLI_NAPI_NAPI_PATH")
            && backend_napi.contains("UNIFFI_NAPI_PATH")
            && backend_napi.contains("uniffi-bindgen javascript build-napi")
            && backend_napi.contains("function __uniffiLoadNativeAddon"),
        "backend-napi.ts should expose an actionable env-overridable addon loader:\n{backend_napi}"
    );

    let Some(node) = which_tool("node") else {
        eprintln!("SKIP cli_build_napi_orchestrates_synthetic_fixture: node unavailable");
        return;
    };
    if !node_supports_strip_types(&node) {
        eprintln!(
            "SKIP cli_build_napi_orchestrates_synthetic_fixture: node --experimental-strip-types unavailable"
        );
        return;
    }
    let driver = tmp.path().join("generated-adapter-driver.ts");
    std::fs::write(
        &driver,
        "import * as root from './generated/node/index.ts';\n\
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

    let default_addon = single_node_addon(out_dir.join("node"));
    let override_addon = tmp.path().join("override_cli_napi.node");
    std::fs::copy(&default_addon, &override_addon).unwrap();
    std::fs::remove_file(&default_addon).unwrap();
    let override_run = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(&driver)
        .env("UNIFFI_CLI_NAPI_NAPI_PATH", override_addon)
        .output()
        .expect("failed to run generated N-API adapter driver through env override");
    if !override_run.status.success() {
        panic!(
            "generated N-API adapter driver failed through env override:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&override_run.stdout),
            String::from_utf8_lossy(&override_run.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&override_run.stdout).contains("ok"),
        "generated N-API env override driver did not print ok"
    );
}

#[test]
fn cli_build_napi_orchestrates_two_components_without_feature_flags() {
    let cargo =
        which_tool("cargo").expect("two-component CLI N-API orchestration requires cargo on PATH");
    let root = workspace_root();
    build_uniffi_bindgen(&root, &cargo);
    let cli = root.join(if cfg!(windows) {
        "target/debug/uniffi-bindgen.exe"
    } else {
        "target/debug/uniffi-bindgen"
    });
    assert!(cli.exists(), "expected built CLI at {cli}");

    let tmp = tempfile::tempdir().unwrap();
    let fixture = CompositeFixture::write(tmp.path());
    fixture.build_cdylib();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let artifact_dir = Utf8PathBuf::from_path_buf(tmp.path().join("artifacts")).unwrap();
    let target_dir = Utf8PathBuf::from_path_buf(tmp.path().join("cargo-target-napi")).unwrap();
    let output = Command::new(cli.as_std_path())
        .current_dir(root.as_std_path())
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
    let expected_addon_path = format!("../../../artifacts/node/{composite_addon}");
    for component in CANONICAL_COMPONENTS {
        for flavor in ["node", "electron"] {
            assert!(
                out_dir
                    .join("components")
                    .join(component.namespace)
                    .join(flavor)
                    .join(component.bridge_filename)
                    .exists(),
                "missing {flavor} bridge for {}",
                component.namespace,
            );
        }
        assert!(
            host_lib.contains(&format!(
                "components/{}/node/{}",
                component.namespace, component.bridge_filename
            )),
            "one composite N-API host must include {}:\n{host_lib}",
            component.namespace,
        );
        let node_backend = std::fs::read_to_string(
            out_dir
                .join("components")
                .join(component.namespace)
                .join("node/backend-napi.ts"),
        )
        .unwrap();
        let electron_preload = std::fs::read_to_string(
            out_dir
                .join("components")
                .join(component.namespace)
                .join("electron/preload.cjs"),
        )
        .unwrap();
        for (surface, source) in [
            ("Node backend", node_backend),
            ("Electron preload", electron_preload),
        ] {
            assert!(
                source.contains(&expected_addon_path),
                "{surface} for {} must use the one canonical composite addon path `{expected_addon_path}`:\n{source}",
                component.namespace,
            );
        }
    }
    for platform_entry in ["node/index.ts", "electron/index.ts"] {
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
