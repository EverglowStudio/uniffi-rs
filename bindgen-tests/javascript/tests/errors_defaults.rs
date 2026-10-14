//! Real Rust metadata -> generated declarations -> WASM/N-API consumers.
//! Artifacts are retained under target/u1 for native Swift/Kotlin consumption.
#[path = "support/shared.rs"]
mod shared;
mod support;
use shared::*;
use support::*;
use uniffi_bindgen::bindings::{GenerateOptions, TargetLanguage};
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
fn error_named_extract_has_strict_constructor_types() {
    let _lock = shared_cargo_target_lock("error-named-extract");
    let root = workspace_root().join("target/u1/error-named-extract");
    let fixture = root.join("fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    // The error's own name shadows TypeScript's global Extract utility type.
    std::fs::write(
        fixture.join("src/empty_review.udl"),
        "namespace empty_review {};\n[Error] enum Extract { \"Closed\" };\n",
    )
    .unwrap();
    std::fs::write(fixture.join("src/lib.rs"), "// UDL metadata only\n").unwrap();
    std::fs::write(
        fixture.join("Cargo.toml"),
        format!(
            r#"[package]
name = "empty-review"
version = "0.0.0"
edition = "2021"
[lib]
crate-type = ["lib", "cdylib"]
[dependencies]
uniffi = {{ path = {:?}, default-features = false }}
[workspace]
"#,
            workspace_root().join("uniffi").as_str()
        ),
    )
    .unwrap();
    let generated = root.join("generated");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: fixture.join("src/empty_review.udl"),
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
            flavors: vec![FlavorTarget::Napi],
        },
    )
    .unwrap();
    std::fs::write(
        generated.join("consumer.mts"),
        r#"import { Extract, UniffiError } from "./components/empty_review/index.js";
const closed = new Extract(undefined, "Closed", "Closed");
const variant: "Closed" = closed.variant;
const data: "Closed" = closed.data;
const business: Extract = closed;
const base: UniffiError = closed;
function caught(error: unknown): void {
  if (error instanceof Extract && error.variant === "Closed") {
    const data: "Closed" = error.data;
    // @ts-expect-error unit variant has no payload fields
    error.data.reason;
  }
}
// @ts-expect-error unknown variant
new Extract(undefined, "Unknown", "Closed");
// @ts-expect-error unit payload must match the variant
new Extract(undefined, "Closed", {});
"#,
    )
    .unwrap();
    std::fs::write(generated.join("tsconfig.json"), r#"{
      "compilerOptions": { "strict": true, "noEmit": true, "target": "ES2022", "module": "NodeNext", "moduleResolution": "NodeNext", "skipLibCheck": false },
      "include": ["consumer.mts"]
    }"#).unwrap();
    checked(
        run_required_typescript_check(
            &required_typescript_compiler(),
            generated.join("tsconfig.json").as_std_path(),
        ),
        "strict TS error named Extract",
        &root.join("typescript.log"),
    );
}

#[test]
fn typed_errors_and_defaults_real_backends() {
    let _lock = shared_cargo_target_lock("errors-defaults");
    let root = workspace_root().join("target/u1/errors-defaults");
    let fixture = root.join("fixture");
    std::fs::create_dir_all(fixture.join("src")).unwrap();
    std::fs::write(
        fixture.join("src/lib.rs"),
        include_str!("support/errors_defaults/lib.rs"),
    )
    .unwrap();
    std::fs::write(
        fixture.join("Cargo.toml"),
        format!(
            r#"[package]
name = "errors-defaults"
version = "0.0.0"
edition = "2021"
[lib]
crate-type = ["lib", "cdylib"]
[dependencies]
uniffi = {{ path = {:?}, features = ["wasm-unstable-single-threaded"] }}
[workspace]
"#,
            workspace_root().join("uniffi").as_str()
        ),
    )
    .unwrap();
    // The copied cdylib retains Cargo's absolute install name on macOS.
    // Keep this build separate so the N-API host cannot replace that library
    // with a differently feature-unified build before Swift consumes it.
    let native_target = shared_cargo_target_dir("errors-defaults-fixture");
    let native_lock = shared_cargo_target_lock("errors-defaults-fixture");
    checked(
        run_cargo_build(&fixture.join("Cargo.toml"), &[], &native_target).unwrap(),
        "Rust fixture",
        &root.join("fixture.log"),
    );
    let source = native_target
        .join("debug")
        .join(cdylib_filename("errors-defaults"));
    // Keep a stable metadata input; the linked library's install name remains in target.
    let library = root.join(cdylib_filename("errors-defaults"));
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
            flavors: vec![FlavorTarget::Wasm, FlavorTarget::Napi],
        },
    )
    .unwrap();
    for (language, directory) in [
        (TargetLanguage::Swift, "swift"),
        (TargetLanguage::Kotlin, "kotlin"),
    ] {
        uniffi_bindgen::bindings::generate(GenerateOptions {
            languages: vec![language],
            source: library.clone(),
            out_dir: root.join(directory),
            metadata_no_deps: true,
            ..Default::default()
        })
        .unwrap();
    }
    let compiler = required_typescript_compiler();
    let node = locate_node_with_strip_types();
    let host_name = composite_host_lib_target("errors-defaults");
    for (flavor, entry) in [("wasm", "browser"), ("napi", "node")] {
        let driver = include_str!("support/errors_defaults/consumer.ts").replace("ENTRY", entry);
        let driver = if flavor == "wasm" {
            format!("import {{ ready }} from './browser/index.js';\nawait ready;\n{driver}")
        } else {
            driver
        };
        std::fs::write(generated.join(format!("{flavor}-consumer.mts")), driver).unwrap();
    }
    std::fs::write(generated.join("tsconfig.json"), r#"{
      "compilerOptions": { "strict": true, "noEmit": true, "target": "ES2022", "module": "NodeNext", "moduleResolution": "NodeNext", "skipLibCheck": false },
      "include": ["*-consumer.mts"]
    }"#).unwrap();
    checked(
        run_required_typescript_check(&compiler, generated.join("tsconfig.json").as_std_path()),
        "strict TS 5.9.3",
        &root.join("typescript.log"),
    );
    for (flavor, kind) in [("wasm", "wasm"), ("napi", "native")] {
        let target = shared_cargo_target_dir(kind);
        let _lock = shared_cargo_target_lock(kind);
        let extra: &[&str] = if flavor == "wasm" {
            &["--target", "wasm32-unknown-unknown"]
        } else {
            &[]
        };
        checked(
            run_cargo_build(
                &generated.join(format!("native/hosts/{flavor}/Cargo.toml")),
                extra,
                &target,
            )
            .unwrap(),
            flavor,
            &root.join(format!("{flavor}-build.log")),
        );
        if flavor == "wasm" {
            let wasm = target.join(format!("wasm32-unknown-unknown/debug/{host_name}.wasm"));
            package
                .emit_wasm_post_link(
                    &wasm,
                    &host_name,
                    WasmPostLinkTarget::Node,
                    &generated.join("browser/pkg"),
                )
                .unwrap();
        } else {
            std::fs::copy(
                target.join("debug").join(cdylib_filename(&host_name)),
                generated.join(format!("node/{host_name}.node")),
            )
            .unwrap();
        }
        let output = Command::new(&node)
            .args(["--experimental-strip-types", "--no-warnings"])
            .arg(generated.join(format!("{flavor}-consumer.mts")))
            .current_dir(&generated)
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("ERRORS_DEFAULTS_OK"),
            "consumer missing completion: {:?}",
            output
        );
        checked(output, flavor, &root.join(format!("{flavor}-run.log")));
    }
}
