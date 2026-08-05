/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! In-memory host-crate planning for a generated JavaScript package.
//!
//! A host crate is another file in the package, not a second generation
//! pass.  This module consumes the normalized frontend plan and Cargo's
//! dependency graph, then returns ordinary package files.  It deliberately
//! never opens a generated facade, scans an output directory, or discovers a
//! component by parsing generated source.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{CargoOpt, DependencyKind, MetadataCommand, Package, PackageId, TargetKind};
use uniffi_js_abi::{ComponentId, PublicTarget};
use uniffi_js_engine_schema::{RustCallTarget, RustPath};

use crate::frontend::NormalizedPackage;

/// Names and native prefix for one component in the normalized package.
/// These values come from the canonical Rust paths; they are never recovered
/// from generated files.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct SelectedHostComponent {
    pub(crate) crate_name: String,
    pub(crate) namespace: String,
    pub(crate) native_export_prefix: String,
}

impl SelectedHostComponent {
    pub(crate) fn describe(&self) -> String {
        format!(
            "component `{}` (namespace `{}`, native export prefix `{}`)",
            self.crate_name, self.namespace, self.native_export_prefix
        )
    }
}

#[derive(Clone, Debug)]
struct PlannedPackageDependency {
    dependency_key: String,
    package_id: PackageId,
    package_name: String,
    package_dir: Utf8PathBuf,
}

#[derive(Clone, Debug)]
struct PlannedComponentDependency {
    package: PlannedPackageDependency,
}

/// Read-only dependency mapping for one generated package.
#[derive(Clone, Debug)]
pub(crate) struct HostCratePlan {
    meta: CoreCrateMetadata,
    root_dependency: PlannedPackageDependency,
    components: Vec<PlannedComponentDependency>,
    /// Package-relative host root.  Keeping this on the plan means the
    /// package renderer and later build legs use exactly the same manifest
    /// paths instead of re-deriving them from generated directory names.
    host_root: Utf8PathBuf,
    flavors: BTreeMap<PublicTarget, HostFlavorPlan>,
}

impl HostCratePlan {
    pub(crate) fn flavor(&self, target: PublicTarget) -> Option<&HostFlavorPlan> {
        self.flavors.get(&target)
    }

    pub(crate) fn core_dependency_key(&self) -> &str {
        &self.root_dependency.dependency_key
    }
}

/// Deterministic build and publication paths for one generated host flavor.
/// The fields are derived from the same `HostCratePlan` that rendered the
/// host source; no consumer may discover them by scanning a directory.
#[derive(Clone, Debug)]
pub(crate) struct HostFlavorPlan {
    pub(crate) crate_version: String,
    pub(crate) lib_target: String,
    pub(crate) crate_root: Utf8PathBuf,
    /// Final native payload path, relative to the package root.
    pub(crate) native_artifact: Utf8PathBuf,
    /// For Wasm, the generated wasm-bindgen loader next to the native
    /// payload.  Other engines do not have a second loader artifact.
    pub(crate) loader_artifact: Option<Utf8PathBuf>,
}

#[derive(Clone, Debug)]
pub(crate) struct RenderedHostPackage {
    pub(crate) plan: HostCratePlan,
    pub(crate) files: Vec<HostSourceFile>,
}

/// Caller-supplied metadata and output location for host files.
#[derive(Clone, Debug)]
pub struct HostCrateOptions {
    pub manifest_path: Utf8PathBuf,
    pub host_crates_dir: Utf8PathBuf,
    /// In-memory publication path used only while rendering staged Cargo
    /// manifests. It is never serialized into generated package metadata.
    pub logical_host_crates_dir: Option<Utf8PathBuf>,
}

#[derive(Clone, Debug)]
pub struct CoreCrateMetadata {
    pub package_name: String,
    pub package_version: String,
    pub description: Option<String>,
    pub authors: Vec<String>,
    pub license: Option<String>,
    pub crate_dir: Utf8PathBuf,
    pub uniffi_dep: Option<UniffiDependency>,
}

