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
use cargo_metadata::{CargoOpt, MetadataCommand};
use clap::{Args, Subcommand, ValueEnum};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Seek, Write};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::process::Command;
use uniffi_bindgen::{
    bindings::{generate, GenerateOptions, TargetLanguage},
    cargo_metadata::CrateConfigSupplier,
    BindgenLoader, BindgenPaths, CargoMetadataOptions, GlobalConfig,
};
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
    /// Root package Cargo manifest.
    #[clap(long = "manifest-path")]
    manifest_path: Utf8PathBuf,

    /// Directory in which to write generated files. Required unless --managed-layout is used.
    #[clap(long, short)]
    out_dir: Option<Utf8PathBuf>,

    /// Artifact target(s) to build: wasm, Mini Program, Node, Electron,
    /// Harmony, Apple, and Android.
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

    /// Directory for built non-source artifacts. With it, wasm-bindgen output
    /// defaults to `<artifact-dir>/browser/pkg` and a composite Node/Electron
    /// addon to `<artifact-dir>/node/<host-stem>.node`; otherwise wasm uses
    /// `<out-dir>/browser/pkg` and source-only output retains its local fallback.
    #[clap(long = "artifact-dir")]
    artifact_dir: Option<Utf8PathBuf>,

    /// Opt in to a package-oriented artifact layout rooted at --package-dir.
    #[clap(long = "managed-layout")]
    managed_layout: bool,

    /// Package root used by --managed-layout. Defaults to the current working directory.
    #[clap(long = "package-dir")]
    package_dir: Option<Utf8PathBuf>,

    /// Build the root package and generated host crates in release mode.
    #[clap(long)]
    release: bool,

    /// Cargo features enabled on the root package for native Apple, Android,
    /// Harmony, and N-API artifacts. May be repeated or comma-separated.
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

    /// Where to write the wasm-bindgen output tree. Defaults to
    /// `<artifact-dir>/browser/pkg` when --artifact-dir is set, otherwise `<out-dir>/browser/pkg`.
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

    /// Invocation-private Cargo target directory for the root-package wasm build.
    #[clap(skip)]
    wasm_core_target_dir: Option<Utf8PathBuf>,

    /// Output directory for built OHOS dist artifacts (intermediate native output).
    #[clap(long = "ohos-dist-dir")]
    pub(super) ohos_dist_dir: Option<Utf8PathBuf>,

    /// OHPM package name for generated Harmony package metadata (HAR or HSP;
    /// supports scoped names like `@scope/name`).
    #[clap(long = "ohos-package-name")]
    pub(super) ohos_package_name: Option<String>,

    /// Harmony module name override.
    #[clap(long = "ohos-module-name")]
    ohos_module_name: Option<String>,

    /// Semantic version override for generated Harmony package metadata.
    #[clap(long = "ohos-package-version")]
    ohos_package_version: Option<String>,

    /// Author override for generated Harmony package metadata.
    #[clap(long = "ohos-author")]
    ohos_author: Option<String>,

    /// SPDX license override for generated Harmony package metadata.
    #[clap(long = "ohos-license")]
    ohos_license: Option<String>,

    /// Description override for generated Harmony package metadata.
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

    /// Final Harmony package kind. HAR is the default; choose HSP explicitly.
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

    /// Optional AAR output path for the Android target.
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

#[derive(Clone, Default, Debug, Eq, PartialEq)]
struct ExpandedTargets {
    wasm: bool,
    mini_program: bool,
    node: bool,
    electron: bool,
    harmony: bool,
    apple: bool,
    android: bool,
}

impl ExpandedTargets {
    fn contains(&self, other: &Self) -> bool {
        (!other.wasm || self.wasm)
            && (!other.mini_program || self.mini_program)
            && (!other.node || self.node)
            && (!other.electron || self.electron)
            && (!other.harmony || self.harmony)
            && (!other.apple || self.apple)
            && (!other.android || self.android)
    }
}

#[derive(Clone, Debug)]
pub(super) struct ManagedLayout {
    package_dir: Utf8PathBuf,
    /// Cargo package identity used for the source-mode planner. This is
    /// intentionally distinct from the root library target: a package may
    /// export a differently named `[lib]`, while `src:<...>` resolves through
    /// the package map in `CrateConfigSupplier`.
    root_source_package: String,
    /// The root library target remains distinct from the composite host
    /// target and is the producer-owned Apple xcframework stem.
    root_lib_target: String,
    source_root: Utf8PathBuf,
    pub(super) artifact_root: Utf8PathBuf,
    host_crates_root: Utf8PathBuf,
    pub(super) manifest_path: Utf8PathBuf,
    /// `ManagedLayout::apply` seeds this with the root library identity only
    /// long enough to select `src:<root-package>` for the read-only planner. Every
    /// managed command promotes the resulting source/library plan before it
    /// may inspect, adopt, lock, or mutate an existing package.
    components: Option<Vec<ManagedComponentIdentity>>,
    /// Only a current source/library plan (or a deliberately test-only
    /// fixture) may set this true. It is then compared exactly with both
    /// existing bridges and the existing manifest.
    components_authoritative: bool,
    /// The logical host tuple deliberately does not vary by JS flavor.  It
    /// is needed to recompute the manifest checksum before any managed lock
    /// or transaction state is created.
    host_identity: Option<ManagedHostIdentity>,
    /// Producer-owned route truth for the current invocation.  It is set
    /// before existing-package preflight so a manifest cannot substitute an
    /// already-existing same-typed path for a generated route.
    expected_routes: Option<ManagedArtifactRoutePlan>,
    /// Inputs needed to reconstruct the producer route plan for an existing
    /// target set before any managed transaction mutation.
    route_inputs: Option<ManagedArtifactRouteInputs>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ManagedComponentIdentity {
    component: String,
    namespace: String,
    native_export_prefix: String,
    interface_abi_digest: String,
}

fn validate_sha256_digest(value: &str, label: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("managed artifact {label} must be a canonical lowercase SHA-256 digest");
    }
    Ok(())
}

impl ManagedComponentIdentity {
    fn with_interface_abi_digest(
        component: impl Into<String>,
        namespace: impl Into<String>,
        interface_abi_digest: impl Into<String>,
    ) -> Result<Self> {
        let component = component.into();
        let namespace = namespace.into();
        let interface_abi_digest = interface_abi_digest.into();
        uniffi_bindgen::interface::validate_harmony_component_identity(&component, &namespace)?;
        validate_sha256_digest(&interface_abi_digest, "component interface ABI digest")?;
        Ok(Self {
            native_export_prefix: uniffi_bindgen::interface::native_export_prefix_for_component(
                &component,
            ),
            component,
            namespace,
            interface_abi_digest,
        })
    }

    #[cfg(test)]
    fn new(component: impl Into<String>, namespace: impl Into<String>) -> Result<Self> {
        let component = component.into();
        let namespace = namespace.into();
        let digest = sha256_bytes(format!("fixture-interface:{component}:{namespace}").as_bytes());
        Self::with_interface_abi_digest(component, namespace, digest)
    }

    fn tuple(&self) -> (String, String, String, String) {
        (
            self.component.clone(),
            self.namespace.clone(),
            self.native_export_prefix.clone(),
            self.interface_abi_digest.clone(),
        )
    }

    fn same_component_key(&self, other: &Self) -> bool {
        self.component == other.component
            && self.namespace == other.namespace
            && self.native_export_prefix == other.native_export_prefix
    }
}

/// Load the current component identities without creating generated output.
/// Explicit `--source`/`--library-path` inputs are used verbatim; otherwise
/// the root package is read through `src:<root-package>`. This makes every managed
/// command authoritative before frontend probing, transaction setup, or
/// backend Cargo work can adopt an older managed package.
fn managed_authoritative_input_components(
    args: &BuildArgs,
    root_package: &str,
) -> Result<Vec<ManagedComponentIdentity>> {
    let source = args
        .source
        .as_ref()
        .or(args.library_path.as_ref())
        .cloned()
        // The regular build path has no readable cdylib until after Cargo
        // runs. Source parsing gives it the same complete component plan up
        // front, including source dependencies selected by Cargo metadata.
        .unwrap_or_else(|| Utf8PathBuf::from(format!("src:{root_package}")));
    let source = source.canonicalize_utf8().unwrap_or(source);

    // Keep this construction deliberately in lockstep with `generate_js`.
    // In particular, `src:<crate>` sources need the Cargo metadata layer and
    // a supplied global config can add crate-root layers before metadata is
    // parsed.  Nothing below creates a generated tree or invokes a build.
    let mut paths = BindgenPaths::default();
    let global_config = if let Some(config) = &args.config {
        let (global_config, crate_roots_layer) = GlobalConfig::from_file(config)?;
        if let Some(layer) = crate_roots_layer {
            paths.add_layer(layer);
        }
        global_config
    } else {
        GlobalConfig::default()
    };
    let mut cargo_metadata = MetadataCommand::new();
    cargo_metadata.manifest_path(args.manifest_path.as_std_path());
    if args.metadata_no_deps {
        cargo_metadata.no_deps();
    }
    if !args.cargo_features.is_empty() {
        cargo_metadata.features(CargoOpt::SomeFeatures(args.cargo_features.clone()));
    }
    let metadata = cargo_metadata
        .exec()
        .with_context(|| format!("running cargo metadata for {}", args.manifest_path))?;
    paths.add_layer(CrateConfigSupplier::from_cargo_metadata(
        metadata,
        CargoMetadataOptions {
            no_deps: args.metadata_no_deps,
            features: args.cargo_features.clone(),
            ..CargoMetadataOptions::default()
        },
    ));
    let loader = BindgenLoader::new(paths, global_config);
    let metadata = loader.load_metadata(&source)?;
    if let Some(crate_filter) = &args.crate_name {
        if !metadata.contains_key(crate_filter) {
            bail!("No UniFFI metadata found for crate {crate_filter}");
        }
    }
    let mut components = loader.load_cis(&source, metadata)?;
    if let Some(crate_filter) = &args.crate_name {
        components.retain(|component| component.crate_name() == crate_filter);
    }
    canonical_managed_component_identities(
        components
            .iter()
            .map(|component| {
                ManagedComponentIdentity::with_interface_abi_digest(
                    component.crate_name(),
                    component.namespace(),
                    uniffi_bindgen_javascript::host_crates::component_interface_abi_digest(
                        component,
                    ),
                )
            })
            .collect::<Result<Vec<_>>>()?,
    )
}

fn canonical_managed_component_identities(
    mut identities: Vec<ManagedComponentIdentity>,
) -> Result<Vec<ManagedComponentIdentity>> {
    if identities.is_empty() {
        bail!("managed artifact component set is empty");
    }
    identities.sort();
    let mut component_names = BTreeSet::new();
    let mut namespaces = BTreeSet::new();
    let mut native_export_prefixes = BTreeSet::new();
    for identity in &identities {
        if !component_names.insert(identity.component.as_str())
            || !namespaces.insert(identity.namespace.as_str())
            || !native_export_prefixes.insert(identity.native_export_prefix.as_str())
        {
            bail!("managed artifact component set has duplicate canonical identity fields");
        }
    }
    Ok(identities)
}

fn same_managed_component_keys(
    left: &[ManagedComponentIdentity],
    right: &[ManagedComponentIdentity],
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|(left, right)| left.same_component_key(right))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ManagedHostIdentity {
    package_name: String,
    lib_target: String,
}

impl ManagedHostIdentity {
    fn from_cargo_metadata(meta: &CargoPackageMetadata) -> Self {
        Self {
            package_name: uniffi_bindgen_javascript::host_crates::composite_host_package_name(
                &meta.package_name,
            ),
            lib_target: uniffi_bindgen_javascript::host_crates::composite_host_lib_target(
                &meta.package_name,
            ),
        }
    }

    fn composite_identity(&self, components: &[ManagedComponentIdentity]) -> Result<String> {
        uniffi_bindgen_javascript::host_crates::composite_host_identity(
            &self.package_name,
            &self.lib_target,
            &components
                .iter()
                .map(ManagedComponentIdentity::tuple)
                .collect::<Vec<_>>(),
        )
    }
}

/// The complete producer-owned route contract for one managed invocation.
///
/// The manifest has to be validated before Cargo, locks, journals, or an
/// invocation root can exist, so the validator cannot infer a route from the
/// file that it is validating.  This plan is derived solely from the managed
/// layout, selected targets, host identity, and already-derived build args;
/// the renderer consumes the same values below.
#[derive(Clone, Debug)]
struct ManagedArtifactRoutePlan {
    targets: ExpandedTargets,
    components: serde_json::Value,
    source: serde_json::Value,
    entrypoints: serde_json::Value,
    artifacts: serde_json::Value,
    host_crates: serde_json::Value,
}

