//! Opt-in real native/WASM/Chrome unified-async fixture. No mock backend.
//!
//! See support/unified_async/README.md for coordinated build/run commands.

#[path = "support/shared.rs"]
mod shared;
mod support;

use shared::{cdylib_filename, locate_node_with_strip_types};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::*;
use uniffi_bindgen_javascript::host_crates::composite_host_lib_target;

const PACKAGE: &str = "unified-async-fixture";
const SOURCES: &[(&str, &str)] = &[
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

fn build(manifest: &Utf8PathBuf, target: &Utf8PathBuf, wasm: bool, logs: &Path, label: &str) {
    let mut command = Command::new("cargo");
    command
        .args(["build", "--manifest-path"])
        .arg(manifest)
        .env("CARGO_TARGET_DIR", target)
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        // A08: S00 NAPI host builds did not deny the pinned engine's emitted
        // warnings. Keep that baseline only for this host step; the fixture
        // core, harness and WASM host remain strict.
        .env(
            "RUSTFLAGS",
            if label == "host-napi" {
                ""
            } else {
                "-Dwarnings"
            },
        );
    if wasm {
        command.args(["--target", "wasm32-unknown-unknown"]);
    }
    run(&mut command, logs, label, 1800);
}

#[test]
#[ignore = "builds real NAPI/WASM hosts and runs installed Chrome; coordinate shared Cargo locks"]
fn unified_async_real_cross_end() {
    // Allow independent WASM diagnostics while another writer repairs NAPI.
    // A partial run produces its own marker and cannot certify the full matrix.
    let wasm_only = std::env::var("UNIFFI_ASYNC_WASM_ONLY").as_deref() == Ok("1");
    let node = locate_node_with_strip_types();
    let log_root = std::env::var("UNIFFI_ASYNC_ARTIFACTS")
        .expect("set UNIFFI_ASYNC_ARTIFACTS to the S01 artifacts directory");
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
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Wasm],
        },
    )
    .expect("generate unified async package from compiled metadata");
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
        run(
            Command::new(&node)
                .arg(root.join("node.mjs"))
                .arg("napi")
                .current_dir(&root),
            logs,
            "node-napi",
            75,
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
    run(
        Command::new(&node)
            .arg(root.join("node.mjs"))
            .arg("wasm")
            .current_dir(&root),
        logs,
        "node-wasm",
        75,
    );

    // Keep each post-link target's files disjoint. Both execute the same built
    // .wasm and public package; the Node process has exited before publication.
    fs::rename(&pkg, root.join("node-wasm-pkg")).unwrap();
    package
        .emit_wasm_post_link(&wasm, &host_name, WasmPostLinkTarget::Web, &pkg)
        .unwrap();
    run(
        Command::new(&node)
            .arg(root.join("chrome.mjs"))
            .current_dir(&root),
        logs,
        "chrome",
        165,
    );
    let realms = if wasm_only {
        vec!["node-wasm", "chrome-window", "chrome-worker"]
    } else {
        vec!["node-napi", "node-wasm", "chrome-window", "chrome-worker"]
    };
    fs::write(
        success_path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "run": root,
            "realms": realms,
            "status": "passed",
            "declarations": "direct local Object markers and generated async sync/async methods; general Future/Stream declarations deferred to S03"
        }))
        .unwrap(),
    )
    .unwrap();
}