#[derive(Clone, Debug)]
pub struct UniffiDependency {
    /// Package directory that owns the dependency declaration.  Paths from
    /// Cargo metadata are already resolved against the workspace, so this is
    /// used only as the base for diagnostics and relative rendering.
    pub base_dir: Utf8PathBuf,
    /// The requirement selected by Cargo after workspace inheritance has
    /// been applied (for example `^0.32`).
    pub version: Option<String>,
    /// Resolved path from Cargo metadata, not a path copied from a manifest.
    pub path: Option<Utf8PathBuf>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    /// The resolved package name.  Keeping this explicit preserves renamed
    /// source dependencies when the generated crate exposes the canonical
    /// `uniffi` dependency key to its Rust bridge.
    pub package: Option<String>,
    pub default_features: Option<bool>,
    pub features: Vec<String>,
    /// The source-side alias, if the root crate renamed the dependency.  The
    /// generated host intentionally uses the stable `uniffi` key, but the
    /// resolved alias remains available for diagnostics and tests.
    pub rename: Option<String>,
}

/// A host source file held in memory until `GeneratedPackage::write_to`.
/// The type is intentionally the same package file representation used by
/// the canonical producer, which keeps host rendering inside one prepare
/// boundary.
pub(crate) type HostSourceFile = crate::package::PackageFile;

/// Render all host projects and return the exact plan used for their source.
/// `artifact_dir` is optional because source-only package preparation may not
/// have a separate artifact root; in that case the generated source root is
/// supplied by the caller as the effective artifact root.
pub(crate) fn render_host_package(
    package: &NormalizedPackage,
    options: &HostCrateOptions,
    targets: &[PublicTarget],
    package_root: &Utf8Path,
    artifact_dir: Option<&Utf8Path>,
) -> Result<RenderedHostPackage> {
    let want_wasm = targets.contains(&PublicTarget::BrowserWasm);
    let want_napi = targets.contains(&PublicTarget::NodeNapi);
    let want_ohos = targets.contains(&PublicTarget::OhosNapi);
    if !want_wasm && !want_napi && !want_ohos {
        bail!("host package rendering requires at least one engine target");
    }

    let selected_components = selected_components_from_plan(package)?;
    let effective_artifact_dir = artifact_dir
        .map(Utf8Path::to_path_buf)
        .unwrap_or_else(|| package_root.to_path_buf());
    let plan = plan(
        options,
        &selected_components,
        want_wasm,
        want_napi,
        want_ohos,
        package_root,
        &effective_artifact_dir,
    )?;

    let mut files = Vec::new();
    if want_wasm {
        files.extend(render_host_crate(
            &plan.host_root,
            "wasm",
            &plan,
            options,
            "native/wasm.rs",
        )?);
    }
    if want_napi {
        files.extend(render_host_crate(
            &plan.host_root,
            "napi",
            &plan,
            options,
            "native/node.rs",
        )?);
    }
    if want_ohos {
        files.extend(render_host_crate(
            &plan.host_root,
            "ohos",
            &plan,
            options,
            "native/ohos.rs",
        )?);
    }
    Ok(RenderedHostPackage { plan, files })
}

/// Derive component crate roots from canonical Rust call/type paths.
fn selected_components_from_plan(
    package: &NormalizedPackage,
) -> Result<Vec<SelectedHostComponent>> {
    let mut crate_by_component = BTreeMap::<ComponentId, String>::new();

    for engine in package.rust.engines.values() {
        for operation in &engine.operations {
            let path = match &operation.call_target {
                RustCallTarget::FreeFunction { module, .. } => module,
                RustCallTarget::Constructor { object, .. }
                | RustCallTarget::Method { object, .. } => object,
                RustCallTarget::CallbackMethod { callback, .. } => callback,
                RustCallTarget::StreamHook { .. } => continue,
            };
            let root = rust_path_root(path)?;
            insert_component_root(&mut crate_by_component, operation.component_id, root)?;
        }
    }

    for ty in &package.api.types {
        let Some(component_id) = package
            .api
            .components
            .iter()
            .find(|component| component.source_key == *ty.source_key.component())
            .map(|component| component.id)
        else {
            bail!("type {} has no normalized component owner", ty.public_name);
        };
        if crate_by_component.contains_key(&component_id) {
            continue;
        }
        if let Some(named) = package.rust.named_type(ty.id) {
            let root = rust_path_root(&named.rust_path)?;
            insert_component_root(&mut crate_by_component, component_id, root)?;
        }
    }

    let mut result = Vec::with_capacity(package.api.components.len());
    for component in &package.api.components {
        let crate_name = crate_by_component.remove(&component.id).with_context(|| {
            format!(
                "component `{}` has no canonical Rust call/type path for host planning",
                component.public_namespace
            )
        })?;
        result.push(SelectedHostComponent {
            native_export_prefix: uniffi_bindgen::interface::native_export_prefix_for_component(
                &crate_name,
            ),
            crate_name,
            namespace: component.public_namespace.clone(),
        });
    }
    result.sort();
    Ok(result)
}

