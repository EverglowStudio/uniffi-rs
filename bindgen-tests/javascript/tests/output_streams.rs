//! The native output-stream probe fixture, consumed through a real WASM package.
//! Artifacts are retained under target/u2 for independent review.
#[path = "support/shared.rs"]
mod shared;
mod support;
use shared::*;
use support::*;
use uniffi_bindgen_javascript::host_crates::composite_host_lib_target;

fn checked(output: std::process::Output, label: &str, log: &camino::Utf8Path) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(log, &text).unwrap();
    assert!(output.status.success(), "{label}: {text}");
    println!("{label}: passed ({log})");
}

#[test]
fn output_stream_probe_real_wasm() {
    let _lock = shared_cargo_target_lock("output-streams-probe");
    let root = workspace_root().join("target/u2/output-streams-probe");
    let fixture = root.join("fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    std::fs::write(
        fixture.join("src/lib.rs"),
        include_str!("../../../fixtures/output-streams/src/lib.rs"),
    )
    .unwrap();
    std::fs::write(
        fixture.join("Cargo.toml"),
        format!(
            r#"[package]
name = "output-streams-probe"
version = "0.0.0"
edition = "2021"
[lib]
crate-type = ["lib", "cdylib"]
[dependencies]
thiserror = "2"
uniffi = {{ path = {:?}, features = ["wasm-unstable-single-threaded"] }}
[workspace]
"#,
            workspace_root().join("uniffi").as_str()
        ),
    )
    .unwrap();
    // Compile the same probe source for metadata. The native language harnesses
    // consume the original fixture; this isolated crate enables the WASM feature.
    let native_target = shared_cargo_target_dir("output-streams-probe-fixture");
    let native_lock = shared_cargo_target_lock("output-streams-probe-fixture");
    checked(
        run_cargo_build(&fixture.join("Cargo.toml"), &[], &native_target).unwrap(),
        "Rust fixture",
        &root.join("fixture.log"),
    );
    let source = native_target
        .join("debug")
        .join(cdylib_filename("output-streams-probe"));
    // Keep a stable metadata input; the linked library's install name remains in target.
    let library = root.join(cdylib_filename("output-streams-probe"));
    std::fs::copy(source, &library).unwrap();
    drop(native_lock);
    let generated = root.join("generated");
    if generated.exists() {
        std::fs::remove_dir_all(&generated).unwrap();
    }
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    let package = generate_package(
        &loader,
        GenerateJsOptions {
            source: library.clone(),
            out_dir: generated.clone(),
            package_root: generated.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: fixture.join("Cargo.toml"),
                host_crates_dir: generated.join("native/hosts"),
                logical_host_crates_dir: None,
            },
            flavors: vec![FlavorTarget::Wasm],
        },
    )
    .unwrap();
    let compiler = required_typescript_compiler();
    let node = locate_node_with_strip_types();
    let host_name = composite_host_lib_target("output-streams-probe");
    std::fs::write(
        generated.join("wasm-consumer.mts"),
        include_str!("support/output_streams/consumer.mts"),
    )
    .unwrap();
    std::fs::write(generated.join("tsconfig.json"), r#"{
      "compilerOptions": { "strict": true, "noEmit": true, "target": "ES2022", "module": "NodeNext", "moduleResolution": "NodeNext", "skipLibCheck": false },
      "include": ["*-consumer.mts"]
    }"#).unwrap();
    checked(
        run_required_typescript_check(&compiler, generated.join("tsconfig.json").as_std_path()),
        "strict TS 5.9.3",
        &root.join("typescript.log"),
    );
    let host_target = shared_cargo_target_dir("wasm");
    let _lock = shared_cargo_target_lock("wasm");
    checked(
        run_cargo_build(
            &generated.join("native/hosts/wasm/Cargo.toml"),
            &["--target", "wasm32-unknown-unknown"],
            &host_target,
        )
        .unwrap(),
        "WASM build",
        &root.join("wasm-build.log"),
    );
    package
        .emit_wasm_post_link(
            &host_target.join(format!("wasm32-unknown-unknown/debug/{host_name}.wasm")),
            &host_name,
            WasmPostLinkTarget::Node,
            &generated.join("browser/pkg"),
        )
        .unwrap();
    let output = Command::new(&node)
        .args([
            "--experimental-strip-types",
            "--unhandled-rejections=strict",
        ])
        .arg(generated.join("wasm-consumer.mts"))
        .current_dir(&generated)
        .output()
        .unwrap();
    let completed = String::from_utf8_lossy(&output.stdout).contains("OUTPUT_STREAMS_WASM_OK");
    checked(output, "WASM consumer", &root.join("wasm-run.log"));
    assert!(completed, "consumer missing completion");
}
