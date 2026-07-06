//! CLI orchestration smoke test for the JavaScript wasm build path.
//!
//! This keeps the productized `uniffi-bindgen javascript build-wasm`
//! flow covered without editing the larger `smoke.rs` harness.

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
        "common/api.ts",
        "browser/index.ts",
        "browser/index.web.ts",
        "browser/backend-wasm.ts",
    ] {
        let file = out_dir.join(path);
        assert!(file.exists(), "missing generated JS file: {file}");
    }
    let browser_rs = std::fs::read_dir(out_dir.join("browser"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .find(|p| p.extension().and_then(|e| e.to_str()) == Some("rs"))
        .expect("browser/ should contain a wasm shim");
    let browser_rs_text = std::fs::read_to_string(&browser_rs).unwrap();
    assert!(
        browser_rs_text.contains("#[wasm_bindgen]"),
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
        host_lib_text.contains("browser/"),
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
            && web_entry.contains("export const ready: Promise<void> = init();")
            && web_entry.contains("export * from \"./index.ts\";"),
        "browser auto-entrypoint should import the real wasm-bindgen output and re-export the explicit entrypoint:\n{web_entry}"
    );
}
