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
#[cfg(test)]
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
#[cfg(test)]
use std::fs::OpenOptions;
use std::io::{Seek, Write};
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::process::Command;
use uniffi_bindgen::bindings::{generate, GenerateOptions, TargetLanguage};
use uniffi_bindgen_javascript::{FlavorTarget, HostCrateOptions};

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
    ohos_dist_dir: Option<Utf8PathBuf>,

    /// OHPM package name for generated HAR metadata (supports scoped names like `@scope/name`).
    #[clap(long = "ohos-package-name")]
    ohos_package_name: Option<String>,

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

    /// Compatible SDK type, such as HarmonyOS or OpenHarmony.
    #[clap(long = "ohos-compatible-sdk-type")]
    ohos_compatible_sdk_type: Option<String>,

    /// Supported Harmony device type. May be repeated or comma-separated.
    #[clap(long = "ohos-device-type", value_delimiter = ',')]
    ohos_device_types: Vec<String>,

    /// Final Harmony package kind. HAR remains the backward-compatible default.
    #[clap(long = "ohos-package-type", value_enum, default_value = "har")]
    ohos_package_kind: super::ohos::PackageKind,

    /// Build an app-independent integrated HSP.
    #[clap(long = "ohos-integrated-hsp")]
    ohos_integrated_hsp: bool,

    /// Host application bundleName for a non-integrated HSP.
    #[clap(long = "ohos-hsp-bundle-name")]
    ohos_hsp_bundle_name: Option<String>,

    /// Output `.har` path. Defaults to `<artifact-root>/<package>.har`.
    #[clap(long = "ohos-har-out")]
    ohos_har_out: Option<Utf8PathBuf>,

    /// Standalone runtime HSP extracted from the release tgz.
    #[clap(long = "ohos-runtime-hsp-out")]
    ohos_runtime_hsp_out: Option<Utf8PathBuf>,

    /// Standalone Interface HAR extracted from the release tgz.
    #[clap(long = "ohos-interface-har-out")]
    ohos_interface_har_out: Option<Utf8PathBuf>,

    /// Original release tgz emitted by Hvigor assembleHsp.
    #[clap(long = "ohos-tgz-out")]
    ohos_tgz_out: Option<Utf8PathBuf>,

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
    ohos_no_har: bool,

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
    ohos_skip_libs: bool,

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
struct ManagedLayout {
    package_dir: Utf8PathBuf,
    source_root: Utf8PathBuf,
    artifact_root: Utf8PathBuf,
    host_crates_root: Utf8PathBuf,
    manifest_path: Utf8PathBuf,
}

struct InvocationMirror {
    guard: super::ohos::IdentityBoundInvocationRoot,
    root: Utf8PathBuf,
    build_root: Utf8PathBuf,
}

impl InvocationMirror {
    fn new() -> Result<Self> {
        let guard = super::ohos::IdentityBoundInvocationRoot::create("uniffi-artifacts-invocation")
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

fn canonicalize_invocation_output(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let path = resolve_cwd_path(path)?;
    match path.canonicalize_utf8() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .with_context(|| format!("invocation output has no resolvable parent: {path}"))?;
            let name = path
                .file_name()
                .with_context(|| format!("invocation output has no file name: {path}"))?;
            Ok(canonicalize_invocation_output(parent)?.join(name))
        }
        Err(error) => {
            Err(error).with_context(|| format!("canonicalizing invocation output {path}"))
        }
    }
}

#[cfg(test)]
const MANAGED_HARMONY_OWNER_MARKER: &str = ".uniffi-managed-harmony-owner";
#[cfg(test)]
const MANAGED_HARMONY_OWNER_KIND: &str = "uniffi-managed-harmony";

#[cfg(test)]
struct ManagedHarmonyTransaction {
    _lock: std::fs::File,
    _private: tempfile::TempDir,
    private_root: Utf8PathBuf,
    public_root: Utf8PathBuf,
    manifest_path: Utf8PathBuf,
    captured_root: Option<super::ohos::OwnedTreeSnapshot>,
    captured_manifest: Option<Vec<u8>>,
    package_kind: Option<super::ohos::PackageKind>,
    integrated_hsp: bool,
    skip_libs: bool,
    expected_har_name: Option<String>,
    expected_runtime_hsp_name: Option<String>,
    expected_interface_har_name: Option<String>,
    expected_tgz_name: Option<String>,
    expected_usage_name: Option<String>,
}

#[cfg(test)]
impl ManagedHarmonyTransaction {
    fn begin(layout: &ManagedLayout, args: &mut BuildArgs) -> Result<Self> {
        std::fs::create_dir_all(&layout.artifact_root)
            .with_context(|| format!("creating managed artifact root {}", layout.artifact_root))?;
        let lock_path = layout.artifact_root.join(".uniffi-harmony.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("opening managed Harmony lock {lock_path}"))?;
        lock.lock_exclusive()
            .with_context(|| format!("locking managed Harmony output {lock_path}"))?;

        let public_root = layout.artifact_root.join("harmony");
        let captured_root = match std::fs::symlink_metadata(&public_root) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!("managed Harmony output must be a real directory: {public_root}");
                }
                Some(super::ohos::validate_owned_tree(
                    &public_root,
                    MANAGED_HARMONY_OWNER_MARKER,
                    MANAGED_HARMONY_OWNER_KIND,
                )?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading managed Harmony output {public_root}"));
            }
        };
        let captured_manifest = match std::fs::read(&layout.manifest_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("reading managed artifact manifest {}", layout.manifest_path)
                });
            }
        };

        let private = tempfile::Builder::new()
            .prefix(".uniffi-harmony-build-")
            .tempdir_in(&layout.artifact_root)
            .with_context(|| {
                format!(
                    "creating invocation-private Harmony output under {}",
                    layout.artifact_root
                )
            })?;
        let private_dir =
            Utf8PathBuf::from_path_buf(private.path().to_path_buf()).map_err(|path| {
                anyhow::anyhow!("private Harmony output is not UTF-8: {}", path.display())
            })?;
        let private_root = private_dir.join("harmony");
        std::fs::create_dir(&private_root)
            .with_context(|| format!("creating private Harmony root {private_root}"))?;

        let expected_har_name = args
            .ohos_har_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_runtime_hsp_name = args
            .ohos_runtime_hsp_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_interface_har_name = args
            .ohos_interface_har_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_tgz_name = args
            .ohos_tgz_out
            .as_ref()
            .and_then(|path| path.file_name())
            .map(str::to_string);
        let expected_usage_name = if args.ohos_package_kind == super::ohos::PackageKind::Hsp {
            let package = args
                .ohos_package_name
                .as_deref()
                .context("managed HSP package name was not derived")?;
            Some(format!("{}-HSP_USAGE.md", harmony_archive_stem(package)?))
        } else {
            None
        };
        args.ohos_dist_dir = Some(private_root.join("dist"));
        args.ohos_har_out = expected_har_name
            .as_deref()
            .map(|name| private_root.join(name));
        args.ohos_runtime_hsp_out = expected_runtime_hsp_name
            .as_deref()
            .map(|name| private_root.join(name));
        args.ohos_interface_har_out = expected_interface_har_name
            .as_deref()
            .map(|name| private_root.join(name));
        args.ohos_tgz_out = expected_tgz_name
            .as_deref()
            .map(|name| private_root.join(name));

        Ok(Self {
            _lock: lock,
            _private: private,
            private_root,
            public_root,
            manifest_path: layout.manifest_path.clone(),
            captured_root,
            captured_manifest,
            package_kind: (!args.ohos_no_har).then_some(args.ohos_package_kind),
            integrated_hsp: args.ohos_integrated_hsp,
            skip_libs: args.ohos_skip_libs,
            expected_har_name,
            expected_runtime_hsp_name,
            expected_interface_har_name,
            expected_tgz_name,
            expected_usage_name,
        })
    }

    fn private_root(&self) -> &Utf8Path {
        &self.private_root
    }

    fn commit(mut self, manifest: &[u8]) -> Result<()> {
        let previous = self.captured_root.clone();
        self.commit_with(manifest, write_file_atomically, move |path| {
            super::ohos::remove_owned_tree_for_cleanup(
                path,
                MANAGED_HARMONY_OWNER_MARKER,
                MANAGED_HARMONY_OWNER_KIND,
                previous
                    .as_ref()
                    .context("managed Harmony cleanup lacks its captured owner inventory")?,
            )
            .with_context(|| format!("removing previous managed Harmony tree {path}"))
        })
    }

    fn commit_with<WriteManifest, RemoveBackup>(
        &mut self,
        manifest: &[u8],
        write_manifest: WriteManifest,
        remove_backup: RemoveBackup,
    ) -> Result<()>
    where
        WriteManifest: Fn(&Utf8Path, &[u8]) -> Result<()>,
        RemoveBackup: Fn(&Utf8Path) -> Result<()>,
    {
        self.validate_private_root()?;
        let next = super::ohos::write_owned_tree_marker(
            &self.private_root,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
        )?;
        self.revalidate_capture()?;

        let parent = self
            .public_root
            .parent()
            .context("managed Harmony output has no parent")?;
        let backup = parent.join(format!(".harmony.uniffi-backup-{}", next.generation()));
        if backup.exists() {
            bail!("managed Harmony backup path already exists: {backup}");
        }
        let had_public = self.public_root.exists();
        if had_public {
            std::fs::rename(&self.public_root, &backup).with_context(|| {
                format!(
                    "moving previous managed Harmony tree {} to {backup}",
                    self.public_root
                )
            })?;
        }
        if let Err(error) = std::fs::rename(&self.private_root, &self.public_root) {
            if had_public {
                if let Err(restore_error) = std::fs::rename(&backup, &self.public_root) {
                    bail!(
                        "publishing private Harmony tree {} to {} failed: {error}; restoring the previous tree from {backup} also failed: {restore_error}",
                        self.private_root,
                        self.public_root
                    );
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "publishing private Harmony tree {} to {}",
                    self.private_root, self.public_root
                )
            });
        }

        // Everything before the manifest publication is reversible: the old
        // tree is still complete in `backup`, so a failure can safely restore
        // the captured tree and manifest generation.
        let prepare_commit = (|| -> Result<()> {
            if had_public {
                let backup_snapshot = super::ohos::validate_owned_tree(
                    &backup,
                    MANAGED_HARMONY_OWNER_MARKER,
                    MANAGED_HARMONY_OWNER_KIND,
                )?;
                if Some(&backup_snapshot) != self.captured_root.as_ref() {
                    bail!("previous managed Harmony backup changed before commit: {backup}");
                }
            }
            write_manifest(&self.manifest_path, manifest)
                .context("publishing managed artifact manifest")?;
            Ok(())
        })();
        if let Err(error) = prepare_commit {
            let rollback = self.rollback_swap(&backup, had_public);
            return match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "managed Harmony transaction failed: {error:#}; rollback also failed: {rollback_error:#}; inspect {} and {backup}",
                    self.public_root
                )),
            };
        }

        // The new public tree and manifest now form one committed generation.
        // Identity-bound cleanup may still fail after deleting part of the old
        // backup, so it is deliberately post-commit and must never trigger
        // rollback from a potentially incomplete backup.
        if let Ok(parent_file) = std::fs::File::open(parent) {
            let _ = parent_file.sync_all();
        }
        if had_public {
            let cleanup_snapshot = parent.join(format!(
                ".harmony.uniffi-previous-generation-{}.tar.gz",
                next.generation()
            ));
            if let Err(error) = super::ohos::snapshot_directory_for_cleanup(
                &backup,
                &cleanup_snapshot,
                "managed Harmony previous generation",
            ) {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed, but the complete previous tree was retained at {backup} because its cleanup safety snapshot could not be created: {error:#}"
                ));
            }
            if let Err(error) = remove_backup(&backup) {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed, but cleanup of previous backup {backup} failed; a complete previous-generation snapshot remains at {cleanup_snapshot}: {error:#}"
                ));
            }
            if backup.exists() {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed, but cleanup reported success without removing {backup}; a complete previous-generation snapshot remains at {cleanup_snapshot}"
                ));
            }
            if let Err(error) = std::fs::remove_file(&cleanup_snapshot) {
                return Err(anyhow::anyhow!(
                    "managed Harmony generation was committed and its previous backup was removed, but the complete cleanup safety snapshot remains at {cleanup_snapshot}: {error}"
                ));
            }
        }
        Ok(())
    }

    fn validate_private_root(&self) -> Result<()> {
        require_real_directory(&self.private_root, "private managed Harmony root")?;
        require_real_directory(
            &self.private_root.join("dist"),
            "private managed Harmony dist",
        )?;
        if self.skip_libs {
            ensure_tree_has_no_native_artifacts(&self.private_root.join("dist"))?;
        }
        let mut expected = BTreeSet::from(["dist".to_string()]);
        match self.package_kind {
            None => {}
            Some(super::ohos::PackageKind::Har) => {
                expected.insert("package".to_string());
                expected.insert(
                    self.expected_har_name
                        .clone()
                        .context("managed HAR transaction has no archive name")?,
                );
            }
            Some(super::ohos::PackageKind::Hsp) => {
                expected.insert("package".to_string());
                expected.insert("module-project".to_string());
                for value in [
                    &self.expected_runtime_hsp_name,
                    &self.expected_interface_har_name,
                    &self.expected_tgz_name,
                    &self.expected_usage_name,
                ] {
                    expected.insert(
                        value
                            .clone()
                            .context("managed HSP transaction is missing a derived output name")?,
                    );
                }
                let module_profile =
                    read_generated_json5(&self.private_root.join("package/build-profile.json5"))?;
                let project_profile = read_generated_json5(
                    &self.private_root.join("module-project/build-profile.json5"),
                )?;
                if module_profile["buildOption"]["generateSharedTgz"] != true
                    || module_profile["buildOption"]["nativeLib"]["excludeSoFromInterfaceHar"]
                        != true
                    || module_profile["buildOption"]["arkOptions"]["integratedHsp"]
                        .as_bool()
                        .unwrap_or(false)
                        != self.integrated_hsp
                    || project_profile["app"]["products"][0]["buildOption"]["strictMode"]
                        ["useNormalizedOHMUrl"]
                        .as_bool()
                        .unwrap_or(false)
                        != self.integrated_hsp
                {
                    bail!(
                        "managed HSP source project does not match the requested integration mode"
                    );
                }
            }
        }
        let actual = std::fs::read_dir(&self.private_root)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().to_string()))
            .collect::<std::io::Result<BTreeSet<_>>>()?;
        if actual != expected {
            bail!(
                "private managed Harmony tree has unexpected top-level entries: expected {expected:?}, found {actual:?}"
            );
        }
        Ok(())
    }

    fn revalidate_capture(&self) -> Result<()> {
        let current_root = if self.public_root.exists() {
            Some(super::ohos::validate_owned_tree(
                &self.public_root,
                MANAGED_HARMONY_OWNER_MARKER,
                MANAGED_HARMONY_OWNER_KIND,
            )?)
        } else {
            None
        };
        if current_root != self.captured_root {
            bail!("managed Harmony public tree changed while the transaction lock was held");
        }
        let current_manifest = match std::fs::read(&self.manifest_path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        if current_manifest != self.captured_manifest {
            bail!("managed artifact manifest changed while the Harmony transaction was running");
        }
        Ok(())
    }

    fn rollback_swap(&mut self, backup: &Utf8Path, had_public: bool) -> Result<()> {
        let backup_name = backup
            .file_name()
            .context("managed Harmony backup has no file name")?;
        let failed_new = self
            .public_root
            .parent()
            .context("managed Harmony output has no parent")?
            .join(format!(".{backup_name}.failed-new"));
        if failed_new.exists() {
            bail!("managed Harmony failed-new path already exists: {failed_new}");
        }
        std::fs::rename(&self.public_root, &failed_new)
            .context("moving failed new managed Harmony tree aside")?;
        if had_public {
            std::fs::rename(backup, &self.public_root)
                .context("restoring previous managed Harmony tree")?;
        }
        restore_file_atomically(&self.manifest_path, self.captured_manifest.as_deref())?;
        let failed_new_snapshot = super::ohos::validate_owned_tree(
            &failed_new,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
        )
        .context("validating failed new managed Harmony tree before cleanup")?;
        super::ohos::remove_owned_tree_for_cleanup(
            &failed_new,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
            &failed_new_snapshot,
        )
        .context("removing failed new managed Harmony tree after rollback")?;
        Ok(())
    }
}

const MANAGED_PACKAGE_OWNER_FILE: &str = ".uniffi-managed-package-owner.json";
const MANAGED_PACKAGE_OWNER_KIND: &str = "uniffi-managed-package";
const MANAGED_PACKAGE_OWNER_SCHEMA_VERSION: u64 = 3;
const MANAGED_PACKAGE_JOURNAL_KIND: &str = "uniffi-managed-package-transaction";
const MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION: u64 = 2;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedPackageOwner {
    owner: String,
    schema_version: u64,
    generation: String,
    state: String,
    root_identity: super::ohos::PersistentFsIdentity,
    #[serde(default)]
    root_mutation_token: Option<String>,
    entries: Vec<super::ohos::HspGenerationEntry>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ManagedPackageJournal {
    owner: String,
    schema_version: u64,
    package_identity: String,
    generation: String,
    sequence: u64,
    previous_record_name: Option<String>,
    previous_record_identity: Option<super::ohos::PersistentFsIdentity>,
    previous_record_digest: Option<String>,
    state: String,
    public_root: String,
    candidate_name: String,
    build_name: String,
    backup_name: String,
    failed_name: String,
    previous_root_identity: Option<super::ohos::PersistentFsIdentity>,
    candidate_root_identity: Option<super::ohos::PersistentFsIdentity>,
    build_root_identity: Option<super::ohos::PersistentFsIdentity>,
    backup_root_identity: Option<super::ohos::PersistentFsIdentity>,
    published_root_identity: Option<super::ohos::PersistentFsIdentity>,
    #[serde(default)]
    cleanup_snapshot_name: Option<String>,
    #[serde(default)]
    cleanup_snapshot_identity: Option<super::ohos::PersistentFsIdentity>,
    #[serde(default)]
    cleanup_snapshot_digest: Option<String>,
    #[serde(default)]
    cleanup_snapshot_len: Option<u64>,
}

/// A directory created by the managed transaction whose cleanup is always
/// tied to the filesystem identity and bounded nested inventory captured by
/// the transaction.  Unlike `TempDir`, Drop never recursively removes a path
/// merely because it has the same spelling as the directory we created.
struct ManagedOwnedDirectory {
    path: Utf8PathBuf,
    root_identity: super::ohos::PersistentFsIdentity,
    snapshot: super::ohos::OwnedTreeSnapshot,
    ephemeral_snapshot: Option<super::ohos::OwnedEphemeralTreeSnapshot>,
    ephemeral: bool,
    state: ManagedOwnedDirectoryState,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedOwnedDirectoryState {
    Armed,
    Sealed,
    Preserve,
}

impl ManagedOwnedDirectory {
    fn create(path: Utf8PathBuf) -> Result<Self> {
        Self::create_with_mode(path, false)
    }

    fn create_ephemeral(path: Utf8PathBuf) -> Result<Self> {
        Self::create_with_mode(path, true)
    }

    fn create_with_mode(path: Utf8PathBuf, ephemeral: bool) -> Result<Self> {
        std::fs::create_dir(&path)
            .with_context(|| format!("creating identity-owned managed directory {path}"))?;
        // Arm ownership immediately after create_dir.  The baseline is the
        // empty root; tool-created nested entries are never adopted during
        // failure cleanup.
        let root_identity = super::ohos::persistent_fs_identity(&path, true).with_context(|| {
            format!(
                "managed directory was created but could not be armed; preserving {path} with its durable transaction record"
            )
        })?;
        let mut budget = super::ohos::TraversalBudget::managed();
        let snapshot = super::ohos::capture_directory_for_cleanup_with_budget(
            &path,
            &mut budget,
        )
        .with_context(|| {
            format!(
                "managed directory identity was captured but its empty baseline failed; preserving {path} with its durable transaction record"
            )
        })?;
        let ephemeral_snapshot = if ephemeral {
            Some(
                super::ohos::capture_ephemeral_directory_for_cleanup_with_budget(
                    &path,
                    &mut budget,
                )?,
            )
        } else {
            None
        };
        let mut guard = Self {
            path,
            root_identity,
            snapshot,
            ephemeral_snapshot,
            ephemeral,
            state: ManagedOwnedDirectoryState::Armed,
            armed: true,
        };
        if let Err(error) = super::ohos::sync_directory(
            guard
                .path
                .parent()
                .context("identity-owned managed directory has no parent")?,
        ) {
            let cleanup = guard.cleanup();
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(anyhow::anyhow!(
                    "arming identity-owned managed directory failed: {error:#}; identity-bound cleanup also failed and the root was preserved: {cleanup:#}"
                )),
            };
        }
        Ok(guard)
    }

    #[cfg(test)]
    fn seal(&mut self) -> Result<()> {
        let mut budget = super::ohos::TraversalBudget::managed();
        self.seal_with_budget(&mut budget)
    }

    fn seal_with_budget(&mut self, budget: &mut super::ohos::TraversalBudget) -> Result<()> {
        if !self.armed {
            bail!("cannot seal a disarmed managed directory guard");
        }
        if self.state != ManagedOwnedDirectoryState::Armed {
            bail!("managed directory guard can only be sealed once");
        }
        if super::ohos::persistent_fs_identity(&self.path, true)? != self.root_identity {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "identity-owned managed directory was replaced before capture: {}",
                self.path
            );
        }
        let capture = if self.ephemeral {
            super::ohos::capture_ephemeral_directory_for_cleanup_with_budget(&self.path, budget)
                .map(|snapshot| {
                    self.ephemeral_snapshot = Some(snapshot);
                })
        } else {
            super::ohos::capture_directory_for_cleanup_with_budget(&self.path, budget).map(
                |snapshot| {
                    self.snapshot = snapshot;
                },
            )
        };
        if let Err(error) = capture {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error).with_context(|| {
                format!("sealing identity-owned managed directory {}", self.path)
            });
        }
        if super::ohos::persistent_fs_identity(&self.path, true)? != self.root_identity {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "identity-owned managed directory changed during capture: {}",
                self.path
            );
        }
        self.state = ManagedOwnedDirectoryState::Sealed;
        Ok(())
    }

    /// Install the exact destination snapshot returned by the seed copier.
    /// The copier records each object as it creates it; this method never
    /// performs a fresh whole-tree capture that could adopt an inserted or
    /// replaced pathname between copy and registration.
    #[cfg(test)]
    fn register_seeded_contents(&mut self, seeded: super::ohos::OwnedTreeSnapshot) -> Result<()> {
        let mut budget = super::ohos::TraversalBudget::managed();
        self.register_seeded_contents_with_budget(seeded, &mut budget)
    }

    fn register_seeded_contents_with_budget(
        &mut self,
        seeded: super::ohos::OwnedTreeSnapshot,
        budget: &mut super::ohos::TraversalBudget,
    ) -> Result<()> {
        if !self.armed || self.state != ManagedOwnedDirectoryState::Armed || self.ephemeral {
            bail!("managed seed registration requires an armed package candidate");
        }
        if seeded.root_identity() != &self.root_identity
            || super::ohos::persistent_fs_identity(&self.path, true)? != self.root_identity
        {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "managed candidate root changed before exact seed registration: {}",
                self.path
            );
        }
        if let Err(error) =
            super::ohos::validate_directory_capture_with_budget(&self.path, &seeded, budget)
        {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error).with_context(|| {
                format!("validating exact managed seed snapshot at {}", self.path)
            });
        }
        self.snapshot = seeded;
        Ok(())
    }

    fn remove_seeded_path(
        &mut self,
        relative: &str,
        budget: &mut super::ohos::TraversalBudget,
    ) -> Result<()> {
        if !self.armed || self.state != ManagedOwnedDirectoryState::Armed || self.ephemeral {
            bail!("managed selected-root removal requires an armed package candidate");
        }
        if let Err(error) = super::ohos::remove_owned_snapshot_path_with_budget(
            &self.path,
            &mut self.snapshot,
            relative,
            budget,
        ) {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error)
                .with_context(|| format!("removing exact seeded selected path `{relative}`"));
        }
        Ok(())
    }

    fn cleanup(&mut self) -> Result<()> {
        let mut budget = super::ohos::TraversalBudget::managed();
        self.cleanup_with_budget(&mut budget)
    }

    fn cleanup_with_budget(&mut self, budget: &mut super::ohos::TraversalBudget) -> Result<()> {
        if !self.armed {
            return Ok(());
        }
        if self.state == ManagedOwnedDirectoryState::Preserve {
            bail!("managed directory is preserved for audit: {}", self.path);
        }
        if super::ohos::persistent_fs_identity(&self.path, true)? != self.root_identity {
            self.state = ManagedOwnedDirectoryState::Preserve;
            bail!(
                "refusing to remove replacement at identity-owned managed path {}",
                self.path
            );
        }
        let result = if self.ephemeral {
            super::ohos::remove_ephemeral_directory_for_cleanup_with_budget(
                &self.path,
                self.ephemeral_snapshot
                    .as_ref()
                    .context("ephemeral managed directory lacks its capture")?,
                budget,
            )
        } else {
            super::ohos::remove_captured_directory_for_cleanup_with_budget(
                &self.path,
                &self.snapshot,
                budget,
            )
        };
        if let Err(error) = result {
            self.state = ManagedOwnedDirectoryState::Preserve;
            return Err(error);
        }
        self.armed = false;
        Ok(())
    }

    fn disarm_after_rename(&mut self) {
        self.armed = false;
    }

    fn preserve(&mut self) {
        self.state = ManagedOwnedDirectoryState::Preserve;
    }
}

impl Drop for ManagedOwnedDirectory {
    fn drop(&mut self) {
        // Explicit transaction cleanup reports errors while locks are held.
        // Drop deliberately preserves instead of retrying outside that scope.
    }
}

struct ManagedPackageTransaction {
    private: ManagedOwnedDirectory,
    build_temp: ManagedOwnedDirectory,
    private_root: Utf8PathBuf,
    public_root: Utf8PathBuf,
    public_layout: ManagedLayout,
    private_layout: ManagedLayout,
    previous_owner: Option<ManagedPackageOwner>,
    previous_owner_witness: Option<super::ohos::DurableRecordWitness>,
    captured_root: Option<super::ohos::OwnedTreeSnapshot>,
    generation: String,
    journal_parent: Utf8PathBuf,
    journal_records: Vec<super::ohos::DurableRecordWitness>,
    preserve_journals: bool,
    journal: ManagedPackageJournal,
    completed: bool,
    // Rust drops fields in declaration order.  Keep the complete union lock
    // last so every guard is finalized/preserved before another invocation can
    // acquire it.
    _locks: super::ohos::OutputLockSet,
}

fn managed_controlled_paths(root: &Utf8Path) -> Vec<(Utf8PathBuf, bool)> {
    let mut paths = vec![
        (root.join("artifact-manifest.json"), false),
        (root.join("src/ffi"), true),
        (root.join("artifacts"), true),
    ];
    for name in [
        "index.web.ts",
        "index.mini-program.ts",
        "index.node.ts",
        "index.electron.ts",
    ] {
        paths.push((root.join("src").join(name), false));
    }
    paths
}

fn capture_managed_entries_with_budget(
    source_root: &Utf8Path,
    public_root: &Utf8Path,
    budget: &mut super::ohos::TraversalBudget,
) -> Result<Vec<super::ohos::HspGenerationEntry>> {
    let mut entries = Vec::new();
    for ((source, is_directory), (public, _)) in managed_controlled_paths(source_root)
        .into_iter()
        .zip(managed_controlled_paths(public_root))
    {
        match std::fs::symlink_metadata(&source) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink()
                    || (is_directory && !metadata.is_dir())
                    || (!is_directory && !metadata.is_file())
                {
                    bail!("managed package controlled path has an unsafe type: {source}");
                }
                entries.push(super::ohos::capture_generic_generation_entry_with_budget(
                    &source,
                    &public,
                    is_directory,
                    budget,
                )?);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("reading managed controlled path {source}"));
            }
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(entries)
}

fn managed_embedded_owner_path(root: &Utf8Path) -> Utf8PathBuf {
    root.join(MANAGED_PACKAGE_OWNER_FILE)
}

fn managed_owner_path(root: &Utf8Path) -> Utf8PathBuf {
    let stable_root = canonicalize_invocation_output(root).unwrap_or_else(|_| root.to_path_buf());
    let digest = managed_package_digest(&stable_root);
    stable_root
        .parent()
        .unwrap_or(&stable_root)
        .join(format!(".uniffi-managed-package-owner-{digest}.json"))
}

