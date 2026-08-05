/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! First-class JavaScript/TypeScript bindings generator for uniffi-rs.
//!
//! This crate emits one high-level TypeScript API per generated component plus
//! one shared runtime, with flavor-specific backend adapters (wasm, N-API,
//! and OHOS) that satisfy a shared low-level FFI contract. Electron preload +
//! renderer facades are emitted as a consumption form of the N-API flavor.
//!
//! See `docs/manual/src/javascript/contract.md` for the stable contract
//! these emitters target.
//!
//! ```text
//! generated/
//!   shared/uniffi_runtime.js
//!   components/<namespace>/common/
//!   components/<namespace>/{browser,node,electron,harmony}/
//!   {browser,node,electron,harmony}/index.js
//! ```

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use fs_err as fs;
use uniffi_bindgen::BindgenLoader;

mod engines;
pub mod frontend;
pub mod host_crates;
pub mod package;

pub use host_crates::HostCrateOptions;

/// Top-level options for the JavaScript target.
#[derive(Clone, Debug)]
pub struct GenerateJsOptions {
    /// Path to a UDL file or compiled cdylib accepted by the UniFFI loader.
    pub source: Utf8PathBuf,
    /// Output directory. Component sources live below
    /// `components/<namespace>/`; each requested flavor also gets one stable,
    /// namespace-only root entrypoint.
    pub out_dir: Utf8PathBuf,
    /// Root of the complete generated package. `out_dir` must be this root
    /// or a descendant; generated source/native files are published below
    /// the corresponding relative source prefix while host crates remain at
    /// their package-root paths.
    pub package_root: Utf8PathBuf,
    /// Optional directory used by artifact-building commands for non-source
    /// outputs such as `.node` addons. When set, generated backend entrypoints
    /// use this location as their default load path.
    pub artifact_dir: Option<Utf8PathBuf>,
    /// Optional uniffi.toml override.
    pub config_override: Option<Utf8PathBuf>,
    /// Limit generation to a single crate.
    pub crate_filter: Option<String>,
    /// Exclude transitive deps when running cargo metadata.
    pub metadata_no_deps: bool,
    /// Which flavors / consumption forms to emit.
    pub flavors: Vec<FlavorTarget>,
    /// Host-crate plan for the atomic package. Generation publishes the
    /// selected `native/hosts/{wasm,napi,ohos}` projects together with its
    /// source and native bridge files. Source-only generation has no public
    /// option because a generated package is always an atomic host package.
    pub host_crates: HostCrateOptions,
}

/// What to emit. `Electron` implies `Napi` plus preload+renderer files.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FlavorTarget {
    Wasm,
    Napi,
    Electron,
    Harmony,
}

/// In-process wasm-bindgen loader target used by the generated package
/// publisher.  This is deliberately separate from the command-line enum so
/// package publication never needs to invoke or discover an external CLI.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum WasmPostLinkTarget {
    Web,
    Bundler,
    Node,
}

/// Entry point invoked by the uniffi-bindgen CLI.
pub fn generate(loader: &BindgenLoader, options: GenerateJsOptions) -> Result<()> {
    generate_package(loader, options).map(|_| ())
}

/// Prepare and publish one complete JavaScript package, returning the exact
/// frozen package used for publication.  Build/post-link callers can borrow
/// its normalized engine plan without reparsing metadata or generated files.
pub fn generate_package(
    loader: &BindgenLoader,
    options: GenerateJsOptions,
) -> Result<package::GeneratedPackage> {
    if options.flavors.is_empty() {
        bail!("uniffi-bindgen javascript: at least one --flavor must be specified");
    }

    let metadata = loader.load_metadata(&options.source)?;
    if let Some(crate_filter) = &options.crate_filter {
        if !metadata.contains_key(crate_filter) {
            bail!("No UniFFI metadata found for crate {crate_filter}");
        }
    }
    let mut cis = loader.load_cis(&options.source, metadata)?;
    if let Some(crate_filter) = &options.crate_filter {
        cis.retain(|ci| ci.crate_name() == crate_filter);
    }
    let override_toml = load_override_toml(options.config_override.as_ref())?;
    let mut components = loader.load_components(cis, |_ci, mut toml| {
        if let Some(override_toml) = &override_toml {
            merge_toml(&mut toml, override_toml.clone());
        }
        JsConfig::from_root_toml(toml)
    })?;
    let build_targets = options
        .flavors
        .iter()
        .map(|flavor| match flavor {
            FlavorTarget::Napi | FlavorTarget::Electron => uniffi_js_abi::PublicTarget::NodeNapi,
            FlavorTarget::Wasm => uniffi_js_abi::PublicTarget::BrowserWasm,
            FlavorTarget::Harmony => uniffi_js_abi::PublicTarget::OhosNapi,
        })
        .collect::<std::collections::BTreeSet<_>>();
    // The package boundary owns the entire invocation.  The frontend is the
    // sole place that filters/derives/sorts ComponentInterfaces and performs
    // normalization; all later stages consume only owned values.
    let normalized = frontend::prepare_components(
        &mut components,
        options.crate_filter.as_deref(),
        build_targets,
    )?;
    let package = package::GeneratedPackage::prepare(&normalized, &options)?;
    package.write_to(&options.package_root)?;
    Ok(package)
}

