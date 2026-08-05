//! Generated host-crate tree, compilation, and flavor-gating tests.

mod support;

#[path = "support/shared.rs"]
mod shared;

use shared::*;
use support::*;
use uniffi_bindgen_javascript::HostCrateOptions;

#[test]
fn package_root_emits_required_host_crate_tree() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(out.path().join("generated/native/hosts")).unwrap();
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
        wasm_lib.contains("include!(") && wasm_lib.contains("wasm.rs"),
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
        napi_lib.contains("include!(") && napi_lib.contains("node.rs"),
        "napi lib.rs must include the generated component node bridge, got:\n{napi_lib}"
    );

    let build_rs = std::fs::read_to_string(host_dir.join("napi/build.rs")).unwrap();
    assert!(build_rs.contains("napi_build::setup"));
}

#[test]
fn generated_package_always_contains_native_host_tree() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().to_path_buf()).unwrap();
    generate_arithmetic(&out_dir);
    // A package always includes its native adapters and host plan. The
    // package root owns the deterministic host directory.
    assert!(out_dir.join("native").is_dir());
    assert!(out_dir.join("native/hosts").is_dir());
}

#[test]
fn emits_ohos_host_crate_when_harmony_is_requested() {
    let out = tempfile::tempdir().unwrap();
    let out_dir = Utf8PathBuf::from_path_buf(out.path().join("generated")).unwrap();
    let host_dir = Utf8PathBuf::from_path_buf(out.path().join("generated/native/hosts")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let manifest = workspace_root().join("examples/arithmetic/Cargo.toml");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
            },
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
        "napi-ohos = { git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"89a51a707d6a9ab1871e36b990987f58c9b3a6f7\", package = \"napi-ohos\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }",
        "napi-derive-ohos = { git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"89a51a707d6a9ab1871e36b990987f58c9b3a6f7\", package = \"napi-derive-ohos\", features = [\"strict\", \"type-def\"] }",
        "napi-ohos-uniffi-engine = { git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"89a51a707d6a9ab1871e36b990987f58c9b3a6f7\" }",
        "napi-build-ohos = { git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"89a51a707d6a9ab1871e36b990987f58c9b3a6f7\", package = \"napi-build-ohos\" }",
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
    let lib_rs = std::fs::read_to_string(host_dir.join("ohos/src/lib.rs")).unwrap();
    assert!(
        lib_rs.contains("include!(") && lib_rs.contains("ohos.rs"),
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
        out_dir.join("native/node.rs"),
        out_dir.join("native/ohos.rs"),
    ] {
        let source = std::fs::read_to_string(&bridge).unwrap();
        let compact = source.split_whitespace().collect::<String>();
        assert!(
            compact.contains("pubspeed:f64")
                && compact.contains("speed:value.speedasf32")
                && compact.contains("speed:value.speedasf64"),
            "float32 bridge must adapt JS number at the FFI boundary: {source}"
        );
        assert!(
            !source.contains("pub speed: f32"),
            "the host crate must not ask N-API to marshal f32 directly: {source}"
        );
        assert!(
            compact.contains("asyncfn__uniffi_raw_operation_2"),
            "async object receiver must use an async raw operation wrapper: {source}"
        );
        assert!(
            compact.contains("__uniffi_receiver:u32"),
            "async object receiver must cross N-API as a u32 lease: {source}"
        );
        assert!(
            compact.contains("__uniffi_lower_2_1(__uniffi_receiver)"),
            "async object receiver must lower through its typed lease helper: {source}"
        );
        assert!(
            compact.contains("__uniffi_native_operation_2(__uniffi_receiver,message).await"),
            "async object receiver must reach the native future after lowering: {source}"
        );
        assert!(
            !compact.contains("ClassInstance"),
            "async object receivers must not capture a N-API ClassInstance across the native future: {source}"
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
fn wasm_flavor_gates_napi_host() {
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
fn napi_flavor_gates_wasm_host() {
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
fn electron_flavor_reuses_napi_host() {
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
fn wasm_flavor_host_passes_cargo_check() {
    // Regression proof for flavor gating: a wasm-only package must not emit
    // a N-API host crate that includes a non-existent node adapter.
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
        Utf8PathBuf::from_path_buf(tmp.path().join("generated/native/node.rs")).unwrap(),
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

    let package_entry = std::fs::read_to_string(
        tmp.path()
            .join("generated/components/napi_temporal_core/index.js"),
    )
    .unwrap();
    assert!(
        package_entry.contains("createNamespace"),
        "temporal package entry should expose the canonical namespace factory:\n{package_entry}"
    );
    assert!(
        tmp.path().join("generated/electron/index.js").is_file(),
        "temporal package must publish the package-level Electron entry"
    );
    assert!(
        !tmp.path()
            .join("generated/components/napi_temporal_core/electron")
            .exists(),
        "temporal package must not recreate a per-component Electron sidecar"
    );
}

#[test]
fn host_crates_napi_compiles_enum_callback_async_fixture() {
    let tmp = tempfile::tempdir().unwrap();
    let host_dir = generate_rich_napi_host(tmp.path());

    // Sanity check: the generated bridge actually uses the newer
    // napi-rs surface whose compatibility is the point of this test.
    let bridge = std::fs::read_to_string(
        Utf8PathBuf::from_path_buf(tmp.path().join("generated/native/node.rs")).unwrap(),
    )
    .unwrap();
    assert!(
        bridge.contains("discriminant = \"tag\""),
        "rich fixture should exercise the tagged #[napi(discriminant = \"tag\")] carrier"
    );
    assert!(
        bridge.contains("string_enum"),
        "rich fixture should exercise #[napi(string_enum)] for flat enums"
    );
    assert!(
        bridge.contains("SessionCallbackTransfers")
            && bridge
                .split_whitespace()
                .collect::<String>()
                .contains("callback_transfer:true"),
        "rich fixture should exercise the session callback transfer contract"
    );
    assert!(
        bridge
            .split_whitespace()
            .collect::<String>()
            .contains("napi::bindgen_prelude::BigInt"),
        "rich fixture should use napi::BigInt for u64/i64, got:\n{bridge}"
    );
    assert!(
        {
            let compact = bridge.split_whitespace().collect::<String>();
            compact.contains(
                "asyncfn__uniffi_raw_operation_1(counter:__UniffiNapiObjectLease)",
            )
                && compact.contains("crate::__uniffi_lower_1_0(counter)")
                && compact.contains("crate::__uniffi_native_operation_1(counter).await")
                && compact.contains("execute_tokio_future_with_finalize_callback")
                && !compact.contains("ClassInstance")
        },
        "async object arguments must use the private object carrier, lower before the N-API future, and await the native operation:\n{bridge}"
    );

    let manifest = host_dir.join("napi/Cargo.toml");
    let cargo_toml = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        cargo_toml.contains("napi = { git = \"https://github.com/EverglowStudio/napi-rs.git\""),
        "napi host crate template must use the pinned napi-rs 3.x engine source, got:\n{cargo_toml}"
    );
    assert!(
        cargo_toml
            .contains("napi-derive = { git = \"https://github.com/EverglowStudio/napi-rs.git\"",)
            && cargo_toml.contains("type-def"),
        "napi-derive must use the pinned napi-rs source with type-def, got:\n{cargo_toml}"
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
    let forward_hosts =
        Utf8PathBuf::from_path_buf(forward_root.join("generated/native/hosts")).unwrap();
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
    let reverse_hosts =
        Utf8PathBuf::from_path_buf(reverse_root.join("generated/native/hosts")).unwrap();
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
        "reverse Cargo dependency/re-export order must not change host crates or native adapters",
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
        let native = match flavor {
            "wasm" => "wasm.rs",
            "napi" => "node.rs",
            "ohos" => "ohos.rs",
            _ => unreachable!(),
        };
        assert!(
            lib_rs.contains(native),
            "{flavor} host must include package-native adapter {native}:\n{lib_rs}"
        );
    }

    // A composite package exposes one deterministic native adapter per target
    // and one host crate per requested flavor.  There is no separate bundle or
    // sidecar reader: the host source includes the adapter from the same root.
    for (flavor, native) in [
        ("wasm", "wasm.rs"),
        ("napi", "node.rs"),
        ("ohos", "ohos.rs"),
    ] {
        let lib_rs =
            std::fs::read_to_string(forward_hosts.join(flavor).join("src/lib.rs")).unwrap();
        assert!(
            lib_rs.contains(native),
            "{flavor} host must include the package-native adapter {native}:\n{lib_rs}"
        );
        assert!(
            forward_out.join("native").join(native).is_file(),
            "package must contain native/{native}"
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
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_crates_dir.clone(),
                logical_host_crates_dir: None,
            },
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
    let host_dir = Utf8PathBuf::from_path_buf(root.join("generated/native/hosts")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
            },
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
    let host_dir = Utf8PathBuf::from_path_buf(root.join("generated/native/hosts")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
            },
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
    let host_dir = Utf8PathBuf::from_path_buf(root.join("generated/native/hosts")).unwrap();
    std::fs::create_dir_all(&out_dir).unwrap();
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source: udl,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: host_dir.clone(),
                logical_host_crates_dir: None,
            },
            flavors,
        },
    )
    .expect("gated generator run should succeed");
    host_dir
}
