/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{BuildScript, Message, MetadataCommand, Package};
use clap::ValueEnum;
use flate2::read::GzDecoder;
use flate2::{Compression, GzBuilder};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::OpenOptions;
use std::io::{BufReader, Cursor, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tar::{Builder, EntryType, Header};

#[cfg(test)]
use super::artifact_staging::engine::*;
#[cfg(not(test))]
use super::artifact_staging::*;

const RUSTC_APPEND_ARGS_ENV: &str = "UNIFFI_OHOS_RUSTC_APPEND_ARGS";
const DEFAULT_DEVICE_TYPES: &[&str] = &["phone", "tablet", "2in1"];
const ALLOWED_DEVICE_TYPES: &[&str] = &[
    "default", "phone", "tablet", "2in1", "tv", "wearable", "car",
];
const MIN_HSP_API: u32 = 12;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, ValueEnum)]
pub(crate) enum PackageKind {
    #[default]
    Har,
    Hsp,
}

pub(crate) struct HspFrontendPreflight<'a> {
    pub package_kind: PackageKind,
    pub integrated_hsp: bool,
    pub hsp_bundle_name: Option<&'a str>,
    pub has_har_output: bool,
    pub has_hsp_output: bool,
    pub no_har: bool,
    pub skip_libs: bool,
    pub compatible_sdk_version: Option<&'a str>,
    pub target_sdk_version: Option<&'a str>,
    pub compatible_sdk_type: Option<&'a str>,
    pub bisheng: bool,
    pub hvigorw: Option<&'a str>,
    pub ohpm: Option<&'a str>,
    pub deveco_sdk_home: Option<&'a Utf8Path>,
}

pub(crate) struct BuildOptions {
    pub cargo_bin: String,
    pub core_manifest_path: Option<Utf8PathBuf>,
    pub additional_source_roots: Vec<(String, Utf8PathBuf)>,
    pub manifest_path: Utf8PathBuf,
    pub dist_dir: Utf8PathBuf,
    pub package_name: Option<String>,
    pub module_name: Option<String>,
    pub package_version: Option<String>,
    pub author: Option<String>,
    pub license: Option<String>,
    pub description: Option<String>,
    pub compatible_sdk_version: Option<String>,
    pub target_sdk_version: Option<String>,
    pub compatible_sdk_type: Option<String>,
    pub device_types: Vec<String>,
    pub package_kind: PackageKind,
    pub integrated_hsp: bool,
    pub hsp_bundle_name: Option<String>,
    pub har_out: Option<Utf8PathBuf>,
    pub runtime_hsp_out: Option<Utf8PathBuf>,
    pub interface_har_out: Option<Utf8PathBuf>,
    pub tgz_out: Option<Utf8PathBuf>,
    pub hvigorw: Option<String>,
    pub ohpm: Option<String>,
    pub deveco_sdk_home: Option<Utf8PathBuf>,
    pub no_har: bool,
    pub arches: Vec<String>,
    pub target_dir: Option<Utf8PathBuf>,
    pub release: bool,
    pub cargo_args: Vec<String>,
    pub copy_static: bool,
    pub skip_libs: bool,
    pub skip_check: bool,
    pub zigbuild: bool,
    pub bisheng: bool,
    pub package: Option<String>,
    pub skip_napi_check: bool,
    pub soname: Option<String>,
    pub frontend_hsp_preflight_done: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Arch {
    Arm64,
    Arm32,
    X86_64,
    LoongArch64,
}

impl Arch {
    fn parse(input: &str) -> Result<Self> {
        match input.to_ascii_lowercase().as_str() {
            "aarch" | "arm64" | "aarch64-linux-ohos" | "aarch64-unknown-linux-ohos" => {
                Ok(Self::Arm64)
            }
            "arm" | "arm32" | "arm-linux-ohos" | "armv7-unknown-linux-ohos" => {
                Ok(Self::Arm32)
            }
            "x86_64" | "x64" | "x86_64-linux-ohos" | "x86_64-unknown-linux-ohos" => {
                Ok(Self::X86_64)
            }
            "loongarch64" | "loongarch64-linux-ohos" | "loongarch64-unknown-linux-ohos" => {
                Ok(Self::LoongArch64)
            }
            _ => bail!(
                "unsupported OHOS arch `{input}`; expected aarch/arm64, arm/arm32, x86_64/x64, or loongarch64"
            ),
        }
    }

    fn dist_dir(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64-v8a",
            Self::Arm32 => "armeabi-v7a",
            Self::X86_64 => "x86_64",
            Self::LoongArch64 => "loongarch64",
        }
    }