fn insert_component_root(
    roots: &mut BTreeMap<ComponentId, String>,
    component: ComponentId,
    root: String,
) -> Result<()> {
    if let Some(previous) = roots.insert(component, root.clone()) {
        if previous != root {
            bail!(
                "component {component} has conflicting canonical Rust roots `{previous}` and `{root}`"
            );
        }
    }
    Ok(())
}

fn rust_path_root(path: &RustPath) -> Result<String> {
    path.segments
        .first()
        .filter(|segment| !segment.is_empty())
        .map(|segment| segment.replace('-', "_"))
        .with_context(|| "canonical Rust path has no crate root")
}

/// Resolve Cargo metadata and exact direct package mappings.  This is the
/// only filesystem reader in this module, and it reads the source manifest,
/// never generated output.
pub(crate) fn plan(
    options: &HostCrateOptions,
    selected_components: &[SelectedHostComponent],
    want_wasm: bool,
    want_napi: bool,
    want_ohos: bool,
    package_root: &Utf8Path,
    artifact_dir: &Utf8Path,
) -> Result<HostCratePlan> {
    if selected_components.is_empty() {
        bail!("host-crate generation requires at least one selected component");
    }
    let mut canonical = selected_components.to_vec();
    canonical.sort();
    if canonical != selected_components {
        bail!("host-crate components must be supplied in canonical order");
    }

    let mut metadata_command = MetadataCommand::new();
    metadata_command
        .manifest_path(options.manifest_path.as_std_path())
        .features(CargoOpt::AllFeatures);
    let metadata = metadata_command.exec().with_context(|| {
        format!(
            "running cargo metadata for host manifest {}",
            options.manifest_path
        )
    })?;
    let canonical_manifest = options
        .manifest_path
        .canonicalize_utf8()
        .unwrap_or_else(|_| options.manifest_path.clone());
    let root = metadata
        .packages
        .iter()
        .find(|package| {
            Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf())
                .ok()
                .map(|path| path.canonicalize_utf8().unwrap_or(path) == canonical_manifest)
                .unwrap_or(false)
        })
        .with_context(|| {
            format!(
                "host manifest {} must name a root package",
                options.manifest_path
            )
        })?;
    let root_meta = core_metadata_from_package(&metadata, root)?;
    let resolve = metadata
        .resolve
        .as_ref()
        .context("Cargo metadata has no resolve graph")?;
    let root_node = resolve
        .nodes
        .iter()
        .find(|node| node.id == root.id)
        .context("Cargo resolve graph has no root node")?;
    let root_dependency = planned_root_dependency(root)?;
    let direct_packages = resolved_direct_normal_packages(&metadata, root, root_node)?;

    let mut components = Vec::with_capacity(selected_components.len());
    for component in selected_components {
        let expected = rust_crate_key(&component.crate_name);
        let mut candidates = Vec::new();
        if package_exposes_lib_target(root, &expected) {
            candidates.push(planned_package_dependency(root, &expected)?);
        }
        for package in &direct_packages {
            if package_exposes_lib_target(package, &expected) {
                candidates.push(planned_package_dependency(package, &expected)?);
            }
        }
        candidates.sort_by(|left, right| {
            (left.dependency_key.as_str(), left.package_id.to_string())
                .cmp(&(right.dependency_key.as_str(), right.package_id.to_string()))
        });
        candidates.dedup_by(|left, right| {
            left.dependency_key == right.dependency_key && left.package_id == right.package_id
        });
        match candidates.as_slice() {
            [dependency] => components.push(PlannedComponentDependency {
                package: dependency.clone(),
            }),
            [] => bail!(
                "host component {} is not an exact root/direct lib target",
                component.describe()
            ),
            _ => bail!(
                "host component {} has multiple direct Cargo mappings",
                component.describe()
            ),
        }
    }

    let mut owners = BTreeMap::<String, String>::new();
    owners.insert(
        root_dependency.dependency_key.clone(),
        root_dependency.package_id.to_string(),
    );
    for component in &components {
        let key = component.package.dependency_key.clone();
        let id = component.package.package_id.to_string();
        if let Some(previous) = owners.insert(key.clone(), id.clone()) {
            if previous != id {
                bail!("host dependency key `{key}` maps to multiple Cargo packages");
            }
        }
    }
    validate_fixed_runtime_dependency_keys(
        &root_dependency,
        &components,
        want_wasm,
        want_napi,
        want_ohos,
    )?;
    let host_root = package_relative_dir(package_root, &options.host_crates_dir)?;
    let mut flavors = BTreeMap::new();
    if want_wasm {
        flavors.insert(
            PublicTarget::BrowserWasm,
            host_flavor_plan(
                package_root,
                artifact_dir,
                &host_root,
                "wasm",
                true,
                &root_meta.package_name,
            )?,
        );
    }
    if want_napi {
        flavors.insert(
            PublicTarget::NodeNapi,
            host_flavor_plan(
                package_root,
                artifact_dir,
                &host_root,
                "napi",
                false,
                &root_meta.package_name,
            )?,
        );
    }
    if want_ohos {
        flavors.insert(
            PublicTarget::OhosNapi,
            host_flavor_plan(
                package_root,
                artifact_dir,
                &host_root,
                "ohos",
                false,
                &root_meta.package_name,
            )?,
        );
    }
    Ok(HostCratePlan {
        meta: root_meta,
        root_dependency,
        components,
        host_root,
        flavors,
    })
}

