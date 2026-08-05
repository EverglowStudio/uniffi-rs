/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::MetadataCommand;
#[cfg(feature = "cli-ohos")]
use cargo_metadata::{CargoOpt, DependencyKind, Package};
use clap::{Args, Subcommand, ValueEnum};
use std::process::Command;
#[cfg(feature = "cli-javascript")]
use uniffi_bindgen::BindgenPaths;
use uniffi_bindgen::{
    cargo_metadata::CrateConfigSupplier, BindgenLoader, CargoMetadataOptions, GlobalConfig,
};
use uniffi_bindgen_javascript::{
    FlavorTarget, GenerateJsOptions, HostCrateOptions, WasmPostLinkTarget,
};

#[derive(Args)]
pub(crate) struct JavascriptArgs {
    #[clap(subcommand)]
    pub command: JavascriptCommands,
}

#[derive(Subcommand)]
pub(crate) enum JavascriptCommands {
    /// Build JavaScript bindings for browser wasm plus Node/Electron N-API.
    ///
    /// This is a convenience orchestration over `build-wasm` followed by
    /// `build-napi`; the specialized subcommands remain available when a
    /// downstream only needs one ABI.
    Build(BuildArgs),

    /// Build JavaScript + wasm bindings, emit the wasm host crate, and run
    /// UniFFI's in-process `wasm-bindgen-cli-support` runner.
    ///
    /// This does not invoke or require an external `wasm-bindgen` executable.
    ///
    /// This path targets downstream crates whose generated wasm host crate can
    /// directly call the Rust API described by the chosen UDL/library input.
    BuildWasm(BuildWasmArgs),

    /// Build JavaScript + N-API bindings, emit/build the napi host crate, and copy `.node` addons.
    BuildNapi(BuildNapiArgs),

    /// Build JavaScript + Harmony/OpenHarmony bindings through OHOS Node-API.
    #[cfg(feature = "cli-ohos")]
    BuildOhos(BuildOhosArgs),
}

#[derive(Clone, Args)]
pub(crate) struct BuildArgs {
    /// Root package Cargo manifest.
    #[clap(long = "manifest-path")]
    manifest_path: Utf8PathBuf,

    /// Directory in which to write generated JavaScript files.
    #[clap(long, short)]
    out_dir: Utf8PathBuf,

    /// Optional override for the library/cdylib path used for JS generation.
    /// When omitted, the command builds the root package at --manifest-path and derives
    /// the cdylib location from Cargo metadata.
    #[clap(long = "library-path")]
    library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the JS generator.
    /// When set, this overrides the built-library path for generation, but
    /// the root package is still built as part of the orchestration.
    #[clap(long)]
    source: Option<Utf8PathBuf>,

    /// Directory (default `<out-dir>/native/hosts`) in which to emit generated host crates.
    #[clap(long = "host-crates-dir", default_value = "native/hosts")]
    host_crates_dir: Utf8PathBuf,

    /// Directory for built non-source artifacts. With it, wasm-bindgen output
    /// defaults to `<artifact-dir>/browser/pkg`; otherwise it uses `<out-dir>/browser/pkg`.
    #[clap(long = "artifact-dir")]
    artifact_dir: Option<Utf8PathBuf>,

    /// Where to write the wasm-bindgen output tree. Defaults to
    /// `<artifact-dir>/browser/pkg` when --artifact-dir is set, otherwise `<out-dir>/browser/pkg`.
    #[clap(long = "wasm-bindgen-out-dir")]
    wasm_bindgen_out_dir: Option<Utf8PathBuf>,

    /// wasm-bindgen output target.
    #[clap(long = "wasm-bindgen-target", value_enum, default_value = "web")]
    wasm_bindgen_target: WasmBindgenTargetArg,

    /// N-API consumption form(s) to emit. Defaults to both node and electron.
    #[clap(long = "napi-flavor", value_enum)]
    napi_flavor: Vec<NapiBuildFlavorArg>,

    /// Override the `cargo` binary to invoke.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    cargo_bin: String,

    /// Cargo target directory for the generated N-API host build.
    #[clap(long = "target-dir")]
    target_dir: Option<Utf8PathBuf>,

    /// Cargo target directory for the generated wasm host build.
    #[clap(long = "wasm-target-dir")]
    wasm_target_dir: Option<Utf8PathBuf>,

    /// Build the root package and generated host crates in release mode.
    #[clap(long)]
    release: bool,

    /// Cargo features enabled on the root package. May be repeated or comma-separated.
    #[clap(long = "cargo-feature", value_delimiter = ',')]
    cargo_features: Vec<String>,

    /// Do not try to format the generated bindings.
    #[clap(long, short)]
    no_format: bool,

    /// Path to optional uniffi config file.
    #[clap(long, short)]
    config: Option<Utf8PathBuf>,

    /// Optional crate filter passed through to the JS generator.
    #[clap(long = "crate")]
    crate_name: Option<String>,

    /// Whether we should exclude dependencies when running cargo metadata.
    #[clap(long)]
    metadata_no_deps: bool,
}

impl BuildArgs {
    fn wasm_args(&self) -> BuildWasmArgs {
        BuildWasmArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir.clone(),
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir.clone(),
            package_root: self.out_dir.clone(),
            logical_host_crates_dir: None,
            artifact_dir: self.artifact_dir.clone(),
            wasm_bindgen_out_dir: self.wasm_bindgen_out_dir.clone(),
            wasm_bindgen_target: self.wasm_bindgen_target,
            cargo_bin: self.cargo_bin.clone(),
            core_target_dir: self.wasm_target_dir.as_ref().map(|root| root.join("core")),
            target_dir: self.wasm_target_dir.as_ref().map(|root| root.join("host")),
            release: self.release,
            cargo_features: self.cargo_features.clone(),
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
        }
    }

    fn napi_args(&self) -> BuildNapiArgs {
        BuildNapiArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir.clone(),
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir.clone(),
            package_root: self.out_dir.clone(),
            logical_host_crates_dir: None,
            artifact_dir: self.artifact_dir.clone(),
            flavor: self.napi_flavor.clone(),
            cargo_bin: self.cargo_bin.clone(),
            target_dir: self.target_dir.clone(),
            release: self.release,
            cargo_features: self.cargo_features.clone(),
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
        }
    }
}

#[derive(Clone, Args)]
pub(crate) struct BuildWasmArgs {
    /// Root package Cargo manifest.
    #[clap(long = "manifest-path")]
    pub(crate) manifest_path: Utf8PathBuf,

    /// Directory in which to write generated JavaScript files.
    #[clap(long, short)]
    pub(crate) out_dir: Utf8PathBuf,

    /// Optional override for the library/cdylib path used for JS generation.
    /// When omitted, the command builds the root package at --manifest-path and derives
    /// the cdylib location from Cargo metadata.
    #[clap(long = "library-path")]
    pub(crate) library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the JS generator.
    /// When set, this overrides the built-library path for generation, but
    /// the root package is still built as part of the orchestration.
    #[clap(long)]
    pub(crate) source: Option<Utf8PathBuf>,

    /// Directory (default `<out-dir>/native/hosts`) in which to emit the generated wasm host crate.
    #[clap(long = "host-crates-dir", default_value = "native/hosts")]
    pub(crate) host_crates_dir: Utf8PathBuf,

    /// Root of the complete generated package. The host directory and source
    /// output must both remain below this root.
    #[clap(skip)]
    pub(crate) package_root: Utf8PathBuf,

    #[clap(skip)]
    pub(crate) logical_host_crates_dir: Option<Utf8PathBuf>,

    /// Directory for built non-source artifacts. With it, wasm-bindgen output
    /// defaults to `<artifact-dir>/browser/pkg`; otherwise it uses `<out-dir>/browser/pkg`.
    #[clap(long = "artifact-dir")]
    pub(crate) artifact_dir: Option<Utf8PathBuf>,

    /// Where to write the wasm-bindgen output tree. Defaults to
    /// `<artifact-dir>/browser/pkg` when --artifact-dir is set, otherwise `<out-dir>/browser/pkg`.
    #[clap(long = "wasm-bindgen-out-dir")]
    pub(crate) wasm_bindgen_out_dir: Option<Utf8PathBuf>,

    /// wasm-bindgen output target.
    #[clap(long = "wasm-bindgen-target", value_enum, default_value = "web")]
    pub(crate) wasm_bindgen_target: WasmBindgenTargetArg,

    /// Override the `cargo` binary to invoke.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    pub(crate) cargo_bin: String,

    /// Cargo target directory for the generated wasm host build.
    #[clap(long = "target-dir")]
    pub(crate) target_dir: Option<Utf8PathBuf>,

    /// Cargo target directory for the root-package build. Artifact
    /// coordinators always set this outside the generation mirror.
    #[clap(long = "core-target-dir")]
    pub(crate) core_target_dir: Option<Utf8PathBuf>,

    /// Build the root package and generated wasm host crate in release mode.
    #[clap(long)]
    pub(crate) release: bool,

    /// Cargo features enabled on the root package. May be repeated or comma-separated.
    #[clap(long = "cargo-feature", value_delimiter = ',')]
    pub(crate) cargo_features: Vec<String>,

    /// Do not try to format the generated bindings.
    #[clap(long, short)]
    pub(crate) no_format: bool,

    /// Path to optional uniffi config file.
    #[clap(long, short)]
    pub(crate) config: Option<Utf8PathBuf>,

    /// Optional crate filter passed through to the JS generator.
    #[clap(long = "crate")]
    pub(crate) crate_name: Option<String>,

    /// Whether we should exclude dependencies when running cargo metadata.
    #[clap(long)]
    pub(crate) metadata_no_deps: bool,
}

#[derive(Clone, Args)]
pub(crate) struct BuildNapiArgs {
    /// Root package Cargo manifest.
    #[clap(long = "manifest-path")]
    pub(crate) manifest_path: Utf8PathBuf,

    /// Directory in which to write generated JavaScript files.
    #[clap(long, short)]
    pub(crate) out_dir: Utf8PathBuf,

    /// Optional override for the library/cdylib path used for JS generation.
    /// When omitted, the command builds the root package at --manifest-path and derives
    /// the cdylib location from Cargo metadata.
    #[clap(long = "library-path")]
    pub(crate) library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the JS generator.
    /// When set, this overrides the built-library path for generation, but
    /// the root package is still built as part of the orchestration.
    #[clap(long)]
    pub(crate) source: Option<Utf8PathBuf>,

    /// Directory (default `<out-dir>/native/hosts`) in which to emit the generated napi host crate.
    #[clap(long = "host-crates-dir", default_value = "native/hosts")]
    pub(crate) host_crates_dir: Utf8PathBuf,

    /// Root of the complete generated package.
    #[clap(skip)]
    pub(crate) package_root: Utf8PathBuf,

    #[clap(skip)]
    pub(crate) logical_host_crates_dir: Option<Utf8PathBuf>,

    /// Directory for built non-source artifacts. With it, the composite addon
    /// defaults to `<artifact-dir>/node/<host-stem>.node`; without it,
    /// source-only single-component output retains its local addon fallback.
    #[clap(long = "artifact-dir")]
    pub(crate) artifact_dir: Option<Utf8PathBuf>,

    /// N-API consumption form(s) to emit. Defaults to both node and electron.
    #[clap(long = "flavor", value_enum)]
    pub(crate) flavor: Vec<NapiBuildFlavorArg>,

    /// Override the `cargo` binary to invoke.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    pub(crate) cargo_bin: String,

    /// Cargo target directory for the generated N-API host build.
    #[clap(long = "target-dir")]
    pub(crate) target_dir: Option<Utf8PathBuf>,

    /// Build the root package and generated N-API host crate in release mode.
    #[clap(long)]
    pub(crate) release: bool,

    /// Cargo features enabled on the root package. May be repeated or comma-separated.
    #[clap(long = "cargo-feature", value_delimiter = ',')]
    pub(crate) cargo_features: Vec<String>,

    /// Do not try to format the generated bindings.
    #[clap(long, short)]
    pub(crate) no_format: bool,

    /// Path to optional uniffi config file.
    #[clap(long, short)]
    pub(crate) config: Option<Utf8PathBuf>,

    /// Optional crate filter passed through to the JS generator.
    #[clap(long = "crate")]
    pub(crate) crate_name: Option<String>,

    /// Whether we should exclude dependencies when running cargo metadata.
    #[clap(long)]
    pub(crate) metadata_no_deps: bool,
}

#[cfg(feature = "cli-ohos")]
#[derive(Clone, Args)]
pub(crate) struct BuildOhosArgs {
    /// Root package Cargo manifest.
    #[clap(long = "manifest-path")]
    pub(crate) manifest_path: Utf8PathBuf,

    /// Directory in which to write generated JavaScript files.
    #[clap(long, short)]
    pub(crate) out_dir: Utf8PathBuf,

    /// Optional override for the library/cdylib path used for JS generation.
    #[clap(long = "library-path")]
    pub(crate) library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the JS generator.
    #[clap(long)]
    pub(crate) source: Option<Utf8PathBuf>,

    /// Directory (default `<out-dir>/native/hosts`) in which to emit the generated OHOS host crate.
    #[clap(long = "host-crates-dir", default_value = "native/hosts")]
    pub(crate) host_crates_dir: Utf8PathBuf,

    /// Root of the complete generated package.
    #[clap(skip)]
    pub(crate) package_root: Utf8PathBuf,

    #[clap(skip)]
    pub(crate) logical_host_crates_dir: Option<Utf8PathBuf>,

    /// Use an existing or generated composite OHOS host manifest (package or
    /// workspace) instead of the default generated composite host manifest.
    #[clap(long = "ohos-host-manifest-path")]
    pub(crate) ohos_host_manifest_path: Option<Utf8PathBuf>,

    /// Directory for built non-source artifacts. Defaults to `<host-crate>/dist` for OHOS output when omitted.
    #[clap(long = "artifact-dir")]
    pub(crate) artifact_dir: Option<Utf8PathBuf>,

