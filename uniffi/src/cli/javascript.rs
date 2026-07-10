/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::MetadataCommand;
use clap::{Args, Subcommand, ValueEnum};
use std::process::Command;
use uniffi_bindgen::{
    cargo_metadata::CrateConfigSupplier, BindgenLoader, BindgenPaths, CargoMetadataOptions,
    GlobalConfig,
};
use uniffi_bindgen_javascript::{generate, FlavorTarget, GenerateJsOptions, HostCrateOptions};
use wasm_bindgen_cli_support::Bindgen;

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

    /// Build JavaScript + wasm bindings, emit the wasm host crate, and run wasm-bindgen.
    ///
    /// This path targets downstream crates whose generated wasm host crate can
    /// directly call the Rust API described by the chosen UDL/library input.
    BuildWasm(BuildWasmArgs),

    /// Build JavaScript + N-API bindings, emit/build the napi host crate, and copy `.node` addons.
    BuildNapi(BuildNapiArgs),

    /// Build JavaScript + Harmony/OpenHarmony bindings through OHOS Node-API.
    BuildOhos(BuildOhosArgs),
}

#[derive(Clone, Args)]
pub(crate) struct BuildArgs {
    /// Downstream core crate Cargo.toml.
    #[clap(long = "manifest-path")]
    manifest_path: Utf8PathBuf,

    /// Directory in which to write generated JavaScript files.
    #[clap(long, short)]
    out_dir: Utf8PathBuf,

    /// Optional override for the library/cdylib path used for JS generation.
    /// When omitted, the command builds the crate at --manifest-path and derives
    /// the cdylib location from Cargo metadata.
    #[clap(long = "library-path")]
    library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the JS generator.
    /// When set, this overrides the built-library path for generation, but
    /// the downstream core crate is still built as part of the orchestration.
    #[clap(long)]
    source: Option<Utf8PathBuf>,

    /// Directory (default `rust_modules`) in which to emit the generated host crates.
    #[clap(long = "host-crates-dir", default_value = "rust_modules")]
    host_crates_dir: Utf8PathBuf,

    /// Directory for built non-source artifacts such as wasm-bindgen pkg and `.node` addons.
    #[clap(long = "artifact-dir")]
    artifact_dir: Option<Utf8PathBuf>,

    /// Where to write the wasm-bindgen output tree. Defaults to `<out-dir>/browser/pkg`.
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

    /// Build the downstream core crate and generated host crates in release mode.
    #[clap(long)]
    release: bool,

    /// Cargo features enabled on the downstream core crate. May be repeated or comma-separated.
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
            artifact_dir: self.artifact_dir.clone(),
            wasm_bindgen_out_dir: self.wasm_bindgen_out_dir.clone(),
            wasm_bindgen_target: self.wasm_bindgen_target,
            cargo_bin: self.cargo_bin.clone(),
            release: self.release,
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
    /// Downstream core crate Cargo.toml.
    #[clap(long = "manifest-path")]
    pub(crate) manifest_path: Utf8PathBuf,

    /// Directory in which to write generated JavaScript files.
    #[clap(long, short)]
    pub(crate) out_dir: Utf8PathBuf,

    /// Optional override for the library/cdylib path used for JS generation.
    /// When omitted, the command builds the crate at --manifest-path and derives
    /// the cdylib location from Cargo metadata.
    #[clap(long = "library-path")]
    pub(crate) library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the JS generator.
    /// When set, this overrides the built-library path for generation, but
    /// the downstream core crate is still built as part of the orchestration.
    #[clap(long)]
    pub(crate) source: Option<Utf8PathBuf>,

    /// Directory (default `rust_modules`) in which to emit the generated wasm host crate.
    #[clap(long = "host-crates-dir", default_value = "rust_modules")]
    pub(crate) host_crates_dir: Utf8PathBuf,

    /// Directory for built non-source artifacts. Defaults to `<out-dir>/browser/pkg` for wasm-bindgen output when omitted.
    #[clap(long = "artifact-dir")]
    pub(crate) artifact_dir: Option<Utf8PathBuf>,

    /// Where to write the wasm-bindgen output tree. Defaults to `<out-dir>/browser/pkg`.
    #[clap(long = "wasm-bindgen-out-dir")]
    pub(crate) wasm_bindgen_out_dir: Option<Utf8PathBuf>,