fn parse_managed_owner(root: &Utf8Path) -> Result<ManagedPackageOwner> {
    let sidecar = managed_owner_path(root);
    let marker = if super::ohos::path_entry_exists(&sidecar)? {
        sidecar
    } else {
        managed_embedded_owner_path(root)
    };
    let bytes = super::ohos::read_verified_regular_file_bounded(
        &marker,
        16 * 1024 * 1024,
        "managed package owner record",
    )?;
    let owner: ManagedPackageOwner = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing managed package owner record {marker}"))?;
    if owner.owner != MANAGED_PACKAGE_OWNER_KIND
        || !matches!(
            owner.schema_version,
            2 | MANAGED_PACKAGE_OWNER_SCHEMA_VERSION
        )
        || owner.generation.is_empty()
        || !matches!(owner.state.as_str(), "prepared" | "committed")
    {
        bail!("unsupported managed package owner record: {marker}");
    }
    Ok(owner)
}

#[cfg(test)]
fn validate_managed_owner(root: &Utf8Path, owner: &ManagedPackageOwner) -> Result<()> {
    let mut budget = super::ohos::TraversalBudget::managed();
    validate_managed_owner_with_budget(root, owner, &mut budget)
}

fn validate_managed_owner_with_budget(
    root: &Utf8Path,
    owner: &ManagedPackageOwner,
    budget: &mut super::ohos::TraversalBudget,
) -> Result<()> {
    if owner.state != "committed" {
        bail!(
            "managed package has no committed final record (state `{}`): {}",
            owner.state,
            managed_owner_path(root)
        );
    }
    if super::ohos::persistent_fs_identity(root, true)? != owner.root_identity {
        bail!("managed package root identity changed: {root}");
    }
    if owner.schema_version >= 3 {
        let current_root_token = super::ohos::directory_mutation_token_for_owner(root)?;
        if owner.root_mutation_token.as_deref() != Some(current_root_token.as_str()) {
            bail!("managed package root mutation witness changed: {root}");
        }
    }
    let mut actual_paths = BTreeSet::new();
    for (path, kind) in managed_controlled_paths(root) {
        if super::ohos::path_entry_exists(&path)? {
            actual_paths.insert((canonicalize_invocation_output(&path)?, kind));
        }
    }
    let owner_paths = owner
        .entries
        .iter()
        .map(|entry| {
            Ok((
                Utf8PathBuf::from(&entry.path),
                match entry.kind.as_str() {
                    "directory" => true,
                    "file" => false,
                    other => bail!("invalid managed owner entry kind `{other}`"),
                },
            ))
        })
        .collect::<Result<BTreeSet<_>>>()?;
    if owner_paths.len() != owner.entries.len() {
        bail!("managed package owner record contains duplicate controlled paths");
    }
    if owner_paths != actual_paths {
        bail!("managed package controlled path set changed from its committed owner record");
    }
    for entry in &owner.entries {
        if owner.schema_version >= 3 {
            super::ohos::validate_hsp_generation_entry_with_budget(
                entry,
                Utf8Path::new(&entry.path),
                budget,
            )
            .context("validating managed package controlled entry")?;
        } else {
            super::ohos::validate_hsp_generation_entry_content_with_budget(
                entry,
                Utf8Path::new(&entry.path),
                budget,
            )
            .context("validating legacy managed package controlled entry")?;
        }
    }
    Ok(())
}

fn validate_legacy_managed_package_with_budget(
    root: &Utf8Path,
    budget: &mut super::ohos::TraversalBudget,
) -> Result<()> {
    let controlled = managed_controlled_paths(root)
        .into_iter()
        .map(|(path, kind)| Ok((super::ohos::path_entry_exists(&path)?, path, kind)))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(exists, path, kind)| exists.then_some((path, kind)))
        .collect::<Vec<_>>();
    if controlled.is_empty() {
        return Ok(());
    }
    let manifest = root.join("artifact-manifest.json");
    let bytes = super::ohos::read_verified_regular_file_bounded(
        &manifest,
        16 * 1024 * 1024,
        "legacy managed artifact manifest",
    )
    .with_context(|| {
        format!(
            "refusing unowned managed controlled paths under {root}; a compatible artifact-manifest.json is required for one-time adoption"
        )
    })?;
    let value: serde_json::Value = serde_json::from_slice(&bytes)?;
    if !matches!(value["schemaVersion"].as_u64(), Some(2 | 3))
        || value["generator"] != "uniffi-bindgen-javascript"
    {
        bail!("refusing incompatible legacy managed package at {root}");
    }
    // Capturing every controlled root proves it is bounded, regular and free
    // of symlink/special-file sentinels before the one-time owner migration.
    let _ = capture_managed_entries_with_budget(root, root, budget)?;
    Ok(())
}

fn managed_package_digest(public_root: &Utf8Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(public_root.as_str().as_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn new_managed_generation() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{:x}-{:x}-{:x}",
        std::process::id(),
        nanos,
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

fn managed_journal_prefix(digest: &str) -> String {
    format!(".uniffi-managed-package-transaction-{digest}-")
}

fn managed_journal_record_path(parent: &Utf8Path, journal: &ManagedPackageJournal) -> Utf8PathBuf {
    parent.join(format!(
        "{}{}-{:020}-{}.json",
        managed_journal_prefix(&journal.package_identity),
        journal.generation,
        journal.sequence,
        journal.state
    ))
}

fn validate_managed_journal(
    journal: &ManagedPackageJournal,
    package_identity: &str,
    public_root: &Utf8Path,
) -> Result<()> {
    if journal.owner != MANAGED_PACKAGE_JOURNAL_KIND
        || journal.schema_version != MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION
        || journal.package_identity != package_identity
        || journal.public_root != public_root.as_str()
        || journal.generation.is_empty()
        || (journal.sequence == 0
            && (journal.previous_record_name.is_some()
                || journal.previous_record_identity.is_some()
                || journal.previous_record_digest.is_some()))
        || (journal.sequence > 0
            && (journal.previous_record_name.is_none()
                || journal.previous_record_identity.is_none()
                || journal.previous_record_digest.is_none()))
    {
        bail!("managed package transaction journal identity/schema mismatch");
    }
    let public_name = public_root
        .file_name()
        .context("managed package transaction public root has no file name")?;
    let expected_names = [
        format!(
            ".uniffi-managed-package-{package_identity}-{}-next",
            journal.generation
        ),
        format!(
            ".uniffi-managed-package-{package_identity}-{}-build",
            journal.generation
        ),
        format!(
            ".uniffi-managed-package-{package_identity}-{}-{public_name}-backup",
            journal.generation
        ),
        format!(
            ".uniffi-managed-package-{package_identity}-{}-{public_name}-failed",
            journal.generation
        ),
    ];
    for (name, expected) in [
        &journal.candidate_name,
        &journal.build_name,
        &journal.backup_name,
        &journal.failed_name,
    ]
    .into_iter()
    .zip(expected_names)
    {
        if name != &expected {
            bail!(
                "managed package transaction journal planned name mismatch: expected `{expected}`, found `{name}`"
            );
        }
    }
    if journal
        .previous_record_name
        .as_deref()
        .is_some_and(|name| name.is_empty() || Utf8Path::new(name).components().count() != 1)
    {
        bail!("managed package transaction journal has an unsafe predecessor name");
    }
    let expected_snapshot_name = format!(
        ".uniffi-managed-package-{package_identity}-{}-previous-generation.tar.gz",
        journal.generation
    );
    match (
        journal.cleanup_snapshot_name.as_deref(),
        journal.cleanup_snapshot_identity.as_ref(),
        journal.cleanup_snapshot_digest.as_deref(),
        journal.cleanup_snapshot_len,
    ) {
        (None, None, None, None) => {}
        (Some(name), None, None, None) | (Some(name), Some(_), Some(_), Some(_))
            if name == expected_snapshot_name => {}
        _ => bail!(
            "managed package transaction journal has an unsafe or partial cleanup snapshot witness"
        ),
    }
    if !matches!(
        journal.state.as_str(),
        "prepared"
            | "candidateCreated"
            | "building"
            | "candidateReady"
            | "buildClean"
            | "renamingPublicToBackup"
            | "publicBackedUp"
            | "renamingCandidateToPublic"
            | "candidatePublished"
            | "publishingFinalOwner"
            | "committed"
            | "snapshottingBackup"
            | "snapshotReady"
            | "cleaningBackup"
            | "backupClean"
            | "cleaningSnapshot"
            | "snapshotClean"
            | "complete"
    ) {
        bail!(
            "managed package transaction journal has unsupported state `{}`",
            journal.state
        );
    }
    Ok(())
}

fn serialize_managed_journal(journal: &ManagedPackageJournal) -> Result<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(journal)?;
    bytes.push(b'\n');
    if bytes.len() > 1024 * 1024 {
        bail!("managed package transaction journal exceeds its bounded size");
    }
    Ok(bytes)
}

fn managed_record_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn write_new_managed_journal(
    parent: &Utf8Path,
    journal: &ManagedPackageJournal,
) -> Result<super::ohos::DurableRecordWrite> {
    let bytes = serialize_managed_journal(journal)?;
    let path = managed_journal_record_path(parent, journal);
    Ok(super::ohos::write_immutable_durable_record(
        &path,
        &bytes,
        "managed package transaction record",
    ))
}

#[cfg(test)]
thread_local! {
    static MANAGED_JOURNAL_TEST_FAULT: std::cell::RefCell<Option<(String, &'static str)>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn managed_journal_test_fault(state: &str) -> Option<&'static str> {
    MANAGED_JOURNAL_TEST_FAULT.with(|fault| {
        let mut fault = fault.borrow_mut();
        if fault.as_ref().is_some_and(|(target, _)| target == state) {
            fault.take().map(|(_, mode)| mode)
        } else {
            None
        }
    })
}

fn append_managed_journal(
    parent: &Utf8Path,
    journal: &mut ManagedPackageJournal,
    records: &mut Vec<super::ohos::DurableRecordWitness>,
    preserve_records: &mut bool,
) -> Result<()> {
    let previous = records
        .last()
        .context("managed package transaction has no durable initial record")?;
    // The predecessor is the chain trust root.  A same-bytes replacement inode
    // or any ABA is rejected before the successor is created.
    super::ohos::verify_immutable_durable_record(
        previous,
        "managed package transaction predecessor",
    )?;
    journal.sequence = journal
        .sequence
        .checked_add(1)
        .context("managed package journal sequence overflow")?;
    journal.previous_record_name = Some(
        previous
            .path
            .file_name()
            .context("managed package predecessor has no file name")?
            .to_string(),
    );
    journal.previous_record_identity = Some(previous.identity.clone());
    journal.previous_record_digest = Some(previous.sha256.clone());
    let intended = serialize_managed_journal(journal)?;
    #[cfg(test)]
    let injected_fault = managed_journal_test_fault(&journal.state);
    #[cfg(test)]
    if let Some(mode @ ("write" | "fileSync" | "parentSync")) = injected_fault {
        super::ohos::set_durable_record_test_fault(Some(mode));
    }
    #[cfg(test)]
    let written = if injected_fault == Some("notCreated") {
        super::ohos::DurableRecordWrite::NotCreated(anyhow::anyhow!(
            "injected managed durable-record create failure"
        ))
    } else {
        super::ohos::write_immutable_durable_record(
            &managed_journal_record_path(parent, journal),
            &intended,
            "managed package transaction record",
        )
    };
    #[cfg(not(test))]
    let written = super::ohos::write_immutable_durable_record(
        &managed_journal_record_path(parent, journal),
        &intended,
        "managed package transaction record",
    );
    #[cfg(test)]
    if injected_fault.is_some_and(|mode| mode != "notCreated") {
        super::ohos::set_durable_record_test_fault(None);
    }
    match written {
        super::ohos::DurableRecordWrite::Durable(witness) => {
            records.push(witness);
            Ok(())
        }
        super::ohos::DurableRecordWrite::NotCreated(error) => Err(error),
        super::ohos::DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
            let mut retained = false;
            if let Some(witness) = evidence.exact_witness() {
                if witness.len == intended.len() as u64
                    && witness.sha256 == managed_record_digest(&intended)
                {
                    // Complete uncertain JSON remains linked to every
                    // predecessor for immediate rollback/audit.
                    records.push(witness);
                    retained = true;
                } else if let Err(cleanup) = super::ohos::remove_immutable_durable_record(
                    &witness,
                    "partial uncertain managed transaction successor",
                ) {
                    *preserve_records = true;
                    return Err(anyhow::anyhow!(
                        "{error:#}; partial managed successor {} differs from intended JSON and exact cleanup failed: {cleanup:#}; preserving every predecessor",
                        evidence.path
                    ));
                }
            } else {
                *preserve_records = true;
                retained = true;
            }
            if retained {
                return Err(anyhow::anyhow!(
                    "{error:#}; managed successor durability is uncertain and the linked chain is preserved at {} (identity {:?}, length {:?}, digest {:?})",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                ));
            }
            Err(anyhow::anyhow!(
                "{error:#}; partial uncertain managed successor at {} was removed by its exact identity/digest witness; durable predecessors remain available for rollback",
                evidence.path
            ))
        }
    }
}

fn remove_managed_journals(records: &mut Vec<super::ohos::DurableRecordWitness>) -> Result<()> {
    let mut budget = super::ohos::TraversalBudget::managed();
    remove_managed_journals_with_budget(records, &mut budget)
}

fn remove_managed_journals_with_budget(
    records: &mut Vec<super::ohos::DurableRecordWitness>,
    budget: &mut super::ohos::TraversalBudget,
) -> Result<()> {
    // Remove newest-to-oldest so any interruption leaves a valid prefix chain.
    while let Some(record) = records.last() {
        budget.consume(
            record.path.as_str(),
            "record",
            std::fs::symlink_metadata(&record.path)?.len(),
        )?;
        super::ohos::remove_immutable_durable_record(record, "managed package transaction record")?;
        records.pop();
    }
    Ok(())
}

fn audit_managed_transaction_residue(
    parent: &Utf8Path,
    public_root: &Utf8Path,
    digest: &str,
) -> Result<()> {
    let record_prefix = managed_journal_prefix(digest);
    let prefix = format!(".uniffi-managed-package-{digest}-");
    let mut budget = super::ohos::TraversalBudget::managed();
    let mut records = Vec::new();
    let mut residues = Vec::new();
    for entry in std::fs::read_dir(parent)
        .with_context(|| format!("auditing managed package parent {parent}"))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().to_string();
        let is_record = name.starts_with(&record_prefix);
        let is_residue = name.starts_with(&prefix);
        if !is_record && !is_residue {
            // Other package transactions use the same parent and their
            // cooperative locks are intentionally disjoint. They may remove
            // an unrelated immutable record/root after read_dir returned its
            // name; count it when still present, but do not turn that legal
            // disappearance into a failure for this package identity.
            let _ = super::ohos::try_consume_unrelated_directory_entry(&entry, &name, &mut budget)?;
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        let kind = if metadata.file_type().is_symlink() {
            "symlink"
        } else if metadata.is_dir() {
            "directory"
        } else if metadata.is_file() {
            "record"
        } else {
            "special"
        };
        let controlled_bytes = (is_record && metadata.is_file())
            .then_some(metadata.len())
            .unwrap_or(0);
        budget.consume(&name, kind, controlled_bytes)?;
        let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
            anyhow::anyhow!(
                "managed package residue path is not utf8: {}",
                path.display()
            )
        })?;
        if is_record {
            let metadata = std::fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("managed package transaction record has an unsafe type: {path}");
            }
            let (bytes, identity) = super::ohos::read_verified_regular_file_bounded_with_identity(
                &path,
                1024 * 1024,
                "managed package crash record",
            )?;
            let journal: ManagedPackageJournal = serde_json::from_slice(&bytes)
                .with_context(|| format!("parsing managed package crash record {path}"))?;
            validate_managed_journal(&journal, digest, public_root)?;
            if managed_journal_record_path(parent, &journal) != path {
                bail!("managed package transaction record filename/content mismatch: {path}");
            }
            records.push((journal, managed_record_digest(&bytes), identity, path));
            continue;
        }
        if is_residue {
            residues.push(path);
        }
    }
    if !records.is_empty() {
        records.sort_by(|left, right| {
            left.0
                .generation
                .cmp(&right.0.generation)
                .then_with(|| left.0.sequence.cmp(&right.0.sequence))
        });
        let generation = records[0].0.generation.clone();
        let mut previous_digest = None;
        let mut previous_identity = None;
        let mut previous_name = None;
        let mut previous_state: Option<&str> = None;
        for (index, (journal, digest, identity, path)) in records.iter().enumerate() {
            let transition_ok = match (previous_state, journal.state.as_str()) {
                (None, "prepared")
                | (Some("prepared"), "candidateCreated")
                | (Some("candidateCreated"), "building")
                | (Some("building"), "building" | "candidateReady")
                | (Some("candidateReady"), "buildClean")
                | (Some("buildClean"), "renamingPublicToBackup")
                | (Some("renamingPublicToBackup"), "publicBackedUp")
                | (Some("publicBackedUp"), "renamingCandidateToPublic")
                | (Some("renamingCandidateToPublic"), "candidatePublished")
                | (Some("candidatePublished"), "publishingFinalOwner")
                | (Some("publishingFinalOwner"), "committed")
                | (Some("committed"), "snapshottingBackup" | "backupClean")
                | (Some("snapshottingBackup"), "snapshotReady")
                | (Some("snapshotReady"), "cleaningBackup")
                | (Some("cleaningBackup"), "backupClean")
                | (Some("backupClean"), "cleaningSnapshot" | "complete")
                | (Some("cleaningSnapshot"), "snapshotClean")
                | (Some("snapshotClean"), "complete") => true,
                _ => false,
            };
            if journal.generation != generation
                || journal.sequence != index as u64
                || journal.previous_record_name != previous_name
                || journal.previous_record_identity != previous_identity
                || journal.previous_record_digest != previous_digest
                || !transition_ok
            {
                bail!("managed package transaction record chain is partial or reordered at {path}");
            }
            previous_digest = Some(digest.clone());
            previous_identity = Some(identity.clone());
            previous_name = Some(
                path.file_name()
                    .context("managed package crash record has no file name")?
                    .to_string(),
            );
            previous_state = Some(journal.state.as_str());
        }
        let last = &records.last().expect("record chain is non-empty").0;
        bail!(
            "previous managed package transaction `{}` stopped in state `{}`; preserving its append-only record chain and {} planned residue(s) for audit",
            last.generation,
            last.state,
            residues.len()
        );
    }
    if let Some(path) = residues.first() {
        bail!("managed package residue has no durable transaction chain: {path}");
    }
    Ok(())
}

#[cfg(test)]
fn managed_crash_sync_point(label: &str) {
    if std::env::var("UNIFFI_TEST_MANAGED_CRASH_AT").as_deref() != Ok(label) {
        return;
    }
    let reached = std::env::var_os("UNIFFI_TEST_MANAGED_CRASH_REACHED")
        .expect("managed crash test requires a reached marker");
    let mut file = std::fs::File::create(reached).expect("creating managed crash marker");
    file.write_all(label.as_bytes())
        .expect("writing managed crash marker");
    file.sync_all().expect("syncing managed crash marker");
    #[cfg(unix)]
    unsafe {
        libc::kill(std::process::id() as i32, libc::SIGKILL);
        libc::_exit(137);
    }
    #[cfg(windows)]
    std::process::abort();
}

impl ManagedPackageTransaction {
    fn committed_error(
        &self,
        stage: &str,
        error: anyhow::Error,
        backup: &Utf8Path,
        snapshot: Option<&Utf8Path>,
    ) -> anyhow::Error {
        anyhow::anyhow!(
            "managed generation {} committed=true; {stage} failed: {error:#}; backup={} snapshot={} append-only-record-parent={}",
            self.generation,
            backup,
            snapshot
                .map(Utf8Path::as_str)
                .unwrap_or("<not-created-or-not-applicable>"),
            self.journal_parent
        )
    }

    /// Restore the public package root after a controlled error before the
    /// final owner sidecar commit point.  The candidate and previous roots are
    /// matched against their creation-time captures before either rename.  A
    /// schema-3 previous owner is rewritten with the mutation epochs caused by
    /// the transaction's own public->backup->public cycle; otherwise the next
    /// invocation would correctly reject our own rollback as an ABA.
    fn rollback_precommit_publication(
        &mut self,
        had_public: bool,
        backup: &Utf8Path,
        failed: &Utf8Path,
        candidate_capture: &super::ohos::OwnedTreeSnapshot,
        owner_successor: Option<&super::ohos::DurableRecordWitness>,
        final_owner_trusted: bool,
        cleanup_journals: bool,
    ) -> Result<()> {
        let mut budget = super::ohos::TraversalBudget::managed();
        if super::ohos::path_entry_exists(failed)? {
            bail!("managed failed-candidate rollback path already exists: {failed}");
        }
        // A controlled error can occur either before or after the candidate
        // rename.  Account for both states from the same creation-time
        // capture; never infer ownership by freshly adopting whichever tree
        // happens to occupy a pathname.
        let candidate_is_public = super::ohos::path_entry_exists(&self.public_root)?;
        let candidate_is_private = super::ohos::path_entry_exists(&self.private_root)?;
        if candidate_is_public == candidate_is_private {
            bail!(
                "managed pre-commit rollback cannot prove one exclusive candidate location (public={candidate_is_public}, private={candidate_is_private}); preserving every root and control record"
            );
        }
        let published_candidate = if candidate_is_public {
            Some(
                super::ohos::recapture_directory_after_owned_rename_with_budget(
                    &self.public_root,
                    candidate_capture,
                    &mut budget,
                )?,
            )
        } else {
            super::ohos::validate_directory_capture_with_budget(
                &self.private_root,
                candidate_capture,
                &mut budget,
            )
            .context("validating private managed candidate during pre-commit rollback")?;
            None
        };
        let previous_backup = if had_public {
            Some(
                super::ohos::recapture_directory_after_owned_rename_with_budget(
                    backup,
                    self.captured_root
                        .as_ref()
                        .context("managed rollback lacks its previous-root capture")?,
                    &mut budget,
                )?,
            )
        } else {
            None
        };

        if candidate_is_public {
            std::fs::rename(&self.public_root, failed)
                .with_context(|| format!("moving uncommitted managed root to {failed}"))?;
            super::ohos::sync_directory(&self.journal_parent)?;
        }
        if had_public {
            std::fs::rename(backup, &self.public_root)
                .context("restoring previous managed package root")?;
            super::ohos::sync_directory(&self.journal_parent)?;
            let _restored = super::ohos::recapture_directory_after_owned_rename_with_budget(
                &self.public_root,
                previous_backup
                    .as_ref()
                    .context("managed rollback lost its previous-root capture")?,
                &mut budget,
            )?;

            if self.previous_owner_witness.is_some() {
                if !final_owner_trusted {
                    bail!(
                        "previous managed owner sidecar changed before rollback; restored root and all control evidence are preserved"
                    );
                }
                let mut rebound = self
                    .previous_owner
                    .clone()
                    .context("managed sidecar witness has no parsed previous owner")?;
                rebound.root_identity =
                    super::ohos::persistent_fs_identity(&self.public_root, true)?;
                rebound.root_mutation_token = Some(
                    super::ohos::directory_mutation_token_for_owner(&self.public_root)?,
                );
                rebound.entries = capture_managed_entries_with_budget(
                    &self.public_root,
                    &self.public_root,
                    &mut budget,
                )?;
                rebound.state = "committed".into();

                let final_owner = managed_owner_path(&self.public_root);
                let rollback_candidate = self.journal_parent.join(format!(
                    ".uniffi-managed-package-owner-rollback-{}.json",
                    self.generation
                ));
                let mut bytes = serde_json::to_vec_pretty(&rebound)?;
                bytes.push(b'\n');
                let rebound_witness = match super::ohos::write_immutable_durable_record(
                    &rollback_candidate,
                    &bytes,
                    "managed rollback owner candidate",
                ) {
                    super::ohos::DurableRecordWrite::Durable(witness) => witness,
                    super::ohos::DurableRecordWrite::NotCreated(error) => return Err(error),
                    super::ohos::DurableRecordWrite::CreatedDurabilityUncertain {
                        evidence,
                        error,
                    } => {
                        bail!(
                            "{error:#}; managed rollback owner durability is uncertain and is preserved at {} with identity {:?}, length {:?}, digest {:?}",
                            evidence.path,
                            evidence.identity,
                            evidence.len,
                            evidence.sha256
                        )
                    }
                };
                super::ohos::verify_immutable_durable_record(
                    self.previous_owner_witness
                        .as_ref()
                        .expect("checked previous managed owner witness"),
                    "previous managed owner immediately before rollback rebind",
                )?;
                super::ohos::verify_immutable_durable_record(
                    &rebound_witness,
                    "managed rollback owner candidate immediately before commit",
                )?;
                super::ohos::replace_file_atomically(&rollback_candidate, &final_owner)?;
                super::ohos::sync_directory(&self.journal_parent)?;
                validate_managed_owner_with_budget(&self.public_root, &rebound, &mut budget)?;
            }
        } else {
            let final_owner = managed_owner_path(&self.public_root);
            if super::ohos::path_entry_exists(&final_owner)? {
                bail!(
                    "managed final owner appeared while rolling back an initially absent package: {final_owner}"
                );
            }
        }

        if let Some(published_candidate) = published_candidate.as_ref() {
            let failed_capture = super::ohos::recapture_directory_after_owned_rename_with_budget(
                failed,
                published_candidate,
                &mut budget,
            )?;
            super::ohos::remove_captured_directory_for_cleanup_with_budget(
                failed,
                &failed_capture,
                &mut budget,
            )?;
        } else {
            self.private.cleanup_with_budget(&mut budget).context(
                "removing the exact private managed candidate during pre-commit rollback",
            )?;
        }
        if let Some(successor) = owner_successor {
            budget.consume(
                successor.path.as_str(),
                "record",
                std::fs::symlink_metadata(&successor.path)?.len(),
            )?;
            super::ohos::remove_immutable_durable_record(
                successor,
                "uncommitted managed final owner candidate",
            )?;
        }
        if cleanup_journals {
            self.remove_journals_with_budget(&mut budget)?;
            self.completed = true;
        }
        Ok(())
    }

    fn precommit_error_after_publication(
        &mut self,
        stage: &str,
        error: anyhow::Error,
        had_public: bool,
        backup: &Utf8Path,
        failed: &Utf8Path,
        candidate_capture: &super::ohos::OwnedTreeSnapshot,
        owner_successor: Option<&super::ohos::DurableRecordWitness>,
        final_owner_trusted: bool,
        cleanup_journals: bool,
    ) -> anyhow::Error {
        match self.rollback_precommit_publication(
            had_public,
            backup,
            failed,
            candidate_capture,
            owner_successor,
            final_owner_trusted,
            cleanup_journals,
        ) {
            Ok(()) => anyhow::anyhow!(
                "managed generation {} committed=false; {stage} failed and the complete previous public generation was restored in this invocation: {error:#}",
                self.generation
            ),
            Err(rollback) => anyhow::anyhow!(
                "managed generation {} committed=false; {stage} failed: {error:#}; identity-bound rollback/cleanup was incomplete: {rollback:#}; preserve public={} backup={} failed={} record-parent={}",
                self.generation,
                self.public_root,
                backup,
                failed,
                self.journal_parent
            ),
        }
    }