    /// Output directory for built OHOS dist artifacts (intermediate native output).
    #[clap(long = "dist-dir")]
    pub(crate) dist_dir: Option<Utf8PathBuf>,

    /// OHPM package name for generated Harmony package metadata (HAR or HSP;
    /// supports scoped names like `@scope/name`).
    #[clap(long = "package-name")]
    pub(crate) package_name: Option<String>,

    /// Harmony module name override. Defaults to a stable normalization of the OHPM package name.
    #[clap(long = "module-name")]
    pub(crate) module_name: Option<String>,

    /// Semantic version override for generated Harmony package metadata.
    #[clap(long = "package-version")]
    pub(crate) package_version: Option<String>,

    /// Author override for generated Harmony package metadata.
    #[clap(long = "author")]
    pub(crate) author: Option<String>,

    /// SPDX license override for generated Harmony package metadata.
    #[clap(long = "license")]
    pub(crate) license: Option<String>,

    /// Description override for generated Harmony package metadata.
    #[clap(long = "description")]
    pub(crate) description: Option<String>,

    /// Minimum compatible Harmony/OpenHarmony SDK version. Must be explicit for final HAR/HSP packaging.
    #[clap(long = "compatible-sdk-version")]
    pub(crate) compatible_sdk_version: Option<String>,

    /// Target Harmony/OpenHarmony SDK version. Defaults to the resolved compile SDK.
    #[clap(long = "target-sdk-version")]
    pub(crate) target_sdk_version: Option<String>,

    /// Compatible SDK type, such as HarmonyOS or OpenHarmony.
    #[clap(long = "compatible-sdk-type")]
    pub(crate) compatible_sdk_type: Option<String>,

    /// Supported Harmony device type. May be repeated or comma-separated.
    #[clap(long = "device-type", value_delimiter = ',')]
    pub(crate) device_types: Vec<String>,

    /// Final Harmony package kind. HAR is the default; choose HSP explicitly.
    #[clap(long = "package-type", value_enum, default_value = "har")]
    pub(crate) package_kind: super::ohos::PackageKind,

    /// Build an app-independent integrated HSP. Only valid with --package-type hsp.
    #[clap(long = "integrated-hsp")]
    pub(crate) integrated_hsp: bool,

    /// Host application bundleName for a non-integrated HSP.
    #[clap(long = "hsp-bundle-name")]
    pub(crate) hsp_bundle_name: Option<String>,

    /// Output `.har` path. Defaults to `<artifact-root>/<package>.har`.
    #[clap(long = "har-out")]
    pub(crate) har_out: Option<Utf8PathBuf>,

    /// Standalone runtime HSP extracted byte-for-byte from the release tgz.
    #[clap(long = "runtime-hsp-out")]
    pub(crate) runtime_hsp_out: Option<Utf8PathBuf>,

    /// Standalone Interface HAR extracted byte-for-byte from the release tgz.
    #[clap(long = "interface-har-out")]
    pub(crate) interface_har_out: Option<Utf8PathBuf>,

    /// Original release tgz emitted by Hvigor assembleHsp.
    #[clap(long = "tgz-out")]
    pub(crate) tgz_out: Option<Utf8PathBuf>,

    /// Hvigor wrapper used to build the final compiled HAR (falls back to HVIGORW or PATH).
    #[clap(long = "hvigorw")]
    pub(crate) hvigorw: Option<String>,

    /// OHPM executable used to resolve and prepublish the final Harmony package (falls back to OHPM or PATH).
    #[clap(long = "ohpm")]
    pub(crate) ohpm: Option<String>,

    /// DevEco SDK root used by Hvigor (falls back to DEVECO_SDK_HOME).
    #[clap(long = "deveco-sdk-home")]
    pub(crate) deveco_sdk_home: Option<Utf8PathBuf>,

    /// Skip final HAR packaging and keep only `dist/` intermediate outputs.
    #[clap(long = "no-har")]
    pub(crate) no_har: bool,

    /// OHOS architecture alias for the built-in OHOS builder. Defaults to `aarch` and `x64`.
    #[clap(long = "arch")]
    pub(crate) arch: Vec<String>,

    /// Override the `cargo` binary used for the initial metadata/source build.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    pub(crate) cargo_bin: String,

    /// Cargo target directory for the generated OHOS host build.
    #[clap(long = "target-dir")]
    pub(crate) target_dir: Option<Utf8PathBuf>,

    /// Cargo features enabled on the root package. May be repeated or comma-separated.
    #[clap(long = "cargo-feature", value_delimiter = ',')]
    pub(crate) cargo_features: Vec<String>,

    /// Copy static `.a` libraries in addition to shared `.so` artifacts.
    #[clap(long = "static")]
    pub(crate) copy_static: bool,

    /// Skip copying native libraries; still generate TypeScript declarations.
    #[clap(long = "skip-libs")]
    pub(crate) skip_libs: bool,

    /// Skip napi-ohos version checks.
    #[clap(long = "skip-check")]
    pub(crate) skip_check: bool,

    /// Use `cargo zigbuild` for the generated OHOS host crate.
    #[clap(long = "zigbuild")]
    pub(crate) zigbuild: bool,

    /// Use HarmonyOS BiSheng toolchain paths instead of OpenHarmony LLVM paths.
    #[clap(long = "bisheng")]
    pub(crate) bisheng: bool,

    /// Package to build when the OHOS manifest is a workspace root.
    #[clap(long = "package", short = 'p')]
    pub(crate) package: Option<String>,

    /// Skip the check that candidate packages depend on napi-derive-ohos.
    #[clap(long = "skip-napi-check")]
    pub(crate) skip_napi_check: bool,

    /// SONAME linker value for the generated shared library.
    #[clap(long = "soname")]
    pub(crate) soname: Option<String>,

    /// Build the root package and generated OHOS host crate in release mode.
    #[clap(long)]
    pub(crate) release: bool,

    /// Do not try to format the generated bindings.
    #[clap(long, short)]
    pub(crate) no_format: bool,

    /// Path to optional uniffi config file.
    #[clap(long, short)]
    pub(crate) config: Option<Utf8PathBuf>,

    /// Optional crate filter passed through to the JS generator.
    #[clap(long = "crate")]
    pub(crate) crate_name: Option<String>,

    /// Whether we should exclude dependencies when running cargo metadata.
    #[clap(long)]
    pub(crate) metadata_no_deps: bool,

    /// Additional cargo args passed to the OHOS host cargo build after `--`.
    #[clap(last = true)]
    pub(crate) cargo_args: Vec<String>,
}

#[derive(Copy, Clone, ValueEnum)]
pub(crate) enum NapiBuildFlavorArg {
    Napi,
    Electron,
}

impl From<NapiBuildFlavorArg> for FlavorTarget {
    fn from(value: NapiBuildFlavorArg) -> Self {
        match value {
            NapiBuildFlavorArg::Napi => FlavorTarget::Napi,
            NapiBuildFlavorArg::Electron => FlavorTarget::Electron,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub(crate) enum WasmBindgenTargetArg {
    Web,
    Nodejs,
    Bundler,
    NoModules,
    Deno,
}

pub(crate) fn run(args: JavascriptArgs) -> Result<()> {
    match args.command {
        JavascriptCommands::Build(args) => build(args),
        JavascriptCommands::BuildWasm(args) => build_wasm(args),
        JavascriptCommands::BuildNapi(args) => build_napi(args),
        #[cfg(feature = "cli-ohos")]
        JavascriptCommands::BuildOhos(args) => build_ohos(args),
    }
}

pub(crate) fn generate_js(
    manifest_path: &Utf8Path,
    source: Utf8PathBuf,
    out_dir: Utf8PathBuf,
    package_root: Utf8PathBuf,
    config: Option<Utf8PathBuf>,
    crate_name: Option<String>,
    metadata_no_deps: bool,
    no_format: bool,
    host_crates: HostCrateOptions,
    flavors: Vec<FlavorTarget>,
    artifact_dir: Option<Utf8PathBuf>,
) -> Result<uniffi_bindgen_javascript::package::GeneratedPackage> {
    let mut paths = BindgenPaths::default();
    let global_config = if let Some(cfg) = &config {
        let (global_config, crate_roots_layer) = GlobalConfig::from_file(cfg)?;
        if let Some(layer) = crate_roots_layer {
            paths.add_layer(layer);
        }
        global_config
    } else {
        GlobalConfig::default()
    };
    let mut cargo_metadata = MetadataCommand::new();
    cargo_metadata.manifest_path(manifest_path.as_std_path());
    if metadata_no_deps {
        cargo_metadata.no_deps();
    }
    let metadata = cargo_metadata
        .exec()
        .with_context(|| format!("running cargo metadata for {manifest_path}"))?;
    paths.add_layer(CrateConfigSupplier::from_cargo_metadata(
        metadata,
        CargoMetadataOptions {
            no_deps: metadata_no_deps,
            ..CargoMetadataOptions::default()
        },
    ));
    let loader = BindgenLoader::new(paths, global_config);
    let package = uniffi_bindgen_javascript::generate_package(
        &loader,
        GenerateJsOptions {
            source,
            out_dir,
            package_root,
            artifact_dir,
            config_override: config,
            crate_filter: crate_name,
            metadata_no_deps,
            flavors,
            host_crates,
        },
    )?;
    if no_format {
        // keep compatibility with existing GenerateJsOptions shape; formatting
        // is not yet a configurable concern for the JS target.
    }
    Ok(package)
}

fn build(args: BuildArgs) -> Result<()> {
    // Prepare one complete package before either target builder runs.  The
    // builders below only consume this frozen value; they never invoke the
    // JavaScript generator a second time or infer names from generated host
    // manifests.
    let manifest_path = canonicalize_or_keep(&args.manifest_path);
    let core_meta = cargo_package_metadata(&manifest_path)?;
    let mut build_core =
        cargo_build_command(&args.cargo_bin, &manifest_path, &[], args.release, None);
    add_cargo_feature_args(&mut build_core, &args.cargo_features);
    run_command(
        &args.cargo_bin,
        &mut build_core,
        "cargo",
        "install Rust's cargo toolchain or pass --cargo-bin <path>",
    )?;
    let generation_source = if let Some(source) = &args.source {
        canonicalize_or_keep(source)
    } else {
        let library_path = args
            .library_path
            .clone()
            .map(|path| canonicalize_or_keep(&path))
            .unwrap_or_else(|| core_meta.host_cdylib_path(args.release));
        if !library_path.exists() {
            bail!(
                "built library not found at {}. Ensure the downstream crate declares a cdylib target, pass --library-path <path>, or pass --source <udl-or-library>",
                library_path
            );
        }
        library_path
    };
    let host_crates_dir = if args.host_crates_dir == Utf8Path::new("native/hosts") {
        args.out_dir.join("native/hosts")
    } else {
        args.host_crates_dir.clone()
    };
    let mut flavors = vec![FlavorTarget::Wasm];
    if args.napi_flavor.is_empty() {
        flavors.extend([FlavorTarget::Napi, FlavorTarget::Electron]);
    } else {
        flavors.extend(args.napi_flavor.iter().copied().map(FlavorTarget::from));
    }
    let package = generate_js(
        &manifest_path,
        generation_source,
        args.out_dir.clone(),
        args.out_dir.clone(),
        args.config.clone(),
        args.crate_name.clone(),
        args.metadata_no_deps,
        args.no_format,
        HostCrateOptions {
            manifest_path: manifest_path.clone(),
            host_crates_dir,
            logical_host_crates_dir: None,
        },
        flavors,
        args.artifact_dir.clone(),
    )?;
    build_wasm_with_generation(args.wasm_args(), false, Some(&package))
        .context("building JavaScript wasm target")?;
    build_napi_with_generation(args.napi_args(), false, Some(&package))
        .context("building JavaScript N-API target")?;
    Ok(())
}

pub(crate) fn build_wasm(args: BuildWasmArgs) -> Result<()> {
    build_wasm_with_generation(args, true, None)
}

/// Build a wasm host whose package sources have already been prepared by the
/// managed artifact coordinator.  This path performs no second JavaScript
/// generation pass.
pub(crate) fn build_wasm_prepared(
    args: BuildWasmArgs,
    package: &uniffi_bindgen_javascript::package::GeneratedPackage,
) -> Result<()> {
    build_wasm_with_generation(args, false, Some(package))
}

fn build_wasm_with_generation(
    mut args: BuildWasmArgs,
    prepare_generation: bool,
    prepared_package: Option<&uniffi_bindgen_javascript::package::GeneratedPackage>,
) -> Result<()> {
    normalize_default_package_root(&mut args.package_root, &args.out_dir);
    normalize_default_host_crates_dir(&mut args.host_crates_dir, &args.package_root);
    validate_package_paths(
        &args.package_root,
        &args.out_dir,
        &args.host_crates_dir,
        args.artifact_dir.as_deref(),
        args.wasm_bindgen_out_dir.as_deref(),
    )?;
    let workspace = (args.core_target_dir.is_none() || args.target_dir.is_none())
        .then(|| {
            super::artifact_staging::TemporaryWorkspace::create("uniffi-javascript-wasm-invocation")
        })
        .transpose()
        .context("creating standalone JavaScript wasm build roots")?;
    let build_root = workspace
        .as_ref()
        .map(|workspace| workspace.build_root().join("wasm"));
    if args.core_target_dir.is_none() {
        args.core_target_dir = Some(
            build_root
                .as_ref()
                .expect("missing wasm target has a temporary workspace")
                .join("core"),
        );
    }
    if args.target_dir.is_none() {
        args.target_dir = Some(
            build_root
                .as_ref()
                .expect("missing wasm target has a temporary workspace")
                .join("host"),
        );
    }
    preflight_wasm_build_paths(&args)?;
    let result = build_wasm_inner(args, prepare_generation, prepared_package);
    drop(workspace);
    result
}

fn validate_real_wasm_directory(path: &Utf8Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading wasm target directory {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("wasm target path component must be a real directory: {path}");
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            bail!("wasm target path component must not be a reparse point: {path}");
        }
    }
    Ok(())
}

fn wasm_absolute_lexical_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path
        .components()
        .any(|component| matches!(component.as_str(), "." | ".."))
    {
        bail!("wasm target/output paths must not contain `.` or `..`: {path}");
    }
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|path| anyhow::anyhow!("cwd is not utf8: {}", path.display()))?
            .join(path)
    };
    Ok(path)
}

