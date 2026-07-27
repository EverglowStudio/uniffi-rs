/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::javascript::{
    build_napi, build_ohos, build_ohos_deferred, build_wasm, emit_mini_program_wasm_runtime,
    generate_js, mini_program_default_wasm_path, rebase_mini_program_auto_entrypoint,
    BuildNapiArgs, BuildOhosArgs, BuildWasmArgs, NapiBuildFlavorArg, WasmBindgenTargetArg,
};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::MetadataCommand;
use clap::{Args, Subcommand, ValueEnum};
use std::collections::BTreeSet;
use std::io::{Seek, Write};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::process::Command;
use uniffi_bindgen::bindings::{generate, GenerateOptions, TargetLanguage};
use uniffi_bindgen_javascript::{FlavorTarget, HostCrateOptions};

use super::artifact_transaction::*;
use super::ohos::publication_hooks;

#[derive(Args)]
pub(crate) struct ArtifactsArgs {
    #[clap(subcommand)]
    pub command: ArtifactsCommands,
}

#[derive(Subcommand)]
pub(crate) enum ArtifactsCommands {
    /// Build final UniFFI consumer artifacts.
    Build(BuildArgs),
}

#[derive(Clone, Args)]
pub(crate) struct BuildArgs {
    /// Downstream core crate Cargo.toml.
    #[clap(long = "manifest-path")]
    manifest_path: Utf8PathBuf,

    /// Directory in which to write generated files. Required unless --managed-layout is used.
    #[clap(long, short)]
    out_dir: Option<Utf8PathBuf>,

    /// Artifact target(s) to build. P0/P1 supports wasm, node, electron, and harmony.
    #[clap(long = "target", value_enum)]
    target: Vec<ArtifactTargetArg>,

    /// Optional override for the library/cdylib path used for generation.
    #[clap(long = "library-path")]
    library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the generator.
    #[clap(long)]
    source: Option<Utf8PathBuf>,

    /// Directory (default `rust_modules`, or `<package-dir>/artifacts/rust` in managed mode) in which to emit generated host crates.
    #[clap(long = "host-crates-dir")]
    host_crates_dir: Option<Utf8PathBuf>,

    #[clap(skip)]
    logical_host_crates_dir: Option<Utf8PathBuf>,

    /// The invocation coordinator owns the complete cross-target output lock set.
    #[clap(skip)]
    invocation_output_lock_held: bool,

    /// Directory for built non-source artifacts such as wasm-bindgen pkg, `.node` addons, and OHOS dist output.
    #[clap(long = "artifact-dir")]
    artifact_dir: Option<Utf8PathBuf>,

    /// Opt in to a package-oriented artifact layout rooted at --package-dir.
    #[clap(long = "managed-layout")]
    managed_layout: bool,

    /// Package root used by --managed-layout. Defaults to the current working directory.
    #[clap(long = "package-dir")]
    package_dir: Option<Utf8PathBuf>,

    /// Build downstream core and generated host crates in release mode.
    #[clap(long)]
    release: bool,

    /// Cargo features enabled when building native Apple/Android/Harmony/N-API core artifacts. May be repeated or comma-separated.
    #[clap(long = "cargo-feature", value_delimiter = ',')]
    cargo_features: Vec<String>,

    /// Override the `cargo` binary to invoke.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    cargo_bin: String,

    /// Do not try to format generated bindings.
    #[clap(long, short)]
    no_format: bool,

    /// Path to optional uniffi config file.
    #[clap(long, short)]
    config: Option<Utf8PathBuf>,

    /// Optional crate filter passed through to generators.
    #[clap(long = "crate")]
    crate_name: Option<String>,

    /// Whether we should exclude dependencies when running cargo metadata.
    #[clap(long)]
    metadata_no_deps: bool,

    /// Where to write the wasm-bindgen output tree. Defaults to `<out-dir>/browser/pkg`.
    #[clap(long = "wasm-bindgen-out-dir")]
    wasm_bindgen_out_dir: Option<Utf8PathBuf>,

    /// wasm-bindgen output target.
    #[clap(long = "wasm-bindgen-target", value_enum, default_value = "web")]
    wasm_bindgen_target: WasmBindgenTargetArg,

    /// Cargo target directory for the generated N-API host build.
    #[clap(long = "napi-target-dir")]
    napi_target_dir: Option<Utf8PathBuf>,

    /// Cargo target directory for the generated wasm host build.
    #[clap(long = "wasm-target-dir")]
    wasm_target_dir: Option<Utf8PathBuf>,

    /// Invocation-private Cargo target directory for the downstream wasm core build.
    #[clap(skip)]
    wasm_core_target_dir: Option<Utf8PathBuf>,

    /// Output directory for built OHOS dist artifacts (intermediate native output).
    #[clap(long = "ohos-dist-dir")]
    pub(super) ohos_dist_dir: Option<Utf8PathBuf>,

    /// OHPM package name for generated HAR metadata (supports scoped names like `@scope/name`).
    #[clap(long = "ohos-package-name")]
    pub(super) ohos_package_name: Option<String>,

    /// Harmony module name override.
    #[clap(long = "ohos-module-name")]
    ohos_module_name: Option<String>,

    /// Semantic version override for generated OHPM package metadata.
    #[clap(long = "ohos-package-version")]
    ohos_package_version: Option<String>,

    /// Author override for generated OHPM package metadata.
    #[clap(long = "ohos-author")]
    ohos_author: Option<String>,

    /// SPDX license override for generated OHPM package metadata.
    #[clap(long = "ohos-license")]
    ohos_license: Option<String>,

    /// Description override for generated OHPM package metadata.
    #[clap(long = "ohos-description")]
    ohos_description: Option<String>,

    /// Minimum compatible Harmony/OpenHarmony SDK version. Must be explicit for final HAR/HSP packaging.
    #[clap(long = "ohos-compatible-sdk-version")]
    ohos_compatible_sdk_version: Option<String>,

    /// Target Harmony/OpenHarmony SDK version. Defaults to the resolved compile SDK.
    #[clap(long = "ohos-target-sdk-version")]
    ohos_target_sdk_version: Option<String>,

    /// Compatible SDK type, such as HarmonyOS or OpenHarmony.
    #[clap(long = "ohos-compatible-sdk-type")]
    ohos_compatible_sdk_type: Option<String>,

    /// Supported Harmony device type. May be repeated or comma-separated.
    #[clap(long = "ohos-device-type", value_delimiter = ',')]
    ohos_device_types: Vec<String>,

    /// Final Harmony package kind. HAR remains the backward-compatible default.
    #[clap(long = "ohos-package-type", value_enum, default_value = "har")]
    pub(super) ohos_package_kind: super::ohos::PackageKind,

    /// Build an app-independent integrated HSP.
    #[clap(long = "ohos-integrated-hsp")]
    pub(super) ohos_integrated_hsp: bool,

    /// Host application bundleName for a non-integrated HSP.
    #[clap(long = "ohos-hsp-bundle-name")]
    ohos_hsp_bundle_name: Option<String>,

    /// Output `.har` path. Defaults to `<artifact-root>/<package>.har`.
    #[clap(long = "ohos-har-out")]
    pub(super) ohos_har_out: Option<Utf8PathBuf>,

    /// Standalone runtime HSP extracted from the release tgz.
    #[clap(long = "ohos-runtime-hsp-out")]
    pub(super) ohos_runtime_hsp_out: Option<Utf8PathBuf>,

    /// Standalone Interface HAR extracted from the release tgz.
    #[clap(long = "ohos-interface-har-out")]
    pub(super) ohos_interface_har_out: Option<Utf8PathBuf>,

    /// Original release tgz emitted by Hvigor assembleHsp.
    #[clap(long = "ohos-tgz-out")]
    pub(super) ohos_tgz_out: Option<Utf8PathBuf>,

    /// Hvigor wrapper used to build the final compiled HAR.
    #[clap(long = "ohos-hvigorw")]
    ohos_hvigorw: Option<String>,

    /// OHPM executable used to resolve and prepublish the final Harmony package.
    #[clap(long = "ohos-ohpm")]
    ohos_ohpm: Option<String>,

    /// DevEco SDK root used by the generated Hvigor project.
    #[clap(long = "ohos-deveco-sdk-home")]
    ohos_deveco_sdk_home: Option<Utf8PathBuf>,

    /// Skip final HAR packaging and keep only `dist/` intermediate outputs.
    #[clap(long = "ohos-no-har")]
    pub(super) ohos_no_har: bool,

    /// OHOS architecture alias for the built-in OHOS builder. Defaults to `aarch` and `x64`.
    #[clap(long = "ohos-arch")]
    ohos_arch: Vec<String>,

    /// Cargo target directory for the generated OHOS host build.
    #[clap(long = "ohos-target-dir")]
    ohos_target_dir: Option<Utf8PathBuf>,

    /// Copy OHOS static `.a` libraries in addition to shared `.so` artifacts.
    #[clap(long = "ohos-static")]
    ohos_static: bool,

    /// Skip copying OHOS native libraries; still generate TypeScript declarations.
    #[clap(long = "ohos-skip-libs")]
    pub(super) ohos_skip_libs: bool,

    /// Reuse the generated OHOS type definition cache.
    #[clap(long = "ohos-dts-cache")]
    ohos_dts_cache: bool,

    /// Skip OHOS napi package version checks.
    #[clap(long = "ohos-skip-check")]
    ohos_skip_check: bool,

    /// Use `cargo zigbuild` for OHOS host builds.
    #[clap(long = "ohos-zigbuild")]
    ohos_zigbuild: bool,

    /// Use HarmonyOS BiSheng toolchain paths for OHOS host builds.
    #[clap(long = "ohos-bisheng")]
    ohos_bisheng: bool,

    /// Package to build when the generated OHOS manifest is a workspace root.
    #[clap(long = "ohos-package")]
    ohos_package: Option<String>,

    /// Skip the check that candidate OHOS packages depend on napi-derive-ohos.
    #[clap(long = "ohos-skip-napi-check")]
    ohos_skip_napi_check: bool,

    /// SONAME linker value for the generated OHOS shared library.
    #[clap(long = "ohos-soname")]
    ohos_soname: Option<String>,

    /// Additional cargo args passed to the OHOS host cargo build after `--`.
    #[clap(last = true)]
    ohos_cargo_args: Vec<String>,

    /// Apple Rust target triple. Defaults to iOS device and Apple Silicon simulator.
    #[clap(long = "apple-target")]
    apple_target: Vec<String>,

    /// Output `.xcframework` path for `--target apple`.
    #[clap(long = "apple-xcframework-out")]
    apple_xcframework_out: Option<Utf8PathBuf>,

    /// Optional directory in which to copy generated Swift sources.
    #[clap(long = "apple-swift-out")]
    apple_swift_out: Option<Utf8PathBuf>,

    /// Optional XCFramework name. Defaults to the output path stem.
    #[clap(long = "apple-framework-name")]
    apple_framework_name: Option<String>,

    /// Android ABI. Defaults to arm64-v8a and x86_64.
    #[clap(long = "android-abi")]
    android_abi: Vec<String>,

    /// Android API level used for NDK clang wrappers.
    #[clap(long = "android-api", default_value_t = 23)]
    android_api: u32,

    /// Android NDK home. Falls back to ANDROID_NDK_HOME or latest ANDROID_SDK_ROOT/ndk/*.
    #[clap(long = "android-ndk-home")]
    android_ndk_home: Option<Utf8PathBuf>,

    /// jniLibs output directory for `--target android`.
    #[clap(long = "android-jni-libs-out")]
    android_jni_libs_out: Option<Utf8PathBuf>,

    /// Optional directory in which to copy generated Kotlin sources.
    #[clap(long = "android-kotlin-out")]
    android_kotlin_out: Option<Utf8PathBuf>,

    /// Kotlin package name override.
    #[clap(long = "android-package-name")]
    android_package_name: Option<String>,

    /// Optional AAR output path. Not implemented in P3.
    #[clap(long = "android-aar-out")]
    android_aar_out: Option<Utf8PathBuf>,
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, ValueEnum)]
pub(crate) enum ArtifactTargetArg {
    Wasm,
    #[clap(name = "mini-program")]
    MiniProgram,
    Node,
    Electron,
    Harmony,
    Apple,
    Android,
    #[clap(name = "all-js")]
    AllJs,
    All,
}

#[derive(Default, Debug, Eq, PartialEq)]
struct ExpandedTargets {
    wasm: bool,
    mini_program: bool,
    node: bool,
    electron: bool,
    harmony: bool,
    apple: bool,
    android: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ManagedLayout {
    package_dir: Utf8PathBuf,
    source_root: Utf8PathBuf,
    pub(super) artifact_root: Utf8PathBuf,
    host_crates_root: Utf8PathBuf,
    pub(super) manifest_path: Utf8PathBuf,
}

impl ManagedTransactionLayout for ManagedLayout {
    fn package_root(&self) -> &Utf8Path {
        &self.package_dir
    }
}

fn managed_private_args(
    transaction: &ManagedPackageTransaction,
    layout: &ManagedLayout,
    public: &BuildArgs,
) -> Result<BuildArgs> {
    let mut private = public.clone();
    let rebase = |path: &Utf8Path| -> Result<Utf8PathBuf> {
        let relative = path
            .strip_prefix(&layout.package_dir)
            .with_context(|| format!("managed output escaped package root: {path}"))?;
        Ok(transaction.candidate_root().join(relative))
    };
    private.out_dir = public.out_dir.as_deref().map(rebase).transpose()?;
    private.host_crates_dir = public.host_crates_dir.as_deref().map(rebase).transpose()?;
    private.artifact_dir = public.artifact_dir.as_deref().map(rebase).transpose()?;
    private.wasm_bindgen_out_dir = public
        .wasm_bindgen_out_dir
        .as_deref()
        .map(rebase)
        .transpose()?;
    private.ohos_dist_dir = public.ohos_dist_dir.as_deref().map(rebase).transpose()?;
    private.ohos_har_out = public.ohos_har_out.as_deref().map(rebase).transpose()?;
    private.ohos_runtime_hsp_out = public
        .ohos_runtime_hsp_out
        .as_deref()
        .map(rebase)
        .transpose()?;
    private.ohos_interface_har_out = public
        .ohos_interface_har_out
        .as_deref()
        .map(rebase)
        .transpose()?;
    private.ohos_tgz_out = public.ohos_tgz_out.as_deref().map(rebase).transpose()?;
    private.apple_xcframework_out = public
        .apple_xcframework_out
        .as_deref()
        .map(rebase)
        .transpose()?;
    private.apple_swift_out = public.apple_swift_out.as_deref().map(rebase).transpose()?;
    private.android_jni_libs_out = public
        .android_jni_libs_out
        .as_deref()
        .map(rebase)
        .transpose()?;
    private.android_kotlin_out = public
        .android_kotlin_out
        .as_deref()
        .map(rebase)
        .transpose()?;
    private.android_aar_out = public.android_aar_out.as_deref().map(rebase).transpose()?;
    private.package_dir = Some(transaction.candidate_root().to_path_buf());
    let build_root = transaction.build_root();
    private.napi_target_dir = Some(build_root.join("napi"));
    private.wasm_core_target_dir = Some(build_root.join("wasm/core"));
    private.wasm_target_dir = Some(build_root.join("wasm/host"));
    private.ohos_target_dir = Some(build_root.join("ohos"));
    private.logical_host_crates_dir = None;
    private.managed_layout = false;
    private.invocation_output_lock_held = true;
    Ok(private)
}

fn clear_managed_selected_roots(
    transaction: &mut ManagedPackageTransaction,
    targets: &ExpandedTargets,
) -> Result<()> {
    let mut paths = Vec::new();
    if targets.harmony {
        paths.push("artifacts/harmony");
    }
    if targets.apple {
        paths.extend(["artifacts/apple", "src/ffi/swift", "src/ffi/apple"]);
    }
    if targets.android {
        paths.extend(["artifacts/android", "src/ffi/kotlin", "src/ffi/android"]);
    }
    transaction.clear_seeded_paths(&paths)
}

struct InvocationMirror {
    guard: super::artifact_transaction::IdentityBoundInvocationRoot,
    root: Utf8PathBuf,
    build_root: Utf8PathBuf,
}

impl InvocationMirror {
    fn new() -> Result<Self> {
        let guard = super::artifact_transaction::IdentityBoundInvocationRoot::create(
            "uniffi-artifacts-invocation",
        )
        .context("creating invocation-private artifact mirror")?;
        let root = guard.mirror_root().to_path_buf();
        let build_root = guard.build_root().to_path_buf();
        Ok(Self {
            guard,
            root,
            build_root,
        })
    }

