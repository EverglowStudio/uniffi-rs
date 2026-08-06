/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::super::artifact_staging::engine::*;
use super::*;

fn empty_build_args() -> BuildArgs {
    BuildArgs {
        manifest_path: Utf8PathBuf::from("/repo/crates/core/Cargo.toml"),
        out_dir: Some(Utf8PathBuf::from("/repo/generated")),
        target: vec![ArtifactTargetArg::Wasm],
        library_path: None,
        source: None,
        host_crates_dir: None,
        package_root: None,
        logical_host_crates_dir: None,
        artifact_dir: None,
        managed_layout: false,
        package_dir: None,
        release: false,
        cargo_features: Vec::new(),
        cargo_bin: "cargo".to_string(),
        no_format: false,
        config: None,
        javascript_support_dir: None,
        crate_name: None,
        metadata_no_deps: false,
        wasm_bindgen_out_dir: None,
        wasm_bindgen_target: WasmBindgenTargetArg::Web,
        napi_target_dir: None,
        wasm_target_dir: None,
        wasm_core_target_dir: None,
        #[cfg(feature = "cli-ohos")]
        ohos_dist_dir: None,
        #[cfg(feature = "cli-ohos")]
        ohos_package_name: None,
        #[cfg(feature = "cli-ohos")]
        ohos_module_name: None,
        #[cfg(feature = "cli-ohos")]
        ohos_package_version: None,
        #[cfg(feature = "cli-ohos")]
        ohos_author: None,
        #[cfg(feature = "cli-ohos")]
        ohos_license: None,
        #[cfg(feature = "cli-ohos")]
        ohos_description: None,
        #[cfg(feature = "cli-ohos")]
        ohos_compatible_sdk_version: None,
        #[cfg(feature = "cli-ohos")]
        ohos_target_sdk_version: None,
        #[cfg(feature = "cli-ohos")]
        ohos_compatible_sdk_type: None,
        #[cfg(feature = "cli-ohos")]
        ohos_device_types: Vec::new(),
        #[cfg(feature = "cli-ohos")]
        ohos_package_kind: super::super::ohos::PackageKind::Har,
        #[cfg(feature = "cli-ohos")]
        ohos_integrated_hsp: false,
        #[cfg(feature = "cli-ohos")]
        ohos_hsp_bundle_name: None,
        #[cfg(feature = "cli-ohos")]
        ohos_har_out: None,
        #[cfg(feature = "cli-ohos")]
        ohos_runtime_hsp_out: None,
        #[cfg(feature = "cli-ohos")]
        ohos_interface_har_out: None,
        #[cfg(feature = "cli-ohos")]
        ohos_tgz_out: None,
        #[cfg(feature = "cli-ohos")]
        ohos_hvigorw: None,
        #[cfg(feature = "cli-ohos")]
        ohos_ohpm: None,
        #[cfg(feature = "cli-ohos")]
        ohos_deveco_sdk_home: None,
        #[cfg(feature = "cli-ohos")]
        ohos_no_har: false,
        #[cfg(feature = "cli-ohos")]
        ohos_arch: Vec::new(),
        #[cfg(feature = "cli-ohos")]
        ohos_target_dir: None,
        #[cfg(feature = "cli-ohos")]
        ohos_static: false,
        #[cfg(feature = "cli-ohos")]
        ohos_skip_libs: false,
        #[cfg(feature = "cli-ohos")]
        ohos_skip_check: false,
        #[cfg(feature = "cli-ohos")]
        ohos_zigbuild: false,
        #[cfg(feature = "cli-ohos")]
        ohos_bisheng: false,
        #[cfg(feature = "cli-ohos")]
        ohos_package: None,
        #[cfg(feature = "cli-ohos")]
        ohos_skip_napi_check: false,
        #[cfg(feature = "cli-ohos")]
        ohos_soname: None,
        #[cfg(feature = "cli-ohos")]
        ohos_cargo_args: Vec::new(),
        apple_target: Vec::new(),
        apple_xcframework_out: None,
        apple_swift_out: None,
        apple_framework_name: None,
        android_abi: Vec::new(),
        android_api: 23,
        android_ndk_home: None,
        android_jni_libs_out: None,
        android_kotlin_out: None,
        android_package_name: None,
        android_aar_out: None,
    }
}