    fn begin(layout: &ManagedLayout) -> Result<Self> {
        let requested_root = &layout.package_dir;
        match std::fs::symlink_metadata(requested_root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                bail!("managed package root must be a real directory: {requested_root}")
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("preflighting managed package root"),
        }
        let public_root = canonicalize_invocation_output(requested_root)?;
        let locks = super::ohos::OutputLockSet::acquire(
            std::slice::from_ref(&public_root),
            "managed package root transaction",
        )?;
        let parent = public_root
            .parent()
            .context("managed package root has no parent")?
            .to_path_buf();
        std::fs::create_dir_all(&parent)
            .with_context(|| format!("creating managed package parent {parent}"))?;
        let package_identity = managed_package_digest(&public_root);
        audit_managed_transaction_residue(&parent, &public_root, &package_identity)?;

        let public_exists = super::ohos::path_entry_exists(&public_root)?;
        let mut startup_budget = super::ohos::TraversalBudget::managed();
        let (previous_owner, previous_owner_witness, captured_root) = if public_exists {
            let sidecar = managed_owner_path(&public_root);
            let embedded = managed_embedded_owner_path(&public_root);
            let has_sidecar = super::ohos::path_entry_exists(&sidecar)?;
            let previous_owner = if has_sidecar || super::ohos::path_entry_exists(&embedded)? {
                let owner = parse_managed_owner(&public_root)?;
                validate_managed_owner_with_budget(&public_root, &owner, &mut startup_budget)?;
                Some(owner)
            } else {
                validate_legacy_managed_package_with_budget(&public_root, &mut startup_budget)?;
                None
            };
            let previous_owner_witness = if has_sidecar {
                let (bytes, identity) =
                    super::ohos::read_verified_regular_file_bounded_with_identity(
                        &sidecar,
                        16 * 1024 * 1024,
                        "managed package final owner sidecar",
                    )?;
                Some(super::ohos::DurableRecordWitness {
                    path: sidecar,
                    identity,
                    sha256: managed_record_digest(&bytes),
                    len: bytes.len() as u64,
                })
            } else {
                None
            };
            let captured = super::ohos::capture_directory_for_cleanup_with_budget(
                &public_root,
                &mut startup_budget,
            )?;
            (previous_owner, previous_owner_witness, Some(captured))
        } else {
            (None, None, None)
        };
        let previous_root_identity = if public_exists {
            Some(super::ohos::persistent_fs_identity(&public_root, true)?)
        } else {
            None
        };
        let generation = new_managed_generation();
        let candidate_name =
            format!(".uniffi-managed-package-{package_identity}-{generation}-next");
        let build_name = format!(".uniffi-managed-package-{package_identity}-{generation}-build");
        let public_name = public_root
            .file_name()
            .context("managed package root has no file name")?;
        let backup_name =
            format!(".uniffi-managed-package-{package_identity}-{generation}-{public_name}-backup");
        let failed_name =
            format!(".uniffi-managed-package-{package_identity}-{generation}-{public_name}-failed");
        let mut journal = ManagedPackageJournal {
            owner: MANAGED_PACKAGE_JOURNAL_KIND.into(),
            schema_version: MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION,
            package_identity,
            generation: generation.clone(),
            sequence: 0,
            previous_record_name: None,
            previous_record_identity: None,
            previous_record_digest: None,
            state: "prepared".into(),
            public_root: public_root.to_string(),
            candidate_name: candidate_name.clone(),
            build_name: build_name.clone(),
            backup_name,
            failed_name,
            previous_root_identity,
            candidate_root_identity: None,
            build_root_identity: None,
            backup_root_identity: None,
            published_root_identity: None,
            cleanup_snapshot_name: None,
            cleanup_snapshot_identity: None,
            cleanup_snapshot_digest: None,
            cleanup_snapshot_len: None,
        };
        validate_managed_journal(&journal, &journal.package_identity, &public_root)?;
        let mut journal_records = Vec::new();
        match write_new_managed_journal(&parent, &journal)? {
            super::ohos::DurableRecordWrite::Durable(witness) => journal_records.push(witness),
            super::ohos::DurableRecordWrite::NotCreated(error) => return Err(error),
            super::ohos::DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                if let Some(witness) = evidence.exact_witness() {
                    journal_records.push(witness);
                }
                return Err(anyhow::anyhow!(
                    "{error:#}; initial managed transaction record may be durable and is preserved at {} with identity {:?}, length {:?}, digest {:?}",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                ));
            }
        }
        #[cfg(test)]
        managed_crash_sync_point("journalDurable");
        let mut preserve_journals = false;