    fn finish<T>(&mut self, result: Result<T>) -> Result<T> {
        self.guard.finish(result, "artifact")
    }

    fn seal(&mut self) -> Result<()> {
        self.guard.seal()
    }

    fn map(&self, path: &Utf8Path) -> Result<Utf8PathBuf> {
        let path = canonicalize_invocation_output(path)?;
        let mut mapped = self.root.clone();
        for component in path.components() {
            let value = component.as_str();
            if value.is_empty() || value == "/" || value == "\\" {
                continue;
            }
            // Encode every UTF-8 byte.  Unlike the previous `:` replacement,
            // this is injective on every legal Utf8Path component and it also
            // avoids case-only private names on case-insensitive temp volumes.
            let mut encoded = String::with_capacity(value.len() * 2 + 24);
            encoded.push('c');
            encoded.push_str(&value.len().to_string());
            encoded.push('-');
            for byte in value.as_bytes() {
                use std::fmt::Write as _;
                write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
            }
            mapped.push(encoded);
        }
        Ok(mapped)
    }
}

impl ManagedLayout {
    pub(super) fn rebased(&self, from: &Utf8Path, to: &Utf8Path) -> Result<Self> {
        let rebase = |path: &Utf8Path| -> Result<Utf8PathBuf> {
            Ok(to
                .join(path.strip_prefix(from).with_context(|| {
                    format!("managed layout path escaped package root: {path}")
                })?))
        };
        Ok(Self {
            package_dir: to.to_path_buf(),
            source_root: rebase(&self.source_root)?,
            artifact_root: rebase(&self.artifact_root)?,
            host_crates_root: rebase(&self.host_crates_root)?,
            manifest_path: rebase(&self.manifest_path)?,
        })
    }

    fn mirrored(&self, mirror: &InvocationMirror) -> Result<Self> {
        Ok(Self {
            package_dir: mirror.map(&self.package_dir)?,
            source_root: mirror.map(&self.source_root)?,
            artifact_root: mirror.map(&self.artifact_root)?,
            host_crates_root: mirror.map(&self.host_crates_root)?,
            manifest_path: mirror.map(&self.manifest_path)?,
        })
    }

    fn apply(args: &mut BuildArgs, targets: &ExpandedTargets) -> Result<Option<Self>> {
        if !args.managed_layout {
            if args.package_dir.is_some() {
                bail!("--package-dir requires --managed-layout");
            }
            if args.out_dir.is_none() {
                bail!("--out-dir <dir> is required unless --managed-layout is used");
            }
            return Ok(None);
        }
        if args.out_dir.is_some() {
            bail!("--managed-layout derives --out-dir; omit --out-dir");
        }
        if args.host_crates_dir.is_some() {
            bail!("--managed-layout derives --host-crates-dir; omit --host-crates-dir");
        }
        if args.artifact_dir.is_some() {
            bail!("--managed-layout derives --artifact-dir; omit --artifact-dir");
        }
        if args.wasm_bindgen_out_dir.is_some() {
            bail!("--managed-layout derives --wasm-bindgen-out-dir; omit --wasm-bindgen-out-dir");
        }
        if args.ohos_dist_dir.is_some() {
            bail!("--managed-layout derives --ohos-dist-dir; omit --ohos-dist-dir");
        }
        if args.ohos_har_out.is_some() {
            bail!("--managed-layout derives --ohos-har-out; omit --ohos-har-out");
        }
        if args.ohos_runtime_hsp_out.is_some() {
            bail!("--managed-layout derives --ohos-runtime-hsp-out; omit --ohos-runtime-hsp-out");
        }
        if args.ohos_interface_har_out.is_some() {
            bail!(
                "--managed-layout derives --ohos-interface-har-out; omit --ohos-interface-har-out"
            );
        }
        if args.ohos_tgz_out.is_some() {
            bail!("--managed-layout derives --ohos-tgz-out; omit --ohos-tgz-out");
        }
        if args.apple_xcframework_out.is_some() {
            bail!("--managed-layout derives --apple-xcframework-out; omit --apple-xcframework-out");
        }
        if args.apple_swift_out.is_some() {
            bail!("--managed-layout writes Swift sources under src/ffi; omit --apple-swift-out");
        }
        if args.android_jni_libs_out.is_some() {
            bail!("--managed-layout derives --android-jni-libs-out; omit --android-jni-libs-out");
        }
        if args.android_kotlin_out.is_some() {
            bail!(
                "--managed-layout writes Kotlin sources under src/ffi; omit --android-kotlin-out"
            );
        }

        let package_dir = args
            .package_dir
            .clone()
            .unwrap_or_else(|| Utf8PathBuf::from("."));
        let package_dir = resolve_cwd_path(&package_dir)?;
        let meta = cargo_package_metadata(&args.manifest_path)?;
        let source_root = package_dir.join("src/ffi");
        let artifact_root = package_dir.join("artifacts");
        let host_crates_root = artifact_root.join("rust");
        let manifest_path = package_dir.join("artifact-manifest.json");

        args.out_dir = Some(source_root.clone());
        args.host_crates_dir = Some(host_crates_root.clone());
        args.artifact_dir = Some(artifact_root.clone());
        if targets.harmony {
            args.ohos_dist_dir = Some(artifact_root.join("harmony/dist"));
            if !args.ohos_no_har {
                let harmony_package = args
                    .ohos_package_name
                    .clone()
                    .unwrap_or_else(|| format!("{}-ohos", meta.package_name));
                args.ohos_package_name = Some(harmony_package.clone());
                let stem = harmony_archive_stem(&harmony_package)?;
                match args.ohos_package_kind {
                    super::ohos::PackageKind::Har => {
                        args.ohos_har_out =
                            Some(artifact_root.join("harmony").join(format!("{stem}.har")));
                    }
                    super::ohos::PackageKind::Hsp => {
                        args.ohos_runtime_hsp_out =
                            Some(artifact_root.join("harmony").join(format!("{stem}.hsp")));
                        args.ohos_interface_har_out = Some(
                            artifact_root
                                .join("harmony")
                                .join(format!("{stem}-interface.har")),
                        );
                        args.ohos_tgz_out =
                            Some(artifact_root.join("harmony").join(format!("{stem}.tgz")));
                    }
                }
            }
        }
        if targets.apple {
            args.apple_xcframework_out = Some(
                artifact_root
                    .join("apple")
                    .join(format!("{}.xcframework", meta.lib_target_name)),
            );
        }
        if targets.android {
            args.android_jni_libs_out = Some(artifact_root.join("android/jniLibs"));
        }

        Ok(Some(Self {
            package_dir,
            source_root,
            artifact_root,
            host_crates_root,
            manifest_path,
        }))
    }

    fn emit(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
    ) -> Result<()> {
        self.emit_supporting_files(targets, meta, args)?;
        let manifest = self.render_manifest(targets, meta, args)?;
        write_file_atomically(&self.manifest_path, manifest.as_bytes())?;
        Ok(())
    }

    fn emit_supporting_files(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
    ) -> Result<()> {
        if targets.wasm {
            self.emit_web_entrypoint()?;
        }
        if targets.mini_program {
            self.emit_mini_program_entrypoint()?;
        }
        if targets.node {
            self.emit_node_entrypoint()?;
        }
        if targets.electron {
            self.emit_electron_entrypoint()?;
        }
        if targets.apple {
            self.emit_apple_package(meta, args)?;
        }
        self.emit_gitignore()?;
        Ok(())
    }

    fn emit_web_entrypoint(&self) -> Result<()> {
        let entrypoint = self.package_dir.join("src/index.web.ts");
        self.write_entrypoint(
            &entrypoint,
            &self.source_root.join("browser/index.web.ts"),
            &self.source_root.join("common/public-types.ts"),
            "web",
        )
    }

    fn emit_mini_program_entrypoint(&self) -> Result<()> {
        let entrypoint = self.package_dir.join("src/index.mini-program.ts");
        self.write_entrypoint(
            &entrypoint,
            &self.source_root.join("browser/index.mini-program.ts"),
            &self.source_root.join("common/public-types.ts"),
            "mini-program",
        )
    }

    fn emit_node_entrypoint(&self) -> Result<()> {
        let entrypoint = self.package_dir.join("src/index.node.ts");
        self.write_entrypoint(
            &entrypoint,
            &self.source_root.join("node/index.ts"),
            &self.source_root.join("common/public-types.ts"),
            "node",
        )
    }

    fn emit_electron_entrypoint(&self) -> Result<()> {
        let entrypoint = self.package_dir.join("src/index.electron.ts");
        self.write_entrypoint(
            &entrypoint,
            &self.source_root.join("electron/renderer.ts"),
            &self.source_root.join("common/public-types.ts"),
            "electron",
        )
    }

