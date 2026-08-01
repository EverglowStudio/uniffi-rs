/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! # Swift bindings backend for UniFFI
//!
//! This module generates Swift bindings from a [`crate::ComponentInterface`] definition,
//! using Swift's builtin support for loading C header files.
//!
//! Conceptually, the generated bindings are split into two Swift modules, one for the low-level
//! C FFI layer and one for the higher-level Swift bindings. For a UniFFI component named "example"
//! we generate:
//!
//!   * A C header file `exampleFFI.h` declaring the low-level structs and functions for calling
//!     into Rust, along with a corresponding `exampleFFI.modulemap` to expose them to Swift.
//!
//!   * A Swift source file `example.swift` that imports the `exampleFFI` module and wraps it
//!     to provide the higher-level Swift API.
//!
//! Most of the concepts in a [`crate::ComponentInterface`] have an obvious counterpart in Swift,
//! with the details documented in inline comments where appropriate.
//!
//! To handle lifting/lowering/serializing types across the FFI boundary, the Swift code
//! defines a `protocol ViaFfi` that is analogous to the `uniffi::ViaFfi` Rust trait.
//! Each type that can traverse the FFI conforms to the `ViaFfi` protocol, which specifies:
//!
//!  * The corresponding low-level type.
//!  * How to lift from and lower into into that type.
//!  * How to read from and write into a byte buffer.
//!

