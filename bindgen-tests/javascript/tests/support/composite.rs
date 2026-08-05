//! A real two-component UniFFI megazord fixture shared by JavaScript tests.
//!
//! The fixture intentionally writes a virtual workspace whose only binary
//! artifact is `composite-core`'s cdylib.  `alpha-core` and `beta-core` are
//! independent proc-macro UniFFI components, so loading the composite library
//! exercises the same library-mode metadata path as a downstream megazord.

#![allow(dead_code)]

use super::core::workspace_root;
use camino::{Utf8Path, Utf8PathBuf};
use std::process::Command;
use uniffi_bindgen::{BindgenLoader, BindgenPaths, GlobalConfig};
use uniffi_bindgen_javascript::package::GeneratedPackage;
use uniffi_bindgen_javascript::{
    generate_package, FlavorTarget, GenerateJsOptions, HostCrateOptions,
};

/// The stable descriptor used by all composite fixture assertions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompositeComponent {
    /// Cargo package and directory name, retained in the fixture's workspace
    /// and managed-artifact assertions.
    pub package_name: &'static str,
    /// Rust library/UniFFI metadata crate name.  This is the identity emitted
    /// by proc-macro metadata and therefore the value host planning receives.
    pub crate_name: &'static str,
    /// Intentionally distinct root source-dependency alias. Host planning
    /// must resolve this alias to a package ID, then render `crate_name` for
    /// generated bridge imports.
    pub source_alias: &'static str,
    pub namespace: &'static str,
    pub bridge_filename: &'static str,
}

/// The alpha component deliberately shares its public function/type names
/// with beta while returning a different sentinel.
pub const ALPHA_COMPONENT: CompositeComponent = CompositeComponent {
    package_name: "alpha-core",
    crate_name: "alpha_core",
    source_alias: "alpha_component_source",
    namespace: "alpha",
    bridge_filename: "alpha_core.rs",
};

/// The beta component deliberately shares its public function/type names
/// with alpha while also accepting alpha's uniquely named external record.
pub const BETA_COMPONENT: CompositeComponent = CompositeComponent {
    package_name: "beta-core",
    crate_name: "beta_core",
    source_alias: "beta_component_source",
    namespace: "beta",
    bridge_filename: "beta_core.rs",
};

/// Canonical package order.  Generation must produce this order regardless
/// of the dependency/re-export order used to write the workspace.
pub const CANONICAL_COMPONENTS: [CompositeComponent; 2] = [ALPHA_COMPONENT, BETA_COMPONENT];

/// Controls only the textual dependency and re-export order in the fixture.
/// Both variants retain the same package names, paths, namespaces, and
/// metadata so callers can compare deterministic generated output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompositeOrder {
    Canonical,
    Reverse,
}

impl CompositeOrder {
    fn components(self) -> [CompositeComponent; 2] {
        match self {
            Self::Canonical => CANONICAL_COMPONENTS,
            Self::Reverse => [BETA_COMPONENT, ALPHA_COMPONENT],
        }
    }
}

/// Paths for one isolated alpha/beta/composite fixture workspace.
///
/// `manifest_path()` deliberately names `composite-core/Cargo.toml`, never the
/// virtual workspace root.  This is the manifest passed to host generation so
/// Cargo metadata resolves the real megazord package and both direct deps.
#[derive(Debug)]
pub struct CompositeFixture {
    root: Utf8PathBuf,
    workspace_manifest: Utf8PathBuf,
    alpha_manifest: Utf8PathBuf,
    beta_manifest: Utf8PathBuf,
    composite_manifest: Utf8PathBuf,
    target_dir: Utf8PathBuf,
    library_path: Utf8PathBuf,
    order: CompositeOrder,
}

impl CompositeFixture {
    /// Write the fixture with alpha then beta dependency/re-export order.
    pub fn write(root: &std::path::Path) -> Self {
        Self::write_with_order(root, CompositeOrder::Canonical)
    }

    /// Write the fixture with beta then alpha dependency/re-export order.
    ///
    /// Call this in a separate temporary directory from [`Self::write`], then
    /// compare generated source/host artifacts rather than relying on binary
    /// byte identity.
    pub fn write_reversed(root: &std::path::Path) -> Self {
        Self::write_with_order(root, CompositeOrder::Reverse)
    }