    fn write_entrypoint(
        &self,
        entrypoint: &Utf8Path,
        runtime_entry: &Utf8Path,
        public_types: &Utf8Path,
        label: &str,
    ) -> Result<()> {
        let parent = entrypoint
            .parent()
            .with_context(|| format!("managed {label} entrypoint has no parent: {entrypoint}"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating managed {label} entrypoint dir {parent}"))?;
        let runtime_spec = module_specifier(parent, runtime_entry)?;
        let public_types_spec = module_specifier(parent, public_types)?;
        let source = format!(
            "// AUTOGENERATED by uniffi_bindgen_javascript (managed {label} entrypoint).\n\
             // Do not edit by hand.\n\
             \n\
             export * from \"{runtime_spec}\";\n\
             export type * from \"{public_types_spec}\";\n",
        );
        std::fs::write(entrypoint, source)
            .with_context(|| format!("writing managed {label} entrypoint {entrypoint}"))?;
        Ok(())
    }

    fn emit_gitignore(&self) -> Result<()> {
        std::fs::create_dir_all(&self.package_dir)
            .with_context(|| format!("creating managed package dir {}", self.package_dir))?;
        let gitignore = self.package_dir.join(".gitignore");
        let block = "\
# UniFFI generated build artifacts\n\
/artifacts/\n\
/target/\n\
node_modules/\n\
*.node\n\
*.wasm\n\
*.har\n\
*.hsp\n\
*.tgz\n\
*.xcframework\n\
*.so\n\
*.dylib\n\
*.dll\n\
*.a\n";
        if gitignore.exists() {
            let existing = std::fs::read_to_string(&gitignore)
                .with_context(|| format!("reading managed .gitignore {gitignore}"))?;
            if !existing.contains("# UniFFI generated build artifacts") {
                let separator = if existing.ends_with('\n') { "" } else { "\n" };
                std::fs::write(&gitignore, format!("{existing}{separator}\n{block}"))
                    .with_context(|| format!("updating managed .gitignore {gitignore}"))?;
            }
        } else {
            std::fs::write(&gitignore, block)
                .with_context(|| format!("writing managed .gitignore {gitignore}"))?;
        }
        Ok(())
    }

    fn emit_apple_package(&self, meta: &CargoPackageMetadata, args: &BuildArgs) -> Result<()> {
        let package_root = self.artifact_root.join("apple");
        std::fs::create_dir_all(&package_root)
            .with_context(|| format!("creating Apple artifact root {package_root}"))?;

        let package_name = apple_package_product_name(meta);
        let source_dir = package_root.join("Sources").join(&package_name);
        if source_dir.exists() {
            bail!(
                "fresh Apple package source path unexpectedly exists without its creation-time witness: {source_dir}"
            );
        }
        std::fs::create_dir_all(&source_dir)
            .with_context(|| format!("creating Apple package source dir {source_dir}"))?;

        let support_file = source_dir.join(format!("{package_name}.swift"));
        std::fs::write(
            &support_file,
            format!(
                "// AUTOGENERATED by uniffi_bindgen_javascript (managed Apple package support).\n\
                 // Do not edit by hand.\n\
                 \n\
                 public enum {package_name}Package {{}}\n"
            ),
        )
        .with_context(|| format!("writing Apple package support source {support_file}"))?;

        let generated_swift_root = self.source_root.join("swift");
        if generated_swift_root.exists() {
            copy_swift_sources(&generated_swift_root, &source_dir)?;
        }

        let package_swift = package_root.join("Package.swift");
        std::fs::write(&package_swift, apple_package_manifest_source(meta, args)?)
            .with_context(|| format!("writing Apple package manifest {package_swift}"))?;

        Ok(())
    }

    fn render_manifest(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
    ) -> Result<String> {
        self.render_manifest_with_harmony_root(targets, meta, args, None)
    }

    fn render_manifest_with_harmony_root(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
        harmony_source_root: Option<&Utf8Path>,
    ) -> Result<String> {
        self.render_manifest_with_read_roots(targets, meta, args, harmony_source_root, None)
    }

    fn render_manifest_with_read_roots(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
        harmony_source_root: Option<&Utf8Path>,
        artifact_read_root: Option<&Utf8Path>,
    ) -> Result<String> {
        let namespace = &meta.lib_target_name;
        let wasm_stem = format!("{}_wasm", rust_identifier(&meta.package_name));
        let node_env = format!("UNIFFI_{}_NAPI_PATH", namespace.to_ascii_uppercase());
        let harmony_package = args
            .ohos_package_name
            .clone()
            .unwrap_or_else(|| format!("{}-ohos", meta.package_name));
        let harmony_archive = if targets.harmony
            && !args.ohos_no_har
            && args.ohos_package_kind == super::ohos::PackageKind::Har
        {
            Some(harmony_archive_file_name(&harmony_package)?)
        } else {
            None
        };
        let harmony_hsp = targets.harmony
            && !args.ohos_no_har
            && args.ohos_package_kind == super::ohos::PackageKind::Hsp;
        let harmony_stem = if harmony_hsp {
            harmony_archive_stem(&harmony_package)?
        } else {
            String::new()
        };
        let default_harmony_source_root = self.artifact_root.join("harmony");
        let harmony_source_root = harmony_source_root.unwrap_or(&default_harmony_source_root);
        let harmony_metadata = if targets.harmony && !args.ohos_no_har {
            self.harmony_package_metadata(meta, args, &harmony_package, harmony_source_root)?
        } else {
            serde_json::Value::Null
        };
        let manifest = serde_json::json!({
            "schemaVersion": 3,
            "generator": "uniffi-bindgen-javascript",
            "namespace": namespace,
            "targets": self.manifest_targets(targets),
            "source": {
                "root": self.rel(&self.source_root)?,
                "common": if self.has_js(targets) { serde_json::Value::String(self.rel(&self.source_root.join("common"))?) } else { serde_json::Value::Null },
                "browser": if targets.wasm || targets.mini_program { serde_json::Value::String(self.rel(&self.source_root.join("browser"))?) } else { serde_json::Value::Null },
                "node": if targets.node { serde_json::Value::String(self.rel(&self.source_root.join("node"))?) } else { serde_json::Value::Null },
                "electron": if targets.electron { serde_json::Value::String(self.rel(&self.source_root.join("electron"))?) } else { serde_json::Value::Null },
                "harmony": if targets.harmony { serde_json::Value::String(self.rel(&self.source_root.join("harmony"))?) } else { serde_json::Value::Null },
                "swift": if targets.apple { serde_json::Value::String(self.rel(&self.source_root.join("swift"))?) } else { serde_json::Value::Null },
                "kotlin": if targets.android { serde_json::Value::String(self.rel(&self.source_root.join("kotlin"))?) } else { serde_json::Value::Null },
                "publicTypes": if self.has_js(targets) { serde_json::Value::String(self.rel(&self.source_root.join("common/public-types.ts"))?) } else { serde_json::Value::Null },
            },
            "entrypoints": {
                "web": if targets.wasm { serde_json::Value::String("src/index.web.ts".to_string()) } else { serde_json::Value::Null },
                "miniProgram": if targets.mini_program { serde_json::Value::String("src/index.mini-program.ts".to_string()) } else { serde_json::Value::Null },
                "node": if targets.node { serde_json::Value::String("src/index.node.ts".to_string()) } else { serde_json::Value::Null },
                "electron": if targets.electron { serde_json::Value::String("src/index.electron.ts".to_string()) } else { serde_json::Value::Null },
                "harmony": if targets.harmony {
                    serde_json::Value::String(self.rel(&self.artifact_root.join(if args.ohos_no_har { "harmony/dist/package-index.ets" } else { "harmony/package/Index.ets" }))?)
                } else { serde_json::Value::Null },
            },
            "artifacts": {
                "wasm": if targets.wasm {
                    serde_json::json!({
                        "glue": self.rel(&self.artifact_root.join(format!("browser/pkg/{wasm_stem}.js")))?,
                        "wasm": self.rel(&self.artifact_root.join(format!("browser/pkg/{wasm_stem}_bg.wasm")))?,
                        "dts": self.rel(&self.artifact_root.join(format!("browser/pkg/{wasm_stem}.d.ts")))?,
                    })
                } else { serde_json::Value::Null },
                "miniProgram": if targets.mini_program {
                    serde_json::json!({
                        "glue": self.rel(&self.artifact_root.join(format!("mini-program/{wasm_stem}.js")))?,
                        "wasm": self.rel(&self.artifact_root.join(format!("mini-program/{wasm_stem}_bg.wasm")))?,
                        "defaultWasmPath": mini_program_default_wasm_path(&wasm_stem),
                    })
                } else { serde_json::Value::Null },
                "node": if targets.node {
                    serde_json::json!({
                        "addon": self.addon_rel_from(artifact_read_root, "node", namespace)?,
                        "env": node_env,
                    })
                } else { serde_json::Value::Null },
                "electron": if targets.electron {
                    serde_json::json!({
                        "addon": self.addon_rel_from(artifact_read_root, "electron", namespace)?,
                        "env": node_env,
                    })
                } else { serde_json::Value::Null },
                "harmony": if targets.harmony {
                    serde_json::json!({
                        "kind": if args.ohos_no_har { "dist" } else { args.ohos_package_kind.as_str() },
                        "integrated": harmony_hsp && args.ohos_integrated_hsp,
                        "har": harmony_archive.as_ref().map(|archive| self.rel(&self.artifact_root.join("harmony").join(archive))).transpose()?,
                        "runtimeHsp": if harmony_hsp { serde_json::Value::String(self.rel(args.ohos_runtime_hsp_out.as_ref().context("managed HSP runtime output was not derived")?)?) } else { serde_json::Value::Null },
                        "interfaceHar": if harmony_hsp { serde_json::Value::String(self.rel(args.ohos_interface_har_out.as_ref().context("managed HSP Interface HAR output was not derived")?)?) } else { serde_json::Value::Null },
                        "tgz": if harmony_hsp { serde_json::Value::String(self.rel(args.ohos_tgz_out.as_ref().context("managed HSP tgz output was not derived")?)?) } else { serde_json::Value::Null },
                        "dist": self.rel(&self.artifact_root.join("harmony/dist"))?,
                        "facade": self.rel(&self.artifact_root.join("harmony/dist/native-facade.ets"))?,
                        "facadeContract": self.rel(&self.artifact_root.join("harmony/dist/harmony-facade-contract.json"))?,
                        "packageFacadeContract": if args.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/harmony-facade-contract.json"))?) },
                        "types": self.rel(&self.artifact_root.join("harmony/dist/index.d.ts"))?,
                        "package": if args.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package"))?) },
                        "moduleProject": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/module-project"))?) } else { serde_json::Value::Null },
                        "moduleSource": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/module-project/library"))?) } else { serde_json::Value::Null },
                        "usage": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony").join(format!("{harmony_stem}-HSP_USAGE.md")))?) } else { serde_json::Value::Null },
                        "packageMetadata": if args.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/oh-package.json5"))?) },
                        "moduleMetadata": if args.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/src/main/module.json5"))?) },
                        "buildProfile": if args.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/build-profile.json5"))?) },
                        "metadata": harmony_metadata,
                    })
                } else { serde_json::Value::Null },
                "apple": if targets.apple {
                    serde_json::json!({
                        "xcframework": self.rel(args.apple_xcframework_out.as_ref().expect("managed apple path derived"))?,
                        "package": self.rel(&self.artifact_root.join("apple"))?,
                        "product": apple_package_product_name(meta),
                    })
                } else { serde_json::Value::Null },
                "android": if targets.android {
                    serde_json::json!({
                        "jniLibs": self.rel(args.android_jni_libs_out.as_ref().expect("managed android path derived"))?,
                        "aar": args.android_aar_out.as_ref().map(|p| self.rel(p)).transpose()?,
                    })
                } else { serde_json::Value::Null },
            },
            "hostCrates": {
                "wasm": if targets.wasm || targets.mini_program { serde_json::Value::String(self.rel(&self.host_crates_root.join("wasm/Cargo.toml"))?) } else { serde_json::Value::Null },
                "napi": if targets.node || targets.electron { serde_json::Value::String(self.rel(&self.host_crates_root.join("napi/Cargo.toml"))?) } else { serde_json::Value::Null },
                "ohos": if targets.harmony { serde_json::Value::String(self.rel(&self.host_crates_root.join("ohos/Cargo.toml"))?) } else { serde_json::Value::Null },
            },
        });
        let manifest = self.merge_existing_manifest(manifest)?;
        let text = serde_json::to_string_pretty(&manifest)?;
        Ok(format!("{text}\n"))
    }

    fn harmony_package_metadata(
        &self,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
        package_name: &str,
        harmony_source_root: &Utf8Path,
    ) -> Result<serde_json::Value> {
        let package_path = harmony_source_root.join("package/oh-package.json5");
        let module_path = harmony_source_root.join("package/src/main/module.json5");
        let profile_path = harmony_source_root.join("package/build-profile.json5");
        if package_path.exists() && module_path.exists() && profile_path.exists() {
            return Ok(serde_json::json!({
                "package": read_generated_json5(&package_path)?,
                "module": read_generated_json5(&module_path)?["module"].clone(),
                "buildProfile": read_generated_json5(&profile_path)?,
            }));
        }

        super::ohos::validate_oh_package_name(package_name)?;
        let version = args
            .ohos_package_version
            .as_deref()
            .unwrap_or(&meta.package_version);
        super::ohos::validate_package_version(version)?;
        let module_name = args
            .ohos_module_name
            .clone()
            .unwrap_or(super::ohos::derive_module_name(package_name)?);
        super::ohos::validate_module_name(&module_name)?;
        let device_types = super::ohos::resolve_device_types(&args.ohos_device_types)?;

        let mut package = serde_json::Map::new();
        package.insert("name".into(), package_name.into());
        package.insert("version".into(), version.into());
        package.insert("main".into(), "Index.ets".into());
        if args.ohos_package_kind == super::ohos::PackageKind::Hsp {
            package.insert("packageType".into(), "InterfaceHar".into());
        }
        if let Some(description) = args.ohos_description.as_ref().or(meta.description.as_ref()) {
            package.insert("description".into(), description.clone().into());
        }
        if let Some(author) = args
            .ohos_author
            .as_ref()
            .or_else(|| meta.authors.iter().find(|author| !author.trim().is_empty()))
        {
            package.insert("author".into(), author.trim().to_string().into());
        }
        if let Some(license) = args.ohos_license.as_ref().or(meta.license.as_ref()) {
            if !license.trim().is_empty() {
                package.insert("license".into(), license.trim().to_string().into());
            }
        }
        if let Some(sdk_version) = &args.ohos_compatible_sdk_version {
            package.insert("compatibleSdkVersion".into(), sdk_version.clone().into());
            package.insert(
                "compatibleSdkType".into(),
                args.ohos_compatible_sdk_type
                    .clone()
                    .context("--ohos-compatible-sdk-version requires --ohos-compatible-sdk-type when generated package metadata is unavailable")?
                    .into(),
            );
        } else if args.ohos_compatible_sdk_type.is_some() {
            bail!("--ohos-compatible-sdk-type requires --ohos-compatible-sdk-version when no package was staged");
        }
        package.insert("obfuscated".into(), false.into());
        package.insert("artifactType".into(), "original".into());

        let mut build_profile = serde_json::json!({
            "apiType": "stageMode"
        });
        if args.ohos_package_kind == super::ohos::PackageKind::Hsp {
            let build_option = build_profile
                .as_object_mut()
                .expect("build profile is an object")
                .entry("buildOption")
                .or_insert_with(|| serde_json::json!({}));
            let build_option = build_option
                .as_object_mut()
                .expect("build option is an object");
            build_option.insert("generateSharedTgz".into(), true.into());
            build_option.insert(
                "nativeLib".into(),
                serde_json::json!({ "excludeSoFromInterfaceHar": true }),
            );
            if args.ohos_integrated_hsp {
                build_option.insert(
                    "arkOptions".into(),
                    serde_json::json!({ "integratedHsp": true }),
                );
            }
        }
        let mut module = serde_json::json!({
            "name": module_name,
            "type": if args.ohos_package_kind == super::ohos::PackageKind::Hsp { "shared" } else { "har" },
            "deviceTypes": device_types,
        });
        if args.ohos_package_kind == super::ohos::PackageKind::Hsp {
            module
                .as_object_mut()
                .expect("module metadata is an object")
                .insert("deliveryWithInstall".into(), true.into());
        }
        Ok(serde_json::json!({
            "package": serde_json::Value::Object(package),
            "module": module,
            "buildProfile": build_profile
        }))
    }

    fn merge_existing_manifest(
        &self,
        mut manifest: serde_json::Value,
    ) -> Result<serde_json::Value> {
        if !self.manifest_path.exists() {
            return Ok(manifest);
        }

        let existing_text = std::fs::read_to_string(&self.manifest_path)
            .with_context(|| format!("reading managed artifact manifest {}", self.manifest_path))?;
        let existing: serde_json::Value = serde_json::from_str(&existing_text)
            .with_context(|| format!("parsing managed artifact manifest {}", self.manifest_path))?;

        let compatible = existing.get("schemaVersion") == manifest.get("schemaVersion")
            && existing.get("generator") == manifest.get("generator")
            && existing.get("namespace") == manifest.get("namespace");
        if !compatible {
            return Ok(manifest);
        }

        let current_targets = manifest
            .get("targets")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        merge_manifest_targets(&mut manifest, &existing);
        for key in ["source", "entrypoints", "artifacts", "hostCrates"] {
            merge_manifest_object_section(&mut manifest, &existing, key, &current_targets);
        }
        Ok(manifest)
    }

    fn has_js(&self, targets: &ExpandedTargets) -> bool {
        targets.wasm || targets.mini_program || targets.node || targets.electron || targets.harmony
    }

    fn manifest_targets(&self, targets: &ExpandedTargets) -> Vec<&'static str> {
        let mut out = Vec::new();
        if targets.wasm {
            out.push("wasm");
        }
        if targets.mini_program {
            out.push("mini-program");
        }
        if targets.node {
            out.push("node");
        }
        if targets.electron {
            out.push("electron");
        }
        if targets.harmony {
            out.push("harmony");
        }
        if targets.apple {
            out.push("apple");
        }
        if targets.android {
            out.push("android");
        }
        out
    }

    fn rel(&self, path: &Utf8Path) -> Result<String> {
        let rel = relative_path_from_dir(&self.package_dir, path)
            .to_string()
            .replace('\\', "/");
        if rel.starts_with('/') || rel.contains(':') {
            bail!("managed manifest path must be relative: {rel}");
        }
        Ok(if rel.is_empty() { ".".to_string() } else { rel })
    }

    fn addon_rel_from(
        &self,
        artifact_read_root: Option<&Utf8Path>,
        subdir: &str,
        fallback_stem: &str,
    ) -> Result<String> {
        let public_dir = self.artifact_root.join(subdir);
        let read_dir = artifact_read_root
            .unwrap_or(&self.artifact_root)
            .join(subdir);
        if read_dir.exists() {
            let mut nodes = Vec::new();
            for entry in
                std::fs::read_dir(&read_dir).with_context(|| format!("reading {read_dir}"))?
            {
                let entry = entry?;
                let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|p| {
                    anyhow::anyhow!("managed addon artifact path is not utf8: {}", p.display())
                })?;
                if path.extension() == Some("node") {
                    nodes.push(path);
                }
            }
            nodes.sort();
            if let [path] = nodes.as_slice() {
                let name = path
                    .file_name()
                    .context("managed addon candidate has no file name")?;
                return self.rel(&public_dir.join(name));
            }
        }
        self.rel(&public_dir.join(format!("{fallback_stem}.node")))
    }
}

#[cfg(test)]
pub(in crate::cli) fn require_real_directory(path: &Utf8Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("{label} does not exist: {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} must be a real directory: {path}");
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cli) fn ensure_tree_has_no_native_artifacts(root: &Utf8Path) -> Result<()> {
    for entry in
        std::fs::read_dir(root).with_context(|| format!("checking managed no-HAR dist {root}"))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!("managed no-HAR dist path is not utf8: {}", path.display())
        })?;
        if file_type.is_symlink() {
            bail!("managed no-HAR dist contains a symlink: {path}");
        }
        if file_type.is_dir() {
            ensure_tree_has_no_native_artifacts(&path)?;
        } else if file_type.is_file() && matches!(path.extension(), Some("so") | Some("a")) {
            bail!("managed --ohos-skip-libs dist still contains a native artifact: {path}");
        }
    }
    Ok(())
}

pub(in crate::cli) fn write_file_atomically(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("managed output path has no parent: {path}"))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("creating managed output directory {parent}"))?;
    let mut temp = tempfile::Builder::new()
        .prefix(".uniffi-managed-")
        .tempfile_in(parent)
        .with_context(|| format!("creating temporary managed output beside {path}"))?;
    temp.write_all(bytes)
        .with_context(|| format!("writing temporary managed output for {path}"))?;
    temp.flush()?;
    temp.as_file().sync_all()?;
    temp.persist(path.as_std_path())
        .map_err(|error| error.error)
        .with_context(|| format!("atomically publishing managed output {path}"))?;
    if let Ok(parent_file) = std::fs::File::open(parent) {
        let _ = parent_file.sync_all();
    }
    Ok(())
}

#[cfg(test)]
pub(in crate::cli) fn restore_file_atomically(
    path: &Utf8Path,
    previous: Option<&[u8]>,
) -> Result<()> {
    if let Some(previous) = previous {
        write_file_atomically(path, previous)
    } else {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("removing newly written file {path}")),
        }
    }
}

fn harmony_archive_file_name(package_name: &str) -> Result<String> {
    Ok(format!("{}.har", harmony_archive_stem(package_name)?))
}

pub(in crate::cli) fn harmony_archive_stem(package_name: &str) -> Result<String> {
    super::ohos::validate_oh_package_name(package_name)?;
    Ok(package_name.trim_start_matches('@').replace('/', "-"))
}

pub(in crate::cli) fn read_generated_json5(path: &Utf8Path) -> Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading generated Harmony metadata {path}"))?;
    serde_json::from_str(&text)
        .with_context(|| format!("parsing generated Harmony metadata {path}"))
}

fn merge_manifest_targets(manifest: &mut serde_json::Value, existing: &serde_json::Value) {
    let mut present = std::collections::BTreeSet::new();
    for value in existing
        .get("targets")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .chain(
            manifest
                .get("targets")
                .and_then(|value| value.as_array())
                .into_iter()
                .flatten(),
        )
    {
        if let Some(target) = value.as_str() {
            present.insert(target.to_string());
        }
    }

    let targets = [
        "wasm",
        "mini-program",
        "node",
        "electron",
        "harmony",
        "apple",
        "android",
    ]
    .into_iter()
    .filter(|target| present.contains(*target))
    .map(serde_json::Value::from)
    .collect();
    manifest["targets"] = serde_json::Value::Array(targets);
}

fn merge_manifest_object_section(
    manifest: &mut serde_json::Value,
    existing: &serde_json::Value,
    key: &str,
    current_targets: &BTreeSet<String>,
) {
    let Some(current) = manifest
        .get_mut(key)
        .and_then(|value| value.as_object_mut())
    else {
        return;
    };
    let Some(previous) = existing.get(key).and_then(|value| value.as_object()) else {
        return;
    };

    for (field, previous_value) in previous {
        let current_target = match (key, field.as_str()) {
            ("source" | "entrypoints" | "artifacts", "miniProgram") => "mini-program",
            ("source" | "entrypoints" | "artifacts", field) => field,
            ("hostCrates", "wasm")
                if current_targets.contains("wasm") || current_targets.contains("mini-program") =>
            {
                continue
            }
            ("hostCrates", "napi")
                if current_targets.contains("node") || current_targets.contains("electron") =>
            {
                continue
            }
            ("hostCrates", "ohos") if current_targets.contains("harmony") => continue,
            _ => "",
        };
        if !current_target.is_empty() && current_targets.contains(current_target) {
            continue;
        }
        if current
            .get(field)
            .map(|value| !value.is_null())
            .unwrap_or(false)
        {
            continue;
        }
        current.insert(field.clone(), previous_value.clone());
    }
}

pub(crate) fn run(args: ArtifactsArgs) -> Result<()> {
    match args.command {
        ArtifactsCommands::Build(args) => build(args),
    }
}