    fn c_target(self) -> &'static str {
        match self {
            Self::Arm64 => "aarch64-linux-ohos",
            Self::Arm32 => "arm-linux-ohos",
            Self::X86_64 => "x86_64-linux-ohos",
            Self::LoongArch64 => "loongarch64-linux-ohos",
        }
    }

    fn rust_target(self) -> &'static str {
        match self {
            Self::Arm64 => "aarch64-unknown-linux-ohos",
            Self::Arm32 => "armv7-unknown-linux-ohos",
            Self::X86_64 => "x86_64-unknown-linux-ohos",
            Self::LoongArch64 => "loongarch64-unknown-linux-ohos",
        }
    }

    fn rust_link_target(self) -> &'static str {
        match self {
            Self::Arm64 => "AARCH64_UNKNOWN_LINUX_OHOS",
            Self::Arm32 => "ARMV7_UNKNOWN_LINUX_OHOS",
            Self::X86_64 => "X86_64_UNKNOWN_LINUX_OHOS",
            Self::LoongArch64 => "LOONGARCH64_UNKNOWN_LINUX_OHOS",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct HostPackage {
    pub(super) cargo_package_id: String,
    pub(super) name: String,
    pub(super) version: String,
    pub(super) description: Option<String>,
    pub(super) authors: Vec<String>,
    pub(super) license: Option<String>,
    pub(super) manifest_path: Utf8PathBuf,
    pub(super) lib_target_name: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeSdkType {
    HarmonyOs,
    OpenHarmony,
}

impl RuntimeSdkType {
    fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "harmonyos" => Ok(Self::HarmonyOs),
            "openharmony" => Ok(Self::OpenHarmony),
            _ => bail!(
                "unsupported Harmony runtime SDK type `{value}`; use `HarmonyOS` or `OpenHarmony`"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::HarmonyOs => "HarmonyOS",
            Self::OpenHarmony => "OpenHarmony",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SdkCompatibility {
    version: String,
    sdk_type: RuntimeSdkType,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CompileSdk {
    api_level: u32,
    platform_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct OhosPackageMetadata {
    name: String,
    module_name: String,
    version: String,
    description: Option<String>,
    author: Option<String>,
    license: Option<String>,
    sdk: Option<SdkCompatibility>,
    device_types: Vec<String>,
}

#[derive(Debug)]
struct HostPlan {
    target_directory: Utf8PathBuf,
    workspace_root: Utf8PathBuf,
    local_source_roots: Vec<(String, Utf8PathBuf)>,
    packages: Vec<HostPackage>,
    package_count: usize,
    explicit_package_arg: bool,
}

#[derive(Debug)]
struct ToolchainPaths {
    ranlib: String,
    ar: String,
    cc: String,
    cxx: String,
    llvm_as: String,
    ld: String,
    strip: String,
    objdump: String,
    objcopy: String,
    nm: String,
    bin_dir: String,
    lib_dir: String,
}

#[derive(Debug)]
struct BuiltArtifacts {
    paths: BTreeSet<Utf8PathBuf>,
    cargo_provenance: BTreeMap<Utf8PathBuf, CargoArtifactProvenance>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct CargoArtifactProvenance {
    package_id: String,
    target_name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RequiredCoreSo {
    package_id: String,
    name: String,
}

/// Native payload inventory used only to enumerate the ABI/name contract while
/// one HSP is being assembled. It deliberately contains no byte digest or
/// cross-file identity data; the generated package directory is the unit of
/// publication.
type HspSoInventory = BTreeMap<String, BTreeSet<String>>;

/// Invocation-local ELF validation facts used while stripping and checking a
/// staged HSP. These values are never serialized, published, or consumed as a
/// package metadata protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RuntimeElfFacts {
    is_64: bool,
    little_endian: bool,
    machine: u16,
    soname: Option<String>,
}

type RuntimeSoInventory = BTreeMap<String, BTreeMap<String, RuntimeElfFacts>>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProtectedDistPath {
    label: String,
    path: Utf8PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathRemap {
    source: Utf8PathBuf,
    destination: String,
}

#[derive(Clone, Debug)]
struct NativePathPolicy {
    remaps: Vec<PathRemap>,
}

struct OhosBuildEnvironment {
    vars: HashMap<String, String>,
    wrapper: OsString,
    inner_wrapper: Option<OsString>,
    append_args: String,
}

impl OhosBuildEnvironment {
    fn apply(&self, command: &mut Command) {
        command.envs(&self.vars);
        command.env("UNIFFI_OHOS_RUSTC_WRAPPER", "1");
        command.env(RUSTC_APPEND_ARGS_ENV, &self.append_args);
        command.env("RUSTC_WRAPPER", &self.wrapper);
        suppress_ohos_upstream_type_def_output(command);
        if let Some(inner) = &self.inner_wrapper {
            command.env("UNIFFI_OHOS_INNER_RUSTC_WRAPPER", inner);
        } else {
            command.env_remove("UNIFFI_OHOS_INNER_RUSTC_WRAPPER");
        }
    }
}

fn suppress_ohos_upstream_type_def_output(command: &mut Command) {
    command.env_remove("NAPI_TYPE_DEF_TMP_FOLDER");
    command.env_remove("TYPE_DEF_TMP_PATH");
}

pub(super) fn run_rustc_wrapper_main() -> ! {
    let mut command = match rustc_wrapper_command_from_env() {
        Ok(command) => command,
        Err(error) => {
            eprintln!("UniFFI OHOS rustc wrapper failed: {error:#}");
            std::process::exit(1);
        }
    };

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = command.exec();
        eprintln!("UniFFI OHOS rustc wrapper failed to exec the compiler: {error}");
        std::process::exit(1);
    }

    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => std::process::exit(status.code().unwrap_or(1)),
            Err(error) => {
                eprintln!("UniFFI OHOS rustc wrapper failed to run the compiler: {error}");
                std::process::exit(1);
            }
        }
    }
}

fn rustc_wrapper_command_from_env() -> Result<Command> {
    let mut argv = env::args_os().skip(1);
    let rustc = argv
        .next()
        .context("Cargo did not pass rustc to the OHOS rustc wrapper")?;
    let args = argv.collect::<Vec<_>>();
    let append_args = decode_wrapper_args(env::var_os(RUSTC_APPEND_ARGS_ENV).as_deref())?;
    rustc_wrapper_command(
        rustc,
        args,
        env::var_os("UNIFFI_OHOS_INNER_RUSTC_WRAPPER"),
        &append_args,
    )
}

fn rustc_wrapper_command(
    rustc: OsString,
    mut args: Vec<OsString>,
    inner_wrapper: Option<OsString>,
    append_args: &[OsString],
) -> Result<Command> {
    let current = env::current_exe()?.into_os_string();
    if same_executable(&rustc, &current)? {
        bail!(
            "Cargo passed the current UniFFI executable as rustc; refusing a recursive OHOS rustc wrapper chain"
        );
    }
    if let Some(inner) = inner_wrapper.as_deref() {
        if same_executable(inner, &current)? {
            bail!(
                "the configured inner rustc wrapper resolves to the current UniFFI executable; refusing a recursive OHOS rustc wrapper chain"
            );
        }
    }
    if rustc_invocation_targets_ohos(&args) {
        args.extend(append_args.iter().cloned());
    }
    let mut command = if let Some(inner_wrapper) = inner_wrapper {
        let mut command = Command::new(inner_wrapper);
        command.arg(&rustc);
        command
    } else {
        Command::new(&rustc)
    };
    command.args(args);
    Ok(command)
}

fn rustc_invocation_targets_ohos(args: &[OsString]) -> bool {
    args.windows(2).any(|window| {
        window[0] == OsStr::new("--target")
            && window[1]
                .to_str()
                .is_some_and(|target| target.ends_with("-linux-ohos"))
    }) || args.iter().any(|arg| {
        arg.to_str()
            .and_then(|arg| arg.strip_prefix("--target="))
            .is_some_and(|target| target.ends_with("-linux-ohos"))
    })
}

fn decode_wrapper_args(encoded: Option<&OsStr>) -> Result<Vec<OsString>> {
    let Some(encoded) = encoded else {
        return Ok(Vec::new());
    };
    let encoded = encoded
        .to_str()
        .context("UniFFI OHOS wrapper arguments are not valid Unicode")?;
    if encoded.is_empty() {
        return Ok(Vec::new());
    }
    let args = encoded
        .split('\x1f')
        .map(OsString::from)
        .collect::<Vec<_>>();
    if args.iter().any(|arg| arg.is_empty()) {
        bail!("UniFFI OHOS wrapper arguments contain an empty token");
    }
    Ok(args)
}

pub(crate) fn build(options: BuildOptions) -> Result<()> {
    if let Some(prepared) = build_internal(options)? {
        prepared.commit()?;
    }
    Ok(())
}

pub(crate) fn build_deferred_hsp(options: BuildOptions) -> Result<PreparedHspInvocation> {
    build_internal(options)?.context("deferred OHOS build did not produce an HSP invocation")
}

fn build_internal(options: BuildOptions) -> Result<Option<PreparedHspInvocation>> {
    validate_package_mode_options(&options)?;
    let ohos_ndk = env::var("OHOS_NDK_HOME").context(
        "OHOS_NDK_HOME is required for the built-in OHOS builder; configure the OHOS SDK/NDK before building Harmony artifacts",
    )?;
    if !Path::new(&ohos_ndk).exists() {
        bail!("OHOS_NDK_HOME does not exist: {ohos_ndk}");
    }

    let arches = parse_arches(&options.arches)?;
    let manifest_path = options
        .manifest_path
        .canonicalize_utf8()
        .unwrap_or_else(|_| options.manifest_path.clone());
    let manifest_dir = manifest_path
        .parent()
        .with_context(|| format!("OHOS manifest has no parent: {manifest_path}"))?;
    let plan = host_plan(&options.cargo_bin, &manifest_path, &options)?;
    let generated_source_root = generated_source_root(&options)?;
    let generated_package_root = generated_package_root(&options)?;
    require_generated_ark_files(&generated_source_root, &generated_package_root)?;
    // An explicitly supplied runtime type is never allowed to flow into a
    // native invocation as an arbitrary Hvigor runtimeOS value, including in
    // no-HAR mode where the remaining publication metadata is intentionally
    // ignored.
    if let Some(sdk_type) = options.compatible_sdk_type.as_deref() {
        RuntimeSdkType::parse(sdk_type)?;
    }
    validate_multi_package_output_overrides(&options, &plan)?;
    let package_metadata = if options.no_har {
        // Pure dist builds do not consume publication metadata.  Keeping this
        // branch free of OHPM/module/SDK validation preserves `--no-har` as a
        // native build mode and prevents stale package staging from affecting
        // the current invocation.
        vec![None; plan.packages.len()]
    } else {
        let sdk = resolve_sdk_compatibility(&options, &ohos_ndk)?;
        let metadata = plan
            .packages
            .iter()
            .map(|package| resolve_oh_package_metadata(&options, package, sdk.clone()))
            .collect::<Result<Vec<_>>>()?;
        ensure_unique_module_names(&plan.packages, &metadata)?;
        metadata.into_iter().map(Some).collect()
    };
    if options.package_kind == PackageKind::Hsp {
        preflight_hsp_environment(
            &options,
            package_metadata
                .iter()
                .map(|value| value.as_ref().expect("HSP metadata is resolved")),
        )?;
    }
    let target_dir = options
        .target_dir
        .clone()
        .map(|p| {
            if p.is_absolute() {
                p
            } else {
                manifest_dir.join(p)
            }
        })
        .unwrap_or_else(|| plan.target_directory.clone());
    let protected_dist_paths = protected_dist_paths(&options, &manifest_path, &plan, &target_dir)?;
    let package_dist_paths = plan
        .packages
        .iter()
        .map(|package| {
            let path = package_dist_dir(&options.dist_dir, package, plan.package_count);
            preflight_dist_output(&path, &protected_dist_paths)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut hsp_outputs = if options.package_kind == PackageKind::Hsp {
        let artifact_root = options
            .dist_dir
            .parent()
            .unwrap_or(options.dist_dir.as_path());
        plan.packages
            .iter()
            .zip(&package_metadata)
            .zip(&package_dist_paths)
            .map(|((package, metadata), dist)| {
                let mut outputs = resolve_hsp_output_paths(
                    &options,
                    artifact_root,
                    package,
                    metadata.as_ref().expect("HSP metadata is resolved"),
                    plan.package_count,
                )?;
                outputs.dist = Some(dist.clone());
                Ok(outputs)
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let package_labels = plan
        .packages
        .iter()
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    if options.package_kind == PackageKind::Hsp {
        normalize_hsp_destinations(&mut hsp_outputs, &package_labels)?;
    }
    // Resolve build inputs before any Cargo/native build can begin.
    let native_path_policy =
        NativePathPolicy::discover(&options, &plan, &manifest_path, &target_dir, &ohos_ndk)?;
    let required_core_so = resolve_required_core_so(&options)?;
    if options.package_kind == PackageKind::Hsp {
        let result = (|| -> Result<Option<PreparedHspInvocation>> {
            let mut prepared = Vec::with_capacity(plan.package_count);
            for (package_index, ((package, metadata), final_package_dist)) in plan
                .packages
                .iter()
                .zip(&package_metadata)
                .zip(&package_dist_paths)
                .enumerate()
            {
                let invocation_dist =
                    InvocationDist::new_detached(final_package_dist.clone(), &target_dir)?;
                let expected_so_inventory = build_package_dist_contents(
                    &options,
                    package,
                    &ohos_ndk,
                    &target_dir,
                    &invocation_dist.path,
                    &generated_source_root,
                    &native_path_policy,
                    &arches,
                    plan.explicit_package_arg,
                    required_core_so.as_ref(),
                )?;
                let staged = stage_hsp_outputs(
                    &options,
                    package,
                    &ohos_ndk,
                    metadata.as_ref().expect("HSP package metadata is resolved"),
                    &invocation_dist.path,
                    &hsp_outputs[package_index],
                    Some((&invocation_dist.path, &invocation_dist.final_path)),
                    &expected_so_inventory,
                    &target_dir,
                )?;
                prepared.push(PreparedHspPackage {
                    _invocation_dist: invocation_dist,
                    staged,
                });
            }
            let prepared = PreparedHspInvocation { prepared };
            Ok(Some(prepared))
        })();
        return match result {
            Ok(prepared) => Ok(prepared),
            Err(error) => Err(error),
        };
    }

    for ((package, metadata), final_package_dist) in plan
        .packages
        .iter()
        .zip(&package_metadata)
        .zip(&package_dist_paths)
    {
        // Each package uses a fresh invocation-scoped dist. All selected ABIs
        // and the optional HAR must succeed before replacing the public dist.
        build_package_dist_from_stage(final_package_dist, |package_dist_dir| {
            let _ = build_package_dist_contents(
                &options,
                package,
                &ohos_ndk,
                &target_dir,
                package_dist_dir,
                &generated_source_root,
                &native_path_policy,
                &arches,
                plan.explicit_package_arg,
                required_core_so.as_ref(),
            )?;
            if !options.no_har {
                package_output(
                    &options,
                    package,
                    metadata
                        .as_ref()
                        .expect("package metadata is resolved when final packaging is enabled"),
                    package_dist_dir,
                    plan.package_count,
                )?;
            }
            Ok(())
        })?;
    }
    Ok(None)
}

pub(crate) fn preflight_hsp_host_build(options: &BuildOptions) -> Result<()> {
    planned_hsp_host_build_outputs(options).map(|_| ())
}

pub(crate) fn planned_hsp_host_build_outputs(
    options: &BuildOptions,
) -> Result<Vec<HspOutputPaths>> {
    validate_package_mode_options(options)?;
    if options.package_kind != PackageKind::Hsp {
        return Ok(Vec::new());
    }
    let ohos_ndk = env::var("OHOS_NDK_HOME")
        .context("OHOS_NDK_HOME is required for read-only HSP host-plan preflight")?;
    if !Path::new(&ohos_ndk).exists() {
        bail!("OHOS_NDK_HOME does not exist: {ohos_ndk}");
    }
    parse_arches(&options.arches)?;
    let manifest_path = options
        .manifest_path
        .canonicalize_utf8()
        .unwrap_or_else(|_| options.manifest_path.clone());
    let plan = host_plan(&options.cargo_bin, &manifest_path, options)?;
    validate_multi_package_output_overrides(options, &plan)?;
    let sdk = resolve_sdk_compatibility(options, &ohos_ndk)?;
    let package_metadata = plan
        .packages
        .iter()
        .map(|package| resolve_oh_package_metadata(options, package, sdk.clone()))
        .collect::<Result<Vec<_>>>()?;
    ensure_unique_module_names(&plan.packages, &package_metadata)?;
    preflight_hsp_environment(options, package_metadata.iter())?;
    let artifact_root = options
        .dist_dir
        .parent()
        .unwrap_or(options.dist_dir.as_path());
    let mut outputs = plan
        .packages
        .iter()
        .zip(&package_metadata)
        .map(|(package, metadata)| {
            let mut outputs = resolve_hsp_output_paths(
                options,
                artifact_root,
                package,
                metadata,
                plan.package_count,
            )?;
            outputs.dist = Some(package_dist_dir(
                &options.dist_dir,
                package,
                plan.package_count,
            ));
            Ok(outputs)
        })
        .collect::<Result<Vec<_>>>()?;
    let labels = plan
        .packages
        .iter()
        .map(|package| package.name.clone())
        .collect::<Vec<_>>();
    normalize_hsp_destinations(&mut outputs, &labels)?;
    Ok(outputs)
}

#[derive(Clone, Copy)]
pub(crate) struct GeneratedHspOutputPreflight<'a> {
    pub dist_dir: &'a Utf8Path,
    pub generated_host_package_name: &'a str,
    pub package_name: Option<&'a str>,
    pub runtime_hsp_out: Option<&'a Utf8Path>,
    pub interface_har_out: Option<&'a Utf8Path>,
    pub tgz_out: Option<&'a Utf8Path>,
}

pub(crate) fn preflight_generated_hsp_outputs(
    options: GeneratedHspOutputPreflight<'_>,
) -> Result<()> {
    planned_generated_hsp_outputs(options).map(|_| ())
}

pub(crate) fn planned_generated_hsp_outputs(
    options: GeneratedHspOutputPreflight<'_>,
) -> Result<HspOutputPaths> {
    let artifact_root = options.dist_dir.parent().unwrap_or(options.dist_dir);
    let package_name = options
        .package_name
        .unwrap_or(options.generated_host_package_name);
    validate_oh_package_name(package_name)?;
    let stem = package_name.trim_start_matches('@').replace('/', "-");
    let outputs = HspOutputPaths {
        dist: Some(options.dist_dir.to_path_buf()),
        tgz: options
            .tgz_out
            .map(Utf8Path::to_path_buf)
            .unwrap_or_else(|| artifact_root.join(format!("{stem}.tgz"))),
        runtime_hsp: options
            .runtime_hsp_out
            .map(Utf8Path::to_path_buf)
            .unwrap_or_else(|| artifact_root.join(format!("{stem}.hsp"))),
        interface_har: options
            .interface_har_out
            .map(Utf8Path::to_path_buf)
            .unwrap_or_else(|| artifact_root.join(format!("{stem}-interface.har"))),
        package_source: artifact_root.join("package"),
        module_project: artifact_root.join("module-project"),
        usage: artifact_root.join(format!("{stem}-HSP_USAGE.md")),
    };
    let mut outputs = [outputs];
    normalize_hsp_destinations(
        &mut outputs,
        &[options.generated_host_package_name.to_string()],
    )?;
    Ok(outputs
        .into_iter()
        .next()
        .expect("one generated HSP output plan"))
}

fn validate_multi_package_output_overrides(options: &BuildOptions, plan: &HostPlan) -> Result<()> {
    if plan.packages.len() <= 1 {
        return Ok(());
    }
    if let Some(package_name) = &options.package_name {
        bail!(
            "--package-name `{package_name}` is ambiguous when multiple OHOS packages are selected; pass --package <name> to build a single package"
        );
    }
    if let Some(har_out) = &options.har_out {
        bail!(
            "--har-out `{har_out}` is ambiguous when multiple OHOS packages are selected; pass --package <name> to build a single package"
        );
    }
    for (flag, output) in [
        ("--runtime-hsp-out", options.runtime_hsp_out.as_ref()),
        ("--interface-har-out", options.interface_har_out.as_ref()),
        ("--tgz-out", options.tgz_out.as_ref()),
    ] {
        if let Some(output) = output {
            bail!(
                "{flag} `{output}` is ambiguous when multiple OHOS packages are selected; pass --package <name> to build a single package"
            );
        }
    }
    if let Some(module_name) = &options.module_name {
        bail!(
            "--module-name `{module_name}` is ambiguous when multiple OHOS packages are selected; pass --package <name> to build a single package"
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn build_package_dist_contents(
    options: &BuildOptions,
    package: &HostPackage,
    ohos_ndk: &str,
    target_dir: &Utf8Path,
    package_dist_dir: &Utf8Path,
    type_dir: &Utf8Path,
    native_path_policy: &NativePathPolicy,
    arches: &[Arch],
    explicit_package_arg: bool,
    required_core_so: Option<&RequiredCoreSo>,
) -> Result<HspSoInventory> {
    let mut expected_so_inventory = HspSoInventory::new();
    for arch in arches {
        let expected_so = build_arch(
            options,
            package,
            ohos_ndk,
            target_dir,
            package_dist_dir,
            type_dir,
            native_path_policy,
            *arch,
            explicit_package_arg,
            required_core_so,
        )
        .with_context(|| {
            format!(
                "building OHOS package {} arch {}",
                package.name,
                arch.dist_dir()
            )
        })?;
        if expected_so_inventory
            .insert(arch.dist_dir().to_string(), expected_so)
            .is_some()
        {
            bail!("duplicate requested OHOS ABI `{}`", arch.dist_dir());
        }
    }
    let generated_package_root = generated_package_root(options)?;
    emit_index_d_ts(
        package_dist_dir,
        type_dir,
        &generated_package_root,
        &package.lib_target_name,
    )?;
    Ok(expected_so_inventory)
}

fn validate_package_mode_options(options: &BuildOptions) -> Result<()> {
    let has_hsp_output = options.runtime_hsp_out.is_some()
        || options.interface_har_out.is_some()
        || options.tgz_out.is_some();
    validate_package_mode_values(
        options.package_kind,
        options.integrated_hsp,
        options.hsp_bundle_name.as_deref(),
        options.har_out.is_some(),
        has_hsp_output,
        options.no_har,
        options.skip_libs,
    )
}

fn validate_package_mode_values(
    package_kind: PackageKind,
    integrated_hsp: bool,
    hsp_bundle_name: Option<&str>,
    has_har_output: bool,
    has_hsp_output: bool,
    no_har: bool,
    skip_libs: bool,
) -> Result<()> {
    match package_kind {
        PackageKind::Har => {
            if integrated_hsp {
                bail!("--integrated-hsp requires --package-type hsp");
            }
            if hsp_bundle_name.is_some() {
                bail!("--hsp-bundle-name requires --package-type hsp");
            }
            if has_hsp_output {
                bail!(
                    "--runtime-hsp-out, --interface-har-out, and --tgz-out require --package-type hsp"
                );
            }
        }
        PackageKind::Hsp => {
            if no_har {
                bail!("--package-type hsp conflicts with --no-har");
            }
            if skip_libs {
                bail!("--package-type hsp conflicts with --skip-libs");
            }
            if has_har_output {
                bail!(
                    "--har-out is a HAR-mode output; use --interface-har-out with --package-type hsp"
                );
            }
            if integrated_hsp {
                if hsp_bundle_name.is_some() {
                    bail!(
                        "--hsp-bundle-name is only valid for a non-integrated HSP; integrated HSP bundleName is intentionally empty"
                    );
                }
            } else {
                let bundle = hsp_bundle_name.with_context(|| {
                    "non-integrated HSP requires --hsp-bundle-name <host.bundle.name>"
                })?;
                validate_hsp_bundle_name(bundle)?;
            }
        }
    }
    Ok(())
}

pub(crate) fn preflight_hsp_frontend(options: HspFrontendPreflight<'_>) -> Result<()> {
    validate_package_mode_values(
        options.package_kind,
        options.integrated_hsp,
        options.hsp_bundle_name,
        options.has_har_output,
        options.has_hsp_output,
        options.no_har,
        options.skip_libs,
    )?;
    if options.package_kind != PackageKind::Hsp {
        return Ok(());
    }

    let ohos_ndk = env::var("OHOS_NDK_HOME").context(
        "OHOS_NDK_HOME is required for HSP preflight before generated output is modified",
    )?;
    if !Path::new(&ohos_ndk).exists() {
        bail!("OHOS_NDK_HOME does not exist: {ohos_ndk}");
    }
    let version = options.compatible_sdk_version.with_context(|| {
        "HSP packaging requires an explicit --compatible-sdk-version before generated output is modified"
    })?;
    let version = validate_sdk_metadata_value(version)?;
    let sdk_type = if let Some(explicit) = options.compatible_sdk_type {
        validate_sdk_metadata_value(explicit)?;
        RuntimeSdkType::parse(explicit)?
    } else {
        discover_sdk_type(&ohos_ndk, options.bisheng)?.with_context(|| {
            "compatible SDK type could not be identified before HSP generation; pass --compatible-sdk-type HarmonyOS or OpenHarmony"
        })?
    };
    let sdk = SdkCompatibility {
        version: version.to_string(),
        sdk_type,
    };
    let api = compatible_sdk_api_level(&sdk)?;
    if api < MIN_HSP_API {
        bail!(
            "HSP compatible SDK API must be at least {MIN_HSP_API}; `{version}` resolves to API {api}"
        );
    }
    let tools = resolve_harmony_tools(options.hvigorw, options.ohpm, options.deveco_sdk_home)?;
    if tools.compile_sdk.api_level < MIN_HSP_API {
        bail!(
            "HSP build requires a DevEco compile SDK at API {MIN_HSP_API} or newer; found API {}",
            tools.compile_sdk.api_level
        );
    }
    resolve_target_sdk_version(&tools.compile_sdk, &sdk, options.target_sdk_version)?;
    preflight_harmony_tools(&tools)
}

fn validate_hsp_bundle_name(value: &str) -> Result<()> {
    if value.len() > 255
        || value.split('.').count() < 2
        || value.split('.').any(|part| {
            part.is_empty()
                || !part
                    .chars()
                    .next()
                    .is_some_and(|character| character.is_ascii_alphabetic())
                || !part
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '_')
        })
    {
        bail!(
            "invalid HSP bundleName `{value}`; use dot-separated identifiers beginning with a letter (for example `com.example.app`)"
        );
    }
    Ok(())
}

fn protected_dist_paths(
    options: &BuildOptions,
    manifest_path: &Utf8Path,
    plan: &HostPlan,
    target_dir: &Utf8Path,
) -> Result<Vec<ProtectedDistPath>> {
    let cwd = Utf8PathBuf::from_path_buf(env::current_dir()?)
        .map_err(|path| anyhow::anyhow!("current directory is not utf8: {}", path.display()))?;
    let mut paths = vec![
        ProtectedDistPath {
            label: "current working directory".into(),
            path: cwd,
        },
        ProtectedDistPath {
            label: "input manifest".into(),
            path: manifest_path.to_path_buf(),
        },
        ProtectedDistPath {
            label: "Cargo workspace".into(),
            path: plan.workspace_root.clone(),
        },
        ProtectedDistPath {
            label: "Cargo target directory".into(),
            path: target_dir.to_path_buf(),
        },
    ];
    if let Some(core_manifest) = &options.core_manifest_path {
        paths.push(ProtectedDistPath {
            label: "downstream core manifest".into(),
            path: core_manifest.clone(),
        });
        if let Some(core_root) = core_manifest.parent() {
            paths.push(ProtectedDistPath {
                label: "downstream core package".into(),
                path: core_root.to_path_buf(),
            });
            paths.push(ProtectedDistPath {
                label: "downstream core source directory".into(),
                path: core_root.join("src"),
            });
        }
    }
    for (name, source_root) in &options.additional_source_roots {
        paths.push(ProtectedDistPath {
            label: format!("additional generated/source root `{name}`"),
            path: source_root.clone(),
        });
    }
    for package in &plan.packages {
        let package_root = package.manifest_path.parent().with_context(|| {
            format!(
                "OHOS package manifest has no parent: {}",
                package.manifest_path
            )
        })?;
        paths.push(ProtectedDistPath {
            label: format!("Cargo package `{}`", package.name),
            path: package_root.to_path_buf(),
        });
        paths.push(ProtectedDistPath {
            label: format!("Cargo package `{}` source directory", package.name),
            path: package_root.join("src"),
        });
    }
    for (name, source_root) in &plan.local_source_roots {
        paths.push(ProtectedDistPath {
            label: format!("local Cargo source `{name}`"),
            path: source_root.clone(),
        });
    }
    Ok(paths)
}

fn preflight_dist_output(
    requested: &Utf8Path,
    protected_paths: &[ProtectedDistPath],
) -> Result<Utf8PathBuf> {
    let resolved = preflight_dist_path_safety(requested, protected_paths)?;
    match std::fs::symlink_metadata(&resolved) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_dir() {
                bail!(
                    "OHOS dist output must be a real directory, not a symlink or file: {requested}"
                );
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("reading OHOS dist output {requested}"));
        }
    }
    Ok(resolved)
}

fn preflight_dist_path_safety(
    requested: &Utf8Path,
    protected_paths: &[ProtectedDistPath],
) -> Result<Utf8PathBuf> {
    let absolute = absolute_output_path(requested)?;
    if std::fs::symlink_metadata(&absolute).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("OHOS dist output must not be a symlink: {requested}");
    }
    let resolved = canonicalize_allow_missing(&absolute)?;
    if resolved.parent().is_none() || resolved.as_str() == "/" {
        bail!("refusing unsafe OHOS dist output at filesystem root: {requested}");
    }

    for protected in protected_paths {
        let protected_path = canonicalize_allow_missing(&absolute_output_path(&protected.path)?)?;
        if protected_path.starts_with(&resolved) {
            bail!(
                "refusing OHOS dist output `{requested}` because it is {} or an ancestor of protected {} `{protected_path}`",
                if protected_path == resolved { "the same path as" } else { "an ancestor of" },
                protected.label,
            );
        }
    }

    Ok(resolved)
}

pub(crate) fn preflight_dist_output_for_generation(
    requested: &Utf8Path,
    protected_paths: &[(String, Utf8PathBuf)],
) -> Result<()> {
    let protected_paths = protected_paths
        .iter()
        .map(|(label, path)| ProtectedDistPath {
            label: label.clone(),
            path: path.clone(),
        })
        .collect::<Vec<_>>();
    preflight_dist_path_safety(requested, &protected_paths).map(|_| ())
}

fn build_package_dist_from_stage<Build>(final_path: &Utf8Path, build: Build) -> Result<()>
where
    Build: FnOnce(&Utf8Path) -> Result<()>,
{
    let final_path = absolute_output_path(final_path)?;
    let final_path = preflight_dist_output(&final_path, &[])?;
    let invocation_dist = InvocationDist::new(final_path)?;
    build(&invocation_dist.path)?;
    invocation_dist.publish_simple()
}

impl NativePathPolicy {
    fn discover(
        options: &BuildOptions,
        plan: &HostPlan,
        host_manifest: &Utf8Path,
        target_dir: &Utf8Path,
        ohos_ndk: &str,
    ) -> Result<Self> {
        let mut remaps = Vec::<PathRemap>::new();
        let mut seen = BTreeSet::<Utf8PathBuf>::new();
        let mut add = |path: &Utf8Path, destination: String| -> Result<()> {
            let lexical = absolute_output_path(path)?;
            let canonical = canonicalize_allow_missing(&lexical)?;
            for source in [lexical, canonical] {
                if source.as_str() == "/" || !seen.insert(source.clone()) {
                    continue;
                }
                if source.as_str().contains('\x1f') {
                    bail!("cannot encode Rust path remap for a path containing unit separator: {source}");
                }
                remaps.push(PathRemap {
                    source,
                    destination: destination.clone(),
                });
            }
            Ok(())
        };

        if let Some(core_manifest) = &options.core_manifest_path {
            if let Some(root) = core_manifest.parent() {
                add(root, "/uniffi/source/core".into())?;
            }
        }
        if let Some(root) = host_manifest.parent() {
            add(root, "/uniffi/source/host".into())?;
        }
        for (name, root) in &options.additional_source_roots {
            add(
                root,
                format!("/uniffi/source/extra/{}", stable_virtual_segment(name)),
            )?;
        }
        let dependency_metadata = MetadataCommand::new()
            .cargo_path(&options.cargo_bin)
            .manifest_path(host_manifest.as_std_path())
            .exec()
            .with_context(|| {
                format!("resolving local OHOS Cargo sources for reproducible path remapping: {host_manifest}")
            })?;
        let mut local_source_roots = dependency_metadata
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
        local_source_roots.extend(plan.local_source_roots.iter().cloned());
        local_source_roots.sort();
        local_source_roots.dedup();
        for (index, (name, root)) in local_source_roots.iter().enumerate() {
            add(
                root,
                format!(
                    "/uniffi/source/local/{index}-{}",
                    stable_virtual_segment(name)
                ),
            )?;
        }
        add(&plan.workspace_root, "/uniffi/source/workspace".into())?;
        add(target_dir, "/uniffi/build/target".into())?;

        let temp_root = Utf8PathBuf::from_path_buf(env::temp_dir()).map_err(|path| {
            anyhow::anyhow!("temporary directory is not utf8: {}", path.display())
        })?;
        add(&temp_root, "/uniffi/build/temp".into())?;
        add(Utf8Path::new(ohos_ndk), "/uniffi/sdk/ohos".into())?;
        if let Some(hos_ndk) = resolve_hos_ndk(ohos_ndk) {
            if let Ok(hos_ndk) = Utf8PathBuf::from_path_buf(hos_ndk) {
                add(&hos_ndk, "/uniffi/sdk/harmony".into())?;
            }
        }
        if let Some(deveco_sdk) = options
            .deveco_sdk_home
            .clone()
            .or_else(|| env::var("DEVECO_SDK_HOME").ok().map(Utf8PathBuf::from))
        {
            add(&deveco_sdk, "/uniffi/sdk/deveco".into())?;
        }

        let home = env::var("HOME").ok().map(Utf8PathBuf::from);
        if let Some(cargo_home) = env::var("CARGO_HOME")
            .ok()
            .map(Utf8PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".cargo")))
        {
            add(&cargo_home, "/uniffi/toolchain/cargo".into())?;
        }
        if let Some(rustup_home) = env::var("RUSTUP_HOME")
            .ok()
            .map(Utf8PathBuf::from)
            .or_else(|| home.as_ref().map(|home| home.join(".rustup")))
        {
            add(&rustup_home, "/uniffi/toolchain/rustup".into())?;
        }
        if let Some(sysroot) = rustc_sysroot()? {
            add(&sysroot, "/uniffi/toolchain/sysroot".into())?;
        }
        if let Some(home) = &home {
            add(home, "/uniffi/home".into())?;
        }

        // Keep the policy model in the prompt-requested most-specific to
        // broadest order.  merge_encoded_rustflags() reverses this when
        // encoding because rustc intentionally applies the last matching
        // --remap-path-prefix rule.
        remaps.sort_by(|left, right| {
            right
                .source
                .as_str()
                .len()
                .cmp(&left.source.as_str().len())
                .then_with(|| left.source.cmp(&right.source))
        });
        Ok(Self { remaps })
    }

    fn for_invocation(&self, paths: &[(&Utf8Path, &str)]) -> Result<Self> {
        let mut policy = self.clone();
        for (path, destination) in paths {
            let lexical = absolute_output_path(path)?;
            let canonical = canonicalize_allow_missing(&lexical)?;
            for source in [lexical, canonical] {
                if source.as_str() == "/"
                    || policy.remaps.iter().any(|remap| remap.source == source)
                {
                    continue;
                }
                if source.as_str().contains('\x1f') {
                    bail!("cannot encode Rust path remap for a path containing unit separator: {source}");
                }
                policy.remaps.push(PathRemap {
                    source: source.clone(),
                    destination: (*destination).to_string(),
                });
            }
        }
        policy.remaps.sort_by(|left, right| {
            right
                .source
                .as_str()
                .len()
                .cmp(&left.source.as_str().len())
                .then_with(|| left.source.cmp(&right.source))
        });
        Ok(policy)
    }
}

fn stable_virtual_segment(value: &str) -> String {
    let segment = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if segment.is_empty() {
        "package".into()
    } else {
        segment
    }
}

fn rustc_sysroot() -> Result<Option<Utf8PathBuf>> {
    let rustc = env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let output = match Command::new(&rustc).args(["--print", "sysroot"]).output() {
        Ok(output) => output,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("running `{rustc} --print sysroot`"))
        }
    };
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).context("rustc sysroot path is not UTF-8")?;
    let value = value.trim();
    if value.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Utf8PathBuf::from(value)))
    }
}

fn parse_arches(arches: &[String]) -> Result<Vec<Arch>> {
    if arches.is_empty() {
        Ok(vec![Arch::Arm64, Arch::X86_64])
    } else {
        arches.iter().map(|arch| Arch::parse(arch)).collect()
    }
}

/// Validate the architecture spellings before a multi-participant HSP
/// coordinator creates durable publication anchors.  The deferred builder
/// validates them again when it consumes the complete options, but that is
/// intentionally too late for a zero-side-effect argument error.
pub(crate) fn preflight_hsp_arches(arches: &[String]) -> Result<()> {
    parse_arches(arches).map(|_| ())
}

fn host_plan(
    cargo_bin: &str,
    manifest_path: &Utf8Path,
    options: &BuildOptions,
) -> Result<HostPlan> {
    let metadata = MetadataCommand::new()
        .cargo_path(cargo_bin)
        .manifest_path(manifest_path.as_std_path())
        .no_deps()
        .exec()
        .with_context(|| format!("running cargo metadata for OHOS host crate {manifest_path}"))?;

    let explicit_package = resolve_package_filter(options.package.as_deref(), &options.cargo_args)?;
    let explicit_package_arg = explicit_package.is_some();
    let mut eligible_packages = Vec::new();
    let workspace_members = metadata
        .workspace_members
        .iter()
        .filter_map(|member_id| metadata.packages.iter().find(|p| &p.id == member_id))
        .collect::<Vec<_>>();
    let candidates = if workspace_members.is_empty() {
        metadata.packages.iter().collect::<Vec<_>>()
    } else {
        workspace_members
    };

    for package in candidates {
        if !options.skip_napi_check && !has_napi_ohos_dependency(package) {
            continue;
        }
        if !has_ohos_library_target(package) {
            continue;
        }
        eligible_packages.push(host_package(package)?);
    }
    let package_count = eligible_packages.len();
    let packages = eligible_packages
        .into_iter()
        .filter(|package| {
            explicit_package.as_ref().map_or(true, |filter| {
                let package_spec = format!("{}@{}", package.name, package.version);
                &package.name == filter || &package_spec == filter
            })
        })
        .collect::<Vec<_>>();

    if packages.is_empty() {
        if let Some(filter) = explicit_package {
            bail!("no OHOS-capable package matched `{filter}`");
        }
        bail!(
            "no OHOS-capable package found; add napi-derive-ohos or pass --skip-napi-check if this is intentional"
        );
    }

    if !options.skip_check {
        check_napi_versions(cargo_bin, manifest_path)?;
    }

    let workspace_root =
        Utf8PathBuf::from_path_buf(metadata.workspace_root.clone().into_std_path_buf())
            .map_err(|p| anyhow::anyhow!("Cargo workspace root is not utf8: {}", p.display()))?;
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

    Ok(HostPlan {
        target_directory: Utf8PathBuf::from_path_buf(
            metadata.target_directory.clone().into_std_path_buf(),
        )
        .map_err(|p| anyhow::anyhow!("cargo metadata target dir is not utf8: {}", p.display()))?,
        workspace_root,
        local_source_roots,
        packages,
        package_count,
        explicit_package_arg,
    })
}

fn has_napi_ohos_dependency(package: &Package) -> bool {
    package
        .dependencies
        .iter()
        .any(|dep| dep.name == "napi-derive-ohos")
}

fn has_ohos_library_target(package: &Package) -> bool {
    package.targets.iter().any(|target| {
        target
            .kind
            .iter()
            .any(|kind| matches!(kind.to_string().as_str(), "cdylib" | "lib"))
    })
}

fn host_package(package: &Package) -> Result<HostPackage> {
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
        .with_context(|| format!("OHOS package {} has no lib target", package.name))?;
    let manifest_path =
        Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf())
            .map_err(|p| anyhow::anyhow!("package manifest path is not utf8: {}", p.display()))?;
    Ok(HostPackage {
        cargo_package_id: package.id.repr.clone(),
        name: package.name.to_string(),
        version: package.version.to_string(),
        description: package.description.clone(),
        authors: package.authors.clone(),
        license: package.license.clone(),
        manifest_path,
        lib_target_name: lib_target.name.clone(),
    })
}