#[cfg(windows)]
fn windows_wasm_semantic_path_key(path: &Utf8Path) -> Result<String> {
    use std::path::{Component, Prefix};

    let mut components = path.as_std_path().components();
    let mut key = String::new();
    match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                key.push_str("disk:");
                key.push((drive as char).to_ascii_lowercase());
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                key.push_str("unc:");
                key.push_str(&server.to_string_lossy().to_lowercase());
                key.push('/');
                key.push_str(&share.to_string_lossy().to_lowercase());
            }
            Prefix::DeviceNS(device) => {
                key.push_str("device:");
                key.push_str(&device.to_string_lossy().to_lowercase());
            }
            Prefix::Verbatim(value) => {
                key.push_str("verbatim:");
                key.push_str(&value.to_string_lossy().to_lowercase());
            }
        },
        Some(other) => {
            bail!("Windows wasm path has no DOS/UNC prefix: {path} ({other:?})");
        }
        None => bail!("Windows wasm path is empty"),
    }
    for component in components {
        match component {
            Component::RootDir => {}
            Component::Normal(value) => {
                key.push('/');
                key.push_str(&value.to_string_lossy().to_lowercase());
            }
            Component::CurDir | Component::ParentDir => {
                bail!("Windows wasm path key contains a relative component: {path}")
            }
            Component::Prefix(_) => bail!("Windows wasm path has a nested prefix: {path}"),
        }
    }
    Ok(key)
}

fn wasm_preflight_nofollow(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let path = wasm_absolute_lexical_path(path)?;
    let mut current = path.as_path();
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    bail!("wasm target/output path traverses a symlink: {current}");
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        bail!("wasm target/output path traverses a reparse point: {current}");
                    }
                }
                if !missing.is_empty() && !metadata.is_dir() {
                    bail!("wasm target/output ancestor is not a directory: {current}");
                }
                let canonical = current
                    .canonicalize_utf8()
                    .with_context(|| format!("canonicalizing wasm path ancestor {current}"))?;
                let expected_macos_alias = if cfg!(target_os = "macos") {
                    [
                        (Utf8Path::new("/var"), Utf8Path::new("/private/var")),
                        (Utf8Path::new("/tmp"), Utf8Path::new("/private/tmp")),
                    ]
                    .into_iter()
                    .any(|(logical, physical)| {
                        current
                            .strip_prefix(logical)
                            .is_ok_and(|suffix| canonical == physical.join(suffix))
                    })
                } else {
                    false
                };
                #[cfg(windows)]
                let canonical_matches_current = windows_wasm_semantic_path_key(&canonical)?
                    == windows_wasm_semantic_path_key(current)?;
                #[cfg(not(windows))]
                let canonical_matches_current = canonical == current;
                if !canonical_matches_current && !expected_macos_alias {
                    bail!(
                        "wasm target/output path has a non-canonical or aliased ancestor: {current} -> {canonical}"
                    );
                }
                let mut resolved = canonical;
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(
                    current
                        .file_name()
                        .with_context(|| format!("wasm path has no existing ancestor: {path}"))?
                        .to_string(),
                );
                current = current
                    .parent()
                    .with_context(|| format!("wasm path has no existing ancestor: {path}"))?;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("preflighting wasm path {current}"))
            }
        }
    }
}

fn wasm_paths_alias_or_overlap(left: &Utf8Path, right: &Utf8Path) -> Result<bool> {
    #[cfg(windows)]
    {
        let left = Utf8PathBuf::from(windows_wasm_semantic_path_key(left)?);
        let right = Utf8PathBuf::from(windows_wasm_semantic_path_key(right)?);
        return Ok(left == right || left.starts_with(&right) || right.starts_with(&left));
    }
    #[cfg(not(windows))]
    let normalize = |path: &Utf8Path| {
        let value = path.as_str().replace('\\', "/");
        if cfg!(target_os = "macos") {
            Utf8PathBuf::from(value.to_ascii_lowercase())
        } else {
            Utf8PathBuf::from(value)
        }
    };
    #[cfg(not(windows))]
    let left = normalize(left);
    #[cfg(not(windows))]
    let right = normalize(right);
    #[cfg(not(windows))]
    Ok(left == right || left.starts_with(&right) || right.starts_with(&left))
}

fn validate_existing_wasm_directory_endpoint(path: &Utf8Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => {
            validate_real_wasm_directory(path)?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("preflighting wasm endpoint {path}")),
    }
}

fn materialize_wasm_directories(paths: &[Utf8PathBuf]) -> Result<()> {
    // Validate every endpoint before the first mutation. In particular, an
    // existing regular file at a later endpoint must not leave earlier
    // generated/output roots behind.
    for path in paths {
        validate_existing_wasm_directory_endpoint(path)?;
    }

    for path in paths {
        std::fs::create_dir_all(path).with_context(|| format!("creating wasm directory {path}"))?;
        validate_real_wasm_directory(path)?;
    }
    Ok(())
}

fn preflight_wasm_build_paths(args: &BuildWasmArgs) -> Result<()> {
    let core = wasm_preflight_nofollow(
        args.core_target_dir
            .as_deref()
            .context("wasm core target directory was not resolved")?,
    )?;
    let host = wasm_preflight_nofollow(
        args.target_dir
            .as_deref()
            .context("wasm host target directory was not resolved")?,
    )?;
    let mut published = vec![
        wasm_preflight_nofollow(&args.out_dir)?,
        wasm_preflight_nofollow(&args.host_crates_dir)?,
    ];
    for path in [
        args.artifact_dir.as_deref(),
        args.wasm_bindgen_out_dir.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        published.push(wasm_preflight_nofollow(path)?);
    }
    if wasm_paths_alias_or_overlap(&core, &host)? {
        bail!(
                "wasm isolated paths alias or overlap: core Cargo target `{core}` vs host Cargo target `{host}`"
            );
    }
    for output in &published {
        for (label, target) in [
            ("core Cargo target", core.as_path()),
            ("host Cargo target", host.as_path()),
        ] {
            if wasm_paths_alias_or_overlap(target, output)? {
                bail!(
                        "wasm isolated paths alias or overlap: {label} `{target}` vs generated/published output `{output}`"
                    );
            }
        }
    }
    // Preflight every endpoint before creating any missing directory.
    let materialized = published
        .iter()
        .cloned()
        .chain([core.clone(), host.clone()])
        .collect::<Vec<_>>();
    materialize_wasm_directories(&materialized)?;
    Ok(())
}

fn build_wasm_inner(
    args: BuildWasmArgs,
    prepare_generation: bool,
    prepared_package: Option<&uniffi_bindgen_javascript::package::GeneratedPackage>,
) -> Result<()> {
    let manifest_path = canonicalize_or_keep(&args.manifest_path);
    let core_meta = cargo_package_metadata(&manifest_path)?;

    let core_target_dir = args
        .core_target_dir
        .as_ref()
        .map(|path| resolve_cwd_path(path))
        .transpose()?;
    let target_dir = args.target_dir.as_ref().map(resolve_cwd_path).transpose()?;
    if let (Some(core), Some(host)) = (&core_target_dir, &target_dir) {
        if wasm_paths_alias_or_overlap(core, host)? {
            bail!("wasm core and generated-host target directories must not alias or overlap");
        }
        let mut published = vec![
            resolve_cwd_path(&args.out_dir)?,
            resolve_cwd_path(&args.host_crates_dir)?,
        ];
        for path in [
            args.artifact_dir.as_ref(),
            args.wasm_bindgen_out_dir.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            published.push(resolve_cwd_path(path)?);
        }
        for path in published {
            if wasm_paths_alias_or_overlap(core, &path)?
                || wasm_paths_alias_or_overlap(host, &path)?
            {
                bail!("wasm Cargo target directory aliases generated/published output: {path}");
            }
        }
    }

    let mut build_core =
        cargo_build_command(&args.cargo_bin, &manifest_path, &[], args.release, None);
    add_cargo_feature_args(&mut build_core, &args.cargo_features);
    if let Some(target_dir) = &core_target_dir {
        build_core
            .env("CARGO_TARGET_DIR", target_dir.as_str())
            .env("CARGO_INCREMENTAL", "0");
    }
    run_command(
        &args.cargo_bin,
        &mut build_core,
        "cargo",
        "install Rust's cargo toolchain or pass --cargo-bin <path>",
    )?;
    let generation_source = if let Some(source) = &args.source {
        canonicalize_or_keep(source)
    } else {
        let library_path = args
            .library_path
            .clone()
            .map(|p| canonicalize_or_keep(&p))
            .unwrap_or_else(|| {
                core_target_dir
                    .as_deref()
                    .map(|target| core_meta.host_cdylib_path_in(target, args.release))
                    .unwrap_or_else(|| core_meta.host_cdylib_path(args.release))
            });
        if !library_path.exists() {
            bail!(
                "built library not found at {}. Ensure the downstream crate declares a cdylib target, pass --library-path <path>, or pass --source <udl-or-library>",
                library_path
            );
        }
        library_path
    };

    // A standalone invocation creates its package plan here.  The managed
    // coordinator passes the exact plan it already prepared, so no second
    // generation pass (and no rediscovery from generated files) is possible.
    let owned_package = if prepared_package.is_none() && prepare_generation {
        Some(generate_js(
            &manifest_path,
            generation_source,
            args.out_dir.clone(),
            args.package_root.clone(),
            args.config.clone(),
            args.crate_name.clone(),
            args.metadata_no_deps,
            args.no_format,
            HostCrateOptions {
                manifest_path: manifest_path.clone(),
                host_crates_dir: args.host_crates_dir.clone(),
                logical_host_crates_dir: args.logical_host_crates_dir.clone(),
            },
            vec![FlavorTarget::Wasm],
            args.artifact_dir.clone(),
        )?)
    } else {
        None
    };
    let package = prepared_package
        .or(owned_package.as_ref())
        .context("wasm build is missing the frozen generated package plan")?;

    let wasm_spec = package
        .wasm_host_spec()
        .context("Wasm build package has no browser host build spec")?;
    let wasm_manifest = host_manifest_path(&args.package_root, wasm_spec)?;
    if !wasm_manifest.exists() {
        bail!(
            "wasm host crate was not emitted at {}",
            wasm_manifest
                .parent()
                .unwrap_or_else(|| Utf8Path::new("<unknown>"))
        );
    }

    let wasm_cargo_args = host_feature_args(wasm_spec, &args.cargo_features);
    let wasm_cargo_args = wasm_cargo_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut build_wasm_host = cargo_build_command(
        &args.cargo_bin,
        &wasm_manifest,
        &wasm_cargo_args,
        args.release,
        None,
    );
    build_wasm_host
        .arg("--target")
        .arg("wasm32-unknown-unknown");
    if let Some(target_dir) = &target_dir {
        build_wasm_host
            .env("CARGO_TARGET_DIR", target_dir.as_str())
            .env("CARGO_INCREMENTAL", "0");
    }
    run_command(
        &args.cargo_bin,
        &mut build_wasm_host,
        "cargo",
        "install Rust's wasm32-unknown-unknown target with `rustup target add wasm32-unknown-unknown`",
    )?;
    let wasm_artifact = wasm_artifact_path_from_spec(
        &args.package_root,
        wasm_spec,
        target_dir.as_deref(),
        args.release,
    );
    if !wasm_artifact.exists() {
        bail!(
            "built wasm artifact not found at {} after cargo build",
            wasm_artifact
        );
    }

    let wasm_bindgen_out_dir = args
        .wasm_bindgen_out_dir
        .clone()
        .or_else(|| {
            args.artifact_dir
                .as_ref()
                .map(|dir| dir.join("browser/pkg"))
        })
        .unwrap_or_else(|| args.out_dir.join("browser/pkg"));
    std::fs::create_dir_all(&wasm_bindgen_out_dir)
        .with_context(|| format!("creating wasm-bindgen output dir {wasm_bindgen_out_dir}"))?;

    package.emit_wasm_post_link(
        &wasm_artifact,
        &wasm_spec.lib_target,
        match args.wasm_bindgen_target {
            WasmBindgenTargetArg::Web => WasmPostLinkTarget::Web,
            WasmBindgenTargetArg::Nodejs => WasmPostLinkTarget::Node,
            WasmBindgenTargetArg::Bundler => WasmPostLinkTarget::Bundler,
            WasmBindgenTargetArg::NoModules | WasmBindgenTargetArg::Deno => {
                bail!(
                    "wasm post-link target {:?} is not supported by the UniFFI engine",
                    args.wasm_bindgen_target
                )
            }
        },
        &wasm_bindgen_out_dir,
    )?;

    Ok(())
}

pub(crate) fn build_napi(args: BuildNapiArgs) -> Result<()> {
    build_napi_with_generation(args, true, None)
}

/// Build a N-API host whose package files were prepared by the artifact
/// coordinator; no second JavaScript generation is performed.
pub(crate) fn build_napi_prepared(
    args: BuildNapiArgs,
    package: &uniffi_bindgen_javascript::package::GeneratedPackage,
) -> Result<()> {
    build_napi_with_generation(args, false, Some(package))
}

