/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! First-class JavaScript/TypeScript bindings generator for uniffi-rs.
//!
//! This crate emits a single high-level TypeScript API plus one or more
//! flavor-specific backend adapters (wasm, napi) that satisfy a shared
//! low-level FFI contract. Electron preload + renderer facades are emitted
//! as a consumption form of the napi flavor.
//!
//! See `docs/manual/src/javascript/contract.md` for the stable contract
//! these emitters target.
//!
//! ```text
//! generated/
//!   common/    api.ts records.ts enums.ts errors.ts objects.ts callbacks.ts runtime.ts
//!   browser/   index.ts backend-wasm.ts
//!   node/      index.ts backend-napi.ts
//!   electron/  preload.cjs renderer.ts
//! ```

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use camino::Utf8PathBuf;
use fs_err as fs;
use uniffi_bindgen::{BindgenLoader, Component};

pub mod api_module;
pub mod dispatch_key;
pub mod electron;
pub mod enum_shape;
pub mod flavors;
pub mod host_crates;
pub mod name_map;

pub use host_crates::HostCrateOptions;

/// Which low-level ABI flavor a generated backend speaks.
///
/// The high-level TS API is identical across flavors; only the backend
/// adapter differs. `Electron` is not its own flavor — it's a consumption
/// form of `Napi` that additionally emits preload + renderer entries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AbiFlavor {
    Wasm,
    Napi,
}

/// Top-level options for the JavaScript target.
#[derive(Clone, Debug)]
pub struct GenerateJsOptions {
    /// Path to UDL file or compiled cdylib.
    pub source: Utf8PathBuf,
    /// Output directory; the generator creates `common/` plus one
    /// subdirectory per requested flavor (`browser/`, `node/`, `electron/`).
    pub out_dir: Utf8PathBuf,
    /// Optional uniffi.toml override.
    pub config_override: Option<Utf8PathBuf>,
    /// Limit generation to a single crate.
    pub crate_filter: Option<String>,
    /// Exclude transitive deps when running cargo metadata.
    pub metadata_no_deps: bool,
    /// Which flavors / consumption forms to emit.
    pub flavors: Vec<FlavorTarget>,
    /// Opt-in: also emit Rust host crates (`rust_modules/wasm`,
    /// `rust_modules/napi`) that wrap the per-component bridge files.
    /// When `None`, behaviour is identical to pre-host-crates invocations.
    pub host_crates: Option<HostCrateOptions>,
}

/// What to emit. `Electron` implies `Napi` plus preload+renderer files.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FlavorTarget {
    Wasm,
    Napi,
    Electron,
}

impl FlavorTarget {
    fn abi(self) -> AbiFlavor {
        match self {
            FlavorTarget::Wasm => AbiFlavor::Wasm,
            FlavorTarget::Napi | FlavorTarget::Electron => AbiFlavor::Napi,
        }
    }
}

/// Entry point invoked by the uniffi-bindgen CLI.
pub fn generate(loader: &BindgenLoader, options: GenerateJsOptions) -> Result<()> {
    if options.flavors.is_empty() {
        bail!("uniffi-bindgen javascript: at least one --flavor must be specified");
    }
    fs::create_dir_all(&options.out_dir)?;

    let metadata = loader.load_metadata(&options.source)?;
    if let Some(crate_filter) = &options.crate_filter {
        if !metadata.contains_key(crate_filter) {
            bail!("No UniFFI metadata found for crate {crate_filter}");
        }
    }
    let cis = loader.load_cis(metadata)?;
    let override_toml = load_override_toml(options.config_override.as_ref())?;
    let mut components: Vec<Component<JsConfig>> =
        loader.load_components(cis, |_ci, mut toml| {
            if let Some(override_toml) = &override_toml {
                merge_toml(&mut toml, override_toml.clone());
            }
            JsConfig::from_root_toml(toml)
        })?;
    for c in components.iter_mut() {
        c.ci.derive_ffi_funcs()?;
    }

    let mut emitted_crate_names: Vec<String> = Vec::new();
    for component in &components {
        if let Some(crate_filter) = &options.crate_filter {
            if component.ci.crate_name() != crate_filter {
                continue;
            }
        }
        emit_component(component, &options)?;
        emitted_crate_names.push(component.ci.crate_name().to_string());
    }

    if let Some(host_opts) = &options.host_crates {
        let meta = host_crates::load_metadata(&host_opts.manifest_path)?;
        let want_wasm = options.flavors.iter().any(|f| f.abi() == AbiFlavor::Wasm);
        // electron reuses the napi host crate — no separate electron crate.
        let want_napi = options.flavors.iter().any(|f| f.abi() == AbiFlavor::Napi);
        host_crates::emit(
            host_opts,
            &options.out_dir,
            &emitted_crate_names,
            &meta,
            want_wasm,
            want_napi,
        )?;
    }
    Ok(())
}

fn emit_component(component: &Component<JsConfig>, options: &GenerateJsOptions) -> Result<()> {
    let common_dir = options.out_dir.join("common");
    fs::create_dir_all(&common_dir)?;
    api_module::emit(&common_dir, component)?;

    for target in &options.flavors {
        let subdir = match target {
            FlavorTarget::Wasm => "browser",
            FlavorTarget::Napi => "node",
            FlavorTarget::Electron => "electron",
        };
        let dir = options.out_dir.join(subdir);
        fs::create_dir_all(&dir)?;
        flavors::emit(&dir, target.abi(), component)?;
        if matches!(target, FlavorTarget::Electron) {
            electron::emit(&dir, component)?;
        }
    }
    Ok(())
}

/// Per-component config loaded from `uniffi.toml`.
///
/// Empty for now; add fields here only when they are part of the
/// supported long-term JavaScript target configuration surface.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[serde(default)]
pub struct JsConfig {
    #[serde(alias = "custom_types", alias = "customTypes")]
    pub custom_types: BTreeMap<String, CustomTypeConfig>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CustomTypeConfig {
    pub imports: Vec<String>,
    #[serde(alias = "type_name")]
    pub type_name: Option<String>,
    #[serde(alias = "lift")]
    pub into_custom: String,
    #[serde(alias = "lower")]
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
