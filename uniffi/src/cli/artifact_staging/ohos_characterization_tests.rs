/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// Artifact-staging and OHOS packaging characterization tests.

use super::*;

fn test_cleanup_temp_root(root: &Utf8Path) {
    std::fs::remove_dir_all(root).ok();
}

fn test_build_options() -> BuildOptions {
    BuildOptions {
        cargo_bin: "cargo".into(),
        core_manifest_path: None,
        additional_source_roots: Vec::new(),
        manifest_path: Utf8PathBuf::from("test/host/Cargo.toml"),
        facade_mode: FacadeMode::RawOnly,
        dist_dir: Utf8PathBuf::from("test/dist"),
        package_name: None,
        module_name: None,
        package_version: None,
        author: None,
        license: None,
        description: None,
        compatible_sdk_version: None,
        target_sdk_version: None,
        compatible_sdk_type: None,
        device_types: Vec::new(),
        package_kind: PackageKind::Har,
        integrated_hsp: false,
        hsp_bundle_name: None,
        har_out: None,
        runtime_hsp_out: None,
        interface_har_out: None,
        tgz_out: None,
        hvigorw: None,
        ohpm: None,
        deveco_sdk_home: None,
        no_har: false,
        arches: Vec::new(),
        target_dir: None,
        release: false,
        cargo_args: Vec::new(),
        copy_static: false,
        skip_libs: false,
        skip_check: true,
        zigbuild: false,
        bisheng: false,
        package: None,
        skip_napi_check: true,
        soname: None,
        frontend_hsp_preflight_done: false,
    }
}

fn test_host_package(name: &str, version: &str, lib_target_name: &str) -> HostPackage {
    HostPackage {
        cargo_package_id: format!("{name} {version} (test)"),
        name: name.into(),
        version: version.into(),
        description: None,
        authors: Vec::new(),
        license: None,
        manifest_path: Utf8PathBuf::from("test/host/Cargo.toml"),
        lib_target_name: lib_target_name.into(),
    }
}

fn test_package_metadata() -> OhosPackageMetadata {
    OhosPackageMetadata {
        name: "@scope/demo".into(),
        module_name: "demo_native".into(),
        version: "1.2.3".into(),
        description: Some("Demo native package".into()),
        author: Some("Demo Author <demo@example.com>".into()),
        license: Some("Apache-2.0".into()),
        sdk: Some(SdkCompatibility {
            version: "22".into(),
            sdk_type: RuntimeSdkType::HarmonyOs,
        }),
        device_types: vec!["phone".into(), "tablet".into(), "2in1".into()],
    }
}