    /// Write the complete fixture in `root` using the requested textual order.
    pub fn write_with_order(root: &std::path::Path, order: CompositeOrder) -> Self {
        let root = Utf8PathBuf::from_path_buf(root.to_path_buf())
            .expect("composite fixture root must be valid UTF-8");
        let alpha_dir = root.join("alpha-core");
        let beta_dir = root.join("beta-core");
        let composite_dir = root.join("composite-core");
        for dir in [&alpha_dir, &beta_dir, &composite_dir] {
            std::fs::create_dir_all(dir.join("src"))
                .expect("creating composite fixture source directory should succeed");
        }

        std::fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["alpha-core", "beta-core", "composite-core"]
resolver = "3"
"#,
        )
        .expect("writing composite fixture workspace manifest should succeed");

        let uniffi = workspace_root().join("uniffi");
        write_component_manifest(&alpha_dir, ALPHA_COMPONENT, &uniffi, &[]);
        write_component_manifest(
            &beta_dir,
            BETA_COMPONENT,
            &uniffi,
            &[(
                "alpha_component_for_beta",
                ALPHA_COMPONENT.package_name,
                "../alpha-core",
            )],
        );
        write_alpha_source(&alpha_dir.join("src/lib.rs"));
        write_beta_source(&beta_dir.join("src/lib.rs"));
        write_composite_manifest(&composite_dir, &uniffi, order);
        write_composite_source(&composite_dir.join("src/lib.rs"), order);