fn ensure_explicit_generated_hsp_outputs(
    args: &mut BuildArgs,
) -> Result<super::artifact_transaction::HspOutputPaths> {
    let meta = cargo_package_metadata(&args.manifest_path)?;
    let generated_host_package = format!("{}-ohos", meta.package_name);
    let host_root = resolve_cwd_path(&args.host_crates_dir())?;
    let dist = args
        .ohos_dist_dir
        .clone()
        .or_else(|| {
            args.artifact_dir
                .as_ref()
                .map(|root| root.join("ohos/dist"))
        })
        .unwrap_or_else(|| host_root.join("ohos/dist"));
    let logical_artifact_root = dist.parent().unwrap_or(dist.as_path()).to_path_buf();
    let requested_runtime = args.ohos_runtime_hsp_out.clone();
    let requested_interface = args.ohos_interface_har_out.clone();
    let requested_tgz = args.ohos_tgz_out.clone();
    let outputs =
        super::ohos::planned_generated_hsp_outputs(super::ohos::GeneratedHspOutputPreflight {
            dist_dir: &dist,
            generated_host_package_name: &generated_host_package,
            package_name: args.ohos_package_name.as_deref(),
            runtime_hsp_out: args.ohos_runtime_hsp_out.as_deref(),
            interface_har_out: args.ohos_interface_har_out.as_deref(),
            tgz_out: args.ohos_tgz_out.as_deref(),
        })?;
    // Keep the logical public spelling in BuildArgs so managed manifest paths
    // stay relative to a package reached through a symlinked ancestor (for
    // example macOS `/var` -> `/private/var`). The immutable transaction plan
    // returned from this function remains fully canonicalized for locking,
    // alias checks, and publication.
    args.ohos_dist_dir = Some(dist);
    args.ohos_runtime_hsp_out = Some(requested_runtime.unwrap_or_else(|| {
        logical_artifact_root.join(
            outputs
                .runtime_hsp
                .file_name()
                .expect("planned HSP runtime output has a file name"),
        )
    }));
    args.ohos_interface_har_out = Some(requested_interface.unwrap_or_else(|| {
        logical_artifact_root.join(
            outputs
                .interface_har
                .file_name()
                .expect("planned HSP Interface HAR output has a file name"),
        )
    }));
    args.ohos_tgz_out = Some(requested_tgz.unwrap_or_else(|| {
        logical_artifact_root.join(
            outputs
                .tgz
                .file_name()
                .expect("planned HSP tgz output has a file name"),
        )
    }));
    Ok(outputs)
}

fn invocation_output_specs(
    args: &BuildArgs,
    targets: &ExpandedTargets,
    layout: Option<&ManagedLayout>,
) -> Result<Vec<super::artifact_transaction::InvocationOutputSpec>> {
    let mut outputs = Vec::new();
    let mut add = |label: &str, path: Utf8PathBuf, is_directory: bool| {
        outputs.push(super::artifact_transaction::InvocationOutputSpec {
            label: label.to_string(),
            path,
            is_directory,
        });
    };
    let out_dir = resolve_cwd_path(&args.out_dir()?)?;
    add("generated source root", out_dir.clone(), true);
    let host_root = resolve_cwd_path(&args.host_crates_dir())?;
    let mut add_host = |kind: &str, build_script: bool, facade: bool| {
        let root = host_root.join(kind);
        add(
            &format!("{kind} host Cargo manifest"),
            root.join("Cargo.toml"),
            false,
        );
        add(
            &format!("{kind} host Cargo lock"),
            root.join("Cargo.lock"),
            false,
        );
        if build_script {
            add(
                &format!("{kind} host build script"),
                root.join("build.rs"),
                false,
            );
        }
        if facade {
            add(
                "OHOS facade bundle",
                root.join("uniffi-ohos-facade-bundle.json"),
                false,
            );
        }
        add(&format!("{kind} host source"), root.join("src"), true);
    };
    if targets.wasm || targets.mini_program {
        add_host("wasm", false, false);
    }
    if targets.node || targets.electron {
        add_host("napi", true, false);
    }
    if targets.harmony {
        add_host("ohos", true, true);
    }

    let covered_by_out = |path: &Utf8Path| -> Result<bool> {
        let path = canonicalize_invocation_output(path)?;
        Ok(path == out_dir || path.starts_with(&out_dir))
    };
    if targets.wasm {
        let path = args.wasm_bindgen_out_dir()?;
        if !covered_by_out(&path)? {
            add("wasm-bindgen package", path, true);
        }
    }
    if targets.mini_program {
        let path = args.mini_program_out_dir()?;
        if !covered_by_out(&path)? {
            add("Mini Program artifact", path, true);
        }
    }
    if let Some(artifact_root) = &args.artifact_dir {
        if targets.node {
            let path = artifact_root.join("node");
            if !covered_by_out(&path)? {
                add("Node addon artifact", path, true);
            }
        }
        if targets.electron {
            let path = artifact_root.join("electron");
            if !covered_by_out(&path)? {
                add("Electron addon artifact", path, true);
            }
        }
    }

    if let Some(layout) = layout {
        if targets.apple {
            add(
                "managed Apple artifact",
                layout.artifact_root.join("apple"),
                true,
            );
        }
        if targets.android {
            add(
                "managed Android artifact",
                layout.artifact_root.join("android"),
                true,
            );
        }
        for (enabled, name) in [
            (targets.wasm, "index.web.ts"),
            (targets.mini_program, "index.mini-program.ts"),
            (targets.node, "index.node.ts"),
            (targets.electron, "index.electron.ts"),
        ] {
            if enabled {
                add(
                    &format!("managed entrypoint {name}"),
                    layout.package_dir.join("src").join(name),
                    false,
                );
            }
        }
        add(
            "managed gitignore",
            layout.package_dir.join(".gitignore"),
            false,
        );
        add(
            "managed artifact manifest",
            layout.manifest_path.clone(),
            false,
        );
    } else {
        if targets.apple {
            add(
                "Apple XCFramework",
                args.apple_xcframework_out
                    .clone()
                    .context("--target apple requires --apple-xcframework-out")?,
                true,
            );
            if let Some(path) = &args.apple_swift_out {
                if !covered_by_out(path)? {
                    add("Apple Swift output", path.clone(), true);
                }
            }
        }
        if targets.android {
            add(
                "Android jniLibs",
                args.android_jni_libs_out
                    .clone()
                    .context("--target android requires --android-jni-libs-out")?,
                true,
            );
            if let Some(path) = &args.android_kotlin_out {
                if !covered_by_out(path)? {
                    add("Android Kotlin output", path.clone(), true);
                }
            }
            if let Some(path) = &args.android_aar_out {
                add("Android AAR", path.clone(), false);
            }
        }
    }
    Ok(outputs)
}

fn mirror_build_args(
    public: &BuildArgs,
    mirror: &InvocationMirror,
    targets: &ExpandedTargets,
) -> Result<BuildArgs> {
    let mut private = public.clone();
    let public_host = resolve_cwd_path(&public.host_crates_dir())?;
    private.logical_host_crates_dir = None;
    private.out_dir = Some(mirror.map(&public.out_dir()?)?);
    private.host_crates_dir = Some(mirror.map(&public_host)?);
    private.artifact_dir = public
        .artifact_dir
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    private.wasm_bindgen_out_dir = public
        .wasm_bindgen_out_dir
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    private.napi_target_dir =
        (targets.node || targets.electron).then(|| mirror.build_root.join("napi"));
    let consumes_wasm = targets.wasm || targets.mini_program;
    private.wasm_core_target_dir = consumes_wasm.then(|| mirror.build_root.join("wasm/core"));
    private.wasm_target_dir = consumes_wasm.then(|| mirror.build_root.join("wasm/host"));
    private.ohos_target_dir = targets.harmony.then(|| mirror.build_root.join("ohos"));
    private.apple_xcframework_out = public
        .apple_xcframework_out
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    private.apple_swift_out = public
        .apple_swift_out
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    private.android_jni_libs_out = public
        .android_jni_libs_out
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    private.android_kotlin_out = public
        .android_kotlin_out
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    private.android_aar_out = public
        .android_aar_out
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    private.package_dir = public
        .package_dir
        .as_deref()
        .map(|path| mirror.map(path))
        .transpose()?;
    // HSP destinations intentionally remain the public immutable plan. The
    // OHOS builder returns a deferred candidate and does not mutate them.
    private.ohos_dist_dir = public.ohos_dist_dir.clone();
    private.ohos_runtime_hsp_out = public.ohos_runtime_hsp_out.clone();
    private.ohos_interface_har_out = public.ohos_interface_har_out.clone();
    private.ohos_tgz_out = public.ohos_tgz_out.clone();
    private.managed_layout = false;
    Ok(private)
}

fn private_output_sources(
    public: &BuildArgs,
    private: &BuildArgs,
    destinations: &[super::artifact_transaction::InvocationOutputSpec],
) -> Result<Vec<Utf8PathBuf>> {
    let mut roots = Vec::<(Utf8PathBuf, Utf8PathBuf)>::new();
    let mut add = |public: Option<Utf8PathBuf>, private: Option<Utf8PathBuf>| -> Result<()> {
        if let (Some(public), Some(private)) = (public, private) {
            roots.push((canonicalize_invocation_output(&public)?, private));
        }
        Ok(())
    };
    add(Some(public.out_dir()?), Some(private.out_dir()?))?;
    add(
        Some(resolve_cwd_path(&public.host_crates_dir())?),
        Some(resolve_cwd_path(&private.host_crates_dir())?),
    )?;
    add(public.artifact_dir.clone(), private.artifact_dir.clone())?;
    add(
        public.wasm_bindgen_out_dir.clone(),
        private.wasm_bindgen_out_dir.clone(),
    )?;
    add(
        public.apple_xcframework_out.clone(),
        private.apple_xcframework_out.clone(),
    )?;
    add(
        public.apple_swift_out.clone(),
        private.apple_swift_out.clone(),
    )?;
    add(
        public.android_jni_libs_out.clone(),
        private.android_jni_libs_out.clone(),
    )?;
    add(
        public.android_kotlin_out.clone(),
        private.android_kotlin_out.clone(),
    )?;
    add(
        public.android_aar_out.clone(),
        private.android_aar_out.clone(),
    )?;
    add(public.package_dir.clone(), private.package_dir.clone())?;
    roots.sort_by(|(left, _), (right, _)| {
        right
            .components()
            .count()
            .cmp(&left.components().count())
            .then_with(|| right.cmp(left))
    });

    let mut sources = Vec::with_capacity(destinations.len());
    for destination in destinations {
        let (public_root, private_root) = roots
            .iter()
            .find(|(public_root, _)| destination.path.starts_with(public_root))
            .with_context(|| {
                format!(
                    "complete artifact destination has no private-root mapping: {} ({})",
                    destination.path, destination.label
                )
            })?;
        sources.push(private_root.join(destination.path.strip_prefix(public_root)?));
    }
    for (index, left) in sources.iter().enumerate() {
        let left_compare = if cfg!(any(target_os = "macos", target_os = "windows")) {
            Utf8PathBuf::from(left.as_str().to_lowercase())
        } else {
            left.clone()
        };
        for right in sources.iter().skip(index + 1) {
            let right_compare = if cfg!(any(target_os = "macos", target_os = "windows")) {
                Utf8PathBuf::from(right.as_str().to_lowercase())
            } else {
                right.clone()
            };
            if left_compare == right_compare
                || left_compare.starts_with(&right_compare)
                || right_compare.starts_with(&left_compare)
            {
                bail!("private artifact source mapping aliases or overlaps: {left} vs {right}");
            }
        }
    }
    Ok(sources)
}

fn rebase_private_javascript_host_crates(
    public: &BuildArgs,
    private: &BuildArgs,
    targets: &ExpandedTargets,
) -> Result<()> {
    let mut flavors = Vec::new();
    if targets.wasm || targets.mini_program {
        flavors.push(FlavorTarget::Wasm);
    }
    if targets.node {
        flavors.push(FlavorTarget::Napi);
    }
    if targets.electron {
        flavors.push(FlavorTarget::Electron);
    }
    if targets.harmony {
        flavors.push(FlavorTarget::Harmony);
    }
    if flavors.is_empty() {
        return Ok(());
    }
    let meta = cargo_package_metadata(&public.manifest_path)?;
    let generation_source = private
        .source
        .clone()
        .or_else(|| private.library_path.clone())
        .unwrap_or_else(|| {
            if targets.wasm || targets.mini_program {
                private
                    .wasm_core_target_dir
                    .as_ref()
                    .map(|target| host_cdylib_path_in(&meta, target, private.release))
                    .unwrap_or_else(|| host_cdylib_path(&meta, private.release))
            } else {
                host_cdylib_path(&meta, private.release)
            }
        });
    if !generation_source.exists() {
        bail!("private artifact rebase source does not exist: {generation_source}");
    }
    generate_js(
        &private.manifest_path,
        generation_source,
        private.out_dir()?,
        private.config.clone(),
        private.crate_name.clone(),
        private.metadata_no_deps,
        private.no_format,
        Some(HostCrateOptions {
            manifest_path: private.manifest_path.clone(),
            host_crates_dir: private.host_crates_dir(),
            // Relative Cargo paths must be calculated from the canonical
            // public filesystem depth. On macOS `/var` publishes under
            // `/private/var`; using the logical alias here leaves every
            // external dependency one ancestor short after the root swap.
            logical_host_crates_dir: Some(canonicalize_invocation_output(
                &public.host_crates_dir(),
            )?),
            logical_out_dir: Some(canonicalize_invocation_output(&public.out_dir()?)?),
            ohos_rs_dir: None,
        }),
        flavors,
        private.artifact_dir.clone(),
    )
    .context("rebasing invocation-private JavaScript host manifests to public paths")?;
    if targets.mini_program {
        let wasm_stem = format!("{}_wasm", rust_identifier(&meta.package_name));
        rebase_mini_program_auto_entrypoint(
            &private.out_dir()?,
            &canonicalize_invocation_output(&public.out_dir()?)?,
            &canonicalize_invocation_output(&public.mini_program_out_dir()?)?,
            &wasm_stem,
        )
        .context("rebasing invocation-private Mini Program entrypoint to public paths")?;
    }
    Ok(())
}

