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

use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use fs_err as fs;
use uniffi_bindgen::{BindgenLoader, Component};

pub mod api_module;
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
    let mut components: Vec<Component<JsConfig>> =
        loader.load_components(cis, |_ci, _toml| Ok(JsConfig::default()))?;
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
pub struct JsConfig {}