fn host_flavor_plan(
    package_root: &Utf8Path,
    artifact_dir: &Utf8Path,
    host_root: &Utf8Path,
    flavor: &'static str,
    wasm: bool,
    package_name: &str,
) -> Result<HostFlavorPlan> {
    let crate_root = host_root.join(flavor);
    let lib_target = composite_host_lib_target(package_name);
    let artifact_root = package_relative_dir(package_root, artifact_dir)?;
    let native_artifact = if wasm {
        artifact_root
            .join("browser/pkg")
            .join(format!("{lib_target}_bg.wasm"))
    } else if flavor == "napi" {
        artifact_root
            .join("node")
            .join(format!("{lib_target}.node"))
    } else {
        // OHOS builds publish ABI-specific .so files below this stable dist
        // root.  The ABI leaf is selected by the build command, while the
        // root itself remains deterministic and is all the package wrapper
        // needs to import the native module.
        artifact_root.join("ohos/dist")
    };
    let loader_artifact = wasm.then(|| {
        artifact_root
            .join("browser/pkg")
            .join(format!("{lib_target}.js"))
    });
    Ok(HostFlavorPlan {
        crate_version: "0.0.0".to_owned(),
        lib_target,
        crate_root,
        native_artifact,
        loader_artifact,
    })
}