fn check_napi_versions(cargo_bin: &str, manifest_path: &Utf8Path) -> Result<()> {
    let metadata = MetadataCommand::new()
        .cargo_path(cargo_bin)
        .manifest_path(manifest_path.as_std_path())
        .exec()
        .with_context(|| {
            format!("running full cargo metadata for OHOS version checks: {manifest_path}")
        })?;
    for name in ["napi-ohos", "napi-derive-ohos"] {
        let package = metadata
            .packages
            .iter()
            .find(|package| package.name.as_str() == name)
            .with_context(|| {
                format!("failed to find {name}; pass --skip-check to bypass this check")
            })?;
        let min = cargo_metadata::semver::Version::parse("1.1.0")?;
        if package.version < min {
            bail!(
                "{name} must be >= 1.1.0 for OHOS builds; found {}. Pass --skip-check to bypass this check",
                package.version
            );
        }
    }
    Ok(())
}

fn package_arg_from_cargo_args(args: &[String]) -> Option<String> {
    args.iter()
        .position(|arg| arg == "-p" || arg == "--package")
        .and_then(|idx| args.get(idx + 1).cloned())
        .or_else(|| {
            args.iter()
                .find_map(|arg| {
                    arg.strip_prefix("-p=")
                        .or_else(|| arg.strip_prefix("--package="))
                })
                .map(str::to_string)
        })
}

fn resolve_package_filter(
    option_package: Option<&str>,
    cargo_args: &[String],
) -> Result<Option<String>> {
    let cargo_package = package_arg_from_cargo_args(cargo_args);
    match (option_package, cargo_package) {
        (Some(option_package), Some(cargo_package)) if option_package != cargo_package => {
            bail!(
                "conflicting OHOS package filters: --package {option_package} and trailing cargo args package {cargo_package}"
            )
        }
        (Some(option_package), _) => Ok(Some(option_package.to_string())),
        (_, Some(cargo_package)) => Ok(Some(cargo_package)),
        _ => Ok(None),
    }
}

fn has_package_arg(args: &[String]) -> bool {
    package_arg_from_cargo_args(args).is_some()
}

fn package_dist_dir(root: &Utf8Path, package: &HostPackage, package_count: usize) -> Utf8PathBuf {
    if package_count <= 1 {
        root.to_path_buf()
    } else {
        root.join(&package.name)
    }
}

fn package_output(
    options: &BuildOptions,
    package: &HostPackage,
    metadata: &OhosPackageMetadata,
    package_dist_dir: &Utf8Path,
    package_count: usize,
) -> Result<()> {
    match options.package_kind {
        PackageKind::Har => {
            package_har(options, package, metadata, package_dist_dir, package_count)
        }
        PackageKind::Hsp => {
            bail!("internal error: HSP packaging requires the independent Cargo SO inventory")
        }
    }
}

fn package_har(
    options: &BuildOptions,
    package: &HostPackage,
    metadata: &OhosPackageMetadata,
    package_dist_dir: &Utf8Path,
    package_count: usize,
) -> Result<()> {
    let artifact_root = options
        .dist_dir
        .parent()
        .unwrap_or(options.dist_dir.as_path())
        .to_path_buf();
    let package_dir = package_stage_dir(&artifact_root, package, package_count);
    let requested_har_out = resolve_har_out(
        options.har_out.as_deref(),
        &artifact_root,
        package,
        package_count,
    );
    package_har_with(
        &package_dir,
        &requested_har_out,
        Some(&options.dist_dir),
        || {
            stage_har_package(
                package_dist_dir,
                &package_dir,
                &package.lib_target_name,
                metadata,
                options.skip_libs,
            )
        },
        |har_out| build_hvigor_har(options, metadata, &package_dir, har_out),
    )
}

fn package_har_with<Stage, Build>(
    package_dir: &Utf8Path,
    requested_har_out: &Utf8Path,
    dist_dir: Option<&Utf8Path>,
    stage: Stage,
    build: Build,
) -> Result<()>
where
    Stage: FnOnce() -> Result<()>,
    Build: FnOnce(&Utf8Path) -> Result<()>,
{
    // This preflight is deliberately the first mutating boundary.  It resolves
    // existing symlinks and prospective missing paths before stage() can
    // delete or rebuild package_dir, so an unsafe output never destroys a
    // previous staging tree or HAR.
    if let Some(dist_dir) = dist_dir {
        let dist_dir = canonicalize_allow_missing(&absolute_output_path(dist_dir)?)?;
        let requested_har_out =
            canonicalize_allow_missing(&absolute_output_path(requested_har_out)?)?;
        if requested_har_out.starts_with(&dist_dir) {
            bail!(
                "HAR output path must not be inside the build-owned OHOS dist directory: {requested_har_out}"
            );
        }
    }
    let har_out = prepare_har_output_path(requested_har_out, Some(package_dir))?;
    stage()?;
    build(&har_out)
}

#[derive(Debug)]
struct HspArchiveMembers {
    tgz: Vec<u8>,
    runtime_name: String,
    runtime_hsp: Vec<u8>,
    interface_name: String,
    interface_har: Vec<u8>,
}

fn stage_hsp_outputs(
    options: &BuildOptions,
    package: &HostPackage,
    ohos_ndk: &str,
    metadata: &OhosPackageMetadata,
    package_dist_dir: &Utf8Path,
    outputs: &HspOutputPaths,
    dist_publication: Option<(&Utf8Path, &Utf8Path)>,
    expected_so_inventory: &HspSoInventory,
    scratch_parent: &Utf8Path,
) -> Result<StagedHspOutputs> {
    let outputs = outputs.clone();
    if let Some((_, final_dist)) = dist_publication {
        if outputs.dist.as_deref() != Some(final_dist) {
            bail!("HSP dist publication does not match the immutable invocation output plan");
        }
    }

    std::fs::create_dir_all(scratch_parent)
        .with_context(|| format!("creating HSP staging parent {scratch_parent}"))?;
    let generation = tempfile::Builder::new()
        .prefix(".uniffi-hsp-generation-")
        .tempdir_in(scratch_parent)
        .with_context(|| format!("creating HSP staging directory in {scratch_parent}"))?;
    let generation_root = Utf8PathBuf::from_path_buf(generation.path().to_path_buf())
        .map_err(|path| anyhow::anyhow!("HSP staging path is not UTF-8: {}", path.display()))?;
    let staged_package = generation_root.join("package");
    stage_package(
        package_dist_dir,
        &staged_package,
        &package.lib_target_name,
        metadata,
        PackageKind::Hsp,
        options.integrated_hsp,
        false,
        Some(expected_so_inventory),
    )?;
    let strip = ohos_llvm_tool_path(ohos_ndk, "llvm-strip");
    let expected_runtime_so_inventory = normalize_staged_hsp_so_inventory(
        &staged_package.join("libs"),
        expected_so_inventory,
        &strip,
    )?;

    let sdk = metadata.sdk.as_ref().with_context(|| {
        "HSP packaging requires --compatible-sdk-version and a resolved compatible SDK type"
    })?;
    let mut tools = resolve_harmony_har_tools(options)?;
    let staged_project = generation_root.join("module-project");
    let staged_module = staged_project.join("library");
    copy_dir_recursive(&staged_package, &staged_module)?;
    write_hvigor_hsp_project(
        &staged_project,
        &staged_module,
        metadata,
        sdk,
        &tools,
        options.target_sdk_version.as_deref(),
        options.integrated_hsp,
        options.hsp_bundle_name.as_deref(),
    )?;
    let members = build_hvigor_hsp_from_project(
        options,
        package,
        metadata,
        &staged_project,
        sdk,
        &mut tools,
        expected_so_inventory,
        &expected_runtime_so_inventory,
    )?;

    let staged_tgz = generation_root.join("release.tgz");
    let staged_runtime = generation_root.join("runtime.hsp");
    let staged_interface = generation_root.join("interface.har");
    write_durable_file(&staged_tgz, &members.tgz)?;
    write_durable_file(&staged_runtime, &members.runtime_hsp)?;
    write_durable_file(&staged_interface, &members.interface_har)?;
    ensure_member_file_matches(&staged_runtime, &members.runtime_hsp, &members.runtime_name)?;
    ensure_member_file_matches(
        &staged_interface,
        &members.interface_har,
        &members.interface_name,
    )?;
    let staged_usage = generation_root.join("HSP_USAGE.md");
    write_durable_file(
        &staged_usage,
        render_hsp_usage(metadata, options.integrated_hsp).as_bytes(),
    )?;

    let mut staged = vec![
        (staged_tgz, outputs.tgz.clone(), false),
        (staged_runtime, outputs.runtime_hsp.clone(), false),
        (staged_interface, outputs.interface_har.clone(), false),
        (staged_package, outputs.package_source.clone(), true),
        (staged_project, outputs.module_project.clone(), true),
        (staged_usage, outputs.usage.clone(), false),
    ];
    if let Some((staged_dist, final_dist)) = dist_publication {
        staged.push((staged_dist.to_path_buf(), final_dist.to_path_buf(), true));
    }
    Ok(StagedHspOutputs {
        _staging: generation,
        outputs,
        staged,
    })
}

fn hsp_archive_stem(
    package: &HostPackage,
    metadata: &OhosPackageMetadata,
    package_count: usize,
) -> String {
    let mut stem = metadata.name.trim_start_matches('@').replace('/', "-");
    if package_count > 1 {
        stem.push('-');
        stem.push_str(&package.lib_target_name);
    }
    stem
}

fn resolve_hsp_output_paths(
    options: &BuildOptions,
    artifact_root: &Utf8Path,
    package: &HostPackage,
    metadata: &OhosPackageMetadata,
    package_count: usize,
) -> Result<HspOutputPaths> {
    let stem = hsp_archive_stem(package, metadata, package_count);
    let package_source = package_stage_dir(artifact_root, package, package_count);
    let module_project = if package_count <= 1 {
        artifact_root.join("module-project")
    } else {
        artifact_root.join("module-project").join(&package.name)
    };
    Ok(HspOutputPaths {
        dist: None,
        tgz: options
            .tgz_out
            .clone()
            .unwrap_or_else(|| artifact_root.join(format!("{stem}.tgz"))),
        runtime_hsp: options
            .runtime_hsp_out
            .clone()
            .unwrap_or_else(|| artifact_root.join(format!("{stem}.hsp"))),
        interface_har: options
            .interface_har_out
            .clone()
            .unwrap_or_else(|| artifact_root.join(format!("{stem}-interface.har"))),
        package_source,
        module_project,
        usage: artifact_root.join(format!("{stem}-HSP_USAGE.md")),
    })
}

fn render_hsp_usage(metadata: &OhosPackageMetadata, integrated: bool) -> String {
    let integration = if integrated {
        "This is an integrated HSP. The consumer must enable `buildOption.strictMode.useNormalizedOHMUrl: true`; Hvigor binds the consumer bundleName and signing identity during the consumer build."
    } else {
        "This is an application-bound HSP. The consumer bundleName and signing identity must match the bundle used to build this package."
    };
    format!(
        "# {} Harmony HSP usage\n\nDepend on the generated `.tgz` directly from the consuming HAP/HSP module, using the exact package name as the dependency key:\n\n```json5\n{{\n  \"dependencies\": {{\n    \"{}\": \"file:./libs/<generated>.tgz\"\n  }}\n}}\n```\n\n{integration}\n\nHSP dependencies are not transitive and circular HSP dependencies are forbidden. A HAR that depends on an HSP becomes application-internal and must not be published to a second- or third-party repository. Do not use the extracted runtime `.hsp` or Interface `.har` as the app dependency; they are verification/debug artifacts for the same tgz generation.\n",
        metadata.name, metadata.name
    )
}

#[derive(Debug)]
struct HarmonyHarTools {
    hvigorw: String,
    ohpm: String,
    sdk_home: Utf8PathBuf,
    node_home: Option<Utf8PathBuf>,
    ohos_base_sdk_home: Option<Utf8PathBuf>,
    model_version: String,
    compile_sdk: CompileSdk,
}

fn build_hvigor_har(
    options: &BuildOptions,
    metadata: &OhosPackageMetadata,
    package_dir: &Utf8Path,
    har_out: &Utf8Path,
) -> Result<()> {
    build_hvigor_har_with(
        options,
        metadata,
        package_dir,
        har_out,
        |tools, tool, args, cwd| run_harmony_tool(tools, tool, args, cwd),
    )
}

fn build_hvigor_har_with<Run>(
    options: &BuildOptions,
    metadata: &OhosPackageMetadata,
    package_dir: &Utf8Path,
    har_out: &Utf8Path,
    mut run: Run,
) -> Result<()>
where
    Run: FnMut(&HarmonyHarTools, &str, &[&str], &Utf8Path) -> Result<()>,
{
    let sdk = metadata.sdk.as_ref().with_context(|| {
        "final Harmony packaging requires an explicit --compatible-sdk-version; the compile SDK API is not a minimum runtime compatibility value"
    })?;
    let mut tools = resolve_harmony_har_tools(options)?;
    resolve_target_sdk_version(
        &tools.compile_sdk,
        sdk,
        options.target_sdk_version.as_deref(),
    )?;
    let project = TemporaryWorkspace::create("uniffi-ohos-har-invocation")
        .context("creating temporary Hvigor HAR project")?;
    let project_root = project.mirror_root().to_path_buf();
    (|| -> Result<()> {
        // Some Hvigor versions create a .hvigor directory even for --version.
        // Run discovery inside the invocation-owned project so tool probing
        // never pollutes the caller's working directory.
        run(&tools, &tools.ohpm, &["--version"], &project_root).with_context(|| {
            format!(
                "required Harmony build tool `{}` is unavailable",
                tools.ohpm
            )
        })?;
        run(&tools, &tools.hvigorw, &["--version"], &project_root).with_context(|| {
            format!(
                "required Harmony build tool `{}` is unavailable",
                tools.hvigorw
            )
        })?;
        if sdk.sdk_type == RuntimeSdkType::OpenHarmony {
            tools.ohos_base_sdk_home = Some(match tools.ohos_base_sdk_home.take() {
                Some(path) => path
                    .canonicalize_utf8()
                    .with_context(|| format!("canonicalizing OpenHarmony base SDK root {path}"))?,
                None => create_openharmony_sdk_mirror(
                    &project_root,
                    &tools.sdk_home,
                    tools.compile_sdk.api_level,
                )?,
            });
        } else {
            tools.ohos_base_sdk_home = None;
        }
        let module_dir = project_root.join("library");
        copy_dir_recursive(package_dir, &module_dir)?;
        write_hvigor_har_project(
            &project_root,
            &module_dir,
            metadata,
            sdk,
            &tools,
            options.target_sdk_version.as_deref(),
        )?;

        run(
            &tools,
            &tools.ohpm,
            &["install", "--all", "--lockfile_stable_order"],
            &project_root,
        )?;
        run(
            &tools,
            &tools.ohpm,
            &["install", "--all", "--lockfile_stable_order"],
            &module_dir,
        )?;
        let module_target = format!("{}@default", metadata.module_name);
        run(
            &tools,
            &tools.hvigorw,
            &[
                "assembleHar",
                "--mode",
                "module",
                "-p",
                &format!("module={module_target}"),
                "-p",
                "product=default",
                "-p",
                "buildMode=release",
                "--no-daemon",
                "--no-incremental",
            ],
            &project_root,
        )?;
        let compiled_har = discover_compiled_har(&module_dir)?;
        validate_compiled_har(&compiled_har, metadata)?;
        publish_compiled_har_with(
            &compiled_har,
            &package_dir.join("oh-package.json5"),
            har_out,
            package_dir,
            |candidate| {
                validate_final_native_components(candidate)?;
                run(
                    &tools,
                    &tools.ohpm,
                    &["prepublish", candidate.as_str()],
                    &project_root,
                )
            },
        )?;
        Ok(())
    })()
}

fn preflight_hsp_environment<'a>(
    options: &BuildOptions,
    metadata: impl IntoIterator<Item = &'a OhosPackageMetadata>,
) -> Result<()> {
    let metadata = metadata.into_iter().collect::<Vec<_>>();
    for value in &metadata {
        let sdk = value.sdk.as_ref().with_context(|| {
            "HSP packaging requires an explicit --compatible-sdk-version and a resolved HarmonyOS/OpenHarmony SDK type"
        })?;
        let api = compatible_sdk_api_level(sdk)?;
        if api < MIN_HSP_API {
            bail!(
                "HSP compatible SDK API must be at least {MIN_HSP_API}; `{}` resolves to API {api}",
                sdk.version
            );
        }
    }
    if options.frontend_hsp_preflight_done {
        return Ok(());
    }
    let tools = resolve_harmony_har_tools(options)?;
    if tools.compile_sdk.api_level < MIN_HSP_API {
        bail!(
            "HSP build requires a DevEco compile SDK at API {MIN_HSP_API} or newer; found API {}",
            tools.compile_sdk.api_level
        );
    }
    for value in metadata {
        resolve_target_sdk_version(
            &tools.compile_sdk,
            value
                .sdk
                .as_ref()
                .expect("HSP compatible SDK was validated above"),
            options.target_sdk_version.as_deref(),
        )?;
    }
    preflight_harmony_tools(&tools)
}

fn preflight_harmony_tools(tools: &HarmonyHarTools) -> Result<()> {
    let workspace = TemporaryWorkspace::create("uniffi-ohos-hsp-preflight")?;
    let probe = workspace.mirror_root();
    run_harmony_tool(tools, &tools.ohpm, &["--version"], probe).with_context(|| {
        format!(
            "required Harmony build tool `{}` is unavailable",
            tools.ohpm
        )
    })?;
    run_harmony_tool(tools, &tools.hvigorw, &["--version"], probe).with_context(|| {
        format!(
            "required Harmony build tool `{}` is unavailable",
            tools.hvigorw
        )
    })
}

fn compatible_sdk_api_level(sdk: &SdkCompatibility) -> Result<u32> {
    sdk_version_api_level(&sdk.version, "compatible")
}

fn sdk_version_api_level(value: &str, label: &str) -> Result<u32> {
    let value = value.trim();
    if let Some(open) = value.rfind('(') {
        let suffix = value
            .strip_suffix(')')
            .with_context(|| format!("{label} SDK version `{value}` has an unmatched `(`"))?;
        return suffix[open + 1..].parse::<u32>().with_context(|| {
            format!("{label} SDK version `{value}` has a non-numeric API suffix")
        });
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value
            .parse::<u32>()
            .with_context(|| format!("{label} SDK API `{value}` is out of range"));
    }
    let major = value
        .split('.')
        .next()
        .with_context(|| format!("{label} SDK version is empty"))?
        .parse::<u32>()
        .with_context(|| {
            format!(
                "{label} SDK version `{value}` does not expose a numeric API level; use the official `platform(api)` spelling before API 26"
            )
        })?;
    if major < 26 {
        bail!(
            "{label} SDK version `{value}` does not expose a numeric API level; use the official `platform(api)` spelling before API 26"
        );
    }
    Ok(major)
}

fn resolve_harmony_har_tools(options: &BuildOptions) -> Result<HarmonyHarTools> {
    resolve_harmony_tools(
        options.hvigorw.as_deref(),
        options.ohpm.as_deref(),
        options.deveco_sdk_home.as_deref(),
    )
}

fn resolve_harmony_tools(
    hvigorw_override: Option<&str>,
    ohpm_override: Option<&str>,
    sdk_home_override: Option<&Utf8Path>,
) -> Result<HarmonyHarTools> {
    let sdk_home = sdk_home_override
        .map(Utf8Path::to_path_buf)
        .or_else(|| env::var("DEVECO_SDK_HOME").ok().map(Utf8PathBuf::from))
        .context(
            "DEVECO SDK root is required for final Harmony packaging; pass --deveco-sdk-home or set DEVECO_SDK_HOME",
        )?;
    let sdk_home = sdk_home
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing DevEco SDK root {sdk_home}"))?;
    let sdk_pkg = [
        sdk_home.join("default/sdk-pkg.json"),
        sdk_home.join("sdk-pkg.json"),
    ]
    .into_iter()
    .find(|path| path.is_file())
    .with_context(|| format!("DevEco SDK metadata sdk-pkg.json not found under {sdk_home}"))?;
    let sdk_json: Value = serde_json::from_str(
        &std::fs::read_to_string(&sdk_pkg)
            .with_context(|| format!("reading DevEco SDK metadata {sdk_pkg}"))?,
    )
    .with_context(|| format!("parsing DevEco SDK metadata {sdk_pkg}"))?;
    let data = sdk_json.get("data").unwrap_or(&sdk_json);
    let model_version = data
        .get("platformVersion")
        .and_then(Value::as_str)
        .map(str::to_string)
        .with_context(|| format!("{sdk_pkg} missing data.platformVersion"))?;
    let api_version = data
        .get("apiVersion")
        .and_then(Value::as_str)
        .with_context(|| format!("{sdk_pkg} missing data.apiVersion"))?;
    validate_sdk_metadata_value(&model_version)?;
    validate_sdk_metadata_value(api_version)?;
    let api_level = api_version.parse::<u32>().with_context(|| {
        format!("{sdk_pkg} data.apiVersion `{api_version}` is not a numeric API level")
    })?;

    let tools_root = sdk_home.parent().map(|parent| parent.join("tools"));
    let derived_hvigor = tools_root
        .as_ref()
        .map(|root| root.join("hvigor/bin/hvigorw"))
        .filter(|path| path.is_file());
    let derived_ohpm = tools_root
        .as_ref()
        .map(|root| root.join("ohpm/bin/ohpm"))
        .filter(|path| path.is_file());
    let node_home = tools_root
        .as_ref()
        .map(|root| root.join("node"))
        .filter(|path| path.is_dir());
    let ohos_base_sdk_home = env::var("OHOS_BASE_SDK_HOME").ok().map(Utf8PathBuf::from);
    let hvigorw = hvigorw_override
        .map(str::to_string)
        .or_else(|| env::var("HVIGORW").ok())
        .or_else(|| derived_hvigor.map(|path| path.to_string()))
        .unwrap_or_else(|| "hvigorw".to_string());
    let ohpm = ohpm_override
        .map(str::to_string)
        .or_else(|| env::var("OHPM").ok())
        .or_else(|| derived_ohpm.map(|path| path.to_string()))
        .unwrap_or_else(|| "ohpm".to_string());
    Ok(HarmonyHarTools {
        hvigorw,
        ohpm,
        sdk_home,
        node_home,
        ohos_base_sdk_home,
        compile_sdk: CompileSdk {
            api_level,
            platform_version: model_version.clone(),
        },
        model_version,
    })
}