fn write_test_manifest(package_dir: &Utf8Path) -> Utf8PathBuf {
    std::fs::create_dir_all(package_dir.join("src")).unwrap();
    std::fs::write(package_dir.join("src/lib.rs"), "pub fn marker() {}\n").unwrap();
    let manifest = package_dir.join("Cargo.toml");
    std::fs::write(
        &manifest,
        "[package]\nname = \"uni-core\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[lib]\nname = \"uni_core\"\n",
    )
    .unwrap();
    manifest
}

fn test_cargo_metadata(target_directory: Utf8PathBuf) -> CargoPackageMetadata {
    CargoPackageMetadata {
        target_directory,
        package_name: "uni-core".to_string(),
        lib_target_name: "uni_core".to_string(),
    }
}

fn assert_no_staging_residue(parent: &Utf8Path, public_name: &str) {
    let prefix = format!(".{public_name}.staging-");
    let residue = std::fs::read_dir(parent)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| name.starts_with(&prefix))
        .collect::<Vec<_>>();
    assert!(residue.is_empty(), "staging residue remains: {residue:?}");
}

fn publish_fixture(public: &Utf8Path, file: &str, contents: &[u8]) {
    let stage = ManagedPackageStage::begin(public).unwrap();
    assert_eq!(stage.root().parent(), public.parent());
    assert_eq!(
        std::fs::read(stage.root().join(MANAGED_PACKAGE_MARKER_NAME)).unwrap(),
        MANAGED_PACKAGE_MARKER_CONTENT
    );
    std::fs::write(stage.root().join(file), contents).unwrap();
    stage.publish().unwrap();
}

#[test]
fn managed_stage_publishes_fresh_root_with_fixed_marker() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let public = parent.join("package");

    publish_fixture(&public, "fresh.txt", b"fresh\n");

    assert_eq!(std::fs::read(public.join("fresh.txt")).unwrap(), b"fresh\n");
    assert_eq!(
        std::fs::read(public.join(MANAGED_PACKAGE_MARKER_NAME)).unwrap(),
        b"uniffi-managed-package\n"
    );
    assert_no_staging_residue(parent, "package");
}

#[test]
fn managed_stage_marked_repeat_replaces_the_entire_generation() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let public = parent.join("package");
    publish_fixture(&public, "old-only.txt", b"old\n");

    let stage = ManagedPackageStage::begin(&public).unwrap();
    assert!(
        !stage.root().join("old-only.txt").exists(),
        "a new generation must never seed files from the public root"
    );
    std::fs::create_dir_all(stage.root().join("src")).unwrap();
    std::fs::write(stage.root().join("src/index.web.js"), "new\n").unwrap();
    stage.publish().unwrap();

    assert!(!public.join("old-only.txt").exists());
    assert_eq!(
        std::fs::read_to_string(public.join("src/index.web.js")).unwrap(),
        "new\n"
    );
    assert_no_staging_residue(parent, "package");
}

#[test]
fn managed_stage_refuses_nonempty_unowned_or_wrongly_marked_roots() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();

    let unowned = parent.join("unowned");
    std::fs::create_dir(&unowned).unwrap();
    std::fs::write(unowned.join("user.txt"), b"keep\n").unwrap();
    let error = ManagedPackageStage::begin(&unowned).err().unwrap();
    assert!(format!("{error:#}").contains("lacks ownership marker"));
    assert_eq!(std::fs::read(unowned.join("user.txt")).unwrap(), b"keep\n");

    let wrong = parent.join("wrong");
    std::fs::create_dir(&wrong).unwrap();
    std::fs::write(wrong.join(MANAGED_PACKAGE_MARKER_NAME), b"wrong\n").unwrap();
    let error = ManagedPackageStage::begin(&wrong).err().unwrap();
    assert!(format!("{error:#}").contains("unexpected content"));
    assert_eq!(
        std::fs::read(wrong.join(MANAGED_PACKAGE_MARKER_NAME)).unwrap(),
        b"wrong\n"
    );
}

#[cfg(unix)]
#[test]
fn managed_stage_refuses_symlinked_ownership_marker() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let public = parent.join("package");
    std::fs::create_dir(&public).unwrap();
    std::fs::write(parent.join("marker"), MANAGED_PACKAGE_MARKER_CONTENT).unwrap();
    std::os::unix::fs::symlink(
        parent.join("marker"),
        public.join(MANAGED_PACKAGE_MARKER_NAME),
    )
    .unwrap();

    let error = ManagedPackageStage::begin(&public).err().unwrap();
    assert!(format!("{error:#}").contains("regular non-symlink file"));
}

