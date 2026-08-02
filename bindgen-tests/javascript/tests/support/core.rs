//! File-layout smoke test + minimal execution test for the JavaScript
//! bindgen scaffolding.
//!
//! `emits_real_tree_for_all_flavors` stays as the cheap, always-on check:
//! file tree exists, key strings are present. It guards regressions in
//! the scaffolding without requiring a node runtime.
//!
//! `runs_common_api_under_node` is the execution-level check for the
//! high-level common API. It installs a pure-JS stub backend, imports
//! the generated `components/<namespace>/common/api.ts` via Node's
//! `--experimental-strip-types`,
//! and exercises free functions, object lifecycle, error marshalling,
//! and the numeric conversion path. Requires Node >= 22.6; older/missing
//! nodes cause the test to be skipped with an eprintln rather than fail.

// Each integration-test crate includes this module independently and consumes
// a different subset of the shared generation helpers.
#![allow(dead_code)]

pub use camino::Utf8PathBuf;
pub use std::process::Command;
pub use uniffi_bindgen::{BindgenLoader, BindgenPaths, GlobalConfig};
pub use uniffi_bindgen_javascript::{generate, FlavorTarget, GenerateJsOptions};

pub fn workspace_root() -> Utf8PathBuf {
    let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../..").canonicalize_utf8().unwrap()
}

pub fn generate_arithmetic(out_dir: &Utf8PathBuf) {
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    assert!(source.exists(), "fixture UDL missing: {source}");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: None,
            flavors: vec![
                FlavorTarget::Wasm,
                FlavorTarget::Napi,
                FlavorTarget::Electron,
                FlavorTarget::Harmony,
            ],
        },
    )
    .expect("generator should succeed");
}