fn create_openharmony_sdk_mirror(
    project_root: &Utf8Path,
    sdk_home: &Utf8Path,
    api_level: u32,
) -> Result<Utf8PathBuf> {
    let components = sdk_home.join("default/openharmony");
    let components = components.canonicalize_utf8().with_context(|| {
        format!(
            "OpenHarmony SDK components were not found under {sdk_home}/default/openharmony; set OHOS_BASE_SDK_HOME to a writable SDK root containing API {api_level}"
        )
    })?;
    let mirror = project_root.join(".uniffi-openharmony-sdk");
    std::fs::create_dir_all(&mirror)
        .with_context(|| format!("creating temporary OpenHarmony SDK mirror {mirror}"))?;
    let api_dir = mirror.join(api_level.to_string());
    create_directory_symlink(&components, &api_dir).with_context(|| {
        format!("linking OpenHarmony API {api_level} SDK components {components} into {mirror}")
    })?;
    Ok(mirror)
}

#[cfg(unix)]
fn create_directory_symlink(source: &Utf8Path, target: &Utf8Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(source, target)
}

#[cfg(windows)]
fn create_directory_symlink(source: &Utf8Path, target: &Utf8Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(source, target)
}

fn run_harmony_tool(
    tools: &HarmonyHarTools,
    tool: &str,
    args: &[&str],
    cwd: &Utf8Path,
) -> Result<()> {
    let mut command = Command::new(tool);
    command
        .args(args)
        .current_dir(cwd)
        .env("DEVECO_SDK_HOME", &tools.sdk_home);
    if let Some(node_home) = &tools.node_home {
        command.env("NODE_HOME", node_home);
    }
    if let Some(ohos_base_sdk_home) = &tools.ohos_base_sdk_home {
        command.env("OHOS_BASE_SDK_HOME", ohos_base_sdk_home);
    } else {
        command.env_remove("OHOS_BASE_SDK_HOME");
    }
    let output = command
        .output()
        .with_context(|| format!("running `{tool} {}` in {cwd}", args.join(" ")))?;
    if !output.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if !output.status.success() {
        bail!(
            "Harmony command `{tool} {}` failed in {cwd} with {}",
            args.join(" "),
            output.status
        );
    }
    Ok(())
}

fn write_hvigor_har_project(
    project_root: &Utf8Path,
    module_dir: &Utf8Path,
    metadata: &OhosPackageMetadata,
    sdk: &SdkCompatibility,
    tools: &HarmonyHarTools,
    target_sdk_version: Option<&str>,
) -> Result<()> {
    std::fs::create_dir_all(project_root.join("AppScope"))?;
    std::fs::create_dir_all(project_root.join("hvigor"))?;
    std::fs::write(
        project_root.join("AppScope/app.json5"),
        render_json5(serde_json::json!({
            "app": {
                "bundleName": "dev.uniffi.generated.har",
                "vendor": "UniFFI",
                "versionCode": 1,
                "versionName": "1.0.0"
            }
        }))?,
    )?;
    std::fs::write(
        project_root.join("hvigorfile.ts"),
        "import { appTasks } from '@ohos/hvigor-ohos-plugin';\n\nexport default { system: appTasks, plugins: [] };\n",
    )?;
    std::fs::write(
        module_dir.join("hvigorfile.ts"),
        "import { harTasks } from '@ohos/hvigor-ohos-plugin';\n\nexport default { system: harTasks, plugins: [] };\n",
    )?;
    let root_package = serde_json::json!({
        "modelVersion": tools.model_version,
        "dependencies": {},
        "devDependencies": {}
    });
    std::fs::write(
        project_root.join("oh-package.json5"),
        render_json5(root_package)?,
    )?;
    std::fs::write(
        project_root.join("hvigor/hvigor-config.json5"),
        render_json5(serde_json::json!({
            "modelVersion": tools.model_version,
            "dependencies": {},
            "execution": { "daemon": false, "incremental": false }
        }))?,
    )?;
    let product = render_hvigor_product(&tools.compile_sdk, sdk, target_sdk_version)?;
    std::fs::write(
        project_root.join("build-profile.json5"),
        render_json5(serde_json::json!({
            "app": {
                "products": [product],
                "buildModeSet": [{ "name": "debug" }, { "name": "release" }]
            },
            "modules": [{
                "name": metadata.module_name,
                "srcPath": "./library"
            }]
        }))?,
    )?;
    Ok(())
}

fn write_hvigor_hsp_project(
    project_root: &Utf8Path,
    module_dir: &Utf8Path,
    metadata: &OhosPackageMetadata,
    sdk: &SdkCompatibility,
    tools: &HarmonyHarTools,
    target_sdk_version: Option<&str>,
    integrated: bool,
    bundle_name: Option<&str>,
) -> Result<()> {
    let app_scope = project_root.join("AppScope");
    let app_resources = app_scope.join("resources/base");
    std::fs::create_dir_all(app_resources.join("element"))?;
    std::fs::create_dir_all(app_resources.join("media"))?;
    std::fs::create_dir_all(project_root.join("hvigor"))?;
    let bundle_name = if integrated {
        "dev.uniffi.generated.integrated_hsp"
    } else {
        bundle_name.context("non-integrated HSP project is missing its host bundleName")?
    };
    validate_hsp_bundle_name(bundle_name)?;
    std::fs::write(
        app_scope.join("app.json5"),
        render_json5(serde_json::json!({
            "app": {
                "bundleName": bundle_name,
                "vendor": "UniFFI",
                "versionCode": 1,
                "versionName": metadata.version,
                "icon": "$media:app_icon",
                "label": "$string:app_name"
            }
        }))?,
    )?;
    std::fs::write(
        app_resources.join("element/string.json"),
        render_json5(serde_json::json!({
            "string": [{ "name": "app_name", "value": metadata.name }]
        }))?,
    )?;
    std::fs::write(
        app_resources.join("media/app_icon.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\" viewBox=\"0 0 64 64\"><rect width=\"64\" height=\"64\" rx=\"12\" fill=\"#0A59F7\"/><path d=\"M18 18h10v10H18zm18 0h10v10H36zM18 36h10v10H18zm18 0h10v10H36z\" fill=\"#FFFFFF\"/></svg>\n",
    )?;
    std::fs::write(
        project_root.join("hvigorfile.ts"),
        "import { appTasks } from '@ohos/hvigor-ohos-plugin';\n\nexport default { system: appTasks, plugins: [] };\n",
    )?;
    std::fs::write(
        module_dir.join("hvigorfile.ts"),
        "import { hspTasks } from '@ohos/hvigor-ohos-plugin';\n\nexport default { system: hspTasks, plugins: [] };\n",
    )?;
    std::fs::write(
        project_root.join("oh-package.json5"),
        render_json5(serde_json::json!({
            "modelVersion": tools.model_version,
            "dependencies": {},
            "devDependencies": {}
        }))?,
    )?;
    std::fs::write(
        project_root.join("hvigor/hvigor-config.json5"),
        render_json5(serde_json::json!({
            "modelVersion": tools.model_version,
            "dependencies": {},
            "execution": { "daemon": false, "incremental": false, "parallel": false }
        }))?,
    )?;
    let mut product = render_hvigor_product(&tools.compile_sdk, sdk, target_sdk_version)?;
    // Packaged HSP bytecode must use the same normalized OHM URL mode as its
    // consumer. Modern Harmony applications enable this mode, and Hvigor
    // rejects an HSP compiled with the legacy default before dependency
    // resolution. This applies to host-bound and integrated HSPs alike.
    product
        .as_object_mut()
        .expect("Hvigor product is an object")
        .insert(
            "buildOption".into(),
            serde_json::json!({
                "strictMode": { "useNormalizedOHMUrl": true }
            }),
        );
    std::fs::write(
        project_root.join("build-profile.json5"),
        render_json5(serde_json::json!({
            "app": {
                "products": [product],
                "buildModeSet": [{ "name": "debug" }, { "name": "release" }]
            },
            "modules": [{
                "name": metadata.module_name,
                "srcPath": "./library",
                "targets": [{
                    "name": "default",
                    "applyToProducts": ["default"]
                }]
            }]
        }))?,
    )?;
    Ok(())
}

fn build_hvigor_hsp_from_project(
    options: &BuildOptions,
    package: &HostPackage,
    metadata: &OhosPackageMetadata,
    source_project: &Utf8Path,
    sdk: &SdkCompatibility,
    tools: &mut HarmonyHarTools,
    expected_so_inventory: &HspSoInventory,
    expected_runtime_so_inventory: &RuntimeSoInventory,
) -> Result<HspArchiveMembers> {
    let build_parent = source_project
        .parent()
        .context("HSP source project has no staging parent")?;
    let build = tempfile::Builder::new()
        .prefix(".uniffi-ohos-hsp-build-")
        .tempdir_in(build_parent)
        .with_context(|| format!("creating HSP build directory in {build_parent}"))?;
    let build_root = Utf8PathBuf::from_path_buf(build.path().to_path_buf())
        .map_err(|path| anyhow::anyhow!("HSP build path is not UTF-8: {}", path.display()))?;
    copy_dir_recursive(source_project, &build_root)?;
    if sdk.sdk_type == RuntimeSdkType::OpenHarmony {
        tools.ohos_base_sdk_home = Some(match tools.ohos_base_sdk_home.take() {
            Some(path) => path
                .canonicalize_utf8()
                .with_context(|| format!("canonicalizing OpenHarmony base SDK root {path}"))?,
            None => create_openharmony_sdk_mirror(
                &build_root,
                &tools.sdk_home,
                tools.compile_sdk.api_level,
            )?,
        });
    } else {
        tools.ohos_base_sdk_home = None;
    }
    let module_dir = build_root.join("library");
    let output_root = module_dir.join("build/default/outputs/default");
    if output_root.exists() {
        bail!("fresh HSP source project unexpectedly contains stale build outputs: {output_root}");
    }
    run_harmony_tool(
        tools,
        &tools.ohpm,
        &["install", "--all", "--lockfile_stable_order"],
        &build_root,
    )?;
    run_harmony_tool(
        tools,
        &tools.ohpm,
        &["install", "--all", "--lockfile_stable_order"],
        &module_dir,
    )?;
    let module_target = format!("{}@default", metadata.module_name);
    run_harmony_tool(
        tools,
        &tools.hvigorw,
        &[
            "assembleHsp",
            "--mode",
            "module",
            "-p",
            &format!("module={module_target}"),
            "-p",
            "product=default",
            "-p",
            "buildMode=release",
            "--no-daemon",
            "--no-incremental",
        ],
        &build_root,
    )?;
    let tgz = discover_release_tgz(&output_root)?;
    let before_prepublish = read_verified_regular_file_bounded(
        &tgz,
        MAX_HSP_ARCHIVE_COMPRESSED_BYTES,
        "Hvigor release tgz",
    )?;
    let members = parse_hsp_tgz(&before_prepublish)?;
    validate_runtime_hsp(
        &members.runtime_hsp,
        package,
        metadata,
        source_project.join("library/libs").as_path(),
        expected_so_inventory,
        expected_runtime_so_inventory,
        options.integrated_hsp,
        options.hsp_bundle_name.as_deref(),
    )?;
    validate_interface_har(&members.interface_har, metadata)?;
    run_harmony_tool(
        tools,
        &tools.ohpm,
        &["prepublish", tgz.as_str()],
        &build_root,
    )?;
    let after_prepublish = read_verified_regular_file_bounded(
        &tgz,
        MAX_HSP_ARCHIVE_COMPRESSED_BYTES,
        "prepublished HSP tgz",
    )?;
    if after_prepublish != before_prepublish {
        bail!("ohpm prepublish modified the Hvigor release tgz; refusing to publish changed bytes");
    }
    Ok(HspArchiveMembers {
        tgz: before_prepublish,
        ..members
    })
}

fn discover_release_tgz(output_root: &Utf8Path) -> Result<Utf8PathBuf> {
    let metadata = std::fs::symlink_metadata(output_root)
        .with_context(|| format!("reading expected HSP release output {output_root}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("HSP release output must be a real directory: {output_root}");
    }
    let mut candidates = Vec::new();
    for entry in std::fs::read_dir(output_root)
        .with_context(|| format!("reading HSP release output {output_root}"))?
    {
        let entry = entry?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!("HSP release output path is not utf8: {}", path.display())
        })?;
        let file_type = entry.file_type()?;
        if path.extension() == Some("tgz") {
            if file_type.is_symlink() || !file_type.is_file() {
                bail!("refusing non-regular release tgz candidate: {path}");
            }
            let metadata = entry.metadata()?;
            ensure_file_has_single_link(&metadata, &path)?;
            candidates.push(path);
        }
    }
    candidates.sort();
    match candidates.as_slice() {
        [candidate] => Ok(candidate.clone()),
        [] => bail!("Hvigor assembleHsp produced no top-level release .tgz under {output_root}"),
        many => bail!(
            "Hvigor assembleHsp produced multiple top-level release .tgz candidates under {output_root}: {many:?}"
        ),
    }
}

fn parse_hsp_tgz(bytes: &[u8]) -> Result<HspArchiveMembers> {
    if bytes.len() as u64 > MAX_HSP_ARCHIVE_TOTAL_BYTES {
        bail!("HSP release tgz exceeds the compressed archive size limit");
    }
    let entries = read_bounded_targz_entries(bytes, false, None, "HSP release tgz")?;
    if entries.len() != 2 {
        bail!(
            "HSP release tgz must contain exactly two top-level files, found {}",
            entries.len()
        );
    }
    let mut runtime = None;
    let mut interface = None;
    for (name, data) in entries {
        let data = data.context("HSP release tgz must not contain directory entries")?;
        let path = Utf8Path::new(&name);
        if path.components().count() != 1 || name.contains(['/', '\\']) {
            bail!("HSP release tgz member must be a top-level file: {name}");
        }
        match path.extension() {
            Some("hsp") if runtime.is_none() => runtime = Some((name, data)),
            Some("har") if interface.is_none() => interface = Some((name, data)),
            Some("hsp") | Some("har") => {
                bail!("HSP release tgz contains a duplicate package kind: {name}")
            }
            _ => bail!("HSP release tgz contains an unknown top-level member: {name}"),
        }
    }
    let (runtime_name, runtime_hsp) = runtime.context("HSP release tgz is missing runtime .hsp")?;
    let (interface_name, interface_har) =
        interface.context("HSP release tgz is missing Interface .har")?;
    Ok(HspArchiveMembers {
        tgz: Vec::new(),
        runtime_name,
        runtime_hsp,
        interface_name,
        interface_har,
    })
}

fn read_bounded_targz_entries(
    bytes: &[u8],
    allow_directories: bool,
    required_root: Option<&str>,
    label: &str,
) -> Result<BTreeMap<String, Option<Vec<u8>>>> {
    let decoder = GzDecoder::new(Cursor::new(bytes));
    let mut archive = tar::Archive::new(decoder);
    let mut entries = BTreeMap::new();
    let mut total = 0_u64;
    for (index, entry) in archive
        .entries()
        .with_context(|| format!("reading {label} entries"))?
        .enumerate()
    {
        if index >= MAX_HSP_ARCHIVE_ENTRIES {
            bail!("{label} exceeds the entry-count limit");
        }
        let mut entry = entry.with_context(|| format!("reading {label} entry {index}"))?;
        let raw = entry.path()?;
        let path = Utf8PathBuf::from_path_buf(raw.into_owned()).map_err(|path| {
            anyhow::anyhow!("{label} entry path is not utf8: {}", path.display())
        })?;
        validate_bounded_archive_path(&path, required_root, label)?;
        let name = path.as_str().to_string();
        if entries.contains_key(&name) {
            bail!("{label} contains a duplicate entry path: {name}");
        }
        let kind = entry.header().entry_type();
        if kind == EntryType::Directory {
            if !allow_directories {
                bail!("{label} contains an unexpected directory entry: {name}");
            }
            entries.insert(name, None);
            continue;
        }
        if !kind.is_file() {
            bail!("{label} contains a non-regular archive entry: {name}");
        }
        let size = entry.size();
        if size > MAX_HSP_ARCHIVE_MEMBER_BYTES {
            bail!("{label} member `{name}` exceeds the per-entry size limit");
        }
        total = total
            .checked_add(size)
            .context("HSP archive expanded size overflow")?;
        if total > MAX_HSP_ARCHIVE_TOTAL_BYTES {
            bail!("{label} exceeds the total expanded size limit");
        }
        let capacity = usize::try_from(size).context("HSP archive member does not fit memory")?;
        let mut data = Vec::with_capacity(capacity);
        entry
            .read_to_end(&mut data)
            .with_context(|| format!("reading {label} member `{name}`"))?;
        if data.len() as u64 != size {
            bail!("{label} member `{name}` size changed while reading");
        }
        entries.insert(name, Some(data));
    }
    Ok(entries)
}

fn validate_bounded_archive_path(
    path: &Utf8Path,
    required_root: Option<&str>,
    label: &str,
) -> Result<()> {
    let raw = path.as_str();
    if raw.is_empty()
        || raw.len() > MAX_HSP_ARCHIVE_PATH_BYTES
        || path.is_absolute()
        || raw.contains('\\')
        || raw.contains("//")
        || path
            .components()
            .any(|part| matches!(part.as_str(), "" | "." | ".."))
    {
        bail!("unsafe {label} archive entry path: {path}");
    }
    if let Some(root) = required_root {
        if !path.starts_with(root)
            || path.components().next().map(|part| part.as_str()) != Some(root)
        {
            bail!("{label} entry is outside the required `{root}/` root: {path}");
        }
    }
    Ok(())
}

fn validate_runtime_hsp(
    bytes: &[u8],
    package: &HostPackage,
    metadata: &OhosPackageMetadata,
    staged_libs: &Utf8Path,
    expected: &HspSoInventory,
    expected_runtime: &RuntimeSoInventory,
    integrated: bool,
    bundle_name: Option<&str>,
) -> Result<()> {
    let entries = read_bounded_zip_entries(bytes, "runtime HSP")?;
    for required in ["module.json", "pack.info", "ets/modules.abc"] {
        let data = entries
            .get(required)
            .and_then(|value| value.as_ref())
            .with_context(|| {
                format!("runtime HSP is missing required regular file `{required}`")
            })?;
        if required == "ets/modules.abc" && data.is_empty() {
            bail!("runtime HSP ArkTS bytecode ets/modules.abc is empty");
        }
    }
    if integrated
        && !entries
            .get("pkgContextInfo.json")
            .is_some_and(|value| value.is_some())
    {
        bail!("integrated runtime HSP is missing required regular file `pkgContextInfo.json`");
    }
    let module: Value = serde_json::from_slice(
        entries["module.json"]
            .as_ref()
            .expect("required module.json is a file"),
    )
    .context("parsing runtime HSP module.json")?;
    if module["module"]["name"] != metadata.module_name
        || module["module"]["packageName"] != metadata.name
        || module["module"]["type"] != "shared"
        || module["module"]["deliveryWithInstall"] != true
        || module["module"]["compileMode"] != "esmodule"
    {
        bail!(
            "runtime HSP module identity/type mismatch for package `{}` module `{}`",
            metadata.name,
            metadata.module_name
        );
    }
    let actual_bundle = module["app"]["bundleName"]
        .as_str()
        .context("runtime HSP module.json app.bundleName is missing")?;
    if integrated {
        if !actual_bundle.is_empty() {
            bail!("integrated runtime HSP must have an empty bundleName, found `{actual_bundle}`");
        }
    } else if Some(actual_bundle) != bundle_name {
        bail!(
            "non-integrated runtime HSP bundleName mismatch: expected `{}`, found `{actual_bundle}`",
            bundle_name.unwrap_or("<missing>")
        );
    }
    validate_staged_hsp_so_inventory(
        staged_libs,
        staged_libs
            .parent()
            .context("staged HSP libs directory has no package root")?,
        package,
        expected,
    )?;
    let mut actual = RuntimeSoInventory::new();
    for (path, data) in &entries {
        if data.is_none() || Utf8Path::new(path).extension() != Some("so") {
            continue;
        }
        let components = Utf8Path::new(path)
            .components()
            .map(|part| part.as_str())
            .collect::<Vec<_>>();
        if components.len() != 3 || components[0] != "libs" {
            bail!("runtime HSP contains a native library outside libs/<abi>/<name>.so: {path}");
        }
        let name = components[2].to_string();
        let facts = runtime_elf_facts(
            data.as_ref().expect("runtime SO is a regular entry"),
            components[1],
            &name,
        )?;
        if actual
            .entry(components[1].to_string())
            .or_default()
            .insert(name.clone(), facts)
            .is_some()
        {
            bail!("runtime HSP contains duplicate native library `{path}`");
        }
    }
    // Hvigor's DoNativeStrip task runs the SDK's `llvm-strip <input>
    // -o<output>`. The expected runtime inventory is produced with that exact
    // command from the independently Cargo-bound staging files. Comparing the
    // ELF ABI facts keeps the staged and packaged native libraries compatible
    // without binding their bytes to a generated identity.
    if &actual != expected_runtime {
        bail!(
            "runtime HSP normalized SO provenance mismatch: expected={expected_runtime:?}, actual={actual:?}"
        );
    }
    Ok(())
}

fn actual_staged_hsp_so_inventory(staged_libs: &Utf8Path) -> Result<HspSoInventory> {
    let mut actual = HspSoInventory::new();
    for entry in std::fs::read_dir(staged_libs)
        .with_context(|| format!("reading staged HSP native inventory {staged_libs}"))?
    {
        let entry = entry?;
        let entry_type = entry.file_type()?;
        let entry_path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!("staged HSP native path is not utf8: {}", path.display())
        })?;
        if entry_type.is_symlink() {
            bail!("unsafe staged HSP native symlink: {entry_path}");
        }
        if entry_type.is_file() && entry.file_name() == OsStr::new("index.d.ts") {
            continue;
        }
        if !entry_type.is_dir() {
            bail!("unexpected staged HSP native root entry: {entry_path}");
        }
        let abi = entry.file_name().to_string_lossy().to_string();
        let mut names = BTreeSet::new();
        for library in std::fs::read_dir(&entry_path)? {
            let library = library?;
            let kind = library.file_type()?;
            let path = Utf8PathBuf::from_path_buf(library.path()).map_err(|path| {
                anyhow::anyhow!("staged HSP library path is not utf8: {}", path.display())
            })?;
            if kind.is_symlink() || !kind.is_file() {
                bail!("unsafe staged HSP native entry: {path}");
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            ensure_file_has_single_link(&metadata, &path)?;
            if path.extension() != Some("so") {
                bail!("unexpected non-SO staged HSP ABI entry: {path}");
            }
            let name = path.file_name().unwrap().to_string();
            let _ = read_verified_regular_file_bounded(
                &path,
                MAX_HSP_ARCHIVE_MEMBER_BYTES,
                "staged HSP native library",
            )?;
            if !names.insert(name.clone()) {
                bail!("duplicate staged HSP SO `{name}` in ABI `{abi}`");
            }
        }
        actual.insert(abi, names);
    }
    if actual.is_empty() {
        bail!("staged HSP contains no requested ABI native libraries");
    }
    Ok(actual)
}