#[test]
fn managed_stage_build_failure_preserves_public_generation() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let public = parent.join("package");
    publish_fixture(&public, "generation.txt", b"old\n");

    {
        let stage = ManagedPackageStage::begin(&public).unwrap();
        std::fs::write(stage.root().join("generation.txt"), b"partial\n").unwrap();
        // Returning from a managed build before publish drops the sibling
        // staging directory and never mutates the public generation.
        drop(stage);
    }

    assert_eq!(
        std::fs::read(public.join("generation.txt")).unwrap(),
        b"old\n"
    );
    assert_no_staging_residue(parent, "package");
}

#[test]
fn managed_stage_replaces_an_empty_unmarked_root() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let public = parent.join("package");
    std::fs::create_dir(&public).unwrap();

    publish_fixture(&public, "generated.txt", b"generated\n");
    assert_eq!(
        std::fs::read(public.join("generated.txt")).unwrap(),
        b"generated\n"
    );
}

#[test]
fn managed_entrypoint_output_is_deterministic() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let render = |root: Utf8PathBuf| {
        let layout = ManagedLayout {
            source_root: root.join("src/ffi"),
            artifact_root: root.join("artifacts"),
            host_crates_root: root.join("native/hosts"),
            package_dir: root.clone(),
        };
        layout.emit_web_entrypoint().unwrap();
        layout.emit_mini_program_entrypoint().unwrap();
        layout.emit_node_entrypoint().unwrap();
        layout.emit_electron_entrypoint().unwrap();
        [
            "src/index.web.js",
            "src/index.mini-program.js",
            "src/index.mini-program.d.ts",
            "src/index.node.js",
            "src/index.electron.js",
        ]
        .map(|path| std::fs::read(root.join(path)).unwrap())
    };

    let first = render(parent.join("one"));
    let second = render(parent.join("two"));
    assert_eq!(first, second);
    assert!(String::from_utf8(first[0].clone())
        .unwrap()
        .contains("export * from \"./ffi/browser/index.js\";"));
}

#[test]
fn managed_layout_derives_paths_without_creating_the_public_root() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("package");
    let mut args = empty_build_args();
    args.manifest_path = manifest;
    args.out_dir = None;
    args.managed_layout = true;
    args.package_dir = Some(package.clone());
    args.target = vec![ArtifactTargetArg::Wasm, ArtifactTargetArg::Node];
    let targets = expand_targets(&args.target).unwrap();

    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    assert_eq!(layout.package_dir, package);
    assert_eq!(
        args.out_dir.as_deref(),
        Some(package.join("src/ffi").as_path())
    );
    assert_eq!(
        args.host_crates_dir.as_deref(),
        Some(package.join("native/hosts").as_path())
    );
    assert_eq!(args.package_root.as_deref(), Some(package.as_path()));
    assert!(!package.exists());
}

#[test]
fn managed_android_aar_override_is_rebased_below_the_private_stage() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("package");
    let public_aar = package.join("custom/android/uni-core.aar");
    let mut args = empty_build_args();
    args.manifest_path = manifest;
    args.out_dir = None;
    args.managed_layout = true;
    args.package_dir = Some(package.clone());
    args.target = vec![ArtifactTargetArg::Android];
    args.android_aar_out = Some(public_aar.clone());
    let targets = expand_targets(&args.target).unwrap();

    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    assert_eq!(args.android_aar_out.as_deref(), Some(public_aar.as_path()));
    let stage = ManagedPackageStage::begin(&package).unwrap();
    let private = managed_private_args(&stage, &layout, &args, &targets).unwrap();
    assert_eq!(
        private.android_aar_out.as_deref(),
        Some(stage.root().join("custom/android/uni-core.aar").as_path())
    );
    drop(stage);
    assert!(!package.exists());
    assert_no_staging_residue(parent, "package");
}