        let private_root = parent.join(candidate_name);
        let private = match ManagedOwnedDirectory::create(private_root.clone()) {
            Ok(directory) => directory,
            Err(error) => {
                if super::ohos::path_entry_exists(&private_root).unwrap_or(true) {
                    return Err(error).with_context(|| {
                        format!(
                            "managed candidate creation left an unsealed root; preserving its append-only transaction records under {parent}"
                        )
                    });
                }
                return match remove_managed_journals(&mut journal_records) {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "creating managed candidate failed: {error:#}; immutable record cleanup also failed: {cleanup:#}"
                    )),
                };
            }
        };
        journal.candidate_root_identity = Some(private.root_identity.clone());
        journal.state = "candidateCreated".into();
        append_managed_journal(
            &parent,
            &mut journal,
            &mut journal_records,
            &mut preserve_journals,
        )?;
        #[cfg(test)]
        managed_crash_sync_point("candidateCreated");
        let build_path = parent.join(build_name);
        let build_temp = match ManagedOwnedDirectory::create_ephemeral(build_path) {
            Ok(directory) => directory,
            Err(error) => {
                if super::ohos::path_entry_exists(&parent.join(&journal.build_name)).unwrap_or(true)
                {
                    return Err(error).with_context(|| {
                        format!(
                            "managed build-root creation left an unsealed root; preserving candidate and append-only records under {parent}"
                        )
                    });
                }
                let mut private = private;
                let cleanup = private.cleanup();
                if cleanup.is_ok() {
                    if let Err(record_cleanup) = remove_managed_journals(&mut journal_records) {
                        return Err(anyhow::anyhow!(
                            "creating managed build root failed: {error:#}; candidate was cleaned, but immutable record cleanup failed: {record_cleanup:#}"
                        ));
                    }
                }
                return match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(anyhow::anyhow!(
                        "creating managed build root failed: {error:#}; candidate cleanup also failed: {cleanup:#}; inspect the append-only transaction records under {parent}"
                    )),
                };
            }
        };
        journal.build_root_identity = Some(build_temp.root_identity.clone());
        journal.state = "building".into();
        append_managed_journal(
            &parent,
            &mut journal,
            &mut journal_records,
            &mut preserve_journals,
        )?;
        #[cfg(test)]
        managed_crash_sync_point("buildCreated");
        let private_layout = layout.rebased(&layout.package_dir, &private_root)?;
        let mut transaction = Self {
            private,
            build_temp,
            private_root,
            public_root,
            public_layout: layout.clone(),
            private_layout,
            previous_owner,
            previous_owner_witness,
            captured_root,
            generation,
            journal_parent: parent,
            journal_records,
            preserve_journals,
            journal,
            completed: false,
            _locks: locks,
        };
        if let Some(captured) = &transaction.captured_root {
            let mut seed_budget = super::ohos::TraversalBudget::managed();
            let seeded = super::ohos::copy_captured_directory_with_budget(
                &transaction.public_root,
                &transaction.private_root,
                captured,
                &mut seed_budget,
            )?;
            transaction
                .private
                .register_seeded_contents_with_budget(seeded, &mut seed_budget)?;
            transaction.journal.candidate_root_identity =
                Some(transaction.private.root_identity.clone());
            append_managed_journal(
                &transaction.journal_parent,
                &mut transaction.journal,
                &mut transaction.journal_records,
                &mut transaction.preserve_journals,
            )?;
        }
        Ok(transaction)
    }

    fn private_layout(&self) -> &ManagedLayout {
        &self.private_layout
    }

    fn append_journal(&mut self) -> Result<()> {
        append_managed_journal(
            &self.journal_parent,
            &mut self.journal,
            &mut self.journal_records,
            &mut self.preserve_journals,
        )
    }

    fn remove_journals_with_budget(
        &mut self,
        budget: &mut super::ohos::TraversalBudget,
    ) -> Result<()> {
        remove_managed_journals_with_budget(&mut self.journal_records, budget)
    }

    fn private_args(&self, public: &BuildArgs) -> Result<BuildArgs> {
        let mut private = public.clone();
        let rebase = |path: &Utf8Path| -> Result<Utf8PathBuf> {
            let relative = path
                .strip_prefix(&self.public_layout.package_dir)
                .with_context(|| format!("managed output escaped package root: {path}"))?;
            Ok(self.private_root.join(relative))
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
        private.package_dir = Some(self.private_root.clone());
        let build_root = self.build_temp.path.clone();
        private.napi_target_dir = Some(build_root.join("napi"));
        private.wasm_core_target_dir = Some(build_root.join("wasm/core"));
        private.wasm_target_dir = Some(build_root.join("wasm/host"));
        private.ohos_target_dir = Some(build_root.join("ohos"));
        private.logical_host_crates_dir = None;
        private.managed_layout = false;
        private.invocation_output_lock_held = true;
        Ok(private)
    }

    fn clear_selected_roots(&mut self, targets: &ExpandedTargets) -> Result<()> {
        let mut budget = super::ohos::TraversalBudget::managed();
        if targets.harmony {
            self.private
                .remove_seeded_path("artifacts/harmony", &mut budget)?;
        }
        if targets.apple {
            self.private
                .remove_seeded_path("artifacts/apple", &mut budget)?;
            self.private
                .remove_seeded_path("src/ffi/swift", &mut budget)?;
            self.private
                .remove_seeded_path("src/ffi/apple", &mut budget)?;
        }
        if targets.android {
            self.private
                .remove_seeded_path("artifacts/android", &mut budget)?;
            self.private
                .remove_seeded_path("src/ffi/kotlin", &mut budget)?;
            self.private
                .remove_seeded_path("src/ffi/android", &mut budget)?;
        }
        let copied_owner = managed_embedded_owner_path(&self.private_root);
        if super::ohos::path_entry_exists(&copied_owner)? {
            self.private
                .remove_seeded_path(MANAGED_PACKAGE_OWNER_FILE, &mut budget)?;
        }
        Ok(())
    }

    fn prepare_owner(&mut self) -> Result<ManagedPackageOwner> {
        let mut budget = super::ohos::TraversalBudget::managed();
        let copied_owner = managed_embedded_owner_path(&self.private_root);
        if super::ohos::path_entry_exists(&copied_owner)? {
            super::ohos::remove_current_regular_file_for_cleanup_with_budget(
                &copied_owner,
                "copied managed owner",
                &mut budget,
            )
            .with_context(|| format!("removing copied managed owner {copied_owner}"))?;
        }
        let entries = capture_managed_entries_with_budget(
            &self.private_root,
            &self.public_root,
            &mut budget,
        )?;
        let owner = ManagedPackageOwner {
            owner: MANAGED_PACKAGE_OWNER_KIND.into(),
            schema_version: MANAGED_PACKAGE_OWNER_SCHEMA_VERSION,
            generation: self.generation.clone(),
            state: "prepared".into(),
            root_identity: super::ohos::persistent_fs_identity(&self.private_root, true)?,
            root_mutation_token: None,
            entries,
        };
        self.private.seal_with_budget(&mut budget)?;
        self.build_temp.seal_with_budget(&mut budget)?;
        self.journal.candidate_root_identity = Some(self.private.root_identity.clone());
        self.journal.build_root_identity = Some(self.build_temp.root_identity.clone());
        self.journal.state = "candidateReady".into();
        self.append_journal()?;
        Ok(owner)
    }

    fn revalidate_previous_with_budget(
        &self,
        budget: &mut super::ohos::TraversalBudget,
    ) -> Result<()> {
        match (&self.previous_owner, &self.captured_root) {
            (Some(owner), Some(captured)) => {
                validate_managed_owner_with_budget(&self.public_root, owner, budget)?;
                super::ohos::validate_directory_capture_with_budget(
                    &self.public_root,
                    captured,
                    budget,
                )
            }
            (None, Some(captured)) => {
                validate_legacy_managed_package_with_budget(&self.public_root, budget)?;
                super::ohos::validate_directory_capture_with_budget(
                    &self.public_root,
                    captured,
                    budget,
                )
            }
            (None, None) if !super::ohos::path_entry_exists(&self.public_root)? => Ok(()),
            _ => bail!("managed package previous generation changed during its transaction"),
        }
    }

    fn abort(mut self, error: anyhow::Error) -> anyhow::Error {
        let cleanup = (|| -> Result<()> {
            let mut budget = super::ohos::TraversalBudget::managed();
            // Cleanup never expands ownership by re-capturing partial tool
            // output.  Try both guards while the union lock is held, then
            // preserve every unprovable root and its durable journal.
            let private = self
                .private
                .cleanup_with_budget(&mut budget)
                .context("cleaning managed candidate after controlled failure");
            let build = self
                .build_temp
                .cleanup_with_budget(&mut budget)
                .context("cleaning managed build root after controlled failure");
            if let (Err(private), Err(build)) = (&private, &build) {
                bail!("candidate cleanup failed: {private:#}; build cleanup failed: {build:#}");
            }
            private?;
            build?;
            if self.preserve_journals {
                bail!(
                    "managed append-only records are preserved because a created successor lacks an exact removable witness"
                );
            }
            self.remove_journals_with_budget(&mut budget)?;
            self.completed = true;
            Ok(())
        })();
        match cleanup {
            Ok(()) => error,
            Err(cleanup) => anyhow::anyhow!(
                "managed package build failed: {error:#}; identity-bound controlled-failure cleanup also failed: {cleanup:#}; preserving crash journal {}",
                self.journal_parent
            ),
        }
    }

    fn commit(mut self, mut owner: ManagedPackageOwner) -> Result<()> {
        let mut budget = super::ohos::TraversalBudget::managed();
        owner.state = "committed".into();
        let candidate_capture = self.private.snapshot.clone();
        self.build_temp
            .cleanup_with_budget(&mut budget)
            .context("cleaning identity-owned managed build root before publication")?;
        self.journal.build_root_identity = None;
        self.journal.candidate_root_identity = Some(self.private.root_identity.clone());
        self.journal.state = "buildClean".into();
        if let Err(error) = self.append_journal() {
            return Err(self.abort(error.context("recording managed build-root cleanup")));
        }
        if let Err(error) = self.revalidate_previous_with_budget(&mut budget) {
            return Err(self.abort(error.context("revalidating previous managed generation")));
        }
        let parent = self
            .public_root
            .parent()
            .context("managed package root has no parent")?
            .to_path_buf();
        let backup = parent.join(&self.journal.backup_name);
        let failed = parent.join(&self.journal.failed_name);
        if super::ohos::path_entry_exists(&backup)? {
            bail!("managed package backup already exists: {backup}");
        }
        if super::ohos::path_entry_exists(&failed)? {
            bail!("managed package failed-generation path already exists: {failed}");
        }
        let had_public = super::ohos::path_entry_exists(&self.public_root)?;
        let mut backup_capture = None;
        self.journal.state = "renamingPublicToBackup".into();
        self.append_journal()?;
        #[cfg(test)]
        managed_crash_sync_point("beforePublicToBackup");
        if had_public {
            if let Err(error) = std::fs::rename(&self.public_root, &backup)
                .with_context(|| format!("moving managed package generation to {backup}"))
            {
                return Err(self.abort(error));
            }
            let captured = (|| -> Result<super::ohos::OwnedTreeSnapshot> {
                let captured = super::ohos::recapture_directory_after_owned_rename_with_budget(
                    &backup,
                    self.captured_root
                        .as_ref()
                        .context("managed package backup lacks its pre-rename capture")?,
                    &mut budget,
                )?;
                self.journal.backup_root_identity =
                    Some(super::ohos::persistent_fs_identity(&backup, true)?);
                super::ohos::sync_directory(&parent)?;
                Ok(captured)
            })();
            match captured {
                Ok(captured) => backup_capture = Some(captured),
                Err(error) => {
                    return Err(self.precommit_error_after_publication(
                        "capturing the renamed previous generation",
                        error,
                        had_public,
                        &backup,
                        &failed,
                        &candidate_capture,
                        None,
                        true,
                        true,
                    ));
                }
            }
        }
        self.journal.state = "publicBackedUp".into();
        if let Err(error) = self.append_journal() {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording the previous-generation backup",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                cleanup_journals,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterPublicToBackup");
        self.journal.state = "renamingCandidateToPublic".into();
        if let Err(error) = self.append_journal() {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording candidate publication intent",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                cleanup_journals,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("beforeCandidateToPublic");
        if let Err(error) = std::fs::rename(&self.private_root, &self.public_root) {
            return Err(self.precommit_error_after_publication(
                "publishing managed package root candidate",
                error.into(),
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                true,
            ));
        }
        self.private.disarm_after_rename();
        let published = (|| -> Result<()> {
            self.journal.published_root_identity = Some(super::ohos::persistent_fs_identity(
                &self.public_root,
                true,
            )?);
            super::ohos::sync_directory(&parent)?;
            self.journal.state = "candidatePublished".into();
            self.append_journal()
        })();
        if let Err(error) = published {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording the published candidate generation",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                cleanup_journals,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterCandidateToPublic");
        // Rebind every mutation witness after the package-root rename.  Every
        // fallible step in this pre-commit section is routed through the same
        // identity-bound rollback; no `?` may strand the new public root under
        // the previous owner record.
        let prepared_owner = (|| -> Result<(Utf8PathBuf, Utf8PathBuf, Vec<u8>)> {
            owner.root_identity = super::ohos::persistent_fs_identity(&self.public_root, true)?;
            owner.root_mutation_token = Some(super::ohos::directory_mutation_token_for_owner(
                &self.public_root,
            )?);
            owner.entries = capture_managed_entries_with_budget(
                &self.public_root,
                &self.public_root,
                &mut budget,
            )?;
            let final_owner = managed_owner_path(&self.public_root);
            let final_owner_name = final_owner
                .file_name()
                .context("managed package owner sidecar has no file name")?;
            let public_successor =
                parent.join(format!(".{final_owner_name}.next-{}", self.generation));
            let mut owner_bytes = serde_json::to_vec_pretty(&owner)?;
            owner_bytes.push(b'\n');
            Ok((final_owner, public_successor, owner_bytes))
        })();
        let (final_owner, public_successor, owner_bytes) = match prepared_owner {
            Ok(prepared) => prepared,
            Err(error) => {
                return Err(self.precommit_error_after_publication(
                    "preparing final owner witness",
                    error,
                    had_public,
                    &backup,
                    &failed,
                    &candidate_capture,
                    None,
                    true,
                    true,
                ));
            }
        };
        let public_successor_witness = match super::ohos::write_immutable_durable_record(
            &public_successor,
            &owner_bytes,
            "managed package committed owner sidecar candidate",
        ) {
            super::ohos::DurableRecordWrite::Durable(witness) => witness,
            super::ohos::DurableRecordWrite::NotCreated(error) => {
                return Err(self.precommit_error_after_publication(
                    "creating final owner candidate",
                    error,
                    had_public,
                    &backup,
                    &failed,
                    &candidate_capture,
                    None,
                    true,
                    true,
                ));
            }
            super::ohos::DurableRecordWrite::CreatedDurabilityUncertain { evidence, error } => {
                let exact = evidence.exact_witness();
                let uncertain = anyhow::anyhow!(
                    "{error:#}; final owner candidate durability is uncertain at {} with identity {:?}, length {:?}, digest {:?}",
                    evidence.path,
                    evidence.identity,
                    evidence.len,
                    evidence.sha256
                );
                return Err(self.precommit_error_after_publication(
                    "creating final owner candidate",
                    uncertain,
                    had_public,
                    &backup,
                    &failed,
                    &candidate_capture,
                    exact.as_ref(),
                    true,
                    exact.is_some(),
                ));
            }
        };
        let previous_owner_valid = match &self.previous_owner_witness {
            Some(previous) => super::ohos::verify_immutable_durable_record(
                previous,
                "previous managed package final owner sidecar",
            )
            .map(|_| ()),
            None => match super::ohos::path_entry_exists(&final_owner) {
                Ok(true) => Err(anyhow::anyhow!(
                    "managed package final owner sidecar appeared before commit: {final_owner}"
                )),
                Ok(false) => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = previous_owner_valid {
            return Err(self.precommit_error_after_publication(
                "validating previous final owner",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                false,
                false,
            ));
        }
        if let Err(error) = super::ohos::verify_immutable_durable_record(
            &public_successor_witness,
            "managed package committed owner successor",
        ) {
            return Err(self.precommit_error_after_publication(
                "validating final owner candidate",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                false,
            ));
        }
        self.journal.state = "publishingFinalOwner".into();
        if let Err(error) = self.append_journal() {
            let cleanup_journals = !self.preserve_journals;
            return Err(self.precommit_error_after_publication(
                "recording final owner publication intent",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                Some(&public_successor_witness),
                true,
                cleanup_journals,
            ));
        }
        // This is the final source/destination witness check immediately
        // before the single-file owner commit rename.
        let immediate_previous = match &self.previous_owner_witness {
            Some(previous) => super::ohos::verify_immutable_durable_record(
                previous,
                "previous managed owner immediately before final rename",
            )
            .map(|_| ()),
            None => match super::ohos::path_entry_exists(&final_owner) {
                Ok(true) => Err(anyhow::anyhow!(
                    "managed owner destination appeared immediately before final rename: {final_owner}"
                )),
                Ok(false) => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = immediate_previous {
            return Err(self.precommit_error_after_publication(
                "revalidating previous final owner",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                false,
                false,
            ));
        }
        if let Err(error) = super::ohos::verify_immutable_durable_record(
            &public_successor_witness,
            "managed owner candidate immediately before final rename",
        ) {
            return Err(self.precommit_error_after_publication(
                "revalidating final owner candidate",
                error,
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                false,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("beforeFinalOwnerPublish");
        if let Err(error) = super::ohos::replace_file_atomically(&public_successor, &final_owner) {
            return Err(self.precommit_error_after_publication(
                "publishing final owner record",
                error.into(),
                had_public,
                &backup,
                &failed,
                &candidate_capture,
                Some(&public_successor_witness),
                true,
                true,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterFinalOwnerPublish");
        self.journal.state = "committed".into();
        if let Err(error) = self.append_journal() {
            return Err(self.committed_error("appending committed state", error, &backup, None));
        }
        // From this point onward the committed record is public.  No error is
        // allowed to restore an older root.  Post-commit durability or
        // validation failures preserve the previous backup for audit.
        if let Err(error) = super::ohos::sync_directory(&self.public_root)
            .and_then(|_| super::ohos::sync_directory(&parent))
            .and_then(|_| {
                validate_managed_owner_with_budget(&self.public_root, &owner, &mut budget)
            })
        {
            return Err(self.committed_error(
                "validating final owner durability",
                error,
                &backup,
                None,
            ));
        }
        // The committed record is the final commit point.  Cleanup is bounded,
        // identity-bound and never rolls a committed generation back.
        if let (true, Some(captured)) = (had_public, backup_capture.as_ref()) {
            let snapshot_name = format!(
                ".uniffi-managed-package-{}-{}-previous-generation.tar.gz",
                self.journal.package_identity, self.generation
            );
            let snapshot_path = parent.join(&snapshot_name);
            self.journal.cleanup_snapshot_name = Some(snapshot_name);
            self.journal.state = "snapshottingBackup".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording cleanup snapshot intent",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            let snapshot = match super::ohos::snapshot_directory_for_cleanup_with_budget(
                &backup,
                &snapshot_path,
                "managed package complete previous generation",
                &mut budget,
            ) {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    return Err(self.committed_error(
                        "creating complete previous-generation snapshot",
                        error,
                        &backup,
                        Some(&snapshot_path),
                    ));
                }
            };
            self.journal.cleanup_snapshot_identity = Some(snapshot.identity.clone());
            self.journal.cleanup_snapshot_digest = Some(snapshot.sha256.clone());
            self.journal.cleanup_snapshot_len = Some(snapshot.len);
            self.journal.state = "snapshotReady".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording durable previous-generation snapshot",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            self.journal.state = "cleaningBackup".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording backup cleanup start",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("beforeBackupCleanup");
            if let Err(error) = super::ohos::remove_captured_directory_for_cleanup_with_budget(
                &backup,
                captured,
                &mut budget,
            ) {
                return Err(self.committed_error(
                    "identity-bound previous backup cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("afterBackupCleanup");
            self.journal.backup_root_identity = None;
            self.journal.state = "backupClean".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording previous backup cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            self.journal.state = "cleaningSnapshot".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording previous-generation snapshot cleanup intent",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("beforeSnapshotCleanup");
            let snapshot_budget = (|| -> Result<()> {
                let len = std::fs::symlink_metadata(&snapshot.path)?.len();
                budget.consume(snapshot.path.as_str(), "record", len)
            })();
            if let Err(error) = snapshot_budget {
                return Err(self.committed_error(
                    "budgeting complete previous-generation snapshot cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            if let Err(error) = super::ohos::remove_immutable_durable_record(
                &snapshot,
                "managed complete previous-generation snapshot",
            ) {
                return Err(self.committed_error(
                    "removing complete previous-generation snapshot",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            self.journal.cleanup_snapshot_name = None;
            self.journal.cleanup_snapshot_identity = None;
            self.journal.cleanup_snapshot_digest = None;
            self.journal.cleanup_snapshot_len = None;
            self.journal.state = "snapshotClean".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording previous-generation snapshot cleanup",
                    error,
                    &backup,
                    Some(&snapshot_path),
                ));
            }
            #[cfg(test)]
            managed_crash_sync_point("afterSnapshotCleanup");
        } else {
            self.journal.backup_root_identity = None;
            self.journal.state = "backupClean".into();
            if let Err(error) = self.append_journal() {
                return Err(self.committed_error(
                    "recording empty previous backup cleanup",
                    error,
                    &backup,
                    None,
                ));
            }
        }
        self.journal.state = "complete".into();
        if let Err(error) = self.append_journal() {
            return Err(self.committed_error("recording complete state", error, &backup, None));
        }
        #[cfg(test)]
        managed_crash_sync_point("beforeJournalCleanup");
        if let Err(error) = self.remove_journals_with_budget(&mut budget) {
            return Err(self.committed_error(
                "cleaning completed append-only records",
                error,
                &backup,
                None,
            ));
        }
        #[cfg(test)]
        managed_crash_sync_point("afterJournalCleanup");
        self.completed = true;
        Ok(())
    }
}

impl Drop for ManagedPackageTransaction {
    fn drop(&mut self) {
        // Every normal build error is routed through `abort`, which reports
        // cleanup failures while the lock is held.  Drop must never retry and
        // swallow an identity violation (or delete after lock release).
        if !self.completed {
            self.private.preserve();
            self.build_temp.preserve();
        }
    }
}

impl ManagedLayout {
    fn rebased(&self, from: &Utf8Path, to: &Utf8Path) -> Result<Self> {
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
fn require_real_directory(path: &Utf8Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("{label} does not exist: {path}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} must be a real directory: {path}");
    }
    Ok(())
}

#[cfg(test)]
fn ensure_tree_has_no_native_artifacts(root: &Utf8Path) -> Result<()> {
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

fn write_file_atomically(path: &Utf8Path, bytes: &[u8]) -> Result<()> {
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
fn restore_file_atomically(path: &Utf8Path, previous: Option<&[u8]>) -> Result<()> {
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

fn harmony_archive_stem(package_name: &str) -> Result<String> {
    super::ohos::validate_oh_package_name(package_name)?;
    Ok(package_name.trim_start_matches('@').replace('/', "-"))
}

fn read_generated_json5(path: &Utf8Path) -> Result<serde_json::Value> {
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
) -> Result<super::ohos::HspOutputPaths> {
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
) -> Result<Vec<super::ohos::InvocationOutputSpec>> {
    let mut outputs = Vec::new();
    let mut add = |label: &str, path: Utf8PathBuf, is_directory: bool| {
        outputs.push(super::ohos::InvocationOutputSpec {
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
    destinations: &[super::ohos::InvocationOutputSpec],
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
    let mut generic_plan =
        super::ohos::GenericPublicationPlan::new(specs, std::slice::from_ref(&hsp_outputs))
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
            Ok(super::ohos::DirectCommitOutcome::Verified) => {
                generic_publication.finalize_hsp(hsp_publication)?;
                generic_publication.finalize()?;
            }
            Ok(super::ohos::DirectCommitOutcome::CommittedNeedsAudit(error)) => {
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
    let bytes = super::ohos::read_verified_regular_file_bounded(
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
    let prepared = (|| -> Result<ManagedPackageOwner> {
        transaction.clear_selected_roots(&targets)?;
        let private_args = transaction.private_args(&public_args)?;
        build_private_target_set(&private_args, &targets)?;
        rebase_private_javascript_host_crates(&public_args, &private_args, &targets)?;
        let meta = cargo_package_metadata(&public_args.manifest_path)?;
        transaction
            .private_layout()
            .emit(&targets, &meta, &private_args)
            .context("emitting complete private managed package manifest")?;
        validate_managed_manifest_candidate(transaction.private_layout(), &public_args.cargo_bin)?;
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
    if super::ohos::path_entry_exists(&framework_build_root)? {
        bail!(
            "fresh Apple framework build path unexpectedly exists without its creation-time witness: {framework_build_root}"
        );
    }
    std::fs::create_dir_all(&framework_build_root)
        .with_context(|| format!("creating framework build dir {framework_build_root}"))?;
    if super::ohos::path_entry_exists(&xcframework_out)? {
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
        super::ohos::capture_directory_for_cleanup(&framework_build_root)?;
    super::ohos::remove_captured_directory_for_cleanup(
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
    if super::ohos::path_entry_exists(&framework_dir)? {
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
    if super::ohos::path_entry_exists(staging_dir)? {
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
mod tests {
    use super::*;

    #[cfg(unix)]
    struct ManagedTestDirectoryCleanup {
        label: String,
        path: Utf8PathBuf,
        snapshot: super::super::ohos::OwnedTreeSnapshot,
    }

    #[cfg(unix)]
    struct ManagedTestCleanupPlan {
        directories: Vec<ManagedTestDirectoryCleanup>,
        owner_records: Vec<(String, super::super::ohos::DurableRecordWitness)>,
        snapshot_records: Vec<(String, super::super::ohos::DurableRecordWitness)>,
        journal_records: Vec<super::super::ohos::DurableRecordWitness>,
    }

    #[cfg(unix)]
    const HISTORICAL_MANAGED_MAX_ENTRIES: usize = 524_288;
    #[cfg(unix)]
    const HISTORICAL_MANAGED_MAX_BYTES: u64 = 16 * 1024 * 1024 * 1024;
    #[cfg(unix)]
    const HISTORICAL_MANAGED_MAX_DEPTH: usize = 4;

    #[cfg(unix)]
    fn historical_managed_budget() -> super::super::ohos::TraversalBudget {
        super::super::ohos::TraversalBudget::bounded(
            HISTORICAL_MANAGED_MAX_ENTRIES,
            HISTORICAL_MANAGED_MAX_BYTES,
        )
    }

    #[cfg(unix)]
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct ManagedTestGeneration {
        pid: u32,
        timestamp_nanos: u128,
    }

    #[cfg(unix)]
    fn parse_managed_test_generation(generation: &str) -> Result<ManagedTestGeneration> {
        let mut fields = generation.split('-');
        let pid = fields
            .next()
            .context("managed test generation has no PID component")?;
        let timestamp = fields
            .next()
            .context("managed test generation has no timestamp component")?;
        let nonce = fields
            .next()
            .context("managed test generation has no nonce component")?;
        if fields.next().is_some()
            || [pid, timestamp, nonce].iter().any(|value| {
                value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        {
            bail!(
                "managed test generation must be exactly positive-pid/timestamp/nonce hexadecimal fields: {generation}"
            );
        }
        let pid = u32::from_str_radix(pid, 16).with_context(|| {
            format!("managed test generation PID is not bounded hexadecimal: {generation}")
        })?;
        let pid_t = libc::pid_t::try_from(pid)
            .with_context(|| format!("managed test generation PID exceeds pid_t: {generation}"))?;
        if pid_t <= 0 {
            bail!("managed test generation PID must be positive: {generation}");
        }
        let timestamp = u128::from_str_radix(timestamp, 16).with_context(|| {
            format!("managed test generation timestamp overflows: {generation}")
        })?;
        if timestamp == 0 {
            bail!("managed test generation timestamp must be positive: {generation}");
        }
        let seconds = u64::try_from(timestamp / 1_000_000_000).with_context(|| {
            format!("managed test generation timestamp exceeds SystemTime: {generation}")
        })?;
        let subsecond_nanos = u32::try_from(timestamp % 1_000_000_000).unwrap();
        let created = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::new(seconds, subsecond_nanos))
            .with_context(|| {
                format!("managed test generation timestamp is invalid: {generation}")
            })?;
        if created > std::time::SystemTime::now() {
            bail!("managed test generation timestamp is in the future: {generation}");
        }
        // The first production generation deliberately uses nonce zero.  It is
        // still a required, bounded third field rather than an omitted witness.
        let nonce = u64::from_str_radix(nonce, 16)
            .with_context(|| format!("managed test generation nonce overflows: {generation}"))?;
        if format!("{pid:x}-{timestamp:x}-{nonce:x}") != generation {
            bail!("managed test generation is not canonical lowercase hexadecimal: {generation}");
        }
        Ok(ManagedTestGeneration {
            pid,
            timestamp_nanos: timestamp,
        })
    }

    #[cfg(unix)]
    fn managed_test_generation_pid(generation: &str) -> Result<u32> {
        Ok(parse_managed_test_generation(generation)?.pid)
    }

    #[cfg(unix)]
    fn managed_test_generation_with_budget(
        generation: &str,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<ManagedTestGeneration> {
        budget.consume(generation, "record", generation.len() as u64)?;
        parse_managed_test_generation(generation)
    }

    #[cfg(unix)]
    fn require_exited_test_pid(pid: u32, label: &str) -> Result<()> {
        let pid = libc::pid_t::try_from(pid)
            .with_context(|| format!("{label} producer PID exceeds positive pid_t"))?;
        if pid <= 0 {
            bail!("{label} producer PID must be a positive pid_t");
        }
        let result = unsafe { libc::kill(pid, 0) };
        if result == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        bail!("{label} producer PID {pid} is still live or cannot be proven exited")
    }

    #[cfg(unix)]
    fn parse_ps_elapsed(value: &str) -> Result<std::time::Duration> {
        let value = value.trim();
        let (days, clock) = match value.split_once('-') {
            Some((days, clock)) => (days.parse::<u64>()?, clock),
            None => (0, value),
        };
        let fields = clock
            .split(':')
            .map(str::parse::<u64>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let (hours, minutes, seconds) = match fields.as_slice() {
            [minutes, seconds] => (0, *minutes, *seconds),
            [hours, minutes, seconds] => (*hours, *minutes, *seconds),
            _ => bail!("unsupported ps elapsed-time value `{value}`"),
        };
        let seconds = days
            .checked_mul(24 * 60 * 60)
            .and_then(|value| value.checked_add(hours * 60 * 60))
            .and_then(|value| value.checked_add(minutes * 60))
            .and_then(|value| value.checked_add(seconds))
            .context("ps elapsed-time overflow")?;
        Ok(std::time::Duration::from_secs(seconds))
    }

    #[cfg(unix)]
    fn require_exited_managed_generation(
        generation: ManagedTestGeneration,
        label: &str,
    ) -> Result<u32> {
        let pid = generation.pid;
        if require_exited_test_pid(pid, label).is_ok() {
            return Ok(pid);
        }

        // `kill(pid, 0)` alone cannot distinguish a still-running producer
        // from a later process that reused the same PID.  The generation embeds
        // its creation time in nanoseconds.  When `ps` proves the current PID
        // instance started strictly after that generation (with a safety
        // margin), the original producer is necessarily gone.
        let now_nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let generation_age = now_nanos
            .checked_sub(generation.timestamp_nanos)
            .context("managed generation timestamp is in the future")?;
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "etime="])
            .output()
            .context("querying live PID elapsed time")?;
        if !output.status.success() {
            bail!("cannot prove whether {label} PID {pid} was reused")
        }
        let elapsed = parse_ps_elapsed(std::str::from_utf8(&output.stdout)?)?;
        let live_age_nanos = elapsed
            .checked_add(std::time::Duration::from_secs(5))
            .context("live PID elapsed-time margin overflow")?
            .as_nanos();
        if generation_age > live_age_nanos {
            return Ok(pid);
        }
        bail!("{label} producer PID {pid} still matches its generation lifetime")
    }

    #[cfg(unix)]
    fn managed_test_root_creator_pid(root: &Utf8Path) -> Result<u32> {
        let name = root
            .file_name()
            .with_context(|| format!("managed test root has no file name: {root}"))?;
        let mut pieces = name.rsplitn(3, '-');
        let nanos = pieces.next().context("managed test root lacks nonce")?;
        let pid_field = pieces
            .next()
            .context("managed test root lacks creator PID")?;
        let prefix = pieces.next().context("managed test root lacks prefix")?;
        if !prefix.starts_with("uniffi-managed-")
            || pid_field.is_empty()
            || !pid_field.bytes().all(|byte| byte.is_ascii_digit())
            || (pid_field.len() > 1 && pid_field.starts_with('0'))
            || nanos.is_empty()
            || !nanos.bytes().all(|byte| byte.is_ascii_digit())
            || (nanos.len() > 1 && nanos.starts_with('0'))
        {
            bail!("managed cleanup root is not a PID/nonce-bound test root: {root}");
        }
        let pid = pid_field
            .parse::<u32>()
            .with_context(|| format!("managed test root creator PID is invalid: {root}"))?;
        let pid_t = libc::pid_t::try_from(pid)
            .with_context(|| format!("managed test root creator PID exceeds pid_t: {root}"))?;
        if pid_t <= 0 {
            bail!("managed test root creator PID must be positive: {root}");
        }
        if pid != pid_t as u32 || pid.to_string() != pid_field {
            bail!("managed test root creator PID is not canonical: {root}");
        }
        let timestamp = nanos
            .parse::<u128>()
            .with_context(|| format!("managed test root timestamp overflows: {root}"))?;
        if timestamp == 0 {
            bail!("managed test root timestamp must be positive: {root}");
        }
        let seconds = u64::try_from(timestamp / 1_000_000_000)
            .with_context(|| format!("managed test root timestamp exceeds SystemTime: {root}"))?;
        let nanos = u32::try_from(timestamp % 1_000_000_000).unwrap();
        let created = std::time::UNIX_EPOCH
            .checked_add(std::time::Duration::new(seconds, nanos))
            .with_context(|| format!("managed test root timestamp is invalid: {root}"))?;
        if created > std::time::SystemTime::now() {
            bail!("managed test root timestamp is in the future: {root}");
        }
        Ok(pid)
    }

    #[cfg(unix)]
    fn managed_test_root_creator_pid_with_budget(
        root: &Utf8Path,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<u32> {
        budget.consume(root.as_str(), "record", root.as_str().len() as u64)?;
        managed_test_root_creator_pid(root)
    }

    #[cfg(unix)]
    fn exact_test_record_witness(
        path: &Utf8Path,
        maximum_bytes: u64,
        label: &str,
    ) -> Result<(Vec<u8>, super::super::ohos::DurableRecordWitness)> {
        let mut budget = super::super::ohos::TraversalBudget::managed();
        exact_test_record_witness_with_budget(path, maximum_bytes, label, &mut budget)
    }

    #[cfg(unix)]
    fn exact_test_record_witness_with_budget(
        path: &Utf8Path,
        maximum_bytes: u64,
        label: &str,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<(Vec<u8>, super::super::ohos::DurableRecordWitness)> {
        let metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("reading {label} metadata at {path}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("{label} must be a regular file before bounded allocation: {path}");
        }
        if metadata.len() > maximum_bytes {
            bail!("{label} exceeds its bounded size before allocation: {path}");
        }
        budget.consume(path.as_str(), "record", metadata.len())?;
        let (bytes, identity) =
            super::super::ohos::read_verified_regular_file_bounded_with_identity(
                path,
                maximum_bytes,
                label,
            )?;
        if bytes.len() as u64 != metadata.len() {
            bail!("{label} length changed after its pre-allocation budget witness: {path}");
        }
        let witness = super::super::ohos::DurableRecordWitness {
            path: path.to_path_buf(),
            identity,
            sha256: managed_record_digest(&bytes),
            len: bytes.len() as u64,
        };
        Ok((bytes, witness))
    }

    #[cfg(unix)]
    fn managed_test_transition_is_valid(previous: Option<&str>, state: &str) -> bool {
        matches!(
            (previous, state),
            (None, "prepared")
                | (Some("prepared"), "candidateCreated")
                | (Some("candidateCreated"), "building")
                | (Some("building"), "building" | "candidateReady")
                | (Some("candidateReady"), "buildClean")
                | (Some("buildClean"), "renamingPublicToBackup")
                | (Some("renamingPublicToBackup"), "publicBackedUp")
                | (Some("publicBackedUp"), "renamingCandidateToPublic")
                | (Some("renamingCandidateToPublic"), "candidatePublished")
                | (Some("candidatePublished"), "publishingFinalOwner")
                | (Some("publishingFinalOwner"), "committed")
                | (Some("committed"), "snapshottingBackup" | "backupClean")
                | (Some("snapshottingBackup"), "snapshotReady")
                | (Some("snapshotReady"), "cleaningBackup")
                | (Some("cleaningBackup"), "backupClean")
                | (Some("backupClean"), "cleaningSnapshot" | "complete")
                | (Some("cleaningSnapshot"), "snapshotClean")
                | (Some("snapshotClean"), "complete")
        )
    }

    #[cfg(unix)]
    fn capture_exact_managed_test_journals(
        parent: &Utf8Path,
        public_root: &Utf8Path,
        package_identity: &str,
        expected_producer_pid: Option<u32>,
    ) -> Result<
        Vec<(
            ManagedPackageJournal,
            super::super::ohos::DurableRecordWitness,
        )>,
    > {
        let mut budget = super::super::ohos::TraversalBudget::managed();
        capture_exact_managed_test_journals_with_budget(
            parent,
            public_root,
            package_identity,
            expected_producer_pid,
            &mut budget,
        )
    }

    #[cfg(unix)]
    fn consume_managed_test_journal_fields(
        journal: &ManagedPackageJournal,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<()> {
        for value in [
            Some(journal.public_root.as_str()),
            Some(journal.candidate_name.as_str()),
            Some(journal.build_name.as_str()),
            Some(journal.backup_name.as_str()),
            Some(journal.failed_name.as_str()),
            journal.previous_record_name.as_deref(),
            journal.cleanup_snapshot_name.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            budget.consume(value, "record", value.len() as u64)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn consume_managed_test_owner_paths(
        owner: &ManagedPackageOwner,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<()> {
        for entry in &owner.entries {
            budget.consume(&entry.path, "record", entry.path.len() as u64)?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn capture_exact_managed_test_journals_with_budget(
        parent: &Utf8Path,
        public_root: &Utf8Path,
        package_identity: &str,
        expected_producer_pid: Option<u32>,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<
        Vec<(
            ManagedPackageJournal,
            super::super::ohos::DurableRecordWitness,
        )>,
    > {
        let mut records = managed_record_paths_with_budget(parent, package_identity, budget)?
            .into_iter()
            .map(|path| {
                let (bytes, witness) = exact_test_record_witness_with_budget(
                    &path,
                    1024 * 1024,
                    "managed test transaction record",
                    budget,
                )?;
                let journal: ManagedPackageJournal = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing managed test journal {path}"))?;
                consume_managed_test_journal_fields(&journal, budget)?;
                let generation =
                    managed_test_generation_with_budget(&journal.generation, budget)?;
                let producer = generation.pid;
                validate_managed_journal(&journal, package_identity, public_root)?;
                if managed_journal_record_path(parent, &journal) != path {
                    bail!("managed test journal filename/content mismatch: {path}");
                }
                if expected_producer_pid.is_some_and(|expected| producer != expected) {
                    bail!(
                        "managed test journal producer PID {producer} does not match expected PID {:?}",
                        expected_producer_pid
                    );
                }
                require_exited_managed_generation(generation, "managed journal")?;
                Ok((journal, witness))
            })
            .collect::<Result<Vec<_>>>()?;
        records.sort_by(|left, right| {
            left.0
                .generation
                .cmp(&right.0.generation)
                .then_with(|| left.0.sequence.cmp(&right.0.sequence))
        });

        for generation in records
            .iter()
            .map(|(journal, _)| journal.generation.clone())
            .collect::<BTreeSet<_>>()
        {
            let chain = records
                .iter()
                .filter(|(journal, _)| journal.generation == generation)
                .collect::<Vec<_>>();
            let mut previous_name = None;
            let mut previous_identity = None;
            let mut previous_digest = None;
            let mut previous_state = None;
            for (index, (journal, witness)) in chain.into_iter().enumerate() {
                if journal.sequence != index as u64
                    || journal.previous_record_name != previous_name
                    || journal.previous_record_identity != previous_identity
                    || journal.previous_record_digest != previous_digest
                    || !managed_test_transition_is_valid(previous_state, &journal.state)
                {
                    bail!(
                        "managed test journal chain is partial/reordered at {}",
                        witness.path
                    );
                }
                previous_name = Some(
                    witness
                        .path
                        .file_name()
                        .context("managed test journal has no file name")?
                        .to_string(),
                );
                previous_identity = Some(witness.identity.clone());
                previous_digest = Some(witness.sha256.clone());
                previous_state = Some(journal.state.as_str());
            }
        }
        Ok(records)
    }

    #[cfg(unix)]
    fn push_expected_identity(
        expected: &mut std::collections::BTreeMap<
            Utf8PathBuf,
            Vec<super::super::ohos::PersistentFsIdentity>,
        >,
        path: Utf8PathBuf,
        identity: &Option<super::super::ohos::PersistentFsIdentity>,
    ) {
        if let Some(identity) = identity {
            let identities = expected.entry(path).or_default();
            if !identities.contains(identity) {
                identities.push(identity.clone());
            }
        }
    }

    #[cfg(unix)]
    fn capture_managed_test_directory(
        path: &Utf8Path,
        identities: &[super::super::ohos::PersistentFsIdentity],
        label: &str,
    ) -> Result<ManagedTestDirectoryCleanup> {
        let mut budget = super::super::ohos::TraversalBudget::managed();
        capture_managed_test_directory_with_budget(path, identities, label, &mut budget)
    }

    #[cfg(unix)]
    fn capture_managed_test_directory_with_budget(
        path: &Utf8Path,
        identities: &[super::super::ohos::PersistentFsIdentity],
        label: &str,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<ManagedTestDirectoryCleanup> {
        let current = super::super::ohos::persistent_fs_identity(path, true)?;
        if !identities.contains(&current) {
            bail!("{label} identity does not match any immutable managed plan witness: {path}");
        }
        let snapshot = super::super::ohos::capture_directory_for_cleanup_with_budget(path, budget)?;
        if super::super::ohos::persistent_fs_identity(path, true)? != current {
            bail!("{label} changed while sealing its test cleanup inventory: {path}");
        }
        Ok(ManagedTestDirectoryCleanup {
            label: label.into(),
            path: path.to_path_buf(),
            snapshot,
        })
    }

    #[cfg(unix)]
    fn capture_unplanned_but_pid_bound_test_directory(
        path: &Utf8Path,
        label: &str,
    ) -> Result<ManagedTestDirectoryCleanup> {
        let mut budget = super::super::ohos::TraversalBudget::managed();
        capture_unplanned_but_pid_bound_test_directory_with_budget(path, label, &mut budget)
    }

    #[cfg(unix)]
    fn capture_unplanned_but_pid_bound_test_directory_with_budget(
        path: &Utf8Path,
        label: &str,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<ManagedTestDirectoryCleanup> {
        let before = super::super::ohos::persistent_fs_identity(path, true)?;
        let snapshot = super::super::ohos::capture_directory_for_cleanup_with_budget(path, budget)?;
        if super::super::ohos::persistent_fs_identity(path, true)? != before {
            bail!("{label} changed while sealing its test cleanup inventory: {path}");
        }
        Ok(ManagedTestDirectoryCleanup {
            label: label.into(),
            path: path.to_path_buf(),
            snapshot,
        })
    }

    #[cfg(unix)]
    fn capture_empty_historical_managed_test_directory_with_budget(
        path: &Utf8Path,
        identities: &[super::super::ohos::PersistentFsIdentity],
        label: &str,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<ManagedTestDirectoryCleanup> {
        let current = super::super::ohos::persistent_fs_identity(path, true)?;
        if !identities.contains(&current) {
            bail!("{label} identity does not match any immutable managed root witness: {path}");
        }
        budget.consume(path.as_str(), "directory", 0)?;
        if let Some(entry) = std::fs::read_dir(path)
            .with_context(|| format!("reading historical managed directory {path}"))?
            .next()
            .transpose()?
        {
            let nested = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "historical managed nested path is not utf8: {}",
                    path.display()
                )
            })?;
            budget.consume_entry_path(nested.as_str())?;
            bail!("{label} has no persisted nested inventory and is non-empty; preserving {path}");
        }
        let snapshot = super::super::ohos::capture_directory_for_cleanup_with_budget(path, budget)?;
        let appeared = std::fs::read_dir(path)?.next().transpose()?;
        if let Some(entry) = &appeared {
            let nested = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!(
                    "historical managed nested path is not utf8: {}",
                    path.display()
                )
            })?;
            budget.consume_entry_path(nested.as_str())?;
        }
        if !snapshot.is_empty()
            || super::super::ohos::persistent_fs_identity(path, true)? != current
            || appeared.is_some()
        {
            bail!(
                "{label} changed or became non-empty while proving historical emptiness; preserving {path}"
            );
        }
        Ok(ManagedTestDirectoryCleanup {
            label: label.into(),
            path: path.to_path_buf(),
            snapshot,
        })
    }

    #[cfg(unix)]
    fn capture_empty_unwitnessed_historical_control_with_budget(
        path: &Utf8Path,
        label: &str,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<ManagedTestDirectoryCleanup> {
        let identity = super::super::ohos::persistent_fs_identity(path, true)?;
        capture_empty_historical_managed_test_directory_with_budget(
            path,
            std::slice::from_ref(&identity),
            label,
            budget,
        )
    }

    #[cfg(unix)]
    fn managed_owner_root(owner: &ManagedPackageOwner) -> Result<Utf8PathBuf> {
        let roots = owner
            .entries
            .iter()
            .map(|entry| {
                let path = Utf8Path::new(&entry.path);
                if path.file_name() == Some("artifact-manifest.json") {
                    return path
                        .parent()
                        .map(Utf8Path::to_path_buf)
                        .context("managed owner manifest has no package root");
                }
                if path.file_name() == Some("artifacts") {
                    return path
                        .parent()
                        .map(Utf8Path::to_path_buf)
                        .context("managed owner artifacts has no package root");
                }
                if path.file_name() == Some("ffi")
                    && path.parent().and_then(Utf8Path::file_name) == Some("src")
                {
                    return path
                        .parent()
                        .and_then(Utf8Path::parent)
                        .map(Utf8Path::to_path_buf)
                        .context("managed owner ffi path has no package root");
                }
                if path.parent().and_then(Utf8Path::file_name) == Some("src")
                    && path
                        .file_name()
                        .is_some_and(|name| name.starts_with("index.") && name.ends_with(".ts"))
                {
                    return path
                        .parent()
                        .and_then(Utf8Path::parent)
                        .map(Utf8Path::to_path_buf)
                        .context("managed owner entrypoint has no package root");
                }
                bail!("managed test owner contains an unknown controlled path: {path}")
            })
            .collect::<Result<BTreeSet<_>>>()?;
        if roots.len() != 1 {
            bail!("managed test owner entries do not bind one package root");
        }
        Ok(roots.into_iter().next().unwrap())
    }

    fn managed_record_paths(parent: &Utf8Path, digest: &str) -> Vec<Utf8PathBuf> {
        let mut budget = super::super::ohos::TraversalBudget::managed();
        managed_record_paths_with_budget(parent, digest, &mut budget)
            .expect("enumerating bounded managed transaction records")
    }

    fn managed_record_paths_with_budget(
        parent: &Utf8Path,
        digest: &str,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<Vec<Utf8PathBuf>> {
        let prefix = managed_journal_prefix(digest);
        let mut paths = Vec::new();
        for entry in std::fs::read_dir(parent)
            .with_context(|| format!("reading managed transaction parent {parent}"))?
        {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!("managed transaction path is not utf8: {}", path.display())
            })?;
            // Consume the shared hard limit before retaining this entry or
            // allocating any directory-wide collection.
            budget.consume(path.as_str(), "record", 0)?;
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                paths.push(path);
            }
        }
        paths.sort();
        Ok(paths)
    }

    #[cfg(unix)]
    fn plan_exact_managed_test_cleanup(
        public_root: &Utf8Path,
        control_roots: &[Utf8PathBuf],
        expected_producer_pid: Option<u32>,
        require_root_creator_exited: bool,
    ) -> Result<ManagedTestCleanupPlan> {
        let mut budget = super::super::ohos::TraversalBudget::managed();
        plan_exact_managed_test_cleanup_with_budget(
            public_root,
            control_roots,
            expected_producer_pid,
            require_root_creator_exited,
            &mut budget,
        )
    }

    #[cfg(unix)]
    fn plan_exact_managed_test_cleanup_with_budget(
        public_root: &Utf8Path,
        control_roots: &[Utf8PathBuf],
        expected_producer_pid: Option<u32>,
        require_root_creator_exited: bool,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<ManagedTestCleanupPlan> {
        let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .map_err(|path| anyhow::anyhow!("system temp path is not utf8: {}", path.display()))?
            .canonicalize_utf8()?;
        let parent = public_root
            .parent()
            .context("managed test public root has no parent")?;
        if !public_root.starts_with(&temp) {
            bail!("managed test cleanup root escaped system temp: {public_root}");
        }
        if parent == temp {
            let creator = managed_test_root_creator_pid_with_budget(public_root, budget)?;
            if require_root_creator_exited {
                require_exited_test_pid(creator, "managed test root")?;
            } else if creator != std::process::id() {
                bail!(
                    "managed test root creator PID {creator} does not match current test PID {}",
                    std::process::id()
                );
            }
        } else if !require_root_creator_exited || expected_producer_pid.is_some() {
            // Nested roots are produced by public integration tests inside a
            // random `TempDir`; their basename intentionally carries no PID.
            // Historical cleanup may select them only from an exact journal
            // chain whose generation PID is independently proven exited.
            bail!(
                "nested managed test cleanup requires historical exact-journal discovery: {public_root}"
            );
        }

        let package_identity = managed_package_digest(public_root);
        let records = capture_exact_managed_test_journals_with_budget(
            parent,
            public_root,
            &package_identity,
            expected_producer_pid,
            budget,
        )?;
        let mut expected_directories = std::collections::BTreeMap::<
            Utf8PathBuf,
            Vec<super::super::ohos::PersistentFsIdentity>,
        >::new();
        let mut snapshot_records = Vec::<(String, super::super::ohos::DurableRecordWitness)>::new();
        let mut planned_residue_paths = BTreeSet::new();
        let mut planned_directory_paths = BTreeSet::new();
        let mut planned_snapshot_paths = BTreeSet::new();
        for (journal, _) in &records {
            let candidate = parent.join(&journal.candidate_name);
            let displaced_candidate = parent.join(format!("{}.displaced", journal.candidate_name));
            let build = parent.join(&journal.build_name);
            let backup = parent.join(&journal.backup_name);
            let failed = parent.join(&journal.failed_name);
            let planned_directories = [
                candidate.clone(),
                displaced_candidate.clone(),
                build.clone(),
                backup.clone(),
                failed.clone(),
            ];
            planned_residue_paths.extend(planned_directories.iter().cloned());
            planned_directory_paths.extend(planned_directories);
            push_expected_identity(
                &mut expected_directories,
                public_root.to_path_buf(),
                &journal.previous_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                public_root.to_path_buf(),
                &journal.candidate_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                public_root.to_path_buf(),
                &journal.published_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                candidate.clone(),
                &journal.candidate_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                displaced_candidate,
                &journal.candidate_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                build,
                &journal.build_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                backup.clone(),
                &journal.previous_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                backup,
                &journal.backup_root_identity,
            );
            push_expected_identity(
                &mut expected_directories,
                failed,
                &journal.candidate_root_identity,
            );

            if let Some(name) = &journal.cleanup_snapshot_name {
                let path = parent.join(name);
                planned_residue_paths.insert(path.clone());
                planned_snapshot_paths.insert(path.clone());
                if super::super::ohos::path_entry_exists(&path)? {
                    let (Some(identity), Some(digest), Some(len)) = (
                        &journal.cleanup_snapshot_identity,
                        &journal.cleanup_snapshot_digest,
                        journal.cleanup_snapshot_len,
                    ) else {
                        // An earlier `snapshottingBackup` record carries only
                        // the planned name.  A later record in this immutable
                        // chain may carry the exact file witness; defer the
                        // decision until the full chain and committed owner
                        // have both been inspected.
                        continue;
                    };
                    let (bytes, witness) = exact_test_record_witness_with_budget(
                        &path,
                        1024 * 1024 * 1024,
                        "managed test previous-generation snapshot",
                        budget,
                    )?;
                    if &witness.identity != identity
                        || witness.sha256 != *digest
                        || witness.len != len
                        || bytes.len() as u64 != len
                    {
                        bail!("managed test cleanup snapshot witness mismatch: {path}");
                    }
                    if !snapshot_records
                        .iter()
                        .any(|(_, existing)| existing.path == path)
                    {
                        snapshot_records
                            .push(("managed test previous-generation snapshot".into(), witness));
                    }
                }
            }
        }

        let final_owner = managed_owner_path(public_root);
        let mut owner_records = Vec::new();
        let mut parsed_owners = Vec::new();
        if super::super::ohos::path_entry_exists(&final_owner)? {
            let (bytes, witness) = exact_test_record_witness_with_budget(
                &final_owner,
                16 * 1024 * 1024,
                "managed test final owner",
                budget,
            )?;
            let owner: ManagedPackageOwner = serde_json::from_slice(&bytes)?;
            consume_managed_test_owner_paths(&owner, budget)?;
            if owner.owner != MANAGED_PACKAGE_OWNER_KIND
                || owner.schema_version != MANAGED_PACKAGE_OWNER_SCHEMA_VERSION
                || owner.state != "committed"
                || managed_owner_root(&owner)? != public_root
                || managed_owner_path(public_root) != final_owner
            {
                bail!("managed test final owner is not bound to {public_root}");
            }
            let generation = managed_test_generation_with_budget(&owner.generation, budget)?;
            let producer = generation.pid;
            if expected_producer_pid.is_some_and(|expected| producer != expected) {
                bail!(
                    "managed final-owner producer PID {producer} does not match expected PID {:?}",
                    expected_producer_pid
                );
            }
            require_exited_managed_generation(generation, "managed final owner")?;
            if super::super::ohos::path_entry_exists(public_root)? {
                validate_managed_owner_with_budget(public_root, &owner, budget)?;
            }
            push_expected_identity(
                &mut expected_directories,
                public_root.to_path_buf(),
                &Some(owner.root_identity.clone()),
            );
            parsed_owners.push(owner);
            owner_records.push(("managed test final owner".into(), witness));
        }

        // The pre-rename owner candidate is deliberately outside the journal
        // payload, so discover only its exact generation-derived pathname and
        // require its bytes to validate the live public generation.
        for generation in records
            .iter()
            .map(|(journal, _)| journal.generation.as_str())
            .collect::<BTreeSet<_>>()
        {
            let owner_name = final_owner
                .file_name()
                .context("managed final owner has no file name")?;
            let candidate = parent.join(format!(".{owner_name}.next-{generation}"));
            planned_residue_paths.insert(candidate.clone());
            if !super::super::ohos::path_entry_exists(&candidate)? {
                continue;
            }
            let (bytes, witness) = exact_test_record_witness_with_budget(
                &candidate,
                16 * 1024 * 1024,
                "managed test owner candidate",
                budget,
            )?;
            let owner: ManagedPackageOwner = serde_json::from_slice(&bytes)?;
            consume_managed_test_owner_paths(&owner, budget)?;
            managed_test_generation_with_budget(&owner.generation, budget)?;
            if owner.owner != MANAGED_PACKAGE_OWNER_KIND
                || owner.schema_version != MANAGED_PACKAGE_OWNER_SCHEMA_VERSION
                || owner.state != "committed"
                || owner.generation != generation
                || managed_owner_root(&owner)? != public_root
            {
                bail!("managed test owner candidate is not plan/generation-bound: {candidate}");
            }
            validate_managed_owner_with_budget(public_root, &owner, budget)?;
            push_expected_identity(
                &mut expected_directories,
                public_root.to_path_buf(),
                &Some(owner.root_identity.clone()),
            );
            parsed_owners.push(owner);
            owner_records.push(("managed test owner candidate".into(), witness));
        }

        // A snapshot name inferred from an owner generation is not an object
        // witness. Only a journal that persisted identity+digest+length above
        // may authorize deletion; an owner-only same-path file is preserved.
        for path in &planned_snapshot_paths {
            if super::super::ohos::path_entry_exists(path)?
                && !snapshot_records
                    .iter()
                    .any(|(_, witness)| witness.path == *path)
            {
                bail!(
                    "managed historical cleanup snapshot exists without a persisted identity/digest/length witness; preserving {path}"
                );
            }
        }

        // Reject every unplanned object sharing the package transaction prefix
        // before any deletion.  This makes a same-name replacement or forged
        // residue a preserve-and-report outcome rather than an adopted object.
        let residue_prefix = format!(".uniffi-managed-package-{package_identity}-");
        for entry in std::fs::read_dir(parent)? {
            let entry = entry?;
            let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                anyhow::anyhow!("managed test residue path is not utf8: {}", path.display())
            })?;
            budget.consume(path.as_str(), "record", 0)?;
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with(&residue_prefix) && !planned_residue_paths.contains(&path) {
                bail!("managed test cleanup found unplanned package residue: {path}");
            }
        }

        let mut directories = Vec::new();
        for (path, identities) in expected_directories {
            if !super::super::ohos::path_entry_exists(&path)? {
                continue;
            }
            let label = if path == public_root {
                "managed test public root"
            } else {
                "managed test planned private root"
            };
            let directory = if require_root_creator_exited {
                capture_empty_historical_managed_test_directory_with_budget(
                    &path,
                    &identities,
                    label,
                    budget,
                )?
            } else {
                capture_managed_test_directory_with_budget(&path, &identities, label, budget)?
            };
            directories.push(directory);
        }
        for path in &planned_directory_paths {
            if super::super::ohos::path_entry_exists(path)?
                && !directories.iter().any(|directory| directory.path == *path)
            {
                bail!(
                    "managed historical planned directory exists without an exact root identity witness; preserving {path}"
                );
            }
        }
        if super::super::ohos::path_entry_exists(public_root)?
            && !directories
                .iter()
                .any(|directory| directory.path == public_root)
        {
            bail!("managed live public root has no exact journal/owner identity: {public_root}");
        }
        for control in control_roots {
            if control.parent() != Some(parent)
                || !control.file_name().is_some_and(|name| {
                    name.starts_with(&format!(".{}-", public_root.file_name().unwrap()))
                        && name.ends_with("-control")
                })
            {
                bail!("managed test control root is not bound to public root name: {control}");
            }
            if super::super::ohos::path_entry_exists(control)? {
                directories.push(if require_root_creator_exited {
                    capture_empty_unwitnessed_historical_control_with_budget(
                        control,
                        "managed test synchronization root",
                        budget,
                    )?
                } else {
                    capture_unplanned_but_pid_bound_test_directory_with_budget(
                        control,
                        "managed test synchronization root",
                        budget,
                    )?
                });
            }
        }
        if require_root_creator_exited {
            let control_prefix = format!(".{}-", public_root.file_name().unwrap());
            for entry in std::fs::read_dir(parent)? {
                let entry = entry?;
                let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                    anyhow::anyhow!(
                        "historical managed control path is not utf8: {}",
                        path.display()
                    )
                })?;
                budget.consume(path.as_str(), "directory", 0)?;
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.starts_with(&control_prefix) || !name.ends_with("-control") {
                    continue;
                }
                if !directories.iter().any(|directory| directory.path == path) {
                    directories.push(capture_empty_unwitnessed_historical_control_with_budget(
                        &path,
                        "historical managed synchronization root",
                        budget,
                    )?);
                }
            }
        }

        // Every parsed owner was validated before any output capture, and the
        // output snapshots above are now sealed.  Keep owner records separate
        // so execution can enforce output-before-owner ordering.
        let _ = parsed_owners;
        Ok(ManagedTestCleanupPlan {
            directories,
            owner_records,
            snapshot_records,
            journal_records: records.into_iter().map(|(_, witness)| witness).collect(),
        })
    }

    #[cfg(unix)]
    fn execute_exact_managed_test_cleanup(plan: ManagedTestCleanupPlan) -> Result<()> {
        let mut budget = super::super::ohos::TraversalBudget::managed();
        execute_exact_managed_test_cleanup_with_budget(plan, &mut budget)
    }

    #[cfg(unix)]
    fn execute_exact_managed_test_cleanup_with_budget(
        mut plan: ManagedTestCleanupPlan,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<()> {
        // Public/private outputs and synchronization roots are removed from
        // their pre-captured identity/inventory witnesses first.
        for directory in &plan.directories {
            super::super::ohos::remove_captured_directory_for_cleanup_with_budget(
                &directory.path,
                &directory.snapshot,
                budget,
            )
            .with_context(|| {
                format!(
                    "removing {} from exact test witness: {}",
                    directory.label, directory.path
                )
            })?;
        }
        for (label, witness) in &plan.snapshot_records {
            budget.consume(witness.path.as_str(), "record", witness.len)?;
            super::super::ohos::remove_immutable_durable_record(witness, label)?;
        }
        for (label, witness) in &plan.owner_records {
            budget.consume(witness.path.as_str(), "record", witness.len)?;
            super::super::ohos::remove_immutable_durable_record(witness, label)?;
        }
        // Newest-to-oldest preserves a valid immutable prefix if the test
        // process itself is interrupted during evidence cleanup.
        plan.journal_records
            .sort_by(|left, right| left.path.cmp(&right.path));
        for witness in plan.journal_records.iter().rev() {
            budget.consume(witness.path.as_str(), "record", witness.len)?;
            super::super::ohos::remove_immutable_durable_record(
                witness,
                "managed test transaction journal",
            )?;
        }
        Ok(())
    }

    #[cfg(unix)]
    fn cleanup_exact_managed_test_case(
        public_root: &Utf8Path,
        control_roots: &[Utf8PathBuf],
        expected_producer_pid: Option<u32>,
    ) -> Result<()> {
        // Discovery intentionally happens here, after the producer exited and
        // after all assertions.  Never reuse the pre-case empty journal list.
        let plan = plan_exact_managed_test_cleanup(
            public_root,
            control_roots,
            expected_producer_pid,
            false,
        )?;
        execute_exact_managed_test_cleanup(plan)?;
        let digest = managed_package_digest(public_root);
        if super::super::ohos::path_entry_exists(public_root)?
            || super::super::ohos::path_entry_exists(&managed_owner_path(public_root))?
            || !managed_record_paths(public_root.parent().unwrap(), &digest).is_empty()
        {
            bail!("managed test cleanup left root/owner/journal evidence for {public_root}");
        }
        Ok(())
    }

    #[cfg(unix)]
    fn historical_managed_test_roots() -> (BTreeSet<Utf8PathBuf>, Vec<String>) {
        let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .unwrap()
            .canonicalize_utf8()
            .unwrap();
        let mut budget = historical_managed_budget();
        historical_managed_test_roots_under(&temp, &mut budget)
            .unwrap_or_else(|error| (BTreeSet::new(), vec![format!("{temp}: {error:#}")]))
    }

    #[cfg(unix)]
    fn historical_managed_test_roots_under(
        temp: &Utf8Path,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<(BTreeSet<Utf8PathBuf>, Vec<String>)> {
        let mut roots = BTreeSet::new();
        let mut preserved = Vec::new();
        // Public integration tests use random nested TempDirs (`.tmp*` is an
        // implementation detail, not an ownership proof).  Discover their
        // immutable journals by schema/path instead of by a top-level name,
        // while bounding both depth and the total namespace inspected.
        let mut queue = std::collections::VecDeque::from([(temp.to_path_buf(), 0usize)]);
        while let Some((directory, depth)) = queue.pop_front() {
            let entries = match std::fs::read_dir(&directory) {
                Ok(entries) => entries,
                // An unreadable arbitrary system-temp directory is not managed
                // evidence.  Only a matched journal pathname may enter the
                // preserve/report set below.
                Err(_) => continue,
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(_) => {
                        // The iterator did not expose a pathname, but the
                        // namespace item still consumes the shared count.
                        budget.consume_entry_bytes(&[])?;
                        continue;
                    }
                };
                let path = match historical_utf8_path_with_budget(entry.path(), budget)? {
                    Some(path) => path,
                    None => continue,
                };
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(_) => continue,
                };
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    if depth < HISTORICAL_MANAGED_MAX_DEPTH {
                        queue.push_back((path, depth + 1));
                    }
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                if !file_type.is_file()
                    || !name.starts_with(".uniffi-managed-package-transaction-")
                    || !name.ends_with(".json")
                {
                    continue;
                }
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) if metadata.is_file() && metadata.len() <= 1024 * 1024 => metadata,
                    Ok(_) => {
                        preserved.push(format!(
                            "{path}: historical nested managed journal has an unsafe type or size"
                        ));
                        continue;
                    }
                    Err(error) => {
                        preserved.push(format!("{path}: reading journal metadata: {error}"));
                        continue;
                    }
                };
                // Account JSON bytes before the bounded reader allocates.
                budget.consume(path.as_str(), "record", metadata.len())?;
                let (bytes, _) = match exact_test_record_witness(
                    &path,
                    1024 * 1024,
                    "historical nested managed journal",
                ) {
                    Ok(value) => value,
                    Err(error) => {
                        preserved.push(format!("{path}: {error:#}"));
                        continue;
                    }
                };
                let journal: ManagedPackageJournal = match serde_json::from_slice(&bytes)
                    .context("parsing historical nested managed journal")
                {
                    Ok(journal) => journal,
                    Err(error) => {
                        preserved.push(format!("{path}: {error:#}"));
                        continue;
                    }
                };
                consume_managed_test_journal_fields(&journal, budget)?;
                let generation = managed_test_generation_with_budget(&journal.generation, budget)?;
                let result = (|| -> Result<Utf8PathBuf> {
                    let public_root = Utf8PathBuf::from(&journal.public_root);
                    if !public_root.starts_with(&temp)
                        || public_root.parent() != path.parent()
                        || managed_package_digest(&public_root) != journal.package_identity
                    {
                        bail!("managed journal does not bind a nested system-temp public root");
                    }
                    validate_managed_journal(&journal, &journal.package_identity, &public_root)?;
                    if managed_journal_record_path(path.parent().unwrap(), &journal) != path {
                        bail!("managed journal filename/content mismatch");
                    }
                    require_exited_managed_generation(
                        generation,
                        "historical nested managed journal",
                    )?;
                    Ok(public_root)
                })();
                match result {
                    Ok(root) => {
                        roots.insert(root);
                    }
                    Err(error) => preserved.push(format!("{path}: {error:#}")),
                }
            }
        }
        Ok((roots, preserved))
    }

    #[cfg(unix)]
    fn historical_utf8_path_with_budget(
        path: std::path::PathBuf,
        budget: &mut super::super::ohos::TraversalBudget,
    ) -> Result<Option<Utf8PathBuf>> {
        budget.consume_entry_bytes(path.as_os_str().as_encoded_bytes())?;
        Ok(Utf8PathBuf::from_path_buf(path).ok())
    }

    #[cfg(unix)]
    fn cleanup_exited_historical_managed_test_controls() -> (usize, Vec<String>) {
        let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .unwrap()
            .canonicalize_utf8()
            .unwrap();
        let mut budget = historical_managed_budget();
        let (roots, mut preserved) = match historical_managed_test_roots_under(&temp, &mut budget) {
            Ok(value) => value,
            Err(error) => return (0, vec![format!("{temp}: {error:#}")]),
        };
        // Seal every independently provable group before the first deletion.
        // A traversal-limit failure therefore discards all prepared plans and
        // makes this invocation a zero-deletion audit.
        let mut plans = Vec::new();
        for root in roots {
            if root.parent() == Some(temp.as_path()) {
                let creator = match managed_test_root_creator_pid_with_budget(&root, &mut budget) {
                    Ok(pid) => pid,
                    Err(error) => {
                        preserved.push(format!("{root}: {error:#}"));
                        continue;
                    }
                };
                if let Err(error) = require_exited_test_pid(creator, "historical managed root") {
                    preserved.push(format!("{root}: {error:#}"));
                    continue;
                }
            }
            match plan_exact_managed_test_cleanup_with_budget(&root, &[], None, true, &mut budget) {
                Ok(plan) => plans.push((root, plan)),
                Err(error) => {
                    let report = format!("{root}: {error:#}");
                    if report.contains("traversal") || report.contains("checked path limit") {
                        preserved.push(report);
                        return (0, preserved);
                    }
                    preserved.push(report);
                }
            }
        }
        let reservation = plans.iter().try_fold(
            (0usize, 0u64),
            |(entries, bytes), (_, plan)| -> Result<(usize, u64)> {
                // Historical directory plans are empty-only. Thirty-two entry
                // units conservatively cover every identity/token/inventory
                // validation and the final root removal before mutation.
                let directory_entries = plan
                    .directories
                    .len()
                    .checked_mul(32)
                    .context("historical empty-directory cleanup reservation overflow")?;
                let record_count = plan
                    .owner_records
                    .len()
                    .checked_add(plan.snapshot_records.len())
                    .and_then(|value| value.checked_add(plan.journal_records.len()))
                    .context("historical record cleanup reservation overflow")?;
                let entries = entries
                    .checked_add(directory_entries)
                    .and_then(|value| value.checked_add(record_count))
                    .context("historical cleanup entry reservation overflow")?;
                let bytes = plan
                    .owner_records
                    .iter()
                    .map(|(_, witness)| witness.len)
                    .chain(plan.snapshot_records.iter().map(|(_, witness)| witness.len))
                    .chain(plan.journal_records.iter().map(|witness| witness.len))
                    .try_fold(bytes, |total, value| {
                        total
                            .checked_add(value)
                            .context("historical cleanup byte reservation overflow")
                    })?;
                Ok((entries, bytes))
            },
        );
        let (reserved_entries, reserved_bytes) = match reservation {
            Ok(value) => value,
            Err(error) => {
                preserved.push(format!("{temp}: {error:#}"));
                return (0, preserved);
            }
        };
        if let Err(error) = budget.require_remaining(reserved_entries, reserved_bytes) {
            preserved.push(format!("{temp}: {error:#}"));
            return (0, preserved);
        }
        let mut cleaned = 0usize;
        for (root, plan) in plans {
            match execute_exact_managed_test_cleanup_with_budget(plan, &mut budget) {
                Ok(()) => cleaned += 1,
                Err(error) => preserved.push(format!("{root}: {error:#}")),
            }
        }
        (cleaned, preserved)
    }

    #[cfg(unix)]
    fn historical_managed_cleanup_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap()
    }

    fn empty_build_args() -> BuildArgs {
        BuildArgs {
            manifest_path: Utf8PathBuf::from("/repo/crates/core/Cargo.toml"),
            out_dir: Some(Utf8PathBuf::from("/repo/generated")),
            target: vec![ArtifactTargetArg::Wasm],
            library_path: None,
            source: None,
            host_crates_dir: None,
            logical_host_crates_dir: None,
            invocation_output_lock_held: false,
            artifact_dir: None,
            managed_layout: false,
            package_dir: None,
            release: false,
            cargo_features: Vec::new(),
            cargo_bin: "cargo".to_string(),
            no_format: false,
            config: None,
            crate_name: None,
            metadata_no_deps: false,
            wasm_bindgen_out_dir: None,
            wasm_bindgen_target: WasmBindgenTargetArg::Web,
            napi_target_dir: None,
            wasm_target_dir: None,
            wasm_core_target_dir: None,
            ohos_dist_dir: None,
            ohos_package_name: None,
            ohos_module_name: None,
            ohos_package_version: None,
            ohos_author: None,
            ohos_license: None,
            ohos_description: None,
            ohos_compatible_sdk_version: None,
            ohos_compatible_sdk_type: None,
            ohos_device_types: Vec::new(),
            ohos_package_kind: super::super::ohos::PackageKind::Har,
            ohos_integrated_hsp: false,
            ohos_hsp_bundle_name: None,
            ohos_har_out: None,
            ohos_runtime_hsp_out: None,
            ohos_interface_har_out: None,
            ohos_tgz_out: None,
            ohos_hvigorw: None,
            ohos_ohpm: None,
            ohos_deveco_sdk_home: None,
            ohos_no_har: false,
            ohos_arch: Vec::new(),
            ohos_target_dir: None,
            ohos_static: false,
            ohos_skip_libs: false,
            ohos_dts_cache: false,
            ohos_skip_check: false,
            ohos_zigbuild: false,
            ohos_bisheng: false,
            ohos_package: None,
            ohos_skip_napi_check: false,
            ohos_soname: None,
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

    fn test_cargo_metadata(target_directory: Utf8PathBuf) -> CargoPackageMetadata {
        CargoPackageMetadata {
            target_directory,
            package_name: "uni-core".to_string(),
            package_version: "0.1.0".to_string(),
            description: Some("Uni Core test package".to_string()),
            authors: vec!["Uni Core Team".to_string()],
            license: Some("MPL-2.0".to_string()),
            lib_target_name: "uni_core".to_string(),
        }
    }

    fn unique_tmp_dir(name: &str) -> Utf8PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Utf8PathBuf::from_path_buf(
            std::env::temp_dir().join(format!("uniffi-{name}-{}-{nanos}", std::process::id())),
        )
        .unwrap()
    }

    fn regular_file_snapshot(root: &Utf8Path) -> std::collections::BTreeMap<Utf8PathBuf, Vec<u8>> {
        fn visit(
            root: &Utf8Path,
            current: &Utf8Path,
            snapshot: &mut std::collections::BTreeMap<Utf8PathBuf, Vec<u8>>,
        ) {
            for entry in std::fs::read_dir(current).unwrap() {
                let entry = entry.unwrap();
                let path = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
                if entry.file_type().unwrap().is_dir() {
                    visit(root, &path, snapshot);
                } else {
                    snapshot.insert(
                        path.strip_prefix(root).unwrap().to_path_buf(),
                        std::fs::read(path).unwrap(),
                    );
                }
            }
        }
        let mut snapshot = std::collections::BTreeMap::new();
        if root.exists() {
            visit(root, root, &mut snapshot);
        }
        snapshot
    }

    fn write_test_manifest(package_dir: &Utf8Path) -> Utf8PathBuf {
        let src_dir = package_dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("lib.rs"), "pub fn marker() {}\n").unwrap();
        let manifest = package_dir.join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"[package]
name = "uni-core"
version = "0.1.0"
edition = "2021"

[lib]
name = "uni_core"
"#,
        )
        .unwrap();
        manifest
    }

    fn test_managed_prepared_journal(public_root: &Utf8Path) -> ManagedPackageJournal {
        test_managed_prepared_journal_for_generation(public_root, new_managed_generation())
    }

    fn test_managed_prepared_journal_for_generation(
        public_root: &Utf8Path,
        generation: String,
    ) -> ManagedPackageJournal {
        let package_identity = managed_package_digest(public_root);
        let public_name = public_root.file_name().unwrap();
        ManagedPackageJournal {
            owner: MANAGED_PACKAGE_JOURNAL_KIND.into(),
            schema_version: MANAGED_PACKAGE_JOURNAL_SCHEMA_VERSION,
            package_identity: package_identity.clone(),
            generation: generation.clone(),
            sequence: 0,
            previous_record_name: None,
            previous_record_identity: None,
            previous_record_digest: None,
            state: "prepared".into(),
            public_root: public_root.to_string(),
            candidate_name: format!(".uniffi-managed-package-{package_identity}-{generation}-next"),
            build_name: format!(".uniffi-managed-package-{package_identity}-{generation}-build"),
            backup_name: format!(
                ".uniffi-managed-package-{package_identity}-{generation}-{public_name}-backup"
            ),
            failed_name: format!(
                ".uniffi-managed-package-{package_identity}-{generation}-{public_name}-failed"
            ),
            previous_root_identity: None,
            candidate_root_identity: None,
            build_root_identity: None,
            backup_root_identity: None,
            published_root_identity: None,
            cleanup_snapshot_name: None,
            cleanup_snapshot_identity: None,
            cleanup_snapshot_digest: None,
            cleanup_snapshot_len: None,
        }
    }

    fn write_owned_harmony_dist(dist: &Utf8Path, contents: &str) {
        std::fs::create_dir_all(dist).unwrap();
        for file in [
            "index.d.ts",
            "Index.d.ets",
            "harmony-facade-contract.json",
            "native-facade.ets",
            "package-index.ets",
        ] {
            std::fs::write(dist.join(file), format!("{file}:{contents}\n")).unwrap();
        }
        super::super::ohos::write_owned_tree_marker(
            dist,
            ".uniffi-ohos-dist-owner",
            "uniffi-ohos-dist",
        )
        .unwrap();
    }

    fn populate_private_harmony(
        transaction: &ManagedHarmonyTransaction,
        args: &BuildArgs,
        contents: &str,
    ) {
        let root = transaction.private_root();
        write_owned_harmony_dist(&root.join("dist"), contents);
        if args.ohos_no_har {
            return;
        }
        let package = root.join("package");
        std::fs::create_dir_all(package.join("src/main")).unwrap();
        let hsp = args.ohos_package_kind == super::super::ohos::PackageKind::Hsp;
        std::fs::write(
            package.join("oh-package.json5"),
            if hsp {
                r#"{"name":"uni-core-ohos","version":"0.1.0","main":"Index.ets","packageType":"InterfaceHar"}"#
            } else {
                r#"{"name":"uni-core-ohos","version":"0.1.0","main":"Index.ets"}"#
            },
        )
        .unwrap();
        std::fs::write(
            package.join("harmony-facade-contract.json"),
            format!("contract:{contents}\n"),
        )
        .unwrap();
        std::fs::write(
            package.join("src/main/module.json5"),
            if hsp {
                r#"{"module":{"name":"uni_core_ohos","type":"shared","deliveryWithInstall":true,"deviceTypes":["phone"]}}"#
            } else {
                r#"{"module":{"name":"uni_core_ohos","type":"har","deviceTypes":["phone"]}}"#
            },
        )
        .unwrap();
        std::fs::write(
            package.join("build-profile.json5"),
            if hsp && args.ohos_integrated_hsp {
                r#"{"apiType":"stageMode","buildOption":{"generateSharedTgz":true,"nativeLib":{"excludeSoFromInterfaceHar":true},"arkOptions":{"integratedHsp":true}}}"#
            } else if hsp {
                r#"{"apiType":"stageMode","buildOption":{"generateSharedTgz":true,"nativeLib":{"excludeSoFromInterfaceHar":true}}}"#
            } else {
                r#"{"apiType":"stageMode"}"#
            },
        )
        .unwrap();
        std::fs::write(package.join("Index.ets"), "export {};\n").unwrap();
        if hsp {
            let project = root.join("module-project");
            std::fs::create_dir_all(project.join("library")).unwrap();
            std::fs::write(
                project.join("build-profile.json5"),
                if args.ohos_integrated_hsp {
                    r#"{"app":{"products":[{"name":"default","buildOption":{"strictMode":{"useNormalizedOHMUrl":true}}}]}}"#
                } else {
                    r#"{"app":{"products":[{"name":"default"}]}}"#
                },
            )
            .unwrap();
            for output in [
                args.ohos_runtime_hsp_out.as_ref().unwrap(),
                args.ohos_interface_har_out.as_ref().unwrap(),
                args.ohos_tgz_out.as_ref().unwrap(),
            ] {
                std::fs::write(
                    root.join(output.file_name().unwrap()),
                    format!("HSP:{contents}"),
                )
                .unwrap();
            }
            std::fs::write(
                root.join(transaction.expected_usage_name.as_ref().unwrap()),
                format!("usage:{contents}"),
            )
            .unwrap();
        } else {
            let har = root.join(args.ohos_har_out.as_ref().unwrap().file_name().unwrap());
            std::fs::write(har, format!("HAR:{contents}")).unwrap();
        }
    }

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

    #[test]
    fn expands_node_electron_as_one_napi_group() {
        assert_eq!(
            expand_targets(&[ArtifactTargetArg::Node, ArtifactTargetArg::Electron]).unwrap(),
            ExpandedTargets {
                wasm: false,
                mini_program: false,
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

    #[cfg(unix)]
    #[test]
    fn invocation_mirror_is_injective_and_canonicalizes_logical_ancestors() {
        let root = unique_tmp_dir("invocation-mirror-injective");
        std::fs::create_dir_all(&root).unwrap();
        let mut mirror = InvocationMirror::new().unwrap();
        let invocation_root = mirror.guard.root().to_path_buf();
        let colon = root.join("a:b");
        let old_escape = root.join("a_driveb");
        let upper = root.join("Foo");
        let lower = root.join("foo");
        let mapped = [&colon, &old_escape, &upper, &lower]
            .into_iter()
            .map(|path| mirror.map(path).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(mapped.iter().collect::<BTreeSet<_>>().len(), mapped.len());
        for (index, left) in mapped.iter().enumerate() {
            for right in mapped.iter().skip(index + 1) {
                assert!(!left.starts_with(right) && !right.starts_with(left));
            }
        }

        let real = root.join("real");
        let alias = root.join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert_eq!(
            mirror.map(&real.join("output")).unwrap(),
            mirror.map(&alias.join("output")).unwrap(),
            "logical symlink spellings must share canonical private identity"
        );

        let mut public = empty_build_args();
        public.out_dir = Some(root.join("generated"));
        public.host_crates_dir = Some(root.join("host"));
        public.artifact_dir = Some(root.join("artifacts"));
        let targets = ExpandedTargets {
            node: true,
            wasm: true,
            ..ExpandedTargets::default()
        };
        let private = mirror_build_args(&public, &mirror, &targets).unwrap();
        let core_target = private.wasm_core_target_dir.as_ref().unwrap();
        let host_target = private.wasm_target_dir.as_ref().unwrap();
        assert!(core_target.starts_with(&mirror.build_root));
        assert!(host_target.starts_with(&mirror.build_root));
        assert!(!core_target.starts_with(&mirror.root));
        assert!(!host_target.starts_with(&mirror.root));
        assert_ne!(core_target, host_target);
        let wasm = private.to_wasm_args().unwrap();
        assert_eq!(wasm.core_target_dir.as_ref(), Some(core_target));
        assert_eq!(wasm.target_dir.as_ref(), Some(host_target));
        let mini_only = ExpandedTargets {
            mini_program: true,
            ..ExpandedTargets::default()
        };
        let mini_private = mirror_build_args(&public, &mirror, &mini_only).unwrap();
        let mini_core = mini_private
            .wasm_core_target_dir
            .as_ref()
            .expect("mini-only core target is externalized");
        let mini_host = mini_private
            .wasm_target_dir
            .as_ref()
            .expect("mini-only host target is externalized");
        assert!(mini_core.starts_with(&mirror.build_root));
        assert!(mini_host.starts_with(&mirror.build_root));
        assert_ne!(mini_core, mini_host);
        let mini_wasm = mini_private.to_wasm_args().unwrap();
        assert_eq!(mini_wasm.core_target_dir.as_ref(), Some(mini_core));
        assert_eq!(mini_wasm.target_dir.as_ref(), Some(mini_host));
        let destination = super::super::ohos::InvocationOutputSpec {
            label: "napi manifest".into(),
            path: canonicalize_invocation_output(&root.join("host/napi/Cargo.toml")).unwrap(),
            is_directory: false,
        };
        assert_eq!(
            private_output_sources(&public, &private, &[destination]).unwrap(),
            vec![private.host_crates_dir().join("napi/Cargo.toml")],
            "mapped roots must preserve generator-appended relative subpaths"
        );

        #[cfg(target_os = "macos")]
        assert_eq!(
            mirror
                .map(Utf8Path::new("/var/tmp/uniffi-map-probe"))
                .unwrap(),
            mirror
                .map(Utf8Path::new("/private/var/tmp/uniffi-map-probe"))
                .unwrap(),
            "macOS /var and /private/var must canonicalize to one identity"
        );
        mirror.finish(Ok(())).unwrap();
        assert!(
            !invocation_root.exists(),
            "successful direct invocation cleanup leaked its private root"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn multi_target_hsp_plan_enlists_web_apple_android_and_rejects_cross_target_aliases() {
        let root = unique_tmp_dir("multi-target-hsp-plan");
        let mut args = empty_build_args();
        args.out_dir = Some(root.join("generated"));
        args.host_crates_dir = Some(root.join("host"));
        args.artifact_dir = Some(root.join("artifacts"));
        args.wasm_bindgen_out_dir = Some(root.join("web-package"));
        args.apple_xcframework_out = Some(root.join("apple/core.xcframework"));
        args.apple_swift_out = Some(root.join("apple/swift"));
        args.android_jni_libs_out = Some(root.join("android/jniLibs"));
        args.android_kotlin_out = Some(root.join("android/kotlin"));
        args.android_aar_out = Some(root.join("android/core.aar"));
        let targets = ExpandedTargets {
            wasm: true,
            mini_program: false,
            node: false,
            electron: false,
            harmony: true,
            apple: true,
            android: true,
        };
        let hsp = super::super::ohos::HspOutputPaths {
            dist: Some(root.join("harmony/dist")),
            tgz: root.join("harmony/core.tgz"),
            runtime_hsp: root.join("harmony/core.hsp"),
            interface_har: root.join("harmony/core-interface.har"),
            package_source: root.join("harmony/package"),
            module_project: root.join("harmony/module-project"),
            usage: root.join("harmony/usage.md"),
        };
        let specs = invocation_output_specs(&args, &targets, None).unwrap();
        let labels = specs
            .iter()
            .map(|spec| spec.label.as_str())
            .collect::<BTreeSet<_>>();
        for label in [
            "generated source root",
            "wasm host Cargo manifest",
            "OHOS facade bundle",
            "wasm-bindgen package",
            "Apple XCFramework",
            "Apple Swift output",
            "Android jniLibs",
            "Android Kotlin output",
            "Android AAR",
        ] {
            assert!(
                labels.contains(label),
                "missing transaction participant {label}"
            );
        }
        drop(
            super::super::ohos::GenericPublicationPlan::new(specs.clone(), &[hsp.clone()]).unwrap(),
        );

        for (label, mut aliased_args) in [
            ("web", args.clone()),
            ("apple", args.clone()),
            ("android", args.clone()),
        ] {
            match label {
                "web" => aliased_args.wasm_bindgen_out_dir = Some(hsp.tgz.clone()),
                "apple" => {
                    aliased_args.apple_xcframework_out =
                        Some(hsp.runtime_hsp.parent().unwrap().to_path_buf())
                }
                "android" => {
                    aliased_args.android_aar_out = Some(hsp.package_source.join("inside.aar"))
                }
                _ => unreachable!(),
            }
            let aliased = invocation_output_specs(&aliased_args, &targets, None).unwrap();
            let error = super::super::ohos::GenericPublicationPlan::new(aliased, &[hsp.clone()])
                .err()
                .expect("cross-target alias must fail")
                .to_string();
            assert!(
                error.contains("aliases HSP publication"),
                "{label}: {error}"
            );
        }
        let _ = std::fs::remove_dir_all(root.as_std_path());
    }

    #[cfg(unix)]
    #[test]
    fn multi_target_hsp_manifest_keeps_logical_paths_under_symlinked_package_ancestor() {
        let root = unique_tmp_dir("multi-target-hsp-logical-manifest");
        let real = root.join("real");
        let alias = root.join("alias");
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let core = real.join("core");
        let package = alias.join("package");
        std::fs::create_dir_all(&package).unwrap();

        let mut args = empty_build_args();
        args.manifest_path = write_test_manifest(&core);
        args.managed_layout = true;
        args.package_dir = Some(package.clone());
        args.out_dir = None;
        args.target = vec![ArtifactTargetArg::Harmony, ArtifactTargetArg::Node];
        args.ohos_package_kind = super::super::ohos::PackageKind::Hsp;
        args.ohos_integrated_hsp = true;
        args.ohos_package_name = Some("@uniffi/uni-core".into());
        args.ohos_compatible_sdk_version = Some("5.0.1(13)".into());
        args.ohos_compatible_sdk_type = Some("HarmonyOS".into());

        let targets = expand_targets(&args.target).unwrap();
        let layout = ManagedLayout::apply(&mut args, &targets)
            .unwrap()
            .expect("managed layout");
        let canonical_outputs = ensure_explicit_generated_hsp_outputs(&mut args).unwrap();
        assert!(canonical_outputs
            .tgz
            .starts_with(real.canonicalize_utf8().unwrap()));
        assert!(args.ohos_tgz_out.as_ref().unwrap().starts_with(&package));

        let manifest = layout
            .render_manifest_with_read_roots(
                &targets,
                &test_cargo_metadata(core.join("target")),
                &args,
                None,
                None,
            )
            .unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest).unwrap();
        assert_eq!(
            manifest["artifacts"]["harmony"]["tgz"],
            "artifacts/harmony/uniffi-uni-core.tgz"
        );
        assert_eq!(
            manifest["artifacts"]["node"]["addon"],
            "artifacts/node/uni_core.node"
        );
        let _ = std::fs::remove_dir_all(root.as_std_path());
    }

    #[test]
    fn managed_layout_derives_package_paths() {
        let mut args = empty_build_args();
        let package_dir = unique_tmp_dir("managed-layout-derive");
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![
            ArtifactTargetArg::Wasm,
            ArtifactTargetArg::MiniProgram,
            ArtifactTargetArg::Node,
            ArtifactTargetArg::Electron,
            ArtifactTargetArg::Harmony,
            ArtifactTargetArg::Apple,
            ArtifactTargetArg::Android,
        ];

        let targets = expand_targets(&args.target).unwrap();
        let layout = ManagedLayout::apply(&mut args, &targets)
            .unwrap()
            .expect("managed layout should resolve");

        assert_eq!(args.out_dir.as_ref().unwrap(), &package_dir.join("src/ffi"));
        assert_eq!(
            args.host_crates_dir.as_ref().unwrap(),
            &package_dir.join("artifacts/rust")
        );
        assert_eq!(
            args.artifact_dir.as_ref().unwrap(),
            &package_dir.join("artifacts")
        );
        assert_eq!(
            args.ohos_dist_dir.as_ref().unwrap(),
            &package_dir.join("artifacts/harmony/dist")
        );
        assert_eq!(
            args.ohos_har_out.as_ref().unwrap(),
            &package_dir.join("artifacts/harmony/uni-core-ohos.har")
        );
        assert_eq!(
            args.apple_xcframework_out.as_ref().unwrap(),
            &package_dir.join("artifacts/apple/uni_core.xcframework")
        );
        assert_eq!(
            args.android_jni_libs_out.as_ref().unwrap(),
            &package_dir.join("artifacts/android/jniLibs")
        );
        assert_eq!(
            layout.manifest_path,
            package_dir.join("artifact-manifest.json")
        );

        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_package_root_transaction_preserves_carried_files_and_fail_closes_owned_changes() {
        let root = unique_tmp_dir("managed-package-root-transaction");
        let package = root.join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), "{\"private\":true}\n").unwrap();
        let layout = ManagedLayout {
            package_dir: package.clone(),
            source_root: package.join("src/ffi"),
            artifact_root: package.join("artifacts"),
            host_crates_root: package.join("artifacts/rust"),
            manifest_path: package.join("artifact-manifest.json"),
        };

        let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
        let mut public_args = empty_build_args();
        public_args.out_dir = None;
        let private_args = transaction.private_args(&public_args).unwrap();
        let wasm_target = private_args
            .wasm_target_dir
            .as_ref()
            .expect("managed wasm host cache is externalized");
        let wasm_core_target = private_args
            .wasm_core_target_dir
            .as_ref()
            .expect("managed wasm core cache is externalized");
        assert!(!wasm_target.starts_with(&transaction.private_root));
        assert!(!wasm_core_target.starts_with(&transaction.private_root));
        assert!(wasm_target.starts_with(transaction.build_temp.path.clone()));
        assert!(wasm_core_target.starts_with(&transaction.build_temp.path));
        assert_ne!(wasm_target, wasm_core_target);
        std::fs::create_dir_all(transaction.private_root.join("src/ffi/node")).unwrap();
        std::fs::write(
            transaction.private_root.join("src/ffi/node/index.ts"),
            "export const generation = 1;\n",
        )
        .unwrap();
        std::fs::write(
            transaction.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
        let owner = transaction.prepare_owner().unwrap();
        transaction.commit(owner).unwrap();
        assert_eq!(parse_managed_owner(&package).unwrap().state, "committed");

        // User-carried paths are copied and revalidated for this transaction,
        // but are deliberately not frozen in the persistent managed inventory.
        std::fs::write(package.join("package.json"), "{\"private\":false}\n").unwrap();
        let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
        assert_eq!(
            std::fs::read_to_string(transaction.private_root.join("package.json")).unwrap(),
            "{\"private\":false}\n"
        );
        std::fs::write(
            transaction.private_root.join("src/ffi/node/index.ts"),
            "export const generation = 2;\n",
        )
        .unwrap();
        let owner = transaction.prepare_owner().unwrap();
        transaction.commit(owner).unwrap();
        assert_eq!(
            std::fs::read_to_string(package.join("package.json")).unwrap(),
            "{\"private\":false}\n"
        );
        assert!(
            std::fs::read_to_string(package.join("src/ffi/node/index.ts"))
                .unwrap()
                .contains("generation = 2")
        );

        std::fs::write(
            package.join("src/ffi/node/index.ts"),
            "unowned replacement\n",
        )
        .unwrap();
        assert!(ManagedPackageTransaction::begin(&layout).is_err());
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn managed_seed_registration_never_adopts_inserted_or_replaced_objects() {
        let root = unique_tmp_dir("managed-exact-seed-races");
        let source = root.join("source");
        std::fs::create_dir_all(source.join("artifacts/apple")).unwrap();
        std::fs::write(source.join("artifacts/apple/value"), b"owned").unwrap();
        let source_snapshot = super::super::ohos::capture_directory_for_cleanup(&source).unwrap();

        let candidate = root.join("candidate");
        let mut guard = ManagedOwnedDirectory::create(candidate.clone()).unwrap();
        let seeded =
            super::super::ohos::copy_captured_directory(&source, &candidate, &source_snapshot)
                .unwrap();
        std::fs::write(candidate.join("inserted-after-copy"), b"user").unwrap();
        assert!(guard.register_seeded_contents(seeded).is_err());
        assert!(guard.cleanup().is_err());
        assert_eq!(
            std::fs::read(candidate.join("inserted-after-copy")).unwrap(),
            b"user"
        );
        guard.armed = false;

        let candidate = root.join("candidate-selected-replacement");
        let mut guard = ManagedOwnedDirectory::create(candidate.clone()).unwrap();
        let seeded =
            super::super::ohos::copy_captured_directory(&source, &candidate, &source_snapshot)
                .unwrap();
        guard.register_seeded_contents(seeded).unwrap();
        let selected = candidate.join("artifacts/apple");
        let displaced = root.join("displaced-selected");
        std::fs::rename(&selected, &displaced).unwrap();
        std::fs::create_dir_all(&selected).unwrap();
        std::fs::write(selected.join("value"), b"owned").unwrap();
        std::fs::write(selected.join("user"), b"survive").unwrap();
        let mut budget = super::super::ohos::TraversalBudget::managed();
        assert!(guard
            .remove_seeded_path("artifacts/apple", &mut budget)
            .is_err());
        assert!(guard.cleanup().is_err());
        assert_eq!(std::fs::read(selected.join("user")).unwrap(), b"survive");
        assert_eq!(std::fs::read(displaced.join("value")).unwrap(), b"owned");
        guard.armed = false;

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn committed_managed_owner_rejects_root_aba_and_missing_schema3_witnesses() {
        let root = unique_tmp_dir("managed-owner-schema3-witness");
        let package = root.join("package");
        std::fs::create_dir_all(&package).unwrap();
        let layout = ManagedLayout {
            package_dir: package.clone(),
            source_root: package.join("src/ffi"),
            artifact_root: package.join("artifacts"),
            host_crates_root: package.join("artifacts/rust"),
            manifest_path: package.join("artifact-manifest.json"),
        };
        let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
        std::fs::create_dir_all(transaction.private_root.join("src/ffi/node")).unwrap();
        std::fs::write(
            transaction.private_root.join("src/ffi/node/index.ts"),
            "export const generation = 1;\n",
        )
        .unwrap();
        std::fs::write(
            transaction.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
        let owner = transaction.prepare_owner().unwrap();
        transaction.commit(owner).unwrap();

        let sidecar = managed_owner_path(&package);
        let original_owner = std::fs::read(&sidecar).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&original_owner).unwrap();
        value.as_object_mut().unwrap().remove("rootMutationToken");
        std::fs::write(&sidecar, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let missing_root = parse_managed_owner(&package).unwrap();
        assert!(validate_managed_owner(&package, &missing_root).is_err());

        std::fs::write(&sidecar, &original_owner).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&original_owner).unwrap();
        let file_entry = value["entries"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|entry| entry["kind"] == "file")
            .unwrap();
        file_entry
            .as_object_mut()
            .unwrap()
            .remove("parentMutationToken");
        std::fs::write(&sidecar, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let missing_parent = parse_managed_owner(&package).unwrap();
        assert!(validate_managed_owner(&package, &missing_parent).is_err());

        std::fs::write(&sidecar, &original_owner).unwrap();
        let displaced = root.join("package-displaced");
        std::fs::rename(&package, &displaced).unwrap();
        std::fs::rename(&displaced, &package).unwrap();
        assert!(
            ManagedPackageTransaction::begin(&layout).is_err(),
            "committed package-root A->B->A mutation was accepted"
        );

        let _ = std::fs::remove_dir_all(root.as_std_path());
        let _ = std::fs::remove_file(sidecar.as_std_path());
    }

    #[test]
    fn managed_precommit_error_restores_old_root_and_rebinds_owner() {
        let root = unique_tmp_dir("managed-precommit-rollback");
        let package = root.join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), "{\"private\":true}\n").unwrap();
        let layout = ManagedLayout {
            package_dir: package.clone(),
            source_root: package.join("src/ffi"),
            artifact_root: package.join("artifacts"),
            host_crates_root: package.join("artifacts/rust"),
            manifest_path: package.join("artifact-manifest.json"),
        };

        let mut first = ManagedPackageTransaction::begin(&layout).unwrap();
        std::fs::create_dir_all(first.private_root.join("src/ffi/node")).unwrap();
        std::fs::write(
            first.private_root.join("src/ffi/node/index.ts"),
            "export const generation = 1;\n",
        )
        .unwrap();
        std::fs::write(
            first.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
        let owner = first.prepare_owner().unwrap();
        first.commit(owner).unwrap();
        let old_payload = regular_file_snapshot(&package);
        let old_generation = parse_managed_owner(&package).unwrap().generation;

        let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
        std::fs::write(
            transaction.private_root.join("src/ffi/node/index.ts"),
            "export const generation = 2;\n",
        )
        .unwrap();
        let _next_owner = transaction.prepare_owner().unwrap();
        let candidate_capture = transaction.private.snapshot.clone();
        transaction.build_temp.cleanup().unwrap();
        let parent = transaction.public_root.parent().unwrap().to_path_buf();
        let backup = parent.join(&transaction.journal.backup_name);
        let failed = parent.join(&transaction.journal.failed_name);
        std::fs::rename(&transaction.public_root, &backup).unwrap();
        std::fs::rename(&transaction.private_root, &transaction.public_root).unwrap();
        transaction.private.disarm_after_rename();
        super::super::ohos::sync_directory(&parent).unwrap();

        transaction
            .rollback_precommit_publication(
                true,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                true,
            )
            .unwrap();
        assert_eq!(regular_file_snapshot(&package), old_payload);
        let rebound = parse_managed_owner(&package).unwrap();
        assert_eq!(rebound.generation, old_generation);
        validate_managed_owner(&package, &rebound).unwrap();
        assert!(!backup.exists() && !failed.exists());
        assert!(
            managed_record_paths(&parent, &managed_package_digest(&transaction.public_root))
                .is_empty()
        );
        drop(transaction);

        // Exercise the same controlled rollback before candidate->public.
        // This is the state reached by a durable-record/fsync error after the
        // old root has moved to backup but while the exact candidate remains
        // at its private creation-time pathname.
        let mut before_candidate = ManagedPackageTransaction::begin(&layout).unwrap();
        std::fs::write(
            before_candidate.private_root.join("src/ffi/node/index.ts"),
            "export const generation = 3;\n",
        )
        .unwrap();
        let _owner = before_candidate.prepare_owner().unwrap();
        let candidate_capture = before_candidate.private.snapshot.clone();
        before_candidate.build_temp.cleanup().unwrap();
        let parent = before_candidate.public_root.parent().unwrap().to_path_buf();
        let backup = parent.join(&before_candidate.journal.backup_name);
        let failed = parent.join(&before_candidate.journal.failed_name);
        std::fs::rename(&before_candidate.public_root, &backup).unwrap();
        super::super::ohos::sync_directory(&parent).unwrap();
        before_candidate
            .rollback_precommit_publication(
                true,
                &backup,
                &failed,
                &candidate_capture,
                None,
                true,
                true,
            )
            .unwrap();
        assert_eq!(regular_file_snapshot(&package), old_payload);
        validate_managed_owner(&package, &parse_managed_owner(&package).unwrap()).unwrap();
        assert!(!backup.exists() && !failed.exists());

        let sidecar = managed_owner_path(&package);
        std::fs::remove_dir_all(root).ok();
        let _ = std::fs::remove_file(sidecar);
    }

    #[test]
    fn managed_precommit_journal_fault_matrix_restores_old_generation_without_residue() {
        let root = unique_tmp_dir("managed-precommit-journal-faults");
        let package = root.join("package");
        std::fs::create_dir_all(&package).unwrap();
        let layout = ManagedLayout {
            package_dir: package.clone(),
            source_root: package.join("src/ffi"),
            artifact_root: package.join("artifacts"),
            host_crates_root: package.join("artifacts/rust"),
            manifest_path: package.join("artifact-manifest.json"),
        };
        let mut first = ManagedPackageTransaction::begin(&layout).unwrap();
        std::fs::create_dir_all(first.private_root.join("src/ffi/node")).unwrap();
        std::fs::write(
            first.private_root.join("src/ffi/node/index.ts"),
            "export const generation = 1;\n",
        )
        .unwrap();
        std::fs::write(
            first.private_root.join("artifact-manifest.json"),
            "{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"node\"]}\n",
        )
        .unwrap();
        let owner = first.prepare_owner().unwrap();
        first.commit(owner).unwrap();
        let old_public = regular_file_snapshot(&package);
        let old_generation = parse_managed_owner(&package).unwrap().generation;
        let public = canonicalize_invocation_output(&package).unwrap();
        let digest = managed_package_digest(&public);
        let parent = public.parent().unwrap().to_path_buf();

        for (index, fault) in ["notCreated", "write", "fileSync", "parentSync"]
            .into_iter()
            .enumerate()
        {
            let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
            std::fs::write(
                transaction.private_root.join("src/ffi/node/index.ts"),
                format!("export const generation = {};\n", index + 2),
            )
            .unwrap();
            let owner = transaction.prepare_owner().unwrap();
            MANAGED_JOURNAL_TEST_FAULT.with(|value| {
                *value.borrow_mut() = Some(("publicBackedUp".into(), fault));
            });
            let error = transaction.commit(owner).unwrap_err();
            MANAGED_JOURNAL_TEST_FAULT.with(|value| *value.borrow_mut() = None);
            let text = format!("{error:#}");
            assert!(text.contains("committed=false"), "{fault}: {text}");
            assert_eq!(
                regular_file_snapshot(&package),
                old_public,
                "{fault} left a mixed managed public generation"
            );
            let rebound = parse_managed_owner(&package).unwrap();
            assert_eq!(rebound.generation, old_generation);
            validate_managed_owner(&package, &rebound).unwrap();
            assert!(
                managed_record_paths(&parent, &digest).is_empty(),
                "{fault} left managed append-only records"
            );
            let residues = std::fs::read_dir(&parent)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!(".uniffi-managed-package-{digest}-"))
                })
                .count();
            assert_eq!(residues, 0, "{fault} left managed transaction residue");
        }

        let sidecar = managed_owner_path(&package);
        let _ = std::fs::remove_dir_all(root.as_std_path());
        let _ = std::fs::remove_file(sidecar.as_std_path());
    }

    #[test]
    fn managed_postcommit_partial_cleanup_retains_complete_old_generation_snapshot() {
        use std::io::Read as _;

        let root = unique_tmp_dir("managed-postcommit-snapshot");
        let package = root.join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), "{\"private\":true}\n").unwrap();
        let layout = ManagedLayout {
            package_dir: package.clone(),
            source_root: package.join("src/ffi"),
            artifact_root: package.join("artifacts"),
            host_crates_root: package.join("artifacts/rust"),
            manifest_path: package.join("artifact-manifest.json"),
        };

        let write_generation = |transaction: &mut ManagedPackageTransaction, value: u8| {
            std::fs::create_dir_all(transaction.private_root.join("src/ffi/node")).unwrap();
            std::fs::write(
                transaction.private_root.join("src/ffi/node/index.ts"),
                format!("export const generation = {value};\n"),
            )
            .unwrap();
            std::fs::write(
                transaction.private_root.join("artifact-manifest.json"),
                format!(
                    "{{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"generation\":{value},\"targets\":[\"node\"]}}\n"
                ),
            )
            .unwrap();
        };

        let mut first = ManagedPackageTransaction::begin(&layout).unwrap();
        write_generation(&mut first, 1);
        let owner = first.prepare_owner().unwrap();
        first.commit(owner).unwrap();
        let old_generation = regular_file_snapshot(&package);

        let mut second = ManagedPackageTransaction::begin(&layout).unwrap();
        write_generation(&mut second, 2);
        let owner = second.prepare_owner().unwrap();
        super::super::ohos::set_captured_directory_cleanup_failure_after(Some(0));
        let error = second.commit(owner).unwrap_err();
        super::super::ohos::set_captured_directory_cleanup_failure_after(None);
        let text = format!("{error:#}");
        assert!(text.contains("committed=true"), "{text}");
        assert!(
            std::fs::read_to_string(package.join("src/ffi/node/index.ts"))
                .unwrap()
                .contains("generation = 2"),
            "post-commit cleanup failure rolled the new generation back"
        );

        let snapshot = std::fs::read_dir(package.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| Utf8PathBuf::from_path_buf(entry.path()).unwrap())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.starts_with(".uniffi-managed-package-")
                        && name.ends_with("-previous-generation.tar.gz")
                })
            })
            .expect("committed cleanup failure retained its complete snapshot");
        let decoder = flate2::read::GzDecoder::new(std::fs::File::open(&snapshot).unwrap());
        let mut archive = tar::Archive::new(decoder);
        let mut archived = std::collections::BTreeMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            if !entry.header().entry_type().is_file() {
                continue;
            }
            let path = entry.path().unwrap().into_owned();
            let relative = path
                .strip_prefix("previous-generation")
                .unwrap()
                .to_path_buf();
            let mut bytes = Vec::new();
            entry.read_to_end(&mut bytes).unwrap();
            archived.insert(Utf8PathBuf::from_path_buf(relative).unwrap(), bytes);
        }
        assert_eq!(archived, old_generation);

        let _ = std::fs::remove_dir_all(root.as_std_path());
    }

    #[test]
    fn managed_layout_uses_safe_archive_name_for_scoped_ohpm_package() {
        let mut args = empty_build_args();
        let package_dir = unique_tmp_dir("managed-layout-scoped-harmony");
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![ArtifactTargetArg::Harmony];
        args.ohos_package_name = Some("@scope/uni-core".into());

        let targets = expand_targets(&args.target).unwrap();
        ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
        assert_eq!(
            args.ohos_har_out.as_ref().unwrap(),
            &package_dir.join("artifacts/harmony/scope-uni-core.har")
        );
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_layout_derives_and_commits_integrated_hsp_generation() {
        let package_dir = unique_tmp_dir("managed-layout-integrated-hsp");
        let mut args = empty_build_args();
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![ArtifactTargetArg::Harmony];
        args.ohos_package_kind = super::super::ohos::PackageKind::Hsp;
        args.ohos_integrated_hsp = true;
        args.ohos_package_name = Some("@scope/uni-core".into());
        args.ohos_compatible_sdk_version = Some("5.0.1(13)".into());
        args.ohos_compatible_sdk_type = Some("HarmonyOS".into());

        let targets = expand_targets(&args.target).unwrap();
        let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
        assert!(args.ohos_har_out.is_none());
        assert_eq!(
            args.ohos_runtime_hsp_out.as_ref().unwrap(),
            &package_dir.join("artifacts/harmony/scope-uni-core.hsp")
        );
        assert_eq!(
            args.ohos_interface_har_out.as_ref().unwrap(),
            &package_dir.join("artifacts/harmony/scope-uni-core-interface.har")
        );
        assert_eq!(
            args.ohos_tgz_out.as_ref().unwrap(),
            &package_dir.join("artifacts/harmony/scope-uni-core.tgz")
        );

        let public_args = args.clone();
        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut args).unwrap();
        populate_private_harmony(&transaction, &public_args, "hsp-generation");
        let meta = test_cargo_metadata(package_dir.join("target"));
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_args,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();

        let harmony = package_dir.join("artifacts/harmony");
        let entries = std::fs::read_dir(&harmony)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            entries,
            BTreeSet::from([
                ".uniffi-managed-harmony-owner".to_string(),
                "dist".to_string(),
                "module-project".to_string(),
                "package".to_string(),
                "scope-uni-core-HSP_USAGE.md".to_string(),
                "scope-uni-core-interface.har".to_string(),
                "scope-uni-core.hsp".to_string(),
                "scope-uni-core.tgz".to_string(),
            ])
        );
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(package_dir.join("artifact-manifest.json")).unwrap(),
        )
        .unwrap();
        let hsp = &manifest["artifacts"]["harmony"];
        assert_eq!(manifest["schemaVersion"], 3);
        assert_eq!(hsp["kind"], "hsp");
        assert_eq!(hsp["integrated"], true);
        assert!(hsp["har"].is_null());
        for field in [
            "runtimeHsp",
            "interfaceHar",
            "tgz",
            "dist",
            "package",
            "moduleProject",
            "moduleSource",
            "usage",
        ] {
            let path = package_dir.join(hsp[field].as_str().unwrap());
            assert!(
                path.exists(),
                "managed HSP manifest path is missing: {field}={path}"
            );
        }
        assert_eq!(hsp["metadata"]["package"]["packageType"], "InterfaceHar");
        assert_eq!(hsp["metadata"]["module"]["type"], "shared");
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_no_har_manifest_only_declares_current_dist_outputs() {
        let mut args = empty_build_args();
        let package_dir = unique_tmp_dir("managed-layout-no-har");
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![ArtifactTargetArg::Harmony];
        args.ohos_no_har = true;
        // Package-only validation must not constrain a pure native dist run.
        args.ohos_package_name = Some("NOT-A-PACKAGE".into());
        args.ohos_package_version = Some("not-semver".into());

        let targets = expand_targets(&args.target).unwrap();
        let layout = ManagedLayout::apply(&mut args, &targets)
            .unwrap()
            .expect("managed layout should resolve");
        assert!(args.ohos_har_out.is_none());
        let dist = package_dir.join("artifacts/harmony/dist");
        std::fs::create_dir_all(&dist).unwrap();
        for file in [
            "index.d.ts",
            "harmony-facade-contract.json",
            "native-facade.ets",
            "package-index.ets",
        ] {
            std::fs::write(dist.join(file), "export {};\n").unwrap();
        }
        let meta = test_cargo_metadata(package_dir.join("target"));
        layout.emit(&targets, &meta, &args).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap(),
        )
        .unwrap();
        let harmony = &manifest["artifacts"]["harmony"];
        assert_eq!(harmony["kind"], "dist");
        assert!(harmony["har"].is_null());
        assert!(harmony["package"].is_null());
        assert!(harmony["packageMetadata"].is_null());
        assert!(harmony["moduleMetadata"].is_null());
        assert!(harmony["buildProfile"].is_null());
        assert!(harmony["metadata"].is_null());
        assert!(harmony["packageFacadeContract"].is_null());
        for field in ["dist", "facade", "facadeContract", "types"] {
            let path = package_dir.join(harmony[field].as_str().unwrap());
            assert!(
                path.exists(),
                "manifest {field} path does not exist: {path}"
            );
        }
        let entry = package_dir.join(manifest["entrypoints"]["harmony"].as_str().unwrap());
        assert!(entry.exists());
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_directory_transaction_switches_har_to_clean_no_har_state() {
        let package_dir = unique_tmp_dir("managed-harmony-switch");
        let meta = test_cargo_metadata(package_dir.join("target"));
        let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();

        let mut har_args = empty_build_args();
        har_args.manifest_path = write_test_manifest(&package_dir);
        har_args.managed_layout = true;
        har_args.package_dir = Some(package_dir.clone());
        har_args.out_dir = None;
        har_args.target = vec![ArtifactTargetArg::Harmony];
        let layout = ManagedLayout::apply(&mut har_args, &targets)
            .unwrap()
            .unwrap();
        let public_har_args = har_args.clone();
        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut har_args).unwrap();
        populate_private_harmony(&transaction, &public_har_args, "har-state");
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_har_args,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();

        let mut no_har_args = public_har_args.clone();
        no_har_args.ohos_no_har = true;
        no_har_args.ohos_skip_libs = true;
        no_har_args.ohos_har_out = None;
        let public_no_har_args = no_har_args.clone();
        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut no_har_args).unwrap();
        populate_private_harmony(&transaction, &public_no_har_args, "dist-only-state");
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_no_har_args,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();

        let harmony_root = package_dir.join("artifacts/harmony");
        let entries = std::fs::read_dir(&harmony_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            entries,
            BTreeSet::from(["dist".to_string(), MANAGED_HARMONY_OWNER_MARKER.to_string()])
        );
        ensure_tree_has_no_native_artifacts(&harmony_root).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(package_dir.join("artifact-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["artifacts"]["harmony"]["kind"], "dist");
        assert!(manifest["artifacts"]["harmony"]["har"].is_null());
        assert!(manifest["artifacts"]["harmony"]["package"].is_null());
        assert!(std::fs::read_dir(package_dir.join("artifacts"))
            .unwrap()
            .filter_map(Result::ok)
            .all(|entry| !entry.file_name().to_string_lossy().contains("backup")));
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_harmony_transaction_switches_har_hsp_and_dist_without_stale_outputs() {
        let package_dir = unique_tmp_dir("managed-harmony-three-state-switch");
        let manifest_path = write_test_manifest(&package_dir);
        let meta = test_cargo_metadata(package_dir.join("target"));
        let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();

        let mut har_args = empty_build_args();
        har_args.manifest_path = manifest_path.clone();
        har_args.managed_layout = true;
        har_args.package_dir = Some(package_dir.clone());
        har_args.out_dir = None;
        har_args.target = vec![ArtifactTargetArg::Harmony];
        let layout = ManagedLayout::apply(&mut har_args, &targets)
            .unwrap()
            .unwrap();
        let public_har = har_args.clone();
        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut har_args).unwrap();
        populate_private_harmony(&transaction, &public_har, "har");
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_har,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();

        let mut hsp_args = empty_build_args();
        hsp_args.manifest_path = manifest_path.clone();
        hsp_args.managed_layout = true;
        hsp_args.package_dir = Some(package_dir.clone());
        hsp_args.out_dir = None;
        hsp_args.target = vec![ArtifactTargetArg::Harmony];
        hsp_args.ohos_package_kind = super::super::ohos::PackageKind::Hsp;
        hsp_args.ohos_integrated_hsp = true;
        let layout = ManagedLayout::apply(&mut hsp_args, &targets)
            .unwrap()
            .unwrap();
        let public_hsp = hsp_args.clone();
        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut hsp_args).unwrap();
        populate_private_harmony(&transaction, &public_hsp, "hsp");
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_hsp,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();
        let harmony = package_dir.join("artifacts/harmony");
        let hsp_entries = std::fs::read_dir(&harmony)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<BTreeSet<_>>();
        assert!(hsp_entries.contains("uni-core-ohos.tgz"));
        assert!(hsp_entries.contains("uni-core-ohos.hsp"));
        assert!(!hsp_entries.contains("uni-core-ohos.har"));
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(package_dir.join("artifact-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["artifacts"]["harmony"]["kind"], "hsp");

        let mut dist_args = empty_build_args();
        dist_args.manifest_path = manifest_path;
        dist_args.managed_layout = true;
        dist_args.package_dir = Some(package_dir.clone());
        dist_args.out_dir = None;
        dist_args.target = vec![ArtifactTargetArg::Harmony];
        dist_args.ohos_no_har = true;
        dist_args.ohos_skip_libs = true;
        let layout = ManagedLayout::apply(&mut dist_args, &targets)
            .unwrap()
            .unwrap();
        let public_dist = dist_args.clone();
        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut dist_args).unwrap();
        populate_private_harmony(&transaction, &public_dist, "dist");
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_dist,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();
        let entries = std::fs::read_dir(&harmony)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            entries,
            BTreeSet::from([
                ".uniffi-managed-harmony-owner".to_string(),
                "dist".to_string(),
            ])
        );
        let manifest: serde_json::Value = serde_json::from_slice(
            &std::fs::read(package_dir.join("artifact-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["artifacts"]["harmony"]["kind"], "dist");
        for field in [
            "har",
            "runtimeHsp",
            "interfaceHar",
            "tgz",
            "moduleProject",
            "usage",
        ] {
            assert!(manifest["artifacts"]["harmony"][field].is_null());
        }
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_transaction_rolls_back_before_commit_and_never_restores_partial_cleanup() {
        let package_dir = unique_tmp_dir("managed-harmony-rollback");
        let meta = test_cargo_metadata(package_dir.join("target"));
        let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();
        let mut args = empty_build_args();
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![ArtifactTargetArg::Harmony];
        args.ohos_no_har = true;
        args.ohos_skip_libs = true;
        let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
        let public_args = args.clone();

        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut args).unwrap();
        populate_private_harmony(&transaction, &public_args, "old-state");
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_args,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();

        let harmony_root = package_dir.join("artifacts/harmony");
        let manifest_path = package_dir.join("artifact-manifest.json");
        let old_tree = regular_file_snapshot(&harmony_root);
        let old_manifest = std::fs::read(&manifest_path).unwrap();

        let mut manifest_args = public_args.clone();
        let mut transaction =
            ManagedHarmonyTransaction::begin(&layout, &mut manifest_args).unwrap();
        populate_private_harmony(&transaction, &public_args, "manifest-failure");
        let result = transaction.commit_with(
            b"{\"phase\":\"manifest-failure\"}\n",
            |_, _| bail!("injected manifest failure"),
            |path| {
                std::fs::remove_dir_all(path)?;
                Ok(())
            },
        );
        assert!(result.is_err());
        drop(transaction);
        assert_eq!(regular_file_snapshot(&harmony_root), old_tree);
        assert_eq!(std::fs::read(&manifest_path).unwrap(), old_manifest);

        let mut cleanup_args = public_args.clone();
        let mut transaction = ManagedHarmonyTransaction::begin(&layout, &mut cleanup_args).unwrap();
        populate_private_harmony(&transaction, &public_args, "cleanup-failure");
        let next_manifest = b"{\"phase\":\"cleanup-failure\"}\n";
        let result = transaction.commit_with(
            next_manifest,
            |path, bytes| write_file_atomically(path, bytes),
            |backup| {
                let victim = regular_file_snapshot(backup)
                    .keys()
                    .find(|path| path.as_str() != MANAGED_HARMONY_OWNER_MARKER)
                    .cloned()
                    .context("backup fixture has no removable inventory file")?;
                std::fs::remove_file(backup.join(victim))?;
                bail!("injected partial backup cleanup failure")
            },
        );
        let error = result.unwrap_err().to_string();
        assert!(error.contains("generation was committed"), "{error}");
        drop(transaction);
        assert_ne!(regular_file_snapshot(&harmony_root), old_tree);
        super::super::ohos::validate_owned_tree(
            &harmony_root,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
        )
        .unwrap();
        assert_eq!(std::fs::read(&manifest_path).unwrap(), next_manifest);
        assert!(std::fs::read_dir(package_dir.join("artifacts"))
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().contains("backup")));
        let cleanup_snapshot = std::fs::read_dir(package_dir.join("artifacts"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy()
                        .contains("harmony.uniffi-previous-generation")
                })
            })
            .expect("partial managed cleanup did not retain a complete safety snapshot");
        let decoder = flate2::read::GzDecoder::new(std::fs::File::open(cleanup_snapshot).unwrap());
        let mut archive = tar::Archive::new(decoder);
        let archived = archive
            .entries()
            .unwrap()
            .map(|entry| entry.unwrap().path().unwrap().into_owned())
            .collect::<Vec<_>>();
        assert!(archived
            .iter()
            .any(|path| path.ends_with("previous-generation/dist/index.d.ts")));
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_cleanup_binds_replaced_backup_root_identity_without_rolling_back_new_generation() {
        let package_dir = unique_tmp_dir("managed-harmony-cleanup-root-identity");
        let meta = test_cargo_metadata(package_dir.join("target"));
        let targets = expand_targets(&[ArtifactTargetArg::Harmony]).unwrap();
        let mut args = empty_build_args();
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![ArtifactTargetArg::Harmony];
        args.ohos_no_har = true;
        args.ohos_skip_libs = true;
        let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
        let public_args = args.clone();

        let transaction = ManagedHarmonyTransaction::begin(&layout, &mut args).unwrap();
        populate_private_harmony(&transaction, &public_args, "old");
        let manifest = layout
            .render_manifest_with_harmony_root(
                &targets,
                &meta,
                &public_args,
                Some(transaction.private_root()),
            )
            .unwrap();
        transaction.commit(manifest.as_bytes()).unwrap();

        let mut next_args = public_args.clone();
        let mut transaction = ManagedHarmonyTransaction::begin(&layout, &mut next_args).unwrap();
        let previous = transaction.captured_root.clone().unwrap();
        populate_private_harmony(&transaction, &public_args, "new");
        let displaced = package_dir.join("artifacts/displaced-owned-backup");
        let result =
            transaction.commit_with(b"{\"phase\":\"new\"}\n", write_file_atomically, |backup| {
                std::fs::rename(backup, &displaced)?;
                std::fs::create_dir(backup)?;
                std::fs::write(backup.join("user-sentinel"), b"preserve")?;
                super::super::ohos::remove_owned_tree_for_cleanup(
                    backup,
                    MANAGED_HARMONY_OWNER_MARKER,
                    MANAGED_HARMONY_OWNER_KIND,
                    &previous,
                )
            });
        let error = result.unwrap_err().to_string();
        assert!(error.contains("generation was committed"), "{error}");
        let replacement_backup = std::fs::read_dir(package_dir.join("artifacts"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| path.join("user-sentinel").is_file())
            .expect("replacement backup root was deleted");
        assert_eq!(
            std::fs::read(replacement_backup.join("user-sentinel")).unwrap(),
            b"preserve"
        );
        assert!(displaced.join("dist/index.d.ts").is_file());
        let harmony = package_dir.join("artifacts/harmony");
        super::super::ohos::validate_owned_tree(
            &harmony,
            MANAGED_HARMONY_OWNER_MARKER,
            MANAGED_HARMONY_OWNER_KIND,
        )
        .unwrap();
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_transaction_refuses_unowned_public_tree_without_mutation() {
        let package_dir = unique_tmp_dir("managed-harmony-unowned");
        let mut args = empty_build_args();
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![ArtifactTargetArg::Harmony];
        args.ohos_no_har = true;
        let targets = expand_targets(&args.target).unwrap();
        let layout = ManagedLayout::apply(&mut args, &targets).unwrap().unwrap();
        let harmony = package_dir.join("artifacts/harmony");
        std::fs::create_dir_all(&harmony).unwrap();
        std::fs::write(harmony.join("user.har"), b"user-owned").unwrap();
        let before = regular_file_snapshot(&harmony);
        assert!(ManagedHarmonyTransaction::begin(&layout, &mut args).is_err());
        assert_eq!(regular_file_snapshot(&harmony), before);
        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_package_lock_child() {
        let Some(package_dir) = std::env::var_os("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE") else {
            return;
        };
        let package_dir = Utf8PathBuf::from_path_buf(package_dir.into()).unwrap();
        let mode = std::env::var("UNIFFI_MANAGED_LOCK_CHILD_MODE").unwrap();
        let layout = ManagedLayout {
            package_dir: package_dir.clone(),
            source_root: package_dir.join("src/ffi"),
            artifact_root: package_dir.join("artifacts"),
            host_crates_root: package_dir.join("artifacts/rust"),
            manifest_path: package_dir.join("artifact-manifest.json"),
        };
        let mut transaction = ManagedPackageTransaction::begin(&layout).unwrap();
        let acquired = std::env::var_os("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED").unwrap();
        let release = std::env::var_os("UNIFFI_MANAGED_LOCK_CHILD_RELEASE").unwrap();
        std::fs::write(acquired, b"acquired").unwrap();
        for _ in 0..1_000 {
            if std::path::Path::new(&release).exists() {
                if mode == "fail" {
                    let error = transaction.abort(anyhow::anyhow!("expected child failure"));
                    assert!(error.to_string().contains("expected child failure"));
                    return;
                }
                std::fs::create_dir_all(transaction.private_root.join("artifacts/harmony"))
                    .unwrap();
                std::fs::write(
                    transaction.private_root.join("artifacts/harmony/mode"),
                    &mode,
                )
                .unwrap();
                std::fs::write(
                    transaction.private_root.join("artifact-manifest.json"),
                    format!(
                        "{{\"schemaVersion\":3,\"generator\":\"uniffi-bindgen-javascript\",\"targets\":[\"harmony\"],\"mode\":{}}}\n",
                        serde_json::to_string(&mode).unwrap()
                    ),
                )
                .unwrap();
                let owner = transaction.prepare_owner().unwrap();
                transaction.commit(owner).unwrap();
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        panic!("timed out waiting for managed lock release");
    }

    #[cfg(unix)]
    #[test]
    fn managed_package_root_lock_serializes_concurrent_processes() {
        use std::time::{Duration, Instant};

        fn wait_for(path: &Utf8Path, timeout: Duration) {
            let started = Instant::now();
            while !path.exists() {
                assert!(started.elapsed() < timeout, "timed out waiting for {path}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }

        let package_dir = unique_tmp_dir("managed-harmony-lock");
        write_test_manifest(&package_dir);
        let control_dir = package_dir.parent().unwrap().join(format!(
            ".{}-lock-control",
            package_dir.file_name().unwrap()
        ));
        std::fs::create_dir(&control_dir).unwrap();
        let executable = std::env::current_exe().unwrap();
        let spawn_child = |acquired: &Utf8Path, release: &Utf8Path, mode: &str| {
            Command::new(&executable)
                .args([
                    "--exact",
                    "cli::artifacts::tests::managed_package_lock_child",
                    "--nocapture",
                ])
                .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
                .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", acquired)
                .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", release)
                .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", mode)
                .spawn()
                .unwrap()
        };

        let mut final_owner_producer = None;
        for (index, first_mode, second_mode) in [(0, "hsp", "fail"), (1, "har", "dist")] {
            let first_acquired = control_dir.join(format!("{index}-first-acquired"));
            let first_release = control_dir.join(format!("{index}-first-release"));
            let second_acquired = control_dir.join(format!("{index}-second-acquired"));
            let second_release = control_dir.join(format!("{index}-second-release"));
            let mut first = spawn_child(&first_acquired, &first_release, first_mode);
            wait_for(&first_acquired, Duration::from_secs(10));
            let mut second = spawn_child(&second_acquired, &second_release, second_mode);
            let second_pid = second.id();
            std::thread::sleep(Duration::from_millis(150));
            assert!(
                !second_acquired.exists(),
                "second managed invocation acquired the output lock concurrently"
            );
            std::fs::write(&first_release, b"release").unwrap();
            wait_for(&second_acquired, Duration::from_secs(10));
            std::fs::write(&second_release, b"release").unwrap();
            assert!(first.wait().unwrap().success());
            assert!(second.wait().unwrap().success());
            if second_mode != "fail" {
                final_owner_producer = Some(second_pid);
            }
        }
        assert_eq!(
            std::fs::read_to_string(package_dir.join("artifacts/harmony/mode")).unwrap(),
            "dist"
        );
        assert_eq!(
            parse_managed_owner(&package_dir).unwrap().state,
            "committed"
        );

        let public = canonicalize_invocation_output(&package_dir).unwrap();
        let control = control_dir.canonicalize_utf8().unwrap();
        cleanup_exact_managed_test_case(
            &public,
            &[control],
            Some(final_owner_producer.expect("final owner producer PID")),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn managed_exited_historical_test_controls_are_cleaned_only_from_exact_witnesses() {
        let _cleanup_lock = historical_managed_cleanup_test_lock();
        let (cleaned, preserved) = cleanup_exited_historical_managed_test_controls();
        for report in &preserved {
            eprintln!("preserved unmatched historical managed test evidence: {report}");
        }
        let (remaining, discovery_reports) = historical_managed_test_roots();
        for report in discovery_reports {
            eprintln!("preserved undiscoverable historical managed test evidence: {report}");
        }
        let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir())
            .unwrap()
            .canonicalize_utf8()
            .unwrap();
        for root in remaining {
            if root.parent() == Some(temp.as_path()) {
                let Ok(creator) = managed_test_root_creator_pid(&root) else {
                    panic!("discovered top-level managed root is not PID-bound: {root}");
                };
                if require_exited_test_pid(creator, "remaining historical managed root").is_err() {
                    continue;
                }
            }
            assert!(
                plan_exact_managed_test_cleanup(&root, &[], None, true).is_err(),
                "historical managed test controls remained even though exact cleanup was still provable: {root}"
            );
        }
        eprintln!(
            "historical managed test cleanup removed {cleaned} exact root/control group(s); preserved {} non-matching group(s)",
            preserved.len()
        );
    }

    #[cfg(unix)]
    #[test]
    fn historical_managed_generation_and_root_pid_witnesses_are_strict() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let valid = format!("{:x}-{now:x}-0", std::process::id());
        assert_eq!(
            managed_test_generation_pid(&valid).unwrap(),
            std::process::id()
        );
        for invalid in [
            format!("0-{now:x}-0"),
            format!("80000000-{now:x}-0"),
            format!("{:x}-{now:x}", std::process::id()),
            format!("{:x}-{now:x}-0-extra", std::process::id()),
            format!("0{:x}-{now:x}-0", std::process::id()),
            format!("{:x}-ABCDEF-0", std::process::id()),
            format!("{:x}-0-0", std::process::id()),
            format!("{:x}-{}-0", std::process::id(), "f".repeat(33)),
            format!("{:x}-{now:x}-{}", std::process::id(), "f".repeat(17)),
            format!(
                "{:x}-{:x}-0",
                std::process::id(),
                now.saturating_add(60_000_000_000)
            ),
        ] {
            assert!(
                managed_test_generation_pid(&invalid).is_err(),
                "forged generation was accepted: {invalid}"
            );
        }
        assert!(require_exited_test_pid(u32::MAX, "forged PID").is_err());

        let temp = Utf8PathBuf::from_path_buf(std::env::temp_dir()).unwrap();
        let valid_root = temp.join(format!(
            "uniffi-managed-strict-{}-{now}",
            std::process::id()
        ));
        assert_eq!(
            managed_test_root_creator_pid(&valid_root).unwrap(),
            std::process::id()
        );
        for invalid in [
            temp.join(format!("uniffi-managed-strict-0-{now}")),
            temp.join(format!("uniffi-managed-strict-2147483648-{now}")),
            temp.join(format!("uniffi-managed-strict-{}-0", std::process::id())),
            temp.join(format!(
                "uniffi-managed-strict-{}-{}",
                std::process::id(),
                "9".repeat(40)
            )),
            temp.join(format!(
                "uniffi-managed-strict-{}-{}",
                std::process::id(),
                now.saturating_add(60_000_000_000)
            )),
        ] {
            assert!(
                managed_test_root_creator_pid(&invalid).is_err(),
                "forged managed root was accepted: {invalid}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn historical_managed_roots_without_nested_witness_only_delete_exact_empty_roots() {
        use std::os::unix::fs::MetadataExt as _;

        let empty = unique_tmp_dir("managed-historical-empty");
        std::fs::create_dir(&empty).unwrap();
        let empty_identity = super::super::ohos::persistent_fs_identity(&empty, true).unwrap();
        let mut budget = historical_managed_budget();
        let cleanup = capture_empty_historical_managed_test_directory_with_budget(
            &empty,
            std::slice::from_ref(&empty_identity),
            "empty historical candidate",
            &mut budget,
        )
        .unwrap();
        execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
            directories: vec![cleanup],
            owner_records: Vec::new(),
            snapshot_records: Vec::new(),
            journal_records: Vec::new(),
        })
        .unwrap();
        assert!(!empty.exists());

        let root = unique_tmp_dir("managed-historical-foreign");
        std::fs::create_dir(&root).unwrap();
        let root_identity = super::super::ohos::persistent_fs_identity(&root, true).unwrap();
        let nested = root.join("nested");
        std::fs::write(&nested, b"same bytes").unwrap();
        let original_inode = std::fs::symlink_metadata(&nested).unwrap().ino();
        let displaced = root
            .parent()
            .unwrap()
            .join(format!(".{}-displaced", root.file_name().unwrap()));
        std::fs::rename(&nested, &displaced).unwrap();
        std::fs::write(&nested, b"same bytes").unwrap();
        assert_ne!(
            std::fs::symlink_metadata(&nested).unwrap().ino(),
            original_inode
        );
        let mut budget = historical_managed_budget();
        let error = capture_empty_historical_managed_test_directory_with_budget(
            &root,
            std::slice::from_ref(&root_identity),
            "historical same-root replacement",
            &mut budget,
        )
        .err()
        .expect("non-empty historical root must be preserved");
        assert!(format!("{error:#}").contains("non-empty"));
        assert_eq!(std::fs::read(&nested).unwrap(), b"same bytes");
        assert_eq!(std::fs::read(&displaced).unwrap(), b"same bytes");

        let control = unique_tmp_dir("managed-historical-forged-control");
        std::fs::create_dir(&control).unwrap();
        std::fs::write(control.join("foreign"), b"must survive").unwrap();
        let mut budget = historical_managed_budget();
        assert!(capture_empty_unwitnessed_historical_control_with_budget(
            &control,
            "forged non-empty control",
            &mut budget,
        )
        .is_err());
        assert_eq!(
            std::fs::read(control.join("foreign")).unwrap(),
            b"must survive"
        );

        for path in [&root, &control] {
            let cleanup = capture_unplanned_but_pid_bound_test_directory(
                path,
                "current test-owned historical negative root",
            )
            .unwrap();
            execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
                directories: vec![cleanup],
                owner_records: Vec::new(),
                snapshot_records: Vec::new(),
                journal_records: Vec::new(),
            })
            .unwrap();
        }
        std::fs::remove_file(displaced).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn historical_managed_discovery_consumes_streaming_entry_byte_and_path_budgets() {
        let root = unique_tmp_dir("managed-historical-budget");
        std::fs::create_dir(&root).unwrap();
        for index in 0..3 {
            std::fs::write(root.join(format!("entry-{index}")), b"data").unwrap();
        }
        let mut entries = super::super::ohos::TraversalBudget::bounded(2, 1024);
        let error = managed_record_paths_with_budget(&root, "no-match", &mut entries).unwrap_err();
        assert!(format!("{error:#}").contains("entry/directory traversal limit"));

        let first = root.join("first.json");
        let second = root.join("second.json");
        std::fs::write(&first, b"four").unwrap();
        std::fs::write(&second, b"four").unwrap();
        let mut bytes = super::super::ohos::TraversalBudget::bounded(8, 7);
        exact_test_record_witness_with_budget(&first, 16, "first budget record", &mut bytes)
            .unwrap();
        let error =
            exact_test_record_witness_with_budget(&second, 16, "second budget record", &mut bytes)
                .unwrap_err();
        assert!(format!("{error:#}").contains("total-byte limit"));

        let public = root.join("package");
        let mut journal = test_managed_prepared_journal(&public);
        journal.public_root = "x".repeat(513);
        let mut paths = super::super::ohos::TraversalBudget::bounded(32, 4096);
        let error = consume_managed_test_journal_fields(&journal, &mut paths).unwrap_err();
        assert!(format!("{error:#}").contains("path limit"));

        let cleanup = capture_unplanned_but_pid_bound_test_directory(
            &root,
            "current test-owned historical budget root",
        )
        .unwrap();
        execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
            directories: vec![cleanup],
            owner_records: Vec::new(),
            snapshot_records: Vec::new(),
            journal_records: Vec::new(),
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn historical_managed_discovery_charges_non_utf8_and_unresolved_entries() {
        use std::os::unix::ffi::OsStringExt;

        let mut budget = super::super::ohos::TraversalBudget::bounded(1, 1024);
        let first = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'a', 0xff]));
        let second = std::path::PathBuf::from(std::ffi::OsString::from_vec(vec![b'b', 0xfe]));
        assert!(historical_utf8_path_with_budget(first, &mut budget)
            .unwrap()
            .is_none());
        let error = historical_utf8_path_with_budget(second, &mut budget).unwrap_err();
        assert!(format!("{error:#}").contains("entry/directory traversal limit"));

        let mut unresolved = super::super::ohos::TraversalBudget::bounded(1, 0);
        unresolved.consume_entry_bytes(&[]).unwrap();
        assert!(unresolved.consume_entry_bytes(&[]).is_err());
    }

    #[test]
    fn managed_journal_rejects_cleanup_snapshot_path_escape_and_partial_witness() {
        let public = Utf8PathBuf::from("/tmp/uniffi-managed-journal-path/package");
        let mut journal = test_managed_prepared_journal(&public);
        journal.cleanup_snapshot_name = Some("../foreign.tar.gz".into());
        assert!(validate_managed_journal(&journal, &journal.package_identity, &public).is_err());

        journal.cleanup_snapshot_name = Some(format!(
            ".uniffi-managed-package-{}-{}-previous-generation.tar.gz",
            journal.package_identity, journal.generation
        ));
        journal.cleanup_snapshot_len = Some(1);
        assert!(validate_managed_journal(&journal, &journal.package_identity, &public).is_err());
    }

    #[cfg(unix)]
    fn exited_managed_test_generation() -> String {
        let mut child = Command::new("true").spawn().unwrap();
        let pid = child.id();
        assert!(child.wait().unwrap().success());
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{pid:x}-{now:x}-0")
    }

    #[cfg(unix)]
    fn write_test_managed_journal(
        parent: &Utf8Path,
        journal: &ManagedPackageJournal,
    ) -> super::super::ohos::DurableRecordWitness {
        match write_new_managed_journal(parent, journal).unwrap() {
            super::super::ohos::DurableRecordWrite::Durable(witness) => witness,
            super::super::ohos::DurableRecordWrite::NotCreated(error) => {
                panic!("test journal was not created: {error:#}")
            }
            super::super::ohos::DurableRecordWrite::CreatedDurabilityUncertain {
                error, ..
            } => panic!("test journal durability was uncertain: {error:#}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn historical_managed_unwitnessed_planned_candidate_preserves_every_record() {
        let temp = tempfile::tempdir().unwrap();
        let parent = Utf8PathBuf::from_path_buf(temp.path().canonicalize().unwrap()).unwrap();
        let public = parent.join("package");
        let journal =
            test_managed_prepared_journal_for_generation(&public, exited_managed_test_generation());
        let candidate = parent.join(&journal.candidate_name);
        std::fs::create_dir(&candidate).unwrap();
        let record = write_test_managed_journal(&parent, &journal);

        let error = plan_exact_managed_test_cleanup_with_budget(
            &public,
            &[],
            None,
            true,
            &mut historical_managed_budget(),
        )
        .err()
        .expect("unwitnessed candidate must preserve the entire transaction");
        assert!(format!("{error:#}").contains("without an exact root identity witness"));
        assert!(candidate.is_dir());
        assert!(record.path.is_file());
    }

    #[cfg(unix)]
    #[test]
    fn historical_managed_name_only_snapshot_intent_preserves_every_record() {
        let temp = tempfile::tempdir().unwrap();
        let parent = Utf8PathBuf::from_path_buf(temp.path().canonicalize().unwrap()).unwrap();
        let public = parent.join("package");
        let mut journal =
            test_managed_prepared_journal_for_generation(&public, exited_managed_test_generation());
        let mut records = vec![write_test_managed_journal(&parent, &journal)];
        let mut preserve_records = false;
        for state in [
            "candidateCreated",
            "building",
            "candidateReady",
            "buildClean",
            "renamingPublicToBackup",
            "publicBackedUp",
            "renamingCandidateToPublic",
            "candidatePublished",
            "publishingFinalOwner",
            "committed",
        ] {
            journal.state = state.into();
            append_managed_journal(&parent, &mut journal, &mut records, &mut preserve_records)
                .unwrap();
        }
        let snapshot_name = format!(
            ".uniffi-managed-package-{}-{}-previous-generation.tar.gz",
            journal.package_identity, journal.generation
        );
        journal.cleanup_snapshot_name = Some(snapshot_name.clone());
        journal.state = "snapshottingBackup".into();
        append_managed_journal(&parent, &mut journal, &mut records, &mut preserve_records).unwrap();
        assert!(!preserve_records);
        let snapshot = parent.join(snapshot_name);
        std::fs::write(&snapshot, b"unwitnessed snapshot bytes").unwrap();

        let error = plan_exact_managed_test_cleanup_with_budget(
            &public,
            &[],
            None,
            true,
            &mut historical_managed_budget(),
        )
        .err()
        .expect("name-only snapshot intent must preserve the entire transaction");
        assert!(format!("{error:#}").contains("without a persisted identity/digest/length witness"));
        assert!(snapshot.is_file());
        assert!(records.iter().all(|record| record.path.is_file()));
    }

    #[cfg(unix)]
    #[test]
    fn managed_nested_random_tempdir_residue_is_discovered_but_not_reowned() {
        use std::os::unix::fs::MetadataExt as _;
        use std::time::{Duration, Instant};

        let _cleanup_lock = historical_managed_cleanup_test_lock();
        let outer = tempfile::tempdir().unwrap();
        let outer = Utf8PathBuf::from_path_buf(outer.into_path()).unwrap();
        let outer_identity = super::super::ohos::persistent_fs_identity(&outer, true).unwrap();
        let sentinel = outer.join("unrelated-sentinel");
        std::fs::write(&sentinel, b"must survive managed residue cleanup").unwrap();
        let sentinel_before = std::fs::symlink_metadata(&sentinel).unwrap();
        let nested = outer.join("nested");
        let package = nested.join("package");
        write_test_manifest(&package);
        let control = nested.join(".package-nested-control");
        std::fs::create_dir(&control).unwrap();
        let acquired = control.join("acquired");
        let release = control.join("release");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::artifacts::tests::managed_package_lock_child",
                "--nocapture",
            ])
            .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package)
            .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
            .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
            .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "fail")
            .spawn()
            .unwrap();
        let started = Instant::now();
        while !acquired.exists() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(child.id() as i32, libc::SIGKILL);
        }
        assert!(!child.wait().unwrap().success());

        let public = canonicalize_invocation_output(&package).unwrap();
        let digest = managed_package_digest(&public);
        assert!(!managed_record_paths(&nested, &digest).is_empty());
        let (discovered, reports) = historical_managed_test_roots();
        assert!(
            discovered.contains(&public),
            "nested random TempDir journal was not discovered; reports={reports:?}"
        );
        let (cleaned, cleanup_reports) = cleanup_exited_historical_managed_test_controls();
        assert_eq!(cleaned, 0);
        assert!(
            cleanup_reports
                .iter()
                .any(|report| report.contains(public.as_str()) && report.contains("non-empty")),
            "historical non-empty root was not reported as preserved: {cleanup_reports:?}"
        );
        assert!(package.exists() && control.exists());
        assert!(!managed_record_paths(&nested, &digest).is_empty());

        let sentinel_after = std::fs::symlink_metadata(&sentinel).unwrap();
        assert_eq!(
            (
                sentinel_after.dev(),
                sentinel_after.ino(),
                sentinel_after.mtime()
            ),
            (
                sentinel_before.dev(),
                sentinel_before.ino(),
                sentinel_before.mtime()
            )
        );
        assert_eq!(
            std::fs::read(&sentinel).unwrap(),
            b"must survive managed residue cleanup"
        );
        // The current test created and retained the outer root identity before
        // the producer ran. Its final in-memory exact snapshot is test-local;
        // the historical scanner itself never adopts this current inventory.
        let cleanup = capture_managed_test_directory(
            &outer,
            std::slice::from_ref(&outer_identity),
            "current test-owned nested residue root",
        )
        .unwrap();
        execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
            directories: vec![cleanup],
            owner_records: Vec::new(),
            snapshot_records: Vec::new(),
            journal_records: Vec::new(),
        })
        .unwrap();
        assert!(!outer.exists());
    }

    #[cfg(unix)]
    #[test]
    fn managed_test_cleanup_reports_and_preserves_an_identity_mismatch() {
        let root = unique_tmp_dir("managed-cleanup-mismatch");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("value"), b"original").unwrap();
        let original_identity = super::super::ohos::persistent_fs_identity(&root, true).unwrap();
        let displaced = Utf8PathBuf::from(format!("{root}.displaced"));
        std::fs::rename(&root, &displaced).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("value"), b"replacement").unwrap();

        let error = capture_managed_test_directory(
            &root,
            &[original_identity],
            "managed mismatch sentinel",
        )
        .err()
        .expect("replacement identity must not be adopted")
        .to_string();
        assert!(error.contains("does not match"), "{error}");
        assert_eq!(std::fs::read(root.join("value")).unwrap(), b"replacement");
        assert_eq!(std::fs::read(displaced.join("value")).unwrap(), b"original");

        let replacement =
            capture_unplanned_but_pid_bound_test_directory(&root, "test-created replacement")
                .unwrap();
        let original = capture_unplanned_but_pid_bound_test_directory(
            &displaced,
            "test-created displaced original",
        )
        .unwrap();
        execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
            directories: vec![replacement, original],
            owner_records: Vec::new(),
            snapshot_records: Vec::new(),
            journal_records: Vec::new(),
        })
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn managed_package_kill_preserves_durable_journal_and_fails_closed() {
        use std::time::{Duration, Instant};

        let package_dir = unique_tmp_dir("managed-package-kill");
        write_test_manifest(&package_dir);
        let control = package_dir.parent().unwrap().join(format!(
            ".{}-kill-control",
            package_dir.file_name().unwrap()
        ));
        std::fs::create_dir(&control).unwrap();
        let acquired = control.join("acquired");
        let release = control.join("release");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::artifacts::tests::managed_package_lock_child",
                "--nocapture",
            ])
            .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
            .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
            .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
            .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "fail")
            .spawn()
            .unwrap();
        let producer_pid = child.id();
        let started = Instant::now();
        while !acquired.exists() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        unsafe {
            libc::kill(child.id() as i32, libc::SIGKILL);
        }
        let status = child.wait().unwrap();
        assert!(!status.success());

        let layout = ManagedLayout {
            package_dir: package_dir.clone(),
            source_root: package_dir.join("src/ffi"),
            artifact_root: package_dir.join("artifacts"),
            host_crates_root: package_dir.join("artifacts/rust"),
            manifest_path: package_dir.join("artifact-manifest.json"),
        };
        let public = canonicalize_invocation_output(&package_dir).unwrap();
        let digest = managed_package_digest(&public);
        let journals = managed_record_paths(public.parent().unwrap(), &digest);
        assert!(!journals.is_empty());
        assert!(ManagedPackageTransaction::begin(&layout).is_err());
        assert_eq!(
            managed_record_paths(public.parent().unwrap(), &digest),
            journals,
            "fail-closed audit must preserve every immutable record"
        );
        let residue = std::fs::read_dir(public.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!(".uniffi-managed-package-{digest}-"))
            })
            .count();
        assert_eq!(residue, 2, "candidate and build roots remain auditable");

        let control = control.canonicalize_utf8().unwrap();
        cleanup_exact_managed_test_case(&public, &[control], Some(producer_pid)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn managed_package_all_rename_boundaries_are_durable_and_fail_closed() {
        use std::time::{Duration, Instant};

        for boundary in [
            "journalDurable",
            "candidateCreated",
            "buildCreated",
            "beforePublicToBackup",
            "afterPublicToBackup",
            "beforeCandidateToPublic",
            "afterCandidateToPublic",
            "beforeFinalOwnerPublish",
            "afterFinalOwnerPublish",
            "beforeBackupCleanup",
            "afterBackupCleanup",
            "beforeSnapshotCleanup",
            "afterSnapshotCleanup",
            "beforeJournalCleanup",
            "afterJournalCleanup",
        ] {
            let package_dir = unique_tmp_dir(&format!("managed-crash-{boundary}"));
            write_test_manifest(&package_dir);
            let parent = package_dir.parent().unwrap().to_path_buf();
            let control = parent.join(format!(
                ".{}-{boundary}-control",
                package_dir.file_name().unwrap()
            ));
            std::fs::create_dir(&control).unwrap();
            let acquired = control.join("acquired");
            let release = control.join("release");
            let reached = control.join("reached");
            std::fs::write(&release, b"release").unwrap();
            let mut child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cli::artifacts::tests::managed_package_lock_child",
                    "--nocapture",
                ])
                .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
                .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
                .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
                .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "crash-boundary")
                .env("UNIFFI_TEST_MANAGED_CRASH_AT", boundary)
                .env("UNIFFI_TEST_MANAGED_CRASH_REACHED", &reached)
                .spawn()
                .unwrap();
            let producer_pid = child.id();
            let started = Instant::now();
            while !reached.exists() {
                assert!(
                    started.elapsed() < Duration::from_secs(60),
                    "timed out waiting for crash boundary {boundary}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!child.wait().unwrap().success());

            let public = canonicalize_invocation_output(&package_dir).unwrap();
            let digest = managed_package_digest(&public);
            let journals = managed_record_paths(&parent, &digest);
            let layout = ManagedLayout {
                package_dir: package_dir.clone(),
                source_root: package_dir.join("src/ffi"),
                artifact_root: package_dir.join("artifacts"),
                host_crates_root: package_dir.join("artifacts/rust"),
                manifest_path: package_dir.join("artifact-manifest.json"),
            };
            if boundary == "afterJournalCleanup" {
                assert!(journals.is_empty());
                let transaction = ManagedPackageTransaction::begin(&layout).unwrap();
                let error = transaction.abort(anyhow::anyhow!(
                    "test-only afterJournalCleanup startup probe"
                ));
                assert!(error
                    .to_string()
                    .contains("test-only afterJournalCleanup startup probe"));
                validate_managed_owner(&public, &parse_managed_owner(&public).unwrap()).unwrap();
            } else {
                assert!(!journals.is_empty(), "missing journal chain at {boundary}");
                assert!(
                    ManagedPackageTransaction::begin(&layout).is_err(),
                    "next invocation crossed crash boundary {boundary}"
                );
            }

            let control = control.canonicalize_utf8().unwrap();
            cleanup_exact_managed_test_case(&public, &[control], Some(producer_pid)).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn managed_package_guard_preserves_same_path_replacement() {
        use std::time::{Duration, Instant};

        let package_dir = unique_tmp_dir("managed-package-replacement");
        write_test_manifest(&package_dir);
        let control = package_dir.parent().unwrap().join(format!(
            ".{}-replacement-control",
            package_dir.file_name().unwrap()
        ));
        std::fs::create_dir(&control).unwrap();
        let acquired = control.join("acquired");
        let release = control.join("release");
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "cli::artifacts::tests::managed_package_lock_child",
                "--nocapture",
            ])
            .env("UNIFFI_MANAGED_LOCK_CHILD_PACKAGE", &package_dir)
            .env("UNIFFI_MANAGED_LOCK_CHILD_ACQUIRED", &acquired)
            .env("UNIFFI_MANAGED_LOCK_CHILD_RELEASE", &release)
            .env("UNIFFI_MANAGED_LOCK_CHILD_MODE", "fail")
            .spawn()
            .unwrap();
        let producer_pid = child.id();
        let started = Instant::now();
        while !acquired.exists() {
            assert!(started.elapsed() < Duration::from_secs(10));
            std::thread::sleep(Duration::from_millis(10));
        }
        let public = canonicalize_invocation_output(&package_dir).unwrap();
        let digest = managed_package_digest(&public);
        let journal_paths = managed_record_paths(public.parent().unwrap(), &digest);
        let journal_path = journal_paths.last().expect("managed record exists").clone();
        let journal: ManagedPackageJournal = serde_json::from_slice(
            &super::super::ohos::read_verified_regular_file_bounded(
                &journal_path,
                1024 * 1024,
                "replacement test journal",
            )
            .unwrap(),
        )
        .unwrap();
        let candidate = public.parent().unwrap().join(&journal.candidate_name);
        let displaced = public
            .parent()
            .unwrap()
            .join(format!("{}.displaced", journal.candidate_name));
        std::fs::rename(&candidate, &displaced).unwrap();
        std::fs::create_dir(&candidate).unwrap();
        std::fs::write(candidate.join("replacement"), b"user bytes").unwrap();
        std::fs::write(&release, b"release").unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(
            std::fs::read(candidate.join("replacement")).unwrap(),
            b"user bytes"
        );
        assert!(displaced.is_dir());
        assert!(journal_path.is_file());

        let control = control.canonicalize_utf8().unwrap();
        let error = plan_exact_managed_test_cleanup(
            &public,
            std::slice::from_ref(&control),
            Some(producer_pid),
            false,
        )
        .err()
        .expect("replacement must make normal exact cleanup preserve evidence")
        .to_string();
        assert!(
            error.contains("identity") || error.contains("unplanned package residue"),
            "{error}"
        );
        assert_eq!(
            std::fs::read(candidate.join("replacement")).unwrap(),
            b"user bytes"
        );
        assert!(displaced.is_dir() && journal_path.is_file());

        // The test itself created the replacement, while the displaced
        // original remains bound to the candidate identity in the immutable
        // journal.  Seal both inventories only after the producer exits.
        let records = capture_exact_managed_test_journals(
            public.parent().unwrap(),
            &public,
            &digest,
            Some(producer_pid),
        )
        .unwrap();
        let latest = &records.last().unwrap().0;
        let replacement = capture_unplanned_but_pid_bound_test_directory(
            &candidate,
            "managed replacement test replacement candidate",
        )
        .unwrap();
        let displaced_original = capture_managed_test_directory(
            &displaced,
            std::slice::from_ref(latest.candidate_root_identity.as_ref().unwrap()),
            "managed replacement test displaced original",
        )
        .unwrap();
        let public_cleanup = capture_managed_test_directory(
            &public,
            std::slice::from_ref(latest.previous_root_identity.as_ref().unwrap()),
            "managed replacement test public root",
        )
        .unwrap();
        let build = public.parent().unwrap().join(&latest.build_name);
        let control_cleanup = capture_unplanned_but_pid_bound_test_directory(
            &control,
            "managed replacement test synchronization root",
        )
        .unwrap();
        let mut directories = vec![
            public_cleanup,
            replacement,
            displaced_original,
            control_cleanup,
        ];
        if super::super::ohos::path_entry_exists(&build).unwrap() {
            directories.push(
                capture_managed_test_directory(
                    &build,
                    std::slice::from_ref(latest.build_root_identity.as_ref().unwrap()),
                    "managed replacement test build root",
                )
                .unwrap(),
            );
        }
        execute_exact_managed_test_cleanup(ManagedTestCleanupPlan {
            directories,
            owner_records: Vec::new(),
            snapshot_records: Vec::new(),
            journal_records: records.into_iter().map(|(_, witness)| witness).collect(),
        })
        .unwrap();
        assert!(
            journal_paths.iter().all(|path| !path.exists())
                && !candidate.exists()
                && !displaced.exists()
                && !public.exists()
        );
    }

    #[cfg(unix)]
    #[test]
    fn managed_directory_guard_detects_nested_and_root_aba() {
        let parent = unique_tmp_dir("managed-guard-aba");
        std::fs::create_dir_all(&parent).unwrap();
        let root = parent.join("owned");
        let mut guard = ManagedOwnedDirectory::create(root.clone()).unwrap();
        std::fs::create_dir(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/value"), b"same bytes").unwrap();
        guard.seal().unwrap();

        let value = root.join("nested/value");
        let moved = root.join("nested/value.moved");
        std::fs::rename(&value, &moved).unwrap();
        std::fs::rename(&moved, &value).unwrap();
        assert!(guard.cleanup().is_err(), "nested A->B->A was not detected");
        assert_eq!(std::fs::read(&value).unwrap(), b"same bytes");

        guard.state = ManagedOwnedDirectoryState::Armed;
        guard.seal().unwrap();
        let moved_root = parent.join("owned.moved");
        std::fs::rename(&root, &moved_root).unwrap();
        std::fs::rename(&moved_root, &root).unwrap();
        assert!(guard.cleanup().is_err(), "root A->B->A was not detected");
        assert_eq!(std::fs::read(&value).unwrap(), b"same bytes");
        guard.armed = false;
        let _ = std::fs::remove_dir_all(parent.as_std_path());
    }

    #[test]
    fn managed_layout_emits_entries_and_relative_manifest() {
        let mut args = empty_build_args();
        let package_dir = unique_tmp_dir("managed-layout-manifest");
        args.manifest_path = write_test_manifest(&package_dir);
        args.managed_layout = true;
        args.package_dir = Some(package_dir.clone());
        args.out_dir = None;
        args.target = vec![
            ArtifactTargetArg::Wasm,
            ArtifactTargetArg::MiniProgram,
            ArtifactTargetArg::Node,
            ArtifactTargetArg::Electron,
            ArtifactTargetArg::Harmony,
            ArtifactTargetArg::Apple,
            ArtifactTargetArg::Android,
        ];

        let targets = expand_targets(&args.target).unwrap();
        let layout = ManagedLayout::apply(&mut args, &targets)
            .unwrap()
            .expect("managed layout should resolve");
        let meta = test_cargo_metadata(package_dir.join("target"));
        layout.emit(&targets, &meta, &args).unwrap();

        let web = std::fs::read_to_string(package_dir.join("src/index.web.ts")).unwrap();
        assert!(web.contains("export * from \"./ffi/browser/index.web.ts\";"));
        assert!(web.contains("export type * from \"./ffi/common/public-types.ts\";"));

        let mini_program =
            std::fs::read_to_string(package_dir.join("src/index.mini-program.ts")).unwrap();
        assert!(mini_program.contains("export * from \"./ffi/browser/index.mini-program.ts\";"));
        assert!(mini_program.contains("export type * from \"./ffi/common/public-types.ts\";"));

        let node = std::fs::read_to_string(package_dir.join("src/index.node.ts")).unwrap();
        assert!(node.contains("export * from \"./ffi/node/index.ts\";"));
        assert!(node.contains("export type * from \"./ffi/common/public-types.ts\";"));

        let electron = std::fs::read_to_string(package_dir.join("src/index.electron.ts")).unwrap();
        assert!(electron.contains("export * from \"./ffi/electron/renderer.ts\";"));
        assert!(electron.contains("export type * from \"./ffi/common/public-types.ts\";"));

        let gitignore = std::fs::read_to_string(package_dir.join(".gitignore")).unwrap();
        assert!(gitignore.contains("# UniFFI generated build artifacts"));
        assert!(gitignore.contains("/artifacts/"));
        assert!(
            !gitignore.contains("src/ffi"),
            "FFI source must be reviewable and not ignored:\n{gitignore}"
        );

        let manifest_text =
            std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap();
        assert!(
            !manifest_text.contains(package_dir.as_str()),
            "manifest must not contain absolute package paths:\n{manifest_text}"
        );
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(manifest["schemaVersion"], 3);
        assert_eq!(manifest["namespace"], "uni_core");
        assert_eq!(
            manifest["targets"],
            serde_json::json!([
                "wasm",
                "mini-program",
                "node",
                "electron",
                "harmony",
                "apple",
                "android"
            ])
        );
        assert_eq!(manifest["source"]["root"], "src/ffi");
        assert_eq!(manifest["source"]["common"], "src/ffi/common");
        assert_eq!(manifest["source"]["swift"], "src/ffi/swift");
        assert_eq!(manifest["source"]["kotlin"], "src/ffi/kotlin");
        assert_eq!(manifest["entrypoints"]["electron"], "src/index.electron.ts");
        assert_eq!(
            manifest["entrypoints"]["harmony"],
            "artifacts/harmony/package/Index.ets"
        );
        assert_eq!(
            manifest["entrypoints"]["miniProgram"],
            "src/index.mini-program.ts"
        );
        assert_eq!(
            manifest["artifacts"]["wasm"]["glue"],
            "artifacts/browser/pkg/uni_core_wasm.js"
        );
        assert_eq!(
            manifest["artifacts"]["miniProgram"]["glue"],
            "artifacts/mini-program/uni_core_wasm.js"
        );
        assert_eq!(
            manifest["artifacts"]["miniProgram"]["wasm"],
            "artifacts/mini-program/uni_core_wasm_bg.wasm"
        );
        assert_eq!(
            manifest["artifacts"]["miniProgram"]["defaultWasmPath"],
            "/assets/uni_core_wasm_bg.wasm"
        );
        assert_eq!(
            manifest["artifacts"]["harmony"]["har"],
            "artifacts/harmony/uni-core-ohos.har"
        );
        assert_eq!(manifest["artifacts"]["harmony"]["kind"], "har");
        assert_eq!(
            manifest["artifacts"]["harmony"]["packageMetadata"],
            "artifacts/harmony/package/oh-package.json5"
        );
        assert_eq!(
            manifest["artifacts"]["harmony"]["metadata"]["package"]["name"],
            "uni-core-ohos"
        );
        assert_eq!(
            manifest["artifacts"]["harmony"]["metadata"]["package"]["version"],
            "0.1.0"
        );
        assert_eq!(
            manifest["artifacts"]["harmony"]["metadata"]["module"]["name"],
            "uni_core_ohos"
        );
        assert_eq!(
            manifest["artifacts"]["harmony"]["metadata"]["module"]["deviceTypes"],
            serde_json::json!(["phone", "tablet", "2in1"])
        );
        assert_eq!(
            manifest["artifacts"]["apple"]["xcframework"],
            "artifacts/apple/uni_core.xcframework"
        );
        assert_eq!(manifest["artifacts"]["apple"]["package"], "artifacts/apple");
        assert_eq!(manifest["artifacts"]["apple"]["product"], "UniCoreApple");
        assert_eq!(
            manifest["artifacts"]["android"]["jniLibs"],
            "artifacts/android/jniLibs"
        );
        assert_eq!(
            manifest["hostCrates"]["ohos"],
            "artifacts/rust/ohos/Cargo.toml"
        );

        let apple_package =
            std::fs::read_to_string(package_dir.join("artifacts/apple/Package.swift")).unwrap();
        assert!(apple_package.contains("name: \"UniCoreApple\""));
        assert!(apple_package.contains("name: \"uni_coreFFI\""));
        assert!(apple_package.contains("path: \"uni_core.xcframework\""));

        let apple_support = std::fs::read_to_string(
            package_dir.join("artifacts/apple/Sources/UniCoreApple/UniCoreApple.swift"),
        )
        .unwrap();
        assert!(apple_support.contains("public enum UniCoreApplePackage {}"));

        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_manifest_merges_incremental_target_runs() {
        let package_dir = unique_tmp_dir("managed-layout-merge");
        let meta = test_cargo_metadata(package_dir.join("target"));

        let mut js_args = empty_build_args();
        js_args.manifest_path = write_test_manifest(&package_dir);
        js_args.managed_layout = true;
        js_args.package_dir = Some(package_dir.clone());
        js_args.out_dir = None;
        js_args.target = vec![
            ArtifactTargetArg::Wasm,
            ArtifactTargetArg::MiniProgram,
            ArtifactTargetArg::Node,
        ];
        let js_targets = expand_targets(&js_args.target).unwrap();
        let js_layout = ManagedLayout::apply(&mut js_args, &js_targets)
            .unwrap()
            .expect("managed layout should resolve");
        js_layout.emit(&js_targets, &meta, &js_args).unwrap();

        let mut apple_args = empty_build_args();
        apple_args.manifest_path = package_dir.join("Cargo.toml");
        apple_args.managed_layout = true;
        apple_args.package_dir = Some(package_dir.clone());
        apple_args.out_dir = None;
        apple_args.target = vec![ArtifactTargetArg::Apple];
        let apple_targets = expand_targets(&apple_args.target).unwrap();
        let apple_layout = ManagedLayout::apply(&mut apple_args, &apple_targets)
            .unwrap()
            .expect("managed layout should resolve");
        apple_layout
            .emit(&apple_targets, &meta, &apple_args)
            .unwrap();

        let manifest_text =
            std::fs::read_to_string(package_dir.join("artifact-manifest.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
        assert_eq!(
            manifest["targets"],
            serde_json::json!(["wasm", "mini-program", "node", "apple"])
        );
        assert_eq!(manifest["source"]["browser"], "src/ffi/browser");
        assert_eq!(manifest["source"]["node"], "src/ffi/node");
        assert_eq!(manifest["source"]["swift"], "src/ffi/swift");
        assert_eq!(manifest["entrypoints"]["web"], "src/index.web.ts");
        assert_eq!(
            manifest["entrypoints"]["miniProgram"],
            "src/index.mini-program.ts"
        );
        assert_eq!(manifest["entrypoints"]["node"], "src/index.node.ts");
        assert_eq!(
            manifest["artifacts"]["wasm"]["wasm"],
            "artifacts/browser/pkg/uni_core_wasm_bg.wasm"
        );
        assert_eq!(
            manifest["artifacts"]["apple"]["xcframework"],
            "artifacts/apple/uni_core.xcframework"
        );
        assert_eq!(manifest["artifacts"]["apple"]["package"], "artifacts/apple");
        assert_eq!(manifest["artifacts"]["apple"]["product"], "UniCoreApple");
        assert_eq!(
            manifest["artifacts"]["miniProgram"]["defaultWasmPath"],
            "/assets/uni_core_wasm_bg.wasm"
        );
        assert_eq!(
            manifest["hostCrates"]["wasm"],
            "artifacts/rust/wasm/Cargo.toml"
        );

        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
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
    fn artifacts_cli_no_longer_exposes_checkout_tool_flags() {
        let artifacts_src = include_str!("artifacts.rs");
        for forbidden in [
            concat!("wasm-bindgen", "-dir"),
            concat!("ohos-rs", "-dir"),
            concat!("wasm-bindgen", "-bin"),
            concat!("ohrs", "-bin"),
            concat!("resolve_wasm", "_bindgen_bin"),
            concat!("resolve_ohrs", "_bin"),
        ] {
            assert!(
                !artifacts_src.contains(forbidden),
                "artifact CLI source still exposes `{forbidden}`:\n{artifacts_src}"
            );
        }
    }

    #[test]
    fn javascript_build_defaults_to_embedded_tooling() {
        let javascript_src = include_str!("javascript.rs");
        for forbidden in [
            concat!("default_value = \"wasm", "-bindgen\""),
            concat!("default_value = \"o", "hrs\""),
            concat!("wasm-bindgen", "-dir"),
            concat!("ohos-rs", "-dir"),
            concat!("wasm-bindgen", "-bin"),
            concat!("ohrs", "-bin"),
            concat!("install wasm", "-bindgen-cli"),
            concat!("install ohos", "-rs"),
        ] {
            assert!(
                !javascript_src.contains(forbidden),
                "javascript CLI source still exposes default external tooling `{forbidden}`:\n{javascript_src}"
            );
        }
        assert!(
            javascript_src.contains("run_wasm_bindgen_in_process"),
            "javascript build-wasm must use the built-in wasm-bindgen runner"
        );
        assert!(
            javascript_src.contains("super::ohos::build"),
            "javascript build-ohos must use the built-in OHOS builder"
        );
    }

    #[test]
    fn artifacts_cli_wires_harmony_har_options() {
        let artifacts_src = include_str!("artifacts.rs");
        for required in [
            concat!("ohos-package", "-name"),
            concat!("ohos-module", "-name"),
            concat!("ohos-package", "-version"),
            concat!("ohos-compatible-sdk", "-version"),
            concat!("ohos-compatible-sdk", "-type"),
            concat!("ohos-device", "-type"),
            concat!("ohos-package", "-type"),
            concat!("ohos-integrated", "-hsp"),
            concat!("ohos-hsp-bundle", "-name"),
            concat!("ohos-har", "-out"),
            concat!("ohos-runtime-hsp", "-out"),
            concat!("ohos-interface-har", "-out"),
            concat!("ohos-tgz", "-out"),
            concat!("ohos-hvigor", "w"),
            concat!("ohos-oh", "pm"),
            concat!("ohos-deveco-sdk", "-home"),
            concat!("ohos-no", "-har"),
        ] {
            assert!(
                artifacts_src.contains(required),
                "artifact CLI source missing harmony HAR option `{required}`:\n{artifacts_src}"
            );
        }

        let javascript_src = include_str!("javascript.rs");
        for required in [
            concat!("package", "-name"),
            concat!("module", "-name"),
            concat!("package", "-version"),
            concat!("compatible-sdk", "-version"),
            concat!("compatible-sdk", "-type"),
            concat!("device", "-type"),
            concat!("package", "-type"),
            concat!("integrated", "-hsp"),
            concat!("hsp-bundle", "-name"),
            concat!("har", "-out"),
            concat!("runtime-hsp", "-out"),
            concat!("interface-har", "-out"),
            concat!("tgz", "-out"),
            concat!("hvigor", "w"),
            concat!("oh", "pm"),
            concat!("deveco-sdk", "-home"),
            concat!("no", "-har"),
        ] {
            assert!(
                javascript_src.contains(required),
                "javascript build-ohos source missing HAR option `{required}`:\n{javascript_src}"
            );
        }
    }
}