fn runtime_elf_facts(bytes: &[u8], abi: &str, name: &str) -> Result<RuntimeElfFacts> {
    use goblin::elf::{header, Elf};

    let elf =
        Elf::parse(bytes).with_context(|| format!("parsing normalized ELF `{abi}/{name}`"))?;
    if !elf.is_lib || !elf.little_endian {
        bail!("normalized HSP native library must be a little-endian ET_DYN ELF: {abi}/{name}");
    }
    let (expected_machine, expected_is_64) = match abi {
        "arm64-v8a" => (header::EM_AARCH64, true),
        "armeabi-v7a" => (header::EM_ARM, false),
        "x86_64" => (header::EM_X86_64, true),
        "loongarch64" => (258, true), // ELF e_machine value assigned to LoongArch.
        other => bail!("unsupported HSP runtime ABI `{other}`"),
    };
    if elf.header.e_machine != expected_machine {
        bail!(
            "normalized HSP ELF architecture mismatch for {abi}/{name}: expected {expected_machine}, found {}",
            elf.header.e_machine
        );
    }
    if elf.is_64 != expected_is_64 {
        bail!(
            "normalized HSP ELF class mismatch for {abi}/{name}: expected ELFCLASS{}, found ELFCLASS{}",
            if expected_is_64 { 64 } else { 32 },
            if elf.is_64 { 64 } else { 32 }
        );
    }
    if let Some(soname) = elf.soname {
        if soname != name {
            bail!("normalized HSP ELF SONAME mismatch for {abi}/{name}: `{soname}`");
        }
    }
    Ok(RuntimeElfFacts {
        is_64: elf.is_64,
        little_endian: elf.little_endian,
        machine: elf.header.e_machine,
        soname: elf.soname.map(str::to_string),
    })
}

fn normalize_staged_hsp_so_inventory(
    staged_libs: &Utf8Path,
    expected_raw: &HspSoInventory,
    strip: &Utf8Path,
) -> Result<RuntimeSoInventory> {
    normalize_staged_hsp_so_inventory_with_hook(staged_libs, expected_raw, strip, |_| Ok(()))
}

fn normalize_staged_hsp_so_inventory_with_hook<F>(
    staged_libs: &Utf8Path,
    expected_raw: &HspSoInventory,
    strip: &Utf8Path,
    mut before_spawn: F,
) -> Result<RuntimeSoInventory>
where
    F: FnMut(&Utf8Path) -> Result<()>,
{
    let strip_metadata = std::fs::symlink_metadata(strip)
        .with_context(|| format!("reading OHOS llvm-strip used for HSP normalization {strip}"))?;
    let canonical_strip = strip
        .canonicalize_utf8()
        .with_context(|| format!("resolving OHOS llvm-strip {strip}"))?;
    let canonical_metadata = std::fs::symlink_metadata(&canonical_strip)?;
    if !canonical_metadata.is_file() || canonical_metadata.file_type().is_symlink() {
        bail!("OHOS HSP llvm-strip must resolve to a regular file: {strip}");
    }
    if strip_metadata.file_type().is_symlink() {
        let requested_parent = strip
            .parent()
            .context("OHOS llvm-strip has no parent")?
            .canonicalize_utf8()?;
        if canonical_strip.parent() != Some(requested_parent.as_path()) {
            bail!(
                "OHOS HSP llvm-strip symlink must resolve inside its SDK bin directory: {strip} -> {canonical_strip}"
            );
        }
    } else if !strip_metadata.is_file() {
        bail!("OHOS HSP llvm-strip must be a regular file or SDK-local alias: {strip}");
    }
    let mut normalized = RuntimeSoInventory::new();
    for (abi, libraries) in expected_raw {
        let mut abi_inventory = BTreeMap::new();
        for name in libraries {
            let source = staged_libs.join(abi).join(name);
            let raw = read_verified_regular_file_bounded(
                &source,
                MAX_HSP_ARCHIVE_MEMBER_BYTES,
                "raw staged HSP native library",
            )?;
            // Reject invalid staged input before creating the temporary strip
            // output directory.
            runtime_elf_facts(&raw, abi, name)?;
            let temp = tempfile::Builder::new()
                .prefix("uniffi-hsp-strip-")
                .tempdir()
                .context("creating temporary HSP strip directory")?;
            let temp_root =
                Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).map_err(|path| {
                    anyhow::anyhow!("temporary HSP strip path is not UTF-8: {}", path.display())
                })?;
            let output = temp_root.join("normalized.so");
            before_spawn(&temp_root)?;
            let mut strip_command = Command::new(&canonical_strip);
            // DevEco ships llvm-strip as an SDK-local alias of the LLVM
            // multi-call binary. Execute the resolved SDK binary while
            // preserving the validated alias argv[0], otherwise LLVM selects
            // llvm-objcopy mode and rejects strip's `-o` contract.
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                strip_command.arg0(
                    strip
                        .file_name()
                        .context("OHOS llvm-strip alias has no file name")?,
                );
            }
            let status = strip_command
                .arg(&source)
                .arg(format!("-o{output}"))
                .status()
                .with_context(|| format!("running OHOS llvm-strip for {source}"))?;
            if !status.success() {
                bail!("OHOS llvm-strip failed for {source} with status {status}");
            }
            let bytes = read_verified_regular_file_bounded(
                &output,
                MAX_HSP_ARCHIVE_MEMBER_BYTES,
                "normalized HSP native library",
            )?;
            let facts = runtime_elf_facts(&bytes, abi, name)?;
            if abi_inventory.insert(name.clone(), facts).is_some() {
                bail!("duplicate normalized HSP SO `{abi}/{name}`");
            }
        }
        normalized.insert(abi.clone(), abi_inventory);
    }
    Ok(normalized)
}

fn validate_staged_hsp_so_inventory(
    staged_libs: &Utf8Path,
    package_root: &Utf8Path,
    package: &HostPackage,
    expected: &HspSoInventory,
) -> Result<()> {
    let actual = actual_staged_hsp_so_inventory(staged_libs)?;
    if &actual != expected {
        bail!("staged HSP SO inventory mismatch: expected={expected:?}, actual={actual:?}");
    }
    let bridge = native_lib_filename(&package.lib_target_name);
    for (abi, names) in expected {
        if !names.contains(&bridge) || !names.contains("libc++_shared.so") {
            bail!(
                "independent HSP SO contract for ABI `{abi}` must contain bridge `{bridge}` and `libc++_shared.so`: {names:?}"
            );
        }
        if names.len() < 3 {
            bail!(
                "independent HSP SO contract for ABI `{abi}` is missing the core/bridge/libc++ triplet: {names:?}"
            );
        }
    }
    let package_json = read_verified_regular_file(&package_root.join("oh-package.json5"))?;
    let package_json: Value =
        serde_json::from_slice(&package_json).context("parsing staged HSP oh-package.json5")?;
    let declared = package_json["nativeComponents"]
        .as_array()
        .context("staged HSP oh-package.json5 is missing nativeComponents")?
        .iter()
        .map(|component| {
            component["name"]
                .as_str()
                .context("staged HSP nativeComponents entry is missing name")
                .map(str::to_string)
        })
        .collect::<Result<BTreeSet<_>>>()?;
    let expected_components = expected
        .values()
        .flat_map(|names| names.iter().cloned())
        .collect::<BTreeSet<_>>();
    if declared != expected_components {
        bail!(
            "staged HSP nativeComponents mismatch: expected={expected_components:?}, declared={declared:?}"
        );
    }
    Ok(())
}

fn read_bounded_zip_entries(
    bytes: &[u8],
    label: &str,
) -> Result<BTreeMap<String, Option<Vec<u8>>>> {
    if bytes.len() as u64 > MAX_HSP_ARCHIVE_TOTAL_BYTES {
        bail!("{label} exceeds the compressed archive size limit");
    }
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).with_context(|| format!("opening {label} ZIP"))?;
    if archive.len() > MAX_HSP_ARCHIVE_ENTRIES {
        bail!("{label} exceeds the entry-count limit");
    }
    let mut total = 0_u64;
    let mut entries = BTreeMap::new();
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("reading {label} ZIP entry {index}"))?;
        let raw_name = entry.name().to_string();
        if entry.enclosed_name().is_none() {
            bail!("unsafe {label} ZIP entry path: {raw_name}");
        }
        let name = if entry.is_dir() {
            raw_name.trim_end_matches('/').to_string()
        } else {
            raw_name
        };
        let path = Utf8Path::new(&name);
        validate_bounded_archive_path(path, None, label)?;
        if entries.contains_key(&name) {
            bail!("{label} contains a duplicate ZIP entry path: {name}");
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            bail!("{label} contains a symlink ZIP entry: {name}");
        }
        if entry.is_dir() {
            entries.insert(name, None);
            continue;
        }
        let size = entry.size();
        if size > MAX_HSP_ARCHIVE_MEMBER_BYTES {
            bail!("{label} member `{name}` exceeds the per-entry size limit");
        }
        total = total
            .checked_add(size)
            .context("runtime HSP expanded size overflow")?;
        if total > MAX_HSP_ARCHIVE_TOTAL_BYTES {
            bail!("{label} exceeds the total expanded size limit");
        }
        let mut data = Vec::with_capacity(usize::try_from(size)?);
        entry.read_to_end(&mut data)?;
        if data.len() as u64 != size {
            bail!("{label} member `{name}` size changed while reading");
        }
        entries.insert(name, Some(data));
    }
    Ok(entries)
}

fn validate_interface_har(bytes: &[u8], metadata: &OhosPackageMetadata) -> Result<()> {
    let entries = read_bounded_targz_entries(bytes, true, Some("package"), "Interface HAR")?;
    if entries
        .iter()
        .any(|(path, data)| data.is_some() && Utf8Path::new(path).extension() == Some("so"))
    {
        bail!("Interface HAR must not contain any native .so files");
    }
    let package_bytes = entries
        .get("package/oh-package.json5")
        .and_then(|value| value.as_ref())
        .context("Interface HAR is missing package/oh-package.json5")?;
    let package: Value =
        serde_json::from_slice(package_bytes).context("parsing Interface HAR oh-package.json5")?;
    if package["name"] != metadata.name
        || package["version"] != metadata.version
        || package["packageType"] != "InterfaceHar"
        || package["types"] != "Index.d.ets"
    {
        bail!("Interface HAR package identity/type metadata mismatch");
    }
    let module_bytes = entries
        .get("package/src/main/module.json")
        .and_then(|value| value.as_ref())
        .context("Interface HAR is missing compiled package/src/main/module.json")?;
    let module: Value =
        serde_json::from_slice(module_bytes).context("parsing Interface HAR module.json")?;
    if module["module"]["name"] != metadata.module_name
        || module["module"]["packageName"] != metadata.name
        || module["module"]["type"] != "shared"
        || module["module"]["deliveryWithInstall"] != true
    {
        bail!("Interface HAR compiled module identity/type mismatch");
    }
    let declarations = entries
        .get("package/Index.d.ets")
        .and_then(|value| value.as_ref())
        .context("Interface HAR is missing public package/Index.d.ets")?;
    if declarations.is_empty() {
        bail!("Interface HAR public Index.d.ets is empty");
    }
    let _root_declarations = std::str::from_utf8(declarations)
        .context("Interface HAR public Index.d.ets is not UTF-8")?;
    let has_native_declarations = entries.iter().any(|(path, data)| {
        data.is_some()
            && path.starts_with("package/src/main/cpp/types/")
            && (path.ends_with("/index.d.ts") || path.ends_with("/Index.d.ts"))
    });
    if !has_native_declarations {
        bail!("Interface HAR is missing native module declarations");
    }
    Ok(())
}

fn render_hvigor_product(
    compile_sdk: &CompileSdk,
    sdk: &SdkCompatibility,
    target_sdk_version: Option<&str>,
) -> Result<Value> {
    let mut product = serde_json::Map::new();
    product.insert("name".into(), Value::String("default".into()));
    product.insert(
        "runtimeOS".into(),
        Value::String(sdk.sdk_type.as_str().into()),
    );
    let target = resolve_target_sdk_version(compile_sdk, sdk, target_sdk_version)?;

    if compile_sdk.api_level >= 26 {
        // API 26 unifies HarmonyOS and OpenHarmony build-profile SDK fields as
        // strings.  platformVersion is the SDK's canonical unified version and
        // no longer uses the legacy `platform(api)` spelling.
        let compile = Value::String(compile_sdk.platform_version.clone());
        product.insert("compileSdkVersion".into(), compile);
        product.insert("targetSdkVersion".into(), target);
        product.insert(
            "compatibleSdkVersion".into(),
            Value::String(sdk.version.clone()),
        );
    } else {
        match sdk.sdk_type {
            RuntimeSdkType::HarmonyOs => {
                // HarmonyOS compileSdkVersion is optional before API 26.  Keep
                // the requested target spelling and preserve the caller's
                // explicit minimum-compatible version verbatim.
                product.insert("targetSdkVersion".into(), target);
                product.insert(
                    "compatibleSdkVersion".into(),
                    Value::String(sdk.version.clone()),
                );
            }
            RuntimeSdkType::OpenHarmony => {
                let compatible = sdk.version.parse::<u32>().with_context(|| {
                    format!(
                        "OpenHarmony compatible SDK `{}` must be a numeric API level before API 26",
                        sdk.version
                    )
                })?;
                let compile = Value::Number(u64::from(compile_sdk.api_level).into());
                product.insert("compileSdkVersion".into(), compile);
                product.insert("targetSdkVersion".into(), target);
                product.insert(
                    "compatibleSdkVersion".into(),
                    Value::Number(u64::from(compatible).into()),
                );
            }
        }
    }
    Ok(Value::Object(product))
}

fn resolve_target_sdk_version(
    compile_sdk: &CompileSdk,
    sdk: &SdkCompatibility,
    explicit: Option<&str>,
) -> Result<Value> {
    let compatible_api = compatible_sdk_api_level(sdk)?;
    let (target_api, target) = if let Some(explicit) = explicit {
        let value = validate_sdk_version_metadata_value(explicit, "target")?;
        let api = sdk_version_api_level(value, "target")?;
        let rendered = match sdk.sdk_type {
            RuntimeSdkType::HarmonyOs => {
                if api < 26 && !(value.contains('(') && value.ends_with(')')) {
                    bail!(
                        "HarmonyOS target SDK version `{value}` must use the official `platform(api)` spelling before API 26"
                    );
                }
                Value::String(value.to_string())
            }
            RuntimeSdkType::OpenHarmony if compile_sdk.api_level < 26 => {
                let numeric = value.parse::<u32>().with_context(|| {
                    format!(
                        "OpenHarmony target SDK `{value}` must be a numeric API level before API 26"
                    )
                })?;
                Value::Number(u64::from(numeric).into())
            }
            RuntimeSdkType::OpenHarmony => Value::String(value.to_string()),
        };
        (api, rendered)
    } else {
        let rendered = if compile_sdk.api_level >= 26 {
            Value::String(compile_sdk.platform_version.clone())
        } else {
            match sdk.sdk_type {
                RuntimeSdkType::HarmonyOs => Value::String(format!(
                    "{}({})",
                    compile_sdk.platform_version, compile_sdk.api_level
                )),
                RuntimeSdkType::OpenHarmony => {
                    Value::Number(u64::from(compile_sdk.api_level).into())
                }
            }
        };
        (compile_sdk.api_level, rendered)
    };

    if target_api < compatible_api {
        bail!(
            "target SDK API {target_api} is lower than compatible SDK API {compatible_api}; expected compatible SDK <= target SDK <= compile SDK"
        );
    }
    if target_api > compile_sdk.api_level {
        bail!(
            "target SDK API {target_api} exceeds compile SDK API {}; expected compatible SDK <= target SDK <= compile SDK",
            compile_sdk.api_level
        );
    }
    Ok(target)
}

fn discover_compiled_har(module_dir: &Utf8Path) -> Result<Utf8PathBuf> {
    let output_root = module_dir.join("build/default/outputs/default");
    let mut candidates = Vec::new();
    collect_files_with_extension(&output_root, "har", &mut candidates)?;
    candidates.sort();
    match candidates.as_slice() {
        [path] => Ok(path.clone()),
        [] => bail!("Hvigor assembleHar produced no .har under {output_root}"),
        paths => bail!(
            "Hvigor assembleHar produced multiple .har candidates under {output_root}: {paths:?}"
        ),
    }
}

fn collect_files_with_extension(
    path: &Utf8Path,
    extension: &str,
    out: &mut Vec<Utf8PathBuf>,
) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading expected Hvigor output path {path}"))?;
    if metadata.file_type().is_symlink() {
        if path.extension() == Some(extension) {
            bail!("refusing symlinked Hvigor .{extension} output candidate: {path}");
        }
        return Ok(());
    }
    if metadata.is_file() {
        if path.extension() == Some(extension) {
            out.push(path.to_path_buf());
        }
        return Ok(());
    }
    for entry in std::fs::read_dir(path).with_context(|| format!("reading {path}"))? {
        let child = Utf8PathBuf::from_path_buf(entry?.path()).map_err(|path| {
            anyhow::anyhow!("Hvigor output path is not utf8: {}", path.display())
        })?;
        collect_files_with_extension(&child, extension, out)?;
    }
    Ok(())
}

fn resolve_oh_package_metadata(
    options: &BuildOptions,
    package: &HostPackage,
    sdk: Option<SdkCompatibility>,
) -> Result<OhosPackageMetadata> {
    let name = resolve_oh_package_name(options.package_name.as_deref(), package)?;
    let module_name = options
        .module_name
        .clone()
        .unwrap_or(derive_module_name(&name)?);
    validate_module_name(&module_name)?;

    let version = options
        .package_version
        .as_deref()
        .unwrap_or(&package.version)
        .to_string();
    validate_package_version(&version)?;

    let description = options
        .description
        .clone()
        .or_else(|| package.description.clone());
    if let Some(description) = &description {
        if utf16_len(description) > 512 {
            bail!(
                "OHPM package description is {} UTF-16 code units; shorten it to at most 512 or pass --description <text>",
                utf16_len(description)
            );
        }
    }

    // OHPM exposes one author field while Cargo preserves an ordered author
    // list. Use the first non-empty Cargo author deterministically; callers
    // that need a different publication identity can use --author.
    let author = resolve_optional_metadata_override(
        "author",
        options.author.as_deref(),
        package
            .authors
            .iter()
            .find(|author| !author.trim().is_empty())
            .map(String::as_str),
    )?;
    let license = resolve_optional_metadata_override(
        "license",
        options.license.as_deref(),
        package.license.as_deref(),
    )?;

    Ok(OhosPackageMetadata {
        name,
        module_name,
        version,
        description,
        author,
        license,
        sdk,
        device_types: resolve_device_types(&options.device_types)?,
    })
}

fn resolve_optional_metadata_override(
    field: &str,
    override_value: Option<&str>,
    cargo_value: Option<&str>,
) -> Result<Option<String>> {
    let value = override_value.or(cargo_value);
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        if override_value.is_some() {
            bail!("OHPM package {field} override must not be empty; omit the flag to leave it unspecified");
        }
        return Ok(None);
    }
    if value.chars().any(char::is_control) {
        bail!(
            "OHPM package {field} must not contain control characters; provide a plain-text value"
        );
    }
    match field {
        "author" => validate_author(value)?,
        "license" if utf16_len(value) > 256 => {
            bail!("OHPM package license must be at most 256 UTF-16 code units")
        }
        _ => {}
    }
    Ok(Some(value.to_string()))
}

fn validate_author(author: &str) -> Result<()> {
    let name_end = author.find(['<', '(']).unwrap_or(author.len());
    let name = author[..name_end].trim();
    if utf16_len(name) > 128 {
        bail!("OHPM package author name must be at most 128 UTF-16 code units");
    }
    if let Some(start) = author.find('<') {
        let end = author[start + 1..]
            .find('>')
            .map(|offset| start + 1 + offset)
            .context("OHPM package author email is missing a closing `>`")?;
        let email = author[start + 1..end].trim();
        if utf16_len(email) > 64
            || !email.is_ascii()
            || !email.contains('@')
            || email.chars().any(char::is_whitespace)
        {
            bail!("OHPM package author email must be a valid ASCII address of at most 64 UTF-16 code units");
        }
    }
    if let Some(start) = author.find('(') {
        let end = author[start + 1..]
            .find(')')
            .map(|offset| start + 1 + offset)
            .context("OHPM package author URL is missing a closing `)`")?;
        if utf16_len(author[start + 1..end].trim()) > 256 {
            bail!("OHPM package author URL must be at most 256 UTF-16 code units");
        }
    }
    Ok(())
}

fn ensure_unique_module_names(
    packages: &[HostPackage],
    metadata: &[OhosPackageMetadata],
) -> Result<()> {
    let mut seen = BTreeMap::new();
    for (package, metadata) in packages.iter().zip(metadata) {
        if let Some(previous) = seen.insert(metadata.module_name.clone(), package.name.clone()) {
            bail!(
                "OHOS module name collision: Cargo packages `{previous}` and `{}` both normalize to `{}`; build one package at a time with --package and pass a unique --module-name",
                package.name,
                metadata.module_name
            );
        }
    }
    Ok(())
}

fn resolve_sdk_compatibility(
    options: &BuildOptions,
    ohos_ndk: &str,
) -> Result<Option<SdkCompatibility>> {
    let version = options
        .compatible_sdk_version
        .as_deref()
        .map(validate_sdk_metadata_value)
        .transpose()?;
    let Some(version) = version else {
        if options.compatible_sdk_type.is_some() {
            bail!(
                "--compatible-sdk-type requires --compatible-sdk-version; compile SDK metadata is not a minimum runtime compatibility declaration"
            );
        }
        if !options.no_har {
            eprintln!(
                "warning: compatibleSdkVersion was not provided; compile SDK apiVersion is intentionally not used as the minimum runtime API. Final HAR packaging requires --compatible-sdk-version."
            );
        }
        return Ok(None);
    };
    let sdk_type = if let Some(explicit) = options.compatible_sdk_type.as_deref() {
        validate_sdk_metadata_value(explicit)?;
        Some(RuntimeSdkType::parse(explicit)?)
    } else {
        discover_sdk_type(ohos_ndk, options.bisheng)?
    }
    .with_context(|| {
            "compatible SDK type could not be identified from the actual SDK root; pass --compatible-sdk-type HarmonyOS or OpenHarmony"
        })?;
    Ok(Some(SdkCompatibility {
        version: version.to_string(),
        sdk_type,
    }))
}