        let workspace_manifest = root.join("Cargo.toml");
        let composite_manifest = composite_dir.join("Cargo.toml");
        let target_dir = super::core::shared_cargo_target_dir("native");
        let library_path = root
            .join("cargo-target-composite")
            .join("debug")
            .join(cdylib_filename("composite_core"));
        Self {
            root,
            workspace_manifest,
            alpha_manifest: alpha_dir.join("Cargo.toml"),
            beta_manifest: beta_dir.join("Cargo.toml"),
            composite_manifest,
            target_dir,
            library_path,
            order,
        }
    }

    pub fn root(&self) -> &Utf8Path {
        &self.root
    }

    pub fn workspace_manifest_path(&self) -> &Utf8Path {
        &self.workspace_manifest
    }

    pub fn alpha_manifest_path(&self) -> &Utf8Path {
        &self.alpha_manifest
    }

    pub fn beta_manifest_path(&self) -> &Utf8Path {
        &self.beta_manifest
    }

    /// The non-virtual root package manifest required by `HostCrateOptions`.
    pub fn manifest_path(&self) -> &Utf8Path {
        &self.composite_manifest
    }

    pub fn target_dir(&self) -> &Utf8Path {
        &self.target_dir
    }

    pub fn library_path(&self) -> &Utf8Path {
        &self.library_path
    }

    pub fn order(&self) -> CompositeOrder {
        self.order
    }

    pub fn canonical_components(&self) -> &'static [CompositeComponent; 2] {
        &CANONICAL_COMPONENTS
    }

    pub fn component_dir(&self, component: CompositeComponent) -> Utf8PathBuf {
        self.root.join(component.package_name)
    }

    pub fn generated_component_dir(
        &self,
        out_dir: &Utf8Path,
        component: CompositeComponent,
    ) -> Utf8PathBuf {
        out_dir.join("components").join(component.namespace)
    }

    pub fn generated_bridge_path(
        &self,
        out_dir: &Utf8Path,
        component: CompositeComponent,
        flavor: &str,
    ) -> Utf8PathBuf {
        self.generated_component_dir(out_dir, component)
            .join(flavor)
            .join(component.bridge_filename)
    }

    pub fn host_manifest_path(&self, host_dir: &Utf8Path, flavor: &str) -> Utf8PathBuf {
        host_dir.join(flavor).join("Cargo.toml")
    }

    /// Build the one composite cdylib in the shared dev target and copy it to
    /// this fixture's private root.  The advisory target lock covers the
    /// Cargo build and copy, so canonical/reverse fixtures cannot observe a
    /// same-name artifact from one another.
    pub fn build_cdylib(&self) -> &Utf8Path {
        let _target_lock = super::core::shared_cargo_target_lock("native");
        let output = Command::new("cargo")
            .args(["build", "--features", "host-gate", "--manifest-path"])
            .arg(self.composite_manifest.as_std_path())
            .env("CARGO_TARGET_DIR", self.target_dir.as_std_path())
            .env_remove("RUSTFLAGS")
            .output()
            .expect("failed to invoke cargo for composite fixture");
        if !output.status.success() {
            panic!(
                "composite fixture cdylib build failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
        let built_library = self
            .target_dir
            .join("debug")
            .join(cdylib_filename("composite_core"));
        assert!(
            built_library.exists(),
            "expected composite fixture cdylib at {}",
            built_library
        );
        std::fs::create_dir_all(self.library_path.parent().unwrap())
            .expect("composite fixture private target directory should exist");
        std::fs::copy(&built_library, &self.library_path)
            .expect("copying composite fixture cdylib should succeed");
        assert!(
            self.library_path.exists(),
            "expected isolated composite fixture cdylib at {}",
            self.library_path
        );
        &self.library_path
    }

    /// Create the loader used by every real-library generation assertion.
    pub fn bindgen_loader(&self) -> BindgenLoader {
        BindgenLoader::new(BindgenPaths::default(), GlobalConfig::default())
    }

    /// Read component metadata from the already-built composite cdylib.
    pub fn loaded_component_crates(&self) -> Vec<String> {
        assert!(
            self.library_path.exists(),
            "build_cdylib() must run before loading {}",
            self.library_path
        );
        let loader = self.bindgen_loader();
        let mut crates = loader
            .load_metadata(&self.library_path)
            .expect("BindgenLoader should load composite fixture metadata")
            .into_keys()
            .collect::<Vec<_>>();
        crates.sort();
        crates
    }

    /// Assert that a real dynamic library carried exactly alpha and beta
    /// metadata groups before generated output is considered valid.
    pub fn assert_loads_both_components(&self) {
        assert_eq!(
            self.loaded_component_crates(),
            vec![
                ALPHA_COMPONENT.crate_name.to_string(),
                BETA_COMPONENT.crate_name.to_string(),
            ],
            "composite cdylib must expose exactly the alpha/beta metadata groups",
        );
    }

    /// Generate a source/host tree directly from the real composite cdylib.
    pub fn generate(
        &self,
        out_dir: &Utf8PathBuf,
        host_crates_dir: Option<Utf8PathBuf>,
        flavors: Vec<FlavorTarget>,
    ) -> GeneratedPackage {
        self.generate_with_artifact_dir(out_dir, None, host_crates_dir, flavors)
    }

    /// Variant of [`Self::generate`] that gives adapters a package-level
    /// artifact directory.  It still loads all component metadata from the
    /// same composite library and passes the real composite package manifest
    /// to the host planner.
    pub fn generate_with_artifact_dir(
        &self,
        out_dir: &Utf8PathBuf,
        artifact_dir: Option<Utf8PathBuf>,
        host_crates_dir: Option<Utf8PathBuf>,
        flavors: Vec<FlavorTarget>,
    ) -> GeneratedPackage {
        self.assert_loads_both_components();
        let loader = self.bindgen_loader();
        let host_crates_dir = host_crates_dir.unwrap_or_else(|| out_dir.join("native/hosts"));
        generate_package(
            &loader,
            GenerateJsOptions {
                source: self.library_path.clone(),
                out_dir: out_dir.clone(),
                package_root: out_dir.clone(),
                artifact_dir,
                config_override: None,
                crate_filter: None,
                // The alpha-owned record used by beta is resolved through the
                // library's complete metadata set, not a no-dependencies UDL
                // shortcut.
                metadata_no_deps: false,
                host_crates: HostCrateOptions {
                    manifest_path: self.composite_manifest.clone(),
                    host_crates_dir,
                    logical_host_crates_dir: None,
                },
                flavors,
            },
        )
        .expect("JavaScript generation should succeed for the composite cdylib")
    }
}

fn write_component_manifest(
    crate_dir: &Utf8Path,
    component: CompositeComponent,
    uniffi: &Utf8Path,
    extra_dependencies: &[(&str, &str, &str)],
) {
    let mut dependencies = format!("uniffi = {{ path = {:?} }}\n", uniffi.as_str());
    for (alias, package, path) in extra_dependencies {
        dependencies.push_str(&format!(
            "{alias} = {{ package = {package:?}, path = {path:?} }}\n"
        ));
    }
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "{}"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
name = "{}"
crate-type = ["lib"]

[dependencies]
{}"#,
            component.package_name, component.crate_name, dependencies,
        ),
    )
    .expect("writing composite fixture component manifest should succeed");
}

