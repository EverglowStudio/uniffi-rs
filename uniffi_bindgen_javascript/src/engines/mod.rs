//! Mechanical adapters for the three programmatic JavaScript engines.
//!
//! These modules intentionally consume only [`NormalizedPackage`].  The
//! frontend is the sole owner of UniFFI metadata traversal; adapters only
//! project the frozen Rust bridge plan into an engine-owned plan and render
//! the engine's private source.

mod rust;

pub(crate) use rust::wasm_source::{post_link as wasm_post_link, render as wasm_source};
pub(crate) use rust::{napi_source, ohos_source};
