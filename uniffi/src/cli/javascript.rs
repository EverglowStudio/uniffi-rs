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

#[derive(Args)]
pub(crate) struct JavascriptArgs {
    #[clap(subcommand)]
    pub command: JavascriptCommands,
}

#[derive(Subcommand)]
pub(crate) enum JavascriptCommands {
    /// Build JavaScript + wasm bindings, emit the wasm host crate, and run wasm-bindgen.
    ///
    /// This path targets downstream crates whose generated wasm host crate can
    /// directly call the Rust API described by the chosen UDL/library input.
    BuildWasm(BuildWasmArgs),

    /// Build JavaScript + N-API bindings, emit/build the napi host crate, and copy `.node` addons.
    BuildNapi(BuildNapiArgs),
}

#[derive(Clone, Args)]
pub(crate) struct BuildWasmArgs {
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

    /// Directory (default `rust_modules`) in which to emit the generated wasm host crate.
    #[clap(long = "host-crates-dir", default_value = "rust_modules")]
    host_crates_dir: Utf8PathBuf,

    /// Where to write the wasm-bindgen output tree. Defaults to `<out-dir>/browser/pkg`.
    #[clap(long = "wasm-bindgen-out-dir")]
    wasm_bindgen_out_dir: Option<Utf8PathBuf>,

    /// wasm-bindgen output target.
    #[clap(long = "wasm-bindgen-target", value_enum, default_value = "web")]
    wasm_bindgen_target: WasmBindgenTargetArg,

    /// Override the `cargo` binary to invoke.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    cargo_bin: String,

    /// Override the `wasm-bindgen` binary to invoke.
    #[clap(long = "wasm-bindgen-bin", default_value = "wasm-bindgen")]
    wasm_bindgen_bin: String,

    /// Build the downstream core crate and generated wasm host crate in release mode.
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

#[derive(Clone, Args)]
pub(crate) struct BuildNapiArgs {
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

    /// Directory (default `rust_modules`) in which to emit the generated napi host crate.
    #[clap(long = "host-crates-dir", default_value = "rust_modules")]
    host_crates_dir: Utf8PathBuf,

    /// N-API consumption form(s) to emit. Defaults to both node and electron.
    #[clap(long = "flavor", value_enum)]
    flavor: Vec<NapiBuildFlavorArg>,

    /// Override the `cargo` binary to invoke.
    #[clap(long = "cargo-bin", default_value = "cargo")]
    cargo_bin: String,

    /// Cargo target directory for the generated N-API host build.
    #[clap(long = "target-dir")]
    target_dir: Option<Utf8PathBuf>,

    /// Build the downstream core crate and generated napi host crate in release mode.
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

#[derive(Copy, Clone, ValueEnum)]
enum NapiBuildFlavorArg {
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

#[derive(Copy, Clone, ValueEnum)]
enum WasmBindgenTargetArg {
    Web,
    Nodejs,
    Bundler,
    NoModules,
    Deno,
}

impl WasmBindgenTargetArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::Nodejs => "nodejs",
            Self::Bundler => "bundler",
            Self::NoModules => "no-modules",
            Self::Deno => "deno",
        }
    }
}

pub(crate) fn run(args: JavascriptArgs) -> Result<()> {
    match args.command {
        JavascriptCommands::BuildWasm(args) => build_wasm(args),
        JavascriptCommands::BuildNapi(args) => build_napi(args),
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

fn build_wasm(args: BuildWasmArgs) -> Result<()> {
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
        }),
        vec![FlavorTarget::Wasm],
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
        .unwrap_or_else(|| args.out_dir.join("browser/pkg"));
    std::fs::create_dir_all(&wasm_bindgen_out_dir)
        .with_context(|| format!("creating wasm-bindgen output dir {wasm_bindgen_out_dir}"))?;

    let mut wasm_bindgen = Command::new(&args.wasm_bindgen_bin);
    wasm_bindgen
        .arg("--target")
        .arg(args.wasm_bindgen_target.as_str())
        .arg("--out-dir")
        .arg(wasm_bindgen_out_dir.as_str())
        .arg(wasm_artifact.as_str());
    run_command(
        &args.wasm_bindgen_bin,
        &mut wasm_bindgen,
        "wasm-bindgen",
        "install wasm-bindgen-cli with `cargo install wasm-bindgen-cli` or pass --wasm-bindgen-bin <path>",
    )?;

    Ok(())
}

fn build_napi(args: BuildNapiArgs) -> Result<()> {
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
        }),
        flavors.clone(),
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
            FlavorTarget::Wasm => continue,
        };
        let flavor_dir = args.out_dir.join(subdir);
        ensure_single_generated_rs(&flavor_dir)
            .with_context(|| format!("finding generated Rust bridge in {flavor_dir}"))?;
        let addon_stem = generated_addon_stem(&flavor_dir)
            .with_context(|| format!("finding generated addon name in {flavor_dir}"))?;
        let addon_path = flavor_dir.join(format!("{addon_stem}.node"));
        std::fs::create_dir_all(&flavor_dir)
            .with_context(|| format!("creating addon output dir {flavor_dir}"))?;
        std::fs::copy(&napi_artifact, &addon_path)
            .with_context(|| format!("copying built napi addon {napi_artifact} to {addon_path}"))?;
    }

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
