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

pub fn emit(dir: &Utf8Path, flavor: AbiFlavor, component: &Component<JsConfig>) -> Result<()> {
    match flavor {
        AbiFlavor::Wasm => wasm::emit(dir, component),
        AbiFlavor::Napi => napi::emit(dir, component),
        AbiFlavor::Ohos => napi::emit_ohos(dir, component),
    }
}