fn write_fake_dist(root: &Utf8Path, lib_target_name: &str) -> Utf8PathBuf {
    let dist = root.join("dist");
    let native = native_lib_filename(lib_target_name);
    std::fs::create_dir_all(dist.join("arm64-v8a")).unwrap();
    std::fs::write(
        dist.join("native-facade.d.ts"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
        dist.join("Index.d.ets"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
            dist.join("native-facade.ets"),
            format!(
                "import native from \"{native}\";\nexport const welcomeAgent = native.welcomeAgent;\nexport default native;\n"
            ),
        )
        .unwrap();
    std::fs::write(
            dist.join("Index.ets"),
            "export { welcomeAgent } from \"./src/main/ets/native-facade\";\nexport { default } from \"./src/main/ets/native-facade\";\n",
        )
        .unwrap();
    std::fs::write(dist.join("arm64-v8a").join(native), "fake").unwrap();
    dist
}

fn write_invocation_dist(dist: &Utf8Path, arches: &[&str], with_native: bool) -> Result<()> {
    std::fs::create_dir_all(dist)?;
    std::fs::write(
        dist.join("native-facade.d.ts"),
        "export declare function demo(): void;\n",
    )?;
    std::fs::write(
        dist.join("Index.d.ets"),
        "export declare function demo(): void;\n",
    )?;
    std::fs::write(
            dist.join("native-facade.ets"),
            "import native from \"libdemo_ohos.so\";\nexport const demo = native.demo;\nexport default native;\n",
        )?;
    std::fs::write(
            dist.join("Index.ets"),
            "export { demo } from \"./src/main/ets/native-facade\";\nexport { default } from \"./src/main/ets/native-facade\";\n",
        )?;
    if with_native {
        for arch in arches {
            let arch_dir = dist.join(arch);
            std::fs::create_dir_all(&arch_dir)?;
            std::fs::write(arch_dir.join("libdemo_ohos.so"), format!("{arch}:main"))?;
            std::fs::write(arch_dir.join("libc++_shared.so"), format!("{arch}:cxx"))?;
            std::fs::write(arch_dir.join("libdemo_ohos.a"), format!("{arch}:static"))?;
        }
    }
    Ok(())
}

fn regular_file_snapshot(root: &Utf8Path) -> BTreeMap<Utf8PathBuf, Vec<u8>> {
    fn visit(root: &Utf8Path, path: &Utf8Path, snapshot: &mut BTreeMap<Utf8PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let child = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
            if entry.file_type().unwrap().is_dir() {
                visit(root, &child, snapshot);
            } else {
                snapshot.insert(
                    child.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(child).unwrap(),
                );
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    if root.is_dir() {
        visit(root, root, &mut snapshot);
    }
    snapshot
}

fn native_abis(snapshot: &BTreeMap<Utf8PathBuf, Vec<u8>>) -> BTreeSet<String> {
    snapshot
        .keys()
        .filter(|path| matches!(path.extension(), Some("so") | Some("a")))
        .filter_map(|path| {
            path.components()
                .next()
                .map(|part| part.as_str().to_string())
        })
        .collect()
}

fn write_fake_compiled_har(project_root: &Utf8Path, metadata: &OhosPackageMetadata) -> Result<()> {
    let output = project_root.join("library/build/default/outputs/default/release/library.har");
    std::fs::create_dir_all(output.parent().unwrap())?;
    let module = serde_json::to_vec(&serde_json::json!({
        "module": { "name": metadata.module_name, "type": "har" }
    }))?;
    let package = serde_json::to_vec(&serde_json::json!({
        "name": metadata.name,
        "version": metadata.version,
        "main": "Index.ets",
        "types": "Index.d.ets"
    }))?;
    write_normalized_har(
        std::fs::File::create(output)?,
        vec![
            NormalizedArchiveEntry {
                path: "package".into(),
                data: None,
            },
            NormalizedArchiveEntry {
                path: "package/src".into(),
                data: None,
            },
            NormalizedArchiveEntry {
                path: "package/src/main".into(),
                data: None,
            },
            NormalizedArchiveEntry {
                path: "package/src/main/module.json".into(),
                data: Some(module),
            },
            NormalizedArchiveEntry {
                path: "package/oh-package.json5".into(),
                data: Some(package),
            },
        ],
    )
}

#[test]
fn parses_ohos_arch_aliases() {
    assert_eq!(Arch::parse("aarch").unwrap(), Arch::Arm64);
    assert_eq!(
        Arch::parse("arm64").unwrap().rust_target(),
        "aarch64-unknown-linux-ohos"
    );
    assert_eq!(Arch::parse("x64").unwrap().dist_dir(), "x86_64");
    assert_eq!(Arch::parse("arm32").unwrap().c_target(), "arm-linux-ohos");
    assert_eq!(
        Arch::parse("loongarch64").unwrap().rust_target(),
        "loongarch64-unknown-linux-ohos"
    );
    assert!(Arch::parse("mips").is_err());
}

#[test]
fn native_lib_filename_matches_harmony_import_name() {
    assert_eq!(native_lib_filename("uni_core_ohos"), "libuni_core_ohos.so");
}

#[test]
fn cargo_args_include_release_package_soname_and_loongarch() {
    let mut opts = test_build_options();
    opts.release = true;
    opts.cargo_args = vec![
        "--no-default-features".into(),
        "--features".into(),
        "ohos".into(),
    ];
    opts.zigbuild = true;
    opts.package = Some("uni-core-ohos".into());
    opts.soname = Some("uni_core_ohos".into());
    let package = test_host_package("uni-core-ohos", "0.0.0", "uni_core_ohos");
    let args = cargo_args_for_arch(&opts, &package, Arch::LoongArch64, true);
    assert_eq!(args[0], "+nightly");
    assert_eq!(args[1], "zigbuild");
    assert!(args.contains(&"-Z".into()));
    assert!(args.contains(&"build-std".into()));
    assert!(args.contains(&"--release".into()));
    assert!(args.windows(2).any(|w| w == ["-p", "uni-core-ohos@0.0.0"]));
    assert!(args
        .windows(3)
        .any(|w| w == ["--no-default-features", "--features", "ohos"]));
    assert_eq!(
        normalize_soname("uni_core_ohos").unwrap(),
        "libuni_core_ohos.so"
    );
}

#[test]
fn ohos_env_uses_target_wrapper_without_overriding_cargo_rustflags() {
    let root = std::env::temp_dir().join(format!(
        "uniffi-ohos-env-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let ohos = root.join("ohos");
    let hms = root.join("hms");
    std::fs::create_dir_all(ohos.join("native/sysroot")).unwrap();
    std::fs::create_dir_all(hms.join("native/BiSheng/bin")).unwrap();
    std::fs::create_dir_all(hms.join("native/BiSheng/lib")).unwrap();
    std::fs::create_dir_all(hms.join("native/sysroot/usr/include")).unwrap();
    std::fs::create_dir_all(hms.join("native/sysroot/usr/lib/aarch64-linux-ohos")).unwrap();

    let ohos = ohos.to_string_lossy().to_string();
    let type_dir = Utf8Path::new("/tmp/uniffi-ohos-types");
    let envs = ohos_env(
        &ohos,
        Arch::Arm64,
        type_dir,
        "demo_ohos",
        true,
        Some("demo_ohos"),
        &[],
        root.as_path(),
        OsStr::new("cargo"),
        &[],
    )
    .unwrap();
    assert!(!envs.vars.contains_key("CARGO_ENCODED_RUSTFLAGS"));
    assert!(!envs.vars.contains_key("RUSTFLAGS"));
    assert!(envs.append_args.contains("-Clinker="));
    assert!(envs
        .append_args
        .contains("-Clink-arg=-Wl,-soname,libdemo_ohos.so"));
    assert!(envs.vars["TARGET_CFLAGS"].contains("native/sysroot/usr/include"));
    assert!(envs.vars["OPENCV_CLANG_ARGS"].contains("native/sysroot/usr/lib/aarch64-linux-ohos"));

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn strict_type_def_compatibility_feature_compiles_without_upstream_raw_output() {
    let temp = tempfile::tempdir().unwrap();
    let crate_dir = temp.path().join("strict_type_def_compatibility");
    let source_dir = crate_dir.join("src");
    let poison_dir = temp.path().join("upstream-type-def-poison");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&poison_dir).unwrap();
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        r#"[package]
name = "uniffi-ohos-type-def-env-clean"
version = "0.0.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
napi-ohos = { version = "1.1.6", default-features = false, features = ["napi8", "tokio_rt"] }
napi-derive-ohos = { version = "1.1.6", default-features = false, features = ["strict", "type-def"] }
"#,
    )
    .unwrap();
    std::fs::write(
        source_dir.join("lib.rs"),
        r#"use napi_derive_ohos::napi;

#[napi]
pub fn answer() -> u32 {
    42
}
"#,
    )
    .unwrap();

    let mut command = std::process::Command::new(env!("CARGO"));
    command
        .args(["check", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", temp.path().join("target"))
        .env("NAPI_TYPE_DEF_TMP_FOLDER", &poison_dir)
        .env("TYPE_DEF_TMP_PATH", "poisoned-upstream-type-def-path");
    suppress_ohos_upstream_type_def_output(&mut command);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "strict + type-def compatibility crate failed to compile:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        std::fs::read_dir(&poison_dir).unwrap().next().is_none(),
        "upstream napi type-def producer wrote raw output despite explicit environment removal"
    );
}

#[test]
fn cargo_config_wrappers_are_resolved_with_cargo_precedence_and_order() {
    let root = temp_test_dir("uniffi-ohos-cargo-wrapper-config");
    let config_dir = root.join(".cargo");
    std::fs::create_dir_all(&config_dir).unwrap();
    let normal = root.join("normal-wrapper");
    let workspace = root.join("workspace-wrapper");
    let env_normal = root.join("env-normal-wrapper");
    let env_workspace = root.join("env-workspace-wrapper");
    let cli_normal = root.join("cli-normal-wrapper");
    let cli_workspace = root.join("cli-workspace-wrapper");
    for path in [
        &normal,
        &workspace,
        &env_normal,
        &env_workspace,
        &cli_normal,
        &cli_workspace,
    ] {
        std::fs::write(path, b"wrapper fixture").unwrap();
    }
    std::fs::write(
        config_dir.join("config.toml"),
        format!(
            "[build]\nrustc-wrapper = {}\nrustc-workspace-wrapper = {}\n",
            serde_json::to_string(normal.as_str()).unwrap(),
            serde_json::to_string(workspace.as_str()).unwrap(),
        ),
    )
    .unwrap();
    let cli_config = root.join(".cargo/cli-wrapper.toml");
    std::fs::write(
        &cli_config,
        format!(
            "[build]\nrustc-workspace-wrapper = {}\n",
            serde_json::to_string(cli_workspace.as_str()).unwrap(),
        ),
    )
    .unwrap();

    let options = || {
        cargo_config2::ResolveOptions::default()
            .cargo_home(Some(root.join("cargo-home").into_std_path_buf()))
    };
    assert_eq!(
        cargo_rustc_wrappers_with_options(
            root.as_std_path(),
            OsStr::new("/custom/cargo"),
            &[],
            &[],
            options(),
        )
        .unwrap(),
        CargoRustcWrappers {
            normal: Some(normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let cargo_args = vec![
        "--config".to_string(),
        format!(
            "build.rustc-wrapper={}",
            serde_json::to_string(cli_normal.as_str()).unwrap()
        ),
        format!("--config={}", cli_config),
    ];
    assert_eq!(
        cargo_rustc_wrappers_with_options(
            root.as_std_path(),
            OsStr::new("/custom/cargo"),
            &cargo_args,
            &[],
            options(),
        )
        .unwrap(),
        CargoRustcWrappers {
            normal: Some(cli_normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(cli_workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let cargo_environment = vec![
        (
            OsString::from("CARGO_BUILD_RUSTC_WRAPPER"),
            env_normal.as_std_path().as_os_str().to_owned(),
        ),
        (
            OsString::from("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER"),
            env_workspace.as_std_path().as_os_str().to_owned(),
        ),
    ];
    assert_eq!(
        cargo_rustc_wrappers_with_options(
            root.as_std_path(),
            OsStr::new("/custom/cargo"),
            &cargo_args,
            &cargo_environment,
            options(),
        )
        .unwrap(),
        CargoRustcWrappers {
            normal: Some(cli_normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(cli_workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let direct_environment = vec![
        (
            OsString::from("RUSTC_WRAPPER"),
            env_normal.as_std_path().as_os_str().to_owned(),
        ),
        (
            OsString::from("RUSTC_WORKSPACE_WRAPPER"),
            env_workspace.as_std_path().as_os_str().to_owned(),
        ),
    ];
    let resolved = cargo_rustc_wrappers_with_options(
        root.as_std_path(),
        OsStr::new("/custom/cargo"),
        &cargo_args,
        &direct_environment,
        options(),
    )
    .unwrap();
    assert_eq!(
        resolved,
        CargoRustcWrappers {
            normal: Some(env_normal.as_std_path().as_os_str().to_owned()),
            workspace: Some(env_workspace.as_std_path().as_os_str().to_owned()),
        }
    );

    let command = rustc_wrapper_command(
        resolved.workspace.unwrap(),
        vec![
            OsString::from("rustc"),
            OsString::from("--target=aarch64-unknown-linux-ohos"),
        ],
        resolved.normal,
        &[OsString::from("--cfg"), OsString::from("uniffi_ohos")],
    )
    .unwrap();
    assert_eq!(command.get_program(), env_normal.as_std_path().as_os_str());
    let args = command.get_args().collect::<Vec<_>>();
    assert_eq!(args[0], env_workspace.as_std_path().as_os_str());
    assert_eq!(args[1], OsStr::new("rustc"));
    assert!(args.contains(&OsStr::new("uniffi_ohos")));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn target_wrapper_preserves_cargo_identity_and_appends_after_resolved_flags() {
    let remaps = vec![
        PathRemap {
            source: "/tmp/work=checkout/src".into(),
            destination: "/uniffi/source/core".into(),
        },
        PathRemap {
            source: "/tmp".into(),
            destination: "/uniffi/temp".into(),
        },
    ];
    let append =
        target_rustc_append_args("/sdk/bin/clang", &["--sysroot=/sdk".into()], &remaps).unwrap();
    assert_eq!(append[0], "-Clinker=/sdk/bin/clang");
    assert_eq!(append[1], "-Clink-arg=--sysroot=/sdk");
    assert_eq!(
        append[2], "--remap-path-prefix=/tmp=/uniffi/temp",
        "broad remap must be appended before the specific rule"
    );
    assert_eq!(
        append[3],
        "--remap-path-prefix=/tmp/work=checkout/src=/uniffi/source/core"
    );

    let cargo_args = vec![
        OsString::from("--crate-name"),
        OsString::from("same"),
        OsString::from("--target"),
        OsString::from("aarch64-unknown-linux-ohos"),
        OsString::from("--cfg"),
        OsString::from("from_cargo_config"),
        OsString::from("-C"),
        OsString::from("metadata=cargo-native-source-id"),
        OsString::from("--extern"),
        OsString::from("same=/target/libsame-source-a.rmeta"),
    ];
    let append = append.into_iter().map(OsString::from).collect::<Vec<_>>();
    let command =
        rustc_wrapper_command(OsString::from("rustc"), cargo_args.clone(), None, &append).unwrap();
    let actual = command.get_args().map(OsString::from).collect::<Vec<_>>();
    assert_eq!(&actual[..cargo_args.len()], cargo_args.as_slice());
    assert!(actual.contains(&OsString::from("metadata=cargo-native-source-id")));
    assert!(actual.contains(&OsString::from("from_cargo_config")));

    let host = rustc_wrapper_command(
        OsString::from("rustc"),
        vec![
            OsString::from("--crate-name"),
            OsString::from("build_script"),
        ],
        None,
        &append,
    )
    .unwrap();
    assert_eq!(host.get_args().count(), 2, "host units must be untouched");
}

#[cfg(unix)]
#[test]
fn wrapper_identity_detects_symlink_alias_and_accepts_non_utf8_inner_path() {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-wrapper-identity");
    let executable = root.join("wrapper");
    std::fs::write(&executable, b"#!/bin/sh\nexit 0\n").unwrap();
    let alias = root.join("wrapper-alias");
    symlink(&executable, &alias).unwrap();
    assert!(same_executable(executable.as_os_str(), alias.as_os_str()).unwrap());

    let non_utf8 = OsString::from_vec(vec![b'/', b't', b'm', b'p', b'/', 0xff]);
    let command = rustc_wrapper_command(
        OsString::from("rustc"),
        vec![OsString::from("-vV")],
        Some(non_utf8.clone()),
        &[],
    )
    .unwrap();
    assert_eq!(command.get_program(), non_utf8.as_os_str());
    assert_eq!(command.get_args().count(), 2);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(any(unix, windows))]
#[test]
fn wrapper_identity_detects_hardlink_alias() {
    let root = temp_test_dir("uniffi-ohos-wrapper-hardlink-identity");
    let executable = root.join("wrapper");
    let alias = root.join("wrapper-hardlink");
    std::fs::write(&executable, b"wrapper fixture").unwrap();
    std::fs::hard_link(&executable, &alias).unwrap();
    assert!(same_executable(executable.as_os_str(), alias.as_os_str()).unwrap());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn fixture_path_check_is_bounded_and_handles_chunk_boundaries() {
    let root = temp_test_dir("uniffi-ohos-path-leak-scan");
    let so = root.join("fixture.so");
    let needle = b"/private/build/source";
    let mut bytes = vec![b'x'; 64 * 1024 - 5];
    bytes.extend_from_slice(needle);
    bytes.resize(8 * 1024 * 1024, b'y');
    std::fs::write(&so, bytes).unwrap();
    assert!(file_contains_bytes_bounded(&so, needle).unwrap());
    assert!(!file_contains_bytes_bounded(&so, b"/absent/path").unwrap());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn cargo_args_do_not_duplicate_existing_package_filter() {
    let mut opts = test_build_options();
    opts.cargo_args = vec!["--package".into(), "from-cli".into()];
    let package = test_host_package("uni-core-ohos", "0.0.0", "uni_core_ohos");
    let args = cargo_args_for_arch(&opts, &package, Arch::Arm64, true);
    assert_eq!(
        args.iter()
            .filter(|arg| arg.as_str() == "-p" || arg.as_str() == "--package")
            .count(),
        1
    );
    assert!(args.windows(2).any(|w| w == ["--package", "from-cli"]));
}

#[test]
fn package_arg_detection_supports_short_long_and_equals() {
    assert_eq!(
        package_arg_from_cargo_args(&["-p".into(), "core".into()]),
        Some("core".into())
    );
    assert_eq!(
        package_arg_from_cargo_args(&["--package=core".into()]),
        Some("core".into())
    );
    assert!(package_arg_from_cargo_args(&["--features".into(), "ohos".into()]).is_none());
}

#[test]
fn package_filter_rejects_conflicting_cli_and_cargo_args() {
    assert_eq!(
        resolve_package_filter(Some("core"), &["--package".into(), "core".into()]).unwrap(),
        Some("core".into())
    );
    assert!(resolve_package_filter(Some("core"), &["--package".into(), "other".into()]).is_err());
}

#[test]
fn validates_ohpm_package_and_harmony_module_name_boundaries() {
    for valid in ["a", "demo.pkg-2_ok", "@group_1/demo.pkg-2_ok"] {
        validate_oh_package_name(valid).unwrap();
    }
    let max = format!("a{}", "1".repeat(127));
    validate_oh_package_name(&max).unwrap();

    for invalid in [
        "",
        "Demo",
        "1demo",
        "demo-",
        "@group_/demo",
        "@group/demo/extra",
        "group/demo",
        "@group/@demo",
        "class",
        "demo.har",
        "demo.tgz",
        "demo.tar",
        "demo.tar.gz",
    ] {
        let error = validate_oh_package_name(invalid).unwrap_err().to_string();
        assert!(
            error.contains("invalid"),
            "unexpected error for {invalid}: {error}"
        );
        assert!(
            error.contains("lowercase"),
            "missing correction for {invalid}: {error}"
        );
    }
    assert!(validate_oh_package_name(&format!("a{}", "1".repeat(128))).is_err());

    assert_eq!(
        derive_module_name("@group/demo.pkg-name").unwrap(),
        "demo_pkg_name"
    );
    validate_module_name("Demo_native2").unwrap();
    validate_module_name(&format!("A{}", "b".repeat(127))).unwrap();
    assert!(validate_module_name(&format!("A{}", "b".repeat(128))).is_err());
    for invalid in ["", "2demo", "demo-name", "demo.name", "模块"] {
        assert!(
            validate_module_name(invalid).is_err(),
            "accepted invalid module {invalid}"
        );
    }
}

#[test]
fn resolves_metadata_fallbacks_overrides_semver_and_json_escaping() {
    let mut package = test_host_package("demo-ohos", "1.4.0", "demo_ohos");
    package.description = Some("Cargo \"description\"\nnext".into());
    package.authors = vec!["First Author <first@example.com>".into(), "Second".into()];
    package.license = Some("MPL-2.0".into());

    let fallback = resolve_oh_package_metadata(
        &test_build_options(),
        &package,
        Some(SdkCompatibility {
            version: "13".into(),
            sdk_type: RuntimeSdkType::OpenHarmony,
        }),
    )
    .unwrap();
    assert_eq!(fallback.version, "1.4.0");
    assert_eq!(
        fallback.author.as_deref(),
        Some("First Author <first@example.com>")
    );
    assert_eq!(fallback.license.as_deref(), Some("MPL-2.0"));
    let rendered = render_oh_package_json5(
        &fallback,
        "demo_ohos",
        &["libdemo_ohos.so".to_string()],
        PackageKind::Har,
    )
    .unwrap();
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(parsed["description"], "Cargo \"description\"\nnext");

    let mut options = test_build_options();
    options.package_name = Some("@scope/renamed".into());
    options.module_name = Some("renamed_native".into());
    options.package_version = Some("2.0.0-beta.1+build.4".into());
    options.author = Some("Override Author".into());
    options.license = Some("Apache-2.0".into());
    options.description = Some("Override description".into());
    options.device_types = vec!["phone".into(), "phone".into(), "tv".into()];
    let overridden = resolve_oh_package_metadata(&options, &package, None).unwrap();
    assert_eq!(overridden.name, "@scope/renamed");
    assert_eq!(overridden.module_name, "renamed_native");
    assert_eq!(overridden.version, "2.0.0-beta.1+build.4");
    assert_eq!(overridden.author.as_deref(), Some("Override Author"));
    assert_eq!(overridden.device_types, vec!["phone", "tv"]);

    options.package_version = Some("v2".into());
    assert!(resolve_oh_package_metadata(&options, &package, None).is_err());
    assert!(validate_package_version("1.2").is_err());
    validate_package_version("9007199254740991.0.0-rc.1+build.7").unwrap();
    assert!(validate_package_version("9007199254740992.0.0").is_err());
    assert!(validate_package_version(&format!("1.0.0+{}", "a".repeat(123))).is_err());

    let mut unicode = test_build_options();
    unicode.author = Some("😀".repeat(64));
    unicode.license = Some("😀".repeat(128));
    unicode.description = Some("😀".repeat(256));
    resolve_oh_package_metadata(&unicode, &package, None).unwrap();
    unicode.author = Some("😀".repeat(65));
    assert!(resolve_oh_package_metadata(&unicode, &package, None).is_err());
    unicode.author = Some("Valid Author".into());
    unicode.license = Some("😀".repeat(129));
    assert!(resolve_oh_package_metadata(&unicode, &package, None).is_err());
    assert!(validate_sdk_metadata_value(&"😀".repeat(32)).is_ok());
    assert!(validate_sdk_metadata_value(&"😀".repeat(33)).is_err());
}

#[test]
fn omits_missing_optional_metadata_and_detects_module_collisions() {
    let package_a = test_host_package("foo-bar", "1.0.0", "foo_bar_ohos");
    let package_b = test_host_package("foo.bar", "1.0.0", "foo_dot_ohos");
    let options = test_build_options();
    let metadata_a = resolve_oh_package_metadata(&options, &package_a, None).unwrap();
    let metadata_b = resolve_oh_package_metadata(&options, &package_b, None).unwrap();
    assert_eq!(metadata_a.module_name, "foo_bar");
    assert_eq!(metadata_b.module_name, "foo_bar");
    assert!(ensure_unique_module_names(
        &[package_a.clone(), package_b.clone()],
        &[metadata_a.clone(), metadata_b]
    )
    .is_err());

    let rendered =
        render_oh_package_json5(&metadata_a, "foo_bar_ohos", &[], PackageKind::Har).unwrap();
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert!(parsed.get("description").is_none());
    assert!(parsed.get("author").is_none());
    assert!(parsed.get("license").is_none());
    assert!(parsed.get("compatibleSdkVersion").is_none());
    assert!(parsed.get("compatibleSdkType").is_none());
    assert!(parsed.get("nativeComponents").is_none());
}

#[test]
fn separates_compile_sdk_from_explicit_compatibility_and_discovers_sdk_type() {
    let root = temp_test_dir("uniffi-ohos-sdk-metadata");
    let ohos = root.join("ohos");
    let hms = root.join("hms");
    std::fs::create_dir_all(ohos.join("native")).unwrap();
    std::fs::create_dir_all(hms.join("native")).unwrap();
    std::fs::write(
        ohos.join("native/oh-uni-package.json"),
        r#"{"apiVersion":"22","version":"6.0.2"}"#,
    )
    .unwrap();
    std::fs::write(
        hms.join("native/uni-package.json"),
        r#"{"apiVersion":"22","platformVersion":"6.0.2"}"#,
    )
    .unwrap();

    // Compile API 22 must never become minimum compatible API 22.
    assert!(
        resolve_sdk_compatibility(&test_build_options(), ohos.as_str())
            .unwrap()
            .is_none()
    );

    let mut options = test_build_options();
    options.compatible_sdk_version = Some("13".into());
    let sdk = resolve_sdk_compatibility(&options, ohos.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(sdk.version, "13");
    assert_eq!(sdk.sdk_type, RuntimeSdkType::OpenHarmony);

    options.bisheng = true;
    let sdk = resolve_sdk_compatibility(&options, ohos.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(sdk.sdk_type, RuntimeSdkType::HarmonyOs);

    options.compatible_sdk_type = Some("  openharmony  ".into());
    let sdk = resolve_sdk_compatibility(&options, ohos.as_str())
        .unwrap()
        .unwrap();
    assert_eq!(sdk.sdk_type, RuntimeSdkType::OpenHarmony);

    options.compatible_sdk_type = Some("ExplicitSDK".into());
    assert!(resolve_sdk_compatibility(&options, ohos.as_str()).is_err());

    let missing = root.join("missing");
    std::fs::create_dir_all(&missing).unwrap();
    options.compatible_sdk_type = None;
    options.bisheng = false;
    assert!(resolve_sdk_compatibility(&options, missing.as_str()).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn renders_hvigor_sdk_products_for_runtime_and_api_generation() {
    let harmony = SdkCompatibility {
        version: "5.0.0(12)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    let open_harmony = SdkCompatibility {
        version: "13".into(),
        sdk_type: RuntimeSdkType::OpenHarmony,
    };
    let open_harmony_20 = SdkCompatibility {
        version: "20".into(),
        sdk_type: RuntimeSdkType::OpenHarmony,
    };
    let api_20 = CompileSdk {
        api_level: 20,
        platform_version: "5.1.0".into(),
    };
    let api_25 = CompileSdk {
        api_level: 25,
        platform_version: "6.0.3".into(),
    };

    let product = render_hvigor_product(&api_20, &open_harmony_20, None).unwrap();
    assert_eq!(product["runtimeOS"], "OpenHarmony");
    assert_eq!(product["compileSdkVersion"], 20);
    assert_eq!(product["targetSdkVersion"], 20);
    assert_eq!(product["compatibleSdkVersion"], 20);

    let product = render_hvigor_product(&api_25, &harmony, None).unwrap();
    assert_eq!(product["runtimeOS"], "HarmonyOS");
    assert_eq!(product["targetSdkVersion"], "6.0.3(25)");
    assert_eq!(product["compatibleSdkVersion"], "5.0.0(12)");
    assert!(product.get("compileSdkVersion").is_none());

    let harmony_20 = SdkCompatibility {
        version: "6.0.0(20)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    for (api_level, platform_version) in [
        (20, "6.0.0"),
        (21, "6.0.1"),
        (22, "6.0.2"),
        (23, "6.1.0"),
        (24, "6.1.1"),
        (26, "26.0.0"),
    ] {
        let compile = CompileSdk {
            api_level,
            platform_version: platform_version.into(),
        };
        let product = render_hvigor_product(&compile, &harmony_20, Some("6.0.0(20)")).unwrap();
        assert_eq!(product["targetSdkVersion"], "6.0.0(20)");
        assert_eq!(product["compatibleSdkVersion"], "6.0.0(20)");
        if api_level >= 26 {
            assert_eq!(product["compileSdkVersion"], "26.0.0");
        } else {
            assert!(product.get("compileSdkVersion").is_none());
        }
    }

    let product = render_hvigor_product(&api_25, &open_harmony, None).unwrap();
    assert_eq!(product["runtimeOS"], "OpenHarmony");
    assert_eq!(product["compileSdkVersion"], 25);
    assert_eq!(product["targetSdkVersion"], 25);
    assert_eq!(product["compatibleSdkVersion"], 13);

    let api_26 = CompileSdk {
        api_level: 26,
        platform_version: "26.0.0".into(),
    };
    for sdk in [&harmony, &open_harmony] {
        let product = render_hvigor_product(&api_26, sdk, None).unwrap();
        assert_eq!(product["compileSdkVersion"], "26.0.0");
        assert_eq!(product["targetSdkVersion"], "26.0.0");
        assert!(product["compatibleSdkVersion"].is_string());
        assert!(!product["targetSdkVersion"].as_str().unwrap().contains('('));
    }

    let invalid_open_harmony = SdkCompatibility {
        version: "5.0.0(13)".into(),
        sdk_type: RuntimeSdkType::OpenHarmony,
    };
    assert!(render_hvigor_product(&api_25, &invalid_open_harmony, None).is_err());

    let target_above_compile = format!(
        "{:#}",
        render_hvigor_product(&api_20, &harmony_20, Some("6.0.1(21)")).unwrap_err()
    );
    assert!(
        target_above_compile.contains("target SDK API 21 exceeds compile SDK API 20"),
        "{target_above_compile}"
    );

    let harmony_21 = SdkCompatibility {
        version: "6.0.1(21)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    let target_below_compatible = format!(
        "{:#}",
        render_hvigor_product(&api_25, &harmony_21, Some("6.0.0(20)")).unwrap_err()
    );
    assert!(
        target_below_compatible.contains("target SDK API 20 is lower than compatible SDK API 21"),
        "{target_below_compatible}"
    );
}

#[test]
fn parses_compile_sdk_metadata_as_typed_api_level() {
    let root = temp_test_dir("uniffi-ohos-compile-sdk");
    let default_sdk = root.join("default");
    std::fs::create_dir_all(&default_sdk).unwrap();
    std::fs::write(
        default_sdk.join("sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"22"}}"#,
    )
    .unwrap();
    let mut options = test_build_options();
    options.deveco_sdk_home = Some(root.clone());
    let tools = resolve_harmony_har_tools(&options).unwrap();
    assert_eq!(
        tools.compile_sdk,
        CompileSdk {
            api_level: 22,
            platform_version: "6.0.2".into()
        }
    );

    std::fs::write(
        default_sdk.join("sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"twenty-two"}}"#,
    )
    .unwrap();
    let error = resolve_harmony_har_tools(&options).unwrap_err().to_string();
    assert!(
        error.contains("numeric API level"),
        "unexpected error: {error}"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn validates_device_type_overrides() {
    assert_eq!(
        resolve_device_types(&[]).unwrap(),
        vec!["phone", "tablet", "2in1"]
    );
    assert_eq!(
        resolve_device_types(&["tv".into(), "tv".into(), "wearable".into()]).unwrap(),
        vec!["tv", "wearable"]
    );
    assert_eq!(
        resolve_device_types(&["default".into()]).unwrap(),
        vec!["default"]
    );
    assert!(resolve_device_types(&["unknown".into()]).is_err());
}

#[test]
fn package_dist_dir_only_splits_multi_package_builds() {
    let package = test_host_package("uni-core-ohos", "0.0.0", "uni_core_ohos");
    assert_eq!(
        package_dist_dir(Utf8Path::new("/tmp/dist"), &package, 1),
        Utf8PathBuf::from("/tmp/dist")
    );
    assert_eq!(
        package_dist_dir(Utf8Path::new("/tmp/dist"), &package, 2),
        Utf8PathBuf::from("/tmp/dist/uni-core-ohos")
    );
}

#[test]
fn dist_publication_replaces_plain_directory_without_protocol_residue() {
    let root = temp_test_dir("uniffi-ohos-simple-dist");
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("stale.txt"), b"stale").unwrap();

    build_package_dist_from_stage(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)
    })
    .unwrap();
    assert!(!dist.join("stale.txt").exists());
    assert!(dist.join("x86_64/libdemo_ohos.so").exists());
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        !name.contains("backup") && !name.contains("journal") && !name.contains("candidate")
    }));
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn dangerous_dist_paths_and_symlink_aliases_fail_without_side_effects() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-danger-dist");
    let project = root.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::write(project.join("Cargo.toml"), b"[workspace]\n").unwrap();
    std::fs::write(project.join("src/lib.rs"), b"pub fn sentinel() {}\n").unwrap();
    std::fs::write(project.join("user-marker.txt"), b"must survive").unwrap();
    symlink(&project, root.join("project-link")).unwrap();
    let protected = vec![ProtectedDistPath {
        label: "fixture project".into(),
        path: project.clone(),
    }];
    let before = regular_file_snapshot(&project);
    for dangerous in [&project, &root, &root.join("project-link")] {
        assert!(preflight_dist_output(dangerous, &protected).is_err());
        assert_eq!(regular_file_snapshot(&project), before);
    }
    assert!(std::fs::read_dir(&root).unwrap().all(|entry| !entry
        .unwrap()
        .file_name()
        .to_string_lossy()
        .contains("uniffi-ohos-dist-")));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn filtered_workspace_keeps_stable_multi_package_layout_and_ignores_bin_members() {
    let root = temp_test_dir("uniffi-ohos-filtered-workspace");
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers = [\"package-a\", \"package-b\", \"tool\"]\nresolver = \"2\"\n",
    )
    .unwrap();
    for package in ["package-a", "package-b"] {
        let dir = root.join(package);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
                dir.join("Cargo.toml"),
                format!(
                    "[package]\nname = \"{package}\"\nversion = \"1.0.0\"\nedition = \"2021\"\n\n[lib]\ncrate-type = [\"cdylib\"]\n"
                ),
            )
            .unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    }
    let tool = root.join("tool");
    std::fs::create_dir_all(tool.join("src")).unwrap();
    std::fs::write(
        tool.join("Cargo.toml"),
        "[package]\nname = \"workspace-tool\"\nversion = \"1.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(tool.join("src/main.rs"), "fn main() {}\n").unwrap();

    let mut options = test_build_options();
    options.package = Some("package-a".into());
    options.skip_napi_check = true;
    let plan = host_plan("cargo", &root.join("Cargo.toml"), &options).unwrap();
    assert_eq!(plan.packages.len(), 1);
    assert_eq!(plan.packages[0].name, "package-a");
    assert_eq!(plan.package_count, 2);
    assert_eq!(
        package_dist_dir(&root.join("dist"), &plan.packages[0], plan.package_count),
        root.join("dist/package-a")
    );
    assert_eq!(
        package_stage_dir(&root, &plan.packages[0], plan.package_count),
        root.join("package/package-a")
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn package_dist_staging_isolates_abi_skip_libs_and_failed_sequences() {
    let root = temp_test_dir("uniffi-ohos-dist-sequences");
    let dist = root.join("dist");
    let publish = |arches: &[&str], with_native: bool| {
        build_package_dist_from_stage(&dist, |invocation| {
            write_invocation_dist(invocation, arches, with_native)
        })
    };

    publish(&["arm64-v8a", "x86_64"], true).unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&dist)),
        BTreeSet::from(["arm64-v8a".to_string(), "x86_64".to_string()])
    );

    // arm64+x64 -> x64 must remove the unselected arm64 ABI.
    publish(&["x86_64"], true).unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&dist)),
        BTreeSet::from(["x86_64".to_string()])
    );

    // x64 -> arm64 must remove the unselected x64 ABI.
    publish(&["arm64-v8a"], true).unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&dist)),
        BTreeSet::from(["arm64-v8a".to_string()])
    );

    // A failed second-ABI/package step never publishes its partial tree.
    let before_failure = regular_file_snapshot(&dist);
    let error = build_package_dist_from_stage(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)?;
        bail!("injected second ABI failure")
    })
    .unwrap_err();
    assert!(error.to_string().contains("second ABI failure"));
    assert_eq!(regular_file_snapshot(&dist), before_failure);
    assert_eq!(
        std::fs::read_dir(&root).unwrap().count(),
        1,
        "failed staging must not leave a private sibling directory"
    );

    // libs -> --skip-libs publishes a types/facade-only dist with no stale
    // shared or static native artifact.
    publish(&[], false).unwrap();
    let skipped = regular_file_snapshot(&dist);
    assert!(native_abis(&skipped).is_empty());
    assert!(skipped.contains_key(Utf8Path::new("native-facade.d.ts")));
    assert!(skipped.contains_key(Utf8Path::new("Index.ets")));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn multi_package_dist_staging_updates_only_selected_package() {
    let root = temp_test_dir("uniffi-ohos-multi-package-dist");
    let dist_root = root.join("dist");
    let package_a = dist_root.join("package-a");
    let package_b = dist_root.join("package-b");
    build_package_dist_from_stage(&package_a, |dist| {
        write_invocation_dist(dist, &["arm64-v8a", "x86_64"], true)
    })
    .unwrap();
    build_package_dist_from_stage(&package_b, |dist| {
        write_invocation_dist(dist, &["arm64-v8a"], true)
    })
    .unwrap();
    let package_b_before = regular_file_snapshot(&package_b);

    // The public JavaScript entrypoint only performs root-level path
    // safety before Cargo metadata.  A valid multi-package container has
    // package inventories below it and must remain valid on later
    // unfiltered or filtered invocations.
    preflight_dist_output_for_generation(&dist_root, &[]).unwrap();

    build_package_dist_from_stage(&package_a, |dist| {
        write_invocation_dist(dist, &["x86_64"], true)
    })
    .unwrap();
    assert_eq!(
        native_abis(&regular_file_snapshot(&package_a)),
        BTreeSet::from(["x86_64".to_string()])
    );
    assert_eq!(regular_file_snapshot(&package_b), package_b_before);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn staged_har_consumes_only_current_invocation_dist() {
    let root = temp_test_dir("uniffi-ohos-current-dist-har");
    let dist = root.join("dist");
    build_package_dist_from_stage(&dist, |invocation| {
        write_invocation_dist(invocation, &["arm64-v8a", "x86_64"], true)
    })
    .unwrap();
    build_package_dist_from_stage(&dist, |invocation| {
        write_invocation_dist(invocation, &["x86_64"], true)
    })
    .unwrap();

    let package = root.join("package");
    stage_har_package(
        &dist,
        &package,
        "demo_ohos",
        &test_package_metadata(),
        false,
    )
    .unwrap();
    let source_har = root.join("current.har");
    generate_har_archive(&source_har, &package).unwrap();
    let entries = read_har_entries(&source_har).unwrap();
    let native_paths = entries
        .iter()
        .filter(|entry| {
            entry.data.is_some() && matches!(entry.path.extension(), Some("so") | Some("a"))
        })
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    assert!(native_paths
        .iter()
        .all(|path| path.starts_with("package/libs/x86_64")));
    assert!(!native_paths.is_empty());
    let package_json: Value =
        serde_json::from_str(&std::fs::read_to_string(package.join("oh-package.json5")).unwrap())
            .unwrap();
    let components = package_json["nativeComponents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|component| component["name"].as_str().unwrap())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        components,
        BTreeSet::from(["libc++_shared.so", "libdemo_ohos.so"])
    );

    // HAR -> no-HAR + skip-libs leaves the old package staging irrelevant
    // while the public current dist contains no native binary.
    build_package_dist_from_stage(&dist, |invocation| {
        write_invocation_dist(invocation, &[], false)
    })
    .unwrap();
    assert!(native_abis(&regular_file_snapshot(&dist)).is_empty());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn artifact_filter_dedupes_and_respects_static_flag() {
    let mut paths = BTreeSet::new();
    paths.insert(Utf8PathBuf::from("/tmp/liba.so"));
    paths.insert(Utf8PathBuf::from("/tmp/liba.so"));
    paths.insert(Utf8PathBuf::from("/tmp/liba.a"));
    let filtered = filter_artifacts(
        BuiltArtifacts {
            paths: paths.clone(),
            cargo_provenance: BTreeMap::new(),
        },
        false,
        None,
    );
    assert_eq!(filtered.paths.len(), 1);
    assert!(filtered.paths.contains(Utf8Path::new("/tmp/liba.so")));

    let filtered = filter_artifacts(
        BuiltArtifacts {
            paths,
            cargo_provenance: BTreeMap::new(),
        },
        true,
        None,
    );
    assert_eq!(filtered.paths.len(), 2);
    assert!(filtered.paths.contains(Utf8Path::new("/tmp/liba.a")));
}

#[test]
fn skip_libs_disables_artifact_copy() {
    assert!(should_skip_artifact_copy(true));
    assert!(!should_skip_artifact_copy(false));
}

#[test]
fn renders_type_defs_from_ohos_json_lines() {
    let json = r#"type_def:{"kind":"fn","name":"add","def":"function add(a: number): number;","typeParameters":[]}"#;
    let def = parse_type_def_line(json).unwrap().unwrap();
    let rendered = render_index_d_ts(vec![def]);
    assert!(rendered.contains("export declare function add(a: number): number;"));
    assert!(!rendered.contains("typeof import"));
}

#[test]
fn ohos_type_renderer_rewrites_only_buffer_type_tokens() {
    let def = |kind: &str, name: &str, body: &str| TypeDefLine {
        kind: kind.into(),
        name: name.into(),
        def: body.into(),
        type_parameters: Vec::new(),
    };
    let defs = vec![
        def(
            "interface",
            "BufferPool",
            "Buffer: Buffer\nBufferPool: BufferPool\nqualified: Custom.Buffer\nliteral: \"Buffer\"",
        ),
        def(
            "fn",
            "napi_service",
            "function napi_service(Buffer: Buffer, BufferPool: BufferPool, qualified: Custom.Buffer): Buffer;",
        ),
        def(
            "string_enum",
            "napi_ohos_bridge",
            "Buffer = 'Buffer', BufferPool = 'BufferPool'",
        ),
        def(
            "type",
            "runtime_unknown_value",
            "{ payload: Buffer, literal: \"Buffer\", qualified: Custom.Buffer }",
        ),
    ];

    let rendered = render_index_d_ts(defs);
    assert!(rendered.contains("export interface BufferPool"));
    assert!(rendered.contains("Buffer: ArrayBuffer"));
    assert!(rendered.contains("BufferPool: BufferPool"));
    assert!(rendered.contains("qualified: Custom.Buffer"));
    assert!(rendered.contains("literal: \"Buffer\""));
    assert!(rendered.contains(
        "export declare function napi_service(Buffer: ArrayBuffer, BufferPool: BufferPool, qualified: Custom.Buffer): ArrayBuffer;"
    ));
    assert!(rendered.contains("export type napi_ohos_bridge = 'Buffer' | 'BufferPool';"));
    assert!(rendered.contains("payload: ArrayBuffer"));
    assert!(rendered.lines().all(|line| line.trim_end() == line));
}

#[test]
fn renders_ohos_string_enum_as_literal_union() {
    let json = r#"type_def:{"kind":"string_enum","name":"LocalAiBackend","def":"Auto = 'Auto',\n Onnx = 'Onnx',\n Mlx = 'Mlx'","typeParameters":[]}"#;
    let def = parse_type_def_line(json).unwrap().unwrap();
    let rendered = render_index_d_ts(vec![def]);

    assert!(rendered.contains("export type LocalAiBackend = 'Auto' | 'Onnx' | 'Mlx';"));
    assert!(!rendered.contains("Onnx ="));
}

#[test]
fn rejects_legacy_original_napi_type_name_aliases() {
    let json = r#"type_def:{"kind":"interface","name":"UniffiOutputStreamStep","original_name":"__UniffiOutputStreamStep","def":"kind: string\\nvalue?: string\\nerror?: string","typeParameters":[]}"#;
    let error = format!("{:#}", parse_type_def_line(json).unwrap_err());
    assert!(error.contains("unknown field `original_name`"), "{error}");
}

#[test]
fn rejects_noncanonical_ohos_type_kind_and_type_parameters() {
    let cases = [
        (
            r#"type_def:{"kind":"enum","name":"LegacyEnum","def":"Value = 0","typeParameters":[]}"#,
            "unknown variant `enum`",
        ),
        (
            r#"type_def:{"kind":"interface","name":"MissingParameters","def":"value: string"}"#,
            "missing field `typeParameters`",
        ),
        (
            r#"type_def:{"kind":"interface","name":"UniffiInputStream","def":"value: string","typeParameters":[]}"#,
            "no owning facade contract",
        ),
        (
            r#"type_def:{"kind":"interface","name":"UniffiInputStream","def":"value: string","typeParameters":["T","U"]}"#,
            "no owning facade contract",
        ),
        (
            r#"type_def:{"kind":"interface","name":"UniffiInputStream","def":"value: string","typeParameters":["T","T"]}"#,
            "no owning facade contract",
        ),
        (
            r#"type_def:{"kind":"interface","name":"OtherGeneric","def":"value: string","typeParameters":["T"]}"#,
            "must not declare typeParameters",
        ),
    ];

    for (line, expected) in cases {
        let error = format!("{:#}", parse_type_def_line(line).unwrap_err());
        assert!(
            error.contains(expected),
            "expected `{expected}` in `{error}`"
        );
    }
}

#[test]
fn facade_matches_runtime_value_declarations_and_keeps_types_type_only() {
    let def = |kind: &str, name: &str, body: &str| TypeDefLine {
        kind: kind.into(),
        name: name.into(),
        def: body.into(),
        type_parameters: Vec::new(),
    };
    let defs = vec![
        def("interface", "Greeting", "  text: string"),
        def(
            "string_enum",
            "GreetingStyle",
            "Friendly = 'Friendly', Formal = 'Formal'",
        ),
        def(
            "type",
            "GreetingEvent",
            "| { type: 'Text', text: string } | { type: 'Done' }",
        ),
        def("interface", "Formatter", "  format(value: string): string"),
        def("struct", "GreetingCounter", ""),
        def("struct", "GreetingTemplate", ""),
        def("fn", "greet", "function greet(name: string): string"),
        def(
            "fn",
            "greetAsync",
            "function greetAsync(name: string): Promise<string>",
        ),
        def(
            "fn",
            "greetingCounterNew",
            "function greetingCounterNew(): GreetingCounter",
        ),
        def(
            "fn",
            "greetingCounterCount",
            "function greetingCounterCount(handle: GreetingCounter): number",
        ),
        def("fn", "messages", "function messages(): bigint"),
        def(
            "fn",
            "messagesNext",
            "function messagesNext(handle: bigint): Promise<string>",
        ),
        def(
            "fn",
            "messagesCancel",
            "function messagesCancel(handle: bigint): void",
        ),
    ];
    let exports = FacadeExports::from_type_defs(&defs).unwrap();
    assert_eq!(
        exports.classes,
        vec![
            "GreetingCounter".to_string(),
            "GreetingTemplate".to_string()
        ]
    );
    let expected_callables = defs
        .iter()
        .filter(|def| def.kind == "fn")
        .map(|def| def.name.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        exports.callables.iter().cloned().collect::<BTreeSet<_>>(),
        expected_callables
    );
    assert_eq!(
        render_callable_function_type(&def(
            "fn",
            "withCallback",
            "function withCallback(callback: (value: string) => void): Promise<Buffer>",
        ))
        .unwrap(),
        "(callback: (value: string) => void) => Promise<ArrayBuffer>"
    );
    assert!(render_callable_function_type(&def(
        "fn",
        "broken",
        "function broken(value: string: void",
    ))
    .is_err());

    let facade = exports.render_native_facade("libfixture.so");
    assert!(facade.contains("import native, {"));
    assert!(facade.contains("GreetingCounter,"));
    assert!(facade.contains("} from \"libfixture.so\";"));
    assert!(facade.contains("export const greet: (name: string) => string = native.greet;"));
    assert!(facade.contains("import type {"));
    assert!(facade.contains("GreetingStyle,"));
    assert!(!facade.contains("export const GreetingStyle"));
    assert!(!facade.contains("export { GreetingStyle"));
    assert!(!facade.contains("export const GreetingCounter"));
    assert!(!facade.contains("export *"));

    let declarations = render_index_d_ts(defs);
    assert!(declarations.contains("export interface Greeting"));
    assert!(declarations.contains("export type GreetingStyle"));
    assert!(declarations.contains("export interface GreetingEventText"));
    assert!(declarations.contains("export interface GreetingEventDone"));
    assert!(
        declarations.contains("export type GreetingEvent = GreetingEventText | GreetingEventDone;")
    );
    assert!(!declarations.contains("export type GreetingEvent = \n{"));
    assert!(declarations.contains("export declare class GreetingCounter"));
    assert!(declarations.contains("export declare function greet"));
    assert!(
        FacadeExports::from_type_defs(&[def("fn", "class", "function class(): void")]).is_err()
    );
}

fn test_harmony_stream_contract() -> (Vec<TypeDefLine>, Vec<HarmonyFacadeContract>) {
    const INPUT_SUFFIX: &str = "NumberStringFingerprint8b30e3aa815a2f4a";
    const INPUT_NEXT: &str = "UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext";
    let def = |kind: &str, name: &str, body: &str| TypeDefLine {
        kind: kind.into(),
        name: name.into(),
        def: body.into(),
        type_parameters: Vec::new(),
    };
    let defs = vec![
            def(
                "fn",
                "countEvents",
                "function countEvents(count: number): bigint",
            ),
            def(
                "fn",
                "countEventsStreamNext",
                "function countEventsStreamNext(handle: bigint): Promise<FixtureNext>",
            ),
            def(
                "fn",
                "countEventsStreamCancel",
                "function countEventsStreamCancel(handle: bigint): void",
            ),
            def(
                "interface",
                "FixtureNext",
                "kind: string\nvalue?: number\nerror?: string",
            ),
            def(
                "fn",
                "echoEvents",
                "function echoEvents(events: UniffiInputStream<UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext>): bigint",
            ),
            def(
                "fn",
                "echoEventsStreamNext",
                "function echoEventsStreamNext(handle: bigint): Promise<FixtureNext>",
            ),
            def(
                "fn",
                "echoEventsStreamCancel",
                "function echoEventsStreamCancel(handle: bigint): void",
            ),
            TypeDefLine {
                kind: "interface".into(),
                name: "UniffiInputStream".into(),
                def: "handle: number;\nnext(error: Error | null, handle: number): Promise<T>;\ncancel(error: Error | null, handle: number): void;".into(),
                type_parameters: vec!["T".into()],
            },
            def(
                "interface",
                INPUT_NEXT,
                "ok: boolean\ndone?: boolean\nvalue?: number\nerror?: string",
            ),
        ];
    let input = HarmonyInputStreamContract {
        suffix: INPUT_SUFFIX.into(),
        item_type: HarmonyTypeDescriptor::Number,
        error_type: HarmonyTypeDescriptor::String,
        next_type: INPUT_NEXT.into(),
        writer_class: format!("{INPUT_SUFFIX}InputWriter"),
        source_class: format!("{INPUT_SUFFIX}InputSource"),
        channel_class: format!("{INPUT_SUFFIX}InputChannel"),
        factory: format!("create{INPUT_SUFFIX}InputChannel"),
    };
    let output = |function: &str, args: Vec<HarmonyFacadeArgument>| {
        let mut chars = function.chars();
        let prefix = format!(
            "{}{}",
            chars.next().unwrap().to_ascii_uppercase(),
            chars.collect::<String>()
        );
        HarmonyOutputStreamContract {
            function: function.into(),
            next_function: format!("{function}StreamNext"),
            cancel_function: format!("{function}StreamCancel"),
            stream_factory: format!("{function}Stream"),
            pull_class: format!("{prefix}PullStream"),
            step_type: "FixtureNext".into(),
            item_type: HarmonyTypeDescriptor::Number,
            error_type: HarmonyTypeDescriptor::String,
            arguments: args,
        }
    };
    let contracts = vec![HarmonyFacadeContract {
        component: "fixture".into(),
        namespace: "fixture".into(),
        native_export_prefix: uniffi_bindgen::interface::native_export_prefix_for_component(
            "fixture",
        ),
        output_streams: vec![
            output(
                "countEvents",
                vec![HarmonyFacadeArgument {
                    name: "count".into(),
                    r#type: HarmonyTypeDescriptor::Number,
                }],
            ),
            output(
                "echoEvents",
                vec![HarmonyFacadeArgument {
                    name: "events".into(),
                    r#type: HarmonyTypeDescriptor::InputSource {
                        suffix: INPUT_SUFFIX.into(),
                        next_type: INPUT_NEXT.into(),
                    },
                }],
            ),
        ],
        input_streams: vec![input],
    }];
    (defs, contracts)
}

/// The rendering characterization fixture intentionally retains short raw
/// names so it can exercise the legacy compatibility projection directly.
/// Schema parsing, however, models the emitted sidecar and therefore needs
/// the component-prefixed native bridge names.
#[test]
fn structured_harmony_stream_contract_renders_reachable_arkts_facade() {
    let (defs, contracts) = test_harmony_stream_contract();
    let exports = FacadeExports::from_type_defs_and_contracts(&defs, contracts).unwrap();
    let facade = exports.render_native_facade("libfixture.so");
    let declarations = render_harmony_declaration_surfaces(&defs, &exports).package_public;
    let index = exports.render_package_index();

    for needle in [
            "export function countEventsStream(count: number): UniFfiStream<number>",
            "export function echoEventsStream(events: NumberStringFingerprint8b30e3aa815a2f4aInputSource): UniFfiStream<number>",
            "export function createNumberStringFingerprint8b30e3aa815a2f4aInputChannel()",
            "implements UniffiInputStream<UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext>",
            "readonly next = (_error: Error | null, handle: number)",
            "private state: number = __UNIFFI_STREAM_IDLE",
            "class __UniFfiStreamStep<T, E>",
            "function __uniffiOutputStepHasOnly(rawKeys: Array<string>, payload: string | null)",
            "__uniffiStrictOutputStep<number, string>(",
            "const rawKeys: Array<string> = Object.keys(raw);",
            "protected startNative(): bigint",
            "const raw: FixtureNext = await native.countEventsStreamNext(handle);",
            "raw stream step must be item, done, or error",
        ] {
            assert!(facade.contains(needle), "missing `{needle}` in:\n{facade}");
        }
    for needle in [
        "export interface UniFfiStreamResult<T>",
        "export interface UniFfiStream<T>",
        "export declare class UniFfiStreamFailure<E> extends Error {\n  nativeError: E;",
        "export declare function countEventsStream(count: number): UniFfiStream<number>",
        "write(item: number): Promise<void>",
        "fail(error: UniFfiInputFailure<string>): void",
    ] {
        assert!(
            declarations.contains(needle),
            "missing `{needle}` in:\n{declarations}"
        );
    }
    for name in [
        "countEventsStream",
        "echoEventsStream",
        "createNumberStringFingerprint8b30e3aa815a2f4aInputChannel",
        "UniFfiStreamFailure",
    ] {
        assert!(
            index.contains(name),
            "package index omits `{name}`:\n{index}"
        );
    }
    for raw in [
        "countEvents",
        "countEventsStreamNext",
        "countEventsStreamCancel",
        "echoEvents",
        "echoEventsStreamNext",
        "echoEventsStreamCancel",
    ] {
        assert!(
            !facade.contains(&format!("export const {raw}:"))
                && !index.contains(&format!("  {raw},\n"))
                && !declarations.contains(&format!("function {raw}(")),
            "public and adapter exports must hide output raw `{raw}`:\nfacade:\n{facade}\nindex:\n{index}\ndeclarations:\n{declarations}"
        );
    }
    for raw_type in ["FixtureNext"] {
        assert!(
            facade.contains(raw_type),
            "native facade must retain output next envelope `{raw_type}`:\n{facade}"
        );
        assert!(
            !index.contains(&format!("  {raw_type},\n")) && !declarations.contains(raw_type),
            "package root must hide output next envelope `{raw_type}` and its aliases:\nindex:\n{index}\ndeclarations:\n{declarations}"
        );
    }
    for arkts_forbidden in ["unknown", "Object.prototype", ".call("] {
        assert!(
            !facade.contains(arkts_forbidden),
            "generated ArkTS contains unsupported dynamic runtime feature `{arkts_forbidden}`:\n{facade}"
        );
    }
    for forbidden in [
        "countEventsEvents",
        "echoEventsEvents",
        "CountEventsEventsStream",
        "EchoEventsEventsStream",
        "__UniFfiEventsStream",
        "dataListeners",
        "errorListeners",
        "doneListeners",
        ".on(",
        ".off(",
    ] {
        assert!(
            !facade.contains(forbidden) && !declarations.contains(forbidden),
            "generated ArkTS contains forbidden Event facade `{forbidden}`"
        );
    }
    let last_import = facade
        .rfind("import ")
        .expect("facade must import native types");
    let first_export = facade.find("export ").expect("facade must export bindings");
    assert!(
        last_import < first_export,
        "ArkTS requires every import before declarations and exports:\n{facade}"
    );
}
#[test]
fn harmony_stream_contract_rejects_missing_raw_export_and_public_collision() {
    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.retain(|def| def.name != "countEventsStreamCancel");
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts.clone())
        .unwrap_err()
        .to_string();
    assert!(error.contains("countEventsStreamCancel"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.push(TypeDefLine {
        kind: "fn".into(),
        name: "countEventsStream".into(),
        def: "function countEventsStream(): void".into(),
        type_parameters: Vec::new(),
    });
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("collision") && error.contains("countEventsStream"),
        "{error}"
    );
}

#[test]
fn harmony_stream_contract_rejects_wrong_signatures_envelopes_and_duplicates() {
    for (name, replacement) in [
        ("countEvents", "function countEvents(count: string): bigint"),
        (
            "countEventsStreamNext",
            "function countEventsStreamNext(handle: bigint): FixtureNext",
        ),
        (
            "countEventsStreamCancel",
            "function countEventsStreamCancel(handle: number): void",
        ),
    ] {
        let (mut defs, contracts) = test_harmony_stream_contract();
        defs.iter_mut().find(|def| def.name == name).unwrap().def = replacement.into();
        let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("signature") || error.contains("raw callable"),
            "{error}"
        );
    }

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.iter_mut()
        .find(|def| def.name == "FixtureNext")
        .unwrap()
        .def = "kind: string\nvalue?: string".into();
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("envelope"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.iter_mut()
        .find(|def| def.name == "UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext")
        .unwrap()
        .def = "ok: boolean\ndone?: boolean\nvalue?: string\nerror?: string".into();
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("input next envelope"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    let duplicate = defs
        .iter()
        .find(|def| def.name == "countEvents")
        .unwrap()
        .clone();
    defs.push(duplicate);
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    let duplicate = defs
        .iter()
        .find(|def| def.name == "FixtureNext")
        .unwrap()
        .clone();
    defs.push(duplicate);
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("duplicate raw OHOS declaration"), "{error}");

    let (mut defs, contracts) = test_harmony_stream_contract();
    defs.iter_mut()
        .find(|def| def.name == "countEvents")
        .unwrap()
        .def = "function differentName(count: number): bigint".into();
    let error = FacadeExports::from_type_defs_and_contracts(&defs, contracts)
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match declaration"), "{error}");
}

#[test]
fn input_stream_interface_parser_is_semantic_unique_and_comment_safe() {
    let valid = r#"
export interface UniffiInputStream < T > {
  cancel(error: Error | null, handle: number): void
  handle: number;
  next(error: Error | null, handle: number): Promise<T>;
}
"#;
    validate_unique_input_stream_interface(valid).unwrap();

    for invalid in [
        r#"export interface UniffiInputStream<T> {
handle: string; next(error: Error | null, handle: string): Promise<string>;
cancel(error: Error | null, handle: string): string;
}
/* export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; } */"#,
        r#"const fake = "export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }";"#,
        r#"export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }
export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; extra:string; }"#,
        r#"export interface UniffiInputStream<T> { handle:number; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface UniffiInputStream<T> { handle:string; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):T; cancel(error:Error|null,handle:number):void; }"#,
        r#"export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):Promise<void>; }"#,
    ] {
        assert!(
            validate_unique_input_stream_interface(invalid).is_err(),
            "invalid declaration unexpectedly passed:\n{invalid}"
        );
    }
}

#[test]
fn input_stream_interface_parser_rejects_unbalanced_and_oversized_sources() {
    let valid = "export interface UniffiInputStream<T> { handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }";
    for invalid in [format!("{valid}}}"), format!("{{{valid}"), "}".to_string()] {
        assert!(
            validate_unique_input_stream_interface(&invalid).is_err(),
            "unbalanced declaration unexpectedly passed: {invalid}"
        );
    }

    let oversized_comment = format!("/*{}*/\n{valid}", "x".repeat(64 * 1024 + 1));
    assert!(validate_unique_input_stream_interface(&oversized_comment).is_err());

    let oversized_string = format!(
        "const ignored = \"{}\";\n{valid}",
        "x".repeat(64 * 1024 + 1)
    );
    assert!(validate_unique_input_stream_interface(&oversized_string).is_err());

    let oversized_identifier = format!("const {} = 1;\n{valid}", "x".repeat(513));
    assert!(validate_unique_input_stream_interface(&oversized_identifier).is_err());

    let deeply_nested = format!("{}{}{}", "{".repeat(129), "}".repeat(129), valid);
    assert!(validate_unique_input_stream_interface(&deeply_nested).is_err());

    let too_many_tokens = format!("{}\n{valid}", ";".repeat(65_537));
    assert!(validate_unique_input_stream_interface(&too_many_tokens).is_err());

    let oversized_body = format!(
            "export interface UniffiInputStream<T> {{ {} handle:number; next(error:Error|null,handle:number):Promise<T>; cancel(error:Error|null,handle:number):void; }}",
            ";".repeat(4097)
        );
    assert!(validate_unique_input_stream_interface(&oversized_body).is_err());

    let oversized_source = format!("{}\n{valid}", " ".repeat(1024 * 1024));
    assert!(validate_unique_input_stream_interface(&oversized_source).is_err());
}
#[test]
fn stream_argument_names_are_mangled_away_from_state_machine_members() {
    let (mut defs, mut contracts) = test_harmony_stream_contract();
    let names = [
        "handle",
        "nextNative",
        "cancelNative",
        "source",
        "state",
        "hasHandle",
        "pulling",
        "cancelSent",
        "cancelled",
        "failure",
    ];
    defs.iter_mut()
        .find(|def| def.name == "countEvents")
        .unwrap()
        .def = format!(
        "function countEvents({}): bigint",
        names
            .iter()
            .map(|name| format!("{name}: number"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    contracts[0].output_streams[0].arguments = names
        .into_iter()
        .map(|name| HarmonyFacadeArgument {
            name: name.into(),
            r#type: HarmonyTypeDescriptor::Number,
        })
        .collect();
    let exports = FacadeExports::from_type_defs_and_contracts(&defs, contracts).unwrap();
    let facade = exports.render_native_facade("libfixture.so");
    for index in 0..names.len() {
        assert!(
            facade.contains(&format!("private __arg{index}: number")),
            "{facade}"
        );
    }
    assert!(
        facade.contains(
            "return native.countEvents(this.__arg0, this.__arg1, this.__arg2, this.__arg3, this.__arg4, this.__arg5, this.__arg6, this.__arg7, this.__arg8, this.__arg9)"
        ),
        "{facade}"
    );
    assert!(
        facade.contains(
            "return new CountEventsPullStream(handle, nextNative, cancelNative, source, state, hasHandle, pulling, cancelSent, cancelled, failure)"
        ),
        "{facade}"
    );
    for member in [
        "private state: number = __UNIFFI_STREAM_IDLE",
        "private handle: bigint = 0n",
        "private hasHandle: boolean = false",
        "private pulling: boolean = false",
        "private cancelSent: boolean = false",
        "private cancelled: boolean = false",
        "private failure: Error | null = null",
    ] {
        assert!(facade.contains(member), "missing `{member}` in:\n{facade}");
    }
    for name in names {
        assert!(
            !facade.contains(&format!("private {name}: number;")),
            "{facade}"
        );
    }
}
fn test_host_facade_bundle() -> HostFacadeBundle {
    let component = "cache_fixture";
    let native_export_prefix =
        uniffi_bindgen::interface::native_export_prefix_for_component(component);
    let contract = HarmonyFacadeContract {
        component: component.into(),
        namespace: component.into(),
        native_export_prefix: native_export_prefix.clone(),
        output_streams: Vec::new(),
        input_streams: Vec::new(),
    };
    let raw_ping = format!("{native_export_prefix}_ping");
    let sidecar_content = format!(
        "type_def:{{\"kind\":\"fn\",\"name\":\"{raw_ping}\",\"def\":\"function {raw_ping}(): number\",\"typeParameters\":[]}}\n"
    );
    HostFacadeBundle {
        contracts: vec![HostFacadeBundleEntry {
            file: format!("{component}.ohos-facade.json"),
            content: serde_json::to_string(&contract).unwrap(),
        }],
        type_sidecars: vec![HostFacadeBundleEntry {
            file: format!("{component}.ohos-extra-types.d.ts"),
            content: sidecar_content,
        }],
        mode: FacadeBundleMode::Required,
    }
}

fn test_input_stream_host_facade_bundle() -> HostFacadeBundle {
    let component = "input_fixture";
    let native_export_prefix =
        uniffi_bindgen::interface::native_export_prefix_for_component(component);
    let suffix = "NumberStringFingerprint8b30e3aa815a2f4a".to_string();
    let input = HarmonyInputStreamContract {
        suffix: suffix.clone(),
        item_type: HarmonyTypeDescriptor::Number,
        error_type: HarmonyTypeDescriptor::String,
        next_type: uniffi_bindgen_javascript::flavors::napi::ohos_raw_input_next_type_for_prefix(
            &native_export_prefix,
            &suffix,
        ),
        writer_class: format!("{suffix}InputWriter"),
        source_class: format!("{suffix}InputSource"),
        channel_class: format!("{suffix}InputChannel"),
        factory: format!("create{suffix}InputChannel"),
    };
    let contract = HarmonyFacadeContract {
        component: component.into(),
        namespace: component.into(),
        native_export_prefix: native_export_prefix.clone(),
        output_streams: Vec::new(),
        input_streams: vec![input.clone()],
    };
    let raw_input_stream =
        uniffi_bindgen_javascript::flavors::napi::ohos_raw_input_stream_type_for_prefix(
            &native_export_prefix,
        );
    let type_def = |kind: &str, name: &str, definition: &str, type_parameters: Value| {
        format!(
            "type_def:{}\n",
            serde_json::to_string(&serde_json::json!({
                "kind": kind,
                "name": name,
                "def": definition,
                "typeParameters": type_parameters,
            }))
            .unwrap()
        )
    };
    let sidecar_content = [
        type_def(
            "interface",
            &raw_input_stream,
            "handle: number;\nnext(error: Error | null, handle: number): Promise<T>;\ncancel(error: Error | null, handle: number): void;",
            serde_json::json!(["T"]),
        ),
        type_def(
            "interface",
            &input.next_type,
            "ok: boolean\ndone?: boolean\nvalue?: number\nerror?: string",
            serde_json::json!([]),
        ),
    ]
    .concat();
    HostFacadeBundle {
        contracts: vec![HostFacadeBundleEntry {
            file: format!("{component}.ohos-facade.json"),
            content: serde_json::to_string(&contract).unwrap(),
        }],
        type_sidecars: vec![HostFacadeBundleEntry {
            file: format!("{component}.ohos-extra-types.d.ts"),
            content: sidecar_content,
        }],
        mode: FacadeBundleMode::Required,
    }
}

fn replace_test_bundle_sidecar(bundle: &mut HostFacadeBundle, sidecar_content: String) {
    bundle.type_sidecars[0].content = sidecar_content;
}

#[test]
fn input_stream_sidecar_generics_require_the_exact_owning_contract_name() {
    let root = temp_test_dir("uniffi-ohos-input-stream-sidecar-contract");
    let bundle_path = root.join("bundle.json");
    let valid = test_input_stream_host_facade_bundle();
    let native_export_prefix =
        uniffi_bindgen::interface::native_export_prefix_for_component("input_fixture");
    let raw_input_stream =
        uniffi_bindgen_javascript::flavors::napi::ohos_raw_input_stream_type_for_prefix(
            &native_export_prefix,
        );
    let valid_sidecar = valid.type_sidecars[0].content.clone();
    let raw_line = valid_sidecar
        .lines()
        .find(|line| line.contains(&format!("\"name\":\"{raw_input_stream}\"")))
        .unwrap()
        .to_string();

    write_required_host_facade_bundle(&bundle_path, &valid);
    load_host_facade_bundle(&FacadeMode::Required(bundle_path.clone())).unwrap();

    let cases = [
        (
            "bare legacy name",
            valid_sidecar.replacen(&raw_input_stream, "UniffiInputStream", 1),
            "only raw input-stream type",
        ),
        (
            "wrong component prefix",
            valid_sidecar.replacen(&raw_input_stream, "ffi_other_UniffiInputStream", 1),
            "only raw input-stream type",
        ),
        (
            "arbitrary generic",
            valid_sidecar.replacen(&raw_input_stream, "OtherGeneric", 1),
            "only raw input-stream type",
        ),
        (
            "wrong expected kind",
            valid_sidecar.replacen(
                &format!("\"kind\":\"interface\",\"name\":\"{raw_input_stream}\""),
                &format!("\"kind\":\"type\",\"name\":\"{raw_input_stream}\""),
                1,
            ),
            "must be an interface",
        ),
        (
            "wrong expected parameters",
            valid_sidecar.replacen(
                "\"typeParameters\":[\"T\"]",
                "\"typeParameters\":[\"T\",\"U\"]",
                1,
            ),
            "exact typeParameters",
        ),
        (
            "missing expected generic",
            valid_sidecar
                .lines()
                .filter(|line| line != &raw_line)
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
            "must declare exactly one raw input-stream type",
        ),
        (
            "duplicate expected generic",
            format!("{valid_sidecar}{raw_line}\n"),
            "must declare exactly one raw input-stream type",
        ),
    ];
    for (label, sidecar, expected) in cases {
        let mut bundle = valid.clone();
        replace_test_bundle_sidecar(&mut bundle, sidecar);
        write_required_host_facade_bundle(&bundle_path, &bundle);
        let error = load_host_facade_bundle(&FacadeMode::Required(bundle_path.clone()))
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{label}: {error}");
    }

    let mut no_input = test_host_facade_bundle();
    let no_input_prefix =
        uniffi_bindgen::interface::native_export_prefix_for_component("cache_fixture");
    let no_input_raw =
        uniffi_bindgen_javascript::flavors::napi::ohos_raw_input_stream_type_for_prefix(
            &no_input_prefix,
        );
    let no_input_sidecar = format!(
        "{}type_def:{{\"kind\":\"interface\",\"name\":\"{no_input_raw}\",\"def\":\"handle: number\",\"typeParameters\":[\"T\"]}}\n",
        no_input.type_sidecars[0].content
    );
    replace_test_bundle_sidecar(&mut no_input, no_input_sidecar);
    write_required_host_facade_bundle(&bundle_path, &no_input);
    let error = load_host_facade_bundle(&FacadeMode::Required(bundle_path.clone()))
        .unwrap_err()
        .to_string();
    assert!(error.contains("declares no inputStreams"), "{error}");
    std::fs::remove_dir_all(root).ok();
}
fn write_required_host_facade_bundle(path: &Utf8Path, bundle: &HostFacadeBundle) {
    let entries = |entries: &[HostFacadeBundleEntry]| {
        entries
            .iter()
            .map(|entry| {
                serde_json::json!({
                    "file": entry.file,
                    "content": entry.content,
                })
            })
            .collect::<Vec<_>>()
    };
    let value = serde_json::json!({
        "contracts": entries(&bundle.contracts),
        "typeSidecars": entries(&bundle.type_sidecars),
    });
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
}

#[test]
fn two_component_prefixed_bundle_loads_and_namespaces_same_short_exports() {
    // A composite host may contain components that deliberately expose the
    // same short UniFFI names.  The native sidecar has to use the component
    // prefix, while the package surface projects those names below the
    // component namespace instead of merging them into one flat export set.
    let component = |component: &str, namespace: &str| {
        let native_export_prefix =
            uniffi_bindgen::interface::native_export_prefix_for_component(component);
        let contract_content = serde_json::to_string(&HarmonyFacadeContract {
            component: component.into(),
            namespace: namespace.into(),
            native_export_prefix: native_export_prefix.clone(),
            output_streams: Vec::new(),
            input_streams: Vec::new(),
        })
        .unwrap();
        let raw_ping = format!("{native_export_prefix}_ping");
        let raw_shared = format!("{native_export_prefix}_Shared");
        let sidecar_content = format!(
            concat!(
                "type_def:{{\"kind\":\"interface\",\"name\":\"{raw_shared}\",",
                "\"def\":\"value: string\",\"typeParameters\":[]}}\n",
                "type_def:{{\"kind\":\"fn\",\"name\":\"{raw_ping}\",",
                "\"def\":\"function {raw_ping}(): {raw_shared}\",\"typeParameters\":[]}}\n",
            ),
            raw_shared = raw_shared,
            raw_ping = raw_ping,
        );
        (
            HostFacadeBundleEntry {
                file: format!("{component}.ohos-facade.json"),
                content: contract_content,
            },
            HostFacadeBundleEntry {
                file: format!("{component}.ohos-extra-types.d.ts"),
                content: sidecar_content,
            },
        )
    };
    // `ffi_a` is a textual prefix of `ffi_a_b`; ownership must come from the
    // exact component→sidecar binding rather than longest-prefix guessing.
    let (alpha_contract, alpha_sidecar) = component("a", "alpha");
    let (beta_contract, beta_sidecar) = component("a_b", "beta");
    let bundle = HostFacadeBundle {
        contracts: vec![alpha_contract, beta_contract],
        type_sidecars: vec![alpha_sidecar, beta_sidecar],
        mode: FacadeBundleMode::Required,
    };

    let root = temp_test_dir("uniffi-ohos-composite-prefixed-facade");
    let bundle_path = root.join("composite-host.ohos-facade-bundle.json");
    write_required_host_facade_bundle(&bundle_path, &bundle);
    let loaded = load_host_facade_bundle(&FacadeMode::Required(bundle_path)).unwrap();
    let contracts = loaded
        .contracts
        .iter()
        .map(|contract| {
            parse_harmony_facade_contract(
                contract.content.as_bytes(),
                Utf8Path::new(&contract.file),
            )
        })
        .collect::<Result<Vec<_>>>()
        .unwrap();
    let owned_defs = parse_owned_bundle_type_defs(&loaded, &root, &contracts).unwrap();
    let exports = FacadeExports::from_owned_type_defs_and_contracts(owned_defs, contracts).unwrap();
    let index = exports.render_package_index();
    assert!(index.contains("import * as alpha from"), "{index}");
    assert!(index.contains("import * as beta from"), "{index}");
    assert!(
        index.contains("export { alpha };") && index.contains("export { beta };"),
        "{index}"
    );
    assert!(!index.contains("export *"), "{index}");
    assert!(!index.contains("ffi_a_ping"), "{index}");
    assert!(!index.contains("ffi_a_b_ping"), "{index}");
    let modules = exports.component_modules().unwrap();
    let alpha = modules
        .iter()
        .find(|module| module.namespace == "alpha")
        .unwrap();
    let beta = modules
        .iter()
        .find(|module| module.namespace == "beta")
        .unwrap();
    for (module, native_export_prefix) in [
        (
            alpha,
            uniffi_bindgen::interface::native_export_prefix_for_component("a"),
        ),
        (
            beta,
            uniffi_bindgen::interface::native_export_prefix_for_component("a_b"),
        ),
    ] {
        assert!(
            module.source.contains(&format!(
                "export const ping: () => Shared = {native_export_prefix}_ping;"
            )),
            "{}",
            module.source
        );
        assert!(
            module.declarations.contains("export type Shared ="),
            "{}",
            module.declarations
        );
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn required_host_facade_bundle_requires_contracts_and_sidecars_but_not_host_identity() {
    let package = test_host_package("cache-host", "1.0.0", "cache_host");
    let bundle = test_host_facade_bundle();

    // This is the explicit no-stream shape: the host still binds a real
    // component and its empty-stream contract instead of guessing from an
    // absent bundle.
    validate_host_facade_bundle_for_package(&bundle, &package).unwrap();

    let mut missing_sidecar = bundle.clone();
    missing_sidecar.type_sidecars.clear();
    assert!(validate_host_facade_bundle_for_package(&missing_sidecar, &package).is_err());

    let mut missing_contract = bundle.clone();
    missing_contract.contracts.clear();
    assert!(validate_host_facade_bundle_for_package(&missing_contract, &package).is_err());

    let mut other_host = package.clone();
    other_host.cargo_package_id = "cache-host 1.0.0 (other source)".into();
    other_host.name = "other-cache-host".into();
    validate_host_facade_bundle_for_package(&bundle, &other_host).unwrap();
}

#[test]
fn harmony_stream_facade_runtime_state_machines() {
    let available = Command::new("node")
        .args(["--experimental-strip-types", "--eval", ""])
        .output()
        .expect("Node with --experimental-strip-types is required for the Harmony runtime test");
    assert!(
        available.status.success(),
        "Node --experimental-strip-types is required for the Harmony runtime test\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&available.stdout),
        String::from_utf8_lossy(&available.stderr)
    );

    let (defs, contracts) = test_harmony_stream_contract();
    let mut exports = FacadeExports::from_type_defs_and_contracts(&defs, contracts).unwrap();
    exports.streams.native_types.clear();
    let mut facade = exports.render_native_facade("libfixture.so");
    let native_stub = r#"interface FixtureNext {
  kind: string;
  value?: number | null;
  error?: string;
}
interface InputNext {
  ok: boolean;
  done?: boolean;
  value?: number;
  error?: string;
}
type UniffiInputStreamNumberStringFingerprint8b30e3aa815a2f4aNext = InputNext;
interface UniffiInputStream<N> {
  handle: number;
  next(error: Error | null, handle: number): Promise<N>;
  cancel(error: Error | null, handle: number): void;
}
class __StubState {
  values: Array<number | null> = new Array<number | null>();
  pulls: number = 0;
  typed: boolean = false;
  malformed: boolean = false;
  extraMalformedKind: string = '';
  reject: boolean = false;
  delayed: boolean = false;
  source: UniffiInputStream<InputNext> | null = null;
}
const __typedError = { variant: 'StorageInvalidated', data: { generation: 7 }, message: 'typed failure' };
const __startError: Error = new Error('start failure');
let __startCancelCallback: (() => void) | null = null;
let __nextHandle: bigint = 1n;
let __startCalls: number = 0;
let __nextCalls: number = 0;
let __cancelCalls: number = 0;
const __states: Map<bigint, __StubState> = new Map<bigint, __StubState>();
const native = {
  countEvents(count: number): bigint {
    __startCalls += 1;
    if (count === 82 || count === 83) {
      if (__startCancelCallback !== null) {
        __startCancelCallback();
      }
      if (count === 82) {
        throw new Error('cancelled start failure');
      }
    }
    if (count === 84) {
      throw __startError;
    }
    const handle: bigint = __nextHandle;
    __nextHandle += 1n;
    const state: __StubState = new __StubState();
    if (count === 98) {
      state.typed = true;
    } else if (count === 97) {
      state.values.push(null);
    } else if (count === 86) {
      state.malformed = true;
    } else if (count === 87) {
      state.extraMalformedKind = 'item';
    } else if (count === 88) {
      state.extraMalformedKind = 'done';
    } else if (count === 89) {
      state.extraMalformedKind = 'error';
    } else if (count === 85) {
      state.reject = true;
    } else {
      state.delayed = count === 77;
      for (let index: number = 0; index < count; index += 1) {
        state.values.push(index);
      }
    }
    __states.set(handle, state);
    return handle;
  },
  countEventsStreamNext(handle: bigint): Promise<FixtureNext> {
    __nextCalls += 1;
    const state: __StubState = __states.get(handle) as __StubState;
    state.pulls += 1;
    if (state.reject) {
      return Promise.reject(new Error('adapter rejection'));
    }
    if (state.malformed) {
      return Promise.resolve({ kind: 'item', value: 1, error: 'extra' });
    }
    if (state.extraMalformedKind === 'item') {
      return Promise.resolve({ kind: 'item', value: 1, extra: true } as FixtureNext);
    }
    if (state.extraMalformedKind === 'done') {
      return Promise.resolve({ kind: 'done', extra: true } as FixtureNext);
    }
    if (state.extraMalformedKind === 'error') {
      return Promise.resolve({ kind: 'error', error: 'extra', extra: true } as FixtureNext);
    }
    if (state.typed) {
      state.typed = false;
      __states.delete(handle);
      return Promise.resolve({ kind: 'error', error: __typedError as unknown as string });
    }
    if (state.values.length === 0) {
      __states.delete(handle);
      return Promise.resolve({ kind: 'done' });
    }
    const result: FixtureNext = { kind: 'item', value: state.values[0] };
    state.values.splice(0, 1);
    if (state.delayed && state.pulls === 1) {
      return new Promise<FixtureNext>((resolve): void => {
        setTimeout((): void => resolve(result), 20);
      });
    }
    return Promise.resolve(result);
  },
  countEventsStreamCancel(handle: bigint): void {
    __cancelCalls += 1;
    __states.delete(handle);
  },
  echoEvents(source: UniffiInputStream<InputNext>): bigint {
    __startCalls += 1;
    const handle: bigint = __nextHandle;
    __nextHandle += 1n;
    const state: __StubState = new __StubState();
    state.source = source;
    __states.set(handle, state);
    return handle;
  },
  async echoEventsStreamNext(handle: bigint): Promise<FixtureNext> {
    __nextCalls += 1;
    const state: __StubState = __states.get(handle) as __StubState;
    const source: UniffiInputStream<InputNext> = state.source as UniffiInputStream<InputNext>;
    const input: InputNext = await source.next(null, source.handle);
    if (!input.ok) {
      __states.delete(handle);
      return { kind: 'error', error: input.error as string };
    }
    if (input.done === true) {
      __states.delete(handle);
      return { kind: 'done' };
    }
    return { kind: 'item', value: input.value };
  },
  echoEventsStreamCancel(handle: bigint): void {
    __cancelCalls += 1;
    const state: __StubState | undefined = __states.get(handle);
    if (state !== undefined && state.source !== null) {
      state.source.cancel(null, state.source.handle);
    }
    __states.delete(handle);
  }
};
export function __testStartCalls(): number { return __startCalls; }
export function __testNextCalls(): number { return __nextCalls; }
export function __testCancelCalls(): number { return __cancelCalls; }
export function __testRegistrySize(): number { return __states.size; }
export function __testTypedError(): object { return __typedError; }
export function __testStartError(): Error { return __startError; }
export function __testSetStartCancelCallback(callback: (() => void) | null): void { __startCancelCallback = callback; }
"#;
    facade = facade.replace("import native from \"libfixture.so\";\n\n", native_stub);
    facade = facade.replace(
        "import type { BusinessError, Callback, ErrorCallback } from \"@kit.BasicServicesKit\";",
        "interface BusinessError<T = void> extends Error { code: number; data?: T; }\ntype Callback<T> = (data: T) => void;\ntype ErrorCallback<T extends Error = BusinessError<void>> = (error: T) => void;",
    );

    let driver = r#"import {
  UniFfiInputFailure,
  UniFfiStreamFailure,
  countEventsStream,
  createNumberStringFingerprint8b30e3aa815a2f4aInputChannel,
  echoEventsStream,
  __testCancelCalls,
  __testNextCalls,
  __testRegistrySize,
  __testSetStartCancelCallback,
  __testStartCalls,
  __testStartError,
  __testTypedError
} from './facade.ts';

function assert(condition: boolean, message: string): void {
  if (!condition) throw new Error(message);
}
function delay(ms: number): Promise<void> {
  return new Promise<void>((resolve): void => { setTimeout(resolve, ms); });
}

const lazyStarts: number = __testStartCalls();
const lazyPulls: number = __testNextCalls();
const lazy = countEventsStream(2);
assert(__testStartCalls() === lazyStarts && __testNextCalls() === lazyPulls,
  'factory called raw start or next');
const first = await lazy.next();
assert(first.done === false && first.value === 0, 'first pull lost its item');
assert(__testStartCalls() === lazyStarts + 1 && __testNextCalls() === lazyPulls + 1,
  'first next did not perform exactly one start and pull');
const second = await lazy.next();
assert(second.done === false && second.value === 1, 'second pull lost its item');
assert(__testNextCalls() === lazyPulls + 2, 'item consumption prefetched a raw next');
const done = await lazy.next();
assert(done.done === true && __testNextCalls() === lazyPulls + 3, 'raw Done was not pulled once');
const doneCancels: number = __testCancelCalls();
assert((await lazy.next()).done === true, 'Done stream restarted');
await lazy.cancel();
await lazy.cancel();
assert(__testCancelCalls() === doneCancels, 'Done stream called raw cancel');

const optional = countEventsStream(97);
const optionalItem = await optional.next();
assert(optionalItem.done === false && optionalItem.value === null, 'Optional null item was treated as Done');
assert((await optional.next()).done === true, 'Optional stream did not complete after null item');

const typed = countEventsStream(98);
const typedCancels: number = __testCancelCalls();
const typedPulls: number = __testNextCalls();
let typedObserved: Error | null = null;
try {
  await typed.next();
} catch (error) {
  typedObserved = error as Error;
}
assert(typedObserved instanceof UniFfiStreamFailure
  && typedObserved.nativeError === __testTypedError(),
  'typed Rust error did not preserve native variant/data identity');
assert(__testCancelCalls() === typedCancels, 'typed Error called raw cancel after Rust terminal cleanup');
try {
  await typed.next();
} catch (error) {
  assert(error === typedObserved, 'failed stream did not preserve its typed terminal error');
}
assert(__testNextCalls() === typedPulls + 1 && __testRegistrySize() === 0,
  'typed Error performed a post-terminal raw next or leaked its registry entry');

const idle = countEventsStream(1);
const idleStarts: number = __testStartCalls();
const idleCancels: number = __testCancelCalls();
await idle.cancel();
await idle.cancel();
assert(__testStartCalls() === idleStarts && __testCancelCalls() === idleCancels,
  'idle cancel called raw start or cancel');
assert((await idle.next()).done === true, 'idle cancel did not remain terminal');

const active = countEventsStream(77);
const activeCancels: number = __testCancelCalls();
const activePending = active.next();
await active.cancel();
await active.cancel();
assert(__testCancelCalls() === activeCancels + 1, 'active cancel was not exactly once');
assert((await activePending).done === true, 'active cancel leaked an in-flight item');
assert((await active.next()).done === true, 'cancelled stream restarted');
assert(__testRegistrySize() === 0, 'active cancel leaked a registry entry');

const starting = countEventsStream(83);
const startingStarts: number = __testStartCalls();
const startingCancels: number = __testCancelCalls();
__testSetStartCancelCallback((): void => { void starting.cancel(); });
const startingDone = await starting.next();
__testSetStartCancelCallback(null);
assert(startingDone.done === true, 'STARTING cancellation did not finish terminally');
assert(__testStartCalls() === startingStarts + 1 && __testCancelCalls() === startingCancels + 1,
  'STARTING cancellation did not cancel the published native handle exactly once');
assert((await starting.next()).done === true && __testRegistrySize() === 0,
  'STARTING cancellation restarted or leaked its native handle');

const cancelledStartFailure = countEventsStream(82);
const cancelledStartAttempts: number = __testStartCalls();
const cancelledStartCancels: number = __testCancelCalls();
__testSetStartCancelCallback((): void => { void cancelledStartFailure.cancel(); });
const cancelledStartDone = await cancelledStartFailure.next();
__testSetStartCancelCallback(null);
assert(cancelledStartDone.done === true, 'cancelled throwing start did not preserve cancellation terminal state');
assert(__testStartCalls() === cancelledStartAttempts + 1
  && __testCancelCalls() === cancelledStartCancels,
  'cancelled throwing start fabricated a native cancel without a handle');
assert((await cancelledStartFailure.next()).done === true,
  'cancelled throwing start retried native start');

const concurrent = countEventsStream(77);
const concurrentPulls: number = __testNextCalls();
const firstConcurrent = concurrent.next();
let concurrentName: string = '';
try {
  await concurrent.next();
} catch (error) {
  concurrentName = (error as Error).name;
}
assert(concurrentName === 'UniFfiStreamConcurrentNext', 'concurrent next did not explicitly fail');
assert(__testNextCalls() === concurrentPulls + 1, 'concurrent next called raw next twice');
await concurrent.cancel();
await firstConcurrent;

const malformed = countEventsStream(86);
const malformedCancels: number = __testCancelCalls();
let malformedName: string = '';
try {
  await malformed.next();
} catch (error) {
  malformedName = (error as Error).name;
}
assert(malformedName === 'UniFfiStreamProtocolError', 'malformed step was accepted');
assert(__testCancelCalls() === malformedCancels + 1 && __testRegistrySize() === 0,
  'malformed step did not clean up its active handle exactly once');
const malformedPulls: number = __testNextCalls();
try {
  await malformed.next();
} catch (_error) {
}
assert(__testNextCalls() === malformedPulls, 'malformed terminal state called raw next');

async function assertExtraOwnKeyProtocolFailure(count: number): Promise<void> {
  const stream = countEventsStream(count);
  const cancelsBefore: number = __testCancelCalls();
  const pullsBefore: number = __testNextCalls();
  let failureName: string = '';
  try {
    await stream.next();
  } catch (error) {
    failureName = (error as Error).name;
  }
  assert(failureName === 'UniFfiStreamProtocolError',
    `extra own key for stream ${count} was accepted`);
  assert(__testCancelCalls() === cancelsBefore + 1 && __testRegistrySize() === 0,
    `extra own key for stream ${count} did not cancel its active handle exactly once`);
  assert(__testNextCalls() === pullsBefore + 1,
    `extra own key for stream ${count} did not execute exactly one raw pull`);
  const terminalPulls: number = __testNextCalls();
  try {
    await stream.next();
  } catch (_error) {
  }
  assert(__testNextCalls() === terminalPulls,
    `extra own key for stream ${count} made a post-terminal raw pull`);
}

await assertExtraOwnKeyProtocolFailure(87);
await assertExtraOwnKeyProtocolFailure(88);
await assertExtraOwnKeyProtocolFailure(89);

const adapterFailure = countEventsStream(85);
const adapterCancels: number = __testCancelCalls();
let adapterMessage: string = '';
try {
  await adapterFailure.next();
} catch (error) {
  adapterMessage = (error as Error).message;
}
assert(adapterMessage === 'adapter rejection', 'adapter rejection was swallowed or replaced');
assert(__testCancelCalls() === adapterCancels + 1 && __testRegistrySize() === 0,
  'adapter rejection did not clean up its active handle exactly once');

const startFailure = countEventsStream(84);
const startCancels: number = __testCancelCalls();
const startAttempts: number = __testStartCalls();
let startObserved: Error | null = null;
try {
  await startFailure.next();
} catch (error) {
  startObserved = error as Error;
}
assert(startObserved === __testStartError(), 'start failure did not preserve the native Error identity');
assert(__testStartCalls() === startAttempts + 1 && __testCancelCalls() === startCancels,
  'start failure fabricated a raw handle or cancel');
try {
  await startFailure.next();
} catch (error) {
  assert(error === startObserved, 'start failure terminal state changed its native Error identity');
}
assert(__testStartCalls() === startAttempts + 1, 'failed start retried native start');

const channel = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
let writeResolved: boolean = false;
const write = channel.writer.write(21).then((): void => { writeResolved = true; });
await Promise.resolve();
assert(!writeResolved, 'input write resolved before rendezvous pull');
const input = await channel.source.next(null, channel.source.handle);
await write;
assert(input.ok && input.value === 21, 'input rendezvous lost its item');
channel.writer.end();
const inputDone = await channel.source.next(null, channel.source.handle);
assert(inputDone.ok && inputDone.done === true, 'input end did not produce Done');
let closedName: string = '';
try {
  await channel.writer.write(22);
} catch (error) {
  closedName = (error as Error).name;
}
assert(closedName === 'UniFfiInputClosedError', 'closed input reused the deleted output BusinessError');

const bidiChannel = createNumberStringFingerprint8b30e3aa815a2f4aInputChannel();
const bidi = echoEventsStream(bidiChannel.source);
const bidiStarts: number = __testStartCalls();
const bidiWrite = bidiChannel.writer.write(31);
const bidiItem = await bidi.next();
await bidiWrite;
assert(bidiItem.done === false && bidiItem.value === 31,
  'input/output Pull bridge did not preserve rendezvous item');
assert(__testStartCalls() === bidiStarts + 1, 'bidi factory was not lazy');
bidiChannel.writer.end();
assert((await bidi.next()).done === true, 'bidi Pull did not complete');
assert(__testRegistrySize() === 0, 'bidi Pull leaked a native registry entry');

console.log('harmony-stream-runtime-ok');
"#;

    let root = temp_test_dir("uniffi-harmony-stream-runtime");
    std::fs::write(root.join("facade.ts"), facade).unwrap();
    std::fs::write(root.join("driver.ts"), driver).unwrap();
    let output = Command::new("node")
        .current_dir(&root)
        .args(["--experimental-strip-types", "driver.ts"])
        .output()
        .expect("Node is required to execute the Harmony stream runtime test");
    assert!(
        output.status.success(),
        "Harmony stream runtime driver failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("harmony-stream-runtime-ok"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn rejects_noncanonical_ohos_type_definition_files() {
    let content = r#"{"kind":"fn","name":"welcomeAgent","def":"function welcomeAgent(agentName: string): string"}"#;
    let error = parse_canonical_ohos_type_defs(
        content.as_bytes(),
        "fixture.ohos-extra-types.d.ts",
        &RawInputStreamTypeExpectation {
            name: "ffi_fixture_UniffiInputStream".into(),
            required: false,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("exact `type_def:` envelope"), "{error}");
}

#[test]
fn renders_har_package_metadata_and_entry_templates() {
    let metadata = test_package_metadata();
    let package_json = render_oh_package_json5(
        &metadata,
        "uni_core_ohos",
        &["libuni_core_ohos.so".to_string()],
        PackageKind::Har,
    )
    .unwrap();
    let package: Value = serde_json::from_str(&package_json).unwrap();
    assert_eq!(package["name"], "@scope/demo");
    assert_eq!(package["main"], "Index.ets");
    assert_eq!(package["types"], "Index.d.ets");
    assert_eq!(
        package["dependencies"]["libuni_core_ohos.so"],
        "file:./src/main/cpp/types/libuni_core_ohos"
    );
    assert_eq!(package["compatibleSdkVersion"], "22");
    assert_eq!(package["compatibleSdkType"], "HarmonyOS");
    assert_eq!(
        package["nativeComponents"][0]["name"],
        "libuni_core_ohos.so"
    );
    assert_eq!(package["nativeComponents"][0]["compatibleSdkVersion"], "22");
    assert_eq!(package["obfuscated"], false);
    assert_eq!(package["artifactType"], "original");

    let module: Value =
        serde_json::from_str(&render_module_json5(&metadata, PackageKind::Har).unwrap()).unwrap();
    assert_eq!(module["module"]["type"], "har");
    assert_eq!(module["module"]["name"], "demo_native");
    assert_eq!(
        module["module"]["deviceTypes"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );

    let profile: Value = serde_json::from_str(
        &render_build_profile_json5(&metadata, PackageKind::Har, false).unwrap(),
    )
    .unwrap();
    assert_eq!(profile["apiType"], "stageMode");
    assert_eq!(profile["targets"][0]["name"], "default");
    assert_eq!(
        profile["targets"][0]["config"]["deviceType"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );
}

#[test]
fn copies_fake_dist_into_package_libs() {
    let root = temp_test_dir("uniffi-ohos-copy-dist");
    let dist = root.join("dist");
    let libs = root.join("package/libs");
    std::fs::create_dir_all(dist.join("arm64-v8a")).unwrap();
    std::fs::create_dir_all(dist.join("x86_64")).unwrap();
    std::fs::create_dir_all(dist.join("component-facades")).unwrap();
    std::fs::write(
        dist.join("native-facade.d.ts"),
        "export declare const add: (a: number, b: number) => number;\n",
    )
    .unwrap();
    std::fs::write(dist.join("arm64-v8a/libuni_core_ohos.so"), "arm").unwrap();
    std::fs::write(dist.join("x86_64/libuni_core_ohos.so"), "x64").unwrap();
    std::fs::write(
        dist.join("component-facades/demo.ets"),
        "export const demo = 1;\n",
    )
    .unwrap();
    std::fs::write(
        dist.join("component-facades/demo.d.ets"),
        "export declare const demo: number;\n",
    )
    .unwrap();

    copy_dist_to_package_libs(&dist, &libs, false).unwrap();
    assert!(libs.join("index.d.ts").exists());
    assert!(libs.join("arm64-v8a/libuni_core_ohos.so").exists());
    assert!(libs.join("x86_64/libuni_core_ohos.so").exists());
    assert!(
        !libs.join("component-facades").exists(),
        "ArkTS component sources must not enter the native ABI inventory"
    );

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn component_facade_declarations_use_only_arkts_d_ets_through_staging() {
    let root = temp_test_dir("uniffi-ohos-component-declaration-extension");
    let dist = root.join("dist");
    let package = root.join("package");
    let modules = vec![FacadeComponentModule {
        namespace: "fixture".into(),
        source: "export const ping = 1;\n".into(),
        declarations: "export declare const ping: number;\n".into(),
    }];

    write_component_facade_modules(&dist, &modules).unwrap();
    let generated = dist.join("component-facades");
    assert!(generated.join("fixture.ets").is_file());
    assert!(generated.join("fixture.d.ets").is_file());
    assert!(
        !generated.join("fixture.d.ts").exists(),
        "component declarations must not emit a legacy .d.ts compatibility copy"
    );

    stage_component_facade_modules(&dist, &package).unwrap();
    let staged = package.join("src/main/ets/components");
    assert_eq!(
        std::fs::read_to_string(staged.join("fixture.ets")).unwrap(),
        modules[0].source
    );
    assert_eq!(
        std::fs::read_to_string(staged.join("fixture.d.ets")).unwrap(),
        modules[0].declarations
    );
    assert!(
        !staged.join("fixture.d.ts").exists(),
        "staging must not synthesize a legacy .d.ts component declaration"
    );

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn skip_libs_keeps_types_and_facade_without_copying_native_binaries() {
    let root = temp_test_dir("uniffi-ohos-skip-package-libs");
    let dist = write_fake_dist(&root, "demo_ohos");
    let package_dir = root.join("package");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::write(package_dir.join("stale.txt"), "stale").unwrap();
    let error = stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        true,
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("creation-time witness"));
    assert_eq!(
        std::fs::read_to_string(package_dir.join("stale.txt")).unwrap(),
        "stale"
    );
    std::fs::remove_dir_all(&package_dir).unwrap();
    stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        true,
    )
    .unwrap();

    assert!(package_dir.join("libs/index.d.ts").exists());
    assert!(!package_dir.join("stale.txt").exists());
    assert!(!package_dir.join("libs/arm64-v8a/libdemo_ohos.so").exists());
    assert!(package_dir
        .join("src/main/cpp/types/libdemo_ohos/index.d.ts")
        .exists());
    assert!(package_dir.join("src/main/ets/native-facade.ets").exists());
    assert!(!package_dir.join("src/main/ets/common").exists());
    let package: Value = serde_json::from_str(
        &std::fs::read_to_string(package_dir.join("oh-package.json5")).unwrap(),
    )
    .unwrap();
    assert_eq!(package["nativeComponents"], serde_json::json!([]));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn native_components_match_deduped_multi_abi_staged_so_set() {
    let root = temp_test_dir("uniffi-ohos-native-components");
    let dist = write_fake_dist(&root, "demo_ohos");
    std::fs::create_dir_all(dist.join("x86_64")).unwrap();
    for (abi, names) in [
        ("arm64-v8a", vec!["libdependency.so", "libc++_shared.so"]),
        (
            "x86_64",
            vec!["libdemo_ohos.so", "libdependency.so", "libc++_shared.so"],
        ),
    ] {
        for name in names {
            std::fs::write(dist.join(abi).join(name), format!("{abi}:{name}")).unwrap();
        }
    }
    let package_dir = root.join("package");
    stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        false,
    )
    .unwrap();
    let components = collect_staged_native_components(&package_dir.join("libs")).unwrap();
    assert_eq!(
        components,
        vec![
            "libc++_shared.so".to_string(),
            "libdemo_ohos.so".to_string(),
            "libdependency.so".to_string(),
        ]
    );
    let package: Value = serde_json::from_str(
        &std::fs::read_to_string(package_dir.join("oh-package.json5")).unwrap(),
    )
    .unwrap();
    let declared = package["nativeComponents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|component| component["name"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(declared, components);
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn package_staging_rejects_symlinked_native_artifact_directories() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-symlink-dist");
    let dist = root.join("dist");
    let outside = root.join("outside");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(dist.join("native-facade.d.ts"), "export {};\n").unwrap();
    symlink(&outside, dist.join("arm64-v8a")).unwrap();
    let error = copy_dist_to_package_libs(&dist, &root.join("package/libs"), false)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symlink"));
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn copies_ohos_cxx_runtime_next_to_native_libs() {
    let root = temp_test_dir("uniffi-ohos-copy-cxx-runtime");
    let ndk = root.join("ndk");
    let source_dir = ndk
        .join("native")
        .join("llvm")
        .join("lib")
        .join("aarch64-linux-ohos");
    let arch_dist = root.join("dist").join("arm64-v8a");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&arch_dist).unwrap();
    std::fs::write(source_dir.join("libc++_shared.so"), "cxx").unwrap();

    copy_ohos_cxx_shared(ndk.as_str(), Arch::Arm64, &arch_dist).unwrap();

    assert_eq!(
        std::fs::read_to_string(arch_dist.join("libc++_shared.so")).unwrap(),
        "cxx"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn generates_har_with_package_root_and_no_absolute_paths() {
    let root = temp_test_dir("uniffi-ohos-har");
    let dist = root.join("dist");
    let package_dir = root.join("package");
    std::fs::create_dir_all(dist.join("arm64-v8a")).unwrap();
    std::fs::write(
        dist.join("native-facade.d.ts"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
        dist.join("Index.d.ets"),
        "export declare function welcomeAgent(name: string): string;\n",
    )
    .unwrap();
    std::fs::write(
            dist.join("native-facade.ets"),
            "import native from \"libdemo_ohos.so\";\nexport const welcomeAgent = native.welcomeAgent;\nexport default native;\n",
        )
        .unwrap();
    std::fs::write(
            dist.join("Index.ets"),
            "export { welcomeAgent } from \"./src/main/ets/native-facade\";\nexport { default } from \"./src/main/ets/native-facade\";\n",
        )
        .unwrap();
    std::fs::write(dist.join("arm64-v8a/libdemo_ohos.so"), "fake").unwrap();

    stage_har_package(
        &dist,
        &package_dir,
        "demo_ohos",
        &test_package_metadata(),
        false,
    )
    .unwrap();
    assert!(package_dir.join("Index.ets").exists());
    assert!(package_dir.join("build-profile.json5").exists());
    assert!(!package_dir.join("harmony-facade-contract.json").exists());
    assert!(package_dir.join("src/main/ets/native-facade.ets").exists());
    assert!(package_dir
        .join("src/main/cpp/types/libdemo_ohos/index.d.ts")
        .exists());
    assert!(package_dir
        .join("src/main/cpp/types/libdemo_ohos/oh-package.json5")
        .exists());
    assert!(!package_dir
        .join("src/main/cpp/types/libdemo_ohos/harmony-facade-contract.json")
        .exists());
    let index = std::fs::read_to_string(package_dir.join("Index.ets")).unwrap();
    assert!(!index.contains(".so"));
    assert!(!index.contains("export *"));
    let facade =
        std::fs::read_to_string(package_dir.join("src/main/ets/native-facade.ets")).unwrap();
    assert!(facade.contains("import native from \"libdemo_ohos.so\""));
    assert!(!facade.contains("export *"));

    let har = root.join("demo.har");
    generate_har_archive(&har, &package_dir).unwrap();

    let file = std::fs::File::open(&har).unwrap();
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    let mut saw_package_json = false;
    let mut saw_module_json5 = false;
    let mut saw_index = false;
    let mut saw_build_profile = false;
    let mut saw_native_types = false;
    for entry in archive.entries().unwrap() {
        let entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().to_string();
        assert!(!path.starts_with('/'), "entry must not be absolute: {path}");
        assert!(
            !path.contains(":\\"),
            "entry must not include windows absolute path: {path}"
        );
        assert!(
            path == "package" || path.starts_with("package/"),
            "entry must be rooted at package/: {path}"
        );
        if path == "package/oh-package.json5" {
            saw_package_json = true;
        }
        if path == "package/src/main/module.json5" {
            saw_module_json5 = true;
        }
        if path == "package/Index.ets" {
            saw_index = true;
        }
        if path == "package/build-profile.json5" {
            saw_build_profile = true;
        }
        if path == "package/src/main/cpp/types/libdemo_ohos/index.d.ts" {
            saw_native_types = true;
        }
        assert!(!path.ends_with("harmony-facade-contract.json"));
    }
    assert!(
        saw_package_json,
        "HAR must contain package/oh-package.json5"
    );
    assert!(
        saw_module_json5,
        "HAR must contain package/src/main/module.json5"
    );
    assert!(saw_index, "HAR must contain package/Index.ets");
    assert!(
        saw_build_profile,
        "HAR must contain package/build-profile.json5"
    );
    assert!(
        saw_native_types,
        "HAR must contain the native type dependency declaration"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn har_archive_rejects_output_inside_staging_tree() {
    let root = temp_test_dir("uniffi-ohos-har-output-traversal");
    let package_dir = root.join("package");
    std::fs::create_dir_all(&package_dir).unwrap();
    std::fs::write(package_dir.join("Index.ets"), "export {};\n").unwrap();
    let error = generate_har_archive(&package_dir.join("nested.har"), &package_dir)
        .unwrap_err()
        .to_string();
    assert!(error.contains("must not be inside"));
    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn production_package_preflight_rejects_staging_outputs_before_mutation() {
    use std::cell::Cell;
    use std::os::unix::fs::symlink;

    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
    let root = cwd.join(format!(
        "target/uniffi-ohos-production-containment-{}",
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let package = root.join("package");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(package.join("old.har"), b"old-staged-har").unwrap();
    std::fs::write(package.join("marker"), b"old-staging-marker").unwrap();
    symlink(&package, root.join("linked-package")).unwrap();
    let relative = package
        .join("relative.har")
        .strip_prefix(&cwd)
        .unwrap()
        .to_path_buf();
    let requests = [
        package.join("absolute.har"),
        relative,
        root.join("linked-package/symlink.har"),
    ];

    for requested in requests {
        let stage_called = Cell::new(false);
        let error = package_har_with(
            &package,
            &requested,
            None,
            || {
                stage_called.set(true);
                Ok(())
            },
            |_| panic!("unsafe output must fail before the build closure"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("must not be inside"));
        assert!(!stage_called.get());
        assert_eq!(
            std::fs::read(package.join("old.har")).unwrap(),
            b"old-staged-har"
        );
        assert_eq!(
            std::fs::read(package.join("marker")).unwrap(),
            b"old-staging-marker"
        );
    }
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn production_package_preflight_rejects_output_inside_public_dist_without_side_effects() {
    use std::cell::Cell;

    let root = temp_test_dir("uniffi-ohos-dist-har-output");
    let package = root.join("package");
    let dist = root.join("dist");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(package.join("marker"), b"old-staging").unwrap();
    let stage_called = Cell::new(false);
    let nested_output = dist.join("new/nested.har");
    let error = package_har_with(
        &package,
        &nested_output,
        Some(&dist),
        || {
            stage_called.set(true);
            Ok(())
        },
        |_| panic!("dist-contained output must fail before build"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("build-owned OHOS dist"));
    assert!(!stage_called.get());
    assert!(!dist.join("new").exists());
    assert_eq!(
        std::fs::read(package.join("marker")).unwrap(),
        b"old-staging"
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn production_package_and_publish_failures_preserve_existing_har() {
    let root = temp_test_dir("uniffi-ohos-production-failures");
    let package = root.join("package");
    let final_har = root.join("final.har");

    for phase in ["tool check", "Hvigor assembleHar"] {
        std::fs::write(&final_har, b"known-good-old-har").unwrap();
        package_har_with(
            &package,
            &final_har,
            None,
            || {
                if package.exists() {
                    std::fs::remove_dir_all(&package)?;
                }
                std::fs::create_dir_all(&package)?;
                std::fs::write(package.join("new-staging"), b"new")?;
                Ok(())
            },
            |_| bail!("injected {phase} failure"),
        )
        .unwrap_err();
        assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");
    }

    std::fs::create_dir_all(package.join("src/main")).unwrap();
    std::fs::write(package.join("oh-package.json5"), "{\"name\":\"demo\"}\n").unwrap();
    std::fs::write(package.join("src/main/module.json"), "{\"module\":{}}\n").unwrap();
    let candidate = root.join("candidate.har");
    generate_har_archive(&candidate, &package).unwrap();
    let entries = read_har_entries(&candidate).unwrap();

    std::fs::write(&final_har, b"known-good-old-har").unwrap();
    publish_archive_entries_with_hooks(
        entries.clone(),
        &final_har,
        Some(&package),
        |_| bail!("injected prepublish failure"),
        |_| Ok(()),
    )
    .unwrap_err();
    assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");

    publish_archive_entries_with_hooks(
        entries,
        &final_har,
        Some(&package),
        |_| Ok(()),
        |_| bail!("injected pre-persist failure"),
    )
    .unwrap_err();
    assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");
    let leftovers = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("uniffi-har-")
        })
        .count();
    assert_eq!(leftovers, 0);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hvigor_production_chain_failures_preserve_existing_har() {
    let root = temp_test_dir("uniffi-ohos-hvigor-failures");
    let sdk_root = root.join("sdk");
    std::fs::create_dir_all(sdk_root.join("default")).unwrap();
    std::fs::create_dir_all(sdk_root.join("default/openharmony")).unwrap();
    std::fs::write(
        sdk_root.join("default/sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"22"}}"#,
    )
    .unwrap();

    let dist_root = root.join("fixture");
    let dist = write_fake_dist(&dist_root, "demo_ohos");
    let package = root.join("package");
    let metadata = test_package_metadata();
    stage_har_package(&dist, &package, "demo_ohos", &metadata, true).unwrap();
    let final_har = root.join("final.har");
    let mut options = test_build_options();
    options.deveco_sdk_home = Some(sdk_root);
    options.hvigorw = Some("fake-hvigorw".into());
    options.ohpm = Some("fake-ohpm".into());

    for phase in ["tool", "hvigor", "prepublish"] {
        std::fs::write(&final_har, b"known-good-old-har").unwrap();
        let mut invocation_root = None;
        let error = build_hvigor_har_with(
            &options,
            &metadata,
            &package,
            &final_har,
            |_, tool, args, cwd| {
                invocation_root.get_or_insert_with(|| {
                    cwd.parent()
                        .expect("HAR project mirror has an invocation root")
                        .to_path_buf()
                });
                if phase == "tool" && args == ["--version"] {
                    bail!("injected tool check failure");
                }
                if tool == "fake-hvigorw" && args.first() == Some(&"assembleHar") {
                    if phase == "hvigor" {
                        bail!("injected Hvigor failure");
                    }
                    write_fake_compiled_har(cwd, &metadata)?;
                }
                if tool == "fake-ohpm" && args.first() == Some(&"prepublish") {
                    if phase == "prepublish" {
                        bail!("injected prepublish failure");
                    }
                }
                Ok(())
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().to_ascii_lowercase().contains(phase),
            "unexpected {phase} error: {error:#}"
        );
        let invocation_root = invocation_root.expect("HAR tool runner captured its root");
        assert!(
            !invocation_root.exists(),
            "temporary HAR project was not cleaned after {phase} failure"
        );
        assert_eq!(std::fs::read(&final_har).unwrap(), b"known-good-old-har");
        let leftovers = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("uniffi-har-")
            })
            .count();
        assert_eq!(leftovers, 0);
    }
    test_cleanup_temp_root(&root);
}

#[test]
fn hvigor_har_temporary_project_builds_and_publishes() {
    let root = temp_test_dir("uniffi-ohos-hvigor-guard");
    let sdk_root = root.join("sdk");
    std::fs::create_dir_all(sdk_root.join("default/openharmony")).unwrap();
    std::fs::write(
        sdk_root.join("default/sdk-pkg.json"),
        r#"{"data":{"platformVersion":"6.0.2","apiVersion":"22"}}"#,
    )
    .unwrap();

    let dist = write_fake_dist(&root.join("fixture"), "demo_ohos");
    let package = root.join("package");
    let metadata = test_package_metadata();
    stage_har_package(&dist, &package, "demo_ohos", &metadata, true).unwrap();
    let mut options = test_build_options();
    options.deveco_sdk_home = Some(sdk_root);
    options.hvigorw = Some("fake-hvigorw".into());
    options.ohpm = Some("fake-ohpm".into());

    let run_fake_tools =
        |_: &HarmonyHarTools, tool: &str, args: &[&str], cwd: &Utf8Path| -> Result<()> {
            if tool == "fake-hvigorw" && args.first() == Some(&"assembleHar") {
                write_fake_compiled_har(cwd, &metadata)?;
            }
            Ok(())
        };

    let success_har = root.join("success.har");
    build_hvigor_har_with(&options, &metadata, &package, &success_har, run_fake_tools).unwrap();
    assert!(success_har.is_file());
    test_cleanup_temp_root(&root);
}

#[test]
fn har_output_paths_support_plain_relative_nested_and_absolute_forms() {
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir().unwrap()).unwrap();
    let plain = prepare_har_output_path(Utf8Path::new("relative-review.har"), None).unwrap();
    assert_eq!(plain, cwd.join("relative-review.har"));

    let root = temp_test_dir("uniffi-ohos-har-output-paths");
    let nested = root.join("nested/review.har");
    let resolved = prepare_har_output_path(&nested, None).unwrap();
    assert_eq!(
        resolved,
        root.join("nested")
            .canonicalize_utf8()
            .unwrap()
            .join("review.har")
    );
    let absolute = root.join("absolute.har");
    assert_eq!(
        prepare_har_output_path(&absolute, None).unwrap(),
        root.canonicalize_utf8().unwrap().join("absolute.har")
    );
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn compiled_har_discovery_requires_exactly_one_regular_candidate() {
    let root = temp_test_dir("uniffi-ohos-compiled-har-discovery");
    let module = root.join("library");
    let outputs = module.join("build/default/outputs/default");
    std::fs::create_dir_all(&outputs).unwrap();

    let error = discover_compiled_har(&module).unwrap_err().to_string();
    assert!(
        error.contains("produced no .har"),
        "unexpected error: {error}"
    );

    let expected = outputs.join("release/library.har");
    std::fs::create_dir_all(expected.parent().unwrap()).unwrap();
    std::fs::write(&expected, "compiled").unwrap();
    assert_eq!(discover_compiled_har(&module).unwrap(), expected);

    let second = outputs.join("debug/library.har");
    std::fs::create_dir_all(second.parent().unwrap()).unwrap();
    std::fs::write(&second, "compiled-debug").unwrap();
    let error = discover_compiled_har(&module).unwrap_err().to_string();
    assert!(
        error.contains("multiple .har candidates"),
        "unexpected error: {error}"
    );

    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn compiled_har_discovery_rejects_symlinked_har_candidate() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-compiled-har-symlink");
    let module = root.join("library");
    let outputs = module.join("build/default/outputs/default");
    std::fs::create_dir_all(&outputs).unwrap();
    std::fs::write(root.join("outside.har"), "outside").unwrap();
    symlink(root.join("outside.har"), outputs.join("library.har")).unwrap();

    let error = discover_compiled_har(&module).unwrap_err().to_string();
    assert!(
        error.contains("symlinked Hvigor .har"),
        "unexpected error: {error}"
    );

    std::fs::remove_dir_all(root).ok();
}

#[cfg(unix)]
#[test]
fn har_output_rejects_symlink_and_resolves_symlinked_parent_safely() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-ohos-har-output-symlink");
    let real = root.join("real");
    std::fs::create_dir_all(&real).unwrap();
    symlink(&real, root.join("linked")).unwrap();
    let through_parent = prepare_har_output_path(&root.join("linked/output.har"), None).unwrap();
    assert_eq!(
        through_parent,
        real.canonicalize_utf8().unwrap().join("output.har")
    );

    std::fs::write(real.join("target.har"), "old").unwrap();
    symlink(real.join("target.har"), root.join("symlink.har")).unwrap();
    assert!(prepare_har_output_path(&root.join("symlink.har"), None).is_err());
    std::fs::remove_dir_all(root).ok();
}

fn test_targz(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut output = Vec::new();
    {
        let encoder = GzBuilder::new()
            .mtime(0)
            .write(&mut output, Compression::default());
        let mut archive = Builder::new(encoder);
        for (name, data) in entries {
            let mut header = Header::new_gnu();
            header.set_entry_type(EntryType::Regular);
            header.set_mode(0o644);
            header.set_size(data.len() as u64);
            header.set_cksum();
            archive
                .append_data(&mut header, name, Cursor::new(*data))
                .unwrap();
        }
        let encoder = archive.into_inner().unwrap();
        encoder.finish().unwrap();
    }
    output
}

fn test_runtime_hsp(
    metadata: &OhosPackageMetadata,
    bundle_name: &str,
    so_files: &[(&str, &str)],
) -> Vec<u8> {
    test_runtime_hsp_with_override(metadata, bundle_name, so_files, None)
}

fn test_runtime_hsp_with_override(
    metadata: &OhosPackageMetadata,
    bundle_name: &str,
    so_files: &[(&str, &str)],
    override_so: Option<(&str, Vec<u8>)>,
) -> Vec<u8> {
    test_runtime_hsp_with_shape(metadata, bundle_name, so_files, override_so, true)
}

fn test_runtime_hsp_with_shape(
    metadata: &OhosPackageMetadata,
    bundle_name: &str,
    so_files: &[(&str, &str)],
    override_so: Option<(&str, Vec<u8>)>,
    include_pkg_context: bool,
) -> Vec<u8> {
    let cursor = Cursor::new(Vec::new());
    let mut archive = zip::ZipWriter::new(cursor);
    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .unix_permissions(0o644);
    let module = serde_json::to_vec(&serde_json::json!({
        "app": { "bundleName": bundle_name },
        "module": {
            "name": metadata.module_name,
            "packageName": metadata.name,
            "type": "shared",
            "deliveryWithInstall": true,
            "compileMode": "esmodule"
        }
    }))
    .unwrap();
    let mut required = vec![
        ("module.json", module.as_slice()),
        ("pack.info", b"{}".as_slice()),
        ("ets/modules.abc", b"fixture-bytecode".as_slice()),
    ];
    if include_pkg_context {
        required.push(("pkgContextInfo.json", b"{}".as_slice()));
    }
    for (name, data) in required {
        archive.start_file(name, options).unwrap();
        archive.write_all(data).unwrap();
    }
    for (abi, name) in so_files {
        archive
            .start_file(format!("libs/{abi}/{name}"), options)
            .unwrap();
        let bytes = override_so
            .as_ref()
            .filter(|(override_name, _)| override_name == name)
            .map(|(_, bytes)| bytes.clone())
            .unwrap_or_else(|| test_elf_bytes(abi, name.as_bytes()));
        archive.write_all(&bytes).unwrap();
    }
    archive.finish().unwrap().into_inner()
}

fn test_elf_bytes(abi: &str, tag: &[u8]) -> Vec<u8> {
    let (machine, is_64): (u16, bool) = match abi {
        "arm64-v8a" => (183, true),
        "armeabi-v7a" => (40, false),
        "x86_64" => (62, true),
        "loongarch64" => (258, true),
        other => panic!("unsupported test ELF ABI {other}"),
    };
    test_elf_bytes_with_class(machine, is_64, tag)
}

fn test_elf_bytes_with_class(machine: u16, is_64: bool, tag: &[u8]) -> Vec<u8> {
    let header_size = if is_64 { 64 } else { 52 };
    let mut bytes = vec![0_u8; header_size];
    bytes[..4].copy_from_slice(b"\x7fELF");
    bytes[4] = if is_64 { 2 } else { 1 };
    bytes[5] = 1; // little endian
    bytes[6] = 1; // ELF version
    bytes[16..18].copy_from_slice(&3_u16.to_le_bytes()); // ET_DYN
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes[20..24].copy_from_slice(&1_u32.to_le_bytes());
    if is_64 {
        bytes[52..54].copy_from_slice(&64_u16.to_le_bytes());
    } else {
        bytes[40..42].copy_from_slice(&52_u16.to_le_bytes());
    }
    bytes.extend_from_slice(tag);
    bytes
}

#[test]
fn rejects_machine_correct_but_class_wrong_hsp_elfs() {
    assert!(runtime_so_identity(
        &test_elf_bytes_with_class(40, true, b"wrong-arm-class"),
        "armeabi-v7a",
        "libdemo.so",
    )
    .is_err());
    #[cfg(unix)]
    {
        let root = temp_test_dir("uniffi-wrong-raw-elf-class");
        let abi = root.join("staged/armeabi-v7a");
        std::fs::create_dir_all(&abi).unwrap();
        let bytes = test_elf_bytes_with_class(40, true, b"wrong-raw-class");
        std::fs::write(abi.join("libdemo.so"), &bytes).unwrap();
        let expected = BTreeMap::from([(
            "armeabi-v7a".to_string(),
            BTreeMap::from([("libdemo.so".to_string(), sha256_bytes(&bytes))]),
        )]);
        let mut temporary_roots = Vec::new();
        assert!(normalize_staged_hsp_so_inventory_with_hook(
            root.join("staged").as_path(),
            &expected,
            Utf8Path::new("/usr/bin/true"),
            |temp_root| {
                temporary_roots.push(temp_root.to_path_buf());
                Ok(())
            },
        )
        .is_err());
        assert!(
            temporary_roots.is_empty(),
            "raw ELF validation created a temporary strip root"
        );
        let _ = std::fs::remove_dir_all(root.as_std_path());
    }
    assert!(runtime_so_identity(
        &test_elf_bytes_with_class(183, false, b"wrong-arm64-class"),
        "arm64-v8a",
        "libdemo.so",
    )
    .is_err());
}

#[cfg(unix)]
#[test]
fn normalized_so_executes_canonical_strip_after_alias_swap() {
    use std::os::unix::fs::{symlink, PermissionsExt};

    let root = temp_test_dir("uniffi-canonical-strip");
    let staged = root.join("staged/arm64-v8a");
    let bin = root.join("sdk/bin");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let library = staged.join("libdemo.so");
    let bytes = test_elf_bytes("arm64-v8a", b"canonical-strip");
    std::fs::write(&library, &bytes).unwrap();
    let real = bin.join("llvm-strip-real");
    let evil = bin.join("llvm-strip-evil");
    let marker = root.join("evil-ran");
    std::fs::write(&real, "#!/bin/sh\ncp \"$1\" \"${2#-o}\"\n").unwrap();
    std::fs::write(&evil, format!("#!/bin/sh\ntouch '{}'\nexit 99\n", marker)).unwrap();
    for path in [&real, &evil] {
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }
    let alias = bin.join("llvm-strip");
    symlink(&real, &alias).unwrap();
    let expected = BTreeMap::from([(
        "arm64-v8a".to_string(),
        BTreeMap::from([("libdemo.so".to_string(), sha256_bytes(&bytes))]),
    )]);
    let mut swapped = false;
    let mut temporary_roots = Vec::new();
    let normalized = normalize_staged_hsp_so_inventory_with_hook(
        root.join("staged").as_path(),
        &expected,
        &alias,
        |temp_root| {
            temporary_roots.push(temp_root.to_path_buf());
            if !swapped {
                std::fs::remove_file(&alias)?;
                symlink(&evil, &alias)?;
                swapped = true;
            }
            Ok(())
        },
    )
    .unwrap();
    assert!(
        !marker.exists(),
        "swapped unverified strip alias was executed"
    );
    assert_eq!(
        normalized["arm64-v8a"]["libdemo.so"].sha256,
        sha256_bytes(&bytes)
    );
    assert_eq!(temporary_roots.len(), 1);
    assert!(!temporary_roots[0].exists());
    let _ = std::fs::remove_dir_all(root.as_std_path());
}

#[cfg(unix)]
#[test]
fn normalized_so_error_cleans_its_temporary_root() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_test_dir("uniffi-invalid-normalized-strip");
    let staged = root.join("staged/arm64-v8a");
    let bin = root.join("sdk/bin");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let bytes = test_elf_bytes("arm64-v8a", b"valid-raw");
    std::fs::write(staged.join("libdemo.so"), &bytes).unwrap();
    let strip = bin.join("llvm-strip");
    std::fs::write(&strip, "#!/bin/sh\nprintf 'not-an-elf' > \"${2#-o}\"\n").unwrap();
    let mut permissions = std::fs::metadata(&strip).unwrap().permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&strip, permissions).unwrap();
    let expected = BTreeMap::from([(
        "arm64-v8a".to_string(),
        BTreeMap::from([("libdemo.so".to_string(), sha256_bytes(&bytes))]),
    )]);
    let mut temporary_roots = Vec::new();
    let error = normalize_staged_hsp_so_inventory_with_hook(
        root.join("staged").as_path(),
        &expected,
        &strip,
        |temp_root| {
            temporary_roots.push(temp_root.to_path_buf());
            Ok(())
        },
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("ELF"));
    assert_eq!(temporary_roots.len(), 1);
    assert!(
        !temporary_roots[0].exists(),
        "invalid normalized ELF left its temporary root"
    );
    test_cleanup_temp_root(&root);
}

fn test_interface_har(metadata: &OhosPackageMetadata, include_so: bool) -> Vec<u8> {
    let public_declarations = b"export interface UniFfiStreamResult<T> {}\nexport interface UniFfiStream<T> { next(): Promise<UniFfiStreamResult<T>>; cancel(): Promise<void>; }\n";
    let package = serde_json::to_vec(&serde_json::json!({
        "name": metadata.name,
        "version": metadata.version,
        "packageType": "InterfaceHar",
        "types": "Index.d.ets"
    }))
    .unwrap();
    let module = serde_json::to_vec(&serde_json::json!({
        "module": {
            "name": metadata.module_name,
            "packageName": metadata.name,
            "type": "shared",
            "deliveryWithInstall": true
        }
    }))
    .unwrap();
    let mut entries = vec![
        ("package/oh-package.json5", package.as_slice()),
        ("package/src/main/module.json", module.as_slice()),
        ("package/Index.d.ets", public_declarations),
        (
            "package/src/main/cpp/types/libdemo/index.d.ts",
            b"export declare function demo(): number;".as_slice(),
        ),
    ];
    if include_so {
        entries.push(("package/libs/arm64-v8a/libdemo.so", b"so".as_slice()));
    }
    test_targz(&entries)
}

#[test]
fn validates_hsp_parameter_matrix_and_api_floor() {
    let mut options = test_build_options();
    options.integrated_hsp = true;
    assert!(validate_package_mode_options(&options).is_err());

    options = test_build_options();
    options.package_kind = PackageKind::Hsp;
    assert!(validate_package_mode_options(&options).is_err());
    options.hsp_bundle_name = Some("com.example.app".into());
    validate_package_mode_options(&options).unwrap();
    options.no_har = true;
    assert!(validate_package_mode_options(&options).is_err());
    options.no_har = false;
    options.skip_libs = true;
    assert!(validate_package_mode_options(&options).is_err());

    options = test_build_options();
    options.package_kind = PackageKind::Hsp;
    options.integrated_hsp = true;
    validate_package_mode_options(&options).unwrap();
    options.hsp_bundle_name = Some("com.example.app".into());
    assert!(validate_package_mode_options(&options).is_err());

    for (version, sdk_type, expected) in [
        ("5.0.1(13)", RuntimeSdkType::HarmonyOs, 13),
        ("12", RuntimeSdkType::OpenHarmony, 12),
        ("26.0.0", RuntimeSdkType::HarmonyOs, 26),
    ] {
        assert_eq!(
            compatible_sdk_api_level(&SdkCompatibility {
                version: version.into(),
                sdk_type,
            })
            .unwrap(),
            expected
        );
    }
    assert!(compatible_sdk_api_level(&SdkCompatibility {
        version: "5.0.0".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    })
    .is_err());
}

#[test]
fn renders_integrated_hsp_module_and_package_templates() {
    let mut metadata = test_package_metadata();
    metadata.sdk = Some(SdkCompatibility {
        version: "5.0.1(13)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    });
    let module: Value =
        serde_json::from_str(&render_module_json5(&metadata, PackageKind::Hsp).unwrap()).unwrap();
    assert_eq!(module["module"]["type"], "shared");
    assert_eq!(module["module"]["deliveryWithInstall"], true);
    assert_eq!(
        module["module"]["deviceTypes"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );
    let profile: Value = serde_json::from_str(
        &render_build_profile_json5(&metadata, PackageKind::Hsp, true).unwrap(),
    )
    .unwrap();
    assert_eq!(profile["apiType"], "stageMode");
    assert_eq!(profile["targets"][0]["runtimeOS"], "HarmonyOS");
    assert_eq!(
        profile["targets"][0]["config"]["deviceType"],
        serde_json::json!(["phone", "tablet", "2in1"])
    );
    assert_eq!(profile["buildOption"]["generateSharedTgz"], true);
    assert_eq!(
        profile["buildOption"]["nativeLib"]["excludeSoFromInterfaceHar"],
        true
    );
    assert_eq!(profile["buildOption"]["arkOptions"]["integratedHsp"], true);
    assert!(profile["buildOption"]["nativeLib"]
        .get("headerPath")
        .is_none());
    let package: Value = serde_json::from_str(
        &render_oh_package_json5(&metadata, "demo", &[], PackageKind::Hsp).unwrap(),
    )
    .unwrap();
    assert_eq!(package["packageType"], "InterfaceHar");
}

#[test]
fn hsp_projects_enable_normalized_ohm_urls_in_both_packaging_modes() {
    let root = temp_test_dir("uniffi-hsp-normalized-ohm-url");
    let metadata = test_package_metadata();
    let sdk = SdkCompatibility {
        version: "5.0.1(13)".into(),
        sdk_type: RuntimeSdkType::HarmonyOs,
    };
    let tools = HarmonyHarTools {
        hvigorw: "hvigorw".into(),
        ohpm: "ohpm".into(),
        sdk_home: root.join("sdk"),
        node_home: None,
        ohos_base_sdk_home: None,
        model_version: "5.0.0".into(),
        compile_sdk: CompileSdk {
            api_level: 13,
            platform_version: "5.0.1".into(),
        },
    };

    for (integrated, bundle_name) in [(true, None), (false, Some("com.example.host"))] {
        let project = root.join(if integrated {
            "integrated"
        } else {
            "host-bound"
        });
        let module = project.join("library");
        std::fs::create_dir_all(&module).unwrap();
        write_hvigor_hsp_project(
            &project,
            &module,
            &metadata,
            &sdk,
            &tools,
            None,
            integrated,
            bundle_name,
        )
        .unwrap();
        let profile: Value = serde_json::from_str(
            &std::fs::read_to_string(project.join("build-profile.json5")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            profile["app"]["products"][0]["buildOption"]["strictMode"]["useNormalizedOHMUrl"],
            true
        );
    }

    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_tgz_runtime_and_interface_parsers_enforce_exact_members_and_so_ownership() {
    let metadata = test_package_metadata();
    let package = test_host_package("demo-host", "1.2.3", "demo");
    let root = temp_test_dir("uniffi-hsp-parser");
    let libs = root.join("libs/arm64-v8a");
    std::fs::create_dir_all(&libs).unwrap();
    let names = ["libdemo_core.so", "libdemo.so", "libc++_shared.so"];
    for name in names {
        std::fs::write(
            libs.join(name),
            test_elf_bytes("arm64-v8a", name.as_bytes()),
        )
        .unwrap();
    }
    std::fs::write(
        root.join("oh-package.json5"),
        render_oh_package_json5(
            &metadata,
            "demo",
            &names.map(str::to_string),
            PackageKind::Hsp,
        )
        .unwrap(),
    )
    .unwrap();
    let expected = BTreeMap::from([(
        "arm64-v8a".to_string(),
        names
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    sha256_bytes(&test_elf_bytes("arm64-v8a", name.as_bytes())),
                )
            })
            .collect(),
    )]);
    let expected_runtime = BTreeMap::from([(
        "arm64-v8a".to_string(),
        names
            .into_iter()
            .map(|name| {
                (
                    name.to_string(),
                    runtime_so_identity(
                        &test_elf_bytes("arm64-v8a", name.as_bytes()),
                        "arm64-v8a",
                        name,
                    )
                    .unwrap(),
                )
            })
            .collect(),
    )]);
    let runtime = test_runtime_hsp(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
    );
    let interface = test_interface_har(&metadata, false);
    let tgz = test_targz(&[("demo-default.hsp", &runtime), ("demo.har", &interface)]);
    let members = parse_hsp_tgz(&tgz).unwrap();
    assert_eq!(members.runtime_hsp, runtime);
    assert_eq!(members.interface_har, interface);
    validate_runtime_hsp(
        &runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
    )
    .unwrap();
    let host_bundle = "com.example.host";
    let host_runtime = test_runtime_hsp_with_shape(
        &metadata,
        host_bundle,
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        None,
        false,
    );
    validate_runtime_hsp(
        &host_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        false,
        Some(host_bundle),
    )
    .unwrap();
    let wrong_runtime_bytes = test_runtime_hsp_with_override(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        Some((
            "libdemo_core.so",
            test_elf_bytes("arm64-v8a", b"same-name-different-runtime-bytes"),
        )),
    );
    assert!(validate_runtime_hsp(
        &wrong_runtime_bytes,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
    )
    .is_err());
    let wrong_arch_runtime = test_runtime_hsp_with_override(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        Some((
            "libdemo_core.so",
            test_elf_bytes("x86_64", b"wrong-architecture"),
        )),
    );
    assert!(validate_runtime_hsp(
        &wrong_arch_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
    )
    .is_err());
    let wrong_class_runtime = test_runtime_hsp_with_override(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
        ],
        Some((
            "libdemo_core.so",
            test_elf_bytes_with_class(183, false, b"wrong-runtime-class"),
        )),
    );
    assert!(validate_runtime_hsp(
        &wrong_class_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
    )
    .is_err());
    for (label, mutate) in [
        (
            "soname",
            (|identity: &mut RuntimeSoIdentity| {
                identity.soname = Some("libwrong-soname.so".into());
            }) as fn(&mut RuntimeSoIdentity),
        ),
        (
            "build-id",
            (|identity: &mut RuntimeSoIdentity| {
                identity.build_id = Some("deadbeef".into());
            }) as fn(&mut RuntimeSoIdentity),
        ),
    ] {
        let mut wrong_identity = expected_runtime.clone();
        mutate(
            wrong_identity
                .get_mut("arm64-v8a")
                .unwrap()
                .get_mut("libdemo_core.so")
                .unwrap(),
        );
        assert!(
            validate_runtime_hsp(
                &runtime,
                &package,
                &metadata,
                root.join("libs").as_path(),
                &expected,
                &wrong_identity,
                true,
                None,
            )
            .is_err(),
            "runtime {label} provenance mismatch must fail"
        );
    }
    validate_interface_har(&interface, &metadata).unwrap();

    let unknown = test_targz(&[("demo.hsp", &runtime), ("README", b"bad")]);
    assert!(parse_hsp_tgz(&unknown).is_err());
    let nested = test_targz(&[("nested/demo.hsp", &runtime), ("demo.har", &interface)]);
    assert!(parse_hsp_tgz(&nested).is_err());
    let duplicate = test_targz(&[("demo.hsp", &runtime), ("demo.hsp", &runtime)]);
    assert!(parse_hsp_tgz(&duplicate).is_err());

    std::fs::write(
        libs.join("libunexpected.so"),
        test_elf_bytes("arm64-v8a", b"libunexpected.so"),
    )
    .unwrap();
    let extra_runtime = test_runtime_hsp(
        &metadata,
        "",
        &[
            ("arm64-v8a", "libdemo_core.so"),
            ("arm64-v8a", "libdemo.so"),
            ("arm64-v8a", "libc++_shared.so"),
            ("arm64-v8a", "libunexpected.so"),
        ],
    );
    assert!(validate_runtime_hsp(
        &extra_runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &expected,
        &expected_runtime,
        true,
        None,
    )
    .is_err());
    std::fs::remove_file(libs.join("libunexpected.so")).unwrap();

    for missing in names {
        let mut missing_expected = expected.clone();
        missing_expected
            .get_mut("arm64-v8a")
            .unwrap()
            .remove(missing);
        assert!(validate_runtime_hsp(
            &runtime,
            &package,
            &metadata,
            root.join("libs").as_path(),
            &missing_expected,
            &expected_runtime,
            true,
            None,
        )
        .is_err());
    }
    let mut extra_abi = expected.clone();
    extra_abi.insert("x86_64".into(), expected["arm64-v8a"].clone());
    assert!(validate_runtime_hsp(
        &runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &extra_abi,
        &expected_runtime,
        true,
        None,
    )
    .is_err());
    let mut wrong_hash = expected.clone();
    wrong_hash.get_mut("arm64-v8a").unwrap().insert(
        "libdemo_core.so".into(),
        sha256_bytes(b"different core bytes"),
    );
    assert!(validate_runtime_hsp(
        &runtime,
        &package,
        &metadata,
        root.join("libs").as_path(),
        &wrong_hash,
        &expected_runtime,
        true,
        None,
    )
    .is_err());
    assert!(validate_interface_har(&test_interface_har(&metadata, true), &metadata,).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn hsp_complete_output_plan_rejects_all_alias_directions_without_residue() {
    let root = temp_test_dir("uniffi-hsp-output-aliases");
    let base = HspOutputPaths {
        dist: Some(root.join("dist")),
        tgz: root.join("release.tgz"),
        runtime_hsp: root.join("runtime.hsp"),
        interface_har: root.join("interface.har"),
        package_source: root.join("package"),
        module_project: root.join("project"),
        usage: root.join("usage.md"),
    };
    let before = std::fs::read_dir(&root).unwrap().count();

    let mut descendant = base.clone();
    descendant.runtime_hsp = descendant.tgz.join("runtime.hsp");
    assert!(normalize_hsp_destinations(&mut [descendant], &["one".into()]).is_err());

    let mut ancestor = base.clone();
    ancestor.tgz = ancestor.runtime_hsp.join("release.tgz");
    assert!(normalize_hsp_destinations(&mut [ancestor], &["one".into()]).is_err());

    let mut same = base.clone();
    same.interface_har = same.tgz.clone();
    assert!(normalize_hsp_destinations(&mut [same], &["one".into()]).is_err());

    let mut lexical = base.clone();
    lexical.usage = root.join("nested/../usage.md");
    assert!(normalize_hsp_destinations(&mut [lexical], &["one".into()]).is_err());

    let mut second = HspOutputPaths {
        dist: Some(root.join("other-dist")),
        tgz: root.join("other-release.tgz"),
        runtime_hsp: root.join("other-runtime.hsp"),
        interface_har: root.join("other-interface.har"),
        package_source: root.join("other-package"),
        module_project: root.join("other-project"),
        usage: root.join("other-usage.md"),
    };
    second.runtime_hsp = base.runtime_hsp.clone();
    assert!(
        normalize_hsp_destinations(&mut [base.clone(), second], &["one".into(), "two".into()],)
            .is_err()
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let real = root.join("real");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, root.join("alias")).unwrap();
        let mut aliased = base.clone();
        aliased.tgz = real.join("same.tgz");
        aliased.runtime_hsp = root.join("alias/same.tgz");
        assert!(normalize_hsp_destinations(&mut [aliased], &["one".into()]).is_err());
    }

    let after = std::fs::read_dir(&root).unwrap().count();
    #[cfg(unix)]
    assert_eq!(
        after,
        before + 2,
        "only the test's real/alias fixtures may appear"
    );
    #[cfg(not(unix))]
    assert_eq!(after, before);
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn simple_output_staging_failure_leaves_missing_destination_parents_absent() {
    let root = temp_test_dir("uniffi-simple-output-stage-failure");
    let source = root.join("source.txt");
    std::fs::write(&source, b"complete").unwrap();
    let invalid_source = root.join("missing-source.txt");
    let first_destination = root.join("public/first/value.txt");
    let second_destination = root.join("public/second/value.txt");

    let error = publish_simple_output_set([
        (source.as_path(), first_destination.as_path(), false),
        (
            invalid_source.as_path(),
            second_destination.as_path(),
            false,
        ),
    ])
    .unwrap_err()
    .to_string();
    assert!(error.contains("reading staged output file"), "{error}");
    assert!(!root.join("public").exists());
    assert_eq!(std::fs::read(&source).unwrap(), b"complete");

    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn recursive_copy_keeps_internal_relative_links_and_rejects_unsafe_links() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("uniffi-recursive-copy-links");
    let source = root.join("source");
    let nested = source.join("nested");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(source.join("inside.txt"), b"inside").unwrap();
    let link = nested.join("link.txt");
    symlink("../inside.txt", &link).unwrap();

    let copied = root.join("copied");
    copy_dir_recursive(&source, &copied).unwrap();
    assert_eq!(
        std::fs::read(copied.join("nested/link.txt")).unwrap(),
        b"inside"
    );

    std::fs::remove_file(&link).unwrap();
    let outside = root.join("outside.txt");
    std::fs::write(&outside, b"outside").unwrap();
    symlink("../../outside.txt", &link).unwrap();
    assert!(copy_dir_recursive(&source, &root.join("escape-copy")).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");

    std::fs::remove_file(&link).unwrap();
    symlink(source.join("inside.txt"), &link).unwrap();
    assert!(copy_dir_recursive(&source, &root.join("absolute-copy")).is_err());

    std::fs::remove_file(&link).unwrap();
    symlink("missing.txt", &link).unwrap();
    assert!(copy_dir_recursive(&source, &root.join("dangling-copy")).is_err());
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");

    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn hsp_multi_package_single_path_overrides_fail_in_read_only_plan() {
    let plan = HostPlan {
        target_directory: "target".into(),
        workspace_root: "workspace".into(),
        local_source_roots: Vec::new(),
        packages: vec![
            test_host_package("first", "1.0.0", "first"),
            test_host_package("second", "1.0.0", "second"),
        ],
        package_count: 2,
        explicit_package_arg: false,
    };
    for flag in ["runtime", "interface", "tgz"] {
        let mut options = test_build_options();
        options.package_kind = PackageKind::Hsp;
        match flag {
            "runtime" => options.runtime_hsp_out = Some("single/output.hsp".into()),
            "interface" => options.interface_har_out = Some("single/interface.har".into()),
            "tgz" => options.tgz_out = Some("single/release.tgz".into()),
            _ => unreachable!(),
        }
        let error = validate_multi_package_output_overrides(&options, &plan)
            .unwrap_err()
            .to_string();
        assert!(error.contains("ambiguous"), "{flag}: {error}");
    }
}

#[test]
fn hsp_archive_parsers_enforce_entry_and_path_limits() {
    let names = (0..=MAX_HSP_ARCHIVE_ENTRIES)
        .map(|index| format!("entry-{index}"))
        .collect::<Vec<_>>();
    let entries = names
        .iter()
        .map(|name| (name.as_str(), b"".as_slice()))
        .collect::<Vec<_>>();
    let archive = test_targz(&entries);
    assert!(read_bounded_targz_entries(&archive, false, None, "limit-test").is_err());

    let long_name = format!("{}.hsp", "a".repeat(MAX_HSP_ARCHIVE_PATH_BYTES));
    let archive = test_targz(&[(long_name.as_str(), b"x")]);
    assert!(read_bounded_targz_entries(&archive, false, None, "path-test").is_err());
}

#[test]
fn shared_traversal_budget_counts_regular_file_reads() {
    let root = temp_test_dir("uniffi-shared-file-budget");
    let file = root.join("payload.bin");
    std::fs::write(&file, b"12345678").unwrap();

    let mut limited = TraversalBudget::bounded(8, 7);
    let error = read_verified_regular_file_bounded_with_budget(
        &file,
        MAX_HSP_ARCHIVE_MEMBER_BYTES,
        "budgeted test file",
        &mut limited,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("total-byte limit"), "{error}");

    let mut sufficient = TraversalBudget::bounded(8, 64);
    assert_eq!(
        read_verified_regular_file_bounded_with_budget(
            &file,
            MAX_HSP_ARCHIVE_MEMBER_BYTES,
            "budgeted test file",
            &mut sufficient,
        )
        .unwrap(),
        b"12345678"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn hsp_release_discovery_is_top_level_unique_and_rejects_hardlinks() {
    let root = temp_test_dir("uniffi-hsp-discovery");
    std::fs::write(root.join("one.tgz"), b"one").unwrap();
    assert_eq!(discover_release_tgz(&root).unwrap(), root.join("one.tgz"));
    std::fs::write(root.join("two.tgz"), b"two").unwrap();
    assert!(discover_release_tgz(&root).is_err());
    std::fs::remove_file(root.join("one.tgz")).unwrap();
    std::fs::remove_file(root.join("two.tgz")).unwrap();
    let outside = root.join("outside");
    std::fs::write(&outside, b"hardlink").unwrap();
    std::fs::hard_link(&outside, root.join("linked.tgz")).unwrap();
    assert!(discover_release_tgz(&root).is_err());
    std::fs::remove_dir_all(root).ok();
}

#[test]
fn multi_package_har_out_and_package_name_require_explicit_package_filter() {
    let mut options = test_build_options();
    options.dist_dir = Utf8PathBuf::from("test/out/ohos/dist");
    options.package_name = Some("@scope/pkg".to_string());
    options.arches = vec!["aarch".to_string()];
    options.skip_check = false;
    options.skip_napi_check = false;
    let package = test_host_package("demo-ohos", "0.1.0", "demo_ohos");
    let root = Utf8Path::new("/tmp/out/ohos");
    assert_eq!(
        package_stage_dir(root, &package, 2),
        Utf8PathBuf::from("/tmp/out/ohos/package/demo-ohos")
    );
    assert_eq!(
        resolve_har_out(None, root, &package, 2),
        Utf8PathBuf::from("/tmp/out/ohos/demo-ohos-demo_ohos.har")
    );
    assert_eq!(
        resolve_oh_package_name(options.package_name.as_deref(), &package).unwrap(),
        "@scope/pkg"
    );
}

fn temp_test_dir(prefix: &str) -> Utf8PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Utf8PathBuf::from_path_buf(dir).unwrap()
}