#[test]
fn managed_layout_keeps_explicit_cargo_caches_outside_the_staging_package() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("package");
    let cache = parent.join("cargo-cache");
    let mut args = empty_build_args();
    args.manifest_path = manifest;
    args.out_dir = None;
    args.managed_layout = true;
    args.package_dir = Some(package.clone());
    args.target = vec![ArtifactTargetArg::Wasm, ArtifactTargetArg::Node];
    args.napi_target_dir = Some(cache.join("napi"));
    args.wasm_target_dir = Some(cache.join("wasm"));
    let targets = expand_targets(&args.target).unwrap();

    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    let stage = ManagedPackageStage::begin(&package).unwrap();
    let private = managed_private_args(&stage, &layout, &args, &targets).unwrap();
    let resolved_cache = canonicalize_invocation_output(&cache).unwrap();
    assert_eq!(private.napi_target_dir, Some(resolved_cache.join("napi")));
    assert_eq!(
        private.wasm_core_target_dir,
        Some(resolved_cache.join("wasm/core"))
    );
    assert_eq!(
        private.wasm_target_dir,
        Some(resolved_cache.join("wasm/host"))
    );
    assert!(!private.napi_target_dir.unwrap().starts_with(stage.root()));
    assert!(!private.wasm_target_dir.unwrap().starts_with(stage.root()));

    drop(stage);
    assert!(!package.exists());
    assert_no_staging_residue(parent, "package");
}

#[cfg(feature = "cli-ohos")]
#[test]
fn managed_layout_keeps_explicit_ohos_cargo_cache_outside_the_staging_package() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("package");
    let cache = parent.join("cargo-cache/ohos");
    let mut args = empty_build_args();
    args.manifest_path = manifest;
    args.out_dir = None;
    args.managed_layout = true;
    args.package_dir = Some(package.clone());
    args.target = vec![ArtifactTargetArg::Harmony];
    args.ohos_no_har = true;
    args.ohos_target_dir = Some(cache.clone());
    let targets = expand_targets(&args.target).unwrap();

    let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
    let stage = ManagedPackageStage::begin(&package).unwrap();
    let private = managed_private_args(&stage, &layout, &args, &targets).unwrap();
    assert_eq!(
        private.ohos_target_dir,
        Some(canonicalize_invocation_output(&cache).unwrap())
    );
    assert!(!private.ohos_target_dir.unwrap().starts_with(stage.root()));

    drop(stage);
    assert!(!package.exists());
    assert_no_staging_residue(parent, "package");
}

#[test]
fn managed_layout_rejects_cargo_cache_overlapping_the_public_package() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("package");
    publish_fixture(&package, "generation.txt", b"old generation\n");

    for cache in [
        package.join("target"),
        package.clone(),
        parent.to_path_buf(),
    ] {
        let mut args = empty_build_args();
        args.manifest_path = manifest.clone();
        args.out_dir = None;
        args.managed_layout = true;
        args.package_dir = Some(package.clone());
        args.target = vec![ArtifactTargetArg::Wasm];
        args.wasm_target_dir = Some(cache);
        let targets = expand_targets(&args.target).unwrap();
        let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
        let stage = ManagedPackageStage::begin(&package).unwrap();

        let error = match managed_private_args(&stage, &layout, &args, &targets) {
            Ok(_) => panic!("overlapping managed Cargo cache unexpectedly accepted"),
            Err(error) => error,
        };
        assert!(
            format!("{error:#}").contains("must be disjoint"),
            "unexpected overlap error: {error:#}"
        );
        drop(stage);
        assert_eq!(
            std::fs::read(package.join("generation.txt")).unwrap(),
            b"old generation\n"
        );
        assert_no_staging_residue(parent, "package");
    }
}

#[test]
fn managed_android_aar_escape_fails_before_staging_and_preserves_external_files() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("package");
    publish_fixture(&package, "generation.txt", b"old generation\n");

    for (label, override_path) in [
        ("dotdot", package.join("../outside-dotdot.aar")),
        ("absolute", parent.join("outside-absolute.aar")),
    ] {
        let outside = parent.join(format!("outside-{label}.aar"));
        std::fs::write(&outside, format!("{label} sentinel\n")).unwrap();
        let before = std::fs::read(&outside).unwrap();
        let mut args = empty_build_args();
        args.manifest_path = manifest.clone();
        args.out_dir = None;
        args.managed_layout = true;
        args.package_dir = Some(package.clone());
        args.target = vec![ArtifactTargetArg::Android];
        args.android_aar_out = Some(override_path);

        let error = build(args).unwrap_err();
        assert!(
            format!("{error:#}").contains("managed Android AAR output"),
            "unexpected {label} escape error: {error:#}"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), before);
        assert_eq!(
            std::fs::read(package.join("generation.txt")).unwrap(),
            b"old generation\n"
        );
        assert_eq!(
            std::fs::read(package.join(MANAGED_PACKAGE_MARKER_NAME)).unwrap(),
            MANAGED_PACKAGE_MARKER_CONTENT
        );
        assert_no_staging_residue(parent, "package");
    }
}

