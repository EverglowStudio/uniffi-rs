//! Generated host-crate tree, compilation, and flavor-gating tests.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::*;
use uniffi_bindgen_javascript::HostCrateOptions;

#[test]
fn emits_host_crate_tree_when_opted_in() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(out.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    generate_arithmetic_with_host_crates(&out_dir, &host_dir);

    for name in [
        "wasm/Cargo.toml",
        "wasm/src/lib.rs",
        "napi/Cargo.toml",
        "napi/src/lib.rs",
        "napi/build.rs",
    ] {
        let p = host_dir.join(name);
        assert!(p.exists(), "missing host-crate file: {p}");
    }

    let wasm_toml = std::fs::read_to_string(host_dir.join("wasm/Cargo.toml")).unwrap();
    assert!(wasm_toml.contains("name = \"uniffi-example-arithmetic-uniffi-js-host\""));
    assert!(wasm_toml.contains("name = \"uniffi_example_arithmetic_uniffi_js_host\""));
    assert!(wasm_toml.contains("crate-type = [\"cdylib\""));
    assert!(wasm_toml.contains("wasm-bindgen ="));
    assert!(wasm_toml.contains("wasm-bindgen-futures"));
    assert!(!wasm_toml.contains("serde-wasm-bindgen"));
    assert!(!wasm_toml.contains("serde ="));
    assert!(wasm_toml.contains("js-sys"));
    assert!(
        wasm_toml.contains("arithmetical = { package = \"uniffi-example-arithmetic\", path ="),
        "wasm Cargo.toml should path-depend on core crate, got:\n{wasm_toml}"
    );
    assert!(
        wasm_toml.contains("[workspace]"),
        "wasm host crate must declare its own [workspace] so it doesn't \
         need the downstream workspace edited first"
    );

    let wasm_lib = std::fs::read_to_string(host_dir.join("wasm/src/lib.rs")).unwrap();
    assert!(
        wasm_lib.contains("include!(")
            && wasm_lib.contains("components/arithmetic/browser/arithmetical.rs"),
        "wasm lib.rs must include the generated component browser bridge, got:\n{wasm_lib}"
    );

    let napi_toml = std::fs::read_to_string(host_dir.join("napi/Cargo.toml")).unwrap();
    assert!(napi_toml.contains("name = \"uniffi-example-arithmetic-uniffi-js-host\""));
    assert!(napi_toml.contains("name = \"uniffi_example_arithmetic_uniffi_js_host\""));
    assert!(napi_toml.contains("crate-type = [\"cdylib\"]"));
    assert!(napi_toml.contains("napi = "));
    assert!(napi_toml.contains("napi-derive"));
    assert!(napi_toml.contains("napi-build"));
    assert!(napi_toml.contains("async-trait = \"0.1\""));
    assert!(
        napi_toml.contains("arithmetical = { package = \"uniffi-example-arithmetic\", path ="),
        "napi Cargo.toml should path-depend on core crate, got:\n{napi_toml}"
    );
    assert!(napi_toml.contains("[workspace]"));

    let napi_lib = std::fs::read_to_string(host_dir.join("napi/src/lib.rs")).unwrap();
    assert!(
        napi_lib.contains("include!(")
            && napi_lib.contains("components/arithmetic/node/arithmetical.rs"),
        "napi lib.rs must include the generated component node bridge, got:\n{napi_lib}"
    );

    let build_rs = std::fs::read_to_string(host_dir.join("napi/build.rs")).unwrap();
    assert!(build_rs.contains("napi_build::setup"));
}

#[test]
fn does_not_emit_host_crates_by_default() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);
    assert!(!out_dir.join("rust_modules").exists());
    assert!(!out_dir.join("wasm/Cargo.toml").exists());
    assert!(!out_dir.join("napi/Cargo.toml").exists());
}