fn render_host_crate(
    host_root: &Utf8Path,
    flavor: &str,
    plan: &HostCratePlan,
    options: &HostCrateOptions,
    native_path: &str,
) -> Result<Vec<HostSourceFile>> {
    let crate_root = host_root.join(flavor);
    let src_root = crate_root.join("src");
    let package_name = composite_host_package_name(&plan.meta.package_name);
    let lib_name = composite_host_lib_target(&plan.meta.package_name);
    // Dependency paths must use the final logical host directory.  Include
    // paths, by contrast, are package-relative and remain valid after the
    // physical staging directory is swapped into place.
    let logical_host_root = options
        .logical_host_crates_dir
        .as_deref()
        .unwrap_or(&options.host_crates_dir);
    let logical_crate_root = absolute_lexical(logical_host_root)?.join(flavor);
    let dependency_text = render_host_dependencies(plan, &logical_crate_root)?;
    let uniffi_dep = render_uniffi_dependency(plan.meta.uniffi_dep.as_ref(), &logical_crate_root)?;
    let native_include = relative_path(&src_root, Utf8Path::new(native_path));
    let (dependencies, build_dependencies, crate_type) = match flavor {
        "wasm" => (
            format!(
                "{dependency_text}{uniffi_dep}async-trait = \"0.1\"\n\
                 futures-channel = \"0.3\"\n\
                 wasm-bindgen = \"=0.2.126\"\n\
                 wasm-bindgen-futures = \"=0.4.76\"\n\
                 js-sys = \"=0.3.103\"\n\
                 wasm-bindgen-uniffi-engine = {{ git = \"https://github.com/EverglowStudio/wasm-bindgen.git\", rev = \"{WASM_REV}\" }}\n"
            ),
            String::new(),
            "[\"cdylib\", \"rlib\"]",
        ),
        "napi" => (
            format!(
                "{dependency_text}{uniffi_dep}async-trait = \"0.1\"\n\
                 futures-channel = \"0.3\"\n\
                 napi = {{ git = \"https://github.com/EverglowStudio/napi-rs.git\", rev = \"{NAPI_REV}\", package = \"napi\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }}\n\
                 napi-derive = {{ git = \"https://github.com/EverglowStudio/napi-rs.git\", rev = \"{NAPI_REV}\", package = \"napi-derive\", features = [\"type-def\"] }}\n\
                 napi-uniffi-engine = {{ git = \"https://github.com/EverglowStudio/napi-rs.git\", rev = \"{NAPI_REV}\" }}\n"
            ),
            format!("napi-build = {{ git = \"https://github.com/EverglowStudio/napi-rs.git\", rev = \"{NAPI_REV}\", package = \"napi-build\" }}\n"),
            "[\"cdylib\"]",
        ),
        "ohos" => (
            format!(
                "{dependency_text}{uniffi_dep}async-trait = \"0.1\"\n\
                 napi-ohos = {{ git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"{OHOS_REV}\", package = \"napi-ohos\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }}\n\
                 napi-derive-ohos = {{ git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"{OHOS_REV}\", package = \"napi-derive-ohos\", features = [\"strict\", \"type-def\"] }}\n\
                 napi-ohos-uniffi-engine = {{ git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"{OHOS_REV}\" }}\n"
            ),
            format!("napi-build-ohos = {{ git = \"https://github.com/EverglowStudio/ohos-rs.git\", rev = \"{OHOS_REV}\", package = \"napi-build-ohos\" }}\n"),
            "[\"cdylib\"]",
        ),
        _ => bail!("unsupported generated host flavor `{flavor}`"),
    };
    let cargo = format!(
        "# AUTOGENERATED by uniffi_bindgen_javascript.\n[package]\nname = \"{package_name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\npublish = false\n\n[lib]\nname = \"{lib_name}\"\ncrate-type = {crate_type}\n\n[dependencies]\n{dependencies}\n[build-dependencies]\n{build_dependencies}\n[workspace]\nresolver = \"3\"\n"
    );
    let build = match flavor {
        "napi" => "extern crate napi_build;\nfn main() { napi_build::setup(); }\n".to_string(),
        "ohos" => {
            "extern crate napi_build_ohos;\nfn main() { napi_build_ohos::setup(); }\n".to_string()
        }
        _ => String::new(),
    };
    let lib = format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript.\ninclude!(\"{}\");\n",
        native_include
    );
    let role = crate::package::PackageFileRole::NativeHost;
    let mut files = vec![
        package_file(crate_root.join("Cargo.toml"), cargo, role),
        package_file(src_root.join("lib.rs"), lib, role),
    ];
    if !build.is_empty() {
        files.push(package_file(crate_root.join("build.rs"), build, role));
    }
    Ok(files)
}

const NAPI_REV: &str = "5ba67e6891722898f2f1e0984ef5d192e0ccd983";
const WASM_REV: &str = "192d5272182776f8d5f7c605611414e2b4435701";
const OHOS_REV: &str = "2a2ae91d91701aadd34734c24769e783bdbdd3c6";

fn package_file(
    path: Utf8PathBuf,
    text: String,
    role: crate::package::PackageFileRole,
) -> HostSourceFile {
    crate::package::PackageFile {
        path: path.as_str().replace('\\', "/"),
        bytes: text.into_bytes(),
        role,
    }
}