fn discover_sdk_type(ohos_ndk: &str, bisheng: bool) -> Result<Option<RuntimeSdkType>> {
    let mut candidates = Vec::new();
    if bisheng {
        if let Some(hos_ndk) = resolve_hos_ndk(ohos_ndk) {
            candidates.push((
                hos_ndk.join("native/uni-package.json"),
                RuntimeSdkType::HarmonyOs,
            ));
        }
    }
    candidates.push((
        Path::new(ohos_ndk).join("native/oh-uni-package.json"),
        RuntimeSdkType::OpenHarmony,
    ));

    for (path, sdk_type) in candidates {
        if !path.exists() {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading OHOS SDK metadata {}", path.display()))?;
        let value: Value = serde_json::from_str(&text)
            .with_context(|| format!("parsing OHOS SDK metadata {}", path.display()))?;
        if !value.is_object() {
            continue;
        }
        return Ok(Some(sdk_type));
    }
    Ok(None)
}

fn validate_sdk_metadata_value(value: &str) -> Result<&str> {
    validate_sdk_version_metadata_value(value, "compatible")
}

fn validate_sdk_version_metadata_value<'a>(value: &'a str, label: &str) -> Result<&'a str> {
    let value = value.trim();
    if value.is_empty() || utf16_len(value) > 64 || value.chars().any(char::is_control) {
        bail!("OHOS {label} SDK metadata must be 1-64 plain-text UTF-16 code units");
    }
    Ok(value)
}

fn resolve_oh_package_name(override_name: Option<&str>, package: &HostPackage) -> Result<String> {
    let name = override_name.unwrap_or(&package.name);
    validate_oh_package_name(name)?;
    Ok(name.to_string())
}

pub(crate) fn validate_oh_package_name(name: &str) -> Result<()> {
    let correction = "use `package` or `@group/package`: start each segment with a lowercase letter, use only lowercase letters/digits plus `-`/`_` (and `.` in package), do not end with punctuation, and keep the full name at most 128 characters";
    if name.is_empty() || name.len() > 128 {
        bail!("invalid OHPM package name `{name}`; {correction}");
    }
    let (group, package) = if let Some(scoped) = name.strip_prefix('@') {
        if name.matches('@').count() != 1 || scoped.matches('/').count() != 1 {
            bail!("invalid scoped OHPM package name `{name}`; {correction}");
        }
        let (group, package) = scoped
            .split_once('/')
            .expect("exactly one scoped package separator was checked");
        (Some(group), package)
    } else {
        if name.contains('@') || name.contains('/') {
            bail!("invalid OHPM package name `{name}`; {correction}");
        }
        (None, name)
    };
    if [".har", ".tgz", ".tar", ".tar.gz"]
        .iter()
        .any(|suffix| package.ends_with(suffix))
    {
        bail!("invalid OHPM package name `{name}`; archive suffixes such as .har, .tgz, .tar, and .tar.gz are reserved; {correction}");
    }
    if let Some(group) = group {
        validate_oh_package_segment(group, false, name, correction)?;
    }
    validate_oh_package_segment(package, true, name, correction)?;
    Ok(())
}

fn validate_oh_package_segment(
    segment: &str,
    allow_dot: bool,
    full_name: &str,
    correction: &str,
) -> Result<()> {
    let mut chars = segment.chars();
    if !chars.next().is_some_and(|ch| ch.is_ascii_lowercase())
        || !segment.chars().all(|ch| {
            ch.is_ascii_lowercase()
                || ch.is_ascii_digit()
                || ch == '-'
                || ch == '_'
                || (allow_dot && ch == '.')
        })
        || !segment
            .chars()
            .last()
            .is_some_and(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        || is_arkts_reserved_word(segment)
    {
        bail!("invalid OHPM package name `{full_name}`; {correction}");
    }
    Ok(())
}

fn is_arkts_reserved_word(value: &str) -> bool {
    uniffi_bindgen::interface::is_arkts_reserved_identifier(value)
}

pub(crate) fn derive_module_name(package_name: &str) -> Result<String> {
    validate_oh_package_name(package_name)?;
    let package = package_name.rsplit('/').next().unwrap_or(package_name);
    let mut module = String::new();
    let mut last_was_separator = false;
    for ch in package.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            module.push(ch);
            last_was_separator = false;
        } else if !last_was_separator {
            module.push('_');
            last_was_separator = true;
        }
    }
    while module.ends_with('_') {
        module.pop();
    }
    validate_module_name(&module)?;
    Ok(module)
}

pub(crate) fn validate_module_name(name: &str) -> Result<()> {
    if name.len() > 128
        || !name
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic())
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        bail!(
            "invalid Harmony module name `{name}`; use a letter first and only ASCII letters, digits, or underscores (for example `my_native_module`)"
        );
    }
    Ok(())
}

pub(crate) fn validate_package_version(version: &str) -> Result<()> {
    if utf16_len(version) > 128 {
        bail!("invalid OHPM package version `{version}`; keep the version at most 128 UTF-16 code units");
    }
    let parsed = cargo_metadata::semver::Version::parse(version).with_context(|| {
        format!(
            "invalid OHPM package version `{version}`; use a complete semantic version such as `1.2.3`"
        )
    })?;
    const JS_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    if parsed.major > JS_MAX_SAFE_INTEGER
        || parsed.minor > JS_MAX_SAFE_INTEGER
        || parsed.patch > JS_MAX_SAFE_INTEGER
    {
        bail!("invalid OHPM package version `{version}`; major, minor, and patch must not exceed JavaScript's maximum safe integer {JS_MAX_SAFE_INTEGER}");
    }
    Ok(())
}

fn utf16_len(value: &str) -> usize {
    value.encode_utf16().count()
}

pub(crate) fn resolve_device_types(overrides: &[String]) -> Result<Vec<String>> {
    let values = if overrides.is_empty() {
        DEFAULT_DEVICE_TYPES
            .iter()
            .map(|value| (*value).to_string())
            .collect::<Vec<_>>()
    } else {
        overrides.to_vec()
    };
    let mut deduped = Vec::new();
    for value in values {
        if !ALLOWED_DEVICE_TYPES.contains(&value.as_str()) {
            bail!(
                "unsupported Harmony device type `{value}`; use one of default, phone, tablet, 2in1, tv, wearable, or car (`default` is the OpenHarmony universal device type)"
            );
        }
        if !deduped.contains(&value) {
            deduped.push(value);
        }
    }
    Ok(deduped)
}

fn resolve_har_out(
    override_path: Option<&Utf8Path>,
    artifact_root: &Utf8Path,
    package: &HostPackage,
    package_count: usize,
) -> Utf8PathBuf {
    if let Some(path) = override_path {
        return path.to_path_buf();
    }
    if package_count <= 1 {
        artifact_root.join(format!("{}.har", package.name))
    } else {
        artifact_root.join(format!("{}-{}.har", package.name, package.lib_target_name))
    }
}

fn package_stage_dir(
    artifact_root: &Utf8Path,
    package: &HostPackage,
    package_count: usize,
) -> Utf8PathBuf {
    let base = artifact_root.join("package");
    if package_count <= 1 {
        base
    } else {
        base.join(&package.name)
    }
}

fn stage_har_package(
    package_dist_dir: &Utf8Path,
    package_dir: &Utf8Path,
    lib_target_name: &str,
    metadata: &OhosPackageMetadata,
    skip_libs: bool,
) -> Result<()> {
    stage_package(
        package_dist_dir,
        package_dir,
        lib_target_name,
        metadata,
        PackageKind::Har,
        false,
        skip_libs,
        None,
    )
}

fn stage_package(
    package_dist_dir: &Utf8Path,
    package_dir: &Utf8Path,
    lib_target_name: &str,
    metadata: &OhosPackageMetadata,
    kind: PackageKind,
    integrated_hsp: bool,
    skip_libs: bool,
    expected_so_inventory: Option<&HspSoInventory>,
) -> Result<()> {
    if path_entry_exists(package_dir)? {
        bail!(
            "fresh OHOS package staging path unexpectedly exists without its creation-time witness: {package_dir}"
        );
    }
    std::fs::create_dir_all(package_dir.join("src/main/ets"))
        .with_context(|| format!("creating OHOS package staging src/main/ets in {package_dir}"))?;
    let facade_path = package_dist_dir.join("native-facade.ets");
    let index_path = package_dist_dir.join("Index.ets");
    let declarations_path = package_dist_dir.join("Index.d.ets");
    let index_bytes = read_verified_regular_file(&index_path)?;
    let declarations = read_verified_regular_file(&declarations_path)?;
    let index_source = String::from_utf8(index_bytes)
        .with_context(|| format!("OHOS package entry is not UTF-8: {index_path}"))?;
    std::fs::write(package_dir.join("Index.ets"), index_source)
        .with_context(|| format!("writing OHOS package Index.ets in {package_dir}"))?;
    let declarations_destination = package_dir.join("Index.d.ets");
    let mut declarations_output = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&declarations_destination)?;
    declarations_output.write_all(&declarations)?;
    declarations_output.sync_all()?;
    std::fs::write(
        package_dir.join("src/main/ets/native-facade.ets"),
        std::fs::read(&facade_path)
            .with_context(|| format!("reading generated Harmony facade {facade_path}"))?,
    )
    .with_context(|| format!("writing OHOS package native facade in {package_dir}"))?;
    let support_source = package_dist_dir.join("support");
    if path_entry_exists(&support_source)? {
        let metadata = std::fs::symlink_metadata(&support_source)
            .with_context(|| format!("reading generated Harmony support {support_source}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("generated Harmony support must be a real directory: {support_source}");
        }
        // Index.ets is consumed both at the OHPM package root and as the
        // compiled native facade under src/main/ets.  Keep its package-local
        // `./support/*` imports valid in both physical reader locations.
        copy_dir_recursive(&support_source, &package_dir.join("support"))?;
        copy_dir_recursive(&support_source, &package_dir.join("src/main/ets/support"))?;
    }
    std::fs::write(
        package_dir.join("build-profile.json5"),
        render_build_profile_json5(metadata, kind, integrated_hsp)?,
    )
    .with_context(|| format!("writing OHOS package build-profile.json5 in {package_dir}"))?;
    std::fs::write(
        package_dir.join("src/main/module.json5"),
        render_module_json5(metadata, kind)?,
    )
    .with_context(|| format!("writing OHOS package module.json5 in {package_dir}"))?;
    stage_native_types(package_dist_dir, package_dir, lib_target_name, metadata)?;
    copy_dist_to_package_libs(package_dist_dir, &package_dir.join("libs"), skip_libs)?;
    let native_components = if let Some(expected) = expected_so_inventory {
        expected
            .values()
            .flat_map(|libraries| libraries.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    } else {
        collect_staged_native_components(&package_dir.join("libs"))?
    };
    std::fs::write(
        package_dir.join("oh-package.json5"),
        render_oh_package_json5(metadata, lib_target_name, &native_components, kind)?,
    )
    .with_context(|| format!("writing OHOS package metadata in {package_dir}"))?;
    Ok(())
}

fn render_oh_package_json5(
    metadata: &OhosPackageMetadata,
    lib_target_name: &str,
    native_components: &[String],
    kind: PackageKind,
) -> Result<String> {
    let native = native_lib_filename(lib_target_name);
    let native_types_dir = native.trim_end_matches(".so");
    let mut value = serde_json::Map::new();
    value.insert("name".into(), Value::String(metadata.name.clone()));
    value.insert("version".into(), Value::String(metadata.version.clone()));
    if let Some(description) = &metadata.description {
        value.insert("description".into(), Value::String(description.clone()));
    }
    value.insert("main".into(), Value::String("Index.ets".into()));
    value.insert("types".into(), Value::String("Index.d.ets".into()));
    if kind == PackageKind::Hsp {
        value.insert("packageType".into(), Value::String("InterfaceHar".into()));
    }
    if let Some(author) = &metadata.author {
        value.insert("author".into(), Value::String(author.clone()));
    }
    if let Some(license) = &metadata.license {
        value.insert("license".into(), Value::String(license.clone()));
    }
    value.insert(
        "dependencies".into(),
        serde_json::json!({
            native.clone(): format!("file:./src/main/cpp/types/{native_types_dir}")
        }),
    );
    if let Some(sdk) = &metadata.sdk {
        value.insert(
            "compatibleSdkVersion".into(),
            Value::String(sdk.version.clone()),
        );
        value.insert(
            "compatibleSdkType".into(),
            Value::String(sdk.sdk_type.as_str().into()),
        );
        let components = native_components
            .iter()
            .map(|name| {
                serde_json::json!({
                    "name": name,
                    "compatibleSdkVersion": sdk.version,
                    "compatibleSdkType": sdk.sdk_type.as_str(),
                })
            })
            .collect::<Vec<_>>();
        value.insert("nativeComponents".into(), Value::Array(components));
    }
    value.insert("obfuscated".into(), Value::Bool(false));
    value.insert("artifactType".into(), Value::String("original".into()));
    render_json5(Value::Object(value))
}

fn collect_staged_native_components(libs_dir: &Utf8Path) -> Result<Vec<String>> {
    fn visit(path: &Utf8Path, names: &mut BTreeSet<String>) -> Result<()> {
        for entry in std::fs::read_dir(path)
            .with_context(|| format!("reading staged Harmony native libraries in {path}"))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let child = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "staged Harmony native library path is not utf8: {}",
                    path.display()
                )
            })?;
            if file_type.is_symlink() {
                bail!("refusing symlink in staged Harmony native libraries: {child}");
            }
            if file_type.is_dir() {
                visit(&child, names)?;
            } else if file_type.is_file() && child.extension() == Some("so") {
                let name = child
                    .file_name()
                    .context("staged Harmony native library has no file name")?;
                names.insert(name.to_string());
            }
        }
        Ok(())
    }

    let mut names = BTreeSet::new();
    visit(libs_dir, &mut names)?;
    Ok(names.into_iter().collect())
}

fn render_module_json5(metadata: &OhosPackageMetadata, kind: PackageKind) -> Result<String> {
    let mut module = serde_json::Map::new();
    module.insert("name".into(), Value::String(metadata.module_name.clone()));
    module.insert(
        "type".into(),
        Value::String(
            match kind {
                PackageKind::Har => "har",
                PackageKind::Hsp => "shared",
            }
            .into(),
        ),
    );
    module.insert(
        "deviceTypes".into(),
        serde_json::to_value(&metadata.device_types)?,
    );
    if kind == PackageKind::Hsp {
        module.insert("deliveryWithInstall".into(), Value::Bool(true));
    }
    render_json5(serde_json::json!({ "module": module }))
}

fn render_build_profile_json5(
    metadata: &OhosPackageMetadata,
    kind: PackageKind,
    integrated_hsp: bool,
) -> Result<String> {
    let mut build_option = serde_json::json!({
        "resOptions": {
            "copyCodeResource": {
                "enable": false
            }
        }
    });
    if kind == PackageKind::Hsp {
        let object = build_option
            .as_object_mut()
            .expect("build option literal is an object");
        object.insert("generateSharedTgz".into(), Value::Bool(true));
        object.insert(
            "nativeLib".into(),
            serde_json::json!({ "excludeSoFromInterfaceHar": true }),
        );
        if integrated_hsp {
            object.insert(
                "arkOptions".into(),
                serde_json::json!({ "integratedHsp": true }),
            );
        }
    }
    let device_types = serde_json::to_value(&metadata.device_types)?;
    let mut target = serde_json::json!({
        "name": "default",
        "config": {
            "deviceType": device_types
        }
    });
    if kind == PackageKind::Hsp {
        let runtime = metadata
            .sdk
            .as_ref()
            .context("HSP module profile requires compatible SDK metadata")?
            .sdk_type
            .as_str();
        target
            .as_object_mut()
            .expect("target literal is an object")
            .insert("runtimeOS".into(), Value::String(runtime.into()));
    }
    render_json5(serde_json::json!({
        "apiType": "stageMode",
        "buildOption": build_option,
        "targets": [target]
    }))
}

fn render_native_types_package_json5(
    metadata: &OhosPackageMetadata,
    native: &str,
) -> Result<String> {
    let mut value = serde_json::Map::new();
    value.insert("name".into(), Value::String(native.to_string()));
    value.insert("types".into(), Value::String("./index.d.ts".into()));
    value.insert("version".into(), Value::String(metadata.version.clone()));
    if let Some(description) = &metadata.description {
        value.insert("description".into(), Value::String(description.clone()));
    }
    render_json5(Value::Object(value))
}

fn render_json5(value: Value) -> Result<String> {
    let mut out = serde_json::to_string_pretty(&value)?;
    out.push('\n');
    Ok(out)
}

fn stage_native_types(
    package_dist_dir: &Utf8Path,
    package_dir: &Utf8Path,
    lib_target_name: &str,
    metadata: &OhosPackageMetadata,
) -> Result<()> {
    let native = native_lib_filename(lib_target_name);
    let type_dir = package_dir
        .join("src/main/cpp/types")
        .join(native.trim_end_matches(".so"));
    std::fs::create_dir_all(&type_dir)
        .with_context(|| format!("creating OHOS native type dependency {type_dir}"))?;
    let dts_src = package_dist_dir.join("native-facade.d.ts");
    require_regular_source_file(&dts_src)?;
    std::fs::copy(&dts_src, type_dir.join("index.d.ts"))
        .with_context(|| format!("copying OHOS native types {dts_src} -> {type_dir}/index.d.ts"))?;
    std::fs::write(
        type_dir.join("oh-package.json5"),
        render_native_types_package_json5(metadata, &native)?,
    )
    .with_context(|| format!("writing OHOS native type dependency metadata in {type_dir}"))?;
    Ok(())
}

fn copy_dist_to_package_libs(
    package_dist_dir: &Utf8Path,
    libs_dir: &Utf8Path,
    skip_libs: bool,
) -> Result<()> {
    if !package_dist_dir.exists() {
        bail!("OHOS dist dir does not exist: {package_dist_dir}");
    }
    if path_entry_exists(libs_dir)? {
        bail!(
            "fresh OHOS package libs path unexpectedly exists without its creation-time witness: {libs_dir}"
        );
    }
    std::fs::create_dir_all(libs_dir)
        .with_context(|| format!("creating OHOS package libs dir {libs_dir}"))?;
    let dts_src = package_dist_dir.join("native-facade.d.ts");
    require_regular_source_file(&dts_src)?;
    std::fs::copy(&dts_src, libs_dir.join("index.d.ts"))
        .with_context(|| format!("copying OHOS types {dts_src} -> {libs_dir}/index.d.ts"))?;
    if skip_libs {
        return Ok(());
    }
    for entry in std::fs::read_dir(package_dist_dir)
        .with_context(|| format!("reading OHOS dist dir {package_dist_dir}"))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let source = Utf8PathBuf::from_path_buf(entry.path())
            .map_err(|p| anyhow::anyhow!("OHOS dist entry is not utf8: {}", p.display()))?;
        let Some(name) = source.file_name() else {
            continue;
        };
        // Component facades and consumer-owned support are already staged as
        // ArkTS sources.  They are not native ABI directories and must not be
        // copied beneath `libs`, where HSP inventory validation accepts only
        // ABI directories containing `.so` files.
        if name == "native-facade.d.ts" || name == "component-facades" || name == "support" {
            continue;
        }
        if file_type.is_symlink() {
            bail!("refusing to copy symlink from OHOS dist into HAR staging: {source}");
        }
        if file_type.is_dir() {
            copy_dir_recursive(&source, &libs_dir.join(name))?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NormalizedArchiveEntry {
    path: Utf8PathBuf,
    data: Option<Vec<u8>>,
}

fn validate_compiled_har(har: &Utf8Path, metadata: &OhosPackageMetadata) -> Result<()> {
    let entries = read_har_entries(har)?;
    let module = entries
        .iter()
        .find(|entry| entry.path == Utf8Path::new("package/src/main/module.json"))
        .and_then(|entry| entry.data.as_ref())
        .with_context(|| {
            format!(
                "Hvigor HAR {har} is missing compiled package/src/main/module.json; source-only archives are not final HAR artifacts"
            )
        })?;
    let module: Value = serde_json::from_slice(module)
        .with_context(|| format!("parsing compiled module.json from {har}"))?;
    if module["module"]["name"] != metadata.module_name {
        bail!(
            "compiled HAR module name mismatch: expected `{}`, found {}",
            metadata.module_name,
            module["module"]["name"]
        );
    }
    let package = entries
        .iter()
        .find(|entry| entry.path == Utf8Path::new("package/oh-package.json5"))
        .and_then(|entry| entry.data.as_ref())
        .with_context(|| format!("Hvigor HAR {har} is missing package/oh-package.json5"))?;
    let package: Value = serde_json::from_slice(package)
        .with_context(|| format!("parsing oh-package.json5 from {har}"))?;
    if package["name"] != metadata.name {
        bail!(
            "compiled HAR package name mismatch: expected `{}`, found {}",
            metadata.name,
            package["name"]
        );
    }
    Ok(())
}

fn publish_compiled_har_with<F>(
    source_har: &Utf8Path,
    source_package_json: &Utf8Path,
    har_out: &Utf8Path,
    package_dir: &Utf8Path,
    validate: F,
) -> Result<()>
where
    F: FnOnce(&Utf8Path) -> Result<()>,
{
    let mut entries = read_har_entries(source_har)?;
    patch_compiled_package_metadata(&mut entries, source_package_json)?;
    publish_archive_entries_with(entries, har_out, Some(package_dir), validate)
}

fn patch_compiled_package_metadata(
    entries: &mut [NormalizedArchiveEntry],
    source_package_json: &Utf8Path,
) -> Result<()> {
    let source: Value = serde_json::from_str(
        &std::fs::read_to_string(source_package_json)
            .with_context(|| format!("reading staged package metadata {source_package_json}"))?,
    )
    .with_context(|| format!("parsing staged package metadata {source_package_json}"))?;
    let compiled_entry = entries
        .iter_mut()
        .find(|entry| entry.path == Utf8Path::new("package/oh-package.json5"))
        .context("compiled HAR is missing package/oh-package.json5")?;
    let compiled_data = compiled_entry
        .data
        .as_ref()
        .context("compiled HAR package metadata is not a regular file")?;
    let mut compiled: Value =
        serde_json::from_slice(compiled_data).context("parsing compiled HAR package metadata")?;
    let compiled_object = compiled
        .as_object_mut()
        .context("compiled HAR package metadata is not an object")?;
    for key in [
        "compatibleSdkVersion",
        "compatibleSdkType",
        "obfuscated",
        "nativeComponents",
    ] {
        if let Some(value) = source.get(key) {
            compiled_object.insert(key.to_string(), value.clone());
        } else {
            compiled_object.remove(key);
        }
    }
    let mut rendered = serde_json::to_vec_pretty(&compiled)?;
    rendered.push(b'\n');
    compiled_entry.data = Some(rendered);
    Ok(())
}

fn validate_final_native_components(har: &Utf8Path) -> Result<()> {
    let entries = read_har_entries(har)?;
    let staged = entries
        .iter()
        .filter(|entry| entry.data.is_some())
        .filter_map(|entry| {
            (entry.path.starts_with("package/libs") && entry.path.extension() == Some("so"))
                .then(|| entry.path.file_name().map(str::to_string))
                .flatten()
        })
        .collect::<BTreeSet<_>>();
    let package = entries
        .iter()
        .find(|entry| entry.path == Utf8Path::new("package/oh-package.json5"))
        .and_then(|entry| entry.data.as_ref())
        .context("final HAR is missing package/oh-package.json5")?;
    let package: Value = serde_json::from_slice(package)?;
    let declared = package
        .get("nativeComponents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|component| component.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if staged != declared {
        bail!(
            "final HAR nativeComponents do not match packaged SO files: staged={staged:?}, declared={declared:?}"
        );
    }
    Ok(())
}

fn publish_archive_entries_with<F>(
    entries: Vec<NormalizedArchiveEntry>,
    har_out: &Utf8Path,
    package_dir: Option<&Utf8Path>,
    validate: F,
) -> Result<()>
where
    F: FnOnce(&Utf8Path) -> Result<()>,
{
    publish_archive_entries_with_hooks(entries, har_out, package_dir, validate, |_| Ok(()))
}

fn publish_archive_entries_with_hooks<Validate, BeforePersist>(
    entries: Vec<NormalizedArchiveEntry>,
    har_out: &Utf8Path,
    package_dir: Option<&Utf8Path>,
    validate: Validate,
    before_persist: BeforePersist,
) -> Result<()>
where
    Validate: FnOnce(&Utf8Path) -> Result<()>,
    BeforePersist: FnOnce(&Utf8Path) -> Result<()>,
{
    let final_path = prepare_har_output_path(har_out, package_dir)?;
    let parent = final_path
        .parent()
        .context("resolved HAR output path has no parent")?;
    let mut temp = tempfile::Builder::new()
        .prefix("uniffi-har-")
        .suffix(".har")
        .tempfile_in(parent)
        .with_context(|| format!("creating temporary HAR beside {final_path}"))?;
    write_normalized_har(&mut temp, entries)?;
    temp.as_file_mut().flush()?;
    temp.as_file().sync_all()?;
    let temp_path = Utf8PathBuf::from_path_buf(temp.path().to_path_buf())
        .map_err(|path| anyhow::anyhow!("temporary HAR path is not utf8: {}", path.display()))?;
    validate(&temp_path)?;
    before_persist(&temp_path)?;
    temp.persist(final_path.as_std_path())
        .map_err(|error| error.error)
        .with_context(|| format!("atomically publishing HAR to {final_path}"))?;
    if let Ok(parent_file) = std::fs::File::open(parent) {
        let _ = parent_file.sync_all();
    }
    Ok(())
}

fn prepare_har_output_path(
    har_out: &Utf8Path,
    package_dir: Option<&Utf8Path>,
) -> Result<Utf8PathBuf> {
    let cwd = Utf8PathBuf::from_path_buf(env::current_dir()?)
        .map_err(|path| anyhow::anyhow!("current directory is not utf8: {}", path.display()))?;
    prepare_har_output_path_at(har_out, package_dir, &cwd)
}

fn prepare_har_output_path_at(
    har_out: &Utf8Path,
    package_dir: Option<&Utf8Path>,
    cwd: &Utf8Path,
) -> Result<Utf8PathBuf> {
    let requested = if har_out.is_absolute() {
        har_out.to_path_buf()
    } else {
        cwd.join(har_out)
    };
    let file_name = requested
        .file_name()
        .with_context(|| format!("HAR output path has no file name: {har_out}"))?;
    if std::fs::symlink_metadata(&requested).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("refusing to write HAR through symlink output path: {har_out}");
    }

    let canonical_package = package_dir
        .map(|package_dir| {
            let package_dir = if package_dir.is_absolute() {
                package_dir.to_path_buf()
            } else {
                cwd.join(package_dir)
            };
            if std::fs::symlink_metadata(&package_dir)
                .is_ok_and(|metadata| metadata.file_type().is_symlink())
            {
                bail!("OHOS package staging directory must not be a symlink: {package_dir}");
            }
            canonicalize_allow_missing(&package_dir)
        })
        .transpose()?;
    let prospective_final = canonicalize_allow_missing(&requested)?;
    if canonical_package
        .as_ref()
        .is_some_and(|package_dir| prospective_final.starts_with(package_dir))
    {
        bail!("HAR output path must not be inside its package staging directory: {har_out}");
    }

    let parent = requested
        .parent()
        .filter(|parent| !parent.as_str().is_empty())
        .unwrap_or(cwd);
    std::fs::create_dir_all(parent).with_context(|| format!("creating HAR output dir {parent}"))?;
    let canonical_parent = parent
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing HAR output dir {parent}"))?;
    let final_path = canonical_parent.join(file_name);
    if std::fs::symlink_metadata(&final_path)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        bail!("refusing to write HAR through symlink output path: {har_out}");
    }
    if let Some(canonical_package) = canonical_package {
        if final_path.starts_with(&canonical_package) {
            bail!("HAR output path must not be inside its package staging directory: {har_out}");
        }
    }
    Ok(final_path)
}