    /// wasm-bindgen output target.
    #[clap(long = "wasm-bindgen-target", value_enum, default_value = "web")]
    pub(crate) wasm_bindgen_target: WasmBindgenTargetArg,

    /// Override the `cargo` binary to invoke.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    pub(crate) cargo_bin: String,

    /// Build the downstream core crate and generated wasm host crate in release mode.
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
}

#[derive(Clone, Args)]
pub(crate) struct BuildNapiArgs {
    /// Downstream core crate Cargo.toml.
    #[clap(long = "manifest-path")]
    pub(crate) manifest_path: Utf8PathBuf,

    /// Directory in which to write generated JavaScript files.
    #[clap(long, short)]
    pub(crate) out_dir: Utf8PathBuf,

    /// Optional override for the library/cdylib path used for JS generation.
    /// When omitted, the command builds the crate at --manifest-path and derives
    /// the cdylib location from Cargo metadata.
    #[clap(long = "library-path")]
    pub(crate) library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the JS generator.
    /// When set, this overrides the built-library path for generation, but
    /// the downstream core crate is still built as part of the orchestration.
    #[clap(long)]
    pub(crate) source: Option<Utf8PathBuf>,

    /// Directory (default `rust_modules`) in which to emit the generated napi host crate.
    #[clap(long = "host-crates-dir", default_value = "rust_modules")]
    pub(crate) host_crates_dir: Utf8PathBuf,

    /// Directory for built non-source artifacts. Defaults to copying `.node` addons next to generated backend files when omitted.
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

    /// Build the downstream core crate and generated napi host crate in release mode.
    #[clap(long)]
    pub(crate) release: bool,

    /// Cargo features enabled on the downstream core crate. May be repeated or comma-separated.
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
pub(crate) struct BuildOhosArgs {
    /// Downstream core crate Cargo.toml.
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

    /// Directory (default `rust_modules`) in which to emit the generated OHOS host crate.
    #[clap(long = "host-crates-dir", default_value = "rust_modules")]
    pub(crate) host_crates_dir: Utf8PathBuf,

    /// Build an existing OHOS host package or workspace instead of the generated single-package host manifest.
    #[clap(long = "ohos-host-manifest-path")]
    pub(crate) ohos_host_manifest_path: Option<Utf8PathBuf>,

    /// Build an explicitly raw-only custom host without a generated facade contract.
    #[clap(long = "raw-only-facade", requires = "ohos_host_manifest_path")]
    pub(crate) raw_only_facade: bool,

    /// Directory for built non-source artifacts. Defaults to `<host-crate>/dist` for OHOS output when omitted.
    #[clap(long = "artifact-dir")]
    pub(crate) artifact_dir: Option<Utf8PathBuf>,

    /// Output directory for built OHOS dist artifacts (intermediate native output).
    #[clap(long = "dist-dir")]
    pub(crate) dist_dir: Option<Utf8PathBuf>,

    /// OHPM package name for generated HAR metadata (supports scoped names like `@scope/name`).
    #[clap(long = "package-name")]
    pub(crate) package_name: Option<String>,

    /// Harmony module name override. Defaults to a stable normalization of the OHPM package name.
    #[clap(long = "module-name")]
    pub(crate) module_name: Option<String>,

    /// Semantic version override for generated OHPM package metadata.
    #[clap(long = "package-version")]
    pub(crate) package_version: Option<String>,

    /// Author override for generated OHPM package metadata.
    #[clap(long = "author")]
    pub(crate) author: Option<String>,

    /// SPDX license override for generated OHPM package metadata.
    #[clap(long = "license")]
    pub(crate) license: Option<String>,

    /// Description override for generated OHPM package metadata.
    #[clap(long = "description")]
    pub(crate) description: Option<String>,

    /// Minimum compatible Harmony/OpenHarmony SDK version. Must be explicit for final HAR packaging.
    #[clap(long = "compatible-sdk-version")]
    pub(crate) compatible_sdk_version: Option<String>,

    /// Compatible SDK type, such as HarmonyOS or OpenHarmony.
    #[clap(long = "compatible-sdk-type")]
    pub(crate) compatible_sdk_type: Option<String>,