fn build_multi_target_hsp(
    mut public_args: BuildArgs,
    targets: ExpandedTargets,
    public_layout: Option<ManagedLayout>,
) -> Result<()> {
    super::ohos::preflight_hsp_arches(&public_args.ohos_arch)
        .context("validating Harmony HSP architectures before publication planning")?;
    let hsp_outputs = ensure_explicit_generated_hsp_outputs(&mut public_args)?;
    let specs = invocation_output_specs(&public_args, &targets, public_layout.as_ref())?;
    let mut generic_plan = super::artifact_transaction::GenericPublicationPlan::new(
        specs,
        std::slice::from_ref(&hsp_outputs),
        publication_hooks(),
    )
    .context("planning complete non-Harmony artifact publication")?;
    // Declared before every candidate/publication object so the complete union
    // lock is released only after all RAII rollback/cleanup objects have run.
    let _union_locks = generic_plan
        .take_output_locks()
        .context("complete direct invocation did not retain its union lock")?;
    let mut mirror = InvocationMirror::new()?;
    let result = (|| -> Result<()> {
        let mut private_args = mirror_build_args(&public_args, &mirror, &targets)?;
        private_args.invocation_output_lock_held = true;
        let private_layout = public_layout
            .as_ref()
            .map(|layout| layout.mirrored(&mirror))
            .transpose()?;

        if let (Some(public), Some(private)) = (public_layout.as_ref(), private_layout.as_ref()) {
            let public_gitignore = public.package_dir.join(".gitignore");
            if let Ok(metadata) = std::fs::symlink_metadata(&public_gitignore) {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    bail!(
                        "managed .gitignore must be a regular non-symlink file: {public_gitignore}"
                    );
                }
                let private_gitignore = private.package_dir.join(".gitignore");
                std::fs::create_dir_all(
                    private_gitignore
                        .parent()
                        .context("private managed .gitignore has no parent")?,
                )?;
                std::fs::copy(&public_gitignore, &private_gitignore).with_context(|| {
                    format!("seeding private managed .gitignore from {public_gitignore}")
                })?;
            }
        }

        let prepared_hsp = build_ohos_deferred(private_args.to_ohos_args()?)
            .context("building deferred Harmony HSP participant")?;
        if targets.apple {
            build_apple(&private_args).context("building private Apple artifact target")?;
        }
        if targets.android {
            build_android(&private_args).context("building private Android artifact target")?;
        }
        if targets.wasm || targets.mini_program {
            build_wasm(private_args.to_wasm_args()?)
                .context("building private wasm artifact target")?;
        }
        if targets.mini_program {
            let meta = cargo_package_metadata(&private_args.manifest_path)?;
            let wasm_stem = format!("{}_wasm", rust_identifier(&meta.package_name));
            emit_mini_program_wasm_runtime(
                &private_args.out_dir()?,
                &private_args.wasm_bindgen_out_dir()?,
                &private_args.mini_program_out_dir()?,
                &wasm_stem,
            )
            .context("emitting private Mini Program wasm runtime")?;
        }
        let mut napi_flavors = Vec::new();
        if targets.node {
            napi_flavors.push(NapiBuildFlavorArg::Napi);
        }
        if targets.electron {
            napi_flavors.push(NapiBuildFlavorArg::Electron);
        }
        if !napi_flavors.is_empty() {
            build_napi(private_args.to_napi_args(napi_flavors)?)
                .context("building private N-API artifact target")?;
        }
        rebase_private_javascript_host_crates(&public_args, &private_args, &targets)?;

        if let (Some(public), Some(private)) = (public_layout.as_ref(), private_layout.as_ref()) {
            let meta = cargo_package_metadata(&public_args.manifest_path)?;
            private
                .emit_supporting_files(&targets, &meta, &private_args)
                .context("emitting invocation-private managed support files")?;
            let manifest = public.render_manifest_with_read_roots(
                &targets,
                &meta,
                &public_args,
                Some(&private.artifact_root.join("harmony")),
                Some(&private.artifact_root),
            )?;
            write_file_atomically(&private.manifest_path, manifest.as_bytes())
                .context("writing invocation-private managed artifact manifest")?;
        }

        let generic_sources =
            private_output_sources(&public_args, &private_args, generic_plan.destinations())?;
        // Generation is complete. Seal the exact private tree before staging or
        // publishing reads it so later replacement/ABA cannot be legitimized by a
        // cleanup-time recapture.
        mirror.seal()?;
        let mut hsp_publication = generic_plan
            .stage_hsp(prepared_hsp)
            .context("staging deferred HSP publication")?;
        let mut generic_publication = generic_plan
            .stage(&generic_sources)
            .context("staging non-Harmony artifact publication")?;

        if let Err(error) =
            generic_publication.register_complete_candidates(&hsp_publication.next_entries())
        {
            let hsp_candidates = if generic_publication.requires_control_preservation() {
                Err(anyhow::anyhow!(
                    "HSP candidates are preserved because candidate-record durability is uncertain"
                ))
            } else {
                hsp_publication.cleanup_unpublished_candidates()
            };
            let controls = generic_publication.rollback();
            return match (hsp_candidates, controls) {
                (Ok(()), Ok(())) => {
                    Err(error).context("registering complete direct candidate set")
                }
                (hsp, controls) => Err(anyhow::anyhow!(
                    "registering complete direct candidate set failed: {error:#}; HSP candidate cleanup={hsp:?}; generic/control cleanup={controls:?}"
                )),
            };
        }
        if let Err(error) = generic_publication.publish_hsp(&mut hsp_publication) {
            let controls = generic_publication.rollback();
            return match controls {
                Ok(()) => Err(error),
                Err(controls) => Err(anyhow::anyhow!(
                    "publishing HSP participant failed: {error:#}; direct control cleanup also failed: {controls:#}"
                )),
            };
        }
        if let Err(error) = generic_publication.publish() {
            let hsp_recovery = if generic_publication.complete_owner_recovery_finished() {
                hsp_publication.mark_recovered_by_complete_owner();
                Ok(())
            } else {
                Err(anyhow::anyhow!(
                    "complete-owner durable recovery did not finish; HSP state is preserved"
                ))
            };
            let controls = generic_publication.abort_controls_after_rollback();
            return match (hsp_recovery, controls) {
                (Ok(()), Ok(())) => {
                    Err(error).context("publishing non-Harmony artifact participant")
                }
                (recovery, controls) => Err(anyhow::anyhow!(
                    "non-Harmony artifact publication failed: {error:#}; HSP complete-owner recovery={recovery:?}; control cleanup={controls:?}"
                )),
            };
        }

        // The single complete owner record is published last and is the only
        // invocation commit point.
        match generic_publication.commit_record(&hsp_publication.next_entries()) {
            Err(error) => {
                let generic_rollback = generic_publication.rollback_outputs_only();
                let hsp_recovery = if generic_rollback.is_ok()
                    && generic_publication.complete_owner_recovery_finished()
                {
                    hsp_publication.mark_recovered_by_complete_owner();
                    Ok(())
                } else {
                    Err(anyhow::anyhow!(
                        "complete-owner durable recovery did not finish; HSP state is preserved"
                    ))
                };
                let controls = generic_publication.abort_controls_after_rollback();
                return match (generic_rollback, hsp_recovery, controls) {
                    (Ok(()), Ok(()), Ok(())) => Err(error),
                    (generic, hsp, controls) => Err(anyhow::anyhow!(
                        "direct final owner failed before commit: {error:#}; complete-owner recovery={generic:?}; HSP recovery state={hsp:?}; control cleanup={controls:?}"
                    )),
                };
            }
            Ok(super::artifact_transaction::DirectCommitOutcome::Verified) => {
                generic_publication.finalize_hsp(hsp_publication)?;
                generic_publication.finalize()?;
            }
            Ok(super::artifact_transaction::DirectCommitOutcome::CommittedNeedsAudit(error)) => {
                hsp_publication.preserve_previous_backups();
                let _ = hsp_publication.finalize();
                let _ = generic_publication.finalize();
                return Err(error);
            }
        }
        Ok(())
    })();
    // Explicitly finish while `_union_locks` is still in scope. This both
    // surfaces cleanup violations and guarantees the lock outlives removal of
    // the invocation-private mirror/build tree.
    mirror.finish(result)
}