fn build_napi_with_generation(
    mut args: BuildNapiArgs,
    prepare_generation: bool,
    prepared_package: Option<&uniffi_bindgen_javascript::package::GeneratedPackage>,
) -> Result<()> {
    normalize_default_package_root(&mut args.package_root, &args.out_dir);
    normalize_default_host_crates_dir(&mut args.host_crates_dir, &args.package_root);
    validate_package_paths(
        &args.package_root,
        &args.out_dir,
        &args.host_crates_dir,
        args.artifact_dir.as_deref(),
        None,
    )?;
    let manifest_path = canonicalize_or_keep(&args.manifest_path);
    let core_meta = cargo_package_metadata(&manifest_path)?;

    let mut build_core =
        cargo_build_command(&args.cargo_bin, &manifest_path, &[], args.release, None);
    add_cargo_feature_args(&mut build_core, &args.cargo_features);
    run_command(
        &args.cargo_bin,
        &mut build_core,
        "cargo",
        "install Rust's cargo toolchain or pass --cargo-bin <path>",
    )?;

    let generation_source = if let Some(source) = &args.source {
        canonicalize_or_keep(source)
    } else {
        let library_path = args
            .library_path
            .clone()
            .map(|p| canonicalize_or_keep(&p))
            .unwrap_or_else(|| core_meta.host_cdylib_path(args.release));
        if !library_path.exists() {
            bail!(
                "built library not found at {}. Ensure the downstream crate declares a cdylib target, pass --library-path <path>, or pass --source <udl-or-library>",
                library_path
            );
        }
        library_path
    };

    let flavors = if args.flavor.is_empty() {
        vec![FlavorTarget::Napi, FlavorTarget::Electron]
    } else {
        args.flavor
            .iter()
            .copied()
            .map(FlavorTarget::from)
            .collect()
    };

    let owned_package = if prepared_package.is_none() && prepare_generation {
        Some(generate_js(
            &manifest_path,
            generation_source,
            args.out_dir.clone(),
            args.package_root.clone(),
            args.config.clone(),
            args.crate_name.clone(),
            args.metadata_no_deps,
            args.no_format,
            HostCrateOptions {
                manifest_path: manifest_path.clone(),
                host_crates_dir: args.host_crates_dir.clone(),
                logical_host_crates_dir: args.logical_host_crates_dir.clone(),
            },
            flavors.clone(),
            args.artifact_dir.clone(),
        )?)
    } else {
        None
    };
    let package = prepared_package
        .or(owned_package.as_ref())
        .context("N-API build is missing the frozen generated package plan")?;
    let napi_spec = package
        .node_host_spec()
        .context("N-API build package has no Node host build spec")?;
    let napi_manifest = host_manifest_path(&args.package_root, napi_spec)?;
    if !napi_manifest.exists() {
        bail!(
            "napi host crate was not emitted at {}",
            napi_manifest
                .parent()
                .unwrap_or_else(|| Utf8Path::new("<unknown>"))
        );
    }

    let target_dir = args.target_dir.as_ref().map(resolve_cwd_path).transpose()?;
    let napi_cargo_args = host_feature_args(napi_spec, &args.cargo_features);
    let napi_cargo_args = napi_cargo_args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    let mut build_napi_host = cargo_build_command(
        &args.cargo_bin,
        &napi_manifest,
        &napi_cargo_args,
        args.release,
        None,
    );
    if let Some(target_dir) = &target_dir {
        build_napi_host
            .env("CARGO_TARGET_DIR", target_dir.as_str())
            .env("CARGO_INCREMENTAL", "0");
    }
    run_command(
        &args.cargo_bin,
        &mut build_napi_host,
        "cargo",
        "install Rust's cargo toolchain or pass --cargo-bin <path>",
    )?;

    let napi_artifact = host_cdylib_path_from_spec(
        &args.package_root,
        napi_spec,
        target_dir.as_deref(),
        args.release,
    );
    if !napi_artifact.exists() {
        bail!(
            "built napi artifact not found at {} after cargo build",
            napi_artifact
        );
    }

    if !flavors
        .iter()
        .any(|flavor| matches!(flavor, FlavorTarget::Napi | FlavorTarget::Electron))
    {
        bail!("N-API build selected no N-API/Electron generation flavors");
    }
    let addon_path = package_path(&args.package_root, &napi_spec.native_artifact)?;
    let addon_dir = addon_path
        .parent()
        .map(Utf8Path::to_path_buf)
        .context("N-API package artifact path has no parent")?;
    std::fs::create_dir_all(&addon_dir)
        .with_context(|| format!("creating composite addon output dir {addon_dir}"))?;
    std::fs::copy(&napi_artifact, &addon_path).with_context(|| {
        format!("copying built composite napi addon {napi_artifact} to {addon_path}")
    })?;

    Ok(())
}

#[cfg(feature = "cli-ohos")]
pub(crate) fn build_ohos(args: BuildOhosArgs) -> Result<()> {
    build_ohos_with_generation(args, true, None)
}

/// Build Harmony outputs from an already prepared package.
#[cfg(feature = "cli-ohos")]
pub(crate) fn build_ohos_prepared(
    args: BuildOhosArgs,
    package: &uniffi_bindgen_javascript::package::GeneratedPackage,
) -> Result<()> {
    build_ohos_with_generation(args, false, Some(package))
}

#[cfg(feature = "cli-ohos")]
fn build_ohos_with_generation(
    mut args: BuildOhosArgs,
    prepare_generation: bool,
    prepared_package: Option<&uniffi_bindgen_javascript::package::GeneratedPackage>,
) -> Result<()> {
    normalize_default_package_root(&mut args.package_root, &args.out_dir);
    normalize_default_host_crates_dir(&mut args.host_crates_dir, &args.package_root);
    validate_package_paths(
        &args.package_root,
        &args.out_dir,
        &args.host_crates_dir,
        args.artifact_dir.as_deref(),
        None,
    )?;
    if let Some(dist_dir) = args.dist_dir.as_deref() {
        validate_package_child(&args.package_root, dist_dir, "OHOS dist")?;
    }
    if args.package_kind == super::ohos::PackageKind::Hsp {
        return build_direct_ohos_hsp(args, prepared_package);
    }
    if let Some(prepared) =
        build_ohos_internal_with_generation(args, false, prepare_generation, prepared_package)?
    {
        prepared.commit()?;
    }
    Ok(())
}

/// Build every direct JavaScript HSP output in a private source/host mirror,
/// then replace the completed public outputs from ordinary sibling staging.
#[cfg(feature = "cli-ohos")]
fn build_direct_ohos_hsp(
    public: BuildOhosArgs,
    _prepared_package: Option<&uniffi_bindgen_javascript::package::GeneratedPackage>,
) -> Result<()> {
    super::ohos::preflight_hsp_frontend(super::ohos::HspFrontendPreflight {
        package_kind: public.package_kind,
        integrated_hsp: public.integrated_hsp,
        hsp_bundle_name: public.hsp_bundle_name.as_deref(),
        has_har_output: public.har_out.is_some(),
        has_hsp_output: public.runtime_hsp_out.is_some()
            || public.interface_har_out.is_some()
            || public.tgz_out.is_some(),
        no_har: public.no_har,
        skip_libs: public.skip_libs,
        compatible_sdk_version: public.compatible_sdk_version.as_deref(),
        target_sdk_version: public.target_sdk_version.as_deref(),
        compatible_sdk_type: public.compatible_sdk_type.as_deref(),
        bisheng: public.bisheng,
        hvigorw: public.hvigorw.as_deref(),
        ohpm: public.ohpm.as_deref(),
        deveco_sdk_home: public.deveco_sdk_home.as_deref(),
    })?;
    super::ohos::preflight_hsp_arches(&public.arch)
        .context("validating Harmony HSP architectures before publication planning")?;
    let outputs = planned_direct_ohos_hsp_outputs(&public)?;
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|path| anyhow::anyhow!("cwd is not utf8: {}", path.display()))?;
    let public_out = if public.out_dir.is_absolute() {
        public.out_dir.clone()
    } else {
        cwd.join(&public.out_dir)
    };
    let public_host = if public.host_crates_dir.is_absolute() {
        public.host_crates_dir.clone()
    } else {
        cwd.join(&public.host_crates_dir)
    };
    let public_ohos = public_host.join("ohos");
    let mut specifications = vec![super::artifact_staging::InvocationOutputSpec {
        label: "generated JavaScript source root".into(),
        path: public_out,
        is_directory: true,
    }];
    specifications.push(super::artifact_staging::InvocationOutputSpec {
        label: "OHOS native engine adapter".into(),
        path: public.package_root.join("native/ohos.rs"),
        is_directory: false,
    });
    for (label, relative, is_directory) in [
        ("OHOS host Cargo manifest", "Cargo.toml", false),
        ("OHOS host Cargo lock", "Cargo.lock", false),
        ("OHOS host build script", "build.rs", false),
        ("OHOS host source", "src", true),
    ] {
        specifications.push(super::artifact_staging::InvocationOutputSpec {
            label: label.into(),
            path: public_ohos.join(relative),
            is_directory,
        });
    }
    let invocation =
        super::artifact_staging::TemporaryWorkspace::create("uniffi-javascript-hsp-invocation")
            .context("creating private JavaScript HSP invocation")?;
    let mirror = invocation.mirror_root().to_path_buf();
    let build_root = invocation.build_root().to_path_buf();
    let private_out = mirror.join("generated");
    let private_host = mirror.join("host");
    let private_ohos = private_host.join("ohos");
    let mut private = public.clone();
    private.package_root = invocation.mirror_root().to_path_buf();
    private.out_dir = private_out.clone();
    private.host_crates_dir = private_host;
    private.logical_host_crates_dir = Some(public_host.clone());
    private.target_dir = Some(build_root.join("ohos"));

    let mut sources = vec![private_out, private.package_root.join("native/ohos.rs")];
    for relative in ["Cargo.toml", "Cargo.lock", "build.rs", "src"] {
        sources.push(private_ohos.join(relative));
    }
    // The direct HSP path rebuilds the private mirror with rebased package
    // paths, so a package prepared for the public root cannot be reused here.
    // The ordinary (non-HSP) coordinator is the path that shares a package.
    let prepared = build_ohos_internal_with_generation(private, true, true, None)?
        .context("private JavaScript HSP build did not return a deferred generation")?;
    if prepared.output_paths() != outputs {
        bail!("direct JavaScript HSP output plan changed during private generation");
    }
    let staged_sources = sources
        .iter()
        .zip(&specifications)
        .map(|(source, destination)| {
            (
                source.as_path(),
                destination.path.as_path(),
                destination.is_directory,
            )
        })
        .collect::<Vec<_>>();
    super::artifact_staging::publish_simple_output_set(staged_sources)
        .context("publishing ordinary staged JavaScript outputs")?;
    prepared
        .commit()
        .context("publishing ordinary staged Harmony HSP outputs")
}

#[cfg(feature = "cli-ohos")]
fn planned_direct_ohos_hsp_outputs(
    args: &BuildOhosArgs,
) -> Result<Vec<super::artifact_staging::HspOutputPaths>> {
    let manifest_path = canonicalize_or_keep(&args.manifest_path);
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|path| anyhow::anyhow!("cwd is not utf8: {}", path.display()))?;
    let host_root = if args.host_crates_dir.is_absolute() {
        args.host_crates_dir.clone()
    } else {
        cwd.join(&args.host_crates_dir)
    };
    let ohos_dir = host_root.join("ohos");
    let dist_dir = args
        .dist_dir
        .clone()
        .or_else(|| {
            args.artifact_dir
                .as_ref()
                .map(|root| root.join("ohos/dist"))
        })
        .unwrap_or_else(|| ohos_dir.join("dist"));
    let core_meta = cargo_package_metadata(&manifest_path)?;
    if let Some(custom_manifest) = args
        .ohos_host_manifest_path
        .as_ref()
        .map(|path| canonicalize_or_keep(path))
    {
        let mut cargo_args = host_dependency_cargo_feature_args(
            &manifest_path,
            &custom_manifest,
            &args.cargo_args,
            &args.cargo_features,
        )?;
        cargo_args.extend(args.cargo_args.clone());
        let mut additional_source_roots = core_meta.local_source_roots.clone();
        additional_source_roots.push(("core-workspace".into(), core_meta.workspace_root.clone()));
        additional_source_roots.push(("generated-bindings".into(), args.out_dir.clone()));
        additional_source_roots.push(("generated-package-root".into(), args.package_root.clone()));
        return super::ohos::planned_hsp_host_build_outputs(&super::ohos::BuildOptions {
            cargo_bin: args.cargo_bin.clone(),
            core_manifest_path: Some(manifest_path),
            additional_source_roots,
            manifest_path: custom_manifest,
            dist_dir,
            package_name: args.package_name.clone(),
            module_name: args.module_name.clone(),
            package_version: args.package_version.clone(),
            author: args.author.clone(),
            license: args.license.clone(),
            description: args.description.clone(),
            compatible_sdk_version: args.compatible_sdk_version.clone(),
            target_sdk_version: args.target_sdk_version.clone(),
            compatible_sdk_type: args.compatible_sdk_type.clone(),
            device_types: args.device_types.clone(),
            package_kind: args.package_kind,
            integrated_hsp: args.integrated_hsp,
            hsp_bundle_name: args.hsp_bundle_name.clone(),
            har_out: args.har_out.clone(),
            runtime_hsp_out: args.runtime_hsp_out.clone(),
            interface_har_out: args.interface_har_out.clone(),
            tgz_out: args.tgz_out.clone(),
            hvigorw: args.hvigorw.clone(),
            ohpm: args.ohpm.clone(),
            deveco_sdk_home: args.deveco_sdk_home.clone(),
            no_har: args.no_har,
            arches: if args.arch.is_empty() {
                vec!["aarch".into(), "x64".into()]
            } else {
                args.arch.clone()
            },
            target_dir: args.target_dir.clone(),
            release: args.release,
            cargo_args,
            copy_static: args.copy_static,
            skip_libs: args.skip_libs,
            skip_check: args.skip_check,
            zigbuild: args.zigbuild,
            bisheng: args.bisheng,
            package: args.package.clone(),
            skip_napi_check: args.skip_napi_check,
            soname: args.soname.clone(),
            frontend_hsp_preflight_done: true,
        });
    }

    let generated_host_package =
        uniffi_bindgen_javascript::host_crates::composite_host_package_name(
            &core_meta.package_name,
        );
    Ok(vec![super::ohos::planned_generated_hsp_outputs(
        super::ohos::GeneratedHspOutputPreflight {
            dist_dir: &dist_dir,
            generated_host_package_name: &generated_host_package,
            package_name: args.package_name.as_deref(),
            runtime_hsp_out: args.runtime_hsp_out.as_deref(),
            interface_har_out: args.interface_har_out.as_deref(),
            tgz_out: args.tgz_out.as_deref(),
        },
    )?])
}