fn render_host_dependencies(plan: &HostCratePlan, crate_root: &Utf8Path) -> Result<String> {
    let mut dependencies = vec![&plan.root_dependency];
    dependencies.extend(plan.components.iter().map(|component| &component.package));
    dependencies.sort_by(|left, right| left.dependency_key.cmp(&right.dependency_key));
    let mut seen = BTreeMap::<String, String>::new();
    let mut output = String::new();
    for dependency in dependencies {
        if let Some(previous) = seen.insert(
            dependency.dependency_key.clone(),
            dependency.package_id.to_string(),
        ) {
            if previous == dependency.package_id.to_string() {
                continue;
            }
            bail!(
                "host dependency key `{}` maps to multiple packages",
                dependency.dependency_key
            );
        }
        let rel = relative_path(crate_root, &dependency.package_dir);
        let default_features = if dependency.package_id == plan.root_dependency.package_id {
            ""
        } else {
            ", default-features = false"
        };
        output.push_str(&format!(
            "{} = {{ package = {}, path = {}{} }}\n",
            dependency.dependency_key,
            toml_string_literal(&dependency.package_name),
            toml_string_literal(rel.as_str()),
            default_features
        ));
    }
    Ok(output)
}

pub fn composite_host_package_name(package_name: &str) -> String {
    format!("{package_name}-uniffi-js-host")
}

pub fn composite_host_lib_target(package_name: &str) -> String {
    format!("{}_uniffi_js_host", rust_crate_key(package_name))
}

fn package_relative_dir(package_root: &Utf8Path, path: &Utf8Path) -> Result<Utf8PathBuf> {
    let root = absolute_lexical(package_root)?;
    let path = absolute_lexical(path)?;
    path.strip_prefix(&root)
        .map(Utf8Path::to_path_buf)
        .with_context(|| format!("host output {path} must be inside package root {root}"))
}

fn absolute_lexical(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not UTF-8: {}", p.display()))?
            .join(path))
    }
}

fn relative_path(from_dir: &Utf8Path, to: &Utf8Path) -> Utf8PathBuf {
    let from = from_dir
        .components()
        .map(|component| component.as_str())
        .collect::<Vec<_>>();
    let to = to
        .components()
        .map(|component| component.as_str())
        .collect::<Vec<_>>();
    let mut common = 0;
    while common < from.len() && common < to.len() && from[common] == to[common] {
        common += 1;
    }
    let mut result = Utf8PathBuf::new();
    for _ in common..from.len() {
        result.push("..");
    }
    for component in &to[common..] {
        result.push(component);
    }
    if result.as_str().is_empty() {
        result.push(".");
    }
    result
}

fn rust_crate_key(value: &str) -> String {
    value.replace('-', "_")
}

fn validate_fixed_runtime_dependency_keys(
    root: &PlannedPackageDependency,
    components: &[PlannedComponentDependency],
    want_wasm: bool,
    want_napi: bool,
    want_ohos: bool,
) -> Result<()> {
    let mut occupied = vec![root.dependency_key.clone()];
    occupied.extend(
        components
            .iter()
            .map(|component| component.package.dependency_key.clone()),
    );
    let mut fixed = BTreeSet::from(["uniffi".to_string(), "async_trait".to_string()]);
    if want_wasm {
        fixed.extend(["wasm_bindgen", "wasm_bindgen_futures", "js_sys"].map(str::to_string));
    }
    if want_napi {
        fixed.extend(
            ["napi", "napi_derive", "napi_build", "napi_uniffi_engine"].map(str::to_string),
        );
    }
    if want_ohos {
        fixed.extend(
            [
                "napi_ohos",
                "napi_derive_ohos",
                "napi_build_ohos",
                "napi_ohos_uniffi_engine",
            ]
            .map(str::to_string),
        );
    }
    fixed.insert(composite_host_lib_target(&root.package_name));
    let collisions = occupied
        .into_iter()
        .filter(|key| fixed.contains(key))
        .collect::<Vec<_>>();
    if !collisions.is_empty() {
        bail!(
            "host dependency key collides with generated runtime: {}",
            collisions.join(", ")
        );
    }
    Ok(())
}

fn resolved_direct_normal_packages<'a>(
    metadata: &'a cargo_metadata::Metadata,
    root: &Package,
    root_node: &cargo_metadata::Node,
) -> Result<Vec<&'a Package>> {
    let mut result = Vec::new();
    for dependency in root
        .dependencies
        .iter()
        .filter(|dep| dep.kind == DependencyKind::Normal)
    {
        let source_key = rust_crate_key(dependency.rename.as_deref().unwrap_or(&dependency.name));
        let matches = root_node
            .deps
            .iter()
            .filter(|node| {
                node.dep_kinds
                    .iter()
                    .any(|kind| kind.kind == DependencyKind::Normal)
                    && rust_crate_key(&node.name) == source_key
            })
            .filter_map(|node| {
                metadata
                    .packages
                    .iter()
                    .find(|package| package.id == node.pkg && package.name == dependency.name)
            })
            .collect::<Vec<_>>();
        if matches.len() > 1 {
            bail!(
                "dependency `{}` resolves to multiple package IDs",
                dependency.name
            );
        }
        result.extend(matches);
    }
    Ok(result)
}