fn validate_managed_manifest_candidate(layout: &ManagedLayout, cargo_bin: &str) -> Result<()> {
    let bytes = super::artifact_transaction::read_verified_regular_file_bounded(
        &layout.manifest_path,
        16 * 1024 * 1024,
        "managed artifact manifest candidate",
    )?;
    let manifest: serde_json::Value = serde_json::from_slice(&bytes)?;
    if manifest["schemaVersion"] != 3 || manifest["generator"] != "uniffi-bindgen-javascript" {
        bail!("managed artifact manifest candidate has an unsupported schema");
    }
    let targets = manifest["targets"]
        .as_array()
        .context("managed manifest targets must be an array")?;
    let mut unique = BTreeSet::new();
    for target in targets {
        let target = target
            .as_str()
            .context("managed manifest target must be a string")?;
        if !matches!(
            target,
            "wasm" | "mini-program" | "node" | "electron" | "harmony" | "apple" | "android"
        ) || !unique.insert(target)
        {
            bail!("managed manifest has an invalid or duplicate target `{target}`");
        }
    }
    let canonical_root = layout.package_dir.canonicalize_utf8()?;
    let validate_path = |pointer: &str, directory: bool| -> Result<Option<Utf8PathBuf>> {
        let value = manifest
            .pointer(pointer)
            .unwrap_or(&serde_json::Value::Null);
        if value.is_null() {
            return Ok(None);
        }
        let relative = value
            .as_str()
            .with_context(|| format!("managed manifest `{pointer}` must be a path string"))?;
        let relative = Utf8Path::new(relative);
        if relative.is_absolute()
            || relative
                .components()
                .any(|component| matches!(component.as_str(), "" | "." | ".."))
        {
            bail!("managed manifest `{pointer}` has an unsafe path `{relative}`");
        }
        let path = layout.package_dir.join(relative);
        let metadata = std::fs::symlink_metadata(&path)
            .with_context(|| format!("managed manifest `{pointer}` path is missing: {path}"))?;
        if metadata.file_type().is_symlink()
            || (directory && !metadata.is_dir())
            || (!directory && !metadata.is_file())
        {
            bail!("managed manifest `{pointer}` has the wrong filesystem type: {path}");
        }
        let canonical = path.canonicalize_utf8()?;
        if !canonical.starts_with(&canonical_root) {
            bail!("managed manifest `{pointer}` escapes the package root: {path}");
        }
        Ok(Some(path))
    };
    for pointer in [
        "/source/root",
        "/source/common",
        "/source/browser",
        "/source/node",
        "/source/electron",
        "/source/harmony",
        "/source/swift",
        "/source/kotlin",
        "/artifacts/harmony/dist",
        "/artifacts/harmony/package",
        "/artifacts/harmony/moduleProject",
        "/artifacts/harmony/moduleSource",
        "/artifacts/apple/xcframework",
        "/artifacts/apple/package",
        "/artifacts/android/jniLibs",
    ] {
        validate_path(pointer, true)?;
    }
    for pointer in [
        "/source/publicTypes",
        "/entrypoints/web",
        "/entrypoints/miniProgram",
        "/entrypoints/node",
        "/entrypoints/electron",
        "/entrypoints/harmony",
        "/artifacts/wasm/glue",
        "/artifacts/wasm/wasm",
        "/artifacts/wasm/dts",
        "/artifacts/miniProgram/glue",
        "/artifacts/miniProgram/wasm",
        "/artifacts/node/addon",
        "/artifacts/electron/addon",
        "/artifacts/harmony/har",
        "/artifacts/harmony/runtimeHsp",
        "/artifacts/harmony/interfaceHar",
        "/artifacts/harmony/tgz",
        "/artifacts/harmony/facade",
        "/artifacts/harmony/facadeContract",
        "/artifacts/harmony/packageFacadeContract",
        "/artifacts/harmony/types",
        "/artifacts/harmony/usage",
        "/artifacts/harmony/packageMetadata",
        "/artifacts/harmony/moduleMetadata",
        "/artifacts/harmony/buildProfile",
        "/artifacts/android/aar",
    ] {
        validate_path(pointer, false)?;
    }
    for pointer in ["/hostCrates/wasm", "/hostCrates/napi", "/hostCrates/ohos"] {
        if let Some(manifest_path) = validate_path(pointer, false)? {
            let output = Command::new(cargo_bin)
                .args([
                    "metadata",
                    "--format-version=1",
                    "--no-deps",
                    "--manifest-path",
                ])
                .arg(&manifest_path)
                .output()
                .with_context(|| {
                    format!("running Cargo metadata for managed host {manifest_path}")
                })?;
            if !output.status.success() {
                bail!(
                    "managed host Cargo metadata failed for {manifest_path}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
    Ok(())
}

fn build_private_target_set(args: &BuildArgs, targets: &ExpandedTargets) -> Result<()> {
    let hsp_first = targets.harmony && args.ohos_package_kind == super::ohos::PackageKind::Hsp;
    if hsp_first {
        build_ohos_deferred(args.to_ohos_args()?)
            .context("building private managed Harmony HSP candidate")?
            .commit_private()
            .context("materializing private managed Harmony HSP candidate")?;
    }
    if targets.apple {
        build_apple(args).context("building private managed Apple target")?;
    }
    if targets.android {
        build_android(args).context("building private managed Android target")?;
    }
    if targets.wasm || targets.mini_program {
        build_wasm(args.to_wasm_args()?).context("building private managed wasm target")?;
    }
    if targets.mini_program {
        let meta = cargo_package_metadata(&args.manifest_path)?;
        let wasm_stem = format!("{}_wasm", rust_identifier(&meta.package_name));
        emit_mini_program_wasm_runtime(
            &args.out_dir()?,
            &args.wasm_bindgen_out_dir()?,
            &args.mini_program_out_dir()?,
            &wasm_stem,
        )?;
    }
    let mut flavors = Vec::new();
    if targets.node {
        flavors.push(NapiBuildFlavorArg::Napi);
    }
    if targets.electron {
        flavors.push(NapiBuildFlavorArg::Electron);
    }
    if !flavors.is_empty() {
        build_napi(args.to_napi_args(flavors)?).context("building private managed N-API target")?;
    }
    if targets.harmony && !hsp_first {
        build_ohos(args.to_ohos_args()?).context("building private managed Harmony target")?;
    }
    Ok(())
}

fn build_managed_package(
    public_args: BuildArgs,
    targets: ExpandedTargets,
    layout: ManagedLayout,
) -> Result<()> {
    let mut transaction = ManagedPackageTransaction::begin(&layout)?;
    let private_layout = layout.rebased(&layout.package_dir, transaction.candidate_root())?;
    let prepared = (|| -> Result<ManagedPackageOwner> {
        clear_managed_selected_roots(&mut transaction, &targets)?;
        let private_args = managed_private_args(&transaction, &layout, &public_args)?;
        build_private_target_set(&private_args, &targets)?;
        rebase_private_javascript_host_crates(&public_args, &private_args, &targets)?;
        let meta = cargo_package_metadata(&public_args.manifest_path)?;
        private_layout
            .emit(&targets, &meta, &private_args)
            .context("emitting complete private managed package manifest")?;
        validate_managed_manifest_candidate(&private_layout, &public_args.cargo_bin)?;
        transaction.prepare_owner()
    })();
    let owner = match prepared {
        Ok(owner) => owner,
        Err(error) => return Err(transaction.abort(error)),
    };
    transaction.commit(owner)
}

fn build(mut args: BuildArgs) -> Result<()> {
    let targets = expand_targets(&args.target)?;
    if targets.mini_program && args.wasm_bindgen_target != WasmBindgenTargetArg::Web {
        bail!("--target mini-program requires --wasm-bindgen-target web");
    }
    if targets.harmony {
        super::ohos::preflight_hsp_frontend(super::ohos::HspFrontendPreflight {
            package_kind: args.ohos_package_kind,
            integrated_hsp: args.ohos_integrated_hsp,
            hsp_bundle_name: args.ohos_hsp_bundle_name.as_deref(),
            has_har_output: args.ohos_har_out.is_some(),
            has_hsp_output: args.ohos_runtime_hsp_out.is_some()
                || args.ohos_interface_har_out.is_some()
                || args.ohos_tgz_out.is_some(),
            no_har: args.ohos_no_har,
            skip_libs: args.ohos_skip_libs,
            compatible_sdk_version: args.ohos_compatible_sdk_version.as_deref(),
            target_sdk_version: args.ohos_target_sdk_version.as_deref(),
            compatible_sdk_type: args.ohos_compatible_sdk_type.as_deref(),
            bisheng: args.ohos_bisheng,
            hvigorw: args.ohos_hvigorw.as_deref(),
            ohpm: args.ohos_ohpm.as_deref(),
            deveco_sdk_home: args.ohos_deveco_sdk_home.as_deref(),
        })
        .context("preflighting Harmony HSP before managed layout or target generation")?;
    }
    let managed_layout = ManagedLayout::apply(&mut args, &targets)?;
    if let Some(layout) = managed_layout {
        return build_managed_package(args, targets, layout);
    }
    if targets.harmony && args.ohos_package_kind == super::ohos::PackageKind::Hsp {
        return build_multi_target_hsp(args, targets, None);
    }
    (|| -> Result<()> {
        let hsp_first = targets.harmony && args.ohos_package_kind == super::ohos::PackageKind::Hsp;
        if hsp_first {
            build_ohos(args.to_ohos_args()?)
                .context("building Harmony/OpenHarmony artifact target")?;
        }

        if targets.apple {
            build_apple(&args).context("building Apple artifact target")?;
        }
        if targets.android {
            build_android(&args).context("building Android artifact target")?;
        }
        if targets.wasm || targets.mini_program {
            build_wasm(args.to_wasm_args()?).context("building wasm artifact target")?;
        }
        if targets.mini_program {
            let meta = cargo_package_metadata(&args.manifest_path)?;
            let wasm_stem = format!("{}_wasm", rust_identifier(&meta.package_name));
            emit_mini_program_wasm_runtime(
                &args.out_dir()?,
                &args.wasm_bindgen_out_dir()?,
                &args.mini_program_out_dir()?,
                &wasm_stem,
            )
            .context("emitting Mini Program wasm runtime")?;
        }
        let mut napi_flavors = Vec::new();
        if targets.node {
            napi_flavors.push(NapiBuildFlavorArg::Napi);
        }
        if targets.electron {
            napi_flavors.push(NapiBuildFlavorArg::Electron);
        }
        if !napi_flavors.is_empty() {
            build_napi(args.to_napi_args(napi_flavors)?)
                .context("building N-API artifact target")?;
        }
        if targets.harmony && !hsp_first {
            build_ohos(args.to_ohos_args()?)
                .context("building Harmony/OpenHarmony artifact target")?;
        }

        Ok(())
    })()
}

fn build_android(args: &BuildArgs) -> Result<()> {
    let jni_libs_out = args
        .android_jni_libs_out
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--target android requires --android-jni-libs-out <dir>"))?;
    let ndk_home = resolve_android_ndk_home(args)?;
    let prebuilt = android_llvm_prebuilt_dir(&ndk_home)?;
    let abis = android_abis(args)?;
    let meta = cargo_package_metadata(&args.manifest_path)?;
    let profile = if args.release { "release" } else { "debug" };

    let mut host_build = Command::new(&args.cargo_bin);
    host_build
        .arg("build")
        .arg("--manifest-path")
        .arg(args.manifest_path.as_str());
    if args.release {
        host_build.arg("--release");
    }
    add_cargo_feature_args(&mut host_build, args);
    run_command(&args.cargo_bin, &mut host_build, "cargo")?;

    for abi in &abis {
        let mut rustup = Command::new("rustup");
        rustup.arg("target").arg("add").arg(abi.rust_target);
        run_command("rustup", &mut rustup, "rustup")?;

        let clang = android_clang_path(&prebuilt, abi, args.android_api);
        if !clang.exists() {
            bail!(
                "Android NDK clang not found at {}. Check --android-ndk-home/ANDROID_NDK_HOME and --android-api",
                clang
            );
        }

        let mut cargo = Command::new(&args.cargo_bin);
        cargo
            .arg("build")
            .arg("--manifest-path")
            .arg(args.manifest_path.as_str())
            .arg("--target")
            .arg(abi.rust_target)
            .env(android_linker_env(abi.rust_target), clang.as_str());
        if args.release {
            cargo.arg("--release");
        }
        add_cargo_feature_args(&mut cargo, args);
        run_command(&args.cargo_bin, &mut cargo, "cargo")?;

        let lib = android_sharedlib_path(&meta, abi.rust_target, profile);
        if !lib.exists() {
            bail!("Android shared library not found at {}", lib);
        }
        let out_dir = jni_libs_out.join(abi.abi);
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("creating Android jniLibs dir {out_dir}"))?;
        let dest = out_dir.join(format!("lib{}.so", meta.lib_target_name));
        std::fs::copy(&lib, &dest)
            .with_context(|| format!("copying Android shared library {lib} to {dest}"))?;
    }

    let generation_source = if let Some(source) = &args.source {
        source.clone()
    } else {
        args.library_path
            .clone()
            .unwrap_or_else(|| host_cdylib_path(&meta, args.release))
    };
    if !generation_source.exists() {
        bail!(
            "Kotlin generation source not found at {}. Pass --library-path or --source to override",
            generation_source
        );
    }

    let out_dir = args.out_dir()?;
    let kotlin_bindings_dir = out_dir.join("kotlin");
    std::fs::create_dir_all(&kotlin_bindings_dir)
        .with_context(|| format!("creating Kotlin output dir {kotlin_bindings_dir}"))?;
    let kotlin_config = android_kotlin_config(args)?;
    generate(GenerateOptions {
        languages: vec![TargetLanguage::Kotlin],
        out_dir: kotlin_bindings_dir.clone(),
        source: generation_source,
        config_override: kotlin_config.or_else(|| args.config.clone()),
        crate_filter: args.crate_name.clone(),
        metadata_no_deps: args.metadata_no_deps,
        format: !args.no_format,
        features: args.cargo_features.clone(),
        all_features: false,
        no_default_features: false,
        target: None,
    })?;

    if let Some(kotlin_out) = &args.android_kotlin_out {
        copy_dir_contents(&kotlin_bindings_dir, kotlin_out)?;
    }

    if let Some(aar_out) = &args.android_aar_out {
        let package_name = args
            .android_package_name
            .clone()
            .unwrap_or_else(|| format!("uniffi.{}", meta.lib_target_name));
        create_android_aar(
            aar_out,
            &out_dir.join("android/aar-root"),
            &jni_libs_out,
            &kotlin_bindings_dir,
            &package_name,
        )?;
    }

    Ok(())
}

fn build_apple(args: &BuildArgs) -> Result<()> {
    let xcframework_out = args
        .apple_xcframework_out
        .clone()
        .ok_or_else(|| anyhow::anyhow!("--target apple requires --apple-xcframework-out <path>"))?;
    let targets = apple_targets(args);
    let meta = cargo_package_metadata(&args.manifest_path)?;
    let profile = if args.release { "release" } else { "debug" };
    let framework_name = apple_binary_target_name(&meta);

    let mut host_build = Command::new(&args.cargo_bin);
    host_build
        .arg("build")
        .arg("--manifest-path")
        .arg(args.manifest_path.as_str());
    if args.release {
        host_build.arg("--release");
    }
    add_cargo_feature_args(&mut host_build, args);
    run_command(&args.cargo_bin, &mut host_build, "cargo")?;

    for target in &targets {
        let mut rustup = Command::new("rustup");
        rustup.arg("target").arg("add").arg(target);
        run_command("rustup", &mut rustup, "rustup")?;

        let mut cargo = Command::new(&args.cargo_bin);
        cargo
            .arg("build")
            .arg("--manifest-path")
            .arg(args.manifest_path.as_str())
            .arg("--target")
            .arg(target);
        add_apple_deployment_env(&mut cargo, target);
        if args.release {
            cargo.arg("--release");
        }
        add_cargo_feature_args(&mut cargo, args);
        run_command(&args.cargo_bin, &mut cargo, "cargo")?;
    }

    let generation_source = if let Some(source) = &args.source {
        source.clone()
    } else {
        args.library_path
            .clone()
            .unwrap_or_else(|| host_cdylib_path(&meta, args.release))
    };
    if !generation_source.exists() {
        bail!(
            "Swift generation source not found at {}. Pass --library-path or --source to override",
            generation_source
        );
    }

    let out_dir = args.out_dir()?;
    let swift_bindings_dir = out_dir.join("swift");
    std::fs::create_dir_all(&swift_bindings_dir)
        .with_context(|| format!("creating Swift output dir {swift_bindings_dir}"))?;
    generate(GenerateOptions {
        languages: vec![TargetLanguage::Swift],
        out_dir: swift_bindings_dir.clone(),
        source: generation_source,
        config_override: args.config.clone(),
        crate_filter: args.crate_name.clone(),
        metadata_no_deps: args.metadata_no_deps,
        format: !args.no_format,
        features: args.cargo_features.clone(),
        all_features: false,
        no_default_features: false,
        target: None,
    })?;

    let headers_dir = out_dir.join("apple/headers");
    stage_swift_headers(&swift_bindings_dir, &headers_dir)?;

    if let Some(swift_out) = &args.apple_swift_out {
        copy_swift_sources(&swift_bindings_dir, swift_out)?;
    }

    if let Some(parent) = xcframework_out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating XCFramework parent dir {parent}"))?;
    }
    let framework_build_root = xcframework_out
        .parent()
        .unwrap_or_else(|| Utf8Path::new("."))
        .join(".framework-build");
    if super::artifact_transaction::path_entry_exists(&framework_build_root)? {
        bail!(
            "fresh Apple framework build path unexpectedly exists without its creation-time witness: {framework_build_root}"
        );
    }
    std::fs::create_dir_all(&framework_build_root)
        .with_context(|| format!("creating framework build dir {framework_build_root}"))?;
    if super::artifact_transaction::path_entry_exists(&xcframework_out)? {
        bail!(
            "Apple XCFramework output was not cleared by its owning transaction: {xcframework_out}"
        );
    }

    let metallibs_required = targets
        .iter()
        .map(|target| apple_metallib_path(&meta, target, profile).exists())
        .any(|exists| exists);
    let frameworks: Vec<_> = targets
        .iter()
        .map(|target| {
            stage_apple_framework_slice(
                &meta,
                target,
                profile,
                &headers_dir,
                &framework_build_root,
                &framework_name,
                metallibs_required,
            )
        })
        .collect::<Result<_>>()?;
    let mut xcodebuild = Command::new("xcodebuild");
    xcodebuild.args(xcodebuild_create_xcframework_args(
        &frameworks,
        &xcframework_out,
    ));
    let build_result = run_command("xcodebuild", &mut xcodebuild, "xcodebuild");
    if let Err(error) = build_result {
        return Err(error).with_context(|| {
            format!(
                "Apple tool output is preserved after failure at identity-owned build path {framework_build_root}"
            )
        });
    }
    let framework_build_snapshot =
        super::artifact_transaction::capture_directory_for_cleanup(&framework_build_root)?;
    super::artifact_transaction::remove_captured_directory_for_cleanup(
        &framework_build_root,
        &framework_build_snapshot,
    )
    .with_context(|| format!("cleaning exact Apple framework build {framework_build_root}"))?;

    if let Some(expected_name) = &args.apple_framework_name {
        let actual = xcframework_out
            .file_stem()
            .unwrap_or_default()
            .trim_end_matches(".xcframework");
        if actual != expected_name {
            eprintln!(
                "warning: --apple-framework-name `{expected_name}` does not match output name `{actual}`"
            );
        }
    }

    Ok(())
}

fn stage_apple_framework_slice(
    meta: &CargoPackageMetadata,
    target: &str,
    profile: &str,
    headers_dir: &Utf8Path,
    framework_build_root: &Utf8Path,
    framework_name: &str,
    metallibs_required: bool,
) -> Result<Utf8PathBuf> {
    let slice = apple_slice_name(target)?;
    let framework_dir = framework_build_root
        .join(slice)
        .join(format!("{framework_name}.framework"));
    if super::artifact_transaction::path_entry_exists(&framework_dir)? {
        bail!(
            "fresh Apple framework slice path unexpectedly exists without its creation-time witness: {framework_dir}"
        );
    }

    let dylib_src = apple_cdylib_path(meta, target, profile);
    if !dylib_src.exists() {
        bail!("expected staged Apple dylib for {target} at {dylib_src}");
    }

    let metallib_src = apple_metallib_path(meta, target, profile);
    if metallibs_required && !metallib_src.exists() {
        bail!("expected staged mlx.metallib for {target} at {metallib_src}");
    }

    let (framework_bin, info_plist, metallib_dest) =
        create_apple_framework_layout(target, &framework_dir, framework_name)?;
    std::fs::copy(&dylib_src, &framework_bin)
        .with_context(|| format!("copying Apple dylib {dylib_src} into {framework_bin}"))?;
    rewrite_apple_framework_install_name(target, framework_name, &framework_bin)?;
    if metallib_src.exists() {
        let metallib_dest = metallib_dest.expect("metallib destination present");
        std::fs::copy(&metallib_src, &metallib_dest).with_context(|| {
            format!("copying Apple metallib {metallib_src} into framework slice {metallib_dest}")
        })?;
    }
    copy_apple_framework_headers(headers_dir, &framework_dir, framework_name)?;
    write_apple_framework_info_plist(target, framework_name, &info_plist)?;

    Ok(framework_dir)
}

fn add_cargo_feature_args(command: &mut Command, args: &BuildArgs) {
    if !args.cargo_features.is_empty() {
        command.arg("--features").arg(args.cargo_features.join(","));
    }
}

fn expand_targets(targets: &[ArtifactTargetArg]) -> Result<ExpandedTargets> {
    if targets.is_empty() {
        bail!("at least one --target is required");
    }

    let mut expanded = ExpandedTargets::default();
    for target in targets {
        match target {
            ArtifactTargetArg::Wasm => expanded.wasm = true,
            ArtifactTargetArg::MiniProgram => expanded.mini_program = true,
            ArtifactTargetArg::Node => expanded.node = true,
            ArtifactTargetArg::Electron => expanded.electron = true,
            ArtifactTargetArg::Harmony => expanded.harmony = true,
            ArtifactTargetArg::Apple => expanded.apple = true,
            ArtifactTargetArg::Android => expanded.android = true,
            ArtifactTargetArg::AllJs => {
                expanded.wasm = true;
                expanded.mini_program = true;
                expanded.node = true;
                expanded.electron = true;
                expanded.harmony = true;
            }
            ArtifactTargetArg::All => {
                expanded.wasm = true;
                expanded.mini_program = true;
                expanded.node = true;
                expanded.electron = true;
                expanded.harmony = true;
                expanded.apple = true;
                expanded.android = true;
            }
        }
    }
    Ok(expanded)
}

impl BuildArgs {
    fn out_dir(&self) -> Result<Utf8PathBuf> {
        self.out_dir.clone().ok_or_else(|| {
            anyhow::anyhow!("--out-dir <dir> is required unless --managed-layout is used")
        })
    }

    fn host_crates_dir(&self) -> Utf8PathBuf {
        self.host_crates_dir
            .clone()
            .unwrap_or_else(|| Utf8PathBuf::from("rust_modules"))
    }

    fn wasm_bindgen_out_dir(&self) -> Result<Utf8PathBuf> {
        Ok(self
            .wasm_bindgen_out_dir
            .clone()
            .or_else(|| {
                self.artifact_dir
                    .as_ref()
                    .map(|dir| dir.join("browser/pkg"))
            })
            .unwrap_or_else(|| {
                self.out_dir()
                    .expect("out_dir is validated")
                    .join("browser/pkg")
            }))
    }

    fn mini_program_out_dir(&self) -> Result<Utf8PathBuf> {
        Ok(self
            .artifact_dir
            .as_ref()
            .map(|dir| dir.join("mini-program"))
            .unwrap_or_else(|| {
                self.out_dir()
                    .expect("out_dir is validated")
                    .join("mini-program")
            }))
    }

    fn to_wasm_args(&self) -> Result<BuildWasmArgs> {
        Ok(BuildWasmArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir()?,
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir(),
            logical_host_crates_dir: self.logical_host_crates_dir.clone(),
            artifact_dir: self.artifact_dir.clone(),
            wasm_bindgen_out_dir: self.wasm_bindgen_out_dir.clone(),
            wasm_bindgen_target: self.wasm_bindgen_target,
            cargo_bin: self.cargo_bin.clone(),
            core_target_dir: self.wasm_core_target_dir.clone(),
            target_dir: self.wasm_target_dir.clone(),
            release: self.release,
            cargo_features: self.cargo_features.clone(),
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
        })
    }

    fn to_napi_args(&self, flavor: Vec<NapiBuildFlavorArg>) -> Result<BuildNapiArgs> {
        Ok(BuildNapiArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir()?,
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir(),
            logical_host_crates_dir: self.logical_host_crates_dir.clone(),
            artifact_dir: self.artifact_dir.clone(),
            flavor,
            cargo_bin: self.cargo_bin.clone(),
            target_dir: self.napi_target_dir.clone(),
            release: self.release,
            cargo_features: self.cargo_features.clone(),
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
        })
    }

    fn to_ohos_args(&self) -> Result<BuildOhosArgs> {
        Ok(BuildOhosArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir()?,
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir(),
            logical_host_crates_dir: self.logical_host_crates_dir.clone(),
            ohos_host_manifest_path: None,
            raw_only_facade: false,
            artifact_dir: self.artifact_dir.clone(),
            dist_dir: self.ohos_dist_dir.clone(),
            package_name: self.ohos_package_name.clone(),
            module_name: self.ohos_module_name.clone(),
            package_version: self.ohos_package_version.clone(),
            author: self.ohos_author.clone(),
            license: self.ohos_license.clone(),
            description: self.ohos_description.clone(),
            compatible_sdk_version: self.ohos_compatible_sdk_version.clone(),
            target_sdk_version: self.ohos_target_sdk_version.clone(),
            compatible_sdk_type: self.ohos_compatible_sdk_type.clone(),
            device_types: self.ohos_device_types.clone(),
            package_kind: self.ohos_package_kind,
            integrated_hsp: self.ohos_integrated_hsp,
            hsp_bundle_name: self.ohos_hsp_bundle_name.clone(),
            har_out: self.ohos_har_out.clone(),
            runtime_hsp_out: self.ohos_runtime_hsp_out.clone(),
            interface_har_out: self.ohos_interface_har_out.clone(),
            tgz_out: self.ohos_tgz_out.clone(),
            hvigorw: self.ohos_hvigorw.clone(),
            ohpm: self.ohos_ohpm.clone(),
            deveco_sdk_home: self.ohos_deveco_sdk_home.clone(),
            no_har: self.ohos_no_har,
            arch: self.ohos_arch.clone(),
            cargo_bin: self.cargo_bin.clone(),
            target_dir: self.ohos_target_dir.clone(),
            cargo_features: self.cargo_features.clone(),
            release: self.release,
            copy_static: self.ohos_static,
            skip_libs: self.ohos_skip_libs,
            dts_cache: self.ohos_dts_cache,
            skip_check: self.ohos_skip_check,
            zigbuild: self.ohos_zigbuild,
            bisheng: self.ohos_bisheng,
            package: self.ohos_package.clone(),
            skip_napi_check: self.ohos_skip_napi_check,
            soname: self.ohos_soname.clone(),
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
            cargo_args: self.ohos_cargo_args.clone(),
            output_lock_held: self.managed_layout || self.invocation_output_lock_held,
        })
    }
}

