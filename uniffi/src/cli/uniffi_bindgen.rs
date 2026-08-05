/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

#[cfg(feature = "cli-javascript")]
use super::artifacts;
#[cfg(feature = "cli-javascript")]
use super::javascript;
use camino::Utf8PathBuf;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::fmt;
use uniffi_bindgen::{
    bindings::{generate, python, GenerateOptions, TargetLanguage},
    BindgenLoader, GlobalConfig,
};
#[cfg(feature = "cli-javascript")]
use uniffi_bindgen_javascript::{FlavorTarget, GenerateJsOptions, HostCrateOptions};
use uniffi_pipeline::PrintOptions;

/// TargetLanguage uniffi_bindgen, with a `clap::ValueEnum` derive.
#[derive(Copy, Clone, ValueEnum)]
enum TargetLanguageArg {
    Kotlin,
    Swift,
    Python,
    Ruby,
    #[cfg(feature = "cli-javascript")]
    Javascript,
}

impl fmt::Display for TargetLanguageArg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Kotlin => write!(f, "kotlin"),
            Self::Swift => write!(f, "swift"),
            Self::Python => write!(f, "python"),
            Self::Ruby => write!(f, "ruby"),
            #[cfg(feature = "cli-javascript")]
            Self::Javascript => write!(f, "javascript"),
        }
    }
}

impl From<TargetLanguageArg> for TargetLanguage {
    fn from(l: TargetLanguageArg) -> Self {
        match l {
            TargetLanguageArg::Kotlin => Self::Kotlin,
            TargetLanguageArg::Swift => Self::Swift,
            TargetLanguageArg::Python => Self::Python,
            TargetLanguageArg::Ruby => Self::Ruby,
            #[cfg(feature = "cli-javascript")]
            TargetLanguageArg::Javascript => Self::Javascript,
        }
    }
}

/// Which JS backend(s) to emit when `--language javascript` is set.
///
/// `electron` is not a standalone ABI — it consumes the napi flavor and
/// additionally emits `preload.cjs` + `index.js`.
#[cfg(feature = "cli-javascript")]
#[derive(Copy, Clone, ValueEnum)]
enum JsFlavorArg {
    Wasm,
    Napi,
    Electron,
    #[cfg(feature = "cli-ohos")]
    Harmony,
}

#[cfg(feature = "cli-javascript")]
impl From<JsFlavorArg> for FlavorTarget {
    fn from(value: JsFlavorArg) -> Self {
        match value {
            JsFlavorArg::Wasm => FlavorTarget::Wasm,
            JsFlavorArg::Napi => FlavorTarget::Napi,
            JsFlavorArg::Electron => FlavorTarget::Electron,
            #[cfg(feature = "cli-ohos")]
            JsFlavorArg::Harmony => FlavorTarget::Harmony,
        }
    }
}

// Structs to help our cmdline parsing. Note that docstrings below form part
// of the "help" output.

/// Scaffolding and bindings generator for Rust
#[derive(Parser)]
#[clap(name = "uniffi-bindgen")]
#[clap(version = clap::crate_version!())]
#[clap(propagate_version = true)]
struct Cli {
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Generate foreign language bindings
    Generate {
        /// Foreign language(s) for which to build bindings.
        #[clap(long, short, value_enum)]
        language: Vec<TargetLanguageArg>,

        /// JavaScript target flavor(s) to emit. Only meaningful when
        /// `--language javascript` is passed. May be repeated.
        /// `electron` implies `napi` plus preload+renderer files.
        #[cfg(feature = "cli-javascript")]
        #[clap(long = "flavor", value_enum)]
        js_flavor: Vec<JsFlavorArg>,

        /// Directory in which to write generated files. Default is same folder as .udl file.
        #[clap(long, short)]
        out_dir: Option<Utf8PathBuf>,

        /// Do not try to format the generated bindings.
        #[clap(long, short)]
        no_format: bool,

        /// Path to a global config file. Supports [defaults]s, [crates.<name>], and [crate-roots].
        /// [default]s are merged with per-crate `uniffi.toml` files, then with [crates.<name>] overrides here.
        #[clap(long, short)]
        config: Option<Utf8PathBuf>,

        /// Deprecated
        ///
        /// This used to signal that a source file is a library rather than a UDL file.
        /// Nowadays, UniFFI will auto-detect this.
        #[clap(long = "library")]
        _library_mode: bool,

        /// When `--library` is passed, only generate bindings for one crate.
        /// When `--library` is not passed, use this as the crate name instead of attempting to
        /// locate and parse Cargo.toml.
        #[clap(long = "crate")]
        crate_name: Option<String>,

        /// Source to generate bindings from.
        ///
        /// Possible values:
        ///
        /// * Path to a UDL file
        /// * Path to a library file
        /// * `src:[crate-name]` to generate from Rust sources
        source: Utf8PathBuf,

        /// Downstream core crate `Cargo.toml` used to derive host-crate
        /// metadata (package name, path dependency target). Required for
        /// JavaScript generation.
        #[cfg(feature = "cli-javascript")]
        #[clap(long = "manifest-path")]
        manifest_path: Option<Utf8PathBuf>,

        /// Directory (default `<out-dir>/native/hosts`) in which to emit
        /// generated host crates. It must remain below the package root.
        #[cfg(feature = "cli-javascript")]
        #[clap(long = "host-crates-dir")]
        host_crates_dir: Option<Utf8PathBuf>,

        /// Directory used by generated JavaScript backend entrypoints as the
        /// default location for built non-source artifacts such as `.node`
        /// addons. Only meaningful with `--language javascript`.
        #[cfg(feature = "cli-javascript")]
        #[clap(long = "artifact-dir")]
        artifact_dir: Option<Utf8PathBuf>,

        /// Whether we should exclude dependencies when running "cargo metadata".
        /// This will mean external types may not be resolved if they are implemented in crates
        /// outside of this workspace.
        /// This can be used in environments when all types are in the namespace and fetching
        /// all sub-dependencies causes obscure platform specific problems.
        #[clap(long)]
        metadata_no_deps: bool,

        /// Features to enable when generating from Rust sources
        #[clap(short, long)]
        features: Vec<String>,

        /// Enable all features
        #[clap(long)]
        all_features: bool,

        /// Don't auto-enable default features
        #[clap(long)]
        no_default_features: bool,

        /// Target triple to use when generating from Rust sources
        #[clap(long)]
        target: Option<String>,
    },

