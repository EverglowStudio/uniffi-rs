//! Opt-in real NAPI/WASM/Chrome/Electron and SDK-compiled Ark unified async.
//!
//! See support/unified_async/README.md for coordinated build/run commands.

#[path = "support/shared.rs"]
mod shared;
mod support;

use shared::{cdylib_filename, locate_node_with_strip_types, required_typescript_compiler};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::*;
use uniffi_bindgen_javascript::host_crates::composite_host_lib_target;

const PACKAGE: &str = "unified-async-fixture";
const SOURCES: &[(&str, &str)] = &[
    ("ark.cjs", include_str!("support/unified_async/ark.cjs")),
    (
        "ark-contract.cjs",
        include_str!("support/unified_async/ark-contract.cjs"),
    ),
    (
        "electron.html",
        "<!doctype html><title>UniFFI S06 Electron</title>",
    ),
    (
        "electron.mjs",
        include_str!("support/unified_async/electron.mjs"),
    ),
    (
        "electron-main.cjs",
        include_str!("support/unified_async/electron-main.cjs"),
    ),
    (
        "contract.mjs",
        include_str!("support/unified_async/contract.mjs"),
    ),
    ("node.mjs", include_str!("support/unified_async/node.mjs")),
    (
        "chrome.mjs",
        include_str!("support/unified_async/chrome.mjs"),
    ),
    (
        "worker.mjs",
        include_str!("support/unified_async/worker.mjs"),
    ),
];

fn preserve_previous(path: &Path) {
    if path.exists() {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!(
            "{}.previous-{stamp}",
            path.file_name().unwrap().to_string_lossy()
        );
        fs::rename(path, path.with_file_name(name)).unwrap();
    }
}

/// Capture child output directly to files (no pipe-buffer deadlock), retain
/// failures, and terminate a hung driver/build with an actionable deadline.
fn run(command: &mut Command, logs: &Path, label: &str, seconds: u64) {
    let stdout_path = logs.join(format!("fixture-{label}.stdout.log"));
    let stderr_path = logs.join(format!("fixture-{label}.stderr.log"));
    let command_path = logs.join(format!("fixture-{label}.command.txt"));
    for path in [&stdout_path, &stderr_path, &command_path] {
        preserve_previous(path);
    }
    fs::write(command_path, format!("{command:?}\n")).unwrap();
    let mut child = command
        .stdout(Stdio::from(fs::File::create(&stdout_path).unwrap()))
        .stderr(Stdio::from(fs::File::create(&stderr_path).unwrap()))
        .spawn()
        .unwrap_or_else(|error| panic!("{label}: spawn failed: {error}"));
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > Duration::from_secs(seconds) {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "deadline: {label} ({seconds}s); logs: {} and {}",
                stdout_path.display(),
                stderr_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    assert!(
        status.success(),
        "{label} failed ({status}); logs: {} and {}\n{}",
        stdout_path.display(),
        stderr_path.display(),
        fs::read_to_string(&stderr_path).unwrap()
    );
    println!("{label}: passed; {}", stdout_path.display());
}

// Run every realm even when one contract fails, preserving each failure.
// Builds still fail immediately, and no success marker is written unless all
// requested realms pass.
fn run_realm(
    command: &mut Command,
    logs: &Path,
    label: &str,
    seconds: u64,
    failures: &mut Vec<String>,
) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(command, logs, label, seconds)
    }))
    .is_err()
    {
        failures.push(label.to_owned());
    }
}

fn build(manifest: &Utf8PathBuf, target: &Utf8PathBuf, wasm: bool, logs: &Path, label: &str) {
    // Each retained temporary package owns a lockfile; resolve it once, then
    // every actual compilation is locked and denies warnings.
    if !manifest.parent().unwrap().join("Cargo.lock").exists() {
        fs::copy(
            workspace_root().join("Cargo.lock"),
            manifest.parent().unwrap().join("Cargo.lock"),
        )
        .unwrap();
        run(
            Command::new("cargo")
                .args([
                    "metadata",
                    "--offline",
                    "--format-version",
                    "1",
                    "--manifest-path",
                ])
                .arg(manifest),
            logs,
            &format!("{label}-lock"),
            180,
        );
    }
    let mut command = Command::new("cargo");
    command
        .args(["build", "--locked", "--manifest-path"])
        .arg(manifest)
        .env("CARGO_TARGET_DIR", target)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("RUSTFLAGS", "-Dwarnings");
    if wasm {
        command.args(["--target", "wasm32-unknown-unknown"]);
    }
    run(&mut command, logs, label, 1800);
}