    /// Supported Harmony device type. May be repeated or comma-separated.
    #[clap(long = "device-type", value_delimiter = ',')]
    pub(crate) device_types: Vec<String>,

    /// Output `.har` path. Defaults to `<artifact-root>/<package>.har`.
    #[clap(long = "har-out")]
    pub(crate) har_out: Option<Utf8PathBuf>,

    /// Hvigor wrapper used to build the final compiled HAR (falls back to HVIGORW or PATH).
    #[clap(long = "hvigorw")]
    pub(crate) hvigorw: Option<String>,

    /// OHPM executable used to resolve and prepublish the final HAR (falls back to OHPM or PATH).
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

    /// Cargo features enabled on the downstream core crate. May be repeated or comma-separated.
    #[clap(long = "cargo-feature", value_delimiter = ',')]
    pub(crate) cargo_features: Vec<String>,

    /// Copy static `.a` libraries in addition to shared `.so` artifacts.
    #[clap(long = "static")]
    pub(crate) copy_static: bool,

    /// Skip copying native libraries; still generate TypeScript declarations.
    #[clap(long = "skip-libs")]
    pub(crate) skip_libs: bool,

    /// Reuse the generated OHOS type definition cache.
    #[clap(long = "dts-cache")]
    pub(crate) dts_cache: bool,

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

    /// Build the downstream core crate and generated OHOS host crate in release mode.
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

    /// The caller already holds the managed Harmony output lock.
    #[clap(skip)]
    pub(crate) output_lock_held: bool,
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

impl WasmBindgenTargetArg {
    fn emits_web_auto_entrypoint(self) -> bool {
        matches!(self, Self::Web)
    }
}

pub(crate) fn run(args: JavascriptArgs) -> Result<()> {
    match args.command {
        JavascriptCommands::Build(args) => build(args),
        JavascriptCommands::BuildWasm(args) => build_wasm(args),
        JavascriptCommands::BuildNapi(args) => build_napi(args),
        JavascriptCommands::BuildOhos(args) => build_ohos(args),
    }
}

pub(crate) fn generate_js(
    manifest_path: &Utf8Path,
    source: Utf8PathBuf,
    out_dir: Utf8PathBuf,
    config: Option<Utf8PathBuf>,
    crate_name: Option<String>,
    metadata_no_deps: bool,
    no_format: bool,
    host_crates: Option<HostCrateOptions>,
    flavors: Vec<FlavorTarget>,
    artifact_dir: Option<Utf8PathBuf>,
) -> Result<()> {
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
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir,
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
    Ok(())
}

fn build(args: BuildArgs) -> Result<()> {
    build_wasm(args.wasm_args()).context("building JavaScript wasm target")?;
    build_napi(args.napi_args()).context("building JavaScript N-API target")?;
    Ok(())
}

pub(crate) fn build_wasm(args: BuildWasmArgs) -> Result<()> {
    let manifest_path = canonicalize_or_keep(&args.manifest_path);
    let core_meta = cargo_package_metadata(&manifest_path)?;

    let mut build_core =
        cargo_build_command(&args.cargo_bin, &manifest_path, &[], args.release, None);
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

    generate_js(
        &manifest_path,
        generation_source,
        args.out_dir.clone(),
        args.config.clone(),
        args.crate_name.clone(),
        args.metadata_no_deps,
        args.no_format,
        Some(HostCrateOptions {
            manifest_path: manifest_path.clone(),
            host_crates_dir: args.host_crates_dir.clone(),
            ohos_rs_dir: None,
        }),
        vec![FlavorTarget::Wasm],
        args.artifact_dir.clone(),
    )?;

    let host_root = if args.host_crates_dir.is_absolute() {
        args.host_crates_dir.clone()
    } else {
        Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
            .join(&args.host_crates_dir)
    };
    let wasm_manifest = host_root.join("wasm/Cargo.toml");
    if !wasm_manifest.exists() {
        bail!(
            "wasm host crate was not emitted at {}",
            wasm_manifest
                .parent()
                .unwrap_or_else(|| Utf8Path::new("<unknown>"))
        );
    }

    let mut build_wasm_host = cargo_build_command(
        &args.cargo_bin,
        &wasm_manifest,
        &["--target", "wasm32-unknown-unknown"],
        args.release,
        None,
    );
    run_command(
        &args.cargo_bin,
        &mut build_wasm_host,
        "cargo",
        "install Rust's wasm32-unknown-unknown target with `rustup target add wasm32-unknown-unknown`",
    )?;

    let wasm_meta = cargo_package_metadata(&wasm_manifest)?;
    let wasm_artifact = wasm_meta.wasm_artifact_path(args.release);
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

    run_wasm_bindgen_in_process(
        &wasm_artifact,
        &wasm_bindgen_out_dir,
        args.wasm_bindgen_target,
    )?;

    emit_browser_auto_entrypoint(
        &args.out_dir,
        &wasm_bindgen_out_dir,
        &wasm_meta.lib_target_name,
        args.wasm_bindgen_target,
    )?;

    Ok(())
}

pub(crate) fn build_napi(args: BuildNapiArgs) -> Result<()> {
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

    generate_js(
        &manifest_path,
        generation_source,
        args.out_dir.clone(),
        args.config.clone(),
        args.crate_name.clone(),
        args.metadata_no_deps,
        args.no_format,
        Some(HostCrateOptions {
            manifest_path: manifest_path.clone(),
            host_crates_dir: args.host_crates_dir.clone(),
            ohos_rs_dir: None,
        }),
        flavors.clone(),
        args.artifact_dir.clone(),
    )?;

    let host_root = if args.host_crates_dir.is_absolute() {
        args.host_crates_dir.clone()
    } else {
        Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
            .join(&args.host_crates_dir)
    };
    let napi_manifest = host_root.join("napi/Cargo.toml");
    if !napi_manifest.exists() {
        bail!(
            "napi host crate was not emitted at {}",
            napi_manifest
                .parent()
                .unwrap_or_else(|| Utf8Path::new("<unknown>"))
        );
    }

    let target_dir = args.target_dir.as_ref().map(resolve_cwd_path).transpose()?;
    let napi_cargo_args =
        dependency_cargo_feature_args(&core_meta.package_name, &args.cargo_features);
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
        build_napi_host.env("CARGO_TARGET_DIR", target_dir.as_str());
    }
    run_command(
        &args.cargo_bin,
        &mut build_napi_host,
        "cargo",
        "install Rust's cargo toolchain or pass --cargo-bin <path>",
    )?;