fn write_composite_manifest(composite_dir: &Utf8Path, uniffi: &Utf8Path, order: CompositeOrder) {
    let mut dependencies = format!("uniffi = {{ path = {:?} }}\n", uniffi.as_str());
    let mut host_gate = Vec::new();
    for component in order.components() {
        dependencies.push_str(&format!(
            "{} = {{ package = {:?}, path = {:?}, optional = true }}\n",
            component.source_alias,
            component.package_name,
            format!("../{}", component.package_name),
        ));
        host_gate.push(format!("dep:{}", component.source_alias));
    }
    std::fs::write(
        composite_dir.join("Cargo.toml"),
        format!(
            r#"[package]
name = "composite-core"
version = "0.0.0"
edition = "2021"
publish = false

[lib]
name = "composite_core"
crate-type = ["lib", "cdylib"]

[features]
host-gate = [{}]

[dependencies]
{}"#,
            host_gate
                .iter()
                .map(|feature| format!("{feature:?}"))
                .collect::<Vec<_>>()
                .join(", "),
            dependencies,
        ),
    )
    .expect("writing composite fixture megazord manifest should succeed");
}

fn write_alpha_source(path: &Utf8Path) {
    std::fs::write(
        path,
        r#"
#[derive(Clone, Debug, uniffi::Record)]
pub struct SharedRecord {
    pub sentinel: String,
}

// Keep this owner-chain type orthogonal to SharedRecord.  The latter is
// deliberately duplicated by alpha and beta to validate namespace isolation;
// cross-component ownership needs a unique name under the TypeUniverse's
// established name-keyed boundary.
#[derive(Clone, Debug, uniffi::Record)]
pub struct AlphaOwned {
    pub sentinel: String,
}

#[derive(uniffi::Object)]
pub struct SharedObject {
    sentinel: String,
}

#[uniffi::export]
pub fn ping() -> String {
    "alpha-ping".to_string()
}

#[uniffi::export]
pub fn make_record() -> SharedRecord {
    SharedRecord {
        sentinel: "alpha-record".to_string(),
    }
}

#[uniffi::export]
pub fn echo_record(value: SharedRecord) -> SharedRecord {
    value
}

#[uniffi::export]
pub fn make_alpha_owned() -> AlphaOwned {
    AlphaOwned {
        sentinel: "alpha-owned".to_string(),
    }
}

#[uniffi::export]
impl SharedObject {
    #[uniffi::constructor]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            sentinel: "alpha-object".to_string(),
        })
    }

    pub fn sentinel(&self) -> String {
        self.sentinel.clone()
    }
}

uniffi::setup_scaffolding!("alpha");
"#,
    )
    .expect("writing alpha component source should succeed");
}

fn write_beta_source(path: &Utf8Path) {
    std::fs::write(
        path,
        r#"
use alpha_component_for_beta::AlphaOwned;

#[derive(Clone, Debug, uniffi::Record)]
pub struct SharedRecord {
    pub sentinel: String,
}

#[derive(uniffi::Object)]
pub struct SharedObject {
    sentinel: String,
}

#[uniffi::export]
pub fn ping() -> String {
    "beta-ping".to_string()
}

#[uniffi::export]
pub fn make_record() -> SharedRecord {
    SharedRecord {
        sentinel: "beta-record".to_string(),
    }
}

#[uniffi::export]
pub fn echo_record(value: SharedRecord) -> SharedRecord {
    value
}

// This must remain an alpha-owned external type in beta's generated bindings.
#[uniffi::export]
pub fn roundtrip_alpha(value: AlphaOwned) -> AlphaOwned {
    value
}

#[uniffi::export]
impl SharedObject {
    #[uniffi::constructor]
    pub fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            sentinel: "beta-object".to_string(),
        })
    }

    pub fn sentinel(&self) -> String {
        self.sentinel.clone()
    }
}

uniffi::setup_scaffolding!("beta");
"#,
    )
    .expect("writing beta component source should succeed");
}

fn write_composite_source(path: &Utf8Path, order: CompositeOrder) {
    let mut reexports = String::new();
    for component in order.components() {
        reexports.push_str(&format!(
            "#[cfg(feature = \"host-gate\")]\n{}::uniffi_reexport_scaffolding!();\n",
            component.source_alias
        ));
    }
    std::fs::write(
        path,
        format!(
            "// Force both component object files, metadata sections, and native scaffolding into one cdylib.\n{reexports}"
        ),
    )
    .expect("writing composite megazord source should succeed");
}

fn cdylib_filename(lib_name: &str) -> String {
    let ext = std::env::consts::DLL_EXTENSION;
    if cfg!(target_os = "windows") {
        format!("{lib_name}.{ext}")
    } else {
        format!("lib{lib_name}.{ext}")
    }
}
