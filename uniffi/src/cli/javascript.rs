/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::MetadataCommand;
use clap::{Args, Subcommand, ValueEnum};
use std::process::Command;
use uniffi_bindgen::{cargo_metadata::CrateConfigSupplier, BindgenLoader, BindgenPaths};
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

    /// Directory for built non-source artifacts. Defaults to `<host-crate>/dist` for OHOS output when omitted.
    #[clap(long = "artifact-dir")]
    pub(crate) artifact_dir: Option<Utf8PathBuf>,

    /// Output directory for built OHOS dist artifacts (intermediate native output).
    #[clap(long = "dist-dir")]
    pub(crate) dist_dir: Option<Utf8PathBuf>,

    /// OHPM package name for generated HAR metadata (supports scoped names like `@scope/name`).
    #[clap(long = "package-name")]
    pub(crate) package_name: Option<String>,

    /// Output `.har` path. Defaults to `<artifact-root>/<package>.har`.
    #[clap(long = "har-out")]
    pub(crate) har_out: Option<Utf8PathBuf>,

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
    if let Some(cfg) = config.clone() {
        paths.add_config_override_layer(cfg);
    }
    let mut cargo_metadata = MetadataCommand::new();
    cargo_metadata.manifest_path(manifest_path.as_std_path());
    if metadata_no_deps {
        cargo_metadata.no_deps();
    }
    let metadata = cargo_metadata
        .exec()
        .with_context(|| format!("running cargo metadata for {manifest_path}"))?;
    paths.add_layer(CrateConfigSupplier::from(metadata));
    let loader = BindgenLoader::new(paths);
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
    let mut build_napi_host =
        cargo_build_command(&args.cargo_bin, &napi_manifest, &[], args.release, None);
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
        vec![FlavorTarget::Harmony],
        args.artifact_dir.clone(),
    )?;

    let host_root = if args.host_crates_dir.is_absolute() {
        args.host_crates_dir.clone()
    } else {
        Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
            .join(&args.host_crates_dir)
    };
    let ohos_dir = host_root.join("ohos");
    let ohos_manifest = ohos_dir.join("Cargo.toml");
    if !ohos_manifest.exists() {
        bail!(
            "OHOS host crate was not emitted at {}",
            ohos_manifest
                .parent()
                .unwrap_or_else(|| Utf8Path::new("<unknown>"))
        );
    }

    let arches = if args.arch.is_empty() {
        vec!["aarch".to_string(), "x64".to_string()]
    } else {
        args.arch.clone()
    };
    let dist_dir = args
        .dist_dir
        .clone()
        .or_else(|| args.artifact_dir.as_ref().map(|dir| dir.join("ohos/dist")))
        .unwrap_or_else(|| ohos_dir.join("dist"));
    super::ohos::build(super::ohos::BuildOptions {
        cargo_bin: args.cargo_bin.clone(),
        manifest_path: ohos_manifest,
        dist_dir,
        package_name: args.package_name,
        har_out: args.har_out,
        no_har: args.no_har,
        arches,
        target_dir: args.target_dir.clone(),
        release: args.release,
        cargo_args: args.cargo_args,
        copy_static: args.copy_static,
        skip_libs: args.skip_libs,
        dts_cache: args.dts_cache,
        skip_check: args.skip_check,
        zigbuild: args.zigbuild,
        bisheng: args.bisheng,
        package: args.package,
        skip_napi_check: args.skip_napi_check,
        soname: args.soname,
    })?;

    Ok(())
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
        "// AUTOGENERATED by uniffi_bindgen_javascript (wasm web auto-entrypoint).\n\
         //\n\
         // This file is emitted by `uniffi-bindgen javascript build-wasm`\n\
         // after `wasm-bindgen --target web` has produced the final JS glue\n\
         // and `.wasm` asset. Advanced consumers can still import\n\
         // `./index.ts` and call `initBackend(glue, init?)` manually.\n\
         \n\
         import * as glue from \"{glue_path}\";\n\
         import wasmUrl from \"{wasm_url_path}\";\n\
         import {{ initBackend }} from \"./index.ts\";\n\
         \n\
         export * from \"./index.ts\";\n\
         \n\
         let readyPromise: Promise<void> | null = null;\n\
         \n\
         export function init(input: unknown = wasmUrl): Promise<void> {{\n\
             readyPromise ??= initBackend(glue, input);\n\
             return readyPromise;\n\
         }}\n\
         \n\
         export const ready: Promise<void> = init();\n",
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

    emit_mini_program_auto_entrypoint(out_dir, mini_program_out_dir, wasm_bindgen_stem)?;
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
    Ok(patched.replace(
        "const ret = typeof window === 'undefined' ? null : window;",
        "const ret = null;",
    ))
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
        "// AUTOGENERATED by uniffi_bindgen_javascript (wasm Mini Program auto-entrypoint).\n\
         //\n\
         // This file is emitted by `uniffi-bindgen artifacts build --target mini-program`.\n\
         // It consumes a patched wasm-bindgen JS glue module whose default init\n\
         // calls `WXWebAssembly.instantiate(packagePath, imports)`.\n\
         \n\
         import * as glue from \"{glue_path}\";\n\
         import {{ initBackend, type WasmBindgenGlue }} from \"./index.ts\";\n\
         \n\
         export * from \"./index.ts\";\n\
         \n\
         declare const WXWebAssembly:\n\
             | undefined\n\
             | {{\n\
                   instantiate(\n\
                       path: string,\n\
                       imports: WebAssembly.Imports,\n\
                   ): Promise<WebAssembly.WebAssemblyInstantiatedSource>;\n\
               }};\n\
         \n\
         export const DEFAULT_WASM_PATH = \"{default_wasm_path}\";\n\
         \n\
         let readyPromise: Promise<void> | null = null;\n\
         \n\
         function assertWXWebAssembly(): void {{\n\
             if (typeof WXWebAssembly === \"undefined\" || typeof WXWebAssembly.instantiate !== \"function\") {{\n\
                 throw new Error(\"UniFFI Mini Program wasm init requires WXWebAssembly.instantiate(path, imports)\");\n\
             }}\n\
         }}\n\
         \n\
         export function init(wasmPath: string = DEFAULT_WASM_PATH): Promise<void> {{\n\
             return initWithGlue(glue, wasmPath);\n\
         }}\n\
         \n\
         export function initWithPath(wasmPath: string): Promise<void> {{\n\
             return init(wasmPath);\n\
         }}\n\
         \n\
         export function initWithGlue(customGlue: WasmBindgenGlue | Promise<WasmBindgenGlue>, wasmPath: string): Promise<void> {{\n\
             assertWXWebAssembly();\n\
             readyPromise ??= initBackend(customGlue, wasmPath);\n\
             return readyPromise;\n\
         }}\n",
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
    Ok(CargoPackageMetadata {
        target_directory: Utf8PathBuf::from_path_buf(
            metadata.target_directory.clone().into_std_path_buf(),
        )
        .map_err(|p| anyhow::anyhow!("cargo metadata target dir is not utf8: {}", p.display()))?,
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