#[cfg(feature = "cli-ohos")]
#[test]
fn expands_all_js_targets() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::AllJs]).unwrap(),
        ExpandedTargets {
            wasm: true,
            mini_program: true,
            node: true,
            electron: true,
            harmony: true,
            apple: false,
            android: false,
        }
    );
}

#[cfg(not(feature = "cli-ohos"))]
#[test]
fn expands_all_js_targets() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::AllJs]).unwrap(),
        ExpandedTargets {
            wasm: true,
            mini_program: true,
            node: true,
            electron: true,
            apple: false,
            android: false,
        }
    );
}

#[cfg(feature = "cli-ohos")]
#[test]
fn expands_all_targets() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::All]).unwrap(),
        ExpandedTargets {
            wasm: true,
            mini_program: true,
            node: true,
            electron: true,
            harmony: true,
            apple: true,
            android: true,
        }
    );
}

#[cfg(not(feature = "cli-ohos"))]
#[test]
fn expands_all_targets() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::All]).unwrap(),
        ExpandedTargets {
            wasm: true,
            mini_program: true,
            node: true,
            electron: true,
            apple: true,
            android: true,
        }
    );
}

#[test]
fn expands_node_electron_as_one_napi_group() {
    assert_eq!(
        expand_targets(&[ArtifactTargetArg::Node, ArtifactTargetArg::Electron]).unwrap(),
        ExpandedTargets {
            node: true,
            electron: true,
            ..ExpandedTargets::default()
        }
    );
}

#[test]
fn rejects_empty_target_list() {
    assert!(expand_targets(&[]).is_err());
}

#[test]
fn managed_native_only_targets_skip_javascript_package_preparation() {
    for target in [ArtifactTargetArg::Apple, ArtifactTargetArg::Android] {
        let targets = expand_targets(&[target]).unwrap();
        assert!(
            !targets.requires_javascript_package(),
            "managed {target:?} invocation must not prepare a JavaScript package"
        );
    }
    let targets = expand_targets(&[ArtifactTargetArg::Apple, ArtifactTargetArg::Android]).unwrap();
    assert!(!targets.requires_javascript_package());
}

#[test]
fn managed_mixed_targets_share_one_javascript_package_plan() {
    let targets = expand_targets(&[ArtifactTargetArg::All]).unwrap();
    assert!(targets.requires_javascript_package());

    // The managed coordinator owns the single preparation call and passes its
    // result through the private target set; native-only targets take the
    // `None` branch above.
    let artifacts_src = include_str!("../artifacts.rs");
    assert_eq!(
        artifacts_src
            .matches("preparing one managed JavaScript package")
            .count(),
        1,
        "managed orchestration must prepare one shared JavaScript package"
    );
}

#[test]
fn ordinary_mixed_targets_share_one_javascript_package_plan() {
    let mixed_targets = expand_targets(&[
        ArtifactTargetArg::Wasm,
        #[cfg(feature = "cli-ohos")]
        ArtifactTargetArg::Harmony,
        #[cfg(not(feature = "cli-ohos"))]
        ArtifactTargetArg::Node,
    ])
    .unwrap();
    assert!(mixed_targets.requires_javascript_package());
    let all_targets = expand_targets(&[ArtifactTargetArg::All]).unwrap();
    assert!(all_targets.requires_javascript_package());

    // Every JavaScript-capable ordinary target, including Harmony when the
    // OHOS feature is enabled, must consume this one package.  Keep the
    // source-level count as a regression guard: a second ordinary prepare
    // call would create a second generation in one invocation.
    let artifacts_src = include_str!("../artifacts.rs");
    assert_eq!(
        artifacts_src
            .matches("prepare_javascript_package(&args, &targets)")
            .count(),
        1,
        "ordinary orchestration must prepare one shared JavaScript package"
    );
    assert!(
        !artifacts_src
            .contains("preparing one JavaScript package for the Harmony artifact invocation"),
        "ordinary Harmony must not prepare a second JavaScript package"
    );
}