    /// Generate Rust scaffolding code
    Scaffolding {
        /// Directory in which to write generated files. Default is same folder as .udl file.
        #[clap(long, short)]
        out_dir: Option<Utf8PathBuf>,

        /// Do not try to format the generated bindings.
        #[clap(long, short)]
        no_format: bool,

        /// Path to the UDL file.
        udl_file: Utf8PathBuf,
    },

    /// Inspect the bindings render pipeline
    Pipeline(PipelineArgs),

    /// JavaScript-target-specific workflows
    #[cfg(feature = "cli-javascript")]
    Javascript(javascript::JavascriptArgs),

    /// Build final artifacts for UniFFI consumer targets
    #[cfg(feature = "cli-javascript")]
    Artifacts(artifacts::ArtifactsArgs),
}

#[derive(Args)]
struct PipelineArgs {
    /// Pass in a cdylib path rather than a UDL file
    #[clap(long = "library")]
    library_mode: bool,

    /// Path to the UDL file, or cdylib if `library-mode` is specified
    source: Utf8PathBuf,

    /// When `--library` is passed, only generate bindings for one crate.
    /// When `--library` is not passed, use this as the crate name instead of attempting to
    /// locate and parse Cargo.toml.
    #[clap(long = "crate")]
    crate_name: Option<String>,

    /// Whether we should exclude dependencies when running "cargo metadata".
    /// This will mean external types may not be resolved if they are implemented in crates
    /// outside of this workspace.
    /// This can be used in environments when all types are in the namespace and fetching
    /// all sub-dependencies causes obscure platform specific problems.
    #[clap(long)]
    metadata_no_deps: bool,

    /// Bindings Language
    language: TargetLanguageArg,

    /// Only show passes that match <PASS>
    ///
    /// Use `last` to only show the last pass, this can be useful when you're writing new pipelines
    #[clap(short, long)]
    pass: Option<String>,

    /// Don't show diffs for middle passes
    #[clap(long)]
    no_diff: bool,

    /// Only show data for types with name <FILTER_TYPE>
    #[clap(short = 't', long = "type")]
    filter_type: Option<String>,

    /// Only show data for items with fields that match <FILTER>
    #[clap(short = 'n', long = "name")]
    filter_name: Option<String>,
}