fn planned_root_dependency(package: &Package) -> Result<PlannedPackageDependency> {
    let target = unique_lib_target(package)?;
    planned_package_dependency(package, &target)
}

fn planned_package_dependency(package: &Package, key: &str) -> Result<PlannedPackageDependency> {
    Ok(PlannedPackageDependency {
        dependency_key: key.to_string(),
        package_id: package.id.clone(),
        package_name: package.name.to_string(),
        package_dir: package_dir(package)?,
    })
}

fn unique_lib_target(package: &Package) -> Result<String> {
    let mut targets = package
        .targets
        .iter()
        .filter(|target| target.kind.iter().any(is_rust_library_target_kind))
        .map(|target| target.name.clone())
        .collect::<Vec<_>>();
    targets.sort();
    targets.dedup();
    match targets.as_slice() {
        [target] => Ok(target.clone()),
        [] => bail!("root package `{}` has no Rust library target", package.name),
        _ => bail!("root package `{}` has multiple lib targets", package.name),
    }
}

fn package_exposes_lib_target(package: &Package, expected: &str) -> bool {
    package.targets.iter().any(|target| {
        target.name == expected && target.kind.iter().any(is_rust_library_target_kind)
    })
}

/// Cargo reports an explicit `[lib] crate-type = ["rlib"]` target as
/// `TargetKind::RLib`, rather than the shorthand `TargetKind::Lib`.  RLib is
/// the normal Rust dependency artifact and is therefore a valid host source
/// target just like `lib`, `dylib`, or `cdylib`; rejecting it would make a
/// legal library fixture look like a binary-only package.  Keep this check
/// tied to Cargo's resolved target kind instead of inferring a path or
/// accepting arbitrary dependency packages.
fn is_rust_library_target_kind(kind: &TargetKind) -> bool {
    matches!(
        kind,
        TargetKind::Lib | TargetKind::RLib | TargetKind::DyLib | TargetKind::CDyLib
    )
}

fn package_dir(package: &Package) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf())
        .map_err(|path| anyhow::anyhow!("Cargo manifest path is not UTF-8: {}", path.display()))?
        .parent()
        .map(Utf8Path::to_path_buf)
        .context("Cargo package manifest has no parent")
}

pub fn load_metadata(manifest_path: &Utf8Path) -> Result<CoreCrateMetadata> {
    let metadata = MetadataCommand::new()
        .manifest_path(manifest_path.as_std_path())
        .features(CargoOpt::AllFeatures)
        .exec()
        .with_context(|| format!("running cargo metadata for {manifest_path}"))?;
    let package = metadata
        .root_package()
        .context("Cargo manifest has no root package")?;
    core_metadata_from_package(&metadata, package)
}

fn core_metadata_from_package(
    metadata: &cargo_metadata::Metadata,
    package: &Package,
) -> Result<CoreCrateMetadata> {
    let crate_dir = package
        .manifest_path
        .parent()
        .unwrap_or_else(|| Utf8Path::new("."));
    Ok(CoreCrateMetadata {
        package_name: package.name.to_string(),
        package_version: package.version.to_string(),
        description: package.description.clone(),
        authors: package.authors.clone(),
        license: package.license.clone(),
        crate_dir: crate_dir.to_path_buf(),
        uniffi_dep: resolve_uniffi_dependency(metadata, package)?,
    })
}