#[test]
fn managed_javascript_builders_fail_closed_without_prepared_package() {
    let args = empty_build_args();
    for targets in [
        ExpandedTargets {
            wasm: true,
            ..ExpandedTargets::default()
        },
        ExpandedTargets {
            node: true,
            ..ExpandedTargets::default()
        },
    ] {
        let error = build_private_target_set(&args, &targets, None).unwrap_err();
        assert!(
            format!("{error:#}").contains("no prepared JavaScript package"),
            "JavaScript builder did not fail closed: {error:#}"
        );
    }
}

#[test]
fn managed_apple_only_skips_javascript_preparation_before_native_failure() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("apple-package");
    publish_fixture(&package, "generation.txt", b"old generation\n");

    let mut args = empty_build_args();
    args.manifest_path = manifest;
    args.out_dir = None;
    args.managed_layout = true;
    args.package_dir = Some(package.clone());
    args.target = vec![ArtifactTargetArg::Apple];
    args.cargo_bin = parent.join("missing-cargo").to_string();

    let error = build(args).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(!rendered.contains("JavaScript package preparation requires"));
    assert!(rendered.contains("building private managed Apple target"));
    assert_eq!(
        std::fs::read(package.join("generation.txt")).unwrap(),
        b"old generation\n"
    );
    assert_no_staging_residue(parent, "apple-package");
}

#[test]
fn managed_android_only_skips_javascript_preparation_before_native_failure() {
    let temp = tempfile::tempdir().unwrap();
    let parent = Utf8Path::from_path(temp.path()).unwrap();
    let manifest = write_test_manifest(&parent.join("crate"));
    let package = parent.join("android-package");
    publish_fixture(&package, "generation.txt", b"old generation\n");
    let ndk = parent.join("ndk");
    let prebuilt = ndk.join("toolchains/llvm/prebuilt/darwin-x86_64");
    std::fs::create_dir_all(prebuilt.join("bin")).unwrap();
    std::fs::write(
        prebuilt.join("bin/aarch64-linux-android23-clang"),
        b"fake clang\n",
    )
    .unwrap();

    let mut args = empty_build_args();
    args.manifest_path = manifest;
    args.out_dir = None;
    args.managed_layout = true;
    args.package_dir = Some(package.clone());
    args.target = vec![ArtifactTargetArg::Android];
    args.android_abi = vec!["arm64-v8a".to_string()];
    args.android_ndk_home = Some(ndk);
    args.cargo_bin = parent.join("missing-cargo").to_string();

    let error = build(args).unwrap_err();
    let rendered = format!("{error:#}");
    assert!(!rendered.contains("JavaScript package preparation requires"));
    assert!(rendered.contains("building private managed Android target"));
    assert_eq!(
        std::fs::read(package.join("generation.txt")).unwrap(),
        b"old generation\n"
    );
    assert_no_staging_residue(parent, "android-package");
}

#[test]
fn apple_helpers_derive_package_contract_names() {
    let meta = test_cargo_metadata(Utf8PathBuf::from("/repo/target"));
    assert_eq!(apple_package_product_name(&meta), "UniCoreApple");
    assert_eq!(apple_binary_target_name(&meta), "uni_coreFFI");
    assert_eq!(
        upper_camel_case_identifier("hello-world_core"),
        "HelloWorldCore"
    );
}

#[test]
fn computes_apple_cdylib_path() {
    let meta = test_cargo_metadata(Utf8PathBuf::from("/repo/target"));
    assert_eq!(
        apple_cdylib_path(&meta, "aarch64-apple-ios", "release"),
        Utf8PathBuf::from("/repo/target/aarch64-apple-ios/release/libuni_core.dylib")
    );
}

#[test]
fn renders_xcodebuild_create_xcframework_args() {
    let args = xcodebuild_create_xcframework_args(
        &[
            Utf8PathBuf::from("/target/device/uni_coreFFI.framework"),
            Utf8PathBuf::from("/target/sim/uni_coreFFI.framework"),
        ],
        Utf8Path::new("/out/uni_core.xcframework"),
    );
    assert_eq!(
        args,
        vec![
            "-create-xcframework",
            "-framework",
            "/target/device/uni_coreFFI.framework",
            "-framework",
            "/target/sim/uni_coreFFI.framework",
            "-output",
            "/out/uni_core.xcframework",
        ]
    );
}

#[test]
fn maps_android_abi() {
    assert_eq!(
        android_abi("arm64-v8a").unwrap(),
        AndroidAbi {
            abi: "arm64-v8a",
            rust_target: "aarch64-linux-android",
            clang_prefix: "aarch64-linux-android",
        }
    );
    assert_eq!(
        android_abi("armeabi-v7a").unwrap().clang_prefix,
        "armv7a-linux-androideabi"
    );
    assert!(android_abi("mips").is_err());
}

