/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Locator for the bundled TypeScript runtime source that
//! `uniffi_bindgen_javascript` copies into generated trees.
//!
//! The actual runtime lives under `typescript/src/` next to this Rust
//! shell.

use camino::Utf8PathBuf;

/// Returns the absolute path to the bundled `typescript/src/` directory.
pub fn typescript_src_dir() -> Utf8PathBuf {
    Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("typescript/src")
}