#[cfg(feature = "cli-ohos")]
pub(crate) fn build_ohos_deferred_prepared(
    args: BuildOhosArgs,
    package: &uniffi_bindgen_javascript::package::GeneratedPackage,
) -> Result<super::artifact_staging::PreparedHspInvocation> {
    build_ohos_internal_with_generation(args, true, false, Some(package))?
        .context("deferred JavaScript OHOS build did not produce an HSP invocation")
}

#[cfg(feature = "cli-ohos")]
fn build_ohos_internal_with_generation(
    args: BuildOhosArgs,
    defer_hsp_publication: bool,
    prepare_generation: bool,
    prepared_package: Option<&uniffi_bindgen_javascript::package::GeneratedPackage>,
) -> Result<Option<super::artifact_staging::PreparedHspInvocation>> {
    super::ohos::preflight_hsp_frontend(super::ohos::HspFrontendPreflight {
        package_kind: args.package_kind,
        integrated_hsp: args.integrated_hsp,
        hsp_bundle_name: args.hsp_bundle_name.as_deref(),
        has_har_output: args.har_out.is_some(),
        has_hsp_output: args.runtime_hsp_out.is_some()
            || args.interface_har_out.is_some()
            || args.tgz_out.is_some(),
        no_har: args.no_har,
        skip_libs: args.skip_libs,
        compatible_sdk_version: args.compatible_sdk_version.as_deref(),
        target_sdk_version: args.target_sdk_version.as_deref(),
        compatible_sdk_type: args.compatible_sdk_type.as_deref(),
        bisheng: args.bisheng,
        hvigorw: args.hvigorw.as_deref(),
        ohpm: args.ohpm.as_deref(),
        deveco_sdk_home: args.deveco_sdk_home.as_deref(),
    })?;
    let manifest_path = canonicalize_or_keep(&args.manifest_path);
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?;
    let host_root = if args.host_crates_dir.is_absolute() {
        args.host_crates_dir.clone()
    } else {
        cwd.join(&args.host_crates_dir)
    };
    let ohos_dir = host_root.join("ohos");
    let custom_ohos_manifest = args
        .ohos_host_manifest_path
        .as_ref()
        .map(|path| canonicalize_or_keep(path));
    let dist_dir = args
        .dist_dir
        .clone()
        .or_else(|| args.artifact_dir.as_ref().map(|dir| dir.join("ohos/dist")))
        .unwrap_or_else(|| ohos_dir.join("dist"));
    let mut protected_dist_paths = vec![
        ("current working directory".to_string(), cwd),
        (
            "downstream core manifest".to_string(),
            manifest_path.clone(),
        ),
        (
            "generated JavaScript source root".to_string(),
            args.out_dir.clone(),
        ),
        ("generated OHOS host root".to_string(), host_root.clone()),
    ];
    if let Some(custom_manifest) = &custom_ohos_manifest {
        protected_dist_paths.push((
            "custom OHOS host manifest".to_string(),
            custom_manifest.clone(),
        ));
        if let Some(custom_root) = custom_manifest.parent() {
            protected_dist_paths.push((
                "custom OHOS host package or workspace".to_string(),
                custom_root.to_path_buf(),
            ));
        }
    }
    if let Some(core_root) = manifest_path.parent() {
        protected_dist_paths.push((
            "downstream core package".to_string(),
            core_root.to_path_buf(),
        ));
        protected_dist_paths.push((
            "downstream core source directory".to_string(),
            core_root.join("src"),
        ));
    }
    if let Some(target_dir) = &args.target_dir {
        protected_dist_paths.push((
            "OHOS Cargo target directory".to_string(),
            target_dir.clone(),
        ));
    }
    super::ohos::preflight_dist_output_for_generation(&dist_dir, &protected_dist_paths)
        .context("preflighting generator-owned OHOS dist before Cargo build")?;

    // Resolve Cargo metadata only after the path-only preflight above: Cargo
    // metadata may create/update a missing lockfile, so obviously dangerous
    // outputs such as `--dist-dir .` must fail before invoking Cargo at all.
    // Once metadata is available, repeat the preflight with the complete
    // workspace/local-source set before the first actual root-package build.
    let core_meta = cargo_package_metadata(&manifest_path)?;
    protected_dist_paths.push((
        "downstream Cargo workspace".to_string(),
        core_meta.workspace_root.clone(),
    ));
    protected_dist_paths.extend(
        core_meta
            .local_source_roots
            .iter()
            .map(|(name, path)| (format!("local Cargo source `{name}`"), path.clone())),
    );
    super::ohos::preflight_dist_output_for_generation(&dist_dir, &protected_dist_paths)
        .context("preflighting generator-owned OHOS dist against Cargo workspace sources")?;

    if args.package_kind == super::ohos::PackageKind::Hsp {
        if let Some(custom_manifest) = custom_ohos_manifest.as_ref() {
            let arches = if args.arch.is_empty() {
                vec!["aarch".to_string(), "x64".to_string()]
            } else {
                args.arch.clone()
            };
            let mut ohos_cargo_args = host_dependency_cargo_feature_args(
                &manifest_path,
                custom_manifest,
                &args.cargo_args,
                &args.cargo_features,
            )?;
            ohos_cargo_args.extend(args.cargo_args.clone());
            let mut additional_source_roots = core_meta.local_source_roots.clone();
            additional_source_roots
                .push(("core-workspace".into(), core_meta.workspace_root.clone()));
            additional_source_roots.push(("generated-bindings".into(), args.out_dir.clone()));
            additional_source_roots
                .push(("generated-package-root".into(), args.package_root.clone()));
            super::ohos::preflight_hsp_host_build(&super::ohos::BuildOptions {
                cargo_bin: args.cargo_bin.clone(),
                core_manifest_path: Some(manifest_path.clone()),
                additional_source_roots,
                manifest_path: custom_manifest.clone(),
                dist_dir: dist_dir.clone(),
                package_name: args.package_name.clone(),
                module_name: args.module_name.clone(),
                package_version: args.package_version.clone(),
                author: args.author.clone(),
                license: args.license.clone(),
                description: args.description.clone(),
                compatible_sdk_version: args.compatible_sdk_version.clone(),
                target_sdk_version: args.target_sdk_version.clone(),
                compatible_sdk_type: args.compatible_sdk_type.clone(),
                device_types: args.device_types.clone(),
                package_kind: args.package_kind,
                integrated_hsp: args.integrated_hsp,
                hsp_bundle_name: args.hsp_bundle_name.clone(),
                har_out: args.har_out.clone(),
                runtime_hsp_out: args.runtime_hsp_out.clone(),
                interface_har_out: args.interface_har_out.clone(),
                tgz_out: args.tgz_out.clone(),
                hvigorw: args.hvigorw.clone(),
                ohpm: args.ohpm.clone(),
                deveco_sdk_home: args.deveco_sdk_home.clone(),
                no_har: args.no_har,
                arches,
                target_dir: args.target_dir.clone(),
                release: args.release,
                cargo_args: ohos_cargo_args,
                copy_static: args.copy_static,
                skip_libs: args.skip_libs,
                skip_check: args.skip_check,
                zigbuild: args.zigbuild,
                bisheng: args.bisheng,
                package: args.package.clone(),
                skip_napi_check: args.skip_napi_check,
                soname: args.soname.clone(),
                frontend_hsp_preflight_done: true,
            })
            .context("preflighting custom multi-package HSP host before core generation")?;
        } else {
            let generated_host_package =
                uniffi_bindgen_javascript::host_crates::composite_host_package_name(
                    &core_meta.package_name,
                );
            super::ohos::preflight_generated_hsp_outputs(
                super::ohos::GeneratedHspOutputPreflight {
                    dist_dir: &dist_dir,
                    generated_host_package_name: &generated_host_package,
                    package_name: args.package_name.as_deref(),
                    runtime_hsp_out: args.runtime_hsp_out.as_deref(),
                    interface_har_out: args.interface_har_out.as_deref(),
                    tgz_out: args.tgz_out.as_deref(),
                },
            )
            .context("preflighting generated HSP output plan before core generation")?;
        }
    }

    let mut build_core =
        cargo_build_command(&args.cargo_bin, &manifest_path, &[], args.release, None);
    add_cargo_feature_args(&mut build_core, &args.cargo_features);
    run_command(
        &args.cargo_bin,
        &mut build_core,
        "cargo",
        "install Rust's cargo toolchain or pass --cargo-bin <path>",
    )?;

    let generation_source = if let Some(source) = &args.source {
        canonicalize_or_keep(source)
    } else {
        let library_path = args
            .library_path
            .clone()
            .map(|p| canonicalize_or_keep(&p))
            .unwrap_or_else(|| core_meta.host_cdylib_path(args.release));
        if !library_path.exists() {
            bail!(
                "built library not found at {}. Ensure the downstream crate declares a cdylib target, pass --library-path <path>, or pass --source <udl-or-library>",
                library_path
            );
        }
        library_path
    };

    let owned_package = if prepared_package.is_none() && prepare_generation {
        Some(generate_js(
            &manifest_path,
            generation_source,
            args.out_dir.clone(),
            args.package_root.clone(),
            args.config.clone(),
            args.crate_name.clone(),
            args.metadata_no_deps,
            args.no_format,
            HostCrateOptions {
                manifest_path: manifest_path.clone(),
                host_crates_dir: args.host_crates_dir.clone(),
                logical_host_crates_dir: args.logical_host_crates_dir.clone(),
            },
            vec![FlavorTarget::Harmony],
            args.artifact_dir.clone(),
        )?)
    } else {
        None
    };
    let package = prepared_package.or(owned_package.as_ref());
    let generated_ohos_manifest = package
        .and_then(|package| package.ohos_host_spec())
        .map(|spec| host_manifest_path(&args.package_root, spec))
        .transpose()?
        .unwrap_or_else(|| ohos_dir.join("Cargo.toml"));
    if !generated_ohos_manifest.exists() {
        bail!(
            "OHOS host crate was not emitted at {}",
            generated_ohos_manifest
                .parent()
                .unwrap_or_else(|| Utf8Path::new("<unknown>"))
        );
    }
    let ohos_manifest = custom_ohos_manifest
        .clone()
        .unwrap_or(generated_ohos_manifest);
    if !ohos_manifest.exists() {
        bail!("custom OHOS host manifest does not exist: {ohos_manifest}");
    }
    let arches = if args.arch.is_empty() {
        vec!["aarch".to_string(), "x64".to_string()]
    } else {
        args.arch.clone()
    };
    let mut ohos_cargo_args = if custom_ohos_manifest.is_some() {
        host_dependency_cargo_feature_args(
            &manifest_path,
            &ohos_manifest,
            &args.cargo_args,
            &args.cargo_features,
        )?
    } else {
        let package = package.context("OHOS build is missing the frozen generated package plan")?;
        let spec = package
            .ohos_host_spec()
            .context("OHOS package has no Harmony host build spec")?;
        host_feature_args(spec, &args.cargo_features)
    };
    ohos_cargo_args.extend(args.cargo_args);
    let mut additional_source_roots = core_meta.local_source_roots.clone();
    additional_source_roots.push(("core-workspace".into(), core_meta.workspace_root.clone()));
    additional_source_roots.push(("generated-bindings".into(), args.out_dir.clone()));
    additional_source_roots.push(("generated-package-root".into(), args.package_root.clone()));
    let options = super::ohos::BuildOptions {
        cargo_bin: args.cargo_bin.clone(),
        core_manifest_path: Some(manifest_path),
        additional_source_roots,
        manifest_path: ohos_manifest,
        dist_dir,
        package_name: args.package_name,
        module_name: args.module_name,
        package_version: args.package_version,
        author: args.author,
        license: args.license,
        description: args.description,
        compatible_sdk_version: args.compatible_sdk_version,
        target_sdk_version: args.target_sdk_version,
        compatible_sdk_type: args.compatible_sdk_type,
        device_types: args.device_types,
        package_kind: args.package_kind,
        integrated_hsp: args.integrated_hsp,
        hsp_bundle_name: args.hsp_bundle_name,
        har_out: args.har_out,
        runtime_hsp_out: args.runtime_hsp_out,
        interface_har_out: args.interface_har_out,
        tgz_out: args.tgz_out,
        hvigorw: args.hvigorw,
        ohpm: args.ohpm,
        deveco_sdk_home: args.deveco_sdk_home,
        no_har: args.no_har,
        arches,
        target_dir: args.target_dir.clone(),
        release: args.release,
        cargo_args: ohos_cargo_args,
        copy_static: args.copy_static,
        skip_libs: args.skip_libs,
        skip_check: args.skip_check,
        zigbuild: args.zigbuild,
        bisheng: args.bisheng,
        package: args.package,
        skip_napi_check: args.skip_napi_check,
        soname: args.soname,
        frontend_hsp_preflight_done: true,
    };
    if defer_hsp_publication {
        return super::ohos::build_deferred_hsp(options).map(Some);
    }
    super::ohos::build(options)?;
    Ok(None)
}

fn add_cargo_feature_args(command: &mut Command, features: &[String]) {
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
}