fn read_har_entries(har: &Utf8Path) -> Result<Vec<NormalizedArchiveEntry>> {
    let bytes = read_verified_regular_file_bounded(
        har,
        MAX_HSP_ARCHIVE_COMPRESSED_BYTES,
        "compiled HAR archive",
    )?;
    let mut entries =
        read_bounded_targz_entries(&bytes, true, Some("package"), "compiled HAR archive")?
            .into_iter()
            .map(|(path, data)| (Utf8PathBuf::from(path), data))
            .collect::<BTreeMap<_, _>>();
    add_archive_parent_directories(&mut entries);
    if !entries.contains_key(Utf8Path::new("package")) {
        bail!("HAR {har} does not have a package/ archive root");
    }
    Ok(entries
        .into_iter()
        .map(|(path, data)| NormalizedArchiveEntry { path, data })
        .collect())
}

fn add_archive_parent_directories(entries: &mut BTreeMap<Utf8PathBuf, Option<Vec<u8>>>) {
    let paths = entries.keys().cloned().collect::<Vec<_>>();
    for path in paths {
        let mut parent = path.parent();
        while let Some(dir) = parent {
            if dir.as_str().is_empty() {
                break;
            }
            entries.entry(dir.to_path_buf()).or_insert(None);
            parent = dir.parent();
        }
    }
}

fn write_normalized_har<W: Write>(
    writer: W,
    mut entries: Vec<NormalizedArchiveEntry>,
) -> Result<()> {
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let encoder = GzBuilder::new()
        .mtime(0)
        .write(writer, Compression::default());
    let mut archive = Builder::new(encoder);
    for entry in entries {
        let mut header = Header::new_gnu();
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(0);
        header.set_username("")?;
        header.set_groupname("")?;
        match entry.data {
            Some(data) => {
                header.set_entry_type(EntryType::Regular);
                header.set_mode(0o644);
                header.set_size(data.len() as u64);
                header.set_cksum();
                archive.append_data(&mut header, &entry.path, Cursor::new(data))?;
            }
            None => {
                header.set_entry_type(EntryType::Directory);
                header.set_mode(0o755);
                header.set_size(0);
                header.set_cksum();
                archive.append_data(&mut header, &entry.path, Cursor::new(Vec::<u8>::new()))?;
            }
        }
    }
    let encoder = archive.into_inner()?;
    encoder.finish()?;
    Ok(())
}

fn build_arch(
    options: &BuildOptions,
    package: &HostPackage,
    ohos_ndk: &str,
    target_dir: &Utf8Path,
    dist_dir: &Utf8Path,
    type_dir: &Utf8Path,
    native_path_policy: &NativePathPolicy,
    arch: Arch,
    explicit_package_arg: bool,
    required_core_so: Option<&RequiredCoreSo>,
) -> Result<BTreeSet<String>> {
    let invocation_root = dist_dir.parent().unwrap_or(dist_dir);
    let native_path_policy = native_path_policy.for_invocation(&[
        (type_dir, "/uniffi/build/types"),
        (invocation_root, "/uniffi/build/invocation"),
        (dist_dir, "/uniffi/build/dist"),
    ])?;
    // Cargo discovers `.cargo/config.toml` from its process working directory,
    // not from `--manifest-path`.  Resolve wrappers from the exact directory
    // inherited by the child command so config and environment precedence stay
    // identical to Cargo's own invocation.
    let cargo_config_cwd = env::current_dir()?;
    let build_environment = ohos_env(
        ohos_ndk,
        arch,
        type_dir,
        &package.lib_target_name,
        options.bisheng,
        options.soname.as_deref(),
        &native_path_policy.remaps,
        &cargo_config_cwd,
        OsStr::new(&options.cargo_bin),
        &options.cargo_args,
    )?;
    let mut command = Command::new(&options.cargo_bin);
    command
        .args(cargo_args_for_arch(
            options,
            package,
            arch,
            explicit_package_arg,
        ))
        .env("CARGO_TARGET_DIR", target_dir.as_str())
        .env("CARGO_INCREMENTAL", "0")
        .stdout(Stdio::piped());
    build_environment.apply(&mut command);

    let mut child = command.spawn().with_context(|| {
        format!(
            "spawning OHOS cargo build for package {} target {}",
            package.name,
            arch.rust_target()
        )
    })?;
    let artifacts = collect_build_messages(child.stdout.take(), ohos_ndk)?;
    let status = child.wait()?;
    if !status.success() {
        bail!(
            "OHOS cargo build failed for package {} target {} with status {status}",
            package.name,
            arch.rust_target()
        );
    }

    if should_skip_artifact_copy(options.skip_libs) {
        return Ok(BTreeSet::new());
    }
    let profile = if options.release { "release" } else { "debug" };
    let expected = target_dir
        .join(arch.rust_target())
        .join(profile)
        .join(native_lib_filename(&package.lib_target_name));
    if !artifacts.paths.contains(&expected) {
        bail!(
            "Cargo messages did not declare the required OHOS bridge artifact at {expected}; refusing to reuse a stale target file"
        );
    }
    let bridge_provenance = artifacts.cargo_provenance.get(&expected).with_context(|| {
        format!("required OHOS bridge artifact lacks Cargo provenance: {expected}")
    })?;
    if bridge_provenance.package_id != package.cargo_package_id
        || bridge_provenance.target_name != package.lib_target_name
    {
        bail!(
            "OHOS bridge Cargo provenance mismatch at {expected}: expected package `{}` target `{}`, found {bridge_provenance:?}",
            package.cargo_package_id,
            package.lib_target_name
        );
    }
    let arch_dist = dist_dir.join(arch.dist_dir());
    let artifacts = filter_artifacts(artifacts, options.copy_static, Some(expected));
    let mut expected_so = BTreeSet::new();
    for artifact in &artifacts.paths {
        if artifact.extension() != Some("so") {
            continue;
        }
        let name = artifact
            .file_name()
            .with_context(|| format!("OHOS Cargo artifact has no file name: {artifact}"))?
            .to_string();
        // Read and bound-check every native input before publication.  The
        // inventory tracks names only; no per-file digest is persisted or
        // compared across generated outputs.
        let _ = read_verified_regular_file_bounded(
            artifact,
            MAX_HSP_ARCHIVE_MEMBER_BYTES,
            "OHOS Cargo native artifact",
        )?;
        if !expected_so.insert(name.clone()) {
            bail!(
                "OHOS Cargo emitted multiple native artifacts named `{name}` for ABI `{}`; refusing an ambiguous HSP SO contract",
                arch.dist_dir()
            );
        }
    }
    let bridge = native_lib_filename(&package.lib_target_name);
    if !expected_so.contains(&bridge) {
        bail!(
            "OHOS Cargo messages did not declare the required bridge `{bridge}` for ABI `{}`",
            arch.dist_dir()
        );
    }
    if let Some(core) = required_core_so {
        let core_artifacts = artifacts
            .cargo_provenance
            .iter()
            .filter(|(path, provenance)| {
                path.file_name() == Some(core.name.as_str())
                    && provenance.package_id == core.package_id
            })
            .map(|(path, _)| path)
            .collect::<Vec<_>>();
        if core_artifacts.len() != 1 || !expected_so.contains(&core.name) {
            bail!(
                "OHOS Cargo messages did not uniquely declare the required downstream core `{}` from package `{}` for ABI `{}`: {core_artifacts:?}",
                core.name,
                core.package_id,
                arch.dist_dir()
            );
        }
    }
    copy_artifacts(&artifacts, &arch_dist)?;
    copy_ohos_cxx_shared(ohos_ndk, arch, &arch_dist)?;
    let cxx = arch_dist.join("libc++_shared.so");
    let _ = read_verified_regular_file_bounded(
        &cxx,
        MAX_HSP_ARCHIVE_MEMBER_BYTES,
        "OHOS libc++ runtime",
    )?;
    if !expected_so.insert("libc++_shared.so".to_string()) {
        bail!(
            "OHOS Cargo artifacts unexpectedly claimed reserved runtime `libc++_shared.so` for ABI `{}`",
            arch.dist_dir()
        );
    }
    Ok(expected_so)
}

fn resolve_required_core_so(options: &BuildOptions) -> Result<Option<RequiredCoreSo>> {
    let Some(manifest) = &options.core_manifest_path else {
        return Ok(None);
    };
    let metadata = MetadataCommand::new()
        .cargo_path(&options.cargo_bin)
        .manifest_path(manifest.as_std_path())
        .exec()
        .with_context(|| format!("resolving downstream core native identity from {manifest}"))?;
    let package = metadata
        .root_package()
        .with_context(|| format!("downstream core manifest has no root package: {manifest}"))?;
    let target = package
        .targets
        .iter()
        .find(|target| target.kind.iter().any(|kind| kind.to_string() == "cdylib"))
        .or_else(|| {
            package
                .targets
                .iter()
                .find(|target| target.kind.iter().any(|kind| kind.to_string() == "lib"))
        })
        .with_context(|| format!("downstream core package {} has no lib target", package.name))?;
    Ok(Some(RequiredCoreSo {
        package_id: package.id.repr.clone(),
        name: native_lib_filename(&target.name),
    }))
}

fn cargo_args_for_arch(
    options: &BuildOptions,
    package: &HostPackage,
    arch: Arch,
    explicit_package_arg: bool,
) -> Vec<String> {
    let mut args = Vec::new();
    if arch == Arch::LoongArch64 {
        args.push("+nightly".to_string());
    }
    args.push(
        if options.zigbuild {
            "zigbuild"
        } else {
            "build"
        }
        .to_string(),
    );
    if arch == Arch::LoongArch64 {
        args.extend(["-Z".to_string(), "build-std".to_string()]);
    }
    args.extend([
        "--manifest-path".to_string(),
        package.manifest_path.to_string(),
        "--target".to_string(),
        arch.rust_target().to_string(),
        "--message-format=json-render-diagnostics".to_string(),
    ]);
    if options.release && !options.cargo_args.iter().any(|arg| arg == "--release") {
        args.push("--release".to_string());
    }
    if explicit_package_arg && !has_package_arg(&options.cargo_args) {
        args.push("-p".to_string());
        args.push(format!("{}@{}", package.name, package.version));
    }
    args.extend(options.cargo_args.iter().cloned());
    args
}

fn collect_build_messages(
    stdout: Option<std::process::ChildStdout>,
    ohos_ndk: &str,
) -> Result<BuiltArtifacts> {
    let mut artifacts = BTreeSet::new();
    let mut cargo_provenance = BTreeMap::new();
    let Some(stdout) = stdout else {
        return Ok(BuiltArtifacts {
            paths: artifacts,
            cargo_provenance,
        });
    };
    for message in Message::parse_stream(BufReader::new(stdout)) {
        match message? {
            Message::CompilerArtifact(artifact) => {
                let provenance = CargoArtifactProvenance {
                    package_id: artifact.package_id.repr.clone(),
                    target_name: artifact.target.name.clone(),
                };
                for filename in artifact.filenames {
                    let path =
                        Utf8PathBuf::from_path_buf(filename.into_std_path_buf()).map_err(|p| {
                            anyhow::anyhow!("artifact path is not utf8: {}", p.display())
                        })?;
                    if artifact_extension_is_native(&path)
                        && (path.extension() == Some("so") || !is_rust_intermediate_lib(&path))
                    {
                        artifacts.insert(path.clone());
                        if let Some(previous) =
                            cargo_provenance.insert(path.clone(), provenance.clone())
                        {
                            if previous != provenance {
                                bail!("Cargo reported conflicting native artifact provenance for {path}");
                            }
                        }
                    }
                }
            }
            Message::BuildScriptExecuted(script) => {
                for path in resolve_dependency_libraries(&script, ohos_ndk)? {
                    artifacts.insert(path);
                }
            }
            Message::CompilerMessage(message) => {
                if let Some(rendered) = message.message.rendered {
                    eprint!("{rendered}");
                }
            }
            _ => {}
        }
    }
    Ok(BuiltArtifacts {
        paths: artifacts,
        cargo_provenance,
    })
}

fn artifact_extension_is_native(path: &Utf8Path) -> bool {
    matches!(path.extension(), Some("so") | Some("a"))
}

fn is_rust_intermediate_lib(path: &Utf8Path) -> bool {
    let path = path.as_str();
    path.contains("/target/") && path.contains("/deps/")
}

fn resolve_dependency_libraries(script: &BuildScript, ohos_ndk: &str) -> Result<Vec<Utf8PathBuf>> {
    if script.linked_libs.is_empty() || script.linked_paths.is_empty() {
        return Ok(Vec::new());
    }
    let ohos_sysroot = Path::new(ohos_ndk)
        .join("native")
        .join("sysroot")
        .join("usr")
        .join("lib");
    let hos_sysroot =
        resolve_hos_ndk(ohos_ndk).map(|p| p.join("native").join("sysroot").join("usr").join("lib"));
    let lib_names = script
        .linked_libs
        .iter()
        .map(|lib| {
            lib.strip_prefix("dylib=")
                .map(|name| format!("lib{name}.so"))
                .unwrap_or_else(|_| format!("lib{lib}.so"))
        })
        .collect::<Vec<_>>();
    let mut out = Vec::new();
    for path in &script.linked_paths {
        let raw = path.as_str();
        let native = raw.strip_prefix("native=").unwrap_or(raw);
        let base = Utf8Path::new(native);
        if base.starts_with(&ohos_sysroot) {
            continue;
        }
        if let Some(hos_sysroot) = &hos_sysroot {
            if base.starts_with(hos_sysroot) {
                continue;
            }
        }
        if is_rust_intermediate_lib(base) || (!base.is_dir() && !base.is_file()) {
            continue;
        }
        let base = base
            .canonicalize_utf8()
            .unwrap_or_else(|_| base.to_path_buf());
        if base.is_file() {
            out.push(base);
        } else {
            for lib_name in &lib_names {
                let lib = base.join(lib_name);
                if lib.exists() {
                    out.push(lib);
                }
            }
        }
    }
    Ok(out)
}

fn filter_artifacts(
    artifacts: BuiltArtifacts,
    copy_static: bool,
    _expected: Option<Utf8PathBuf>,
) -> BuiltArtifacts {
    let paths: BTreeSet<Utf8PathBuf> = artifacts
        .paths
        .into_iter()
        .filter(|path| copy_static || path.extension() != Some("a"))
        .collect();
    let cargo_provenance = artifacts
        .cargo_provenance
        .into_iter()
        .filter(|(path, _)| paths.contains(path))
        .collect();
    BuiltArtifacts {
        paths,
        cargo_provenance,
    }
}

fn should_skip_artifact_copy(skip_libs: bool) -> bool {
    skip_libs
}

fn copy_artifacts(artifacts: &BuiltArtifacts, arch_dist: &Utf8Path) -> Result<()> {
    if artifacts.paths.is_empty() {
        bail!("OHOS build produced no native artifacts");
    }
    if path_entry_exists(arch_dist)? {
        bail!(
            "fresh OHOS arch staging path unexpectedly exists without its creation-time witness: {arch_dist}"
        );
    }
    std::fs::create_dir_all(arch_dist)
        .with_context(|| format!("creating OHOS arch dist dir {arch_dist}"))?;
    for artifact in &artifacts.paths {
        let Some(file_name) = artifact.file_name() else {
            continue;
        };
        std::fs::copy(artifact, arch_dist.join(file_name))
            .with_context(|| format!("copying OHOS artifact {artifact}"))?;
    }
    Ok(())
}

fn ohos_cxx_shared_candidates(ohos_ndk: &str, arch: Arch) -> Vec<Utf8PathBuf> {
    let lib_dir = Utf8Path::new(ohos_ndk)
        .join("native")
        .join("llvm")
        .join("lib")
        .join(arch.c_target());
    vec![
        lib_dir.join("libc++_shared.so"),
        lib_dir.join("c++").join("libc++_shared.so"),
    ]
}

fn ohos_llvm_tool_path(ohos_ndk: &str, tool: &str) -> Utf8PathBuf {
    let base = Utf8Path::new(ohos_ndk)
        .join("native")
        .join("llvm")
        .join("bin")
        .join(tool);
    #[cfg(windows)]
    {
        let executable = base.with_extension("exe");
        if executable.exists() {
            return executable;
        }
    }
    base
}

fn copy_ohos_cxx_shared(ohos_ndk: &str, arch: Arch, arch_dist: &Utf8Path) -> Result<()> {
    let Some(source) = ohos_cxx_shared_candidates(ohos_ndk, arch)
        .into_iter()
        .find(|path| path.exists())
    else {
        bail!(
            "OHOS libc++_shared.so not found for {}; expected it under {}/native/llvm/lib/{}",
            arch.c_target(),
            ohos_ndk,
            arch.c_target()
        );
    };
    let dest = arch_dist.join("libc++_shared.so");
    std::fs::copy(&source, &dest)
        .with_context(|| format!("copying OHOS C++ runtime {source} -> {dest}"))?;
    Ok(())
}