#[test]
fn computes_android_linker_env() {
    assert_eq!(
        android_linker_env("aarch64-linux-android"),
        "CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER"
    );
}

#[test]
fn computes_android_sharedlib_path() {
    let meta = test_cargo_metadata(Utf8PathBuf::from("/repo/target"));
    assert_eq!(
        android_sharedlib_path(&meta, "aarch64-linux-android", "debug"),
        Utf8PathBuf::from("/repo/target/aarch64-linux-android/debug/libuni_core.so")
    );
}

#[test]
fn renders_android_manifest() {
    assert!(android_manifest("com.example.core").contains("package=\"com.example.core\""));
}

#[test]
fn artifacts_cli_has_no_manifest_or_managed_transaction_protocol() {
    let artifacts_src = include_str!("../artifacts.rs");
    let engine_src = include_str!("engine.rs");
    for forbidden in [
        "ManagedPackageTransaction",
        "ManagedPackageJournal",
        "preflight_managed_package",
        "merge_existing_manifest",
    ] {
        assert!(
            !artifacts_src.contains(forbidden) && !engine_src.contains(forbidden),
            "managed publisher still contains legacy protocol token `{forbidden}`"
        );
    }
    assert!(artifacts_src.contains("ManagedPackageStage::begin"));
    assert!(engine_src.contains(".uniffi-managed-owner"));
}

#[test]
fn artifacts_cli_no_longer_exposes_checkout_tool_flags() {
    let artifacts_src = include_str!("../artifacts.rs");
    for forbidden in [
        concat!("wasm-bindgen", "-dir"),
        concat!("ohos-rs", "-dir"),
        concat!("wasm-bindgen", "-bin"),
        concat!("ohrs", "-bin"),
        concat!("resolve_wasm", "_bindgen_bin"),
        concat!("resolve_ohrs", "_bin"),
    ] {
        assert!(!artifacts_src.contains(forbidden));
    }
}

#[test]
fn javascript_build_defaults_to_embedded_tooling() {
    let javascript_src = include_str!("../javascript.rs");
    assert!(javascript_src.contains("emit_wasm_post_link"));
    #[cfg(feature = "cli-ohos")]
    assert!(javascript_src.contains("super::ohos::build"));
}

#[test]
fn javascript_support_is_validated_and_copied_into_the_source_root() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8Path::from_path(temp.path()).unwrap();
    let source = root.join("consumer-support");
    std::fs::create_dir_all(source.join("nested")).unwrap();
    std::fs::write(source.join("email.js"), "export const email = true;\n").unwrap();
    std::fs::write(
        source.join("nested/email.d.ts"),
        "export declare const email: boolean;\n",
    )
    .unwrap();

    let package = root.join("package");
    let mut args = empty_build_args();
    args.out_dir = Some(package.join("src/ffi"));
    args.package_root = Some(package.clone());
    args.javascript_support_dir = Some(source.clone());
    install_javascript_support(&args).unwrap();

    assert_eq!(
        std::fs::read_to_string(package.join("src/ffi/support/email.js")).unwrap(),
        "export const email = true;\n"
    );
    assert!(package.join("src/ffi/support/nested/email.d.ts").is_file());

    std::fs::write(source.join("invalid.bin"), [0xff, 0xfe]).unwrap();
    let error = install_javascript_support(&args).unwrap_err().to_string();
    assert!(error.contains("not UTF-8 text"), "{error}");
    assert_eq!(
        std::fs::read_to_string(package.join("src/ffi/support/email.js")).unwrap(),
        "export const email = true;\n",
        "validation failure must leave the previously copied support tree unchanged"
    );
}

#[cfg(feature = "cli-ohos")]
#[test]
fn artifacts_cli_wires_harmony_har_options() {
    let artifacts_src = include_str!("../artifacts.rs");
    for required in [
        concat!("ohos-package", "-name"),
        concat!("ohos-package", "-type"),
        concat!("ohos-integrated", "-hsp"),
        concat!("ohos-runtime-hsp", "-out"),
        concat!("ohos-interface-har", "-out"),
        concat!("ohos-no", "-har"),
    ] {
        assert!(artifacts_src.contains(required));
    }
}