#[test]
#[ignore = "requires installed Chrome, Electron, TypeScript and DevEco Ark SDK; builds native/WASM hosts"]
fn unified_async_real_cross_end() {
    // Allow independent WASM diagnostics while another writer repairs NAPI.
    // A partial run produces its own marker and cannot certify the full matrix.
    let wasm_only = std::env::var("UNIFFI_ASYNC_WASM_ONLY").as_deref() == Ok("1");
    let node = locate_node_with_strip_types();
    let log_root = std::env::var("UNIFFI_ASYNC_ARTIFACTS")
        .expect("set UNIFFI_ASYNC_ARTIFACTS to a retained fixture artifacts directory");
    let logs = Path::new(&log_root);
    fs::create_dir_all(logs).unwrap();
    let success_path = logs.join(if wasm_only {
        "fixture-success-wasm.json"
    } else {
        "fixture-success.json"
    });
    preserve_previous(&success_path);
    // A unique retained source tree prevents stale generated output satisfying
    // this run; dependency compilation still reuses the existing shared cache.
    let run_dir = tempfile::Builder::new()
        .prefix("fixture-run-")
        .tempdir_in(logs)
        .unwrap()
        .into_path();
    let root = Utf8PathBuf::from_path_buf(run_dir).unwrap();
    let crate_dir = root.join(PACKAGE);
    fs::create_dir_all(crate_dir.join("src")).unwrap();
    fs::write(crate_dir.join("Cargo.toml"), format!(r#"[package]
name = "{PACKAGE}"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
crate-type = ["rlib", "cdylib"]

[dependencies]
uniffi = {{ path = {:?}, features = ["async-runtime", "tokio", "default-async-runtime-tokio", "wasm-unstable-single-threaded"] }}
futures-util = "0.3"
async-trait = "0.1"

[workspace]
resolver = "3"
"#, workspace_root().join("uniffi").as_str())).unwrap();
    fs::write(
        crate_dir.join("src/lib.rs"),
        include_str!("support/unified_async/fixture.rs"),
    )
    .unwrap();
    fs::write(
        crate_dir.join("src/local.rs"),
        include_str!("support/unified_async/local.rs"),
    )
    .unwrap();
    fs::write(
        crate_dir.join("src/actor.rs"),
        include_str!("support/unified_async/actor.rs"),
    )
    .unwrap();
    fs::write(
        crate_dir.join("src/declarations.rs"),
        include_str!("support/unified_async/declarations.rs"),
    )
    .unwrap();
    for (name, source) in SOURCES {
        fs::write(root.join(name), source).unwrap();
    }
    fs::write(logs.join("fixture-latest-run.txt"), format!("{root}\n")).unwrap();

    let native_target = shared_cargo_target_dir("native");
    let native_lock = shared_cargo_target_lock("native");
    build(
        &crate_dir.join("Cargo.toml"),
        &native_target,
        false,
        logs,
        "core-native",
    );
    let lib_path = root.join(cdylib_filename(PACKAGE));
    fs::copy(
        native_target.join("debug").join(cdylib_filename(PACKAGE)),
        &lib_path,
    )
    .unwrap();
    drop(native_lock);
    let generated = root.join("generated");
    // Same metadata/package generation path as shared::generate_stream_tree;
    // its fixture fields are private, so pass this independently built core's
    // inputs directly instead of changing another writer's shared helper.
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let package = generate_package(
        &loader,
        GenerateJsOptions {
            source: lib_path,
            out_dir: generated.clone(),
            package_root: generated.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: crate_dir.join("Cargo.toml"),
                host_crates_dir: generated.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![
                FlavorTarget::Napi,
                FlavorTarget::Wasm,
                FlavorTarget::Electron,
                FlavorTarget::Harmony,
            ],
        },
    )
    .expect("generate unified async package from compiled metadata");
    fs::write(
        generated.join("declarations.mts"),
        include_str!("support/unified_async/declarations.mts"),
    )
    .unwrap();
    fs::write(generated.join("tsconfig.strict.json"), r#"{
      "compilerOptions": { "strict": true, "noEmit": true, "target": "ES2022", "module": "NodeNext", "moduleResolution": "NodeNext", "skipLibCheck": false },
      "include": ["declarations.mts", "shared/**/*.d.ts", "components/**/*.d.ts", "node/**/*.d.ts", "browser/**/*.d.ts", "electron/**/*.d.ts"]
    }"#).unwrap();
    run(
        Command::new(required_typescript_compiler())
            .args(["--noEmit", "-p"])
            .arg(generated.join("tsconfig.strict.json")),
        logs,
        "declarations",
        60,
    );
    let mut runtime_failures = Vec::new();
    let host_name = composite_host_lib_target(PACKAGE);
    if !wasm_only {
        let native_lock = shared_cargo_target_lock("native");
        build(
            &generated.join("native/hosts/napi/Cargo.toml"),
            &native_target,
            false,
            logs,
            "host-napi",
        );
        fs::copy(
            native_target
                .join("debug")
                .join(cdylib_filename(&host_name)),
            generated.join("node").join(format!("{host_name}.node")),
        )
        .unwrap();
        drop(native_lock);
        run_realm(
            Command::new(&node)
                .arg(root.join("node.mjs"))
                .arg("napi")
                .current_dir(&root),
            logs,
            "node-napi",
            75,
            &mut runtime_failures,
        );
    }

    if !wasm_only {
        run_realm(
            Command::new(&node)
                .arg(root.join("ark.cjs"))
                .current_dir(&root),
            logs,
            "ark",
            90,
            &mut runtime_failures,
        );
        run_realm(
            Command::new(&node)
                .arg(root.join("electron.mjs"))
                .current_dir(&root),
            logs,
            "electron",
            90,
            &mut runtime_failures,
        );
    }

    let wasm_target = shared_cargo_target_dir("wasm");
    let wasm_lock = shared_cargo_target_lock("wasm");
    build(
        &generated.join("native/hosts/wasm/Cargo.toml"),
        &wasm_target,
        true,
        logs,
        "host-wasm",
    );
    let wasm = root.join(format!("{host_name}.wasm"));
    fs::copy(
        wasm_target
            .join("wasm32-unknown-unknown/debug")
            .join(format!("{host_name}.wasm")),
        &wasm,
    )
    .unwrap();
    drop(wasm_lock);
    let pkg = generated.join("browser/pkg");
    package
        .emit_wasm_post_link(&wasm, &host_name, WasmPostLinkTarget::Node, &pkg)
        .unwrap();
    run_realm(
        Command::new(&node)
            .arg(root.join("node.mjs"))
            .arg("wasm")
            .current_dir(&root),
        logs,
        "node-wasm",
        75,
        &mut runtime_failures,
    );

    // Keep each post-link target's files disjoint. Both execute the same built
    // .wasm and public package; the Node process has exited before publication.
    fs::rename(&pkg, root.join("node-wasm-pkg")).unwrap();
    package
        .emit_wasm_post_link(&wasm, &host_name, WasmPostLinkTarget::Web, &pkg)
        .unwrap();
    run_realm(
        Command::new(&node)
            .arg(root.join("chrome.mjs"))
            .current_dir(&root),
        logs,
        "chrome",
        165,
        &mut runtime_failures,
    );
    assert!(
        runtime_failures.is_empty(),
        "realm failures: {runtime_failures:?}; retained logs: {}",
        logs.display()
    );
    let realms = if wasm_only {
        vec!["node-wasm", "chrome-window", "chrome-worker"]
    } else {
        vec![
            "node-napi",
            "node-wasm",
            "chrome-window",
            "chrome-worker",
            "electron-renderer",
        ]
    };
    fs::write(
        success_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "run": root,
            "realms": realms,
            "status": "passed",
            "declarations": "strict TypeScript and SDK ArkTS; all real Rust compilations locked and deny warnings",
            "baseline_cases_per_realm": 21,
            "added_cases_per_realm": 23,
            "declaration_cases_per_realm": 13,
            "review_p1_cases_per_realm": 4,
            "actor_cases_per_realm": 6,
            "cases_per_javascript_realm": 44,
            "ark_native_cases": if wasm_only { 0 } else { 17 },
            "ohos_builds": "run.py builds both architectures from this package before fixture-summary.json"
        }))
        .unwrap(),
    )
    .unwrap();
}