fn ohos_env(
    ohos_ndk: &str,
    arch: Arch,
    _type_dir: &Utf8Path,
    build_target_name: &str,
    bisheng: bool,
    soname: Option<&str>,
    path_remaps: &[PathRemap],
    cargo_config_cwd: &Path,
    cargo_bin: &OsStr,
    cargo_args: &[String],
) -> Result<OhosBuildEnvironment> {
    let hos_ndk = resolve_hos_ndk(ohos_ndk);
    let toolchain_root = if bisheng {
        let hos_ndk = hos_ndk
            .as_ref()
            .context("HOS_NDK_HOME or sibling hms SDK is required when --bisheng is enabled")?;
        hos_ndk.join("native").join("BiSheng")
    } else {
        Path::new(ohos_ndk).join("native").join("llvm")
    };
    let toolchain = resolve_toolchain_paths(&toolchain_root);
    let sysroot = Path::new(ohos_ndk).join("native").join("sysroot");
    let mut compile_flags = vec![
        format!("--target={}", arch.c_target()),
        format!("--sysroot={}", sysroot.to_string_lossy()),
        "-D__MUSL__".to_string(),
    ];
    if arch == Arch::Arm32 {
        compile_flags.extend([
            "-march=armv7-a".to_string(),
            "-mfloat-abi=softfp".to_string(),
            "-mtune=generic-armv7-a".to_string(),
            "-mthumb".to_string(),
        ]);
    }
    let mut link_flags = compile_flags.clone();
    if let Some(hos_ndk) = &hos_ndk {
        append_hms_link_flags(&mut link_flags, hos_ndk, arch);
    }
    if let Some(soname) = soname {
        link_flags.push(format!("-Wl,-soname,{}", normalize_soname(soname)?));
    }

    let append_args = target_rustc_append_args(&toolchain.cc, &link_flags, path_remaps)?;

    let mut path = env::var("PATH").unwrap_or_default();
    let separator = if cfg!(target_os = "windows") {
        ";"
    } else {
        ":"
    };
    path = format!("{}{}{}", toolchain.bin_dir, separator, path);

    let mut envs = HashMap::from([
        ("LIBCLANG_PATH".to_string(), toolchain.lib_dir.clone()),
        ("CLANG_PATH".to_string(), toolchain.cxx.clone()),
        (
            format!("CXXSTDLIB_{}", arch.rust_link_target()),
            "c++".to_string(),
        ),
        ("TARGET_CC".to_string(), toolchain.cc.clone()),
        ("TARGET_CXX".to_string(), toolchain.cxx.clone()),
        ("TARGET_RANLIB".to_string(), toolchain.ranlib.clone()),
        ("TARGET_AR".to_string(), toolchain.ar.clone()),
        ("TARGET_AS".to_string(), toolchain.llvm_as.clone()),
        ("TARGET_LD".to_string(), toolchain.ld.clone()),
        ("TARGET_STRIP".to_string(), toolchain.strip.clone()),
        ("TARGET_OBJDUMP".to_string(), toolchain.objdump.clone()),
        ("TARGET_OBJCOPY".to_string(), toolchain.objcopy.clone()),
        ("TARGET_NM".to_string(), toolchain.nm.clone()),
        ("PATH".to_string(), path),
        (
            "NAPI_BUILD_TARGET_NAME".to_string(),
            build_target_name.to_string(),
        ),
        ("OPENCV_CLANG_ARGS".to_string(), link_flags.join(" ")),
        ("DEP_ATOMIC".to_string(), "clang_rt.builtins".to_string()),
    ]);

    envs.insert("CARGO_INCREMENTAL".into(), "0".into());
    apply_ohos_compile_env(&mut envs, arch, &compile_flags);

    if let Some(hos_ndk) = &hos_ndk {
        apply_hms_include_env(&mut envs, hos_ndk, arch);
    }
    let wrapper = env::current_exe()?.into_os_string();
    let configured_wrappers = cargo_rustc_wrappers(cargo_config_cwd, cargo_bin, cargo_args)?;
    for (name, existing) in [
        ("build.rustc-wrapper", configured_wrappers.normal.as_ref()),
        (
            "build.rustc-workspace-wrapper",
            configured_wrappers.workspace.as_ref(),
        ),
    ] {
        if let Some(existing) = existing.map(OsString::as_os_str) {
            if same_executable(existing, &wrapper)? {
                bail!(
                    "{name} resolves to the current UniFFI executable; refusing a recursive OHOS rustc wrapper chain"
                );
            }
        }
    }
    // Cargo's order is normal wrapper -> workspace wrapper -> rustc.  Replacing
    // only the normal wrapper with UniFFI and invoking the resolved normal
    // wrapper from inside it preserves that order; Cargo continues to insert
    // the configured workspace wrapper as the first compiler argument.
    let inner_wrapper = configured_wrappers.normal;
    Ok(OhosBuildEnvironment {
        vars: envs,
        wrapper,
        inner_wrapper,
        append_args: encode_wrapper_args(&append_args)?,
    })
}

#[derive(Debug, Eq, PartialEq)]
struct CargoRustcWrappers {
    normal: Option<OsString>,
    workspace: Option<OsString>,
}

fn cargo_rustc_wrappers(
    cwd: &Path,
    cargo_bin: &OsStr,
    cargo_args: &[String],
) -> Result<CargoRustcWrappers> {
    let environment = env::vars_os().collect::<Vec<_>>();
    cargo_rustc_wrappers_with_options(
        cwd,
        cargo_bin,
        cargo_args,
        &environment,
        cargo_config2::ResolveOptions::default(),
    )
}

fn cargo_rustc_wrappers_with_options(
    cwd: &Path,
    cargo_bin: &OsStr,
    cargo_args: &[String],
    environment: &[(OsString, OsString)],
    options: cargo_config2::ResolveOptions,
) -> Result<CargoRustcWrappers> {
    // Cargo's wrapper precedence is config files -> CARGO_BUILD_* -> command
    // line `--config` -> RUSTC_*.  Exclude all four wrapper variables from
    // cargo-config2 and reproduce those two distinct environment layers around
    // the CLI overlays below.
    let config_environment = environment
        .iter()
        .filter(|(name, _)| !is_cargo_wrapper_environment(name))
        .cloned()
        .collect::<Vec<_>>();
    let options = options
        .cargo(cargo_bin.to_os_string())
        .env(config_environment);
    let config = cargo_config2::Config::load_with_options(cwd, options).with_context(|| {
        format!(
            "loading Cargo configuration for OHOS wrapper chaining from {}",
            cwd.display()
        )
    })?;
    let mut wrappers = CargoRustcWrappers {
        normal: config.build.rustc_wrapper.map(PathBuf::into_os_string),
        workspace: config
            .build
            .rustc_workspace_wrapper
            .map(PathBuf::into_os_string),
    };
    if let Some(value) =
        cargo_wrapper_environment_value(environment, "CARGO_BUILD_RUSTC_WRAPPER", cwd)
    {
        wrappers.normal = value;
    }
    if let Some(value) =
        cargo_wrapper_environment_value(environment, "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", cwd)
    {
        wrappers.workspace = value;
    }
    apply_cargo_config_overlays(&mut wrappers, cwd, cargo_args)?;
    if let Some(value) = cargo_wrapper_environment_value(environment, "RUSTC_WRAPPER", cwd) {
        wrappers.normal = value;
    }
    if let Some(value) =
        cargo_wrapper_environment_value(environment, "RUSTC_WORKSPACE_WRAPPER", cwd)
    {
        wrappers.workspace = value;
    }
    Ok(wrappers)
}

fn is_cargo_wrapper_environment(name: &OsStr) -> bool {
    [
        "RUSTC_WRAPPER",
        "CARGO_BUILD_RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
        "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER",
    ]
    .iter()
    .any(|candidate| name == OsStr::new(candidate))
}

fn cargo_wrapper_environment_value(
    environment: &[(OsString, OsString)],
    name: &str,
    cwd: &Path,
) -> Option<Option<OsString>> {
    environment
        .iter()
        .find(|(key, _)| key == OsStr::new(name))
        .map(|(_, value)| resolve_wrapper_program(value.clone(), cwd))
}

fn apply_cargo_config_overlays(
    wrappers: &mut CargoRustcWrappers,
    cwd: &Path,
    cargo_args: &[String],
) -> Result<()> {
    let mut index = 0;
    while index < cargo_args.len() {
        let argument = &cargo_args[index];
        let overlay = if argument == "--config" {
            index += 1;
            cargo_args
                .get(index)
                .with_context(|| "Cargo --config requires a KEY=VALUE or file path")?
                .as_str()
        } else if let Some(overlay) = argument.strip_prefix("--config=") {
            if overlay.is_empty() {
                bail!("Cargo --config= requires a KEY=VALUE or file path");
            }
            overlay
        } else {
            index += 1;
            continue;
        };
        apply_cargo_config_overlay(wrappers, cwd, overlay)?;
        index += 1;
    }
    Ok(())
}

fn apply_cargo_config_overlay(
    wrappers: &mut CargoRustcWrappers,
    cwd: &Path,
    overlay: &str,
) -> Result<()> {
    let (contents, definition_root, label) = if overlay.contains('=') {
        (
            overlay.to_string(),
            cwd.to_path_buf(),
            "command line".to_string(),
        )
    } else {
        let requested = PathBuf::from(overlay);
        let path = if requested.is_absolute() {
            requested
        } else {
            cwd.join(requested)
        };
        let path = std::fs::canonicalize(&path)
            .with_context(|| format!("canonicalizing Cargo --config file {}", path.display()))?;
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("reading Cargo --config file {}", path.display()))?;
        // Cargo intentionally gives paths inside a CLI config file the same
        // two-level root convention as `.cargo/config.toml`.
        let definition_root = path
            .parent()
            .and_then(Path::parent)
            .unwrap_or(cwd)
            .to_path_buf();
        let label = path.display().to_string();
        (contents, definition_root, label)
    };
    let config: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("parsing Cargo --config overlay from {label}"))?;
    if let Some(build) = config.get("build").and_then(toml::Value::as_table) {
        if let Some(value) = build.get("rustc-wrapper") {
            let value = value.as_str().with_context(|| {
                format!("Cargo build.rustc-wrapper from {label} must be a string")
            })?;
            wrappers.normal = resolve_wrapper_program(value.into(), &definition_root);
        }
        if let Some(value) = build.get("rustc-workspace-wrapper") {
            let value = value.as_str().with_context(|| {
                format!("Cargo build.rustc-workspace-wrapper from {label} must be a string")
            })?;
            wrappers.workspace = resolve_wrapper_program(value.into(), &definition_root);
        }
    }
    Ok(())
}

fn resolve_wrapper_program(value: OsString, root: &Path) -> Option<OsString> {
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(&value);
    if path.is_absolute() || path.components().count() <= 1 {
        Some(value)
    } else {
        Some(root.join(path).into_os_string())
    }
}

fn target_rustc_append_args(
    linker: &str,
    link_flags: &[String],
    path_remaps: &[PathRemap],
) -> Result<Vec<String>> {
    let mut args = vec![format!("-Clinker={linker}")];
    args.extend(link_flags.iter().map(|flag| format!("-Clink-arg={flag}")));
    // rustc applies the last matching remap.  NativePathPolicy stores the
    // requested specific-to-broad order, so reverse it here to append broad
    // rules first and let the most specific managed rule win last.
    for remap in path_remaps.iter().rev() {
        if remap.source.as_str().contains('\x1f') || remap.destination.contains(['\x1f', '=']) {
            bail!("OHOS Rust path remap cannot be encoded safely: {remap:?}");
        }
        args.push(format!(
            "--remap-path-prefix={}={}",
            remap.source, remap.destination
        ));
    }
    Ok(args)
}

fn encode_wrapper_args(args: &[String]) -> Result<String> {
    if args
        .iter()
        .any(|arg| arg.is_empty() || arg.contains('\x1f'))
    {
        bail!("OHOS rustc wrapper arguments contain an unsupported empty or unit-separator token");
    }
    Ok(args.join("\x1f"))
}

fn same_executable(left: &OsStr, right: &OsStr) -> Result<bool> {
    if left == right {
        return Ok(true);
    }
    let Some(left) = resolve_executable(left)? else {
        return Ok(false);
    };
    let Some(right) = resolve_executable(right)? else {
        return Ok(false);
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let left_metadata = std::fs::metadata(&left)?;
        let right_metadata = std::fs::metadata(&right)?;
        Ok(left_metadata.dev() == right_metadata.dev()
            && left_metadata.ino() == right_metadata.ino())
    }
    #[cfg(windows)]
    {
        Ok(windows_file_information(&left)?.identity == windows_file_information(&right)?.identity)
    }
    #[cfg(not(any(unix, windows)))]
    {
        bail!(
            "executable file identity is unsupported on this host; refusing an unverifiable OHOS wrapper chain"
        )
    }
}

fn resolve_executable(value: &OsStr) -> Result<Option<PathBuf>> {
    let path = PathBuf::from(value);
    let has_separator = path.components().count() > 1;
    let candidate = if path.is_absolute() || has_separator {
        Some(if path.is_absolute() {
            path
        } else {
            env::current_dir()?.join(path)
        })
    } else {
        env::var_os("PATH").and_then(|paths| {
            env::split_paths(&paths)
                .flat_map(|directory| executable_path_candidates(&directory, &path))
                .find(|candidate| candidate.is_file())
        })
    };
    match candidate {
        Some(candidate) if candidate.exists() => Ok(Some(
            std::fs::canonicalize(candidate)
                .context("canonicalizing configured rustc wrapper executable")?,
        )),
        _ => Ok(None),
    }
}

fn executable_path_candidates(directory: &Path, executable: &Path) -> Vec<PathBuf> {
    let base = directory.join(executable);
    #[cfg(windows)]
    {
        if executable.extension().is_none() {
            let extensions =
                env::var_os("PATHEXT").unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"));
            let mut candidates = vec![base.clone()];
            for extension in extensions.to_string_lossy().split(';') {
                let extension = extension.trim().trim_start_matches('.');
                if extension.is_empty() {
                    continue;
                }
                let mut candidate = base.clone();
                candidate.set_extension(extension);
                candidates.push(candidate);
            }
            return candidates;
        }
    }
    vec![base]
}

fn normalize_soname(soname: &str) -> Result<String> {
    if soname.contains(".so.")
        && soname
            .split(".so.")
            .nth(1)
            .is_some_and(|s| s.chars().next().is_some_and(|ch| ch.is_ascii_digit()))
    {
        bail!("SONAME with version number is not supported: {soname}");
    }
    if soname.starts_with("lib") && soname.contains(".so") {
        Ok(soname.to_string())
    } else if soname.ends_with(".so") {
        Ok(format!("lib{soname}"))
    } else {
        Ok(format!("lib{soname}.so"))
    }
}

fn resolve_toolchain_paths(root: &Path) -> ToolchainPaths {
    let bin_dir = root.join("bin");
    let lib_dir = root.join("lib");
    let to_string = |path: PathBuf| path.to_string_lossy().to_string();
    let tool = |name: &str| to_string(bin_dir.join(name));
    ToolchainPaths {
        ranlib: tool("llvm-ranlib"),
        ar: tool("llvm-ar"),
        cc: tool("clang"),
        cxx: tool("clang++"),
        llvm_as: tool("llvm-as"),
        ld: tool("ld.lld"),
        strip: tool("llvm-strip"),
        objdump: tool("llvm-objdump"),
        objcopy: tool("llvm-objcopy"),
        nm: tool("llvm-nm"),
        bin_dir: to_string(bin_dir),
        lib_dir: to_string(lib_dir),
    }
}

fn resolve_hos_ndk(ohos_ndk: &str) -> Option<PathBuf> {
    let sibling = Path::new(ohos_ndk).parent()?.join("hms");
    if sibling.exists() {
        Some(sibling)
    } else {
        env::var_os("HOS_NDK_HOME").map(PathBuf::from)
    }
}

fn append_hms_link_flags(flags: &mut Vec<String>, hos_ndk: &Path, arch: Arch) {
    let lib = hos_ndk
        .join("native")
        .join("sysroot")
        .join("usr")
        .join("lib")
        .join(arch.c_target());
    if lib.exists() {
        flags.push(format!("-L{}", lib.to_string_lossy()));
        flags.push(format!("-Wl,-rpath-link,{}", lib.to_string_lossy()));
    }
}

fn apply_hms_include_env(envs: &mut HashMap<String, String>, hos_ndk: &Path, arch: Arch) {
    let include = hos_ndk
        .join("native")
        .join("sysroot")
        .join("usr")
        .join("include");
    if !include.exists() {
        return;
    }
    let include_flag = format!("-I{}", include.to_string_lossy());
    append_env_with_flag(envs, "TARGET_CFLAGS", &include_flag);
    append_env_with_flag(envs, "TARGET_CXXFLAGS", &include_flag);
    let bindgen_target = arch.rust_target().replace('-', "_");
    append_env_with_flag(
        envs,
        &format!("BINDGEN_EXTRA_CLANG_ARGS_{bindgen_target}"),
        &include_flag,
    );
    append_env_with_flag(
        envs,
        &format!(
            "BINDGEN_EXTRA_CLANG_ARGS_{}",
            bindgen_target.to_ascii_uppercase()
        ),
        &include_flag,
    );
}

fn apply_ohos_compile_env(
    envs: &mut HashMap<String, String>,
    arch: Arch,
    compile_flags: &[String],
) {
    let compile_flags = compile_flags.join(" ");
    append_env_with_flag(envs, "TARGET_CFLAGS", &compile_flags);
    append_env_with_flag(envs, "TARGET_CXXFLAGS", &compile_flags);
    let bindgen_target = arch.rust_target().replace('-', "_");
    append_env_with_flag(
        envs,
        &format!("BINDGEN_EXTRA_CLANG_ARGS_{bindgen_target}"),
        &compile_flags,
    );
    append_env_with_flag(
        envs,
        &format!(
            "BINDGEN_EXTRA_CLANG_ARGS_{}",
            bindgen_target.to_ascii_uppercase()
        ),
        &compile_flags,
    );
}

fn append_env_with_flag(envs: &mut HashMap<String, String>, key: &str, append: &str) {
    let current = envs
        .get(key)
        .cloned()
        .unwrap_or_else(|| env::var(key).unwrap_or_default());
    let merged = if current.is_empty() {
        append.to_string()
    } else {
        format!("{current} {append}")
    };
    envs.insert(key.to_string(), merged);
}

fn native_lib_filename(lib_target_name: &str) -> String {
    format!("lib{lib_target_name}.so")
}

fn require_regular_source_file(path: &Utf8Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("reading generated Harmony source {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("generated Harmony source must be a regular file: {path}");
    }
    Ok(())
}

fn require_generated_ark_files(
    generated_source_root: &Utf8Path,
    generated_package_root: &Utf8Path,
) -> Result<()> {
    for name in ["Index.ets", "Index.d.ets"] {
        require_regular_source_file(&generated_source_root.join(name))?;
    }
    require_regular_source_file(&generated_package_root.join("native/index.d.ts"))?;
    Ok(())
}

fn generated_source_root(options: &BuildOptions) -> Result<Utf8PathBuf> {
    options
        .additional_source_roots
        .iter()
        .find(|(label, _)| label == "generated-bindings")
        .map(|(_, path)| path.clone())
        .with_context(|| "OHOS host build requires the generated-bindings package source root")
}

fn generated_package_root(options: &BuildOptions) -> Result<Utf8PathBuf> {
    options
        .additional_source_roots
        .iter()
        .find(|(label, _)| label == "generated-package-root")
        .map(|(_, path)| path.clone())
        .with_context(|| "OHOS host build requires the generated package root")
}

fn emit_index_d_ts(
    dist_dir: &Utf8Path,
    generated_source_root: &Utf8Path,
    generated_package_root: &Utf8Path,
    _lib_target_name: &str,
) -> Result<()> {
    let index_source = generated_source_root.join("Index.ets");
    let declarations_source = generated_source_root.join("Index.d.ets");
    let native_declarations_source = generated_package_root.join("native/index.d.ts");
    require_regular_source_file(&index_source)?;
    require_regular_source_file(&declarations_source)?;
    require_regular_source_file(&native_declarations_source)?;
    let index = std::fs::read(&index_source)
        .with_context(|| format!("reading generated Harmony entrypoint {index_source}"))?;
    let declarations = std::fs::read(&declarations_source)
        .with_context(|| format!("reading generated Harmony declarations {declarations_source}"))?;
    let native_declarations = std::fs::read(&native_declarations_source).with_context(|| {
        format!("reading generated Harmony native declarations {native_declarations_source}")
    })?;
    std::fs::write(dist_dir.join("Index.ets"), &index)
        .context("writing generated Harmony package source")?;
    std::fs::write(dist_dir.join("Index.d.ets"), &declarations)
        .context("writing generated Harmony package declarations")?;
    // HAR/HSP staging has a native module reader for this declaration file;
    // it is sourced from the package's deterministic native/index.d.ts, not
    // from the public ArkTS declaration surface.
    std::fs::write(dist_dir.join("native-facade.d.ts"), native_declarations)
        .context("writing generated Harmony native declarations")?;
    std::fs::write(dist_dir.join("native-facade.ets"), &index)
        .context("writing generated Harmony native facade input")?;
    let support_source = generated_source_root.join("support");
    if path_entry_exists(&support_source)? {
        let metadata = std::fs::symlink_metadata(&support_source)
            .with_context(|| format!("reading generated Harmony support {support_source}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("generated Harmony support must be a real directory: {support_source}");
        }
        copy_dir_recursive(&support_source, &dist_dir.join("support"))
            .context("copying generated Harmony support into package dist")?;
    }
    Ok(())
}

#[cfg(test)]
mod support_staging_tests {
    use super::*;

    #[test]
    fn package_libs_exclude_ark_support_sources() {
        let temp = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(temp.path()).unwrap();
        let dist = root.join("dist");
        let libs = root.join("package/libs");
        std::fs::create_dir_all(dist.join("support")).unwrap();
        std::fs::create_dir_all(dist.join("component-facades")).unwrap();
        std::fs::create_dir_all(dist.join("arm64-v8a")).unwrap();
        std::fs::write(dist.join("native-facade.d.ts"), "export {};\n").unwrap();
        std::fs::write(dist.join("support/email.d.ts"), "export {};\n").unwrap();
        std::fs::write(dist.join("component-facades/core.ets"), "export {};\n").unwrap();
        std::fs::write(dist.join("arm64-v8a/libcore.so"), b"ELF").unwrap();

        copy_dist_to_package_libs(&dist, &libs, false).unwrap();

        assert!(libs.join("index.d.ts").is_file());
        assert!(libs.join("arm64-v8a/libcore.so").is_file());
        assert!(!libs.join("support").exists());
        assert!(!libs.join("component-facades").exists());
    }
}

#[cfg(test)]
mod ohos_compile_environment_tests {
    use super::*;

    #[test]
    fn target_compile_environment_includes_sysroot_and_preserves_existing_flags() {
        let mut envs = HashMap::from([
            ("TARGET_CFLAGS".to_string(), "-DEXISTING_C".to_string()),
            ("TARGET_CXXFLAGS".to_string(), "-DEXISTING_CXX".to_string()),
        ]);
        let flags = vec![
            "--target=aarch64-linux-ohos".to_string(),
            "--sysroot=/sdk/native/sysroot".to_string(),
            "-D__MUSL__".to_string(),
        ];

        apply_ohos_compile_env(&mut envs, Arch::Arm64, &flags);
        append_env_with_flag(&mut envs, "TARGET_CFLAGS", "-I/hms/usr/include");

        assert_eq!(
            envs.get("TARGET_CFLAGS").map(String::as_str),
            Some(
                "-DEXISTING_C --target=aarch64-linux-ohos --sysroot=/sdk/native/sysroot -D__MUSL__ -I/hms/usr/include"
            )
        );
        assert_eq!(
            envs.get("TARGET_CXXFLAGS").map(String::as_str),
            Some(
                "-DEXISTING_CXX --target=aarch64-linux-ohos --sysroot=/sdk/native/sysroot -D__MUSL__"
            )
        );
        for key in [
            "BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_ohos",
            "BINDGEN_EXTRA_CLANG_ARGS_AARCH64_UNKNOWN_LINUX_OHOS",
        ] {
            assert!(envs
                .get(key)
                .is_some_and(|value| value.contains("--sysroot=/sdk/native/sysroot")));
        }
    }
}
