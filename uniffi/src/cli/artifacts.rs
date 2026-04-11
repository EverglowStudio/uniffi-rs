/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::javascript::{
    build_napi, build_ohos, build_wasm, BuildNapiArgs, BuildOhosArgs, BuildWasmArgs,
    NapiBuildFlavorArg, WasmBindgenTargetArg,
};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::MetadataCommand;
use clap::{Args, Subcommand, ValueEnum};
use std::process::Command;
use uniffi_bindgen::bindings::{generate, GenerateOptions, TargetLanguage};

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
        build_apple(&args).context("building Apple artifact target")?;
    }
    if targets.android {
        build_android(&args).context("building Android artifact target")?;
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

fn build_android(args: &BuildArgs) -> Result<()> {
    if args.android_aar_out.is_some() {
        bail!("--android-aar-out is not implemented yet; P3 currently builds jniLibs + Kotlin");
    }

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

    let kotlin_bindings_dir = args.out_dir.join("kotlin");
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
    })?;

    if let Some(kotlin_out) = &args.android_kotlin_out {
        copy_dir_contents(&kotlin_bindings_dir, kotlin_out)?;
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

    let mut host_build = Command::new(&args.cargo_bin);
    host_build
        .arg("build")
        .arg("--manifest-path")
        .arg(args.manifest_path.as_str());
    if args.release {
        host_build.arg("--release");
    }
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
        if args.release {
            cargo.arg("--release");
        }
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

    let swift_bindings_dir = args.out_dir.join("swift");
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
    })?;

    let headers_dir = args.out_dir.join("apple/headers");
    stage_swift_headers(&swift_bindings_dir, &headers_dir)?;

    if let Some(swift_out) = &args.apple_swift_out {
        copy_swift_sources(&swift_bindings_dir, swift_out)?;
    }

    if let Some(parent) = xcframework_out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating XCFramework parent dir {parent}"))?;
    }
    if xcframework_out.exists() {
        std::fs::remove_dir_all(&xcframework_out)
            .with_context(|| format!("removing stale XCFramework {xcframework_out}"))?;
    }

    let libs: Vec<_> = targets
        .iter()
        .map(|target| apple_staticlib_path(&meta, target, profile))
        .collect();
    let mut xcodebuild = Command::new("xcodebuild");
    xcodebuild.args(xcodebuild_create_xcframework_args(
        &libs,
        &headers_dir,
        &xcframework_out,
    ));
    run_command("xcodebuild", &mut xcodebuild, "xcodebuild")?;

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

fn stage_swift_headers(swift_bindings_dir: &Utf8Path, headers_dir: &Utf8Path) -> Result<()> {
    if headers_dir.exists() {
        std::fs::remove_dir_all(headers_dir)
            .with_context(|| format!("removing stale headers dir {headers_dir}"))?;
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

#[derive(Debug)]
struct CargoPackageMetadata {
    target_directory: Utf8PathBuf,
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
        lib_target_name: lib_target.name.clone(),
    })
}

fn host_cdylib_path(meta: &CargoPackageMetadata, release: bool) -> Utf8PathBuf {
    meta.target_directory
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

fn apple_staticlib_path(meta: &CargoPackageMetadata, target: &str, profile: &str) -> Utf8PathBuf {
    meta.target_directory
        .join(target)
        .join(profile)
        .join(format!("lib{}.a", meta.lib_target_name))
}

fn xcodebuild_create_xcframework_args(
    libs: &[Utf8PathBuf],
    headers_dir: &Utf8Path,
    output: &Utf8Path,
) -> Vec<String> {
    let mut args = vec!["-create-xcframework".to_string()];
    for lib in libs {
        args.push("-library".to_string());
        args.push(lib.to_string());
        args.push("-headers".to_string());
        args.push(headers_dir.to_string());
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
    let dir = args.out_dir.join("android");
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
    if to.exists() {
        std::fs::remove_dir_all(to).with_context(|| format!("removing stale directory {to}"))?;
    }
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

    #[test]
    fn computes_apple_staticlib_path() {
        let meta = CargoPackageMetadata {
            target_directory: Utf8PathBuf::from("/repo/target"),
            lib_target_name: "uni_core".to_string(),
        };
        assert_eq!(
            apple_staticlib_path(&meta, "aarch64-apple-ios", "release"),
            Utf8PathBuf::from("/repo/target/aarch64-apple-ios/release/libuni_core.a")
        );
    }

    #[test]
    fn renders_xcodebuild_create_xcframework_args() {
        let args = xcodebuild_create_xcframework_args(
            &[
                Utf8PathBuf::from("/target/device/libuni_core.a"),
                Utf8PathBuf::from("/target/sim/libuni_core.a"),
            ],
            Utf8Path::new("/headers"),
            Utf8Path::new("/out/uni_core.xcframework"),
        );
        assert_eq!(
            args,
            vec![
                "-create-xcframework",
                "-library",
                "/target/device/libuni_core.a",
                "-headers",
                "/headers",
                "-library",
                "/target/sim/libuni_core.a",
                "-headers",
                "/headers",
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
        let meta = CargoPackageMetadata {
            target_directory: Utf8PathBuf::from("/repo/target"),
            lib_target_name: "uni_core".to_string(),
        };
        assert_eq!(
            android_sharedlib_path(&meta, "aarch64-linux-android", "debug"),
            Utf8PathBuf::from("/repo/target/aarch64-linux-android/debug/libuni_core.so")
        );
    }
}