    let napi_meta = cargo_package_metadata(&napi_manifest)?;
    let napi_artifact = if let Some(target_dir) = &target_dir {
        napi_meta.host_cdylib_path_in(target_dir, args.release)
    } else {
        napi_meta.host_cdylib_path(args.release)
    };
    if !napi_artifact.exists() {
        bail!(
            "built napi artifact not found at {} after cargo build",
            napi_artifact
        );
    }

    for flavor in flavors {
        let subdir = match flavor {
            FlavorTarget::Napi => "node",
            FlavorTarget::Electron => "electron",
            FlavorTarget::Wasm | FlavorTarget::Harmony => continue,
        };
        let flavor_dir = args.out_dir.join(subdir);
        ensure_single_generated_rs(&flavor_dir)
            .with_context(|| format!("finding generated Rust bridge in {flavor_dir}"))?;
        let addon_stem = generated_addon_stem(&flavor_dir)
            .with_context(|| format!("finding generated addon name in {flavor_dir}"))?;
        let addon_dir = args
            .artifact_dir
            .as_ref()
            .map(|dir| dir.join(subdir))
            .unwrap_or_else(|| flavor_dir.clone());
        let addon_path = addon_dir.join(format!("{addon_stem}.node"));
        std::fs::create_dir_all(&addon_dir)
            .with_context(|| format!("creating addon output dir {addon_dir}"))?;
        std::fs::copy(&napi_artifact, &addon_path)
            .with_context(|| format!("copying built napi addon {napi_artifact} to {addon_path}"))?;
    }