#[derive(Clone, Debug)]
struct ManagedArtifactRouteInputs {
    ohos_package_name: Option<String>,
    ohos_no_har: bool,
    ohos_package_kind: super::ohos::PackageKind,
    ohos_integrated_hsp: bool,
    android_aar_out: Option<Utf8PathBuf>,
}

impl From<&BuildArgs> for ManagedArtifactRouteInputs {
    fn from(args: &BuildArgs) -> Self {
        Self {
            ohos_package_name: args.ohos_package_name.clone(),
            ohos_no_har: args.ohos_no_har,
            ohos_package_kind: args.ohos_package_kind,
            ohos_integrated_hsp: args.ohos_integrated_hsp,
            android_aar_out: args.android_aar_out.clone(),
        }
    }
}

impl ManagedArtifactRoutePlan {
    fn validate_manifest_routes(
        &self,
        manifest: &serde_json::Value,
        strict_current_targets: bool,
    ) -> Result<()> {
        let mut manifest_targets = BTreeSet::new();
        for target in manifest
            .get("targets")
            .and_then(serde_json::Value::as_array)
            .context("managed artifact manifest lacks targets while validating routes")?
        {
            manifest_targets.insert(
                target
                    .as_str()
                    .context("managed artifact manifest target must be a string")?,
            );
        }
        let active = |target: &str, planned: bool| -> Result<bool> {
            if !planned {
                return Ok(false);
            }
            let present = manifest_targets.contains(target);
            if strict_current_targets && !present {
                bail!(
                    "managed artifact manifest is missing currently requested `{target}` target while validating producer routes"
                );
            }
            Ok(present)
        };
        let wasm = active("wasm", self.targets.wasm)?;
        let mini_program = active("mini-program", self.targets.mini_program)?;
        let node = active("node", self.targets.node)?;
        let electron = active("electron", self.targets.electron)?;
        let harmony = active("harmony", self.targets.harmony)?;
        let apple = active("apple", self.targets.apple)?;
        let android = active("android", self.targets.android)?;
        let has_js = wasm || mini_program || node || electron || harmony;
        let has_browser = wasm || mini_program;

        let check = |pointer: &str, expected: &serde_json::Value, label: &str| -> Result<()> {
            let actual = manifest.pointer(pointer).with_context(|| {
                format!("managed artifact manifest `{label}` is missing while validating routes")
            })?;
            if actual != expected {
                bail!(
                    "managed artifact manifest `{label}` route mismatch: expected {expected}, got {actual}"
                );
            }
            Ok(())
        };
        let expected_source = self
            .source
            .as_object()
            .context("managed route plan source must be an object")?;
        check("/source/root", &expected_source["root"], "source.root")?;
        for (field, present) in [
            ("shared", has_js),
            ("browser", has_browser),
            ("node", node),
            ("electron", electron),
            ("harmony", harmony),
            ("swift", apple),
            ("kotlin", android),
        ] {
            if present {
                check(
                    &format!("/source/{field}"),
                    &expected_source[field],
                    &format!("source.{field}"),
                )?;
            }
        }

        let expected_components = self
            .components
            .as_array()
            .context("managed route plan components must be an array")?;
        let actual_components = manifest
            .get("components")
            .and_then(serde_json::Value::as_array)
            .context("managed artifact manifest lacks components while validating routes")?;
        // The caller owns component-identity compatibility and reports that
        // separately.  In particular, an Apple-only incremental publication
        // must be able to inspect a previously generated multi-component JS
        // package before it adopts the generated identities.  Only compare
        // component routes when the identities are already the same; never
        // turn a component-set mismatch into a misleading route error.
        let components_match = actual_components.len() == expected_components.len()
            && actual_components
                .iter()
                .zip(expected_components)
                .all(|(actual, expected)| {
                    ["component", "namespace", "nativeExportPrefix"]
                        .iter()
                        .all(|field| actual.get(*field) == expected.get(*field))
                });
        if components_match {
            for (index, expected_component) in expected_components.iter().enumerate() {
                for field in ["component", "namespace", "nativeExportPrefix"] {
                    check(
                        &format!("/components/{index}/{field}"),
                        &expected_component[field],
                        &format!("components[{index}].{field}"),
                    )?;
                }
                let expected_component_source = expected_component["source"]
                    .as_object()
                    .context("managed route plan component source must be an object")?;
                for (field, present) in [
                    ("common", has_js),
                    ("publicTypes", has_js),
                    ("browser", has_browser),
                    ("node", node),
                    ("electron", electron),
                    ("harmony", harmony),
                ] {
                    if present {
                        check(
                            &format!("/components/{index}/source/{field}"),
                            &expected_component_source[field],
                            &format!("components[{index}].source.{field}"),
                        )?;
                    }
                }
            }
        }

        let expected_entrypoints = self
            .entrypoints
            .as_object()
            .context("managed route plan entrypoints must be an object")?;
        for (field, present) in [
            ("web", wasm),
            ("miniProgram", mini_program),
            ("node", node),
            ("electron", electron),
            ("harmony", harmony),
        ] {
            if present {
                check(
                    &format!("/entrypoints/{field}"),
                    &expected_entrypoints[field],
                    &format!("entrypoints.{field}"),
                )?;
            }
        }

        for (field, present) in [
            ("wasm", wasm),
            ("miniProgram", mini_program),
            ("node", node),
            ("electron", electron),
            ("apple", apple),
            ("android", android),
        ] {
            if present {
                check(
                    &format!("/artifacts/{field}"),
                    &self.artifacts[field],
                    &format!("artifacts.{field}"),
                )?;
            }
        }
        if harmony {
            let expected = self.artifacts["harmony"]
                .as_object()
                .context("managed route plan Harmony artifact must be an object")?;
            let actual = manifest
                .pointer("/artifacts/harmony")
                .and_then(serde_json::Value::as_object)
                .context("managed artifact manifest Harmony artifact must be an object")?;
            for (field, expected_value) in expected {
                if field == "metadata" {
                    continue;
                }
                let actual_value = actual.get(field).with_context(|| {
                    format!("managed artifact manifest artifacts.harmony.{field} is missing")
                })?;
                if actual_value != expected_value {
                    bail!(
                        "managed artifact manifest `artifacts.harmony.{field}` route mismatch: expected {expected_value}, got {actual_value}"
                    );
                }
            }
        }

        let expected_host_crates = self
            .host_crates
            .as_object()
            .context("managed route plan host crates must be an object")?;
        for (field, present) in [
            ("wasm", wasm || mini_program),
            ("napi", node || electron),
            ("ohos", harmony),
        ] {
            if present {
                check(
                    &format!("/hostCrates/{field}"),
                    &expected_host_crates[field],
                    &format!("hostCrates.{field}"),
                )?;
            }
        }
        Ok(())
    }
}

fn expanded_targets_from_managed_manifest(manifest: &serde_json::Value) -> Result<ExpandedTargets> {
    let mut targets = ExpandedTargets::default();
    for target in manifest
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .context("managed artifact manifest lacks targets while planning routes")?
    {
        match target
            .as_str()
            .context("managed artifact manifest target must be a string while planning routes")?
        {
            "wasm" => targets.wasm = true,
            "mini-program" => targets.mini_program = true,
            "node" => targets.node = true,
            "electron" => targets.electron = true,
            "harmony" => targets.harmony = true,
            "apple" => targets.apple = true,
            "android" => targets.android = true,
            target => bail!("managed artifact manifest has unsupported target `{target}`"),
        }
    }
    Ok(targets)
}

impl ManagedTransactionLayout for ManagedLayout {
    fn package_root(&self) -> &Utf8Path {
        &self.package_dir
    }

    fn preflight_existing_package(&self) -> Result<()> {
        let Some(host_identity) = self.host_identity.as_ref() else {
            return Ok(());
        };
        let Some(existing) = read_existing_managed_manifest_components(self, host_identity)? else {
            return Ok(());
        };
        let existing_components = &existing.components;
        if self.components_authoritative {
            let planned = self.exact_components()?;
            if !same_managed_component_keys(planned, existing_components) {
                bail!(
                    "managed artifact component set mismatch between authoritative planned metadata and existing manifest: expected {planned:?}, got {existing_components:?}"
                );
            }
            if planned != existing_components {
                let requested = self.expected_routes.as_ref().context(
                    "managed artifact interface ABI changed without an authoritative target plan",
                )?;
                if !requested.targets.contains(&existing.targets) {
                    bail!(
                        "managed artifact component interface ABI digest changed; refusing a partial target update that could mix new common bindings with retained sibling backends/native artifacts. Rebuild the complete existing target union (or use --target all)"
                    );
                }
            }
            let generated_components =
                self.generated_component_identities_with_fallback(Some(existing_components))?;
            if let Some(generated) = generated_components.as_ref() {
                if planned != generated {
                    bail!(
                        "managed artifact component set mismatch between authoritative planned metadata and generated bridges: expected {planned:?}, got {generated:?}"
                    );
                }
            }
            return Ok(());
        }

        // `build` always installs a source/library plan before reaching this
        // point. Keep this branch only for direct test fixtures and legacy
        // internal callers that construct a layout by hand; it must never be
        // a production path that adopts an existing package.
        let generated_components =
            self.generated_component_identities_with_fallback(Some(existing_components))?;
        if let Some(generated) = generated_components {
            if !same_managed_component_keys(&generated, existing_components) {
                bail!(
                    "managed artifact component set mismatch between existing manifest and generated bridges: manifest {existing_components:?}, bridges {generated:?}"
                );
            }
        }
        Ok(())
    }
}

/// A nullable field that must still be present in the serialized manifest.
/// `Option<T>` would silently accept a missing key, which is an old-schema
/// compatibility path rather than a legitimate target-specific absence.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManifestNullable<T> {
    Value(T),
    // A unit variant is serde's exact JSON `null` representation.  Keeping
    // this distinct from `Option<T>` preserves the surrounding struct's
    // required-key check: omitted fields remain a schema error.
    Null,
}

