/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::javascript::{
    build_napi, build_ohos, build_wasm, BuildNapiArgs, BuildOhosArgs, BuildWasmArgs,
    NapiBuildFlavorArg, WasmBindgenTargetArg,
};
use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use clap::{Args, Subcommand, ValueEnum};
use std::process::Command;

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

    /// Directory in which to write generated files.
    #[clap(long, short)]
    out_dir: Utf8PathBuf,

    /// Artifact target(s) to build. P0/P1 supports wasm, node, electron, and harmony.
    #[clap(long = "target", value_enum)]
    target: Vec<ArtifactTargetArg>,

    /// Optional override for the library/cdylib path used for generation.
    #[clap(long = "library-path")]
    library_path: Option<Utf8PathBuf>,

    /// Optional UDL or source input passed directly to the generator.
    #[clap(long)]
    source: Option<Utf8PathBuf>,

    /// Directory (default `rust_modules`) in which to emit generated host crates.
    #[clap(long = "host-crates-dir", default_value = "rust_modules")]
    host_crates_dir: Utf8PathBuf,

    /// Build downstream core and generated host crates in release mode.
    #[clap(long)]
    release: bool,

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

    /// Override the `wasm-bindgen` binary to invoke.
    #[clap(long = "wasm-bindgen-bin", default_value = "wasm-bindgen")]
    wasm_bindgen_bin: String,

    /// Cargo target directory for the generated N-API host build.
    #[clap(long = "napi-target-dir")]
    napi_target_dir: Option<Utf8PathBuf>,

    /// Output directory passed to `ohrs build --dist`.
    #[clap(long = "ohos-dist-dir")]
    ohos_dist_dir: Option<Utf8PathBuf>,

    /// OHOS architecture alias passed to `ohrs build --arch`. Defaults to `aarch` and `x64`.
    #[clap(long = "ohos-arch")]
    ohos_arch: Vec<String>,

    /// Override the `ohrs` binary used to build the OHOS host crate.
    #[clap(long = "ohrs-bin")]
    ohrs_bin: Option<String>,

    /// Optional local checkout of ohos-rs; when set, generated host crate uses path deps.
    #[clap(long = "ohos-rs-dir")]
    ohos_rs_dir: Option<Utf8PathBuf>,

    /// Cargo target directory passed through to `ohrs build --target-dir`.
    #[clap(long = "ohos-target-dir")]
    ohos_target_dir: Option<Utf8PathBuf>,

    /// Additional cargo args passed to `ohrs build` after `--`.
    #[clap(last = true)]
    ohos_cargo_args: Vec<String>,
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Debug, ValueEnum)]
pub(crate) enum ArtifactTargetArg {
    Wasm,
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
    node: bool,
    electron: bool,
    harmony: bool,
    apple: bool,
    android: bool,
}

pub(crate) fn run(args: ArtifactsArgs) -> Result<()> {
    match args.command {
        ArtifactsCommands::Build(args) => build(args),
    }
}

fn build(args: BuildArgs) -> Result<()> {
    let targets = expand_targets(&args.target)?;

    if targets.apple {
        bail!("artifacts build --target apple is not implemented yet; see artifact CLI roadmap P2");
    }
    if targets.android {
        bail!(
            "artifacts build --target android is not implemented yet; see artifact CLI roadmap P3"
        );
    }

    if targets.wasm {
        build_wasm(args.to_wasm_args()).context("building wasm artifact target")?;
    }

    let mut napi_flavors = Vec::new();
    if targets.node {
        napi_flavors.push(NapiBuildFlavorArg::Napi);
    }
    if targets.electron {
        napi_flavors.push(NapiBuildFlavorArg::Electron);
    }
    if !napi_flavors.is_empty() {
        build_napi(args.to_napi_args(napi_flavors)).context("building N-API artifact target")?;
    }

    if targets.harmony {
        build_ohos(args.to_ohos_args()?).context("building Harmony/OpenHarmony artifact target")?;
    }

    Ok(())
}