/// Scope downstream feature selection to the exact dependency alias used by
/// the host Cargo manifest.  A core package can have a different package
/// name, Rust lib target and host dependency alias; querying the host resolve
/// graph is the only way to avoid silently targeting the wrong package in a
/// custom host manifest.
#[cfg(feature = "cli-ohos")]
fn host_dependency_cargo_feature_args(
    core_manifest_path: &Utf8Path,
    host_manifest_path: &Utf8Path,
    cargo_args: &[String],
    features: &[String],
) -> Result<Vec<String>> {
    if features.is_empty() {
        return Ok(Vec::new());
    }
    let mut metadata_command = MetadataCommand::new();
    metadata_command
        .manifest_path(host_manifest_path.as_std_path())
        // A custom host may make its core dependency optional.  Inspect all
        // feature-gated direct edges, then restrict the eventual Cargo build
        // to the uniquely resolved core alias below.
        .features(CargoOpt::AllFeatures);
    let metadata = metadata_command.exec().with_context(|| {
        format!("running cargo metadata for host manifest {host_manifest_path}")
    })?;
    let canonical_core_manifest = canonicalize_or_keep(core_manifest_path);
    let canonical_host_manifest = canonicalize_or_keep(host_manifest_path);
    let core_package = cargo_package_for_manifest(&metadata, &canonical_core_manifest).with_context(|| {
        format!(
            "host manifest {host_manifest_path} does not resolve downstream core package {core_manifest_path}"
        )
    })?;
    let resolve = metadata.resolve.as_ref().with_context(|| {
        format!(
            "cargo metadata for host manifest {host_manifest_path} did not include a resolve graph"
        )
    })?;
    let host_packages = if let Some(host_package) =
        cargo_package_for_manifest(&metadata, &canonical_host_manifest)
    {
        vec![host_package]
    } else {
        let selector = cargo_package_selector(cargo_args)?;
        let use_default_members = selector.is_none()
            && !cargo_requests_workspace_members(cargo_args)
            && metadata.workspace_default_members.is_available()
            && !metadata.workspace_default_members.is_empty();
        let member_ids: &[cargo_metadata::PackageId] = if use_default_members {
            &metadata.workspace_default_members
        } else {
            &metadata.workspace_members
        };
        let mut members = member_ids
            .iter()
            .filter_map(|id| metadata.packages.iter().find(|package| package.id == *id))
            .collect::<Vec<_>>();
        if let Some(selector) = selector.as_deref() {
            members.retain(|package| cargo_package_matches_selector(package, selector));
            if members.is_empty() {
                bail!(
                    "virtual host workspace {host_manifest_path} has no member matching Cargo package selector `{selector}`"
                );
            }
        }
        members
    };

    let mut resolved = Vec::new();
    for host_package in host_packages {
        let host_node = resolve
            .nodes
            .iter()
            .find(|node| node.id == host_package.id)
            .with_context(|| {
                format!(
                    "cargo metadata resolve graph has no node for host package {}",
                    host_package.name
                )
            })?;
        let mut keys = host_node
            .deps
            .iter()
            .filter(|dependency| dependency.pkg == core_package.id)
            .filter(|dependency| {
                dependency
                    .dep_kinds
                    .iter()
                    .any(|kind| kind.kind == DependencyKind::Normal)
            })
            .map(|dependency| dependency.name.clone())
            .collect::<Vec<_>>();
        keys.sort();
        keys.dedup();
        match keys.as_slice() {
            [] => {}
            [key] if !key.is_empty() => {
                resolved.push((host_package.name.to_string(), key.clone()));
            }
            _ => bail!(
                "host package {} in {host_manifest_path} has ambiguous direct aliases ({}) for core package {} ({core_manifest_path}); cannot scope requested core features",
                host_package.name,
                keys.join(", "),
                core_package.name,
            ),
        }
    }
    let root_dependency_key = match resolved.as_slice() {
        [(_, key)] => key,
        [] => bail!(
            "host manifest {host_manifest_path} has no uniquely selected direct normal dependency on core package {} ({core_manifest_path}); cannot scope requested core features",
            core_package.name
        ),
        _ => bail!(
            "host manifest {host_manifest_path} has multiple workspace members with direct aliases ({}) for core package {} ({core_manifest_path}); pass --package to select one host member",
            resolved
                .iter()
                .map(|(package, key)| format!("{package}:{key}"))
                .collect::<Vec<_>>()
                .join(", "),
            core_package.name,
        ),
    };
    Ok(vec![
        "--features".to_string(),
        features
            .iter()
            .map(|feature| format!("{root_dependency_key}/{feature}"))
            .collect::<Vec<_>>()
            .join(","),
    ])
}

#[cfg(feature = "cli-ohos")]
fn cargo_package_for_manifest<'a>(
    metadata: &'a cargo_metadata::Metadata,
    manifest_path: &Utf8Path,
) -> Option<&'a Package> {
    metadata.packages.iter().find(|package| {
        Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf())
            .ok()
            .map(|path| canonicalize_or_keep(&path) == manifest_path)
            .unwrap_or(false)
    })
}

#[cfg(feature = "cli-ohos")]
fn cargo_package_selector(cargo_args: &[String]) -> Result<Option<String>> {
    let mut selector = None;
    let mut index = 0;
    while index < cargo_args.len() {
        let value = &cargo_args[index];
        let candidate = match value.as_str() {
            "--package" | "-p" => {
                index += 1;
                cargo_args.get(index).with_context(|| {
                    format!("Cargo argument `{value}` requires a package selector")
                })?
            }
            _ if value.starts_with("--package=") => &value["--package=".len()..],
            _ if value.starts_with("-p") && value.len() > 2 => &value[2..],
            _ => {
                index += 1;
                continue;
            }
        };
        match &selector {
            None => selector = Some(candidate.to_string()),
            Some(existing) if existing == candidate => {}
            Some(existing) => bail!(
                "multiple Cargo package selectors are ambiguous for host feature routing: `{existing}` and `{candidate}`"
            ),
        }
        index += 1;
    }
    Ok(selector)
}

#[cfg(feature = "cli-ohos")]
fn cargo_requests_workspace_members(cargo_args: &[String]) -> bool {
    cargo_args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--workspace" | "--all"))
}

