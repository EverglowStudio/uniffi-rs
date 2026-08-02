//! CLI orchestration smoke test for the JavaScript wasm build path.
//!
//! This keeps the productized `uniffi-bindgen javascript build-wasm`
//! flow covered without editing the larger `smoke.rs` harness.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
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

fn has_wasm32_target(cargo: &std::path::Path) -> bool {
    if let Ok(out) = Command::new("rustup")
        .args(["target", "list", "--installed"])
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout).contains("wasm32-unknown-unknown");
        }
    }
    let _ = cargo;
    true
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
    let Some(cargo) = which_tool("cargo") else {
        eprintln!("SKIP cli_build_wasm_orchestrates_synthetic_fixture: cargo unavailable");
        return;
    };
    if !has_wasm32_target(&cargo) {
        eprintln!(
            "SKIP cli_build_wasm_orchestrates_synthetic_fixture: wasm32-unknown-unknown target not installed"
        );
        return;
    }
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
    let pkg_dir = Utf8PathBuf::from_path_buf(tmp.path().join("pkg")).unwrap();
    let manifest = write_cli_wasm_fixture(tmp.path());

    let output = Command::new(cli.as_std_path())
        .current_dir(&root)
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
        "shared/runtime.ts",
        "browser/index.ts",
        "browser/index.web.ts",
        "components/cli_wasm/common/api.ts",
        "components/cli_wasm/browser/index.ts",
        "components/cli_wasm/browser/backend-wasm.ts",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing generated JS file: {file}");
    }
    let browser_rs = std::fs::read_dir(out_dir.join("components/cli_wasm/browser"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .expect("the cli_wasm browser component should contain a wasm shim");
    let browser_rs_text = std::fs::read_to_string(&browser_rs).unwrap();
    assert!(
        browser_rs_text.contains("#[wasm_bindgen"),
        "browser/*.rs should be a wasm-bindgen shim"
    );

    let host_manifest = host_dir.join("wasm/Cargo.toml");
    assert!(host_manifest.exists(), "missing generated wasm host crate");
    let host_lib = host_dir.join("wasm/src/lib.rs");
    let host_lib_text = std::fs::read_to_string(&host_lib).unwrap();
    assert!(
        host_lib_text.contains("include!("),
        "generated wasm host crate should include the per-component shim"
    );
    assert!(
        host_lib_text.contains("components/cli_wasm/browser/"),
        "generated wasm host crate should point at the browser shim"
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
    let glue_stem = pkg_entries
        .iter()
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
        .and_then(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .expect("wasm-bindgen output dir should contain a .js glue file");
    let web_entry = std::fs::read_to_string(out_dir.join("browser/index.web.ts")).unwrap();
    assert!(
        web_entry.contains(&format!("{glue_stem}.js"))
            && web_entry.contains(&format!("{glue_stem}_bg.wasm?url"))
            && web_entry.contains("import { cli_wasm } from \"./index.ts\";")
            && web_entry.contains("export const ready: Promise<void> = init();")
            && web_entry.contains("export * from \"./index.ts\";"),
        "browser auto-entrypoint should initialize through the root namespace, import the real wasm-bindgen output, and re-export the explicit entrypoint:\n{web_entry}"
    );

    let node = which_tool("node").expect(
        "Browser readonly-default runtime acceptance requires Node.js with --experimental-strip-types",
    );
    assert!(
        node_supports_strip_types(&node),
        "Browser readonly-default runtime acceptance requires Node.js with --experimental-strip-types"
    );
    // Wrap the real in-process wasm-bindgen named exports in an explicitly
    // frozen object. The generated Browser coordinator must shadow the
    // inherited non-writable default rather than assigning through it.
    let readonly_glue = pkg_dir.join("readonly-default-glue.mjs");
    std::fs::write(
        readonly_glue.as_std_path(),
        format!(
            r#"import init, * as actualGlue from "./{glue_stem}.js";
import {{ readFile }} from "node:fs/promises";

export let defaultCalls = 0;
export const readonlyGlue = Object.freeze({{
    ...actualGlue,
    default: async (_input) => {{
        defaultCalls += 1;
        const bytes = await readFile(new URL("./{glue_stem}_bg.wasm", import.meta.url));
        return init(bytes);
    }},
}});
"#,
        ),
    )
    .unwrap();
    let glue_import = web_entry
        .lines()
        .find(|line| line.starts_with("import * as glue from "))
        .expect("generated Browser entry must import wasm-bindgen glue");
    let wasm_url_import = web_entry
        .lines()
        .find(|line| line.starts_with("import wasmUrl from "))
        .expect("generated Browser entry must import its wasm URL asset");
    let readonly_entry = web_entry
        .replace(
            glue_import,
            "import { readonlyGlue as glue } from \"../../pkg/readonly-default-glue.mjs\";",
        )
        // Node does not understand Vite's `?url` import. The wrapper owns
        // loading the real wasm bytes, so preserving every other generated
        // statement exercises the Browser coordinator.
        .replace(wasm_url_import, "const wasmUrl: unknown = undefined;");
    let readonly_entry_path = out_dir.join("browser/index.web.readonly-default-test.ts");
    std::fs::write(readonly_entry_path.as_std_path(), readonly_entry).unwrap();
    let driver = tmp.path().join("browser-readonly-default-driver.ts");
    std::fs::write(
        &driver,
        r#"import { defaultCalls, readonlyGlue } from "./pkg/readonly-default-glue.mjs";

if (!Object.isFrozen(readonlyGlue)) {
    throw new Error("readonly glue fixture must retain a frozen real named-export object");
}
const browser = await import("./generated/browser/index.web.readonly-default-test.ts");
await browser.ready;
if (defaultCalls !== 1) {
    throw new Error(`expected one default initialization, got ${defaultCalls}`);
}
await Promise.all([browser.init(undefined), browser.init(undefined)]);
if (defaultCalls !== 1) {
    throw new Error(`repeated Browser init invoked default ${defaultCalls} times`);
}
if (browser.cli_wasm.add(2n, 3n) !== 5n) {
    throw new Error("Browser API was not callable through the readonly-default coordinator");
}
console.log("browser readonly default ok");
"#,
    )
    .unwrap();
    let run = Command::new(&node)
        .arg("--experimental-strip-types")
        .arg("--no-warnings")
        .arg(&driver)
        .current_dir(tmp.path())
        .output()
        .expect("failed to run Browser readonly-default driver");
    if !run.status.success() {
        panic!(
            "Browser readonly-default driver failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr),
        );
    }
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("browser readonly default ok"),
        "Browser readonly-default driver did not report success:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
}

#[test]
fn cli_artifacts_wasm_and_mini_orchestrate_two_components_without_feature_flags() {
    let cargo =
        which_tool("cargo").expect("two-component CLI wasm orchestration requires cargo on PATH");
    assert!(
        has_wasm32_target(&cargo),
        "two-component CLI wasm orchestration requires wasm32-unknown-unknown"
    );
    let root = workspace_root();
    build_uniffi_bindgen(&root, &cargo);
    let cli = root.join(if cfg!(windows) {
        "target/debug/uniffi-bindgen.exe"
    } else {
        "target/debug/uniffi-bindgen"
    });

    let tmp = tempfile::tempdir().unwrap();
    let fixture = CompositeFixture::write(tmp.path());
    fixture.build_cdylib();
    let out_dir = Utf8PathBuf::from_path_buf(tmp.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(tmp.path().join("rust_modules")).unwrap();
    let artifact_dir = Utf8PathBuf::from_path_buf(tmp.path().join("artifacts")).unwrap();
    let pkg_dir = artifact_dir.join("browser/pkg");
    let wasm_target_dir = Utf8PathBuf::from_path_buf(tmp.path().join("cargo-target-wasm")).unwrap();
    let output = Command::new(cli.as_std_path())
        .current_dir(root.as_std_path())
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
    let auto_entries = [
        (
            "Browser",
            std::fs::read_to_string(out_dir.join("browser/index.web.ts")).unwrap(),
        ),
        (
            "Mini Program",
            std::fs::read_to_string(out_dir.join("browser/index.mini-program.ts")).unwrap(),
        ),
    ];
    for (label, auto_entry) in &auto_entries {
        assert!(
            auto_entry.contains("import { alpha, beta } from \"./index.ts\";"),
            "two-component {label} auto entry must import both namespaces:\n{auto_entry}"
        );
        assert!(
            auto_entry.contains(&format!("{glue_stem}.js")),
            "two-component {label} auto entry must use the one canonical glue module:\n{auto_entry}"
        );
        for component in CANONICAL_COMPONENTS {
            assert_eq!(
                auto_entry
                    .matches(&format!(
                        "{}.initBackend(initializedGlue)",
                        component.namespace
                    ))
                    .count(),
                1,
                "{label} auto entry must initialize {} once:\n{auto_entry}",
                component.namespace,
            );
        }
    }
    for component in CANONICAL_COMPONENTS {
        assert!(
            out_dir
                .join("components")
                .join(component.namespace)
                .join("browser")
                .join(component.bridge_filename)
                .exists(),
            "missing Browser bridge for {}",
            component.namespace,
        );
    }
    let host_lib = std::fs::read_to_string(host_dir.join("wasm/src/lib.rs")).unwrap();
    for component in CANONICAL_COMPONENTS {
        assert!(
            host_lib.contains(&format!(
                "components/{}/browser/{}",
                component.namespace, component.bridge_filename
            )),
            "one composite wasm host must include {}:\n{host_lib}",
            component.namespace,
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
    let (manifest, source) = shared::write_cli_wasm_fixture(tmp.path());

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
        "shared/runtime.ts",
        "browser/index.ts",
        "components/cli_wasm/common/api.ts",
        "components/cli_wasm/browser/index.ts",
        "components/cli_wasm/browser/backend-wasm.ts",
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