    Ok(())
}

pub(crate) fn build_ohos(args: BuildOhosArgs) -> Result<()> {
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
    // workspace/local-source set before the first actual core build.
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

    generate_js(
        &manifest_path,
        generation_source,
        args.out_dir.clone(),
        args.config.clone(),
        args.crate_name.clone(),
        args.metadata_no_deps,
        args.no_format,
        Some(HostCrateOptions {
            manifest_path: manifest_path.clone(),
            host_crates_dir: args.host_crates_dir.clone(),
            ohos_rs_dir: None,
        }),
        vec![FlavorTarget::Harmony],
        args.artifact_dir.clone(),
    )?;

    let generated_ohos_manifest = ohos_dir.join("Cargo.toml");
    if !generated_ohos_manifest.exists() {
        bail!(
            "OHOS host crate was not emitted at {}",
            generated_ohos_manifest
                .parent()
                .unwrap_or_else(|| Utf8Path::new("<unknown>"))
        );
    }
    let ohos_manifest = custom_ohos_manifest.unwrap_or(generated_ohos_manifest);
    if !ohos_manifest.exists() {
        bail!("custom OHOS host manifest does not exist: {ohos_manifest}");
    }
    let facade_mode = if args.raw_only_facade {
        super::ohos::FacadeMode::RawOnly
    } else {
        let facade_bundle_path = ohos_manifest
            .parent()
            .context("OHOS host manifest has no parent for its facade bundle")?
            .join("uniffi-ohos-facade-bundle.json");
        super::ohos::FacadeMode::Required(facade_bundle_path)
    };

    let arches = if args.arch.is_empty() {
        vec!["aarch".to_string(), "x64".to_string()]
    } else {
        args.arch.clone()
    };
    let mut ohos_cargo_args =
        dependency_cargo_feature_args(&core_meta.package_name, &args.cargo_features);
    ohos_cargo_args.extend(args.cargo_args);
    let mut additional_source_roots = core_meta.local_source_roots.clone();
    additional_source_roots.push(("core-workspace".into(), core_meta.workspace_root.clone()));
    additional_source_roots.push(("generated-bindings".into(), args.out_dir.clone()));
    super::ohos::build(super::ohos::BuildOptions {
        cargo_bin: args.cargo_bin.clone(),
        core_manifest_path: Some(manifest_path),
        additional_source_roots,
        manifest_path: ohos_manifest,
        facade_mode,
        dist_dir,
        package_name: args.package_name,
        module_name: args.module_name,
        package_version: args.package_version,
        author: args.author,
        license: args.license,
        description: args.description,
        compatible_sdk_version: args.compatible_sdk_version,
        compatible_sdk_type: args.compatible_sdk_type,
        device_types: args.device_types,
        har_out: args.har_out,
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
        dts_cache: args.dts_cache,
        skip_check: args.skip_check,
        zigbuild: args.zigbuild,
        bisheng: args.bisheng,
        package: args.package,
        skip_napi_check: args.skip_napi_check,
        soname: args.soname,
        output_lock_held: args.output_lock_held,
    })?;

    Ok(())
}

fn add_cargo_feature_args(command: &mut Command, features: &[String]) {
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }
}