/// Per-component config loaded from `uniffi.toml`.
///
/// Empty for now; add fields here only when they are part of the
/// supported long-term JavaScript target configuration surface.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JsConfig {
    #[serde(default)]
    pub custom_types: BTreeMap<String, CustomTypeConfig>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CustomTypeConfig {
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub type_name: Option<String>,
    #[serde(default)]
    pub into_custom: String,
    #[serde(default)]
    pub from_custom: String,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RootConfig {
    bindings: BindingsConfig,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
struct BindingsConfig {
    #[serde(default)]
    javascript: JsConfig,
}

impl JsConfig {
    fn from_root_toml(toml: toml::Value) -> Result<Self> {
        let root: RootConfig = toml.try_into()?;
        Ok(root.bindings.javascript)
    }

    pub fn custom_type(&self, name: &str) -> Option<&CustomTypeConfig> {
        self.custom_types.get(name)
    }
}

impl CustomTypeConfig {
    pub fn public_type<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.type_name.as_deref().unwrap_or(fallback)
    }

    pub fn into_custom_expr(&self, builtin_expr: &str) -> String {
        if self.into_custom.is_empty() {
            builtin_expr.to_string()
        } else {
            self.into_custom.replace("{}", builtin_expr)
        }
    }

    pub fn from_custom_expr(&self, custom_expr: &str) -> String {
        if self.from_custom.is_empty() {
            custom_expr.to_string()
        } else {
            self.from_custom.replace("{}", custom_expr)
        }
    }
}

fn load_override_toml(path: Option<&Utf8PathBuf>) -> Result<Option<toml::Value>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text =
        fs::read_to_string(path).with_context(|| format!("reading config override {path}"))?;
    let value = toml::from_str(&text).with_context(|| format!("parsing config override {path}"))?;
    Ok(Some(value))
}

fn merge_toml(into: &mut toml::Value, from: toml::Value) {
    match (into, from) {
        (toml::Value::Table(into), toml::Value::Table(from)) => {
            for (key, value) in from {
                match into.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        into.insert(key, value);
                    }
                }
            }
        }
        (into, from) => *into = from,
    }
}

#[cfg(test)]
mod config_tests {
    use super::JsConfig;

    fn parse(input: &str) -> anyhow::Result<JsConfig> {
        JsConfig::from_root_toml(toml::from_str(input)?)
    }

    #[test]
    fn javascript_config_accepts_only_the_canonical_custom_type_shape() {
        let config = parse(
            r#"
                [bindings.javascript.customTypes.MyType]
                typeName = "MyType"
                imports = []
                intoCustom = "{}"
                fromCustom = "{}"
            "#,
        )
        .unwrap();

        let custom = config.custom_type("MyType").unwrap();
        assert_eq!(custom.public_type("Fallback"), "MyType");
        assert_eq!(custom.into_custom_expr("value"), "value");
        assert_eq!(custom.from_custom_expr("value"), "value");
    }

    #[test]
    fn javascript_config_rejects_legacy_aliases_and_unknown_subtree_fields() {
        for (label, input) in [
            ("custom_types", "[bindings.javascript]\ncustom_types = {}\n"),
            (
                "type_name",
                "[bindings.javascript.customTypes.MyType]\ntype_name = \"MyType\"\n",
            ),
            (
                "into_custom",
                "[bindings.javascript.customTypes.MyType]\ninto_custom = \"{}\"\n",
            ),
            (
                "from_custom",
                "[bindings.javascript.customTypes.MyType]\nfrom_custom = \"{}\"\n",
            ),
            (
                "lift",
                "[bindings.javascript.customTypes.MyType]\nlift = \"{}\"\n",
            ),
            (
                "lower",
                "[bindings.javascript.customTypes.MyType]\nlower = \"{}\"\n",
            ),
            (
                "unknown javascript field",
                "[bindings.javascript]\nunknown = true\n",
            ),
            (
                "unknown custom type field",
                "[bindings.javascript.customTypes.MyType]\nunknown = true\n",
            ),
        ] {
            let error = parse(input).unwrap_err().to_string();
            assert!(
                error.contains(label.split(' ').next().unwrap()),
                "{label} must be rejected with a useful parse error: {error}"
            );
        }
    }

    #[test]
    fn javascript_config_remains_optional_and_does_not_reject_other_bindings() {
        assert!(parse("[bindings.swift]\nmoduleName = \"Example\"\n")
            .unwrap()
            .custom_types
            .is_empty());
        assert!(parse("[bindings.kotlin]\npackageName = \"example\"\n")
            .unwrap()
            .custom_types
            .is_empty());
    }
}
