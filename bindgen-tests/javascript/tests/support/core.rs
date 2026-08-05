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

/// Stable Cargo target roots shared by the layered JavaScript integration
/// suites.  Temporary fixture source trees remain isolated; only Cargo's
/// dependency and fingerprint cache is reused.  The caller chooses a target
/// kind (`native`, `wasm`, `cli`, ...), keeping host and wasm artifacts in
/// separate roots while allowing independent fixture packages to reuse the
/// expensive UniFFI dependency graph.
pub fn shared_cargo_target_dir(kind: &str) -> Utf8PathBuf {
    assert!(
        !kind.is_empty() && !kind.contains('/') && !kind.contains('\\'),
        "shared Cargo target kind must be a single path component: {kind:?}"
    );
    let root = workspace_root().join("target/javascript-tests");
    std::fs::create_dir_all(&root).expect("shared JavaScript Cargo target root should be writable");
    root.join(kind)
}

/// Cross-process lock for a shared JavaScript Cargo target root.
///
/// Cargo serializes writes to a target directory internally, but integration
/// tests also copy same-named cdylibs out of that directory after a build.
/// The advisory lock covers both the build and that copy, preventing
/// concurrent tests from observing an artifact produced by another fixture.
/// The operating system releases it automatically if a test process exits.
pub struct SharedCargoTargetLock {
    _file: std::fs::File,
}

pub fn shared_cargo_target_lock(kind: &str) -> SharedCargoTargetLock {
    let target_root = shared_cargo_target_dir(kind);
    let lock_dir = target_root
        .parent()
        .expect("shared Cargo target root should have a parent")
        .join(".locks");
    std::fs::create_dir_all(&lock_dir)
        .expect("shared JavaScript Cargo lock directory should exist");
    let path = lock_dir.join(format!("{kind}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .unwrap_or_else(|error| {
            panic!("failed to open shared JavaScript Cargo lock {path}: {error}")
        });
    fs2::FileExt::lock_exclusive(&file).unwrap_or_else(|error| {
        panic!(
            "failed to acquire shared JavaScript Cargo lock {}: {error}",
            path
        )
    });
    SharedCargoTargetLock { _file: file }
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