fn expand_targets(targets: &[ArtifactTargetArg]) -> Result<ExpandedTargets> {
    if targets.is_empty() {
        bail!("at least one --target is required");
    }

    let mut expanded = ExpandedTargets::default();
    for target in targets {
        match target {
            ArtifactTargetArg::Wasm => expanded.wasm = true,
            ArtifactTargetArg::Node => expanded.node = true,
            ArtifactTargetArg::Electron => expanded.electron = true,
            ArtifactTargetArg::Harmony => expanded.harmony = true,
            ArtifactTargetArg::Apple => expanded.apple = true,
            ArtifactTargetArg::Android => expanded.android = true,
            ArtifactTargetArg::AllJs | ArtifactTargetArg::All => {
                expanded.wasm = true;
                expanded.node = true;
                expanded.electron = true;
                expanded.harmony = true;
            }
        }
    }
    Ok(expanded)
}

impl BuildArgs {
    fn to_wasm_args(&self) -> BuildWasmArgs {
        BuildWasmArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir.clone(),
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir.clone(),
            wasm_bindgen_out_dir: self.wasm_bindgen_out_dir.clone(),
            wasm_bindgen_target: self.wasm_bindgen_target,
            cargo_bin: self.cargo_bin.clone(),
            wasm_bindgen_bin: self.wasm_bindgen_bin.clone(),
            release: self.release,
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
        }
    }

    fn to_napi_args(&self, flavor: Vec<NapiBuildFlavorArg>) -> BuildNapiArgs {
        BuildNapiArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir.clone(),
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir.clone(),
            flavor,
            cargo_bin: self.cargo_bin.clone(),
            target_dir: self.napi_target_dir.clone(),
            release: self.release,
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
        }
    }

    fn to_ohos_args(&self) -> Result<BuildOhosArgs> {
        Ok(BuildOhosArgs {
            manifest_path: self.manifest_path.clone(),
            out_dir: self.out_dir.clone(),
            library_path: self.library_path.clone(),
            source: self.source.clone(),
            host_crates_dir: self.host_crates_dir.clone(),
            dist_dir: self.ohos_dist_dir.clone(),
            arch: self.ohos_arch.clone(),
            cargo_bin: self.cargo_bin.clone(),
            ohrs_bin: self.resolve_ohrs_bin()?,
            ohos_rs_dir: self.ohos_rs_dir.clone(),
            target_dir: self.ohos_target_dir.clone(),
            release: self.release,
            no_format: self.no_format,
            config: self.config.clone(),
            crate_name: self.crate_name.clone(),
            metadata_no_deps: self.metadata_no_deps,
            cargo_args: self.ohos_cargo_args.clone(),
        })
    }

    fn resolve_ohrs_bin(&self) -> Result<String> {
        if let Some(bin) = &self.ohrs_bin {
            return Ok(bin.clone());
        }
        let Some(ohos_rs_dir) = &self.ohos_rs_dir else {
            return Ok("ohrs".to_string());
        };

        let manifest = ohos_rs_dir.join("cli/cargo-ohrs/Cargo.toml");
        if !manifest.exists() {
            bail!(
                "--ohos-rs-dir was provided, but {} does not exist",
                manifest
            );
        }

        let mut cargo = Command::new(&self.cargo_bin);
        cargo
            .arg("build")
            .arg("--manifest-path")
            .arg(manifest.as_str())
            .arg("--bin")
            .arg("ohrs");
        let rendered = format!("{cargo:?}");
        let output = cargo.output().with_context(|| {
            format!(
                "spawning cargo to build ohrs from {}. Pass --ohrs-bin <path> to skip auto-build",
                manifest
            )
        })?;
        if !output.status.success() {
            bail!(
                "cargo command failed while building ohrs: {rendered}\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let bin = ohos_rs_dir
            .join("target/debug")
            .join(if cfg!(target_os = "windows") {
                "ohrs.exe"
            } else {
                "ohrs"
            });
        if !bin.exists() {
            bail!("built ohrs binary not found at {}", bin);
        }
        Ok(bin.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_all_js_targets() {
        assert_eq!(
            expand_targets(&[ArtifactTargetArg::AllJs]).unwrap(),
            ExpandedTargets {
                wasm: true,
                node: true,
                electron: true,
                harmony: true,
                apple: false,
                android: false,
            }
        );
    }

    #[test]
    fn expands_node_electron_as_one_napi_group() {
        assert_eq!(
            expand_targets(&[ArtifactTargetArg::Node, ArtifactTargetArg::Electron]).unwrap(),
            ExpandedTargets {
                wasm: false,
                node: true,
                electron: true,
                harmony: false,
                apple: false,
                android: false,
            }
        );
    }

    #[test]
    fn rejects_empty_target_list() {
        assert!(expand_targets(&[]).is_err());
    }
}