fn resolve_uniffi_dependency(
    metadata: &cargo_metadata::Metadata,
    package: &Package,
) -> Result<Option<UniffiDependency>> {
    let mut candidates = package
        .dependencies
        .iter()
        .filter(|dependency| {
            dependency.kind == DependencyKind::Normal
                && (dependency.name == "uniffi" || dependency.rename.as_deref() == Some("uniffi"))
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return Ok(None);
    }
    candidates.sort_by_key(|dependency| dependency.target.as_ref().map(ToString::to_string));
    let dependency = candidates[0];
    let root_node = metadata
        .resolve
        .as_ref()
        .and_then(|resolve| resolve.nodes.iter().find(|node| node.id == package.id));
    let resolved = root_node
        .into_iter()
        .flat_map(|node| node.deps.iter())
        .filter(|node_dep| {
            node_dep
                .dep_kinds
                .iter()
                .any(|kind| kind.kind == DependencyKind::Normal)
                && (node_dep.name == dependency.rename.as_deref().unwrap_or(&dependency.name)
                    || node_dep.name == dependency.name)
        })
        .filter_map(|node_dep| metadata.packages.iter().find(|pkg| pkg.id == node_dep.pkg))
        .next();
    let package_name = resolved
        .map(|pkg| pkg.name.to_string())
        .unwrap_or_else(|| dependency.name.clone());
    let (git, rev) = dependency
        .source
        .as_ref()
        .and_then(|source| parse_git_source(&source.repr))
        .map(|(git, rev)| (Some(git), rev))
        .unwrap_or((None, None));
    let base_dir = package
        .manifest_path
        .parent()
        .unwrap_or_else(|| Utf8Path::new("."))
        .to_path_buf();
    Ok(Some(UniffiDependency {
        base_dir,
        version: (!dependency.req.to_string().is_empty()).then(|| dependency.req.to_string()),
        path: dependency.path.clone(),
        git,
        branch: None,
        tag: None,
        rev,
        package: Some(package_name),
        default_features: Some(dependency.uses_default_features),
        features: dependency.features.clone(),
        rename: dependency.rename.clone(),
    }))
}

fn parse_git_source(source: &str) -> Option<(String, Option<String>)> {
    let source = source.strip_prefix("git+")?;
    let (url, revision) = source.split_once('#').unwrap_or((source, ""));
    Some((
        url.to_owned(),
        (!revision.is_empty()).then(|| revision.to_owned()),
    ))
}

fn render_uniffi_dependency(
    dep: Option<&UniffiDependency>,
    crate_dir: &Utf8Path,
) -> Result<String> {
    let Some(dep) = dep else {
        return Ok(String::new());
    };
    let mut fields = Vec::new();
    if let Some(path) = &dep.path {
        fields.push(format!(
            "path = {}",
            toml_string_literal(relative_path(crate_dir, path).as_str())
        ));
    }
    if let Some(version) = &dep.version {
        fields.push(format!("version = {}", toml_string_literal(version)));
    }
    if let Some(git) = &dep.git {
        fields.push(format!("git = {}", toml_string_literal(git)));
    }
    if let Some(rev) = &dep.rev {
        fields.push(format!("rev = {}", toml_string_literal(rev)));
    }
    if let Some(default_features) = dep.default_features {
        fields.push(format!("default-features = {default_features}"));
    }
    if !dep.features.is_empty() {
        let features = dep
            .features
            .iter()
            .map(|feature| toml_string_literal(feature))
            .collect::<Vec<_>>()
            .join(", ");
        fields.push(format!("features = [{features}]"));
    }
    if fields.is_empty() {
        bail!("resolved `uniffi` dependency has no renderable fields");
    }
    // Always keep the generated dependency key stable.  `package` carries
    // the resolved package name when the source dependency was inherited or
    // renamed, so this does not silently turn an alias into a different
    // package.
    if let Some(package) = &dep.package {
        fields.insert(0, format!("package = {}", toml_string_literal(package)));
    }
    Ok(format!("uniffi = {{ {} }}\n", fields.join(", ")))
}

fn toml_string_literal(value: &str) -> String {
    let mut output = String::from("\"");
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                output.push_str(&format!("\\u{:04X}", character as u32))
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_staging_uses_physical_package_relative_paths() {
        let staging = Utf8Path::new("/tmp/uniffi-stage");
        let physical_host = staging.join("native/hosts");
        let logical_host = Utf8Path::new("/Users/example/package/native/hosts");

        let host_rel = package_relative_dir(staging, &physical_host).unwrap();
        assert_eq!(host_rel, Utf8Path::new("native/hosts"));

        // The logical final path is used only for Cargo dependency paths while
        // the physical host remains inside the staging package root.
        assert!(!logical_host.starts_with(staging));
        let include = relative_path(&host_rel.join("napi/src"), Utf8Path::new("native/node.rs"));
        assert_eq!(include, Utf8Path::new("../../../node.rs"));
    }
}