#[test]
fn emits_ohos_host_crate_when_harmony_is_requested() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(out.path().join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let manifest = workspace_root().join("examples/arithmetic/Cargo.toml");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Harmony],
        },
    )
    .expect("harmony host-crate generation should succeed");

    assert!(host_dir.join("ohos/Cargo.toml").exists());
    assert!(host_dir.join("ohos/build.rs").exists());
    assert!(host_dir.join("ohos/src/lib.rs").exists());
    assert!(
        !host_dir.join("napi/Cargo.toml").exists(),
        "harmony-only generation must not emit ordinary napi host crate"
    );
    assert!(
        !host_dir.join("wasm/Cargo.toml").exists(),
        "harmony-only generation must not emit wasm host crate"
    );

    let toml = std::fs::read_to_string(host_dir.join("ohos/Cargo.toml")).unwrap();
    for required in [
        "name = \"uniffi-example-arithmetic-uniffi-js-host\"",
        "name = \"uniffi_example_arithmetic_uniffi_js_host\"",
        "napi-ohos = { version = \"1.1.6\"",
        "napi-derive-ohos = { version = \"1.1.6\", default-features = false, features = [\"strict\", \"type-def\"] }",
        "napi-build-ohos = \"1.1.6\"",
        "features = [\"napi8\", \"tokio_rt\"]",
        "[workspace]",
    ] {
        assert!(
            toml.contains(required),
            "OHOS Cargo.toml missing `{required}`:\n{toml}"
        );
    }
    assert!(
        !toml.contains("napi-derive-backend-ohos"),
        "OHOS host must not add an independent type-definition backend:\n{toml}"
    );
    for forbidden in ["/Users/frain/Developer/refer/uni/ohos-rs", "ohos-rs/crates"] {
        assert!(
            !toml.contains(forbidden),
            "default OHOS host crate must not use local ohos-rs path deps `{forbidden}`:\n{toml}"
        );
    }
    let build_rs = std::fs::read_to_string(host_dir.join("ohos/build.rs")).unwrap();
    assert!(build_rs.contains("napi_build_ohos::setup"));
    assert!(!build_rs.contains("std::fs::write"));
    assert!(!build_rs.contains("ohos-extra-types.d.ts"));
    let bundle: serde_json::Value = serde_json::from_slice(
        &std::fs::read(host_dir.join("ohos/uniffi-ohos-facade-bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle["hostBundleSchemaVersion"], 3);
    assert_eq!(bundle["components"][0]["namespace"], "arithmetic");
    assert_eq!(
        bundle["components"][0]["nativeExportPrefix"],
        "ffi_arithmetical"
    );
    assert!(bundle["fingerprint"]
        .as_str()
        .is_some_and(|value| value.len() == 64));
    assert!(bundle["typeSidecars"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["file"] == "arithmetical.ohos-extra-types.d.ts"));
    let lib_rs = std::fs::read_to_string(host_dir.join("ohos/src/lib.rs")).unwrap();
    assert!(
        lib_rs.contains("include!(")
            && lib_rs.contains("components/arithmetic/harmony/arithmetical.rs"),
        "OHOS lib.rs must include generated component harmony bridge:\n{lib_rs}"
    );
}

// Build a tiny synthetic downstream core crate + UDL inside `root`
// whose public function signatures match what the JS bridge codegen
// emits. This lets the compile-level tests below run `cargo check`
// without depending on any fixture that relies on uniffi scaffolding
// macros or private helper fns.
#[test]
fn host_crates_napi_passes_cargo_check() {
    let tmp = tempfile::tempdir().unwrap();
    let (_out, host_dir) = generate_synthetic_with_host_crates(tmp.path());
    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-napi");
    let output = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_passes_cargo_check: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn host_crates_napi_and_ohos_compile_float32_record_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let (out_dir, host_dir) = generate_float32_record_hosts(tmp.path());

    for bridge in [
        out_dir.join("components/float32_record_core/node/float32_record_core.rs"),
        out_dir.join("components/float32_record_core/harmony/float32_record_core.rs"),
    ] {
        let source = std::fs::read_to_string(&bridge).unwrap();
        assert!(
            source.contains("pub speed: f64")
                && source.contains("speed: value.speed as f32")
                && source.contains("speed: value.speed as f64"),
            "float32 bridge must adapt JS number at the FFI boundary: {source}"
        );
        assert!(
            !source.contains("pub speed: f32"),
            "the host crate must not ask N-API to marshal f32 directly: {source}"
        );
        assert!(
            source.contains("pub fn async_service_greet(")
                && source.contains("__uniffi_env: Env,")
                && source
                    .contains("handle: napi::bindgen_prelude::ClassInstance<'_, AsyncService>,")
                && source.contains("let __uniffi_core = (*(handle)).0.clone();")
                && source.contains("drop(handle);")
                && source.contains("spawn_future(__uniffi_future)"),
            "async object receivers must lower before entering the Send promise future: {source}"
        );
        assert!(
            !source.contains("pub async fn async_service_greet("),
            "an async N-API function would capture ClassInstance before its body can drop it: {source}"
        );
    }

    let napi_manifest = host_dir.join("napi/Cargo.toml");
    let napi_target = tmp.path().join("cargo-target-float32-napi");
    let napi_output = run_cargo_check(&napi_manifest, &[], &napi_target)
        .expect("cargo must be available for the N-API f32 host regression");
    assert!(
        napi_output.status.success(),
        "cargo check on f32 N-API host crate failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&napi_output.stdout),
        String::from_utf8_lossy(&napi_output.stderr),
    );

    let target = "aarch64-unknown-linux-ohos";
    let Some(target_libdir) = cargo_target_libdir(target)
        .expect("the rustc selected by cargo must be available for the OHOS f32 host regression")
    else {
        eprintln!(
            "SKIP host_crates_napi_and_ohos_compile_float32_record_fixture: {target} standard library is not installed for Cargo's rust toolchain"
        );
        return;
    };
    assert!(
        target_libdir.is_dir(),
        "Cargo's target libdir must exist before compiling the OHOS host: {}",
        target_libdir.display()
    );

    let ohos_manifest = host_dir.join("ohos/Cargo.toml");
    let ohos_target = tmp.path().join("cargo-target-float32-ohos");
    let ohos_output = run_cargo_check(&ohos_manifest, &["--target", target], &ohos_target)
        .expect("cargo must be available for the OHOS f32 host regression");
    assert!(
        ohos_output.status.success(),
        "cargo check on f32 OHOS host crate failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ohos_output.stdout),
        String::from_utf8_lossy(&ohos_output.stderr),
    );
}

#[test]
fn host_crates_wasm_passes_cargo_check() {
    // Skip if wasm32 target not installed.
    let probe = Command::new("rustc")
        .args([
            "--print",
            "target-libdir",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .output();
    match probe {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("SKIP host_crates_wasm_passes_cargo_check: wasm32-unknown-unknown target not installed");
            return;
        }
    }

    let tmp = tempfile::tempdir().unwrap();
    let (_out, host_dir) = generate_synthetic_with_host_crates(tmp.path());
    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-wasm");
    let output = match run_cargo_check(
        &manifest,
        &["--target", "wasm32-unknown-unknown"],
        &target_dir,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_wasm_passes_cargo_check: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on wasm host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

// ---------------------------------------------------------------------
// Host-crate flavor gating.
// ---------------------------------------------------------------------

#[test]
fn host_crates_wasm_only_skips_napi() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Wasm]);
    assert!(host_dir.join("wasm/Cargo.toml").exists());
    assert!(host_dir.join("wasm/src/lib.rs").exists());
    assert!(
        !host_dir.join("napi").exists(),
        "napi host crate must not be emitted when only --flavor wasm is requested"
    );
}

#[test]
fn host_crates_napi_only_skips_wasm() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Napi]);
    assert!(host_dir.join("napi/Cargo.toml").exists());
    assert!(host_dir.join("napi/src/lib.rs").exists());
    assert!(
        !host_dir.join("wasm").exists(),
        "wasm host crate must not be emitted when only --flavor napi is requested"
    );
}