impl<T> ManifestNullable<T> {
    fn is_value(&self) -> bool {
        matches!(self, Self::Value(_))
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifest {
    artifact_manifest_schema_version: u64,
    generator: String,
    components: Vec<ManagedArtifactManifestComponent>,
    host_composite_identity: String,
    targets: Vec<String>,
    source: ManagedArtifactManifestSource,
    entrypoints: ManagedArtifactManifestEntrypoints,
    artifacts: ManagedArtifactManifestArtifacts,
    host_crates: ManagedArtifactManifestHostCrates,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestSource {
    root: String,
    shared: ManifestNullable<String>,
    browser: ManifestNullable<String>,
    node: ManifestNullable<String>,
    electron: ManifestNullable<String>,
    harmony: ManifestNullable<String>,
    swift: ManifestNullable<String>,
    kotlin: ManifestNullable<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestComponent {
    component: String,
    namespace: String,
    native_export_prefix: String,
    interface_abi_digest: String,
    source: ManagedArtifactManifestComponentSource,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestComponentSource {
    common: ManifestNullable<String>,
    browser: ManifestNullable<String>,
    node: ManifestNullable<String>,
    electron: ManifestNullable<String>,
    harmony: ManifestNullable<String>,
    public_types: ManifestNullable<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestEntrypoints {
    web: ManifestNullable<String>,
    mini_program: ManifestNullable<String>,
    node: ManifestNullable<String>,
    electron: ManifestNullable<String>,
    harmony: ManifestNullable<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestArtifacts {
    wasm: ManifestNullable<ManagedArtifactManifestWasm>,
    mini_program: ManifestNullable<ManagedArtifactManifestMiniProgram>,
    node: ManifestNullable<ManagedArtifactManifestAddon>,
    electron: ManifestNullable<ManagedArtifactManifestAddon>,
    harmony: ManifestNullable<ManagedArtifactManifestHarmony>,
    apple: ManifestNullable<ManagedArtifactManifestApple>,
    android: ManifestNullable<ManagedArtifactManifestAndroid>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestWasm {
    glue: String,
    wasm: String,
    dts: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestMiniProgram {
    glue: String,
    wasm: String,
    default_wasm_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestAddon {
    addon: String,
    env: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmony {
    kind: String,
    integrated: bool,
    har: ManifestNullable<String>,
    runtime_hsp: ManifestNullable<String>,
    interface_har: ManifestNullable<String>,
    tgz: ManifestNullable<String>,
    dist: String,
    facade: String,
    facade_contract: String,
    package_facade_contract: ManifestNullable<String>,
    types: String,
    package: ManifestNullable<String>,
    module_project: ManifestNullable<String>,
    module_source: ManifestNullable<String>,
    usage: ManifestNullable<String>,
    package_metadata: ManifestNullable<String>,
    module_metadata: ManifestNullable<String>,
    build_profile: ManifestNullable<String>,
    metadata: ManifestNullable<ManagedArtifactManifestHarmonyMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ManagedArtifactManifestHarmonyMetadata {
    // `render_oh_package_json5` / `render_build_profile_json5` write this
    // complete staged shape during a real Harmony package publication.
    Staged(ManagedArtifactManifestHarmonyStagedMetadata),
    // `render_manifest_with_read_roots` also has a no-staged-package fallback
    // used before Harmony package files exist.  It is a distinct current
    // producer shape, not a permissive legacy fallback.
    Fallback(ManagedArtifactManifestHarmonyFallbackMetadata),
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyStagedMetadata {
    package: ManagedArtifactManifestHarmonyStagedPackageMetadata,
    module: ManagedArtifactManifestHarmonyModuleMetadata,
    build_profile: ManagedArtifactManifestHarmonyStagedBuildProfile,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyFallbackMetadata {
    package: ManagedArtifactManifestHarmonyFallbackPackageMetadata,
    module: ManagedArtifactManifestHarmonyModuleMetadata,
    build_profile: ManagedArtifactManifestHarmonyFallbackBuildProfile,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyStagedPackageMetadata {
    name: String,
    version: String,
    main: String,
    types: String,
    #[serde(rename = "packageType")]
    package_type: Option<String>,
    description: Option<String>,
    author: Option<String>,
    license: Option<String>,
    dependencies: BTreeMap<String, String>,
    compatible_sdk_version: Option<String>,
    compatible_sdk_type: Option<String>,
    native_components: Option<Vec<ManagedArtifactManifestHarmonyNativeComponent>>,
    obfuscated: bool,
    artifact_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyFallbackPackageMetadata {
    name: String,
    version: String,
    main: String,
    #[serde(rename = "packageType")]
    package_type: Option<String>,
    description: Option<String>,
    author: Option<String>,
    license: Option<String>,
    compatible_sdk_version: Option<String>,
    compatible_sdk_type: Option<String>,
    obfuscated: bool,
    artifact_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyNativeComponent {
    name: String,
    compatible_sdk_version: String,
    compatible_sdk_type: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyModuleMetadata {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    device_types: Vec<String>,
    delivery_with_install: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyFallbackBuildProfile {
    api_type: String,
    build_option: Option<ManagedArtifactManifestHarmonyFallbackBuildOption>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyFallbackBuildOption {
    generate_shared_tgz: Option<bool>,
    native_lib: Option<ManagedArtifactManifestHarmonyNativeLib>,
    ark_options: Option<ManagedArtifactManifestHarmonyArkOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyStagedBuildProfile {
    api_type: String,
    build_option: ManagedArtifactManifestHarmonyStagedBuildOption,
    targets: Vec<ManagedArtifactManifestHarmonyBuildTarget>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyStagedBuildOption {
    res_options: ManagedArtifactManifestHarmonyResOptions,
    generate_shared_tgz: Option<bool>,
    native_lib: Option<ManagedArtifactManifestHarmonyNativeLib>,
    ark_options: Option<ManagedArtifactManifestHarmonyArkOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyResOptions {
    copy_code_resource: ManagedArtifactManifestHarmonyCopyCodeResource,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyCopyCodeResource {
    enable: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyBuildTarget {
    name: String,
    config: ManagedArtifactManifestHarmonyBuildTargetConfig,
    // The generated Harmony profile spells its acronym as `runtimeOS`, not
    // serde's default `runtimeOs` camel-case conversion.
    #[serde(rename = "runtimeOS")]
    runtime_os: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyBuildTargetConfig {
    device_type: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyNativeLib {
    exclude_so_from_interface_har: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHarmonyArkOptions {
    integrated_hsp: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestApple {
    xcframework: String,
    package: String,
    product: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestAndroid {
    jni_libs: String,
    aar: ManifestNullable<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedArtifactManifestHostCrates {
    wasm: ManifestNullable<String>,
    napi: ManifestNullable<String>,
    ohos: ManifestNullable<String>,
}

const ARTIFACT_MANIFEST_SCHEMA_VERSION: u64 = 4;

fn artifact_manifest_version_diagnostic(value: &serde_json::Value) -> String {
    match value.get("artifactManifestSchemaVersion") {
        Some(serde_json::Value::Number(number)) => number.to_string(),
        Some(serde_json::Value::String(value)) => format!("`{value}`"),
        Some(_) => "a non-integer value".to_string(),
        None => "missing".to_string(),
    }
}

fn validate_manifest_relative_path(path: &str, label: &str) -> Result<()> {
    let path = Utf8Path::new(path);
    if path.as_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| matches!(component.as_str(), "" | "." | ".."))
    {
        bail!("managed artifact manifest `{label}` has an unsafe path `{path}`");
    }
    Ok(())
}

fn validate_manifest_nullable_path(value: &ManifestNullable<String>, label: &str) -> Result<()> {
    if let ManifestNullable::Value(path) = value {
        validate_manifest_relative_path(path, label)?;
    }
    Ok(())
}

fn validate_manifest_harmony_metadata(
    value: &ManifestNullable<ManagedArtifactManifestHarmonyMetadata>,
    kind: &str,
    integrated: bool,
) -> Result<()> {
    let ManifestNullable::Value(value) = value else {
        return Ok(());
    };
    let validate_package = |name: &str,
                            version: &str,
                            main: &str,
                            package_type: Option<&str>,
                            description: Option<&str>,
                            author: Option<&str>,
                            license: Option<&str>,
                            compatible_sdk_version: Option<&str>,
                            compatible_sdk_type: Option<&str>,
                            artifact_type: &str|
     -> Result<()> {
        if name.is_empty()
            || version.is_empty()
            || main.is_empty()
            || artifact_type.is_empty()
            || package_type.is_some_and(str::is_empty)
            || description.is_some_and(str::is_empty)
            || author.is_some_and(str::is_empty)
            || license.is_some_and(str::is_empty)
            || compatible_sdk_version.is_some_and(str::is_empty)
            || compatible_sdk_type.is_some_and(str::is_empty)
            || compatible_sdk_version.is_some() != compatible_sdk_type.is_some()
        {
            bail!("managed artifact manifest Harmony package metadata is invalid");
        }
        Ok(())
    };
    let validate_module = |module: &ManagedArtifactManifestHarmonyModuleMetadata| -> Result<()> {
        if module.name.is_empty()
            || module.kind.is_empty()
            || module.device_types.is_empty()
            || module.device_types.iter().any(|value| value.is_empty())
        {
            bail!("managed artifact manifest Harmony module metadata is invalid");
        }
        Ok(())
    };
    let validate_fallback_option = |option: &ManagedArtifactManifestHarmonyFallbackBuildOption| {
        if let Some(native) = &option.native_lib {
            let _ = native.exclude_so_from_interface_har;
        }
        if let Some(ark) = &option.ark_options {
            let _ = ark.integrated_hsp;
        }
        let _ = option.generate_shared_tgz;
    };

    match value {
        ManagedArtifactManifestHarmonyMetadata::Fallback(value) => {
            let package = &value.package;
            validate_package(
                &package.name,
                &package.version,
                &package.main,
                package.package_type.as_deref(),
                package.description.as_deref(),
                package.author.as_deref(),
                package.license.as_deref(),
                package.compatible_sdk_version.as_deref(),
                package.compatible_sdk_type.as_deref(),
                &package.artifact_type,
            )?;
            validate_module(&value.module)?;
            if value.build_profile.api_type.is_empty() {
                bail!("managed artifact manifest Harmony fallback build profile is invalid");
            }
            if let Some(option) = &value.build_profile.build_option {
                validate_fallback_option(option);
            }
            validate_harmony_metadata_target_shape(
                &package.package_type,
                &value.module,
                value.build_profile.build_option.as_ref().map(|option| {
                    (
                        option.generate_shared_tgz,
                        option.ark_options.as_ref().map(|ark| ark.integrated_hsp),
                    )
                }),
                kind,
                integrated,
            )?;
            let _ = package.obfuscated;
            let _ = value.module.delivery_with_install;
        }
        ManagedArtifactManifestHarmonyMetadata::Staged(value) => {
            let package = &value.package;
            validate_package(
                &package.name,
                &package.version,
                &package.main,
                package.package_type.as_deref(),
                package.description.as_deref(),
                package.author.as_deref(),
                package.license.as_deref(),
                package.compatible_sdk_version.as_deref(),
                package.compatible_sdk_type.as_deref(),
                &package.artifact_type,
            )?;
            if package.types.is_empty()
                || package.dependencies.is_empty()
                || package
                    .dependencies
                    .iter()
                    .any(|(name, path)| name.is_empty() || path.is_empty())
                || package.compatible_sdk_version.is_some() != package.native_components.is_some()
            {
                bail!("managed artifact manifest staged Harmony package metadata is invalid");
            }
            if let Some(components) = &package.native_components {
                if components.is_empty()
                    || components.iter().any(|component| {
                        component.name.is_empty()
                            || component.compatible_sdk_version.is_empty()
                            || component.compatible_sdk_type.is_empty()
                    })
                {
                    bail!("managed artifact manifest staged Harmony native components are invalid");
                }
            }
            validate_module(&value.module)?;
            let profile = &value.build_profile;
            if profile.api_type.is_empty()
                || profile.targets.is_empty()
                || profile.targets.iter().any(|target| {
                    target.name.is_empty()
                        || target.config.device_type.is_empty()
                        || target
                            .config
                            .device_type
                            .iter()
                            .any(|device| device.is_empty())
                        || target.runtime_os.as_deref().is_some_and(str::is_empty)
                })
            {
                bail!("managed artifact manifest staged Harmony build profile is invalid");
            }
            let option = &profile.build_option;
            let _ = option.res_options.copy_code_resource.enable;
            if let Some(native) = &option.native_lib {
                let _ = native.exclude_so_from_interface_har;
            }
            if let Some(ark) = &option.ark_options {
                let _ = ark.integrated_hsp;
            }
            let _ = option.generate_shared_tgz;
            validate_harmony_metadata_target_shape(
                &package.package_type,
                &value.module,
                Some((
                    option.generate_shared_tgz,
                    option.ark_options.as_ref().map(|ark| ark.integrated_hsp),
                )),
                kind,
                integrated,
            )?;
            let _ = package.obfuscated;
            let _ = value.module.delivery_with_install;
        }
    }
    Ok(())
}

fn validate_harmony_metadata_target_shape(
    package_type: &Option<String>,
    module: &ManagedArtifactManifestHarmonyModuleMetadata,
    build_flags: Option<(Option<bool>, Option<bool>)>,
    kind: &str,
    integrated: bool,
) -> Result<()> {
    let hsp = kind == "hsp";
    let har = kind == "har";
    if !hsp && !har {
        bail!("managed artifact manifest has Harmony metadata for a non-package target");
    }
    let expected_package_type = hsp.then_some("InterfaceHar");
    let expected_module_kind = if hsp { "shared" } else { "har" };
    if package_type.as_deref() != expected_package_type
        || module.kind != expected_module_kind
        || module.delivery_with_install != hsp.then_some(true)
    {
        bail!("managed artifact manifest Harmony metadata does not match its target kind");
    }
    match (hsp, build_flags) {
        (true, Some((Some(true), ark_integrated)))
            if ark_integrated == integrated.then_some(true) => {}
        (false, Some((None, None))) | (false, None) => {}
        _ => {
            bail!("managed artifact manifest Harmony build metadata does not match its target kind")
        }
    }
    Ok(())
}

fn validate_manifest_harmony_artifact_shape(value: &ManagedArtifactManifestHarmony) -> Result<()> {
    let exact_presence = |actual: bool, expected: bool, label: &str| -> Result<()> {
        if actual != expected {
            bail!(
                "managed artifact manifest Harmony field `{label}` does not match its target kind"
            );
        }
        Ok(())
    };
    let hsp = value.kind == "hsp";
    let har = value.kind == "har";
    let package = hsp || har;
    exact_presence(value.har.is_value(), har, "har")?;
    exact_presence(value.runtime_hsp.is_value(), hsp, "runtimeHsp")?;
    exact_presence(value.interface_har.is_value(), hsp, "interfaceHar")?;
    exact_presence(value.tgz.is_value(), hsp, "tgz")?;
    exact_presence(
        value.package_facade_contract.is_value(),
        package,
        "packageFacadeContract",
    )?;
    exact_presence(value.package.is_value(), package, "package")?;
    exact_presence(value.module_project.is_value(), hsp, "moduleProject")?;
    exact_presence(value.module_source.is_value(), hsp, "moduleSource")?;
    exact_presence(value.usage.is_value(), hsp, "usage")?;
    exact_presence(
        value.package_metadata.is_value(),
        package,
        "packageMetadata",
    )?;
    exact_presence(value.module_metadata.is_value(), package, "moduleMetadata")?;
    exact_presence(value.build_profile.is_value(), package, "buildProfile")?;
    exact_presence(value.metadata.is_value(), package, "metadata")?;
    validate_manifest_harmony_metadata(&value.metadata, &value.kind, value.integrated)
}

fn manifest_component_identities(
    components: &[ManagedArtifactManifestComponent],
) -> Result<Vec<ManagedComponentIdentity>> {
    if components.is_empty() {
        bail!("managed artifact manifest has no components");
    }
    let mut identities = Vec::with_capacity(components.len());
    for component in components {
        let identity = ManagedComponentIdentity::with_interface_abi_digest(
            &component.component,
            &component.namespace,
            &component.interface_abi_digest,
        )?;
        if component.native_export_prefix != identity.native_export_prefix {
            bail!(
                "managed artifact manifest component `{}` namespace `{}` has an invalid native export prefix `{}`",
                component.component,
                component.namespace,
                component.native_export_prefix
            );
        }
        identities.push(identity);
    }
    let canonical = canonical_managed_component_identities(identities.clone())?;
    if identities != canonical {
        bail!("managed artifact manifest components are not in canonical order");
    }
    Ok(identities)
}

fn validate_exact_nullable_route(
    value: &ManifestNullable<String>,
    expected_present: bool,
    expected_path: &str,
    label: &str,
) -> Result<()> {
    validate_manifest_nullable_path(value, label)?;
    match (value, expected_present) {
        (ManifestNullable::Value(actual), true) if actual == expected_path => Ok(()),
        (ManifestNullable::Null, false) => Ok(()),
        (ManifestNullable::Value(actual), true) => bail!(
            "managed artifact manifest `{label}` route mismatch: expected `{expected_path}`, got `{actual}`"
        ),
        (ManifestNullable::Value(actual), false) => bail!(
            "managed artifact manifest `{label}` must be null, got `{actual}`"
        ),
        (ManifestNullable::Null, true) => bail!(
            "managed artifact manifest `{label}` is missing its required route `{expected_path}`"
        ),
    }
}

fn validate_exact_managed_artifact_manifest(
    manifest: &ManagedArtifactManifest,
    expected_components: Option<&[ManagedComponentIdentity]>,
    host_identity: Option<&ManagedHostIdentity>,
) -> Result<()> {
    if manifest.artifact_manifest_schema_version != ARTIFACT_MANIFEST_SCHEMA_VERSION {
        bail!(
            "managed artifact manifest schema mismatch: expected {}, got {}",
            ARTIFACT_MANIFEST_SCHEMA_VERSION,
            manifest.artifact_manifest_schema_version
        );
    }
    if manifest.generator != "uniffi-bindgen-javascript" {
        bail!("managed artifact manifest has an unexpected generator");
    }
    let component_identities = manifest_component_identities(&manifest.components)?;
    if let Some(expected_components) = expected_components {
        if component_identities != expected_components {
            bail!(
                "managed artifact manifest component set mismatch: expected {expected_components:?}, got {component_identities:?}"
            );
        }
    }
    validate_sha256_digest(&manifest.host_composite_identity, "host composite identity")?;
    if let Some(host_identity) = host_identity {
        let expected = host_identity.composite_identity(&component_identities)?;
        if manifest.host_composite_identity != expected {
            bail!(
                "managed artifact manifest host composite identity mismatch: expected `{expected}`, got `{}`",
                manifest.host_composite_identity
            );
        }
    }
    validate_manifest_relative_path(&manifest.source.root, "source.root")?;
    for (label, value) in [
        ("source.shared", &manifest.source.shared),
        ("source.browser", &manifest.source.browser),
        ("source.node", &manifest.source.node),
        ("source.electron", &manifest.source.electron),
        ("source.harmony", &manifest.source.harmony),
        ("source.swift", &manifest.source.swift),
        ("source.kotlin", &manifest.source.kotlin),
        ("entrypoints.web", &manifest.entrypoints.web),
        (
            "entrypoints.miniProgram",
            &manifest.entrypoints.mini_program,
        ),
        ("entrypoints.node", &manifest.entrypoints.node),
        ("entrypoints.electron", &manifest.entrypoints.electron),
        ("entrypoints.harmony", &manifest.entrypoints.harmony),
        ("hostCrates.wasm", &manifest.host_crates.wasm),
        ("hostCrates.napi", &manifest.host_crates.napi),
        ("hostCrates.ohos", &manifest.host_crates.ohos),
    ] {
        validate_manifest_nullable_path(value, label)?;
    }
    match &manifest.artifacts.wasm {
        ManifestNullable::Value(value) => {
            validate_manifest_relative_path(&value.glue, "artifacts.wasm.glue")?;
            validate_manifest_relative_path(&value.wasm, "artifacts.wasm.wasm")?;
            validate_manifest_relative_path(&value.dts, "artifacts.wasm.dts")?;
        }
        ManifestNullable::Null => {}
    }
    match &manifest.artifacts.mini_program {
        ManifestNullable::Value(value) => {
            validate_manifest_relative_path(&value.glue, "artifacts.miniProgram.glue")?;
            validate_manifest_relative_path(&value.wasm, "artifacts.miniProgram.wasm")?;
            if !value.default_wasm_path.starts_with('/') || value.default_wasm_path.contains("..") {
                bail!("managed artifact manifest Mini Program default wasm path is invalid");
            }
        }
        ManifestNullable::Null => {}
    }
    for (label, value) in [
        ("artifacts.node", &manifest.artifacts.node),
        ("artifacts.electron", &manifest.artifacts.electron),
    ] {
        if let ManifestNullable::Value(value) = value {
            validate_manifest_relative_path(&value.addon, label)?;
            if value.env.is_empty() {
                bail!("managed artifact manifest `{label}` has an empty environment key");
            }
        }
    }
    if let ManifestNullable::Value(value) = &manifest.artifacts.harmony {
        if !matches!(value.kind.as_str(), "dist" | "har" | "hsp")
            || (value.integrated && value.kind != "hsp")
        {
            bail!("managed artifact manifest Harmony artifact kind is invalid");
        }
        validate_manifest_relative_path(&value.dist, "artifacts.harmony.dist")?;
        validate_manifest_relative_path(&value.facade, "artifacts.harmony.facade")?;
        validate_manifest_relative_path(
            &value.facade_contract,
            "artifacts.harmony.facadeContract",
        )?;
        validate_manifest_relative_path(&value.types, "artifacts.harmony.types")?;
        for (label, value) in [
            ("artifacts.harmony.har", &value.har),
            ("artifacts.harmony.runtimeHsp", &value.runtime_hsp),
            ("artifacts.harmony.interfaceHar", &value.interface_har),
            ("artifacts.harmony.tgz", &value.tgz),
            (
                "artifacts.harmony.packageFacadeContract",
                &value.package_facade_contract,
            ),
            ("artifacts.harmony.package", &value.package),
            ("artifacts.harmony.moduleProject", &value.module_project),
            ("artifacts.harmony.moduleSource", &value.module_source),
            ("artifacts.harmony.usage", &value.usage),
            ("artifacts.harmony.packageMetadata", &value.package_metadata),
            ("artifacts.harmony.moduleMetadata", &value.module_metadata),
            ("artifacts.harmony.buildProfile", &value.build_profile),
        ] {
            validate_manifest_nullable_path(value, label)?;
        }
        validate_manifest_harmony_artifact_shape(value)?;
    }
    if let ManifestNullable::Value(value) = &manifest.artifacts.apple {
        validate_manifest_relative_path(&value.xcframework, "artifacts.apple.xcframework")?;
        validate_manifest_relative_path(&value.package, "artifacts.apple.package")?;
        if value.product.is_empty() {
            bail!("managed artifact manifest Apple product is empty");
        }
    }
    if let ManifestNullable::Value(value) = &manifest.artifacts.android {
        validate_manifest_relative_path(&value.jni_libs, "artifacts.android.jniLibs")?;
        validate_manifest_nullable_path(&value.aar, "artifacts.android.aar")?;
    }

    let canonical_targets = [
        "wasm",
        "mini-program",
        "node",
        "electron",
        "harmony",
        "apple",
        "android",
    ];
    let targets = manifest
        .targets
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    if targets.len() != manifest.targets.len()
        || manifest
            .targets
            .iter()
            .any(|target| !canonical_targets.contains(&target.as_str()))
        || manifest.targets
            != canonical_targets
                .into_iter()
                .filter(|target| targets.contains(target))
                .map(str::to_string)
                .collect::<Vec<_>>()
    {
        bail!("managed artifact manifest targets are invalid or non-canonical");
    }
    let has = |target| targets.contains(target);
    let has_js =
        has("wasm") || has("mini-program") || has("node") || has("electron") || has("harmony");
    let exact_presence = |actual: bool, expected: bool, label: &str| -> Result<()> {
        if actual != expected {
            bail!(
                "managed artifact manifest target section `{label}` is inconsistent with targets"
            );
        }
        Ok(())
    };
    let has_browser = has("wasm") || has("mini-program");
    validate_exact_nullable_route(
        &manifest.source.shared,
        has_js,
        &format!("{}/shared", manifest.source.root),
        "source.shared",
    )?;
    validate_exact_nullable_route(
        &manifest.source.browser,
        has_browser,
        &format!("{}/browser", manifest.source.root),
        "source.browser",
    )?;
    validate_exact_nullable_route(
        &manifest.source.node,
        has("node"),
        &format!("{}/node", manifest.source.root),
        "source.node",
    )?;
    validate_exact_nullable_route(
        &manifest.source.electron,
        has("electron"),
        &format!("{}/electron", manifest.source.root),
        "source.electron",
    )?;
    validate_exact_nullable_route(
        &manifest.source.harmony,
        has("harmony"),
        &format!("{}/harmony", manifest.source.root),
        "source.harmony",
    )?;
    validate_exact_nullable_route(
        &manifest.source.swift,
        has("apple"),
        &format!("{}/swift", manifest.source.root),
        "source.swift",
    )?;
    validate_exact_nullable_route(
        &manifest.source.kotlin,
        has("android"),
        &format!("{}/kotlin", manifest.source.root),
        "source.kotlin",
    )?;
    for (component, identity) in manifest.components.iter().zip(&component_identities) {
        let component_root = format!("{}/components/{}", manifest.source.root, identity.namespace);
        validate_exact_nullable_route(
            &component.source.common,
            has_js,
            &format!("{component_root}/common"),
            &format!("components.{}.source.common", identity.namespace),
        )?;
        validate_exact_nullable_route(
            &component.source.public_types,
            has_js,
            &format!("{component_root}/common/public-types.ts"),
            &format!("components.{}.source.publicTypes", identity.namespace),
        )?;
        validate_exact_nullable_route(
            &component.source.browser,
            has_browser,
            &format!("{component_root}/browser"),
            &format!("components.{}.source.browser", identity.namespace),
        )?;
        validate_exact_nullable_route(
            &component.source.node,
            has("node"),
            &format!("{component_root}/node"),
            &format!("components.{}.source.node", identity.namespace),
        )?;
        validate_exact_nullable_route(
            &component.source.electron,
            has("electron"),
            &format!("{component_root}/electron"),
            &format!("components.{}.source.electron", identity.namespace),
        )?;
        validate_exact_nullable_route(
            &component.source.harmony,
            has("harmony"),
            &format!("{component_root}/harmony"),
            &format!("components.{}.source.harmony", identity.namespace),
        )?;
    }
    exact_presence(
        manifest.entrypoints.web.is_value(),
        has("wasm"),
        "entrypoints.web",
    )?;
    exact_presence(
        manifest.entrypoints.mini_program.is_value(),
        has("mini-program"),
        "entrypoints.miniProgram",
    )?;
    exact_presence(
        manifest.entrypoints.node.is_value(),
        has("node"),
        "entrypoints.node",
    )?;
    exact_presence(
        manifest.entrypoints.electron.is_value(),
        has("electron"),
        "entrypoints.electron",
    )?;
    exact_presence(
        manifest.entrypoints.harmony.is_value(),
        has("harmony"),
        "entrypoints.harmony",
    )?;
    exact_presence(
        manifest.artifacts.wasm.is_value(),
        has("wasm"),
        "artifacts.wasm",
    )?;
    exact_presence(
        manifest.artifacts.mini_program.is_value(),
        has("mini-program"),
        "artifacts.miniProgram",
    )?;
    exact_presence(
        manifest.artifacts.node.is_value(),
        has("node"),
        "artifacts.node",
    )?;
    exact_presence(
        manifest.artifacts.electron.is_value(),
        has("electron"),
        "artifacts.electron",
    )?;
    exact_presence(
        manifest.artifacts.harmony.is_value(),
        has("harmony"),
        "artifacts.harmony",
    )?;
    exact_presence(
        manifest.artifacts.apple.is_value(),
        has("apple"),
        "artifacts.apple",
    )?;
    exact_presence(
        manifest.artifacts.android.is_value(),
        has("android"),
        "artifacts.android",
    )?;
    exact_presence(
        manifest.host_crates.wasm.is_value(),
        has("wasm") || has("mini-program"),
        "hostCrates.wasm",
    )?;
    exact_presence(
        manifest.host_crates.napi.is_value(),
        has("node") || has("electron"),
        "hostCrates.napi",
    )?;
    exact_presence(
        manifest.host_crates.ohos.is_value(),
        has("harmony"),
        "hostCrates.ohos",
    )?;
    Ok(())
}

#[cfg(test)]
fn parse_exact_managed_artifact_manifest(
    bytes: &[u8],
    expected_components: Option<&[ManagedComponentIdentity]>,
    host_identity: Option<&ManagedHostIdentity>,
    label: &str,
) -> Result<(ManagedArtifactManifest, serde_json::Value)> {
    parse_exact_managed_artifact_manifest_with_routes(
        bytes,
        expected_components,
        host_identity,
        None,
        false,
        label,
    )
}

fn parse_exact_managed_artifact_manifest_with_routes(
    bytes: &[u8],
    expected_components: Option<&[ManagedComponentIdentity]>,
    host_identity: Option<&ManagedHostIdentity>,
    route_plan: Option<&ManagedArtifactRoutePlan>,
    strict_current_targets: bool,
    label: &str,
) -> Result<(ManagedArtifactManifest, serde_json::Value)> {
    let raw: serde_json::Value =
        serde_json::from_slice(bytes).with_context(|| format!("parsing {label}"))?;
    if raw
        .get("artifactManifestSchemaVersion")
        .and_then(serde_json::Value::as_u64)
        != Some(ARTIFACT_MANIFEST_SCHEMA_VERSION)
    {
        bail!(
            "{label} schema mismatch: expected {}, got {}",
            ARTIFACT_MANIFEST_SCHEMA_VERSION,
            artifact_manifest_version_diagnostic(&raw)
        );
    }
    let typed: ManagedArtifactManifest =
        serde_json::from_slice(bytes).with_context(|| format!("parsing exact {label}"))?;
    validate_exact_managed_artifact_manifest(&typed, expected_components, host_identity)?;
    if let Some(route_plan) = route_plan {
        route_plan.validate_manifest_routes(&raw, strict_current_targets)?;
    }
    Ok((typed, raw))
}

struct ExistingManagedManifestIdentity {
    components: Vec<ManagedComponentIdentity>,
    targets: ExpandedTargets,
}

fn read_existing_managed_manifest_components(
    layout: &ManagedLayout,
    host_identity: &ManagedHostIdentity,
) -> Result<Option<ExistingManagedManifestIdentity>> {
    if !path_entry_exists(&layout.package_dir)? {
        return Ok(None);
    }
    let bytes = super::artifact_transaction::read_verified_regular_file_bounded(
        &layout.manifest_path,
        16 * 1024 * 1024,
        "existing managed artifact manifest",
    )?;
    let (typed, raw) = parse_exact_managed_artifact_manifest_with_routes(
        &bytes,
        // Read the actual identities first.  The authoritative/current plan
        // comparison belongs to `preflight_existing_package`, after parsing,
        // so incremental direct fixtures retain their established diagnostic.
        None,
        Some(host_identity),
        // An existing manifest must prove its own declared historical routes
        // before a current invocation is allowed to replace any target. In
        // particular, Harmony's package kind is dynamic, so validating this
        // read against the current HAR/HSP plan would make a valid transition
        // look like an existing-manifest route mismatch.
        None,
        false,
        "existing managed artifact manifest",
    )?;
    if let Some(declared_plan) = layout.historical_manifest_declared_route_plan(&raw)? {
        // This is deliberately independent from the current invocation. The
        // immutable declared-target union protects every route retained from
        // the existing generation, including a Harmony HSP/HAR shape that a
        // new invocation is about to replace.
        declared_plan.validate_manifest_routes(&raw, true)?;
    }
    validate_managed_manifest_paths(layout, &raw)?;
    Ok(Some(ExistingManagedManifestIdentity {
        components: manifest_component_identities(&typed.components)?,
        targets: expanded_targets_from_managed_manifest(&raw)?,
    }))
}

#[cfg(test)]
fn validate_existing_managed_manifest(
    layout: &ManagedLayout,
    expected_components: &[ManagedComponentIdentity],
    host_identity: &ManagedHostIdentity,
) -> Result<()> {
    let Some(actual) = read_existing_managed_manifest_components(layout, host_identity)? else {
        return Ok(());
    };
    if actual.components != expected_components {
        bail!(
            "managed artifact manifest component set mismatch: expected {expected_components:?}, got {:?}",
            actual.components
        );
    }
    Ok(())
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
    layout: &ManagedLayout,
    targets: &ExpandedTargets,
) -> Result<()> {
    let mut paths = Vec::<String>::new();
    let has_browser = targets.wasm || targets.mini_program;
    let has_napi = targets.node || targets.electron;
    let has_js = has_browser || has_napi || targets.harmony;

    // A managed candidate is seeded byte-for-byte from the previous owned
    // generation.  Remove every selected generator-owned route through the
    // transaction snapshot before invoking a generator: overwriting a seeded
    // path would retain stale files, and creation-time guards (notably the
    // Mini Program snippets copier) must never treat an old pathname as one
    // created by the current invocation.  Shared/common routes are rebuilt by
    // every JavaScript flavor, while unselected flavor routes remain seeded.
    if has_js {
        paths.push("src/ffi/shared".into());
        for component in layout.exact_components()? {
            let root = format!("src/ffi/components/{}", component.namespace);
            paths.push(format!("{root}/common"));
            if has_browser {
                paths.push(format!("{root}/browser"));
            }
            if targets.node {
                paths.push(format!("{root}/node"));
            }
            if targets.electron {
                paths.push(format!("{root}/electron"));
            }
            if targets.harmony {
                paths.push(format!("{root}/harmony"));
            }
        }
    }
    if has_browser {
        // The browser namespace root is shared by Web and Mini Program.
        // build_wasm always regenerates index.ts and index.web.ts, including
        // for a Mini-Program-only request, while the Mini entry is selected
        // independently and must survive an unselected Web-only refresh.
        paths.extend([
            "src/ffi/browser/index.ts".into(),
            "src/ffi/browser/index.web.ts".into(),
            "artifacts/browser".into(),
            "artifacts/rust/wasm".into(),
        ]);
    }
    if targets.wasm {
        paths.push("src/index.web.ts".into());
    }
    if targets.mini_program {
        paths.extend([
            "src/ffi/browser/index.mini-program.ts".into(),
            "src/index.mini-program.ts".into(),
            "artifacts/mini-program".into(),
        ]);
    }
    if has_napi {
        // Node and Electron intentionally share one composite N-API host and
        // one addon publication route.
        paths.extend(["artifacts/node".into(), "artifacts/rust/napi".into()]);
    }
    if targets.node {
        paths.extend(["src/ffi/node".into(), "src/index.node.ts".into()]);
    }
    if targets.electron {
        paths.extend(["src/ffi/electron".into(), "src/index.electron.ts".into()]);
    }
    if targets.harmony {
        paths.extend([
            "src/ffi/harmony".into(),
            "artifacts/harmony".into(),
            "artifacts/rust/ohos".into(),
        ]);
    }
    if targets.apple {
        paths.extend([
            "artifacts/apple".into(),
            "src/ffi/swift".into(),
            "src/ffi/apple".into(),
        ]);
    }
    if targets.android {
        paths.extend([
            "artifacts/android".into(),
            "src/ffi/kotlin".into(),
            "src/ffi/android".into(),
        ]);
    }
    let paths = paths.iter().map(String::as_str).collect::<Vec<_>>();
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
    fn exact_components(&self) -> Result<&[ManagedComponentIdentity]> {
        self.components
            .as_deref()
            .context("managed artifact layout lacks planned component identities")
    }

    /// Build the sole canonical route plan used by both manifest rendering
    /// and exact manifest validation.  It deliberately derives every path
    /// from managed layout state and invocation arguments, never from a
    /// manifest being checked or from a same-typed file found on disk.
    fn managed_artifact_route_plan(
        &self,
        targets: &ExpandedTargets,
        inputs: &ManagedArtifactRouteInputs,
        artifact_read_root: Option<&Utf8Path>,
    ) -> Result<ManagedArtifactRoutePlan> {
        let components = self.exact_components()?;
        let host_identity = self
            .host_identity
            .as_ref()
            .context("managed artifact layout lacks its host identity")?;
        let wasm_stem = &host_identity.lib_target;
        let harmony_package = inputs
            .ohos_package_name
            .clone()
            .unwrap_or_else(|| format!("{}-ohos", self.root_source_package));
        let harmony_archive = if targets.harmony
            && !inputs.ohos_no_har
            && inputs.ohos_package_kind == super::ohos::PackageKind::Har
        {
            Some(harmony_archive_file_name(&harmony_package)?)
        } else {
            None
        };
        let harmony_hsp = targets.harmony
            && !inputs.ohos_no_har
            && inputs.ohos_package_kind == super::ohos::PackageKind::Hsp;
        let harmony_stem = if harmony_hsp {
            harmony_archive_stem(&harmony_package)?
        } else {
            String::new()
        };
        let has_js = self.has_js(targets);
        let has_browser = targets.wasm || targets.mini_program;
        let manifest_components = components
            .iter()
            .map(|component| -> Result<serde_json::Value> {
                let component_root = self.component_source_root(component);
                Ok(serde_json::json!({
                    "component": component.component,
                    "namespace": component.namespace,
                    "nativeExportPrefix": component.native_export_prefix,
                    "interfaceAbiDigest": component.interface_abi_digest,
                    "source": {
                        "common": if has_js { serde_json::Value::String(self.rel(&component_root.join("common"))?) } else { serde_json::Value::Null },
                        "browser": if has_browser { serde_json::Value::String(self.rel(&component_root.join("browser"))?) } else { serde_json::Value::Null },
                        "node": if targets.node { serde_json::Value::String(self.rel(&component_root.join("node"))?) } else { serde_json::Value::Null },
                        "electron": if targets.electron { serde_json::Value::String(self.rel(&component_root.join("electron"))?) } else { serde_json::Value::Null },
                        "harmony": if targets.harmony { serde_json::Value::String(self.rel(&component_root.join("harmony"))?) } else { serde_json::Value::Null },
                        "publicTypes": if has_js { serde_json::Value::String(self.rel(&component_root.join("common/public-types.ts"))?) } else { serde_json::Value::Null },
                    },
                }))
            })
            .collect::<Result<Vec<_>>>()?;
        let source = serde_json::json!({
            "root": self.rel(&self.source_root)?,
            "shared": if has_js { serde_json::Value::String(self.rel(&self.source_root.join("shared"))?) } else { serde_json::Value::Null },
            "browser": if has_browser { serde_json::Value::String(self.rel(&self.source_root.join("browser"))?) } else { serde_json::Value::Null },
            "node": if targets.node { serde_json::Value::String(self.rel(&self.source_root.join("node"))?) } else { serde_json::Value::Null },
            "electron": if targets.electron { serde_json::Value::String(self.rel(&self.source_root.join("electron"))?) } else { serde_json::Value::Null },
            "harmony": if targets.harmony { serde_json::Value::String(self.rel(&self.source_root.join("harmony"))?) } else { serde_json::Value::Null },
            "swift": if targets.apple { serde_json::Value::String(self.rel(&self.source_root.join("swift"))?) } else { serde_json::Value::Null },
            "kotlin": if targets.android { serde_json::Value::String(self.rel(&self.source_root.join("kotlin"))?) } else { serde_json::Value::Null },
        });
        let entrypoints = serde_json::json!({
            "web": if targets.wasm { serde_json::Value::String("src/index.web.ts".to_string()) } else { serde_json::Value::Null },
            "miniProgram": if targets.mini_program { serde_json::Value::String("src/index.mini-program.ts".to_string()) } else { serde_json::Value::Null },
            "node": if targets.node { serde_json::Value::String("src/index.node.ts".to_string()) } else { serde_json::Value::Null },
            "electron": if targets.electron { serde_json::Value::String("src/index.electron.ts".to_string()) } else { serde_json::Value::Null },
            "harmony": if targets.harmony {
                serde_json::Value::String(self.rel(&self.artifact_root.join(if inputs.ohos_no_har { "harmony/dist/Index.ets" } else { "harmony/package/Index.ets" }))?)
            } else { serde_json::Value::Null },
        });
        let artifacts = serde_json::json!({
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
                    "defaultWasmPath": mini_program_default_wasm_path(wasm_stem),
                })
            } else { serde_json::Value::Null },
            "node": if targets.node {
                serde_json::json!({
                    "addon": self.addon_rel_from(artifact_read_root, "node", wasm_stem)?,
                    "env": "UNIFFI_NAPI_PATH",
                })
            } else { serde_json::Value::Null },
            "electron": if targets.electron {
                serde_json::json!({
                    "addon": self.addon_rel_from(artifact_read_root, "node", wasm_stem)?,
                    "env": "UNIFFI_NAPI_PATH",
                })
            } else { serde_json::Value::Null },
            "harmony": if targets.harmony {
                serde_json::json!({
                    "kind": if inputs.ohos_no_har { "dist" } else { inputs.ohos_package_kind.as_str() },
                    "integrated": harmony_hsp && inputs.ohos_integrated_hsp,
                    "har": harmony_archive.as_ref().map(|archive| self.rel(&self.artifact_root.join("harmony").join(archive))).transpose()?,
                    "runtimeHsp": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony").join(format!("{harmony_stem}.hsp")))?) } else { serde_json::Value::Null },
                    "interfaceHar": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony").join(format!("{harmony_stem}-interface.har")))?) } else { serde_json::Value::Null },
                    "tgz": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony").join(format!("{harmony_stem}.tgz")))?) } else { serde_json::Value::Null },
                    "dist": self.rel(&self.artifact_root.join("harmony/dist"))?,
                    "facade": self.rel(&self.artifact_root.join("harmony/dist/native-facade.ets"))?,
                    "facadeContract": self.rel(&self.artifact_root.join("harmony/dist/harmony-facade-contract.json"))?,
                    "packageFacadeContract": if inputs.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/harmony-facade-contract.json"))?) },
                    "types": self.rel(&self.artifact_root.join("harmony/dist/native-facade.d.ts"))?,
                    "package": if inputs.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package"))?) },
                    "moduleProject": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/module-project"))?) } else { serde_json::Value::Null },
                    "moduleSource": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/module-project/library"))?) } else { serde_json::Value::Null },
                    "usage": if harmony_hsp { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony").join(format!("{harmony_stem}-HSP_USAGE.md")))?) } else { serde_json::Value::Null },
                    "packageMetadata": if inputs.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/oh-package.json5"))?) },
                    "moduleMetadata": if inputs.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/src/main/module.json5"))?) },
                    "buildProfile": if inputs.ohos_no_har { serde_json::Value::Null } else { serde_json::Value::String(self.rel(&self.artifact_root.join("harmony/package/build-profile.json5"))?) },
                    "metadata": serde_json::Value::Null,
                })
            } else { serde_json::Value::Null },
            "apple": if targets.apple {
                serde_json::json!({
                    "xcframework": self.rel(&self.artifact_root.join("apple").join(format!("{}.xcframework", self.root_lib_target)))?,
                    "package": self.rel(&self.artifact_root.join("apple"))?,
                    "product": format!("{}Apple", upper_camel_case_identifier(&self.root_source_package)),
                })
            } else { serde_json::Value::Null },
            "android": if targets.android {
                serde_json::json!({
                    "jniLibs": self.rel(&self.artifact_root.join("android/jniLibs"))?,
                    "aar": inputs.android_aar_out.as_ref().map(|path| self.rel(path)).transpose()?,
                })
            } else { serde_json::Value::Null },
        });
        let host_crates = serde_json::json!({
            "wasm": if targets.wasm || targets.mini_program { serde_json::Value::String(self.rel(&self.host_crates_root.join("wasm/Cargo.toml"))?) } else { serde_json::Value::Null },
            "napi": if targets.node || targets.electron { serde_json::Value::String(self.rel(&self.host_crates_root.join("napi/Cargo.toml"))?) } else { serde_json::Value::Null },
            "ohos": if targets.harmony { serde_json::Value::String(self.rel(&self.host_crates_root.join("ohos/Cargo.toml"))?) } else { serde_json::Value::Null },
        });
        Ok(ManagedArtifactRoutePlan {
            targets: targets.clone(),
            components: serde_json::Value::Array(manifest_components),
            source,
            entrypoints,
            artifacts,
            host_crates,
        })
    }

    fn read_historical_harmony_route_metadata(
        &self,
        path: &Utf8Path,
        label: &str,
    ) -> Result<Option<serde_json::Value>> {
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error).with_context(|| format!("reading {label} {path}")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("historical Harmony {label} must be a regular file: {path}");
        }
        Ok(Some(read_generated_json5(path)?))
    }

    /// Recover the dynamic Harmony route inputs from canonical producer-owned
    /// package metadata, rather than from the manifest being validated or the
    /// current invocation's unrelated defaults. This is only used for a
    /// retained Harmony target that was not requested in the current run.
    fn historical_harmony_route_inputs(
        &self,
        base: &ManagedArtifactRouteInputs,
    ) -> Result<ManagedArtifactRouteInputs> {
        let package_root = self.artifact_root.join("harmony/package");
        let package_path = package_root.join("oh-package.json5");
        let module_path = package_root.join("src/main/module.json5");
        let build_profile_path = package_root.join("build-profile.json5");
        let Some(package) =
            self.read_historical_harmony_route_metadata(&package_path, "package metadata")?
        else {
            for (label, path) in [
                ("module metadata", &module_path),
                ("build profile", &build_profile_path),
            ] {
                if path_entry_exists(path)? {
                    bail!(
                        "historical Harmony route evidence is incomplete: canonical {label} exists without {package_path}"
                    );
                }
            }
            let mut inputs = base.clone();
            inputs.ohos_no_har = true;
            inputs.ohos_package_kind = super::ohos::PackageKind::Har;
            inputs.ohos_integrated_hsp = false;
            inputs.ohos_package_name = None;
            return Ok(inputs);
        };
        let module = self
            .read_historical_harmony_route_metadata(&module_path, "module metadata")?
            .context("historical Harmony route evidence lacks canonical module metadata")?;
        let build_profile = self
            .read_historical_harmony_route_metadata(&build_profile_path, "build profile")?
            .context("historical Harmony route evidence lacks canonical build profile")?;
        let package_name = package
            .get("name")
            .and_then(serde_json::Value::as_str)
            .context("historical Harmony package metadata lacks string `name`")?
            .to_string();
        super::ohos::validate_oh_package_name(&package_name)?;
        let module = module
            .get("module")
            .and_then(serde_json::Value::as_object)
            .context("historical Harmony module metadata lacks object `module`")?;
        let package_kind = match module.get("type").and_then(serde_json::Value::as_str) {
            Some("har") => super::ohos::PackageKind::Har,
            Some("shared") => super::ohos::PackageKind::Hsp,
            Some(kind) => bail!("historical Harmony module metadata has unsupported type `{kind}`"),
            None => bail!("historical Harmony module metadata lacks string `module.type`"),
        };
        match package_kind {
            super::ohos::PackageKind::Har => {
                if package.get("packageType").is_some() {
                    bail!(
                        "historical Harmony HAR metadata must not declare `packageType`; refusing to infer routes"
                    );
                }
            }
            super::ohos::PackageKind::Hsp => {
                if package
                    .get("packageType")
                    .and_then(serde_json::Value::as_str)
                    != Some("InterfaceHar")
                {
                    bail!(
                        "historical Harmony HSP metadata must declare packageType `InterfaceHar`; refusing to infer routes"
                    );
                }
            }
        }
        let integrated_hsp = package_kind == super::ohos::PackageKind::Hsp
            && build_profile
                .pointer("/buildOption/arkOptions/integratedHsp")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
        let mut inputs = base.clone();
        inputs.ohos_no_har = false;
        inputs.ohos_package_kind = package_kind;
        inputs.ohos_integrated_hsp = integrated_hsp;
        inputs.ohos_package_name = Some(package_name);
        Ok(inputs)
    }

    /// Reconstruct the declared target union from immutable layout state,
    /// captured inputs, and canonical producer-owned package metadata when a
    /// retained Harmony target was not requested in the current run. The
    /// manifest may select which dynamic evidence is needed, but never
    /// supplies a route value used by the plan.
    fn manifest_declared_route_inputs(
        &self,
        manifest: &serde_json::Value,
        targets: &ExpandedTargets,
        force_historical_harmony: bool,
    ) -> Result<ManagedArtifactRouteInputs> {
        let base = self.route_inputs.as_ref().context(
            "managed artifact route inputs are unavailable for declared-target validation; refusing to adopt historical routes",
        )?;
        let current = self.expected_routes.as_ref().map(|plan| &plan.targets);
        let current_harmony = current.is_some_and(|targets| targets.harmony);
        let current_android = current.is_some_and(|targets| targets.android);
        let mut inputs = base.clone();
        if targets.harmony && (force_historical_harmony || !current_harmony) {
            inputs = self.historical_harmony_route_inputs(&inputs)?;
        }
        if targets.android && !current_android {
            let aar = manifest
                .pointer("/artifacts/android/aar")
                .context("historical Android manifest lacks artifacts.android.aar")?;
            if aar.is_null() {
                inputs.android_aar_out = None;
            } else if inputs.android_aar_out.is_none() {
                bail!(
                    "historical Android AAR route cannot be reconstructed; repeat --android-aar-out to retain this managed target"
                );
            }
        }
        Ok(inputs)
    }

    /// Reconstruct the declared route union for a merged candidate. Current
    /// requested targets use the current producer inputs; dynamic retained
    /// targets use independent canonical evidence or fail closed.
    fn manifest_declared_route_plan(
        &self,
        manifest: &serde_json::Value,
    ) -> Result<Option<ManagedArtifactRoutePlan>> {
        self.manifest_declared_route_plan_with_harmony_mode(manifest, false)
    }

    /// Reconstruct a route plan for an already-published manifest. Unlike a
    /// merged candidate, this must never inherit the current Harmony kind:
    /// its canonical package metadata is the only accepted evidence for the
    /// historical HSP/HAR route shape.
    fn historical_manifest_declared_route_plan(
        &self,
        manifest: &serde_json::Value,
    ) -> Result<Option<ManagedArtifactRoutePlan>> {
        self.manifest_declared_route_plan_with_harmony_mode(manifest, true)
    }

    fn manifest_declared_route_plan_with_harmony_mode(
        &self,
        manifest: &serde_json::Value,
        force_historical_harmony: bool,
    ) -> Result<Option<ManagedArtifactRoutePlan>> {
        if self.route_inputs.is_none() {
            if self.expected_routes.is_some() {
                bail!(
                    "managed artifact route inputs are unavailable for declared-target validation; refusing to adopt historical routes"
                );
            }
            return Ok(None);
        }
        let targets = expanded_targets_from_managed_manifest(manifest)?;
        let inputs =
            self.manifest_declared_route_inputs(manifest, &targets, force_historical_harmony)?;
        Ok(Some(
            self.managed_artifact_route_plan(&targets, &inputs, None)?,
        ))
    }

    /// Promote the current source/library plan before any mutable managed
    /// state is created. This selection is authoritative and therefore makes
    /// an existing v4 manifest an exact compatibility check rather than an
    /// adoptable source of truth.
    fn apply_authoritative_input_components(
        &mut self,
        args: &BuildArgs,
        targets: &ExpandedTargets,
    ) -> Result<()> {
        let components = managed_authoritative_input_components(args, &self.root_source_package)?;
        self.components = Some(components);
        self.components_authoritative = true;
        let inputs = ManagedArtifactRouteInputs::from(args);
        self.expected_routes = Some(self.managed_artifact_route_plan(targets, &inputs, None)?);
        self.route_inputs = Some(inputs);
        Ok(())
    }

    /// Direct test fixtures may still ask to promote an exact existing v4
    /// component set after owner validation. Production `build` has already
    /// installed an authoritative current plan, so it only revalidates here.
    fn adopt_owner_verified_existing_components(&mut self) -> Result<()> {
        let Some(host_identity) = self.host_identity.as_ref() else {
            return Ok(());
        };
        let Some(existing) = read_existing_managed_manifest_components(self, host_identity)? else {
            return Ok(());
        };
        let existing_components = existing.components;
        let generated_components =
            self.generated_component_identities_with_fallback(Some(&existing_components))?;
        if self.components_authoritative {
            self.preflight_existing_package()?;
            return Ok(());
        }
        if let Some(generated) = generated_components {
            if !same_managed_component_keys(&generated, &existing_components) {
                bail!(
                    "managed artifact component set mismatch between existing manifest and generated bridges: manifest {existing_components:?}, bridges {generated:?}"
                );
            }
        }
        self.components = Some(existing_components);
        self.components_authoritative = true;
        self.preflight_existing_package()
    }

    /// Read the generated bridges as an independent check on the selected
    /// component set.  The namespace directory alone is not an identity: the
    /// bridge filename carries the Cargo component name, from which the
    /// authoritative native export prefix is re-derived.
    fn generated_component_identities(&self) -> Result<Option<Vec<ManagedComponentIdentity>>> {
        self.generated_component_identities_with_fallback(None)
    }

    fn generated_component_identities_with_fallback(
        &self,
        fallback_components: Option<&[ManagedComponentIdentity]>,
    ) -> Result<Option<Vec<ManagedComponentIdentity>>> {
        let components_dir = self.source_root.join("components");
        if !components_dir.exists() {
            return Ok(None);
        }
        let mut identities = Vec::new();
        for entry in std::fs::read_dir(&components_dir)
            .with_context(|| format!("reading generated component root {components_dir}"))?
        {
            let entry = entry.with_context(|| format!("reading entry below {components_dir}"))?;
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "generated managed component path is not UTF-8 below {components_dir}: {}",
                    path.display()
                )
            })?;
            let file_type = entry.file_type().with_context(|| {
                format!("reading file type for generated component path {path}")
            })?;
            if file_type.is_symlink() || !file_type.is_dir() {
                bail!("generated managed component entry must be a real directory: {path}");
            }
            let namespace = path
                .file_name()
                .with_context(|| format!("generated component directory has no name: {path}"))?
                .to_string();
            let mut bridges = BTreeSet::new();
            for flavor in ["browser", "node", "electron", "harmony"] {
                let flavor_dir = path.join(flavor);
                if !flavor_dir.exists() {
                    continue;
                }
                let flavor_metadata =
                    std::fs::symlink_metadata(&flavor_dir).with_context(|| {
                        format!("reading generated component flavor directory {flavor_dir}")
                    })?;
                if flavor_metadata.file_type().is_symlink() || !flavor_metadata.is_dir() {
                    bail!(
                        "generated managed component flavor must be a real directory: {flavor_dir}"
                    );
                }
                let mut flavor_bridges = Vec::new();
                for file in std::fs::read_dir(&flavor_dir)
                    .with_context(|| format!("reading generated component flavor {flavor_dir}"))?
                {
                    let file = file?;
                    let file_path = Utf8PathBuf::from_path_buf(file.path()).map_err(|path| {
                        anyhow::anyhow!(
                            "generated managed bridge path is not UTF-8 below {flavor_dir}: {}",
                            path.display()
                        )
                    })?;
                    if file_path.extension() == Some("rs") {
                        let file_type = file.file_type()?;
                        if file_type.is_symlink() || !file_type.is_file() {
                            bail!("generated managed bridge must be a regular file: {file_path}");
                        }
                        flavor_bridges.push(
                            file_path
                                .file_stem()
                                .with_context(|| {
                                    format!("generated managed bridge has no stem: {file_path}")
                                })?
                                .to_string(),
                        );
                    }
                }
                match flavor_bridges.as_slice() {
                    [] => {}
                    [bridge] => {
                        bridges.insert(bridge.clone());
                    }
                    _ => bail!(
                        "generated managed component namespace `{namespace}` flavor `{flavor}` has multiple Rust bridges: {flavor_bridges:?}"
                    ),
                }
            }
            let component = match bridges.into_iter().collect::<Vec<_>>().as_slice() {
                [component] => component.clone(),
                [] => bail!(
                    "generated managed component namespace `{namespace}` has no Rust bridge in any selected flavor"
                ),
                bridges => bail!(
                    "generated managed component namespace `{namespace}` has inconsistent Rust bridge identities: {bridges:?}"
                ),
            };
            let planned = self
                .components
                .as_ref()
                .and_then(|components| {
                    components.iter().find(|identity| {
                        identity.component == component && identity.namespace == namespace
                    })
                })
                .or_else(|| {
                    fallback_components.and_then(|components| {
                        components.iter().find(|identity| {
                            identity.component == component && identity.namespace == namespace
                        })
                    })
                })
                .with_context(|| {
                    format!(
                        "generated managed component `{component}` namespace `{namespace}` is absent from the authoritative interface ABI plan and validated existing manifest"
                    )
                })?;
            identities.push(ManagedComponentIdentity::with_interface_abi_digest(
                component,
                namespace,
                &planned.interface_abi_digest,
            )?);
        }
        if identities.is_empty() {
            return Ok(None);
        }
        canonical_managed_component_identities(identities).map(Some)
    }

    fn refresh_generated_component_identities(&mut self) -> Result<()> {
        let fallback_components = if self.components_authoritative {
            None
        } else if let Some(host_identity) = self.host_identity.as_ref() {
            read_existing_managed_manifest_components(self, host_identity)?
                .map(|existing| existing.components)
        } else {
            None
        };
        let components = self
            .generated_component_identities_with_fallback(fallback_components.as_deref())?
            .with_context(|| {
                format!(
                    "generated managed JavaScript source tree has no component identities below {}",
                    self.source_root.join("components")
                )
            })?;
        if self.components_authoritative {
            let planned = self.exact_components()?;
            if planned != components {
                bail!(
                    "managed artifact component set mismatch between authoritative planned metadata and generated bridges: expected {planned:?}, got {components:?}"
                );
            }
        }
        self.components = Some(components);
        self.components_authoritative = true;
        Ok(())
    }

    pub(super) fn rebased(&self, from: &Utf8Path, to: &Utf8Path) -> Result<Self> {
        let rebase = |path: &Utf8Path| -> Result<Utf8PathBuf> {
            Ok(to
                .join(path.strip_prefix(from).with_context(|| {
                    format!("managed layout path escaped package root: {path}")
                })?))
        };
        let route_inputs = self
            .route_inputs
            .as_ref()
            .map(|inputs| -> Result<ManagedArtifactRouteInputs> {
                let mut inputs = inputs.clone();
                inputs.android_aar_out =
                    inputs.android_aar_out.as_deref().map(rebase).transpose()?;
                Ok(inputs)
            })
            .transpose()?;
        Ok(Self {
            package_dir: to.to_path_buf(),
            root_source_package: self.root_source_package.clone(),
            root_lib_target: self.root_lib_target.clone(),
            source_root: rebase(&self.source_root)?,
            artifact_root: rebase(&self.artifact_root)?,
            host_crates_root: rebase(&self.host_crates_root)?,
            manifest_path: rebase(&self.manifest_path)?,
            components: self.components.clone(),
            components_authoritative: self.components_authoritative,
            host_identity: self.host_identity.clone(),
            expected_routes: self.expected_routes.clone(),
            route_inputs,
        })
    }

    fn mirrored(&self, mirror: &InvocationMirror) -> Result<Self> {
        let route_inputs = self
            .route_inputs
            .as_ref()
            .map(|inputs| -> Result<ManagedArtifactRouteInputs> {
                let mut inputs = inputs.clone();
                inputs.android_aar_out = inputs
                    .android_aar_out
                    .as_deref()
                    .map(|path| mirror.map(path))
                    .transpose()?;
                Ok(inputs)
            })
            .transpose()?;
        Ok(Self {
            package_dir: mirror.map(&self.package_dir)?,
            root_source_package: self.root_source_package.clone(),
            root_lib_target: self.root_lib_target.clone(),
            source_root: mirror.map(&self.source_root)?,
            artifact_root: mirror.map(&self.artifact_root)?,
            host_crates_root: mirror.map(&self.host_crates_root)?,
            manifest_path: mirror.map(&self.manifest_path)?,
            components: self.components.clone(),
            components_authoritative: self.components_authoritative,
            host_identity: self.host_identity.clone(),
            expected_routes: self.expected_routes.clone(),
            route_inputs,
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
            root_source_package: meta.package_name.clone(),
            root_lib_target: meta.lib_target_name.clone(),
            source_root,
            artifact_root,
            host_crates_root,
            manifest_path,
            // This is only a seed for selecting `src:<root-package>` in the
            // mandatory read-only planner above the transaction boundary.
            components: Some(vec![ManagedComponentIdentity::with_interface_abi_digest(
                meta.lib_target_name.clone(),
                meta.lib_target_name.clone(),
                sha256_bytes(
                    format!(
                        "uniffi-managed-component-plan-seed:{}:{}",
                        meta.lib_target_name, meta.lib_target_name
                    )
                    .as_bytes(),
                ),
            )?]),
            components_authoritative: false,
            host_identity: Some(ManagedHostIdentity::from_cargo_metadata(&meta)),
            expected_routes: None,
            route_inputs: None,
        }))
    }

    #[cfg(test)]
    fn emit(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
    ) -> Result<()> {
        self.emit_with_artifact_read_root(targets, meta, args, None)
    }

    /// Direct manifest fixtures deliberately have no built addon yet, so
    /// they retain the pure renderer entry point above.
    #[cfg(test)]
    fn emit_with_artifact_read_root(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
        artifact_read_root: Option<&Utf8Path>,
    ) -> Result<()> {
        self.emit_with_artifact_read_root_and_existing_manifest_evidence(
            targets,
            meta,
            args,
            artifact_read_root,
            None,
        )
    }

    /// Emit a private candidate while keeping the pre-replacement public
    /// layout available to validate an existing manifest's historical route
    /// evidence. `artifact_read_root` is only for current generated artifacts;
    /// it must never be used to interpret the manifest being replaced.
    fn emit_with_artifact_read_root_and_existing_manifest_evidence(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
        artifact_read_root: Option<&Utf8Path>,
        existing_manifest_evidence_layout: Option<&ManagedLayout>,
    ) -> Result<()> {
        self.emit_supporting_files(targets, meta, args)?;
        let manifest = self.render_manifest_with_read_roots_and_existing_manifest_evidence(
            targets,
            meta,
            args,
            None,
            artifact_read_root,
            existing_manifest_evidence_layout,
        )?;
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
            "web",
        )
    }

    fn emit_mini_program_entrypoint(&self) -> Result<()> {
        let entrypoint = self.package_dir.join("src/index.mini-program.ts");
        self.write_entrypoint(
            &entrypoint,
            &self.source_root.join("browser/index.mini-program.ts"),
            "mini-program",
        )
    }

    fn emit_node_entrypoint(&self) -> Result<()> {
        let entrypoint = self.package_dir.join("src/index.node.ts");
        self.write_entrypoint(&entrypoint, &self.source_root.join("node/index.ts"), "node")
    }

    fn emit_electron_entrypoint(&self) -> Result<()> {
        let entrypoint = self.package_dir.join("src/index.electron.ts");
        self.write_entrypoint(
            &entrypoint,
            &self.source_root.join("electron/index.ts"),
            "electron",
        )
    }

    fn write_entrypoint(
        &self,
        entrypoint: &Utf8Path,
        runtime_entry: &Utf8Path,
        label: &str,
    ) -> Result<()> {
        let parent = entrypoint
            .parent()
            .with_context(|| format!("managed {label} entrypoint has no parent: {entrypoint}"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating managed {label} entrypoint dir {parent}"))?;
        let runtime_spec = module_specifier(parent, runtime_entry)?;
        let source = format!(
            "// AUTOGENERATED by uniffi_bindgen_javascript (managed {label} entrypoint).\n\
             // Do not edit by hand.\n\
             \n\
             export * from \"{runtime_spec}\";\n",
        );
        std::fs::write(entrypoint, source)
            .with_context(|| format!("writing managed {label} entrypoint {entrypoint}"))?;
        Ok(())
    }

    fn component_source_root(&self, component: &ManagedComponentIdentity) -> Utf8PathBuf {
        self.source_root
            .join("components")
            .join(&component.namespace)
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

    #[cfg(test)]
    fn render_manifest(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
    ) -> Result<String> {
        self.render_manifest_with_harmony_root(targets, meta, args, None)
    }

    #[cfg(test)]
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
        self.render_manifest_with_read_roots_and_existing_manifest_evidence(
            targets,
            meta,
            args,
            harmony_source_root,
            artifact_read_root,
            None,
        )
    }

    /// Render a manifest from current staged outputs. When a transaction has
    /// already replaced a selected private target tree, the existing manifest
    /// still has to be validated against the untouched public layout rather
    /// than this current artifact read root.
    fn render_manifest_with_read_roots_and_existing_manifest_evidence(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
        harmony_source_root: Option<&Utf8Path>,
        artifact_read_root: Option<&Utf8Path>,
        existing_manifest_evidence_layout: Option<&ManagedLayout>,
    ) -> Result<String> {
        let route_inputs = ManagedArtifactRouteInputs::from(args);
        let route_plan =
            self.managed_artifact_route_plan(targets, &route_inputs, artifact_read_root)?;
        let components = self.exact_components()?;
        let host_identity = self
            .host_identity
            .as_ref()
            .context("managed artifact layout lacks its host identity")?;
        let host_composite_identity = host_identity.composite_identity(components)?;
        let harmony_package = args
            .ohos_package_name
            .clone()
            .unwrap_or_else(|| format!("{}-ohos", meta.package_name));
        let default_harmony_source_root = self.artifact_root.join("harmony");
        let harmony_source_root = harmony_source_root.unwrap_or(&default_harmony_source_root);
        let harmony_metadata = if targets.harmony && !args.ohos_no_har {
            self.harmony_package_metadata(meta, args, &harmony_package, harmony_source_root)?
        } else {
            serde_json::Value::Null
        };
        // Every route-bearing section is emitted directly from the exact
        // producer plan shared with preflight and candidate validation. The
        // staged Harmony metadata is the only dynamic, non-route payload.
        let mut artifacts = route_plan.artifacts.clone();
        if let Some(harmony) = artifacts
            .get_mut("harmony")
            .and_then(serde_json::Value::as_object_mut)
        {
            harmony.insert("metadata".to_string(), harmony_metadata);
        }
        let manifest = serde_json::json!({
            "artifactManifestSchemaVersion": ARTIFACT_MANIFEST_SCHEMA_VERSION,
            "generator": "uniffi-bindgen-javascript",
            "components": route_plan.components.clone(),
            "hostCompositeIdentity": host_composite_identity,
            "targets": self.manifest_targets(targets),
            "source": route_plan.source.clone(),
            "entrypoints": route_plan.entrypoints.clone(),
            "artifacts": artifacts,
            "hostCrates": route_plan.host_crates.clone(),
        });
        let manifest = self.merge_existing_manifest(manifest, existing_manifest_evidence_layout)?;
        let bytes = serde_json::to_vec(&manifest)?;
        let (_, parsed_manifest) = parse_exact_managed_artifact_manifest_with_routes(
            &bytes,
            Some(self.exact_components()?),
            self.host_identity.as_ref(),
            Some(&route_plan),
            true,
            "managed artifact manifest candidate",
        )?;
        if let Some(merged_plan) = self.manifest_declared_route_plan(&parsed_manifest)? {
            merged_plan.validate_manifest_routes(&parsed_manifest, true)?;
        }
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
        existing_manifest_evidence_layout: Option<&ManagedLayout>,
    ) -> Result<serde_json::Value> {
        if !self.manifest_path.exists() {
            return Ok(manifest);
        }

        let existing_text = std::fs::read_to_string(&self.manifest_path)
            .with_context(|| format!("reading managed artifact manifest {}", self.manifest_path))?;
        let (_, existing) = parse_exact_managed_artifact_manifest_with_routes(
            existing_text.as_bytes(),
            None,
            None,
            // Validate the published document as a historical exact-v4
            // artifact first. The current route plan is only relevant after
            // this merge has selected the replacement target values.
            None,
            false,
            "existing managed artifact manifest",
        )?;
        let existing_manifest_evidence_layout = existing_manifest_evidence_layout.unwrap_or(self);
        if let Some(declared_plan) =
            existing_manifest_evidence_layout.historical_manifest_declared_route_plan(&existing)?
        {
            declared_plan.validate_manifest_routes(&existing, true)?;
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
        merge_manifest_components(&mut manifest, &existing, &current_targets)?;
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
        composite_stem: &str,
    ) -> Result<String> {
        let public_dir = self.artifact_root.join(subdir);
        let canonical = public_dir.join(format!("{composite_stem}.node"));
        let Some(artifact_read_root) = artifact_read_root else {
            return self.rel(&canonical);
        };
        let read_dir = artifact_read_root.join(subdir);
        let expected_candidate = read_dir.join(format!("{composite_stem}.node"));
        let metadata = std::fs::symlink_metadata(&expected_candidate).with_context(|| {
            format!(
                "managed composite addon candidate is missing at the canonical private route {expected_candidate}"
            )
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "managed composite addon candidate must be a regular file at {expected_candidate}"
            );
        }
        let mut nodes = Vec::new();
        for entry in std::fs::read_dir(&read_dir).with_context(|| format!("reading {read_dir}"))? {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "managed addon artifact path is not utf8: {}",
                    path.display()
                )
            })?;
            if path.extension() == Some("node") {
                nodes.push(path);
            }
        }
        nodes.sort();
        if nodes != vec![expected_candidate.clone()] {
            bail!(
                "managed composite addon candidate set must contain exactly `{expected_candidate}`, got {nodes:?}"
            );
        }
        self.rel(&canonical)
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

fn merge_manifest_components(
    manifest: &mut serde_json::Value,
    existing: &serde_json::Value,
    current_targets: &BTreeSet<String>,
) -> Result<()> {
    let current = manifest
        .get_mut("components")
        .and_then(serde_json::Value::as_array_mut)
        .context("managed manifest candidate lacks components array")?;
    let previous = existing
        .get("components")
        .and_then(serde_json::Value::as_array)
        .context("existing managed manifest lacks components array")?;
    if current.len() != previous.len() {
        bail!("managed artifact manifest component set changed during incremental merge");
    }
    let current_has_js = current_targets.iter().any(|target| {
        matches!(
            target.as_str(),
            "wasm" | "mini-program" | "node" | "electron" | "harmony"
        )
    });
    for (current_component, previous_component) in current.iter_mut().zip(previous) {
        for field in ["component", "namespace", "nativeExportPrefix"] {
            if current_component.get(field) != previous_component.get(field) {
                bail!("managed artifact manifest component set changed during incremental merge");
            }
        }
        let current_source = current_component
            .get_mut("source")
            .and_then(serde_json::Value::as_object_mut)
            .context("managed manifest candidate component lacks source object")?;
        let previous_source = previous_component
            .get("source")
            .and_then(serde_json::Value::as_object)
            .context("existing managed manifest component lacks source object")?;
        for (field, previous_value) in previous_source {
            let generated_this_run = match field.as_str() {
                "common" | "publicTypes" => current_has_js,
                "browser" => {
                    current_targets.contains("wasm") || current_targets.contains("mini-program")
                }
                "node" => current_targets.contains("node"),
                "electron" => current_targets.contains("electron"),
                "harmony" => current_targets.contains("harmony"),
                _ => {
                    bail!("existing managed manifest component source has unknown route `{field}`")
                }
            };
            if generated_this_run
                || current_source
                    .get(field)
                    .is_some_and(|value| !value.is_null())
            {
                continue;
            }
            current_source.insert(field.clone(), previous_value.clone());
        }
    }
    Ok(())
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
    let generated_host_package =
        uniffi_bindgen_javascript::host_crates::composite_host_package_name(&meta.package_name);
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
        let wasm_stem =
            uniffi_bindgen_javascript::host_crates::composite_host_lib_target(&meta.package_name);
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
            let wasm_stem = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(
                &meta.package_name,
            );
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
    let (_, manifest) = parse_exact_managed_artifact_manifest_with_routes(
        &bytes,
        Some(layout.exact_components()?),
        layout.host_identity.as_ref(),
        layout.expected_routes.as_ref(),
        true,
        "managed artifact manifest candidate",
    )?;
    if let Some(merged_plan) = layout.manifest_declared_route_plan(&manifest)? {
        merged_plan.validate_manifest_routes(&manifest, true)?;
    }
    let host_crates = validate_managed_manifest_paths(layout, &manifest)?;
    for manifest_path in host_crates.into_iter().flatten() {
        let output = Command::new(cargo_bin)
            .args([
                "metadata",
                "--format-version=1",
                "--no-deps",
                "--manifest-path",
            ])
            .arg(&manifest_path)
            .output()
            .with_context(|| format!("running Cargo metadata for managed host {manifest_path}"))?;
        if !output.status.success() {
            bail!(
                "managed host Cargo metadata failed for {manifest_path}: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    Ok(())
}

/// Validate every path used by a manifest without invoking Cargo.  This is
/// shared by existing-package preflight and the post-build candidate check;
/// callers that need host metadata execute it only after this exact parser and
/// filesystem validation succeed.
fn validate_managed_manifest_paths(
    layout: &ManagedLayout,
    manifest: &serde_json::Value,
) -> Result<Vec<Option<Utf8PathBuf>>> {
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
        "/source/shared",
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
    let components = manifest
        .get("components")
        .and_then(serde_json::Value::as_array)
        .context("managed manifest lacks components array while validating paths")?;
    for (index, _) in components.iter().enumerate() {
        for suffix in ["common", "browser", "node", "electron", "harmony"] {
            validate_path(&format!("/components/{index}/source/{suffix}"), true)?;
        }
        validate_path(&format!("/components/{index}/source/publicTypes"), false)?;
    }
    for pointer in [
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
    ["/hostCrates/wasm", "/hostCrates/napi", "/hostCrates/ohos"]
        .into_iter()
        .map(|pointer| validate_path(pointer, false))
        .collect()
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
        let wasm_stem =
            uniffi_bindgen_javascript::host_crates::composite_host_lib_target(&meta.package_name);
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
    mut layout: ManagedLayout,
) -> Result<()> {
    // `build` installs the authoritative source/library plan before this
    // function is reached. The owner check is still repeated under the output
    // lock by `ManagedPackageTransaction::begin` before mutable state exists.
    preflight_managed_package(&layout)?;
    layout.adopt_owner_verified_existing_components()?;
    let mut transaction = ManagedPackageTransaction::begin(&layout)?;
    let mut private_layout = layout.rebased(&layout.package_dir, transaction.candidate_root())?;
    let prepared = (|| -> Result<ManagedPackageOwner> {
        clear_managed_selected_roots(&mut transaction, &layout, &targets)?;
        let private_args = managed_private_args(&transaction, &layout, &public_args)?;
        build_private_target_set(&private_args, &targets)?;
        rebase_private_javascript_host_crates(&public_args, &private_args, &targets)?;
        if private_layout.has_js(&targets) {
            private_layout.refresh_generated_component_identities()?;
        } else if let Some(components) = private_layout.generated_component_identities()? {
            if private_layout.components_authoritative
                && private_layout.exact_components()? != components
            {
                bail!(
                    "managed artifact component set mismatch between authoritative planned metadata and generated bridges: expected {:?}, got {components:?}",
                    private_layout.exact_components()?
                );
            }
            private_layout.components = Some(components);
            private_layout.components_authoritative = true;
        }
        let meta = cargo_package_metadata(&public_args.manifest_path)?;
        private_layout
            .emit_with_artifact_read_root_and_existing_manifest_evidence(
                &targets,
                &meta,
                &private_args,
                Some(&private_layout.artifact_root),
                Some(&layout),
            )
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
    // Managed target generation is intentionally fail-closed once its durable
    // transaction starts: partial tool output is not adopted after an error
    // merely so it can be deleted.  Reject deterministic Android toolchain
    // configuration errors before any journal, candidate, or build root can
    // exist. `build_android` repeats these checks at the point of use so this
    // early validation does not weaken the normal TOCTOU boundary.
    if args.managed_layout && targets.android {
        preflight_android_toolchain(&args)
            .context("preflighting Android toolchain before target generation")?;
    }

    // Managed HSP derives its public layout without touching the filesystem so
    // existing ownership, residue, and manifest evidence can fail closed
    // before frontend tool probing creates an invocation root.
    let mut managed_layout = if args.managed_layout
        && targets.harmony
        && args.ohos_package_kind == super::ohos::PackageKind::Hsp
    {
        ManagedLayout::apply(&mut args, &targets)?
    } else {
        None
    };
    if let Some(layout) = managed_layout.as_mut() {
        layout
            .apply_authoritative_input_components(&args, &targets)
            .context("planning managed component identities from the current source/library")?;
    }
    if targets.harmony {
        if let Some(layout) = managed_layout.as_ref() {
            preflight_managed_package(layout).context(
                "preflighting existing managed package owner and transaction residue before Harmony HSP tools",
            )?;
            layout.preflight_existing_package().context(
                "preflighting existing managed artifact manifest before Harmony HSP tools",
            )?;
        }
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
        .context("preflighting Harmony HSP before target generation")?;
    }
    if managed_layout.is_none() {
        managed_layout = ManagedLayout::apply(&mut args, &targets)?;
        if let Some(layout) = managed_layout.as_mut() {
            layout
                .apply_authoritative_input_components(&args, &targets)
                .context("planning managed component identities from the current source/library")?;
        }
    }
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
            let wasm_stem = uniffi_bindgen_javascript::host_crates::composite_host_lib_target(
                &meta.package_name,
            );
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

fn preflight_android_toolchain(args: &BuildArgs) -> Result<()> {
    let ndk_home = resolve_android_ndk_home(args)?;
    let prebuilt = android_llvm_prebuilt_dir(&ndk_home)?;
    for abi in android_abis(args)? {
        let clang = android_clang_path(&prebuilt, &abi, args.android_api);
        if !clang.exists() {
            bail!(
                "Android NDK clang not found at {}. Check --android-ndk-home/ANDROID_NDK_HOME and --android-api",
                clang
            );
        }
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
