/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Per-flavor backend emitters.
//!
//! Each flavor owns its ABI-specific adapter and Rust bridge emission.

pub mod napi;
pub mod wasm;

use anyhow::Result;
use camino::Utf8Path;
use uniffi_bindgen::Component;

use crate::{AbiFlavor, JsConfig};

#[derive(Clone, Debug, Default)]
pub struct FlavorEmitOptions {
    pub default_addon_path: Option<String>,
    /// One composite OHOS cdylib can serve many generated component
    /// adapters. When absent, retain the standalone component default.
    pub ohos_native_library_stem: Option<String>,
}

pub fn emit(
    dir: &Utf8Path,
    flavor: AbiFlavor,
    component: &Component<JsConfig>,
    options: &FlavorEmitOptions,
) -> Result<()> {
    match flavor {
        AbiFlavor::Wasm => wasm::emit(dir, component),
        AbiFlavor::Napi => napi::emit(dir, component, options.default_addon_path.as_deref()),
        AbiFlavor::Ohos => {
            napi::emit_ohos(dir, component, options.ohos_native_library_stem.as_deref())
        }
    }
}