#[test]
fn host_crates_electron_only_emits_napi_and_skips_wasm() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Electron]);
    assert!(
        host_dir.join("napi/Cargo.toml").exists(),
        "electron must reuse the napi host crate"
    );
    assert!(
        !host_dir.join("wasm").exists(),
        "wasm host crate must not be emitted when only --flavor electron is requested"
    );
}

#[test]
fn host_crates_wasm_only_passes_cargo_check() {
    // Regression proof for the broken scenario before flavor gating:
    // `--flavor wasm --emit-host-crates` would also emit a napi
    // host crate that `include!`-ed a non-existent `out/node/*.rs`.
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_synthetic_gated(tmp.path(), vec![FlavorTarget::Wasm]);
    assert!(!host_dir.join("napi").exists());

    // Skip if wasm32 target not installed.
    let probe = Command::new("rustc")
        .args([
            "--print",
            "target-libdir",
            "--target",
            "wasm32-unknown-unknown",
        ])
        .output();
    match probe {
        Ok(o) if o.status.success() => {}
        _ => {
            eprintln!("SKIP host_crates_wasm_only_passes_cargo_check: wasm32 target not installed");
            return;
        }
    }

    let manifest = host_dir.join("wasm/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-wasm-only");
    let output = match run_cargo_check(
        &manifest,
        &["--target", "wasm32-unknown-unknown"],
        &target_dir,
    ) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_wasm_only_passes_cargo_check: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on wasm-only host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn host_crates_napi_compiles_temporal_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_temporal_napi_host(tmp.path());

    let bridge = std::fs::read_to_string(
        Utf8PathBuf::from_path_buf(
            tmp.path()
                .join("generated/components/napi_temporal_core/node/napi_temporal_core.rs"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        bridge.contains("__UniffiTimestamp") && bridge.contains("__UniffiDuration"),
        "temporal napi bridge should emit explicit wrappers, got:\n{bridge}"
    );
    assert!(
        !bridge.contains("timestamp/duration are not supported"),
        "temporal napi bridge must not reject timestamp/duration anymore:\n{bridge}"
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let target_dir = tmp.path().join("cargo-target-temporal-napi-check");
    let output = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_compiles_temporal_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on temporal napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    let electron_preload = std::fs::read_to_string(
        tmp.path()
            .join("generated/components/napi_temporal_core/electron/preload.cjs"),
    )
    .unwrap();
    assert!(
        !electron_preload.contains("unsupported"),
        "electron preload should remain on the supported temporal path:\n{electron_preload}"
    );
}

#[test]
fn host_crates_napi_compiles_enum_callback_async_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());

    // Sanity check: the generated bridge actually uses the newer
    // napi-rs surface whose compatibility is the point of this test.
    let bridge = std::fs::read_to_string(
        Utf8PathBuf::from_path_buf(
            tmp.path()
                .join("generated/components/napi_compat/node/napi_compat.rs"),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(
        bridge.contains("discriminant = \"type\""),
        "rich fixture should exercise #[napi(discriminant = \"type\")]"
    );
    assert!(
        bridge.contains("string_enum"),
        "rich fixture should exercise #[napi(string_enum)] for flat enums"
    );
    assert!(
        bridge.contains("ThreadsafeFunction"),
        "rich fixture should exercise ThreadsafeFunction"
    );
    assert!(
        bridge.contains("napi::bindgen_prelude::BigInt"),
        "rich fixture should use napi::BigInt for u64/i64, got:\n{bridge}"
    );
    assert!(
        bridge.contains("pub fn async_counter_value(")
            && bridge.contains("__uniffi_env: Env,")
            && bridge.contains("counter: napi::bindgen_prelude::ClassInstance<'_, Counter>,")
            && bridge.contains("napi::bindgen_prelude::Result<")
            && bridge.contains("napi::bindgen_prelude::PromiseRaw<'static, napi::bindgen_prelude::BigInt>")
            && bridge.contains("let __uniffi_counter = (*(counter)).0.clone();")
            && bridge.contains(".spawn_future(async move"),
        "async function with object args should lower ClassInstance before spawning a Promise:\n{bridge}"
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("napi = { version = \"3"),
        "napi host crate template must default to napi 3.x, got:\n{cargo_toml}"
    );
    assert!(
        cargo_toml.contains("napi-derive = { version = \"3") && cargo_toml.contains("type-def"),
        "napi-derive must default to 3.x with type-def, got:\n{cargo_toml}"
    );

    let target_dir = tmp.path().join("cargo-target-napi-rich");
    let output = match run_cargo_check(&manifest, &[], &target_dir) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("SKIP host_crates_napi_compiles_enum_callback_async_fixture: cargo unavailable: {e}");
            return;
        }
    };
    if !output.status.success() {
        panic!(
            "cargo check on rich napi host crate failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

#[test]
fn composite_host_crates_are_deterministic_complete_and_compile() {
    let tmp = tempfile::tempdir().unwrap();
    let forward_root = tmp.path().join("forward");
    let reverse_root = tmp.path().join("reverse");

    let forward = CompositeFixture::write(&forward_root);
    forward.build_cdylib();
    let forward_out = Utf8PathBuf::from_path_buf(forward_root.join("generated")).unwrap();
    let forward_hosts = Utf8PathBuf::from_path_buf(forward_root.join("rust_modules")).unwrap();
    forward.generate(
        &forward_out,
        Some(forward_hosts.clone()),
        vec![
            FlavorTarget::Wasm,
            FlavorTarget::Napi,
            FlavorTarget::Electron,
            FlavorTarget::Harmony,
        ],
    );

    let reverse = CompositeFixture::write_reversed(&reverse_root);
    reverse.build_cdylib();
    let reverse_out = Utf8PathBuf::from_path_buf(reverse_root.join("generated")).unwrap();
    let reverse_hosts = Utf8PathBuf::from_path_buf(reverse_root.join("rust_modules")).unwrap();
    reverse.generate(
        &reverse_out,
        Some(reverse_hosts.clone()),
        vec![
            FlavorTarget::Wasm,
            FlavorTarget::Napi,
            FlavorTarget::Electron,
            FlavorTarget::Harmony,
        ],
    );

    assert_eq!(
        regular_tree_snapshot(forward_out.as_std_path(), forward.root().as_std_path()),
        regular_tree_snapshot(reverse_out.as_std_path(), reverse.root().as_std_path()),
        "reverse Cargo dependency/re-export order must not change generated source",
    );
    assert_eq!(
        regular_tree_snapshot(forward_hosts.as_std_path(), forward.root().as_std_path()),
        regular_tree_snapshot(reverse_hosts.as_std_path(), reverse.root().as_std_path()),
        "reverse Cargo dependency/re-export order must not change host crates or OHOS bundle",
    );

    let host_package = "composite-core-uniffi-js-host";
    let host_target = "composite_core_uniffi_js_host";
    for flavor in ["wasm", "napi", "ohos"] {
        let manifest = forward.host_manifest_path(&forward_hosts, flavor);
        let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
        assert!(
            cargo_toml.contains(&format!("name = \"{host_package}\"")),
            "{flavor} host must use the one composite package:\n{cargo_toml}"
        );
        assert!(
            cargo_toml.contains(&format!("name = \"{host_target}\"")),
            "{flavor} host must use the one composite lib target:\n{cargo_toml}"
        );
        for component in CANONICAL_COMPONENTS {
            assert!(
                cargo_toml.contains(&format!(
                    "{} = {{ package = \"{}\", path =",
                    component.crate_name, component.package_name,
                )),
                "{flavor} host must directly depend on {}:\n{cargo_toml}",
                component.package_name,
            );
            let dependency_line = cargo_toml
                .lines()
                .find(|line| line.starts_with(&format!("{} = ", component.crate_name)))
                .unwrap_or_else(|| {
                    panic!(
                        "{flavor} host has no dependency line for {}:\n{cargo_toml}",
                        component.crate_name
                    )
                });
            assert!(
                dependency_line.contains("default-features = false"),
                "{flavor} host component dependencies must not enable unrelated component defaults:\n{cargo_toml}",
            );
        }

        let lib_rs =
            std::fs::read_to_string(forward_hosts.join(flavor).join("src/lib.rs")).unwrap();
        for component in CANONICAL_COMPONENTS {
            let encoded_crate_root = component
                .crate_name
                .as_bytes()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            assert!(
                lib_rs.contains(&format!("mod __uniffi_component_{encoded_crate_root}"))
                    && lib_rs.contains(&format!(
                        "components/{}/{}/{}",
                        component.namespace,
                        match flavor {
                            "wasm" => "browser",
                            "napi" => "node",
                            "ohos" => "harmony",
                            _ => unreachable!(),
                        },
                        component.bridge_filename,
                    )),
                "{flavor} host must include {} in an isolated module:\n{lib_rs}",
                component.namespace,
            );
        }
    }

    let bundle: serde_json::Value = serde_json::from_slice(
        &std::fs::read(forward_hosts.join("ohos/uniffi-ohos-facade-bundle.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(bundle["hostBundleSchemaVersion"], 3);
    assert_eq!(bundle["packageName"], host_package);
    assert_eq!(bundle["libTarget"], host_target);
    assert_eq!(bundle["components"].as_array().map(Vec::len), Some(2));
    assert_eq!(bundle["contracts"].as_array().map(Vec::len), Some(2));
    assert_eq!(bundle["typeSidecars"].as_array().map(Vec::len), Some(2));
    assert_eq!(
        bundle["fingerprint"].as_str().map(str::len),
        Some(64),
        "OHOS bundle fingerprint must be an exact SHA-256",
    );

    let components = bundle["components"].as_array().unwrap();
    let identity_components = components
        .iter()
        .map(|component| {
            (
                component["component"].as_str().unwrap().to_string(),
                component["namespace"].as_str().unwrap().to_string(),
                component["nativeExportPrefix"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        identity_components,
        vec![
            (
                "alpha_core".to_string(),
                "alpha".to_string(),
                "ffi_alpha_core".to_string()
            ),
            (
                "beta_core".to_string(),
                "beta".to_string(),
                "ffi_beta_core".to_string()
            ),
        ],
        "bundle components must preserve stable crate/namespace/native-prefix ownership",
    );
    assert_eq!(
        bundle["hostCompositeIdentity"],
        uniffi_bindgen_javascript::host_crates::composite_host_identity(
            host_package,
            host_target,
            &identity_components,
        )
        .unwrap(),
        "bundle identity must be recomputable from its exact component tuple",
    );

    for component in components {
        let namespace = component["namespace"].as_str().unwrap();
        let crate_name = component["component"].as_str().unwrap();
        let contract_file = component["contractFile"].as_str().unwrap();
        let identity_export = component["identityExport"].as_str().unwrap();
        assert_eq!(component["contractSha256"].as_str().map(str::len), Some(64));
        assert!(
            std::fs::read_to_string(
                forward_out
                    .join("components")
                    .join(namespace)
                    .join("harmony")
                    .join(contract_file),
            )
            .unwrap()
            .contains(crate_name),
            "each bundle contract must be the exact generated component contract"
        );
        let sidecar = forward_out
            .join("components")
            .join(namespace)
            .join("harmony")
            .join(format!("{crate_name}.ohos-extra-types.d.ts"));
        let bridge = forward.generated_bridge_path(
            &forward_out,
            component_for_namespace(namespace),
            "harmony",
        );
        assert!(
            std::fs::read_to_string(&sidecar)
                .unwrap()
                .contains(identity_export)
                && std::fs::read_to_string(&bridge)
                    .unwrap()
                    .contains(identity_export),
            "OHOS sidecar and bridge must carry the same component identity export",
        );
    }

    let napi_target = forward_root.join("target-napi");
    let host_feature_args = ["--features", "composite_core/host-gate"];
    let napi = run_cargo_check(
        &forward.host_manifest_path(&forward_hosts, "napi"),
        &host_feature_args,
        &napi_target,
    )
    .expect("cargo must be available for composite N-API host checking");
    assert!(
        napi.status.success(),
        "composite N-API host check failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&napi.stdout),
        String::from_utf8_lossy(&napi.stderr),
    );

    let wasm_target = forward_root.join("target-wasm");
    let wasm = run_cargo_check(
        &forward.host_manifest_path(&forward_hosts, "wasm"),
        &[
            "--target",
            "wasm32-unknown-unknown",
            "--features",
            "composite_core/host-gate",
        ],
        &wasm_target,
    )
    .expect("cargo must be available for composite wasm host checking");
    assert!(
        wasm.status.success(),
        "composite wasm host check failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&wasm.stdout),
        String::from_utf8_lossy(&wasm.stderr),
    );

    let ohos_target = forward_root.join("target-ohos");
    let ohos = run_cargo_check(
        &forward.host_manifest_path(&forward_hosts, "ohos"),
        &[
            "--target",
            "aarch64-unknown-linux-ohos",
            "--features",
            "composite_core/host-gate",
        ],
        &ohos_target,
    )
    .expect("cargo must be available for composite OHOS host checking");
    assert!(
        ohos.status.success(),
        "composite OHOS host check failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&ohos.stdout),
        String::from_utf8_lossy(&ohos.stderr),
    );
}

fn component_for_namespace(namespace: &str) -> CompositeComponent {
    CANONICAL_COMPONENTS
        .into_iter()
        .find(|component| component.namespace == namespace)
        .unwrap_or_else(|| panic!("unexpected composite namespace `{namespace}`"))
}

fn regular_tree_snapshot(
    root: &std::path::Path,
    fixture_root: &std::path::Path,
) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(
        root: &std::path::Path,
        dir: &std::path::Path,
        output: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
    ) {
        let mut entries = std::fs::read_dir(dir)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let file_type = entry.file_type().unwrap();
            if file_type.is_dir() {
                visit(root, &path, output);
            } else if file_type.is_file() {
                output.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                ));
            } else {
                panic!(
                    "composite generated tree contains an unsupported non-file entry: {}",
                    path.display()
                );
            }
        }
    }

    let mut output = Vec::new();
    visit(root, root, &mut output);
    let fixture_root = fixture_root.to_string_lossy();
    output
        .into_iter()
        .map(|(path, contents)| {
            let contents = match String::from_utf8(contents) {
                Ok(contents) => contents
                    .replace(fixture_root.as_ref(), "<composite-fixture-root>")
                    .into_bytes(),
                Err(error) => error.into_bytes(),
            };
            (path, contents)
        })
        .collect()
}
pub fn generate_arithmetic_with_host_crates(out_dir: &Utf8PathBuf, host_crates_dir: &Utf8PathBuf) {
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let manifest = workspace_root().join("examples/arithmetic/Cargo.toml");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_crates_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![
                FlavorTarget::Wasm,
                FlavorTarget::Napi,
                FlavorTarget::Electron,
            ],
        },
    )
    .expect("generator with host crates should succeed");
}

pub fn write_synthetic_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("tiny_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"tiny-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\nname = \"tiny\"\ncrate-type = [\"lib\"]\n\n[dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub fn echo(s: String) -> String { s }\n",
    )
    .unwrap();
    let udl = src.join("tiny.udl");
    std::fs::write(&udl, "namespace tiny {\n    string echo(string s);\n};\n").unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_synthetic_with_host_crates(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let (udl, manifest) = write_synthetic_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Wasm, FlavorTarget::Napi],
        },
    )
    .expect("synthetic generator run should succeed");
    (out_dir, host_dir)
}

pub fn write_float32_record_core_crate(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let core = root.join("float32_record_core");
    let src = core.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        core.join("Cargo.toml"),
        "[package]\nname = \"float32-record-core\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\nname = \"float32_record_core\"\ncrate-type = [\"lib\"]\n\n[dependencies]\n\n[workspace]\nresolver = \"3\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "#[derive(Clone, Debug)]\npub struct Float32Record {\n    pub speed: f32,\n}\n\npub fn roundtrip_float32_record(value: Float32Record) -> Float32Record {\n    value\n}\n\npub struct AsyncService;\n\nimpl AsyncService {\n    pub fn new() -> std::sync::Arc<Self> {\n        std::sync::Arc::new(Self)\n    }\n\n    pub async fn greet(&self, message: String) -> String {\n        message\n    }\n}\n",
    )
    .unwrap();
    let udl = src.join("float32_record_core.udl");
    std::fs::write(
        &udl,
        "dictionary Float32Record {\n    float speed;\n};\n\ninterface AsyncService {\n    constructor();\n    [Async]\n    string greet(string message);\n};\n\nnamespace float32_record_core {\n    Float32Record roundtrip_float32_record(Float32Record value);\n};\n",
    )
    .unwrap();
    (
        Utf8PathBuf::from_path_buf(udl).unwrap(),
        Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap(),
    )
}