#[cfg(feature = "cli-ohos")]
fn cargo_package_matches_selector(package: &Package, selector: &str) -> bool {
    selector == package.name.as_str() || selector == format!("{}@{}", package.name, package.version)
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "cli-ohos")]
    use super::host_dependency_cargo_feature_args;
    use super::{
        patch_mini_program_web_runtime, preflight_wasm_build_paths, BuildWasmArgs,
        WasmBindgenTargetArg,
    };
    #[cfg(windows)]
    use super::{wasm_preflight_nofollow, windows_wasm_semantic_path_key};
    use camino::Utf8PathBuf;

    fn guarded_wasm_args(root: &camino::Utf8Path) -> BuildWasmArgs {
        BuildWasmArgs {
            manifest_path: root.join("core/Cargo.toml"),
            out_dir: root.join("published/generated"),
            library_path: None,
            source: None,
            host_crates_dir: root.join("published/host"),
            package_root: root.join("published"),
            logical_host_crates_dir: None,
            artifact_dir: Some(root.join("published/artifacts")),
            wasm_bindgen_out_dir: Some(root.join("published/wasm-bindgen")),
            wasm_bindgen_target: WasmBindgenTargetArg::Web,
            cargo_bin: "cargo".into(),
            target_dir: Some(root.join("targets/host")),
            core_target_dir: Some(root.join("targets/core")),
            release: false,
            cargo_features: Vec::new(),
            no_format: true,
            config: None,
            crate_name: None,
            metadata_no_deps: false,
        }
    }

    #[test]
    fn mini_program_web_runtime_patch_removes_free_fetch_and_constructor_lookups() {
        let source = r#"
const ret = getObject(arg0).fetch(getObject(arg1));
const ret = fetch(getObject(arg0));
result = getObject(arg0) instanceof Response;
const ret = new Headers();
const ret = new Request(getStringFromWasm0(arg0, arg1), getObject(arg2));
if (typeof Response === 'function' && module instanceof Response) {
}
"#;

        let patched = patch_mini_program_web_runtime(source);

        assert!(patched.contains("export function setMiniProgramWebRuntime(runtime)"));
        assert!(patched.contains("__uniffiMiniProgramFetch(getObject(arg1))"));
        assert!(patched.contains("__uniffiMiniProgramFetch(getObject(arg0))"));
        assert!(patched.contains("instanceof __uniffiMiniProgramResponse"));
        assert!(patched.contains("new __uniffiMiniProgramHeaders()"));
        assert!(patched.contains("new __uniffiMiniProgramRequest("));
        assert!(!patched.contains("getObject(arg0).fetch("));
        assert!(!patched.contains("const ret = fetch("));
        assert!(!patched.contains("instanceof Response"));
        assert!(!patched.contains("new Headers()"));
        assert!(!patched.contains("new Request("));
    }

    #[cfg(feature = "cli-ohos")]
    #[test]
    fn host_dependency_feature_args_use_the_resolved_host_alias() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let core = root.join("core");
        let host = root.join("host");
        std::fs::create_dir_all(core.join("src")).unwrap();
        std::fs::create_dir_all(host.join("src")).unwrap();
        std::fs::write(core.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        std::fs::write(host.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        std::fs::write(
            core.join("Cargo.toml"),
            r#"[package]
name = "core-package"
version = "0.1.0"
edition = "2021"

[lib]
name = "core_bridge"

[features]
local-llm = []
local-llm-vision = []
local-llm-audio = []
"#,
        )
        .unwrap();
        std::fs::write(
            host.join("Cargo.toml"),
            r#"[package]
name = "custom-host"
version = "0.1.0"
edition = "2021"

[dependencies]
custom_core_alias = { package = "core-package", path = "../core" }
"#,
        )
        .unwrap();

        let args = host_dependency_cargo_feature_args(
            &core.join("Cargo.toml"),
            &host.join("Cargo.toml"),
            &[],
            &[
                "local-llm".to_string(),
                "local-llm-vision".to_string(),
                "local-llm-audio".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(
            args,
            vec![
                "--features".to_string(),
                "custom_core_alias/local-llm,custom_core_alias/local-llm-vision,custom_core_alias/local-llm-audio".to_string(),
            ]
        );
    }

    #[cfg(feature = "cli-ohos")]
    #[test]
    fn host_dependency_feature_args_support_virtual_workspace_package_selection() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let core = root.join("core");
        let workspace = root.join("host-workspace");
        for package in [&core, &workspace.join("host-a"), &workspace.join("host-b")] {
            std::fs::create_dir_all(package.join("src")).unwrap();
            std::fs::write(package.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
        }
        std::fs::write(
            core.join("Cargo.toml"),
            r#"[package]
name = "core-package"
version = "0.1.0"
edition = "2021"

[features]
host-gate = []
"#,
        )
        .unwrap();
        std::fs::write(
            workspace.join("Cargo.toml"),
            r#"[workspace]
members = ["host-a", "host-b"]
default-members = ["host-b"]
resolver = "3"
"#,
        )
        .unwrap();
        for (name, alias) in [("host-a", "core_for_a"), ("host-b", "core_for_b")] {
            std::fs::write(
                workspace.join(name).join("Cargo.toml"),
                format!(
                    r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[dependencies]
{alias} = {{ package = "core-package", path = "../../core" }}
"#,
                ),
            )
            .unwrap();
        }

        let default_args = host_dependency_cargo_feature_args(
            &core.join("Cargo.toml"),
            &workspace.join("Cargo.toml"),
            &[],
            &["host-gate".to_string()],
        )
        .unwrap();
        assert_eq!(
            default_args,
            vec!["--features".to_string(), "core_for_b/host-gate".to_string()]
        );

        let package_args = host_dependency_cargo_feature_args(
            &core.join("Cargo.toml"),
            &workspace.join("Cargo.toml"),
            &["--package".to_string(), "host-a".to_string()],
            &["host-gate".to_string()],
        )
        .unwrap();
        assert_eq!(
            package_args,
            vec!["--features".to_string(), "core_for_a/host-gate".to_string()]
        );
    }

    #[cfg(feature = "cli-ohos")]
    #[test]
    fn host_dependency_feature_args_are_empty_without_features() {
        assert!(host_dependency_cargo_feature_args(
            camino::Utf8Path::new("missing-core/Cargo.toml"),
            camino::Utf8Path::new("missing-host/Cargo.toml"),
            &[],
            &[],
        )
        .unwrap()
        .is_empty());
    }

    #[test]
    fn explicit_wasm_preflight_rejects_dotdot_overlap_and_symlink_aliases() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();

        let mut dotdot = guarded_wasm_args(&root);
        dotdot.core_target_dir = Some(root.join("targets/../core"));
        assert!(preflight_wasm_build_paths(&dotdot).is_err());

        let mut overlap = guarded_wasm_args(&root);
        overlap.target_dir = Some(root.join("targets/core/host"));
        assert!(preflight_wasm_build_paths(&overlap).is_err());

        #[cfg(unix)]
        {
            std::fs::create_dir(root.join("real-targets")).unwrap();
            std::os::unix::fs::symlink(root.join("real-targets"), root.join("alias-targets"))
                .unwrap();
            let mut alias = guarded_wasm_args(&root);
            alias.core_target_dir = Some(root.join("alias-targets/core"));
            assert!(preflight_wasm_build_paths(&alias).is_err());
        }
    }

    #[cfg(windows)]
    fn windows_verbatim_spelling(path: &camino::Utf8Path) -> Utf8PathBuf {
        use std::path::{Component, Prefix};

        let mut components = path.as_std_path().components();
        let Some(Component::Prefix(prefix)) = components.next() else {
            panic!("test verbatim spelling requires an absolute DOS/UNC path: {path}");
        };
        match prefix.kind() {
            Prefix::VerbatimDisk(_) | Prefix::VerbatimUNC(_, _) => path.to_path_buf(),
            Prefix::Disk(_) => Utf8PathBuf::from(format!(r"\\?\{}", path.as_str())),
            Prefix::UNC(_, _) => {
                let unc = path
                    .as_str()
                    .strip_prefix(r"\\")
                    .expect("ordinary UNC path has its leading separators");
                Utf8PathBuf::from(format!(r"\\?\UNC\{unc}"))
            }
            other => panic!("test verbatim spelling rejects unsupported prefix {other:?}: {path}"),
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_wasm_path_key_normalizes_dos_unc_verbatim_and_case() {
        let drive = camino::Utf8Path::new(r"C:\Work\Target");
        let verbatim_drive = windows_verbatim_spelling(drive);
        assert_eq!(verbatim_drive, camino::Utf8Path::new(r"\\?\C:\Work\Target"));
        assert_eq!(windows_verbatim_spelling(&verbatim_drive), verbatim_drive);
        assert_eq!(
            windows_wasm_semantic_path_key(drive).unwrap(),
            windows_wasm_semantic_path_key(camino::Utf8Path::new(r"\\?\c:\work\TARGET")).unwrap()
        );
        let unc = camino::Utf8Path::new(r"\\Server\Share\Work\Target");
        let verbatim_unc = windows_verbatim_spelling(unc);
        assert_eq!(
            verbatim_unc,
            camino::Utf8Path::new(r"\\?\UNC\Server\Share\Work\Target")
        );
        assert_eq!(windows_verbatim_spelling(&verbatim_unc), verbatim_unc);
        assert_eq!(
            windows_wasm_semantic_path_key(unc).unwrap(),
            windows_wasm_semantic_path_key(camino::Utf8Path::new(
                r"\\?\UNC\server\share\work\TARGET"
            ))
            .unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_wasm_preflight_accepts_normal_and_verbatim_equivalents() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let canonical = root.canonicalize_utf8().unwrap();
        assert_eq!(
            windows_verbatim_spelling(&windows_verbatim_spelling(&canonical)),
            windows_verbatim_spelling(&canonical),
            "verbatim conversion must be idempotent for Rust canonical paths"
        );
        let normal = wasm_preflight_nofollow(&root.join("ordinary/missing")).unwrap();
        let verbatim = wasm_preflight_nofollow(
            &windows_verbatim_spelling(&canonical).join("ordinary/missing"),
        )
        .unwrap();
        assert_eq!(
            windows_wasm_semantic_path_key(&normal).unwrap(),
            windows_wasm_semantic_path_key(&verbatim).unwrap()
        );

        // When the native test workspace itself is on a UNC share, exercise
        // the real filesystem preflight in both UNC spellings as well.
        if root.as_str().starts_with(r"\\") {
            let unc = wasm_preflight_nofollow(&root.join("unc/missing")).unwrap();
            let verbatim_unc =
                wasm_preflight_nofollow(&windows_verbatim_spelling(&root).join("unc/missing"))
                    .unwrap();
            assert_eq!(
                windows_wasm_semantic_path_key(&unc).unwrap(),
                windows_wasm_semantic_path_key(&verbatim_unc).unwrap()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_wasm_preflight_rejects_junction_reparse_points() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let target = root.join("real-target");
        let junction = root.join("junction-target");
        std::fs::create_dir(&target).unwrap();
        let status = std::process::Command::new("cmd")
            .args(["/D", "/C", "mklink", "/J"])
            .arg(junction.as_std_path())
            .arg(target.as_std_path())
            .status()
            .unwrap();
        assert!(status.success(), "failed to create a test junction");
        let error = wasm_preflight_nofollow(&junction.join("child")).unwrap_err();
        assert!(
            format!("{error:#}").contains("reparse point"),
            "unexpected junction rejection: {error:#}"
        );
    }

    #[test]
    fn explicit_wasm_preflight_checks_every_endpoint_before_creating_any_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
        let args = guarded_wasm_args(&root);
        std::fs::create_dir(root.join("published")).unwrap();
        std::fs::write(&args.host_crates_dir, b"not-a-directory").unwrap();

        assert!(preflight_wasm_build_paths(&args).is_err());
        assert!(
            !args.out_dir.exists()
                && !args.artifact_dir.as_ref().unwrap().exists()
                && !args.wasm_bindgen_out_dir.as_ref().unwrap().exists()
                && !args.core_target_dir.as_ref().unwrap().exists()
                && !args.target_dir.as_ref().unwrap().exists(),
            "failed endpoint preflight left a wasm root behind"
        );
        assert_eq!(
            std::fs::read(&args.host_crates_dir).unwrap(),
            b"not-a-directory"
        );
    }
}

pub(crate) fn cargo_build_command<'a>(
    cargo_bin: &str,
    manifest_path: &'a Utf8Path,
    extra_args: &[&'a str],
    release: bool,
    current_dir: Option<&'a Utf8Path>,
) -> Command {
    let mut command = Command::new(cargo_bin);
    command
        .arg("build")
        .arg("--manifest-path")
        .arg(manifest_path.as_str());
    if release {
        command.arg("--release");
    }
    command.args(extra_args);
    if let Some(dir) = current_dir {
        command.current_dir(dir.as_std_path());
    }
    command
}

pub(crate) fn run_command(
    binary: &str,
    command: &mut Command,
    tool_name: &str,
    missing_hint: &str,
) -> Result<()> {
    let rendered = format!("{command:?}");
    let output = command.output().with_context(|| {
        format!("{tool_name} invocation failed while spawning `{binary}`. {missing_hint}")
    })?;
    if !output.status.success() {
        bail!(
            "{tool_name} command failed: {rendered}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }
    Ok(())
}

fn canonicalize_or_keep(path: &Utf8Path) -> Utf8PathBuf {
    path.canonicalize_utf8()
        .unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_cwd_path(path: &Utf8PathBuf) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        Ok(path.clone())
    } else {
        Ok(Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
            .join(path))
    }
}

fn package_path(package_root: &Utf8Path, relative: &Utf8Path) -> Result<Utf8PathBuf> {
    resolve_cwd_path(&package_root.join(relative))
}

fn host_manifest_path(
    package_root: &Utf8Path,
    spec: &uniffi_bindgen_javascript::package::HostBuildSpec,
) -> Result<Utf8PathBuf> {
    Ok(package_path(package_root, &spec.crate_root)?.join("Cargo.toml"))
}

fn host_feature_args(
    spec: &uniffi_bindgen_javascript::package::HostBuildSpec,
    features: &[String],
) -> Vec<String> {
    if features.is_empty() {
        return Vec::new();
    }
    vec![
        "--features".to_owned(),
        features
            .iter()
            .map(|feature| format!("{}/{}", spec.core_dependency_key, feature))
            .collect::<Vec<_>>()
            .join(","),
    ]
}

fn host_cdylib_path_from_spec(
    package_root: &Utf8Path,
    spec: &uniffi_bindgen_javascript::package::HostBuildSpec,
    target_dir: Option<&Utf8Path>,
    release: bool,
) -> Utf8PathBuf {
    let root = target_dir
        .map(Utf8Path::to_path_buf)
        .unwrap_or_else(|| package_root.join(&spec.crate_root).join("target"));
    root.join(if release { "release" } else { "debug" })
        .join(host_cdylib_filename(&spec.lib_target))
}

fn wasm_artifact_path_from_spec(
    package_root: &Utf8Path,
    spec: &uniffi_bindgen_javascript::package::HostBuildSpec,
    target_dir: Option<&Utf8Path>,
    release: bool,
) -> Utf8PathBuf {
    let root = target_dir
        .map(Utf8Path::to_path_buf)
        .unwrap_or_else(|| package_root.join(&spec.crate_root).join("target"));
    root.join("wasm32-unknown-unknown")
        .join(if release { "release" } else { "debug" })
        .join(format!("{}.wasm", spec.lib_target))
}

fn normalize_default_host_crates_dir(host_crates_dir: &mut Utf8PathBuf, package_root: &Utf8Path) {
    if host_crates_dir == Utf8Path::new("native/hosts") {
        *host_crates_dir = package_root.join("native/hosts");
    }
}

fn normalize_default_package_root(package_root: &mut Utf8PathBuf, out_dir: &Utf8Path) {
    // Clap's `skip` fields receive an empty default for the direct
    // `javascript build-{wasm,napi,ohos}` commands.  The generated source
    // directory is the package root for those commands; coordinators pass an
    // explicit staging root and are left untouched.
    if package_root.as_str().is_empty() {
        *package_root = out_dir.to_path_buf();
    }
}

fn absolute_lexical(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not UTF-8: {}", p.display()))?
            .join(path))
    }
}

fn validate_package_paths(
    package_root: &Utf8Path,
    out_dir: &Utf8Path,
    host_crates_dir: &Utf8Path,
    artifact_dir: Option<&Utf8Path>,
    wasm_bindgen_out_dir: Option<&Utf8Path>,
) -> Result<()> {
    let root = absolute_lexical(package_root)?;
    let mut paths = vec![
        ("generated source", out_dir),
        ("host crates", host_crates_dir),
    ];
    if let Some(path) = artifact_dir {
        paths.push(("artifact", path));
    }
    if let Some(path) = wasm_bindgen_out_dir {
        paths.push(("wasm-bindgen", path));
    }
    for (label, path) in paths {
        validate_package_child(&root, path, label)?;
    }
    Ok(())
}

fn validate_package_child(root: &Utf8Path, path: &Utf8Path, label: &str) -> Result<()> {
    let root = absolute_lexical(root)?;
    let path = absolute_lexical(path)?;
    if path != root && !path.starts_with(&root) {
        bail!("{label} output {path} must remain below package root {root}");
    }
    Ok(())
}

pub(crate) fn emit_mini_program_wasm_runtime(
    out_dir: &Utf8Path,
    wasm_bindgen_out_dir: &Utf8Path,
    mini_program_out_dir: &Utf8Path,
    wasm_bindgen_stem: &str,
) -> Result<()> {
    std::fs::create_dir_all(mini_program_out_dir).with_context(|| {
        format!("creating Mini Program wasm artifact dir {mini_program_out_dir}")
    })?;

    let glue_source_path = wasm_bindgen_out_dir.join(format!("{wasm_bindgen_stem}.js"));
    let glue_dest_path = mini_program_out_dir.join(format!("{wasm_bindgen_stem}.js"));
    let glue_source = std::fs::read_to_string(&glue_source_path)
        .with_context(|| format!("reading wasm-bindgen JS glue {glue_source_path}"))?;
    let glue_source = patch_mini_program_wasm_bindgen_glue(&glue_source, wasm_bindgen_stem)?;
    std::fs::write(&glue_dest_path, glue_source)
        .with_context(|| format!("writing Mini Program wasm-bindgen JS glue {glue_dest_path}"))?;

    for suffix in ["_bg.wasm", ".d.ts"] {
        let source = wasm_bindgen_out_dir.join(format!("{wasm_bindgen_stem}{suffix}"));
        if !source.exists() {
            if suffix == ".d.ts" {
                continue;
            }
            bail!("wasm-bindgen output missing required artifact {source}");
        }
        let dest = mini_program_out_dir.join(format!("{wasm_bindgen_stem}{suffix}"));
        std::fs::copy(&source, &dest)
            .with_context(|| format!("copying Mini Program artifact {source} to {dest}"))?;
    }

    let snippets_source = wasm_bindgen_out_dir.join("snippets");
    if snippets_source.exists() {
        let snippets_dest = mini_program_out_dir.join("snippets");
        if snippets_dest.exists() {
            bail!(
                "fresh Mini Program snippets path unexpectedly exists without its creation-time witness: {snippets_dest}"
            );
        }
        copy_dir_contents(&snippets_source, &snippets_dest).with_context(|| {
            format!("copying wasm-bindgen snippets {snippets_source} to {snippets_dest}")
        })?;
    }

    emit_mini_program_auto_entrypoint(out_dir, mini_program_out_dir, wasm_bindgen_stem)?;
    Ok(())
}

fn copy_dir_contents(from: &Utf8Path, to: &Utf8Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating directory {to}"))?;
    for entry in std::fs::read_dir(from).with_context(|| format!("reading {from}"))? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("path is not utf8: {}", p.display()))?;
        let name = path
            .file_name()
            .with_context(|| format!("path has no file name: {path}"))?;
        let dest = to.join(name);
        if entry.file_type()?.is_dir() {
            copy_dir_contents(&path, &dest)?;
        } else {
            std::fs::copy(&path, &dest).with_context(|| format!("copying {path} to {dest}"))?;
        }
    }
    Ok(())
}

fn patch_mini_program_wasm_bindgen_glue(source: &str, wasm_bindgen_stem: &str) -> Result<String> {
    let default_wasm_path = mini_program_default_wasm_path(wasm_bindgen_stem);
    let replacement = format!(
        r#"async function __wbg_init(wasmPath = "{default_wasm_path}") {{
    if (wasm !== undefined) return wasm;

    if (typeof wasmPath !== "string" || wasmPath.length === 0) {{
        throw new Error("UniFFI Mini Program wasm init requires a non-empty package path string");
    }}
    if (typeof WXWebAssembly === "undefined" || typeof WXWebAssembly.instantiate !== "function") {{
        throw new Error("UniFFI Mini Program wasm init requires WXWebAssembly.instantiate(path, imports)");
    }}

    const imports = __wbg_get_imports();
    if (typeof __wbg_init_memory === "function") {{
        __wbg_init_memory(imports);
    }}
    const instantiated = await WXWebAssembly.instantiate(wasmPath, imports);
    return __wbg_finalize_init(instantiated.instance, instantiated.module);
}}"#
    );
    let patched = replace_js_function(source, "async function __wbg_init", &replacement)
        .context("patching wasm-bindgen __wbg_init for Mini Program")?;
    let patched = patched.replace(
        "const ret = typeof window === 'undefined' ? null : window;",
        "const ret = null;",
    );
    let patched = patch_mini_program_web_runtime(&patched);
    Ok(patch_mini_program_text_encoding(&patched))
}

fn patch_mini_program_web_runtime(source: &str) -> String {
    let patched = source
        .replace(
            "const ret = getObject(arg0).fetch(getObject(arg1));",
            "const ret = __uniffiMiniProgramFetch(getObject(arg1));",
        )
        .replace(
            "const ret = fetch(getObject(arg0));",
            "const ret = __uniffiMiniProgramFetch(getObject(arg0));",
        )
        .replace(
            "result = getObject(arg0) instanceof Response;",
            "result = getObject(arg0) instanceof __uniffiMiniProgramResponse;",
        )
        .replace(
            "const ret = new Headers();",
            "const ret = new __uniffiMiniProgramHeaders();",
        )
        .replace(
            "const ret = new Request(getStringFromWasm0(arg0, arg1), getObject(arg2));",
            "const ret = new __uniffiMiniProgramRequest(getStringFromWasm0(arg0, arg1), getObject(arg2));",
        )
        .replace(
            "if (typeof Response === 'function' && module instanceof Response) {",
            "if (typeof __uniffiMiniProgramResponse === 'function' && module instanceof __uniffiMiniProgramResponse) {",
        );
    format!("{}\n{}", mini_program_web_runtime_prelude(), patched)
}

