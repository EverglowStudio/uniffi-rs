/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::javascript::{
    build_napi, build_ohos, build_wasm, emit_mini_program_wasm_runtime,
    mini_program_default_wasm_path, BuildNapiArgs, BuildOhosArgs, BuildWasmArgs,
    NapiBuildFlavorArg, WasmBindgenTargetArg,
};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::MetadataCommand;
use clap::{Args, Subcommand, ValueEnum};
use std::io::{Seek, Write};
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

    /// Cargo features enabled when building native Apple/Android core artifacts. May be repeated or comma-separated.
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

    /// Output directory for built OHOS dist artifacts (intermediate native output).
    #[clap(long = "ohos-dist-dir")]
    ohos_dist_dir: Option<Utf8PathBuf>,

    /// OHPM package name for generated HAR metadata (supports scoped names like `@scope/name`).
    #[clap(long = "ohos-package-name")]
    ohos_package_name: Option<String>,

    /// Output `.har` path. Defaults to `<artifact-root>/<package>.har`.
    #[clap(long = "ohos-har-out")]
    ohos_har_out: Option<Utf8PathBuf>,

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

impl ManagedLayout {
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
            let harmony_package = args
                .ohos_package_name
                .clone()
                .unwrap_or_else(|| format!("{}-ohos", meta.package_name));
            args.ohos_dist_dir = Some(artifact_root.join("harmony/dist"));
            args.ohos_har_out = Some(artifact_root.join(format!("harmony/{harmony_package}.har")));
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
        self.emit_gitignore()?;
        self.emit_manifest(targets, meta, args)?;
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

