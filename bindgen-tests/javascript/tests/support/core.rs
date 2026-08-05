//! File-layout smoke test + minimal execution test for the JavaScript
//! bindgen scaffolding.
//!
//! The package-tree smoke check stays cheap and always-on: the deterministic
//! root exists and key ECMAScript/native entries are present. It guards
//! regressions in the scaffolding without requiring a native build.
//!
//! The execution-level check installs a pure-JS stub backend and imports the
//! generated `components/<namespace>/index.js` through Node. Native host
//! compilation belongs to the N-API/Wasm/Harmony E2E targets.

// Each integration-test crate includes this module independently and consumes
// a different subset of the shared generation helpers.
#![allow(dead_code)]

pub use camino::Utf8PathBuf;
pub use std::process::Command;
pub use uniffi_bindgen::{BindgenLoader, BindgenPaths, GlobalConfig};
pub use uniffi_bindgen_javascript::package::GeneratedPackage;
pub use uniffi_bindgen_javascript::{
    generate, generate_package, FlavorTarget, GenerateJsOptions, WasmPostLinkTarget,
};

pub fn workspace_root() -> Utf8PathBuf {
    let manifest = Utf8PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../..").canonicalize_utf8().unwrap()
}

pub fn generate_arithmetic(out_dir: &Utf8PathBuf) {
    let source = workspace_root().join("examples/arithmetic/src/arithmetic.udl");
    let manifest = workspace_root().join("examples/arithmetic/Cargo.toml");
    assert!(source.exists(), "fixture UDL missing: {source}");
    let loader = BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default());
    generate(
        &loader,
        GenerateJsOptions {
            source,
            out_dir: out_dir.clone(),
            package_root: out_dir.clone(),
            artifact_dir: None,
            config_override: None,
            crate_filter: None,
            metadata_no_deps: true,
            host_crates: uniffi_bindgen_javascript::HostCrateOptions {
                manifest_path: manifest,
                host_crates_dir: out_dir.join("native/hosts"),
                logical_host_crates_dir: None,
            },
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