fn apple_targets(args: &BuildArgs) -> Vec<String> {
    if args.apple_target.is_empty() {
        vec![
            "aarch64-apple-ios".to_string(),
            "aarch64-apple-ios-sim".to_string(),
        ]
    } else {
        args.apple_target.clone()
    }
}

fn add_apple_deployment_env(command: &mut Command, target: &str) {
    if target.contains("apple-ios-sim") {
        let value = std::env::var("IPHONESIMULATOR_DEPLOYMENT_TARGET")
            .or_else(|_| std::env::var("IPHONEOS_DEPLOYMENT_TARGET"))
            .unwrap_or_else(|_| "16.0".to_string());
        command.env("IPHONESIMULATOR_DEPLOYMENT_TARGET", value);
    } else if target.contains("apple-ios") {
        let value =
            std::env::var("IPHONEOS_DEPLOYMENT_TARGET").unwrap_or_else(|_| "16.0".to_string());
        command.env("IPHONEOS_DEPLOYMENT_TARGET", value);
    }
}

fn apple_slice_name(target: &str) -> Result<&'static str> {
    match target {
        "aarch64-apple-darwin" => Ok("macos-arm64"),
        "aarch64-apple-ios" => Ok("ios-arm64"),
        "aarch64-apple-ios-sim" => Ok("ios-arm64-simulator"),
        _ => bail!("unsupported Apple target `{target}`"),
    }
}

fn apple_sdk_name(target: &str) -> Result<&'static str> {
    match target {
        "aarch64-apple-darwin" => Ok("macosx"),
        "aarch64-apple-ios" => Ok("iphoneos"),
        "aarch64-apple-ios-sim" => Ok("iphonesimulator"),
        _ => bail!("unsupported Apple target `{target}`"),
    }
}

fn apple_sdk_platform_name(target: &str) -> Result<&'static str> {
    apple_sdk_name(target)
}

fn apple_supported_platform_name(target: &str) -> Result<&'static str> {
    match target {
        "aarch64-apple-darwin" => Ok("MacOSX"),
        "aarch64-apple-ios" => Ok("iPhoneOS"),
        "aarch64-apple-ios-sim" => Ok("iPhoneSimulator"),
        _ => bail!("unsupported Apple target `{target}`"),
    }
}

fn apple_min_os(target: &str) -> Result<String> {
    Ok(match target {
        "aarch64-apple-darwin" => {
            std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "15.0".to_string())
        }
        "aarch64-apple-ios" | "aarch64-apple-ios-sim" => {
            std::env::var("IPHONEOS_DEPLOYMENT_TARGET")
                .or_else(|_| std::env::var("IPHONESIMULATOR_DEPLOYMENT_TARGET"))
                .unwrap_or_else(|_| "16.0".to_string())
        }
        _ => bail!("unsupported Apple target `{target}`"),
    })
}

fn apple_cdylib_path(meta: &CargoPackageMetadata, target: &str, profile: &str) -> Utf8PathBuf {
    meta.target_directory
        .join(target)
        .join(profile)
        .join(format!("lib{}.dylib", meta.lib_target_name))
}

fn apple_metallib_path(meta: &CargoPackageMetadata, target: &str, profile: &str) -> Utf8PathBuf {
    meta.target_directory
        .join(target)
        .join(profile)
        .join("mlx.metallib")
}

fn create_apple_framework_layout(
    target: &str,
    framework_dir: &Utf8Path,
    framework_name: &str,
) -> Result<(Utf8PathBuf, Utf8PathBuf, Option<Utf8PathBuf>)> {
    let framework_binary = framework_dir.join(framework_name);
    if target == "aarch64-apple-darwin" {
        let versions_dir = framework_dir.join("Versions");
        let current_dir = versions_dir.join("Current");
        let version_a_dir = versions_dir.join("A");
        let resources_dir = version_a_dir.join("Resources");
        std::fs::create_dir_all(&resources_dir)
            .with_context(|| format!("creating macOS framework resources dir {resources_dir}"))?;
        std::fs::create_dir_all(version_a_dir.join("Headers")).with_context(|| {
            format!(
                "creating macOS framework headers dir {}",
                version_a_dir.join("Headers")
            )
        })?;
        std::fs::create_dir_all(version_a_dir.join("Modules")).with_context(|| {
            format!(
                "creating macOS framework modules dir {}",
                version_a_dir.join("Modules")
            )
        })?;
        create_symlink("A", &current_dir)
            .with_context(|| format!("creating framework symlink {current_dir} -> A"))?;
        create_symlink(
            format!("Versions/Current/{framework_name}"),
            &framework_binary,
        )
        .with_context(|| format!("creating framework binary symlink {framework_binary}"))?;
        create_symlink("Versions/Current/Headers", &framework_dir.join("Headers")).with_context(
            || {
                format!(
                    "creating framework headers symlink {}",
                    framework_dir.join("Headers")
                )
            },
        )?;
        create_symlink(
            "Versions/Current/Resources",
            &framework_dir.join("Resources"),
        )
        .with_context(|| {
            format!(
                "creating framework resources symlink {}",
                framework_dir.join("Resources")
            )
        })?;
        create_symlink("Versions/Current/Modules", &framework_dir.join("Modules")).with_context(
            || {
                format!(
                    "creating framework modules symlink {}",
                    framework_dir.join("Modules")
                )
            },
        )?;
        Ok((
            version_a_dir.join(framework_name),
            resources_dir.join("Info.plist"),
            Some(resources_dir.join("default.metallib")),
        ))
    } else {
        std::fs::create_dir_all(framework_dir.join("Headers")).with_context(|| {
            format!(
                "creating iOS framework headers dir {}",
                framework_dir.join("Headers")
            )
        })?;
        std::fs::create_dir_all(framework_dir.join("Modules")).with_context(|| {
            format!(
                "creating iOS framework modules dir {}",
                framework_dir.join("Modules")
            )
        })?;
        Ok((
            framework_binary,
            framework_dir.join("Info.plist"),
            Some(framework_dir.join("mlx.metallib")),
        ))
    }
}

fn copy_apple_framework_headers(
    headers_dir: &Utf8Path,
    framework_dir: &Utf8Path,
    framework_name: &str,
) -> Result<()> {
    let (dest_headers_dir, dest_modules_dir) = if framework_dir.join("Versions/A").exists() {
        (
            framework_dir.join("Versions/A/Headers"),
            framework_dir.join("Versions/A/Modules"),
        )
    } else {
        (framework_dir.join("Headers"), framework_dir.join("Modules"))
    };
    std::fs::create_dir_all(&dest_headers_dir)
        .with_context(|| format!("creating framework headers dir {dest_headers_dir}"))?;
    std::fs::create_dir_all(&dest_modules_dir)
        .with_context(|| format!("creating framework modules dir {dest_modules_dir}"))?;

    for entry in std::fs::read_dir(headers_dir)
        .with_context(|| format!("reading staged Apple headers dir {headers_dir}"))?
    {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("Apple header path is not utf8: {}", p.display()))?;
        let Some(name) = path.file_name() else {
            continue;
        };
        if name == "module.modulemap" {
            continue;
        }
        let destination = dest_headers_dir.join(name);
        std::fs::copy(&path, &destination).with_context(|| {
            format!("copying Apple framework support file {path} into {destination}")
        })?;
    }
    let modulemap = format!(
        "framework module {framework_name} {{\n    umbrella header \"{framework_name}.h\"\n    export *\n    use \"Darwin\"\n    use \"_Builtin_stdbool\"\n    use \"_Builtin_stdint\"\n}}\n"
    );
    std::fs::write(dest_modules_dir.join("module.modulemap"), modulemap).with_context(|| {
        format!(
            "writing Apple framework modulemap for {} at {}",
            framework_name,
            dest_modules_dir.join("module.modulemap")
        )
    })?;
    Ok(())
}

fn rewrite_apple_framework_install_name(
    target: &str,
    framework_name: &str,
    framework_bin: &Utf8Path,
) -> Result<()> {
    let install_name = if target == "aarch64-apple-darwin" {
        format!("@rpath/{framework_name}.framework/Versions/A/{framework_name}")
    } else {
        format!("@rpath/{framework_name}.framework/{framework_name}")
    };
    let mut command = Command::new("install_name_tool");
    command
        .arg("-id")
        .arg(&install_name)
        .arg(framework_bin.as_str());
    run_command("install_name_tool", &mut command, "install_name_tool")
}

fn write_apple_framework_info_plist(
    target: &str,
    framework_name: &str,
    info_plist: &Utf8Path,
) -> Result<()> {
    let min_os = apple_min_os(target)?;
    let sdk_name = apple_sdk_name(target)?;
    let supported_platform = apple_supported_platform_name(target)?;
    let platform_name = apple_sdk_platform_name(target)?;
    let platform_version = command_stdout(
        "xcrun",
        &["--sdk", sdk_name, "--show-sdk-platform-version"],
        "xcrun",
    )?;
    let sdk_build = command_stdout(
        "xcrun",
        &["--sdk", sdk_name, "--show-sdk-build-version"],
        "xcrun",
    )?;
    let xcode_build = command_stdout("xcodebuild", &["-version"], "xcodebuild")?
        .lines()
        .find_map(|line| line.strip_prefix("Build version "))
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("unable to determine Xcode build version"))?;
    let xcode_marketing = command_stdout("xcodebuild", &["-version"], "xcodebuild")?
        .lines()
        .find_map(|line| line.strip_prefix("Xcode "))
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("unable to determine Xcode version"))?;
    let host_build =
        command_stdout("sw_vers", &["-buildVersion"], "sw_vers").context("reading host build")?;
    let xcode_numeric = apple_xcode_numeric_version(&xcode_marketing)?;
    let extra_keys = match target {
        "aarch64-apple-darwin" => format!(
            "  <key>LSMinimumSystemVersion</key>\n  <string>{min_os}</string>\n"
        ),
        "aarch64-apple-ios" => format!(
            "  <key>MinimumOSVersion</key>\n  <string>{min_os}</string>\n  <key>UIDeviceFamily</key>\n  <array>\n    <integer>1</integer>\n    <integer>2</integer>\n  </array>\n  <key>UIRequiredDeviceCapabilities</key>\n  <array>\n    <string>arm64</string>\n  </array>\n"
        ),
        "aarch64-apple-ios-sim" => format!(
            "  <key>MinimumOSVersion</key>\n  <string>{min_os}</string>\n  <key>UIDeviceFamily</key>\n  <array>\n    <integer>1</integer>\n    <integer>2</integer>\n  </array>\n"
        ),
        _ => bail!("unsupported Apple target `{target}`"),
    };
    let bundle_id = format!(
        "org.mozilla.uniffi.{}.{}",
        apple_bundle_identifier_component(framework_name),
        apple_bundle_identifier_component(platform_name)
    );

    let plist = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         \x20\x20<key>BuildMachineOSBuild</key>\n\
         \x20\x20<string>{host_build}</string>\n\
         \x20\x20<key>CFBundleDevelopmentRegion</key>\n\
         \x20\x20<string>en</string>\n\
         \x20\x20<key>CFBundleExecutable</key>\n\
         \x20\x20<string>{framework_name}</string>\n\
         \x20\x20<key>CFBundleIdentifier</key>\n\
         \x20\x20<string>{bundle_id}</string>\n\
         \x20\x20<key>CFBundleInfoDictionaryVersion</key>\n\
         \x20\x20<string>6.0</string>\n\
         \x20\x20<key>CFBundleName</key>\n\
         \x20\x20<string>{framework_name}</string>\n\
         \x20\x20<key>CFBundlePackageType</key>\n\
         \x20\x20<string>FMWK</string>\n\
         \x20\x20<key>CFBundleSupportedPlatforms</key>\n\
         \x20\x20<array>\n\
         \x20\x20\x20\x20<string>{supported_platform}</string>\n\
         \x20\x20</array>\n\
         \x20\x20<key>CFBundleVersion</key>\n\
         \x20\x20<string>1</string>\n\
         \x20\x20<key>DTCompiler</key>\n\
         \x20\x20<string>com.apple.compilers.llvm.clang.1_0</string>\n\
         \x20\x20<key>DTPlatformBuild</key>\n\
         \x20\x20<string>{sdk_build}</string>\n\
         \x20\x20<key>DTPlatformName</key>\n\
         \x20\x20<string>{platform_name}</string>\n\
         \x20\x20<key>DTPlatformVersion</key>\n\
         \x20\x20<string>{platform_version}</string>\n\
         \x20\x20<key>DTSDKBuild</key>\n\
         \x20\x20<string>{sdk_build}</string>\n\
         \x20\x20<key>DTSDKName</key>\n\
         \x20\x20<string>{platform_name}{platform_version}</string>\n\
         \x20\x20<key>DTXcode</key>\n\
         \x20\x20<string>{xcode_numeric}</string>\n\
         \x20\x20<key>DTXcodeBuild</key>\n\
         \x20\x20<string>{xcode_build}</string>\n\
         {extra_keys}\
         </dict>\n\
         </plist>\n"
    );
    std::fs::write(info_plist, plist)
        .with_context(|| format!("writing Apple framework Info.plist at {info_plist}"))?;
    Ok(())
}

fn apple_xcode_numeric_version(version: &str) -> Result<String> {
    let mut parts = version.split('.');
    let major = parts
        .next()
        .ok_or_else(|| anyhow::anyhow!("missing Xcode major version"))?
        .parse::<u32>()
        .with_context(|| format!("parsing Xcode major version from `{version}`"))?;
    let minor = parts
        .next()
        .unwrap_or("0")
        .parse::<u32>()
        .with_context(|| format!("parsing Xcode minor version from `{version}`"))?;
    Ok(format!("{major}{minor:02}"))
}

fn apple_bundle_identifier_component(value: &str) -> String {
    let lowered: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect();
    if lowered.is_empty() {
        "artifact".to_string()
    } else {
        lowered
    }
}

#[cfg(unix)]
fn create_symlink(target: impl AsRef<std::path::Path>, link: &Utf8Path) -> Result<()> {
    symlink(target.as_ref(), link.as_std_path())
        .with_context(|| format!("creating symlink {}", link))
}

#[cfg(not(unix))]
fn create_symlink(_target: impl AsRef<std::path::Path>, link: &Utf8Path) -> Result<()> {
    bail!("Apple framework symlink creation is only supported on unix hosts: {link}");
}

fn command_stdout(binary: &str, args: &[&str], tool_name: &str) -> Result<String> {
    let mut command = Command::new(binary);
    command.args(args);
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .with_context(|| format!("{tool_name} invocation failed while spawning `{binary}`"))?;
    if !output.status.success() {
        bail!(
            "{tool_name} command failed: {rendered}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn stage_swift_headers(swift_bindings_dir: &Utf8Path, headers_dir: &Utf8Path) -> Result<()> {
    if headers_dir.exists() {
        bail!(
            "fresh Apple header staging path unexpectedly exists without its creation-time witness: {headers_dir}"
        );
    }
    std::fs::create_dir_all(headers_dir)
        .with_context(|| format!("creating headers dir {headers_dir}"))?;

    let mut modulemap = String::new();
    for entry in std::fs::read_dir(swift_bindings_dir)
        .with_context(|| format!("reading Swift bindings dir {swift_bindings_dir}"))?
    {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("Swift output path is not utf8: {}", p.display()))?;
        let Some(name) = path.file_name() else {
            continue;
        };
        if name.ends_with("FFI.h") {
            std::fs::copy(&path, headers_dir.join(name))
                .with_context(|| format!("copying Swift FFI header {path}"))?;
        } else if name.ends_with("FFI.modulemap") {
            modulemap.push_str(&std::fs::read_to_string(&path)?);
            modulemap.push('\n');
        }
    }
    if modulemap.is_empty() {
        bail!("no Swift FFI modulemap found in {swift_bindings_dir}");
    }
    std::fs::write(headers_dir.join("module.modulemap"), modulemap)
        .with_context(|| format!("writing module.modulemap in {headers_dir}"))?;
    Ok(())
}

fn copy_swift_sources(swift_bindings_dir: &Utf8Path, swift_out: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(swift_out)
        .with_context(|| format!("creating Swift source output dir {swift_out}"))?;
    for entry in std::fs::read_dir(swift_bindings_dir)
        .with_context(|| format!("reading Swift bindings dir {swift_bindings_dir}"))?
    {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("Swift output path is not utf8: {}", p.display()))?;
        if path.extension() == Some("swift") {
            let Some(name) = path.file_name() else {
                continue;
            };
            std::fs::copy(&path, swift_out.join(name))
                .with_context(|| format!("copying Swift source {path}"))?;
        }
    }
    Ok(())
}