use crate::{
    bindings::GenerateOptions,
    interface::{apply_exclusions, rename},
    BindgenLoader, BindgenPaths, Component, ComponentInterface, GlobalConfig,
};
use anyhow::{bail, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fs_err as fs;
use std::collections::HashMap;
use std::process::Command;

mod gen_swift;
use gen_swift::{
    generate_bindings_with_stream_runtime, generate_header, generate_modulemap,
    generate_swift_with_stream_runtime, Config,
};

#[cfg(feature = "bindgen-tests")]
pub mod test;

/// The Swift bindings generated from a [`crate::ComponentInterface`].
///
struct Bindings {
    /// The contents of the generated `.swift` file, as a string.
    library: String,
    /// The contents of the generated `.h` file, as a string.
    header: String,
    /// The contents of the generated `.modulemap` file, as a string.
    modulemap: Option<String>,
}

/// Generate Swift bindings
///
/// Returns the components generated
pub fn generate(
    loader: &BindgenLoader,
    options: GenerateOptions,
) -> Result<Vec<Component<Config>>> {
    let metadata = loader.load_metadata(&options.source)?;
    if let Some(crate_filter) = &options.crate_filter {
        if !metadata.contains_key(crate_filter) {
            bail!("No UniFFI metadata found for crate {crate_filter}");
        }
    }
    let cis = loader.load_cis(&options.source, metadata)?;
    let mut components = loader.load_components(cis, parse_config)?;
    apply_renames(&mut components);
    for c in components.iter_mut() {
        // Call derive_ffi_functions after `apply_renames`
        c.ci.derive_ffi_funcs()?;
    }

    write_component_bindings(
        &mut components,
        options.crate_filter.as_deref(),
        &options.out_dir,
        options.format,
    )?;
    Ok(components)
}

/// Write all Swift component bindings destined for a single generated source set.
///
/// Output stream functions in that set share one public `UniFfiStream` runtime, even when
/// multiple components contain streams. The generated Swift sources are compiled together by the
/// Swift runner and are normally added to one Swift target by consumers.
fn write_component_bindings(
    components: &mut [Component<Config>],
    crate_filter: Option<&str>,
    out_dir: &Utf8Path,
    format: bool,
) -> Result<()> {
    let stream_runtime_emitter = stream_runtime_emitter_index(components, crate_filter);

    for (component_index, Component { ci, config, .. }) in components.iter_mut().enumerate() {
        if let Some(crate_filter) = crate_filter {
            if ci.crate_name() != crate_filter {
                continue;
            }
        }
        let include_stream_runtime = stream_runtime_emitter == Some(component_index);
        let Bindings {
            header,
            library,
            modulemap,
        } = generate_bindings_with_stream_runtime(config, ci, include_stream_runtime)?;

        let source_file = out_dir.join(format!("{}.swift", config.module_name()));
        fs::write(&source_file, library)?;

        let header_file = out_dir.join(config.header_filename());
        fs::write(header_file, header)?;

        if let Some(modulemap) = modulemap {
            let modulemap_file = out_dir.join(config.modulemap_filename());
            fs::write(modulemap_file, modulemap)?;
        }

        if format {
            let commands_to_try = [
                // Available in Xcode 16.
                vec!["xcrun", "swift-format"],
                // The official swift-format command name.
                vec!["swift-format"],
                // Shortcut for the swift-format command.
                vec!["swift", "format"],
                vec!["swiftformat"],
            ];

            let successful_output = commands_to_try.into_iter().find_map(|command| {
                Command::new(command[0])
                    .args(&command[1..])
                    .arg(source_file.as_str())
                    .output()
                    .ok()
            });
            if successful_output.is_none() {
                println!(
                    "Warning: Unable to auto-format {} using swift-format. Please make sure it is installed.",
                    source_file.as_str()
                );
            }
        }
    }
    Ok(())
}

/// Select the single output-stream runtime emitter for a generated Swift source set.
fn stream_runtime_emitter_index(
    components: &[Component<Config>],
    crate_filter: Option<&str>,
) -> Option<usize> {
    components.iter().position(|component| {
        component.ci.has_stream_fns()
            && crate_filter.map_or(true, |crate_filter| {
                component.ci.crate_name() == crate_filter
            })
    })
}

/// Generate Swift bindings (specialized version)
///
/// This is used by the uniffi-bindgen-swift command, which supports Swift-specific options.
///
/// In the future, we may want to replace the generalized `uniffi-bindgen` with a set of
/// specialized `uniffi-bindgen-[language]` commands.
pub fn generate_swift_bindings(options: SwiftBindingsOptions) -> Result<()> {
    #[cfg(not(feature = "cargo-metadata"))]
    let mut paths = BindgenPaths::default();

    #[cfg(feature = "cargo-metadata")]
    let mut paths = BindgenPaths::default();

    let global_config = if let Some(ref path) = options.config {
        let (config, crate_roots_layer) = GlobalConfig::from_file(path)?;
        if let Some(layer) = crate_roots_layer {
            paths.add_layer(layer);
        }
        config
    } else {
        GlobalConfig::default()
    };

    // TODO: input cargo features and the target from the CLI and use that to build
    // `CargoMetadataOptions`
    #[cfg(feature = "cargo-metadata")]
    paths.add_cargo_metadata_layer(crate::CargoMetadataOptions {
        no_deps: options.metadata_no_deps,
        ..crate::CargoMetadataOptions::default()
    })?;

    fs::create_dir_all(&options.out_dir)?;

    let loader = BindgenLoader::new(paths, global_config);
    let metadata = loader.load_metadata(&options.source)?;
    let cis = loader.load_cis(&options.source, metadata)?;
    let mut components = loader.load_components(cis, parse_config)?;
    apply_renames(&mut components);
    // Call derive_ffi_funcs after apply_renames()
    for Component { ci, .. } in components.iter_mut() {
        ci.derive_ffi_funcs()?;
    }

    let stream_runtime_emitter = stream_runtime_emitter_index(&components, None);
    for (component_index, Component { ci, config }) in components.iter().enumerate() {
        if options.generate_swift_sources {
            let source_file = options
                .out_dir
                .join(format!("{}.swift", config.module_name()));
            fs::write(
                &source_file,
                generate_swift_with_stream_runtime(
                    config,
                    ci,
                    stream_runtime_emitter == Some(component_index),
                )?,
            )?;
        }

        if options.generate_headers {
            let header_file = options.out_dir.join(config.header_filename());
            fs::write(header_file, generate_header(config, ci)?)?;
        }
    }

    // Derive the default module_name/modulemap_filename from the source filename.
    let source_basename = loader.source_basename(&options.source);

    let module_name = options
        .module_name
        .unwrap_or_else(|| source_basename.to_string());
    let modulemap_filename = options
        .modulemap_filename
        .unwrap_or_else(|| format!("{source_basename}.modulemap"));

    if options.generate_modulemap {
        let mut header_filenames: Vec<_> = components
            .iter()
            .map(|Component { config, .. }| config.header_filename())
            .collect();
        header_filenames.sort();
        let modulemap_source = generate_modulemap(
            module_name,
            header_filenames,
            options.xcframework,
            options.link_frameworks,
        )?;
        let modulemap_path = options.out_dir.join(modulemap_filename);
        fs::write(modulemap_path, modulemap_source)?;
    }

    Ok(())
}

fn parse_config(ci: &ComponentInterface, root_toml: toml::Value) -> Result<Config> {
    let mut config: Config = match root_toml.get("bindings").and_then(|b| b.get("swift")) {
        Some(v) => v.clone().try_into()?,
        None => Default::default(),
    };
    config
        .module_name
        .get_or_insert_with(|| ci.namespace().into());
    Ok(config)
}

#[derive(Debug, Default)]
pub struct SwiftBindingsOptions {
    pub generate_swift_sources: bool,
    pub generate_headers: bool,
    pub generate_modulemap: bool,
    pub source: Utf8PathBuf,
    pub out_dir: Utf8PathBuf,
    pub xcframework: bool,
    pub module_name: Option<String>,
    pub modulemap_filename: Option<String>,
    pub metadata_no_deps: bool,
    pub link_frameworks: Vec<String>,
    pub config: Option<Utf8PathBuf>,
}

// A helper for renaming items.
fn apply_renames(components: &mut Vec<Component<Config>>) {
    // Remove excluded items, this happens before renaming
    for c in components.iter_mut() {
        apply_exclusions(&mut c.ci, &c.config.exclude);
    }

    let mut module_renames = HashMap::new();
    // Collect all rename configurations from all components, keyed by module_path
    for c in components.iter() {
        if !c.config.rename.is_empty() {
            let module_path = c.ci.crate_name().to_string();
            module_renames.insert(module_path, c.config.rename.clone());
        }
    }

    // Apply rename configurations to all components
    if !module_renames.is_empty() {
        for c in &mut *components {
            rename(&mut c.ci, &module_renames);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8PathBuf;
    use std::{collections::BTreeSet, process::Command};
    use uniffi_meta::{
        EnumMetadata, EnumShape, FnMetadata, Metadata, MetadataGroup, NamespaceMetadata, Type,
        VariantMetadata,
    };

    fn output_stream_component(
        module_path: &str,
        function_name: &str,
        error_name: &str,
    ) -> ComponentInterface {
        let mut items = BTreeSet::new();
        let stream_error_type = Type::Enum {
            module_path: module_path.to_owned(),
            name: error_name.to_owned(),
        };
        items.insert(Metadata::Enum(EnumMetadata {
            module_path: module_path.to_owned(),
            name: error_name.to_owned(),
            orig_name: None,
            rust_path: None,
            discr_type: None,
            shape: EnumShape::Error { flat: true },
            remote: false,
            variants: vec![VariantMetadata {
                name: "boom".to_owned(),
                orig_name: None,
                discr: None,
                fields: vec![],
                docstring: None,
            }],
            non_exhaustive: false,
            docstring: None,
        }));
        items.insert(Metadata::Func(FnMetadata {
            module_path: module_path.to_owned(),
            name: function_name.to_owned(),
            orig_name: None,
            is_async: false,
            inputs: vec![],
            return_type: Some(Type::Stream {
                item_type: Box::new(Type::UInt32),
                error_type: Box::new(stream_error_type),
                is_send: true,
            }),
            throws: None,
            checksum: None,
            docstring: None,
        }));
        let mut ci = ComponentInterface::from_metadata(MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: module_path.to_owned(),
                name: module_path.to_owned(),
            },
            namespace_docstring: None,
            items,
        })
        .unwrap();
        ci.derive_ffi_funcs().unwrap();
        ci
    }

    fn config_with_module_name(module_name: &str) -> Config {
        let mut config = Config::default();
        config.module_name = Some(module_name.to_owned());
        config
    }

    #[test]
    fn swift_stream_multi_component_generation_emits_shared_runtime_once_and_typechecks() {
        let tempdir = tempfile::tempdir().unwrap();
        let out_dir = Utf8PathBuf::from_path_buf(tempdir.path().to_path_buf()).unwrap();
        let mut components = vec![
            Component {
                ci: output_stream_component("first_component", "first_stream", "FirstStreamError"),
                config: config_with_module_name("FirstComponent"),
            },
            Component {
                ci: output_stream_component(
                    "second_component",
                    "second_stream",
                    "SecondStreamError",
                ),
                config: config_with_module_name("SecondComponent"),
            },
        ];

        write_component_bindings(&mut components, None, &out_dir, false).unwrap();

        let first_source = std::fs::read_to_string(out_dir.join("FirstComponent.swift")).unwrap();
        let second_source = std::fs::read_to_string(out_dir.join("SecondComponent.swift")).unwrap();
        let sources = [&first_source, &second_source];
        assert_eq!(
            sources
                .iter()
                .map(|source| source
                    .matches("public struct UniFfiStream<Element>: AsyncSequence")
                    .count())
                .sum::<usize>(),
            1,
        );
        assert_eq!(
            sources
                .iter()
                .map(|source| source
                    .matches("final class UniFfiStreamState<Element>")
                    .count())
                .sum::<usize>(),
            1,
        );
        assert!(first_source.contains("public func firstStream() -> UniFfiStream<UInt32>"));
        assert!(second_source.contains("public func secondStream() -> UniFfiStream<UInt32>"));
        assert!(first_source.contains("return UniFfiStream("));
        assert!(second_source.contains("return UniFfiStream("));
        assert!(!second_source.contains("public struct UniFfiStream<Element>: AsyncSequence"));

        let swiftc_version = Command::new("swiftc")
            .arg("--version")
            .output()
            .expect("swiftc is required for the multi-component stream typecheck");
        assert!(
            swiftc_version.status.success(),
            "swiftc --version failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&swiftc_version.stdout),
            String::from_utf8_lossy(&swiftc_version.stderr)
        );

        let modulemap_path = out_dir.join("combined.modulemap");
        let modulemap = [
            "FirstComponentFFI.modulemap",
            "SecondComponentFFI.modulemap",
        ]
        .into_iter()
        .map(|filename| std::fs::read_to_string(out_dir.join(filename)).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
        std::fs::write(&modulemap_path, modulemap).unwrap();

        let usage_path = out_dir.join("UseBothStreams.swift");
        std::fs::write(
            &usage_path,
            r#"
private func useBothStreamComponents() {
    let first: UniFfiStream<UInt32> = firstStream()
    let second: UniFfiStream<UInt32> = secondStream()
    _ = (first, second)
}
"#,
        )
        .unwrap();

        let output = Command::new("swiftc")
            .arg("-typecheck")
            .arg("-module-name")
            .arg("CombinedStreams")
            .arg("-swift-version")
            .arg("5")
            .arg("-I")
            .arg(&out_dir)
            .arg("-Xcc")
            .arg(format!("-fmodule-map-file={modulemap_path}"))
            .arg(out_dir.join("FirstComponent.swift"))
            .arg(out_dir.join("SecondComponent.swift"))
            .arg(&usage_path)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "swiftc multi-component typecheck failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
