/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Canonical names for exports shared by multiple generated JavaScript
//! component bridges in one native host.
//!
//! Rust module wrapping protects internal helper identifiers, but it does not
//! namespace `#[napi]` or `#[wasm_bindgen]` exports.  Every native-facing
//! callable and type therefore uses this exact prefixing rule.  Adapters use
//! the same helper when they build their static dispatch maps; there is no
//! runtime short-name fallback.

use uniffi_bindgen::ComponentInterface;

pub fn native_export_name(ci: &ComponentInterface, key: &str) -> String {
    native_export_name_for_prefix(&ci.native_export_prefix(), key)
}

pub fn native_export_name_for_prefix(prefix: &str, key: &str) -> String {
    format!("{prefix}_{key}")
}

#[cfg(test)]
mod tests {
    use super::native_export_name_for_prefix;

    #[test]
    fn keeps_same_component_keys_disjoint() {
        assert_eq!(
            native_export_name_for_prefix("ffi_alpha_core", "ping"),
            "ffi_alpha_core_ping"
        );
        assert_eq!(
            native_export_name_for_prefix("ffi_beta_core", "ping"),
            "ffi_beta_core_ping"
        );
    }
}