fn dependency_cargo_feature_args(package_name: &str, features: &[String]) -> Vec<String> {
    if features.is_empty() {
        Vec::new()
    } else {
        vec![
            "--features".to_string(),
            features
                .iter()
                .map(|feature| format!("{package_name}/{feature}"))
                .collect::<Vec<_>>()
                .join(","),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::dependency_cargo_feature_args;

    #[test]
    fn napi_and_ohos_dependency_feature_args_are_package_qualified() {
        let args = dependency_cargo_feature_args(
            "uni-core",
            &[
                "local-llm".to_string(),
                "local-llm-vision".to_string(),
                "local-llm-audio".to_string(),
            ],
        );

        assert_eq!(
            args,
            vec![
                "--features".to_string(),
                "uni-core/local-llm,uni-core/local-llm-vision,uni-core/local-llm-audio".to_string(),
            ]
        );
    }

    #[test]
    fn napi_and_ohos_dependency_feature_args_are_empty_without_features() {
        assert!(dependency_cargo_feature_args("uni-core", &[]).is_empty());
    }
}

fn run_wasm_bindgen_in_process(
    wasm_artifact: &Utf8Path,
    out_dir: &Utf8Path,
    target: WasmBindgenTargetArg,
) -> Result<()> {
    let mut bindgen = Bindgen::new();
    match target {
        WasmBindgenTargetArg::Web => {
            bindgen.web(true)?;
        }
        WasmBindgenTargetArg::Nodejs => {
            bindgen.nodejs(true)?;
        }
        WasmBindgenTargetArg::Bundler => {
            bindgen.bundler(true)?;
        }
        WasmBindgenTargetArg::NoModules => {
            bindgen.no_modules(true)?;
        }
        WasmBindgenTargetArg::Deno => {
            bindgen.deno(true)?;
        }
    };
    bindgen.input_path(wasm_artifact.as_std_path());
    bindgen.typescript(true);
    bindgen
        .generate(out_dir.as_std_path())
        .with_context(|| format!("running built-in wasm-bindgen for {wasm_artifact}"))?;
    Ok(())
}

fn cargo_build_command<'a>(
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

fn run_command(
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

fn ensure_single_generated_rs(dir: &Utf8Path) -> Result<()> {
    let mut stems = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {dir}"))? {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("generated path is not utf8: {}", p.display()))?;
        if path.extension() == Some("rs") {
            let stem = path
                .file_stem()
                .with_context(|| format!("generated Rust bridge path has no stem: {path}"))?;
            stems.push(stem.to_string());
        }
    }
    match stems.as_slice() {
        [_stem] => Ok(()),
        [] => bail!("no generated Rust bridge (*.rs) found in {dir}"),
        _ => bail!("multiple generated Rust bridges found in {dir}: {stems:?}"),
    }
}

fn generated_addon_stem(dir: &Utf8Path) -> Result<String> {
    for file_name in ["backend-napi.ts", "preload.cjs"] {
        let path = dir.join(file_name);
        if path.exists() {
            let text = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
            if let Some(stem) = parse_node_addon_stem(&text) {
                return Ok(stem);
            }
        }
    }
    bail!("no generated addon reference (*.node) found in {dir}")
}

fn parse_node_addon_stem(text: &str) -> Option<String> {
    let marker = ".node";
    for (idx, _) in text.match_indices(marker) {
        let before = &text[..idx];
        let Some(quote_idx) = before.rfind(['"', '\'']) else {
            continue;
        };
        let raw = &text[quote_idx + 1..idx];
        let Some(stem) = raw.rsplit(['/', '\\']).next() else {
            continue;
        };
        if !stem.is_empty() {
            return Some(stem.to_string());
        }
    }
    None
}

fn emit_browser_auto_entrypoint(
    out_dir: &Utf8Path,
    wasm_bindgen_out_dir: &Utf8Path,
    wasm_bindgen_stem: &str,
    target: WasmBindgenTargetArg,
) -> Result<()> {
    let browser_dir = out_dir.join("browser");
    let entrypoint = browser_dir.join("index.web.ts");
    if !target.emits_web_auto_entrypoint() {
        if entrypoint.exists() {
            std::fs::remove_file(&entrypoint)
                .with_context(|| format!("removing stale browser auto-entrypoint {entrypoint}"))?;
        }
        return Ok(());
    }

    std::fs::create_dir_all(&browser_dir)
        .with_context(|| format!("creating browser output dir {browser_dir}"))?;
    let browser_dir_abs = browser_dir
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing browser dir {browser_dir}"))?;
    let wasm_bindgen_out_dir_abs = wasm_bindgen_out_dir.canonicalize_utf8().with_context(|| {
        format!("canonicalizing wasm-bindgen output dir {wasm_bindgen_out_dir}")
    })?;
    let rel_pkg_dir = relative_path_from_dir(&browser_dir_abs, &wasm_bindgen_out_dir_abs)
        .to_string()
        .replace('\\', "/");
    let rel_pkg_dir = if rel_pkg_dir.is_empty() {
        ".".to_string()
    } else if rel_pkg_dir.starts_with('.') {
        rel_pkg_dir
    } else {
        format!("./{rel_pkg_dir}")
    };
    let glue_path = format!("{rel_pkg_dir}/{wasm_bindgen_stem}.js");
    let wasm_url_path = format!("{rel_pkg_dir}/{wasm_bindgen_stem}_bg.wasm?url");

    let source = format!(
        r#"// AUTOGENERATED by uniffi_bindgen_javascript (wasm web auto-entrypoint).
//
// This file is emitted by `uniffi-bindgen javascript build-wasm`
// after `wasm-bindgen --target web` has produced the final JS glue
// and `.wasm` asset. Advanced consumers can still import
// `./index.ts` and call `initBackend(glue, init?)` manually.

import * as glue from "{glue_path}";
import wasmUrl from "{wasm_url_path}";
import {{ initBackend }} from "./index.ts";

export * from "./index.ts";

let readyPromise: Promise<void> | null = null;

export function init(input: unknown = wasmUrl): Promise<void> {{
    readyPromise ??= initBackend(glue, input);
    return readyPromise;
}}

export const ready: Promise<void> = init();
"#,
    );
    std::fs::write(&entrypoint, source)
        .with_context(|| format!("writing browser auto-entrypoint {entrypoint}"))?;
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
            std::fs::remove_dir_all(&snippets_dest).with_context(|| {
                format!("removing stale Mini Program snippets dir {snippets_dest}")
            })?;
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
    Ok(patch_mini_program_text_encoding(&patched))
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
    let browser_dir = out_dir.join("browser");
    let entrypoint = browser_dir.join("index.mini-program.ts");
    std::fs::create_dir_all(&browser_dir)
        .with_context(|| format!("creating browser output dir {browser_dir}"))?;
    let browser_dir_abs = browser_dir
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing browser dir {browser_dir}"))?;
    let mini_program_out_dir_abs = mini_program_out_dir.canonicalize_utf8().with_context(|| {
        format!("canonicalizing Mini Program artifact dir {mini_program_out_dir}")
    })?;
    let rel_artifact_dir = relative_path_from_dir(&browser_dir_abs, &mini_program_out_dir_abs)
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

    let source = format!(
        r#"// AUTOGENERATED by uniffi_bindgen_javascript (wasm Mini Program auto-entrypoint).
//
// This file is emitted by `uniffi-bindgen artifacts build --target mini-program`.
// It consumes a patched wasm-bindgen JS glue module whose default init
// calls `WXWebAssembly.instantiate(packagePath, imports)`.

import * as glue from "{glue_path}";
import {{ initBackend, type WasmBindgenGlue }} from "./index.ts";

export * from "./index.ts";

declare const WXWebAssembly:
    | undefined
    | {{
          instantiate(
              path: string,
              imports: WebAssembly.Imports,
          ): Promise<WebAssembly.WebAssemblyInstantiatedSource>;
      }};

export const DEFAULT_WASM_PATH = "{default_wasm_path}";

let readyPromise: Promise<void> | null = null;

function assertWXWebAssembly(): void {{
    if (typeof WXWebAssembly === "undefined" || typeof WXWebAssembly.instantiate !== "function") {{
        throw new Error("UniFFI Mini Program wasm init requires WXWebAssembly.instantiate(path, imports)");
    }}
}}

export function init(wasmPath: string = DEFAULT_WASM_PATH): Promise<void> {{
    return initWithGlue(glue, wasmPath);
}}

export function initWithPath(wasmPath: string): Promise<void> {{
    return init(wasmPath);
}}

export function initWithGlue(
    customGlue: WasmBindgenGlue | Promise<WasmBindgenGlue>,
    wasmPath: string,
): Promise<void> {{
    assertWXWebAssembly();
    readyPromise ??= initBackend(customGlue, wasmPath);
    return readyPromise;
}}
"#,
    );
    std::fs::write(&entrypoint, source)
        .with_context(|| format!("writing Mini Program auto-entrypoint {entrypoint}"))?;
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
    workspace_root: Utf8PathBuf,
    local_source_roots: Vec<(String, Utf8PathBuf)>,
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

    fn wasm_artifact_path(&self, release: bool) -> Utf8PathBuf {
        self.target_directory
            .join("wasm32-unknown-unknown")
            .join(if release { "release" } else { "debug" })
            .join(format!("{}.wasm", self.lib_target_name))
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
    let workspace_root =
        Utf8PathBuf::from_path_buf(metadata.workspace_root.clone().into_std_path_buf())
            .map_err(|p| anyhow::anyhow!("cargo workspace root is not utf8: {}", p.display()))?;
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
    local_source_roots.sort();
    local_source_roots.dedup();
    Ok(CargoPackageMetadata {
        target_directory: Utf8PathBuf::from_path_buf(
            metadata.target_directory.clone().into_std_path_buf(),
        )
        .map_err(|p| anyhow::anyhow!("cargo metadata target dir is not utf8: {}", p.display()))?,
        workspace_root,
        local_source_roots,
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