fn apple_package_product_name(meta: &CargoPackageMetadata) -> String {
    format!("{}Apple", upper_camel_case_identifier(&meta.package_name))
}

fn apple_binary_target_name(meta: &CargoPackageMetadata) -> String {
    format!("{}FFI", meta.lib_target_name)
}

fn apple_package_manifest_source(meta: &CargoPackageMetadata, args: &BuildArgs) -> Result<String> {
    let package_name = apple_package_product_name(meta);
    let binary_target = apple_binary_target_name(meta);
    let xcframework_name = args
        .apple_xcframework_out
        .as_ref()
        .and_then(|path| path.file_name())
        .unwrap_or("uni_core.xcframework");
    let platforms = apple_package_platform_lines(args);

    Ok(format!(
        "// swift-tools-version: 6.0\n\
         import PackageDescription\n\
         \n\
         let package = Package(\n\
         \x20\x20\x20\x20name: \"{package_name}\",\n\
         \x20\x20\x20\x20platforms: [\n\
         {platforms}\n\
         \x20\x20\x20\x20],\n\
         \x20\x20\x20\x20products: [\n\
         \x20\x20\x20\x20\x20\x20\x20\x20.library(\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20name: \"{package_name}\",\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20targets: [\"{package_name}\"]\n\
         \x20\x20\x20\x20\x20\x20\x20\x20),\n\
         \x20\x20\x20\x20],\n\
         \x20\x20\x20\x20targets: [\n\
         \x20\x20\x20\x20\x20\x20\x20\x20.binaryTarget(\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20name: \"{binary_target}\",\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20path: \"{xcframework_name}\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20),\n\
         \x20\x20\x20\x20\x20\x20\x20\x20.target(\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20name: \"{package_name}\",\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20dependencies: [\"{binary_target}\"],\n\
         \x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20path: \"Sources/{package_name}\"\n\
         \x20\x20\x20\x20\x20\x20\x20\x20),\n\
         \x20\x20\x20\x20],\n\
         \x20\x20\x20\x20swiftLanguageModes: [.v5]\n\
         )\n"
    ))
}

fn apple_package_platform_lines(args: &BuildArgs) -> String {
    let targets = apple_targets(args);
    let mut lines = Vec::new();
    if targets.iter().any(|target| target.contains("apple-ios")) {
        let deployment = std::env::var("IPHONEOS_DEPLOYMENT_TARGET")
            .or_else(|_| std::env::var("IPHONESIMULATOR_DEPLOYMENT_TARGET"))
            .unwrap_or_else(|_| "16.0".to_string());
        lines.push(format!("        .iOS(\"{deployment}\"),"));
    }
    if targets
        .iter()
        .any(|target| target == "aarch64-apple-darwin")
    {
        let deployment =
            std::env::var("MACOSX_DEPLOYMENT_TARGET").unwrap_or_else(|_| "15.0".to_string());
        lines.push(format!("        .macOS(\"{deployment}\"),"));
    }
    if lines.is_empty() {
        lines.push("        .iOS(\"16.0\"),".to_string());
    }
    lines.join("\n")
}

fn upper_camel_case_identifier(value: &str) -> String {
    let mut output = String::new();
    for segment in value
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
    {
        let mut chars = segment.chars();
        if let Some(first) = chars.next() {
            output.extend(first.to_uppercase());
            output.push_str(chars.as_str());
        }
    }
    if output.is_empty() {
        "UniFfi".to_string()
    } else {
        output
    }
}

fn run_command(binary: &str, command: &mut Command, tool_name: &str) -> Result<()> {
    let rendered = format!("{command:?}");
    let output = command
        .output()
        .with_context(|| format!("{tool_name} invocation failed while spawning `{binary}`"))?;
    if !output.status.success() {
        bail!(
            "{tool_name} command failed: {rendered}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

fn resolve_cwd_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
            .join(path))
    }
}

fn module_specifier(from_dir: &Utf8Path, to: &Utf8Path) -> Result<String> {
    let rel = relative_path_from_dir(from_dir, to)
        .to_string()
        .replace('\\', "/");
    let rel = if rel.is_empty() {
        ".".to_string()
    } else if rel.starts_with('.') {
        rel
    } else {
        format!("./{rel}")
    };
    Ok(rel.replace('"', "\\\""))
}

fn relative_path_from_dir(from_dir: &Utf8Path, to: &Utf8Path) -> Utf8PathBuf {
    let from: Vec<&str> = from_dir.components().map(|c| c.as_str()).collect();
    let to_vec: Vec<&str> = to.components().map(|c| c.as_str()).collect();
    let mut i = 0;
    while i < from.len() && i < to_vec.len() && from[i] == to_vec[i] {
        i += 1;
    }
    let mut result = Utf8PathBuf::new();
    for _ in i..from.len() {
        result.push("..");
    }
    for c in &to_vec[i..] {
        result.push(c);
    }
    result
}

#[derive(Debug)]
struct CargoPackageMetadata {
    target_directory: Utf8PathBuf,
    package_name: String,
    package_version: String,
    description: Option<String>,
    authors: Vec<String>,
    license: Option<String>,
    lib_target_name: String,
}

fn cargo_package_metadata(manifest_path: &Utf8Path) -> Result<CargoPackageMetadata> {
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_path.as_std_path())
        .exec()
        .with_context(|| format!("running cargo metadata for {manifest_path}"))?;
    let package = metadata
        .root_package()
        .with_context(|| format!("no root package found for manifest {manifest_path}"))?;
    let lib_target = package
        .targets
        .iter()
        .find(|target| {
            target
                .kind
                .iter()
                .any(|kind| kind.to_string() == "staticlib")
        })
        .or_else(|| {
            package
                .targets
                .iter()
                .find(|target| target.kind.iter().any(|kind| kind.to_string() == "lib"))
        })
        .with_context(|| format!("package {} has no lib/staticlib target", package.name))?;
    Ok(CargoPackageMetadata {
        target_directory: Utf8PathBuf::from_path_buf(
            metadata.target_directory.clone().into_std_path_buf(),
        )
        .map_err(|p| anyhow::anyhow!("cargo metadata target dir is not utf8: {}", p.display()))?,
        package_name: package.name.to_string(),
        package_version: package.version.to_string(),
        description: package.description.clone(),
        authors: package.authors.clone(),
        license: package.license.clone(),
        lib_target_name: lib_target.name.clone(),
    })
}

fn host_cdylib_path(meta: &CargoPackageMetadata, release: bool) -> Utf8PathBuf {
    host_cdylib_path_in(meta, &meta.target_directory, release)
}

fn host_cdylib_path_in(
    meta: &CargoPackageMetadata,
    target_directory: &Utf8Path,
    release: bool,
) -> Utf8PathBuf {
    target_directory
        .join(if release { "release" } else { "debug" })
        .join(host_cdylib_filename(&meta.lib_target_name))
}

fn host_cdylib_filename(lib_target_name: &str) -> String {
    if cfg!(target_os = "windows") {
        format!("{lib_target_name}.dll")
    } else if cfg!(target_os = "macos") {
        format!("lib{lib_target_name}.dylib")
    } else {
        format!("lib{lib_target_name}.so")
    }
}

fn rust_identifier(package_name: &str) -> String {
    package_name.replace('-', "_")
}

fn xcodebuild_create_xcframework_args(
    frameworks: &[Utf8PathBuf],
    output: &Utf8Path,
) -> Vec<String> {
    let mut args = vec!["-create-xcframework".to_string()];
    for framework in frameworks {
        args.push("-framework".to_string());
        args.push(framework.to_string());
    }
    args.push("-output".to_string());
    args.push(output.to_string());
    args
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct AndroidAbi {
    abi: &'static str,
    rust_target: &'static str,
    clang_prefix: &'static str,
}

fn android_abis(args: &BuildArgs) -> Result<Vec<AndroidAbi>> {
    let requested = if args.android_abi.is_empty() {
        vec!["arm64-v8a".to_string(), "x86_64".to_string()]
    } else {
        args.android_abi.clone()
    };
    requested
        .iter()
        .map(|abi| android_abi(abi))
        .collect::<Result<Vec<_>>>()
}

fn android_abi(abi: &str) -> Result<AndroidAbi> {
    match abi {
        "arm64-v8a" | "aarch64-linux-android" => Ok(AndroidAbi {
            abi: "arm64-v8a",
            rust_target: "aarch64-linux-android",
            clang_prefix: "aarch64-linux-android",
        }),
        "x86_64" | "x86_64-linux-android" => Ok(AndroidAbi {
            abi: "x86_64",
            rust_target: "x86_64-linux-android",
            clang_prefix: "x86_64-linux-android",
        }),
        "armeabi-v7a" | "armv7-linux-androideabi" => Ok(AndroidAbi {
            abi: "armeabi-v7a",
            rust_target: "armv7-linux-androideabi",
            clang_prefix: "armv7a-linux-androideabi",
        }),
        "x86" | "i686-linux-android" => Ok(AndroidAbi {
            abi: "x86",
            rust_target: "i686-linux-android",
            clang_prefix: "i686-linux-android",
        }),
        _ => bail!(
            "unsupported Android ABI `{abi}`; expected arm64-v8a, x86_64, armeabi-v7a, or x86"
        ),
    }
}

fn resolve_android_ndk_home(args: &BuildArgs) -> Result<Utf8PathBuf> {
    if let Some(path) = &args.android_ndk_home {
        return Ok(path.clone());
    }
    if let Ok(path) = std::env::var("ANDROID_NDK_HOME") {
        if !path.is_empty() {
            return Utf8PathBuf::from_path_buf(path.into())
                .map_err(|p| anyhow::anyhow!("ANDROID_NDK_HOME is not utf8: {}", p.display()));
        }
    }
    if let Ok(sdk_root) = std::env::var("ANDROID_SDK_ROOT") {
        let ndk_root = Utf8PathBuf::from_path_buf(sdk_root.into())
            .map_err(|p| anyhow::anyhow!("ANDROID_SDK_ROOT is not utf8: {}", p.display()))?
            .join("ndk");
        if let Some(latest) = latest_child_dir(&ndk_root)? {
            return Ok(latest);
        }
    }
    bail!(
        "Android NDK not found. Pass --android-ndk-home, set ANDROID_NDK_HOME, or set ANDROID_SDK_ROOT with an ndk/<version> directory"
    )
}

fn latest_child_dir(root: &Utf8Path) -> Result<Option<Utf8PathBuf>> {
    if !root.exists() {
        return Ok(None);
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root).with_context(|| format!("reading {root}"))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|p| anyhow::anyhow!("NDK path is not utf8: {}", p.display()))?;
            dirs.push(path);
        }
    }
    dirs.sort();
    Ok(dirs.pop())
}

fn android_llvm_prebuilt_dir(ndk_home: &Utf8Path) -> Result<Utf8PathBuf> {
    let host_candidates: &[&str] = if cfg!(target_os = "macos") {
        &["darwin-x86_64", "darwin-aarch64"]
    } else if cfg!(target_os = "linux") {
        &["linux-x86_64"]
    } else if cfg!(target_os = "windows") {
        &["windows-x86_64"]
    } else {
        &[]
    };
    for host in host_candidates {
        let path = ndk_home.join("toolchains/llvm/prebuilt").join(host);
        if path.exists() {
            return Ok(path);
        }
    }
    bail!(
        "Android NDK LLVM prebuilt directory not found under {}",
        ndk_home.join("toolchains/llvm/prebuilt")
    )
}

fn android_clang_path(prebuilt: &Utf8Path, abi: &AndroidAbi, api: u32) -> Utf8PathBuf {
    prebuilt.join("bin").join(format!(
        "{}{}-clang{}",
        abi.clang_prefix,
        api,
        std::env::consts::EXE_SUFFIX
    ))
}

fn android_linker_env(rust_target: &str) -> String {
    format!(
        "CARGO_TARGET_{}_LINKER",
        rust_target.replace('-', "_").to_ascii_uppercase()
    )
}

fn android_sharedlib_path(meta: &CargoPackageMetadata, target: &str, profile: &str) -> Utf8PathBuf {
    meta.target_directory
        .join(target)
        .join(profile)
        .join(format!("lib{}.so", meta.lib_target_name))
}

fn android_kotlin_config(args: &BuildArgs) -> Result<Option<Utf8PathBuf>> {
    let Some(package_name) = &args.android_package_name else {
        return Ok(None);
    };
    let dir = args.out_dir()?.join("android");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating Android generated config dir {dir}"))?;
    let path = dir.join("uniffi-kotlin.toml");
    std::fs::write(
        &path,
        format!("[bindings.kotlin]\npackage_name = \"{package_name}\"\n"),
    )
    .with_context(|| format!("writing Kotlin config override {path}"))?;
    Ok(Some(path))
}

fn copy_dir_contents(from: &Utf8Path, to: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating directory {to}"))?;
    copy_dir_contents_inner(from, to)
}

fn copy_dir_contents_inner(from: &Utf8Path, to: &Utf8Path) -> Result<()> {
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {from}"))? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("path is not utf8: {}", p.display()))?;
        let name = path
            .file_name()
            .with_context(|| format!("path has no file name: {path}"))?;
        let dest = to.join(name);
        if entry.file_type()?.is_dir() {
            std::fs::create_dir_all(&dest).with_context(|| format!("creating {dest}"))?;
            copy_dir_contents_inner(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).with_context(|| format!("copying {path} to {dest}"))?;
        }
    }
    Ok(())
}

fn create_android_aar(
    aar_out: &Utf8Path,
    staging_dir: &Utf8Path,
    jni_libs_dir: &Utf8Path,
    kotlin_dir: &Utf8Path,
    package_name: &str,
) -> Result<()> {
    if super::artifact_transaction::path_entry_exists(staging_dir)? {
        bail!(
            "fresh Android AAR staging path unexpectedly exists without its creation-time witness: {staging_dir}"
        );
    }
    std::fs::create_dir_all(staging_dir)
        .with_context(|| format!("creating AAR staging dir {staging_dir}"))?;
    copy_dir_contents(jni_libs_dir, &staging_dir.join("jni"))?;
    copy_dir_contents(kotlin_dir, &staging_dir.join("kotlin"))?;
    std::fs::write(
        staging_dir.join("AndroidManifest.xml"),
        android_manifest(package_name),
    )
    .with_context(|| format!("writing AndroidManifest.xml in {staging_dir}"))?;
    std::fs::write(staging_dir.join("R.txt"), "")
        .with_context(|| format!("writing R.txt in {staging_dir}"))?;
    std::fs::write(
        staging_dir.join("proguard.txt"),
        format!("-keep class {}.** {{ *; }}\n", package_name),
    )
    .with_context(|| format!("writing proguard.txt in {staging_dir}"))?;

    if let Some(parent) = aar_out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating AAR output parent dir {parent}"))?;
    }
    let file = std::fs::File::create(aar_out).with_context(|| format!("creating {aar_out}"))?;
    let mut zip = zip::ZipWriter::new(file);
    add_dir_to_zip(&mut zip, staging_dir, staging_dir)?;
    zip.finish()
        .with_context(|| format!("finishing AAR archive {aar_out}"))?;
    Ok(())
}

fn android_manifest(package_name: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<manifest xmlns:android=\"http://schemas.android.com/apk/res/android\" package=\"{package_name}\" />\n"
    )
}

fn add_dir_to_zip<W: Write + Seek>(
    zip: &mut zip::ZipWriter<W>,
    dir: &Utf8Path,
    root: &Utf8Path,
) -> Result<()> {
    let options = zip::write::FileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o644);
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {dir}"))? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("AAR staging path is not utf8: {}", p.display()))?;
        let rel = path
            .strip_prefix(root)
            .with_context(|| format!("computing relative AAR path for {path}"))?
            .to_string()
            .replace('\\', "/");
        if entry.file_type()?.is_dir() {
            zip.add_directory(format!("{rel}/"), options)
                .with_context(|| format!("adding AAR directory {rel}"))?;
            add_dir_to_zip(zip, &path, root)?;
        } else {
            zip.start_file(&rel, options)
                .with_context(|| format!("adding AAR file {rel}"))?;
            let mut file = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
            std::io::copy(&mut file, zip).with_context(|| format!("zipping {path}"))?;
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "artifact_transaction/artifacts_characterization_tests.rs"]
mod tests;