pub fn run_main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Generate {
            language,
            #[cfg(feature = "cli-javascript")]
            js_flavor,
            out_dir,
            no_format,
            config,
            source,
            crate_name,
            metadata_no_deps,
            features,
            no_default_features,
            all_features,
            target,
            #[cfg(feature = "cli-javascript")]
            manifest_path,
            #[cfg(feature = "cli-javascript")]
            host_crates_dir,
            #[cfg(feature = "cli-javascript")]
            artifact_dir,
            ..
        } => {
            if language.is_empty() {
                panic!("please specify at least one language with --language")
            }
            let out_dir =
                out_dir.expect("--out-dir is required when generating {language} bindings");

            #[cfg(not(feature = "cli-javascript"))]
            {
                generate(GenerateOptions {
                    languages: language.into_iter().map(TargetLanguage::from).collect(),
                    out_dir: out_dir.clone(),
                    source: source.clone(),
                    config_override: config.clone(),
                    crate_filter: crate_name.clone(),
                    metadata_no_deps,
                    format: !no_format,
                    features: features.clone(),
                    all_features,
                    no_default_features,
                    target: target.clone(),
                })?;
            }

            #[cfg(feature = "cli-javascript")]
            {
                // Split Javascript off: it is emitted by a standalone crate
                // with its own option shape (flavors + electron consumption
                // form).
                let (js_langs, other_langs): (Vec<_>, Vec<_>) = language
                    .into_iter()
                    .partition(|l| matches!(l, TargetLanguageArg::Javascript));

                if !other_langs.is_empty() {
                    generate(GenerateOptions {
                        languages: other_langs.into_iter().map(TargetLanguage::from).collect(),
                        out_dir: out_dir.clone(),
                        source: source.clone(),
                        config_override: config.clone(),
                        crate_filter: crate_name.clone(),
                        metadata_no_deps,
                        format: !no_format,
                        features: features.clone(),
                        all_features,
                        no_default_features,
                        target: target.clone(),
                    })?;
                }

                if js_langs.is_empty() {
                    return Ok(());
                }
                if js_flavor.is_empty() {
                    #[cfg(feature = "cli-ohos")]
                    anyhow::bail!(
                        "--flavor is required when --language javascript is set \
                         (pick one or more of: wasm, napi, electron, harmony)"
                    );
                    #[cfg(not(feature = "cli-ohos"))]
                    anyhow::bail!(
                        "--flavor is required when --language javascript is set \
                         (pick one or more of: wasm, napi, electron)"
                    );
                }
                let manifest = manifest_path.clone().ok_or_else(|| {
                    anyhow::anyhow!("--language javascript requires --manifest-path <Cargo.toml>")
                })?;
                let mut paths = uniffi_bindgen::BindgenPaths::default();
                let global_config = if let Some(cfg) = &config {
                    let (global_config, crate_roots_layer) = GlobalConfig::from_file(cfg)?;
                    if let Some(layer) = crate_roots_layer {
                        paths.add_layer(layer);
                    }
                    global_config
                } else {
                    GlobalConfig::default()
                };
                #[cfg(feature = "cargo-metadata")]
                paths.add_cargo_metadata_layer(uniffi_bindgen::CargoMetadataOptions {
                    no_deps: metadata_no_deps,
                    all_features,
                    no_default_features,
                    features,
                })?;
                let loader = BindgenLoader::new(paths, global_config);
                let package_root = out_dir.clone();
                uniffi_bindgen_javascript::generate(
                    &loader,
                    GenerateJsOptions {
                        source,
                        package_root: package_root.clone(),
                        out_dir,
                        artifact_dir,
                        config_override: config,
                        crate_filter: crate_name,
                        metadata_no_deps,
                        flavors: js_flavor.into_iter().map(FlavorTarget::from).collect(),
                        host_crates: HostCrateOptions {
                            manifest_path: manifest,
                            host_crates_dir: host_crates_dir
                                .clone()
                                .unwrap_or_else(|| package_root.join("native/hosts")),
                            logical_host_crates_dir: None,
                        },
                    },
                )?;
            }
            #[cfg(feature = "cli-javascript")]
            // The cfg block above owns the return path only for the JS build;
            // keep the outer match arm fallible without an early process exit.
            let _ = ();
        }
        #[cfg(feature = "cli-javascript")]
        Commands::Javascript(args) => {
            javascript::run(args)?;
        }
        #[cfg(feature = "cli-javascript")]
        Commands::Artifacts(args) => {
            artifacts::run(args)?;
        }
        Commands::Scaffolding {
            out_dir,
            no_format,
            udl_file,
        } => {
            uniffi_bindgen::generate_component_scaffolding(
                &udl_file,
                out_dir.as_deref(),
                !no_format,
            )?;
        }
        Commands::Pipeline(args) => {
            #[allow(unused_mut)]
            let mut paths = uniffi_bindgen::BindgenPaths::default();
            #[cfg(feature = "cargo-metadata")]
            paths.add_cargo_metadata_layer(uniffi_bindgen::CargoMetadataOptions {
                no_deps: args.metadata_no_deps,
                ..uniffi_bindgen::CargoMetadataOptions::default()
            })?;
            let global_config = GlobalConfig::default();
            let loader = BindgenLoader::new(paths, global_config);
            let metadata = loader.load_metadata(&args.source)?;
            let initial_root = loader.load_pipeline_initial_root(&args.source, metadata)?;

            let opts = PrintOptions {
                pass: args.pass,
                no_diff: args.no_diff,
                filter_type: args.filter_type,
                filter_name: args.filter_name,
            };
            match args.language {
                TargetLanguageArg::Python => python::pipeline().print_passes(initial_root, opts)?,
                language => unimplemented!("{language} does not use the bindings IR pipeline yet"),
            };
        }
    };
    Ok(())
}