    fn emit_manifest(
        &self,
        targets: &ExpandedTargets,
        meta: &CargoPackageMetadata,
        args: &BuildArgs,
    ) -> Result<()> {
        let namespace = &meta.lib_target_name;
        let wasm_stem = format!("{}_wasm", rust_identifier(&meta.package_name));
        let node_env = format!("UNIFFI_{}_NAPI_PATH", namespace.to_ascii_uppercase());
        let harmony_package = args
            .ohos_package_name
            .clone()
            .unwrap_or_else(|| format!("{}-ohos", meta.package_name));
        let manifest = serde_json::json!({
            "schemaVersion": 2,
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
                "harmony": serde_json::Value::Null,
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
                        "addon": self.addon_rel("node", namespace)?,
                        "env": node_env,
                    })
                } else { serde_json::Value::Null },
                "electron": if targets.electron {
                    serde_json::json!({
                        "addon": self.addon_rel("electron", namespace)?,
                        "env": node_env,
                    })
                } else { serde_json::Value::Null },
                "harmony": if targets.harmony {
                    serde_json::json!({
                        "har": self.rel(&self.artifact_root.join(format!("harmony/{harmony_package}.har")))?,
                        "dist": self.rel(&self.artifact_root.join("harmony/dist"))?,
                        "package": self.rel(&self.artifact_root.join("harmony/package"))?,
                    })
                } else { serde_json::Value::Null },
                "apple": if targets.apple {
                    serde_json::json!({
                        "xcframework": self.rel(args.apple_xcframework_out.as_ref().expect("managed apple path derived"))?,
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
        std::fs::write(&self.manifest_path, format!("{text}\n"))
            .with_context(|| format!("writing managed artifact manifest {}", self.manifest_path))?;
        Ok(())
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

        merge_manifest_targets(&mut manifest, &existing);
        for key in ["source", "entrypoints", "artifacts", "hostCrates"] {
            merge_manifest_object_section(&mut manifest, &existing, key);
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

    fn addon_rel(&self, subdir: &str, fallback_stem: &str) -> Result<String> {
        let dir = self.artifact_root.join(subdir);
        if dir.exists() {
            let mut nodes = Vec::new();
            for entry in std::fs::read_dir(&dir).with_context(|| format!("reading {dir}"))? {
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
                return self.rel(path);
            }
        }
        self.rel(&dir.join(format!("{fallback_stem}.node")))
    }
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

fn build(mut args: BuildArgs) -> Result<()> {
    let targets = expand_targets(&args.target)?;
    if targets.mini_program && args.wasm_bindgen_target != WasmBindgenTargetArg::Web {
        bail!("--target mini-program requires --wasm-bindgen-target web");
    }
    let managed_layout = ManagedLayout::apply(&mut args, &targets)?;

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
        build_napi(args.to_napi_args(napi_flavors)?).context("building N-API artifact target")?;
    }

    if targets.harmony {
        build_ohos(args.to_ohos_args()?).context("building Harmony/OpenHarmony artifact target")?;
    }

    if let Some(layout) = managed_layout {
        let meta = cargo_package_metadata(&args.manifest_path)?;
        layout
            .emit(&targets, &meta, &args)
            .context("emitting managed artifact layout")?;
    }

    Ok(())
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
            artifact_dir: self.artifact_dir.clone(),
            wasm_bindgen_out_dir: self.wasm_bindgen_out_dir.clone(),
            wasm_bindgen_target: self.wasm_bindgen_target,
            cargo_bin: self.cargo_bin.clone(),
            release: self.release,
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
            artifact_dir: self.artifact_dir.clone(),
            flavor,
            cargo_bin: self.cargo_bin.clone(),
            target_dir: self.napi_target_dir.clone(),
            release: self.release,
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
            artifact_dir: self.artifact_dir.clone(),
            dist_dir: self.ohos_dist_dir.clone(),
            package_name: self.ohos_package_name.clone(),
            har_out: self.ohos_har_out.clone(),
            no_har: self.ohos_no_har,
            arch: self.ohos_arch.clone(),
            cargo_bin: self.cargo_bin.clone(),
            target_dir: self.ohos_target_dir.clone(),
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

fn rust_identifier(package_name: &str) -> String {
    package_name.replace('-', "_")
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
    if staging_dir.exists() {
        std::fs::remove_dir_all(staging_dir)
            .with_context(|| format!("removing stale AAR staging dir {staging_dir}"))?;
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

    fn empty_build_args() -> BuildArgs {
        BuildArgs {
            manifest_path: Utf8PathBuf::from("/repo/crates/core/Cargo.toml"),
            out_dir: Some(Utf8PathBuf::from("/repo/generated")),
            target: vec![ArtifactTargetArg::Wasm],
            library_path: None,
            source: None,
            host_crates_dir: None,
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
            ohos_dist_dir: None,
            ohos_package_name: None,
            ohos_har_out: None,
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
        let meta = CargoPackageMetadata {
            target_directory: package_dir.join("target"),
            package_name: "uni-core".to_string(),
            lib_target_name: "uni_core".to_string(),
        };
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
        assert_eq!(manifest["schemaVersion"], 2);
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
        assert_eq!(
            manifest["artifacts"]["apple"]["xcframework"],
            "artifacts/apple/uni_core.xcframework"
        );
        assert_eq!(
            manifest["artifacts"]["android"]["jniLibs"],
            "artifacts/android/jniLibs"
        );
        assert_eq!(
            manifest["hostCrates"]["ohos"],
            "artifacts/rust/ohos/Cargo.toml"
        );

        let _ = std::fs::remove_dir_all(package_dir.as_std_path());
    }

    #[test]
    fn managed_manifest_merges_incremental_target_runs() {
        let package_dir = unique_tmp_dir("managed-layout-merge");
        let meta = CargoPackageMetadata {
            target_directory: package_dir.join("target"),
            package_name: "uni-core".to_string(),
            lib_target_name: "uni_core".to_string(),
        };

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
    fn computes_apple_staticlib_path() {
        let meta = CargoPackageMetadata {
            target_directory: Utf8PathBuf::from("/repo/target"),
            package_name: "uni-core".to_string(),
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
            package_name: "uni-core".to_string(),
            lib_target_name: "uni_core".to_string(),
        };
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
            concat!("ohos-har", "-out"),
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
            concat!("har", "-out"),
            concat!("no", "-har"),
        ] {
            assert!(
                javascript_src.contains(required),
                "javascript build-ohos source missing HAR option `{required}`:\n{javascript_src}"
            );
        }
    }
}