pub fn generate_float32_record_hosts(root: &std::path::Path) -> (Utf8PathBuf, Utf8PathBuf) {
    let (udl, manifest) = write_float32_record_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors: vec![FlavorTarget::Napi, FlavorTarget::Harmony],
        },
    )
    .expect("float32 record host generation should succeed");
    (out_dir, host_dir)
}
pub fn cargo_target_libdir(target: &str) -> std::io::Result<Option<std::path::PathBuf>> {
    let rustc = match std::env::var_os("RUSTC") {
        Some(value) if !value.is_empty() => std::path::PathBuf::from(value),
        _ => {
            let output = Command::new("rustup").args(["which", "rustc"]).output()?;
            if !output.status.success() {
                return Err(std::io::Error::other(format!(
                    "rustup could not resolve the rustc used by cargo: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
            std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
        }
    };
    let output = Command::new(rustc)
        .args(["--print", "target-libdir", "--target", target])
        .output()?;
    if !output.status.success() {
        return Ok(None);
    }
    let libdir = std::path::PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    Ok(libdir.is_dir().then_some(libdir))
}

pub fn generate_synthetic_gated(root: &std::path::Path, flavors: Vec<FlavorTarget>) -> Utf8PathBuf {
    let (udl, manifest) = write_synthetic_core_crate(root);
    let out_dir = Utf8PathBuf::from_path_buf(root.join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(root.join("rust_modules")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: Some(uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
                logical_out_dir: None,
                ohos_rs_dir: None,
            }),
            flavors,
        },
    )
    .expect("gated generator run should succeed");
    host_dir
}