fn mini_program_web_runtime_prelude() -> &'static str {
    r#"let __uniffiMiniProgramFetch = typeof fetch === "function" ? fetch : undefined;
let __uniffiMiniProgramHeaders = typeof Headers === "function" ? Headers : undefined;
let __uniffiMiniProgramRequest = typeof Request === "function" ? Request : undefined;
let __uniffiMiniProgramResponse = typeof Response === "function" ? Response : undefined;

export function setMiniProgramWebRuntime(runtime) {
    if (runtime === null || typeof runtime !== "object") {
        throw new TypeError("Mini Program web runtime must be an object");
    }
    if (typeof runtime.fetch !== "function"
        || typeof runtime.Headers !== "function"
        || typeof runtime.Request !== "function"
        || typeof runtime.Response !== "function") {
        throw new TypeError("Mini Program web runtime requires fetch, Headers, Request, and Response");
    }
    __uniffiMiniProgramFetch = runtime.fetch;
    __uniffiMiniProgramHeaders = runtime.Headers;
    __uniffiMiniProgramRequest = runtime.Request;
    __uniffiMiniProgramResponse = runtime.Response;
}"#
}

fn patch_mini_program_text_encoding(source: &str) -> String {
    let patched = source
        .replace("new TextDecoder(", "new __uniffiTextDecoder(")
        .replace("new TextEncoder(", "new __uniffiTextEncoder(");
    format!("{}\n{}", mini_program_text_encoding_prelude(), patched)
}

fn mini_program_text_encoding_prelude() -> &'static str {
    r#"const __uniffiMiniProgramGlobal = typeof globalThis !== "undefined" ? globalThis : {};
const __uniffiTextDecoder = __uniffiMiniProgramGlobal.TextDecoder ?? class {
    decode(input) {
        if (input === undefined) return "";
        const bytes = input instanceof Uint8Array
            ? input
            : ArrayBuffer.isView(input)
                ? new Uint8Array(input.buffer, input.byteOffset, input.byteLength)
                : new Uint8Array(input);
        let out = "";
        for (let i = 0; i < bytes.length;) {
            const b0 = bytes[i++];
            if (b0 < 0x80) {
                out += String.fromCharCode(b0);
                continue;
            }
            if ((b0 & 0xe0) === 0xc0 && i < bytes.length) {
                const b1 = bytes[i++] & 0x3f;
                out += String.fromCharCode(((b0 & 0x1f) << 6) | b1);
                continue;
            }
            if ((b0 & 0xf0) === 0xe0 && i + 1 < bytes.length) {
                const b1 = bytes[i++] & 0x3f;
                const b2 = bytes[i++] & 0x3f;
                out += String.fromCharCode(((b0 & 0x0f) << 12) | (b1 << 6) | b2);
                continue;
            }
            if ((b0 & 0xf8) === 0xf0 && i + 2 < bytes.length) {
                const b1 = bytes[i++] & 0x3f;
                const b2 = bytes[i++] & 0x3f;
                const b3 = bytes[i++] & 0x3f;
                let cp = ((b0 & 0x07) << 18) | (b1 << 12) | (b2 << 6) | b3;
                cp -= 0x10000;
                out += String.fromCharCode(0xd800 + (cp >> 10), 0xdc00 + (cp & 0x3ff));
                continue;
            }
            out += "\ufffd";
        }
        return out;
    }
};
const __uniffiTextEncoder = __uniffiMiniProgramGlobal.TextEncoder ?? class {
    encode(input = "") {
        const bytes = [];
        const text = String(input);
        for (let i = 0; i < text.length; i += 1) {
            let cp = text.charCodeAt(i);
            if (cp >= 0xd800 && cp <= 0xdbff && i + 1 < text.length) {
                const next = text.charCodeAt(i + 1);
                if (next >= 0xdc00 && next <= 0xdfff) {
                    cp = 0x10000 + ((cp - 0xd800) << 10) + (next - 0xdc00);
                    i += 1;
                }
            }
            if (cp < 0x80) {
                bytes.push(cp);
            } else if (cp < 0x800) {
                bytes.push(0xc0 | (cp >> 6), 0x80 | (cp & 0x3f));
            } else if (cp < 0x10000) {
                bytes.push(0xe0 | (cp >> 12), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
            } else {
                bytes.push(0xf0 | (cp >> 18), 0x80 | ((cp >> 12) & 0x3f), 0x80 | ((cp >> 6) & 0x3f), 0x80 | (cp & 0x3f));
            }
        }
        return new Uint8Array(bytes);
    }
    encodeInto(input, view) {
        const bytes = this.encode(input);
        const written = Math.min(bytes.length, view.length);
        view.set(bytes.subarray(0, written));
        return { read: String(input).length, written };
    }
};"#
}

fn replace_js_function(source: &str, signature_start: &str, replacement: &str) -> Result<String> {
    let start = source
        .find(signature_start)
        .with_context(|| format!("could not find `{signature_start}`"))?;
    let brace_start = source[start..]
        .find('{')
        .map(|idx| start + idx)
        .with_context(|| format!("could not find `{signature_start}` body"))?;
    let brace_end = matching_brace(source, brace_start)
        .with_context(|| format!("could not find `{signature_start}` body end"))?;

    let mut out = String::with_capacity(source.len() + replacement.len());
    out.push_str(&source[..start]);
    out.push_str(replacement);
    out.push_str(&source[brace_end + 1..]);
    Ok(out)
}

fn matching_brace(source: &str, open_idx: usize) -> Option<usize> {
    let bytes = source.as_bytes();
    if bytes.get(open_idx).copied() != Some(b'{') {
        return None;
    }
    let mut depth = 0usize;
    for (idx, byte) in bytes.iter().enumerate().skip(open_idx) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(idx);
                }
            }
            _ => {}
        }
    }
    None
}

fn emit_mini_program_auto_entrypoint(
    out_dir: &Utf8Path,
    mini_program_out_dir: &Utf8Path,
    wasm_bindgen_stem: &str,
) -> Result<()> {
    let logical_out_dir = out_dir
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing Mini Program generated root {out_dir}"))?;
    let logical_mini_program_out_dir =
        mini_program_out_dir.canonicalize_utf8().with_context(|| {
            format!("canonicalizing Mini Program artifact dir {mini_program_out_dir}")
        })?;
    write_mini_program_auto_entrypoint(
        out_dir,
        &logical_out_dir,
        &logical_mini_program_out_dir,
        wasm_bindgen_stem,
    )
}

fn write_mini_program_auto_entrypoint(
    actual_out_dir: &Utf8Path,
    logical_out_dir: &Utf8Path,
    logical_mini_program_out_dir: &Utf8Path,
    wasm_bindgen_stem: &str,
) -> Result<()> {
    let browser_dir = actual_out_dir.join("browser");
    let entrypoint = browser_dir.join("index.mini-program.js");
    let declaration = browser_dir.join("index.mini-program.d.ts");
    std::fs::create_dir_all(&browser_dir)
        .with_context(|| format!("creating browser output dir {browser_dir}"))?;
    let logical_browser_dir = logical_out_dir.join("browser");
    let rel_artifact_dir =
        relative_path_from_dir(&logical_browser_dir, logical_mini_program_out_dir)
            .to_string()
            .replace('\\', "/");
    let rel_artifact_dir = if rel_artifact_dir.is_empty() {
        ".".to_string()
    } else if rel_artifact_dir.starts_with('.') {
        rel_artifact_dir
    } else {
        format!("./{rel_artifact_dir}")
    };
    let glue_path = format!("{rel_artifact_dir}/{wasm_bindgen_stem}.js");
    let default_wasm_path = mini_program_default_wasm_path(wasm_bindgen_stem);
    let mut components = std::fs::read_dir(actual_out_dir.join("components"))
        .with_context(|| {
            format!(
                "reading generated component namespaces below {}",
                actual_out_dir.join("components")
            )
        })?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(|kind| kind.is_dir())
                .and_then(|_| entry.file_name().into_string().ok())
        })
        .collect::<Vec<_>>();
    components.sort();
    let source = format!(
        r#"// AUTOGENERATED by uniffi_bindgen_javascript (wasm Mini Program auto-entrypoint).
//
// This file is emitted by `uniffi-bindgen artifacts build --target mini-program`.
// It consumes a patched wasm-bindgen JS glue module whose default init
// calls `WXWebAssembly.instantiate(packagePath, imports)`.

import * as glue from "{glue_path}";
import * as __backend from "./backend.js";

export {{ session, close{component_exports} }} from "./backend.js";

export const DEFAULT_WASM_PATH = "{default_wasm_path}";

let readyPromise = null;

function assertWXWebAssembly() {{
    const runtime = globalThis.WXWebAssembly;
    if (!runtime || typeof runtime.instantiate !== "function") {{
        throw new Error("UniFFI Mini Program wasm init requires WXWebAssembly.instantiate(path, imports)");
    }}
}}

export function init(wasmPath = DEFAULT_WASM_PATH) {{
    return initWithGlue(glue, wasmPath);
}}

export function initWithPath(wasmPath) {{
    return init(wasmPath);
}}

export function setMiniProgramWebRuntime(runtime) {{
    if (typeof glue.setMiniProgramWebRuntime !== "function") {{
        throw new Error("UniFFI Mini Program wasm glue does not expose setMiniProgramWebRuntime");
    }}
    glue.setMiniProgramWebRuntime(runtime);
}}

export function initWithGlue(
    customGlue,
    wasmPath,
) {{
    assertWXWebAssembly();
    readyPromise ??= installAll(customGlue, wasmPath);
    return readyPromise;
}}

async function installAll(
    customGlue,
    wasmPath,
) {{
    return __backend.initWithGlue(customGlue, wasmPath);
}}
"#,
        component_exports = components
            .iter()
            .map(|component| format!(", {component}"))
            .collect::<String>(),
    );
    std::fs::write(&entrypoint, source)
        .with_context(|| format!("writing Mini Program auto-entrypoint {entrypoint}"))?;
    let declaration_source = format!(
        r#"// AUTOGENERATED by uniffi_bindgen_javascript (wasm Mini Program declarations).

export type {{ ReadyApi }} from "./index.js";
export {{ session, close{component_exports} }} from "./index.js";

export interface MiniProgramWebRuntime {{
    readonly fetch: (...args: never[]) => unknown;
    readonly Headers: new (...args: never[]) => unknown;
    readonly Request: new (...args: never[]) => unknown;
    readonly Response: new (...args: never[]) => unknown;
}}

export declare const DEFAULT_WASM_PATH: string;
export declare function init(wasmPath?: string): Promise<import("./index.js").ReadyApi>;
export declare function initWithPath(wasmPath: string): Promise<import("./index.js").ReadyApi>;
export declare function setMiniProgramWebRuntime(runtime: MiniProgramWebRuntime): void;
export declare function initWithGlue(
    customGlue: unknown,
    wasmPath: string,
): Promise<import("./index.js").ReadyApi>;
"#,
        component_exports = components
            .iter()
            .map(|component| format!(", {component}"))
            .collect::<String>(),
    );
    std::fs::write(&declaration, declaration_source)
        .with_context(|| format!("writing Mini Program declarations {declaration}"))?;
    Ok(())
}

pub(crate) fn mini_program_default_wasm_path(wasm_bindgen_stem: &str) -> String {
    format!("/assets/{wasm_bindgen_stem}_bg.wasm")
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

struct CargoPackageMetadata {
    target_directory: Utf8PathBuf,
    #[cfg(feature = "cli-ohos")]
    workspace_root: Utf8PathBuf,
    #[cfg(feature = "cli-ohos")]
    local_source_roots: Vec<(String, Utf8PathBuf)>,
    #[cfg(feature = "cli-ohos")]
    package_name: String,
    lib_target_name: String,
}

impl CargoPackageMetadata {
    fn host_cdylib_path(&self, release: bool) -> Utf8PathBuf {
        self.host_cdylib_path_in(&self.target_directory, release)
    }

    fn host_cdylib_path_in(&self, target_directory: &Utf8Path, release: bool) -> Utf8PathBuf {
        target_directory
            .join(if release { "release" } else { "debug" })
            .join(host_cdylib_filename(&self.lib_target_name))
    }
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
        .find(|target| target.kind.iter().any(|kind| kind.to_string() == "cdylib"))
        .or_else(|| {
            package
                .targets
                .iter()
                .find(|target| target.kind.iter().any(|kind| kind.to_string() == "lib"))
        })
        .with_context(|| format!("package {} has no lib/cdylib target", package.name))?;
    #[cfg(feature = "cli-ohos")]
    let workspace_root =
        Utf8PathBuf::from_path_buf(metadata.workspace_root.clone().into_std_path_buf())
            .map_err(|p| anyhow::anyhow!("cargo workspace root is not utf8: {}", p.display()))?;
    #[cfg(feature = "cli-ohos")]
    let mut local_source_roots = metadata
        .packages
        .iter()
        .filter(|package| package.source.is_none())
        .filter_map(|package| {
            let manifest =
                Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf())
                    .ok()?;
            Some((
                format!("{}-{}", package.name, package.version),
                manifest.parent()?.to_path_buf(),
            ))
        })
        .collect::<Vec<_>>();
    #[cfg(feature = "cli-ohos")]
    {
        local_source_roots.sort();
        local_source_roots.dedup();
    }
    Ok(CargoPackageMetadata {
        target_directory: Utf8PathBuf::from_path_buf(
            metadata.target_directory.clone().into_std_path_buf(),
        )
        .map_err(|p| anyhow::anyhow!("cargo metadata target dir is not utf8: {}", p.display()))?,
        #[cfg(feature = "cli-ohos")]
        workspace_root,
        #[cfg(feature = "cli-ohos")]
        local_source_roots,
        #[cfg(feature = "cli-ohos")]
        package_name: package.name.to_string(),
        lib_target_name: lib_target.name.clone(),
    })
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
