/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Optional Rust host-crate emitter for the JavaScript target.
//!
//! Browsers need a `cdylib` that `#[wasm_bindgen]`-exports the generated
//! `components/<namespace>/browser/<crate>.rs`; Node/Electron consumers need
//! one that `#[napi]`-exports `components/<namespace>/node/<crate>.rs`; and
//! Harmony/OpenHarmony needs the corresponding NAPI-OHOS bridge. Before this
//! module existed, each downstream project had to hand-maintain those shim
//! crates. This module derives package-level composite hosts from the root
//! package manifest, so users only maintain their root package.
//!
//! The feature is fully opt-in — invoking
//! `uniffi-bindgen generate --language javascript` without
//! `--emit-host-crates` produces the exact same tree as before.
//!
//! Electron does **not** get its own Rust crate; it reuses the N-API host
//! crate (see `electron/mod.rs`).

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use cargo_metadata::{CargoOpt, DependencyKind, MetadataCommand, Package, PackageId, TargetKind};
use fs_err as fs;
use sha2::{Digest, Sha256};

const HOST_BUNDLE_SCHEMA_VERSION: u32 = 3;

/// Canonical identity of one selected JavaScript component before Cargo
/// dependency planning.  The generator supplies these in stable order, but
/// the planner independently verifies that order and every native prefix so a
/// host crate can never accidentally bind a bridge to a similarly named
/// package.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct HostComponentIdentity {
    pub(crate) crate_name: String,
    pub(crate) namespace: String,
    pub(crate) native_export_prefix: String,
}

impl HostComponentIdentity {
    pub(crate) fn describe(&self) -> String {
        format!(
            "component `{}` (namespace `{}`, native export prefix `{}`)",
            self.crate_name, self.namespace, self.native_export_prefix
        )
    }
}

#[derive(Clone, Debug)]
struct PlannedPackageDependency {
    /// Rust extern-crate key written in every generated host Cargo.toml.
    dependency_key: String,
    /// Resolved Cargo package identity.  Package names alone are not unique
    /// enough to prove that an aliased direct dependency is the dependency
    /// selected by the current root resolve graph.
    package_id: PackageId,
    /// Cargo package name, which may differ from its Rust lib target.
    package_name: String,
    package_dir: Utf8PathBuf,
}

#[derive(Clone, Debug)]
struct PlannedComponentDependency {
    identity: HostComponentIdentity,
    package: PlannedPackageDependency,
}

/// A complete, read-only host plan.  It is created before the generator owns
/// its first output path, which makes missing/ambiguous Cargo mappings fail
/// closed without leaving a partial source or host tree behind.
#[derive(Clone, Debug)]
pub(crate) struct HostCratePlan {
    meta: CoreCrateMetadata,
    /// Every generated host crate depends on its root package exactly once so
    /// `--features <root-lib>/<feature>` has a stable owner even when the
    /// root package is not one of the selected UniFFI components.
    root_dependency: PlannedPackageDependency,
    components: Vec<PlannedComponentDependency>,
}

impl HostCratePlan {
    pub(crate) fn napi_artifact_stem(&self) -> String {
        composite_host_lib_target(&self.meta.package_name)
    }

    pub(crate) fn wasm_artifact_stem(&self) -> String {
        composite_host_lib_target(&self.meta.package_name)
    }

    pub(crate) fn ohos_artifact_stem(&self) -> String {
        composite_host_lib_target(&self.meta.package_name)
    }

    fn ohos_host_package_name(&self) -> String {
        composite_host_package_name(&self.meta.package_name)
    }

    fn ohos_composite_identity(&self) -> Result<String> {
        composite_host_identity(
            &self.ohos_host_package_name(),
            &self.ohos_artifact_stem(),
            &self
                .components
                .iter()
                .map(|component| {
                    (
                        component.identity.crate_name.clone(),
                        component.identity.namespace.clone(),
                        component.identity.native_export_prefix.clone(),
                    )
                })
                .collect::<Vec<_>>(),
        )
    }
}

/// Caller-supplied metadata + CLI flags for host-crate emission.
#[derive(Clone, Debug)]
pub struct HostCrateOptions {
    /// Path to the root package's Cargo manifest.
    pub manifest_path: Utf8PathBuf,
    /// Directory (usually `rust_modules`) in which to write
    /// `wasm/`, `napi/`, and/or `ohos/` subcrates. Resolved relative to
    /// the current working directory if not absolute.
    pub host_crates_dir: Utf8PathBuf,
    /// Logical publication directory used when generated Cargo manifests
    /// compute path dependencies. Invocation-private artifact builds write to
    /// `host_crates_dir` but must retain paths valid after the tree is moved to
    /// this final location.
    pub logical_host_crates_dir: Option<Utf8PathBuf>,
    /// Logical publication root for generated bridge files. Invocation-private
    /// coordinators use this to emit `include!` paths that remain valid after
    /// the host crate and bindings are published.
    pub logical_out_dir: Option<Utf8PathBuf>,
    /// Optional local development checkout of `ohos-rs`; when set, the OHOS
    /// host crate uses path dependencies instead of the default crates.io
    /// versions.
    pub ohos_rs_dir: Option<Utf8PathBuf>,
}

/// Manifest metadata extracted from the root package manifest.
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
    pub base_dir: Utf8PathBuf,
    pub version: Option<String>,
    pub path: Option<String>,
    pub git: Option<String>,
    pub branch: Option<String>,
    pub tag: Option<String>,
    pub rev: Option<String>,
    pub package: Option<String>,
    pub default_features: Option<bool>,
    pub features: Vec<String>,
}

pub fn load_metadata(manifest_path: &Utf8Path) -> Result<CoreCrateMetadata> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("reading manifest at {manifest_path}"))?;
    let value: toml::Value =
        toml::from_str(&text).with_context(|| format!("parsing {manifest_path}"))?;
    // Cargo owns workspace inheritance semantics.  Reading the package table
    // directly would see `version.workspace = true` as a table and would also
    // silently lose inherited optional metadata.  Use Cargo's resolved model
    // for every field that can be inherited, while retaining the source TOML
    // below only for rendering the original `uniffi` dependency declaration.
    let cargo_metadata = MetadataCommand::new()
        .manifest_path(manifest_path.as_std_path())
        .no_deps()
        .exec()
        .with_context(|| format!("running cargo metadata for {manifest_path}"))?;
    let canonical_manifest = manifest_path
        .canonicalize_utf8()
        .unwrap_or_else(|_| manifest_path.to_path_buf());
    let package = cargo_metadata
        .packages
        .iter()
        .find(|package| {
            let package_manifest =
                Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf()).ok();
            package_manifest
                .map(|path| path.canonicalize_utf8().unwrap_or(path) == canonical_manifest)
                .unwrap_or(false)
        })
        .or_else(|| cargo_metadata.root_package())
        .with_context(|| format!("cargo metadata did not resolve package {manifest_path}"))?;
    core_metadata_from_package(manifest_path, &value, package)
}

/// Build host-manifest metadata from a Cargo-resolved root package while
/// retaining the source TOML only for exact rendering of the `uniffi`
/// dependency declaration.  Keeping this separate lets [`plan`] use the same
/// all-features Cargo metadata snapshot for both resolve-graph mapping and
/// package metadata, rather than risking a second, differently-featured
/// query.
fn core_metadata_from_package(
    manifest_path: &Utf8Path,
    value: &toml::Value,
    package: &Package,
) -> Result<CoreCrateMetadata> {
    let package_name = package.name.to_string();
    let package_version = package.version.to_string();
    let description = package.description.clone();
    let authors = package.authors.clone();
    let license = package.license.clone();
    let uniffi_dep = resolve_uniffi_dependency(manifest_path, &value)?;
    let crate_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("."));
    let crate_dir = crate_dir.canonicalize_utf8().unwrap_or_else(|_| crate_dir);
    Ok(CoreCrateMetadata {
        package_name,
        package_version,
        description,
        authors,
        license,
        crate_dir,
        uniffi_dep,
    })
}

/// Resolve every selected UniFFI component against the root package's direct
/// Cargo dependencies.  The generated bridge uses the component's Rust lib
/// target as an extern-crate path, so accepting a transitive dependency, a
/// package-name lookalike, or an alias mismatch would produce a host crate
/// that compiles only by accident (or routes to the wrong component).
pub(crate) fn plan(
    options: &HostCrateOptions,
    identities: &[HostComponentIdentity],
    want_wasm: bool,
    want_napi: bool,
    want_ohos: bool,
) -> Result<HostCratePlan> {
    if identities.is_empty() {
        bail!("host-crate generation requires at least one selected component");
    }

    let mut canonical_identities = identities.to_vec();
    canonical_identities.sort();
    if canonical_identities != identities {
        bail!(
            "host-crate component identities must be supplied in canonical (crate, namespace, native prefix) order"
        );
    }
    for identity in identities {
        let expected =
            uniffi_bindgen::interface::native_export_prefix_for_component(&identity.crate_name);
        if identity.crate_name.is_empty()
            || identity.namespace.is_empty()
            || identity.native_export_prefix != expected
        {
            bail!(
                "host-crate component identity is invalid for {}",
                identity.describe()
            );
        }
    }

    let mut metadata_command = MetadataCommand::new();
    metadata_command.manifest_path(options.manifest_path.as_std_path());
    // The generated source has already selected its actual components.  Plan
    // over every optional direct edge so a feature-gated component selected by
    // that source is not accidentally invisible to this independent Cargo
    // metadata invocation.  We still reject any ambiguous package mapping.
    metadata_command.features(CargoOpt::AllFeatures);
    let metadata = metadata_command.exec().with_context(|| {
        format!(
            "running cargo metadata for composite host manifest {}",
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
                "host manifest {} must name one real root package; virtual workspaces are not supported",
                options.manifest_path
            )
    })?;
    let manifest_text = fs::read_to_string(&options.manifest_path)
        .with_context(|| format!("reading manifest at {}", options.manifest_path))?;
    let manifest_value: toml::Value = toml::from_str(&manifest_text)
        .with_context(|| format!("parsing {}", options.manifest_path))?;
    let root_meta = core_metadata_from_package(&options.manifest_path, &manifest_value, root)?;
    let resolve = metadata.resolve.as_ref().with_context(|| {
        format!(
            "cargo metadata for host manifest {} did not include a resolve graph",
            options.manifest_path
        )
    })?;
    let root_node = resolve
        .nodes
        .iter()
        .find(|node| node.id == root.id)
        .with_context(|| {
            format!(
                "cargo metadata resolve graph has no node for root package {}",
                root.name
            )
        })?;
    let root_dependency = planned_root_dependency(root)?;
    let direct_packages = resolved_direct_normal_packages(&metadata, root, root_node)?;

    let mut components = Vec::with_capacity(identities.len());
    for identity in identities {
        let expected_lib_target = rust_crate_key(&identity.crate_name);
        let mut candidates = Vec::new();

        if package_exposes_lib_target(root, &expected_lib_target) {
            candidates.push(planned_package_dependency(root, &expected_lib_target)?);
        }

        for package in &direct_packages {
            if package_exposes_lib_target(package, &expected_lib_target) {
                // `NodeDep.name` proved this is a direct dependency selected
                // by the current root resolve graph (including aliases), but
                // the generated bridge imports the library target.  Render
                // that target as the host dependency key instead of copying
                // the root manifest's alias.
                candidates.push(planned_package_dependency(package, &expected_lib_target)?);
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
            [package] => components.push(PlannedComponentDependency {
                identity: identity.clone(),
                package: package.clone(),
            }),
            [] => bail!(
                "host-crate component {} is not an exact lib target of the root package or one of its direct normal dependencies",
                identity.describe()
            ),
            _ => bail!(
                "host-crate component {} has multiple direct package mappings; use one exact dependency key/lib target",
                identity.describe()
            ),
        }
    }

    let mut dependency_owners = std::collections::BTreeMap::<String, (String, Vec<String>)>::new();
    dependency_owners.insert(
        root_dependency.dependency_key.clone(),
        (
            root_dependency.package_id.to_string(),
            vec![format!("root package `{}`", root_dependency.package_name)],
        ),
    );
    let mut collisions = Vec::new();
    for component in &components {
        let package = &component.package;
        let owner = component.identity.describe();
        match dependency_owners.entry(package.dependency_key.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((package.package_id.to_string(), vec![owner]));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                let (existing_package_id, owners) = entry.get_mut();
                if *existing_package_id == package.package_id.to_string() {
                    owners.push(owner);
                } else {
                    collisions.push((
                        package.dependency_key.clone(),
                        existing_package_id.clone(),
                        package.package_id.to_string(),
                        owners.clone(),
                        owner,
                    ));
                }
            }
        }
    }
    if !collisions.is_empty() {
        let details = collisions
            .into_iter()
            .map(|(key, left_id, right_id, owners, owner)| {
                format!(
                    "`{key}` => package {left_id} ({}) conflicts with package {right_id} ({owner})",
                    owners.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        bail!("host-crate direct dependency key collision: {details}");
    }
    validate_fixed_runtime_dependency_keys(
        &root_dependency,
        &components,
        want_wasm,
        want_napi,
        want_ohos,
    )?;

    Ok(HostCratePlan {
        meta: root_meta,
        root_dependency,
        components,
    })
}

/// Dependency keys occupied by the requested generated host runtimes. Check
/// them while planning, before any generated source or host directory is
/// created, rather than emitting an invalid duplicate TOML key after source
/// generation.  Flavor-specific keys stay scoped to the requested flavors so
/// a wasm-only component named `napi`, for example, remains valid.
fn validate_fixed_runtime_dependency_keys(
    root_dependency: &PlannedPackageDependency,
    components: &[PlannedComponentDependency],
    want_wasm: bool,
    want_napi: bool,
    want_ohos: bool,
) -> Result<()> {
    let mut occupied = vec![root_dependency];
    occupied.extend(components.iter().map(|component| &component.package));
    let mut fixed_keys = vec!["uniffi".to_string(), "async_trait".to_string()];
    if want_wasm {
        fixed_keys.extend(
            ["wasm_bindgen", "wasm_bindgen_futures", "js_sys"]
                .into_iter()
                .map(str::to_string),
        );
    }
    if want_napi {
        fixed_keys.extend(
            ["napi", "napi_derive", "napi_build"]
                .into_iter()
                .map(str::to_string),
        );
    }
    if want_ohos {
        fixed_keys.extend(
            ["napi_ohos", "napi_derive_ohos", "napi_build_ohos"]
                .into_iter()
                .map(str::to_string),
        );
    }
    fixed_keys.push(composite_host_lib_target(&root_dependency.package_name));
    let collisions = occupied
        .into_iter()
        .filter(|dependency| fixed_keys.contains(&dependency.dependency_key))
        .map(|dependency| {
            format!(
                "`{}` (package {})",
                dependency.dependency_key, dependency.package_id
            )
        })
        .collect::<Vec<_>>();
    if !collisions.is_empty() {
        bail!(
            "host-crate dependency key collides with a fixed generated runtime dependency: {}",
            collisions.join(", ")
        );
    }
    Ok(())
}

/// Return the direct, normal packages selected by the root package's current
/// Cargo resolve node.  `Package.dependencies` alone cannot distinguish
/// aliases or multiple package IDs with the same package name; `NodeDep` can.
fn resolved_direct_normal_packages<'a>(
    metadata: &'a cargo_metadata::Metadata,
    root: &Package,
    root_node: &cargo_metadata::Node,
) -> Result<Vec<&'a Package>> {
    let mut direct_packages = Vec::new();
    for dependency in root
        .dependencies
        .iter()
        .filter(|dependency| dependency.kind == DependencyKind::Normal)
    {
        let source_key = rust_crate_key(
            dependency
                .rename
                .as_deref()
                .unwrap_or(dependency.name.as_str()),
        );
        let mut matches = Vec::new();
        for node_dependency in &root_node.deps {
            if !node_dependency
                .dep_kinds
                .iter()
                .any(|kind| kind.kind == DependencyKind::Normal)
            {
                continue;
            }
            if rust_crate_key(&node_dependency.name) != source_key {
                continue;
            }
            let package = metadata
                .packages
                .iter()
                .find(|package| package.id == node_dependency.pkg)
                .with_context(|| {
                    format!(
                        "cargo metadata resolve node for {} references missing package {}",
                        root.name, node_dependency.pkg
                    )
                })?;
            if package.name != dependency.name {
                continue;
            }
            matches.push(package);
        }
        matches.sort_by_key(|package| package.id.to_string());
        matches.dedup_by(|left, right| left.id == right.id);
        if matches.len() > 1 {
            let ids = matches
                .iter()
                .map(|package| package.id.to_string())
                .collect::<Vec<_>>();
            bail!(
                "host-crate direct dependency alias `{}` for package `{}` resolves to multiple package IDs: {}",
                dependency.rename.as_deref().unwrap_or(&dependency.name),
                dependency.name,
                ids.join(", ")
            );
        }
        direct_packages.extend(matches);
    }
    direct_packages.sort_by_key(|package| package.id.to_string());
    direct_packages.dedup_by(|left, right| left.id == right.id);
    Ok(direct_packages)
}

fn planned_root_dependency(package: &Package) -> Result<PlannedPackageDependency> {
    let target = unique_lib_target(package)?;
    planned_package_dependency(package, &target)
}

fn planned_package_dependency(
    package: &Package,
    dependency_key: &str,
) -> Result<PlannedPackageDependency> {
    Ok(PlannedPackageDependency {
        dependency_key: dependency_key.to_string(),
        package_id: package.id.clone(),
        package_name: package.name.to_string(),
        package_dir: package_dir(package)?,
    })
}

fn unique_lib_target(package: &Package) -> Result<String> {
    let mut targets = package
        .targets
        .iter()
        .filter(|target| {
            target
                .kind
                .iter()
                .any(|kind| matches!(kind, TargetKind::Lib | TargetKind::CDyLib))
        })
        .map(|target| target.name.clone())
        .collect::<Vec<_>>();
    targets.sort();
    targets.dedup();
    match targets.as_slice() {
        [target] => Ok(target.clone()),
        [] => bail!(
            "root package `{}` has no Rust lib/cdylib target for generated host feature ownership",
            package.name
        ),
        _ => bail!(
            "root package `{}` has multiple Rust lib/cdylib targets ({}) and cannot provide one generated host feature owner",
            package.name,
            targets.join(", ")
        ),
    }
}

fn rust_crate_key(value: &str) -> String {
    value.replace('-', "_")
}

/// Stable private Rust module name for one generated component bridge.
///
/// The per-component bridge files are included into sibling modules in every
/// generated composite host crate.  Their Rust type metadata identifies an
/// owner by crate root, not by its public JavaScript namespace, so this name
/// must be derived from that same normalized crate-root identity.  Keeping
/// the encoding here makes host inclusion and cross-component bridge paths
/// share one contract even when a component's Cargo package, Rust lib target,
/// and JavaScript namespace differ.
pub(crate) fn component_bridge_module_name(component_or_module_path: &str) -> String {
    let crate_root = component_or_module_path
        .split("::")
        .next()
        .unwrap_or(component_or_module_path);
    let crate_root = rust_crate_key(crate_root);
    let encoded = crate_root
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("__uniffi_component_{encoded}")
}

fn package_exposes_lib_target(package: &Package, expected: &str) -> bool {
    package.targets.iter().any(|target| {
        target.name == expected
            && target
                .kind
                .iter()
                .any(|kind| matches!(kind, TargetKind::Lib | TargetKind::CDyLib))
    })
}

fn package_dir(package: &Package) -> Result<Utf8PathBuf> {
    Utf8PathBuf::from_path_buf(package.manifest_path.clone().into_std_path_buf())
        .map_err(|path| anyhow::anyhow!("Cargo manifest path is not UTF-8: {}", path.display()))?
        .parent()
        .map(Utf8Path::to_path_buf)
        .context("Cargo package manifest has no parent directory")
}

/// Stable logical Cargo package name shared by all generated JavaScript host
/// flavors. The host directories and artifact extensions remain flavor
/// specific, but this tuple is intentionally identical for wasm, N-API and
/// OHOS so a package-level manifest has one unambiguous host identity.
pub fn composite_host_package_name(package_name: &str) -> String {
    format!("{package_name}-uniffi-js-host")
}

/// Stable logical library target shared by all generated JavaScript host
/// flavors. See [`composite_host_package_name`].
pub fn composite_host_lib_target(package_name: &str) -> String {
    format!("{}_uniffi_js_host", rust_crate_key(package_name))
}

/// Canonical composite-host identity shared by generated host bundles and
/// artifact manifests.  It deliberately excludes generated contract text:
/// contract/sidecar integrity belongs to the OHOS bundle fingerprint, while
/// this digest means exactly "this host target contains this component set".
///
/// Callers may supply identities in loader order; the serialized payload is
/// always sorted by the complete `(component, namespace, nativeExportPrefix)`
/// tuple so equivalent selections have one stable identity.
pub fn composite_host_identity(
    package_name: &str,
    lib_target: &str,
    components: &[(String, String, String)],
) -> Result<String> {
    let mut components = components
        .iter()
        .map(|(component, namespace, native_export_prefix)| {
            serde_json::json!({
                "component": component,
                "namespace": namespace,
                "nativeExportPrefix": native_export_prefix,
            })
        })
        .collect::<Vec<_>>();
    components.sort_by(|left, right| {
        (
            left["component"].as_str(),
            left["namespace"].as_str(),
            left["nativeExportPrefix"].as_str(),
        )
            .cmp(&(
                right["component"].as_str(),
                right["namespace"].as_str(),
                right["nativeExportPrefix"].as_str(),
            ))
    });
    let payload = serde_json::json!({
        "packageName": package_name,
        "libTarget": lib_target,
        "components": components,
    });
    Ok(sha256_bytes(&serde_json::to_vec(&payload)?))
}

/// Emit selected `<host_crates_dir>/{wasm,napi,ohos}/*` composite hosts.
///
/// `out_dir` is the JS target's `--out-dir` (already populated with
/// `components/<namespace>/browser/<crate>.rs` and
/// `components/<namespace>/node/<crate>.rs`, and/or
/// `components/<namespace>/harmony/<crate>.rs` by earlier steps). Each
/// selected host `include!`s one bridge module per component.
pub(crate) fn emit(
    options: &HostCrateOptions,
    out_dir: &Utf8Path,
    plan: &HostCratePlan,
    want_wasm: bool,
    want_napi: bool,
    want_ohos: bool,
) -> Result<()> {
    if plan.components.is_empty() {
        bail!("host-crate emission requested but no components were generated");
    }
    if !want_wasm && !want_napi && !want_ohos {
        return Ok(());
    }
    let host_dir = if options.host_crates_dir.is_absolute() {
        options.host_crates_dir.clone()
    } else {
        let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?;
        cwd.join(&options.host_crates_dir)
    };
    let out_dir_abs = out_dir
        .canonicalize_utf8()
        .with_context(|| format!("canonicalizing out_dir {out_dir}"))?;
    let host_dir_abs = canonicalize_planned_path(&host_dir)
        .with_context(|| format!("canonicalizing planned host directory {host_dir}"))?;
    let logical_host_dir = options
        .logical_host_crates_dir
        .clone()
        .unwrap_or_else(|| host_dir_abs.clone());
    let logical_host_dir = if logical_host_dir.is_absolute() {
        logical_host_dir
    } else {
        Utf8PathBuf::from_path_buf(std::env::current_dir()?)
            .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
            .join(logical_host_dir)
    };
    let logical_out_dir = options
        .logical_out_dir
        .clone()
        .unwrap_or_else(|| out_dir_abs.clone());

    // Validate every OHOS invocation input and every fallible host rendering
    // operation before creating the host directory or emitting another flavor.
    // A malformed facade bundle must never leave wasm/napi/OHOS partial output.
    if want_ohos {
        preflight_ohos_host_emission(
            &logical_host_dir,
            &out_dir_abs,
            &logical_out_dir,
            plan,
            options.ohos_rs_dir.as_ref(),
        )?;
    }

    fs::create_dir_all(&host_dir_abs)?;

    if want_wasm {
        emit_wasm(&host_dir_abs, &logical_host_dir, &logical_out_dir, plan)?;
    }
    if want_napi {
        emit_napi(
            &host_dir_abs,
            &logical_host_dir,
            &out_dir_abs,
            &logical_out_dir,
            plan,
        )?;
    }
    if want_ohos {
        emit_ohos(
            &host_dir_abs,
            &logical_host_dir,
            &out_dir_abs,
            &logical_out_dir,
            plan,
            options.ohos_rs_dir.as_ref(),
        )?;
    }
    Ok(())
}

fn emit_wasm(
    host_dir: &Utf8Path,
    logical_host_dir: &Utf8Path,
    out_dir: &Utf8Path,
    plan: &HostCratePlan,
) -> Result<()> {
    let crate_dir = host_dir.join("wasm");
    let logical_crate_dir = logical_host_dir.join("wasm");
    let src_dir = crate_dir.join("src");
    let logical_src_dir = logical_crate_dir.join("src");
    fs::create_dir_all(&src_dir)?;

    let package_name = composite_host_package_name(&plan.meta.package_name);
    let lib_name = plan.wasm_artifact_stem();
    let component_dependencies = render_host_dependencies(plan, &logical_crate_dir)?;

    let cargo_toml = format!(
        "# AUTOGENERATED by uniffi_bindgen_javascript (host crate: wasm).\n\
         # Regenerate via `uniffi-bindgen generate --language javascript \\\n\
         #   --flavor wasm --emit-host-crates --manifest-path <root-package>/Cargo.toml`.\n\
         [package]\n\
         name = \"{package_name}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"{lib_name}\"\n\
         crate-type = [\"cdylib\", \"rlib\"]\n\
         \n\
         [dependencies]\n\
         {component_dependencies}\
         {uniffi_dep}\
         async-trait = \"0.1\"\n\
         wasm-bindgen = \"=0.2.117\"\n\
         wasm-bindgen-futures = \"0.4\"\n\
         js-sys = \"0.3\"\n\
         \n\
         [profile.release]\n\
         opt-level = \"z\"\n\
         lto = \"fat\"\n\
         codegen-units = 1\n\
         panic = \"abort\"\n\
         strip = true\n\
         debug = false\n\
         \n\
         [workspace]\n\
         resolver = \"3\"\n",
        package_name = package_name,
        lib_name = lib_name,
        component_dependencies = component_dependencies,
        uniffi_dep = render_uniffi_dependency(plan.meta.uniffi_dep.as_ref(), &logical_crate_dir)?,
    );
    fs::write(crate_dir.join("Cargo.toml"), cargo_toml)?;

    let mut lib_rs = String::from(
        "// AUTOGENERATED by uniffi_bindgen_javascript (host crate: wasm).\n\
         //\n\
         // Each `include!` below pastes the generator's per-component\n\
         // wasm-bindgen shim into this crate, so `cargo build --target\n\
         // wasm32-unknown-unknown` produces the final `cdylib`.\n\n",
    );
    for component in &plan.components {
        let crate_name = &component.identity.crate_name;
        let namespace = &component.identity.namespace;
        let rs_path = out_dir
            .join("components")
            .join(namespace)
            .join("browser")
            .join(format!("{crate_name}.rs"));
        let rel = relative_path(&logical_src_dir, &rs_path);
        lib_rs.push_str(&component_module_include(crate_name, &rel));
    }
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
}

fn emit_napi(
    host_dir: &Utf8Path,
    logical_host_dir: &Utf8Path,
    actual_out_dir: &Utf8Path,
    logical_out_dir: &Utf8Path,
    plan: &HostCratePlan,
) -> Result<()> {
    let crate_dir = host_dir.join("napi");
    let logical_crate_dir = logical_host_dir.join("napi");
    let src_dir = crate_dir.join("src");
    let logical_src_dir = logical_crate_dir.join("src");
    fs::create_dir_all(&src_dir)?;

    let package_name = composite_host_package_name(&plan.meta.package_name);
    let lib_name = plan.napi_artifact_stem();
    let component_dependencies = render_host_dependencies(plan, &logical_crate_dir)?;

    let cargo_toml = format!(
        "# AUTOGENERATED by uniffi_bindgen_javascript (host crate: napi).\n\
         # Regenerate via `uniffi-bindgen generate --language javascript \\\n\
         #   --flavor napi --emit-host-crates --manifest-path <root-package>/Cargo.toml`.\n\
         # Also reused by the electron consumption form — electron does\n\
         # NOT get its own Rust host crate.\n\
         [package]\n\
         name = \"{package_name}\"\n\
         version = \"0.0.0\"\n\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"{lib_name}\"\n\
         crate-type = [\"cdylib\"]\n\
         \n\
         [dependencies]\n\
         {component_dependencies}\
         {uniffi_dep}\
         async-trait = \"0.1\"\n\
         napi = {{ version = \"3.8.4\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }}\n\
         napi-derive = {{ version = \"3.5.3\", features = [\"type-def\"] }}\n\
         \n\
         [build-dependencies]\n\
         napi-build = \"2.3.1\"\n\
         \n\
         [workspace]\n\
         resolver = \"3\"\n",
        package_name = package_name,
        lib_name = lib_name,
        component_dependencies = component_dependencies,
        uniffi_dep = render_uniffi_dependency(plan.meta.uniffi_dep.as_ref(), &logical_crate_dir)?,
    );
    fs::write(crate_dir.join("Cargo.toml"), cargo_toml)?;

    let build_rs = "// AUTOGENERATED by uniffi_bindgen_javascript (host crate: napi).\n\
                    extern crate napi_build;\n\
                    fn main() {\n    napi_build::setup();\n}\n";
    fs::write(crate_dir.join("build.rs"), build_rs)?;

    let mut lib_rs = String::from(
        "// AUTOGENERATED by uniffi_bindgen_javascript (host crate: napi).\n\
         //\n\
         // Each `include!` below pastes the generator's per-component\n\
         // napi-rs bridge into this crate, so `cargo build` produces the\n\
         // final `.node` cdylib consumed by the generated `backend-napi.ts`.\n\n",
    );
    for component in &plan.components {
        let crate_name = &component.identity.crate_name;
        let namespace = &component.identity.namespace;
        let actual_node_rs_path = actual_out_dir
            .join("components")
            .join(namespace)
            .join("node")
            .join(format!("{crate_name}.rs"));
        let flavor = if actual_node_rs_path.exists() {
            "node"
        } else {
            "electron"
        };
        let rs_path = logical_out_dir
            .join("components")
            .join(namespace)
            .join(flavor)
            .join(format!("{crate_name}.rs"));
        let rel = relative_path(&logical_src_dir, &rs_path);
        lib_rs.push_str(&component_module_include(crate_name, &rel));
    }
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
}

fn render_host_dependencies(plan: &HostCratePlan, crate_dir: &Utf8Path) -> Result<String> {
    // The root package owns host `--features` selection even if it does not
    // own a selected component.  Root-as-component is deduplicated by exact
    // Cargo package ID and lib-target key, so its path dependency appears
    // once in every generated host manifest.
    let mut dependencies = vec![(&plan.root_dependency, false)];
    dependencies.extend(
        plan.components
            .iter()
            .map(|component| (&component.package, true)),
    );
    dependencies.sort_by(|(left, _), (right, _)| {
        (left.dependency_key.as_str(), left.package_id.to_string())
            .cmp(&(right.dependency_key.as_str(), right.package_id.to_string()))
    });

    let mut rendered = String::new();
    let mut rendered_keys = std::collections::BTreeMap::<String, String>::new();
    for (dependency, disable_default_features) in dependencies {
        match rendered_keys.entry(dependency.dependency_key.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(dependency.package_id.to_string());
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if *entry.get() == dependency.package_id.to_string() {
                    // Keep the root package's default features if a root
                    // component reuses it, and never render a duplicate key.
                    continue;
                }
                bail!(
                    "host-crate dependency key `{}` maps to both package {} and {}",
                    dependency.dependency_key,
                    entry.get(),
                    dependency.package_id
                );
            }
        }
        let rel = relative_path(crate_dir, &dependency.package_dir);
        let default_features = if disable_default_features {
            ", default-features = false"
        } else {
            ""
        };
        rendered.push_str(&format!(
            "{} = {{ package = {}, path = {}{} }}\n",
            dependency.dependency_key,
            toml_string_literal(&dependency.package_name),
            toml_string_literal(rel.as_str()),
            default_features,
        ));
    }
    if rendered.is_empty() {
        bail!("composite host plan has no direct dependencies");
    }
    Ok(rendered)
}

fn component_module_include(component_crate_name: &str, relative_bridge_path: &Utf8Path) -> String {
    let module_name = component_bridge_module_name(component_crate_name);
    format!("mod {module_name} {{\n    include!(\"{relative_bridge_path}\");\n}}\n")
}

fn emit_ohos(
    host_dir: &Utf8Path,
    logical_host_dir: &Utf8Path,
    actual_out_dir: &Utf8Path,
    logical_out_dir: &Utf8Path,
    plan: &HostCratePlan,
    ohos_rs_dir: Option<&Utf8PathBuf>,
) -> Result<()> {
    let crate_dir = host_dir.join("ohos");
    let logical_crate_dir = logical_host_dir.join("ohos");
    let src_dir = crate_dir.join("src");
    let logical_src_dir = logical_crate_dir.join("src");

    let package_name = plan.ohos_host_package_name();
    let ohos_deps = render_ohos_dependencies(ohos_rs_dir, &logical_crate_dir)?;
    let package_metadata = render_ohos_package_metadata(&plan.meta);
    let lib_name = plan.ohos_artifact_stem();
    let component_dependencies = render_host_dependencies(plan, &logical_crate_dir)?;
    let host_composite_identity = plan.ohos_composite_identity()?;

    let cargo_toml = format!(
        "# AUTOGENERATED by uniffi_bindgen_javascript (host crate: ohos).\n\
         # Regenerate via `uniffi-bindgen javascript build-ohos \\\n\
         #   --manifest-path <root-package>/Cargo.toml --out-dir <generated>`.\n\
         [package]\n\
         name = \"{package_name}\"\n\
         {package_metadata}\
         edition = \"2021\"\n\
         publish = false\n\
         \n\
         [lib]\n\
         name = \"{lib_name}\"\n\
         crate-type = [\"cdylib\"]\n\
         \n\
         [dependencies]\n\
         {component_dependencies}\
         {uniffi_dep}\
         async-trait = \"0.1\"\n\
         {ohos_deps}\
         \n\
         [workspace]\n\
         resolver = \"3\"\n",
        package_name = package_name,
        package_metadata = package_metadata,
        lib_name = lib_name,
        component_dependencies = component_dependencies,
        uniffi_dep = render_uniffi_dependency(plan.meta.uniffi_dep.as_ref(), &logical_crate_dir)?,
        ohos_deps = ohos_deps,
    );
    let bundle_text = render_ohos_facade_bundle(
        actual_out_dir,
        &plan.components,
        &package_name,
        &lib_name,
        &host_composite_identity,
    )?;

    let build_rs = r#"// AUTOGENERATED by uniffi_bindgen_javascript (host crate: ohos).
extern crate napi_build_ohos;
fn main() {
    napi_build_ohos::setup();
    println!("cargo:rustc-link-arg=-Wl,--wrap=napi_add_env_cleanup_hook");
    println!("cargo:rustc-link-arg=-Wl,--wrap=napi_remove_env_cleanup_hook");
}
"#;
    let mut lib_rs = String::from(
        r#"// AUTOGENERATED by uniffi_bindgen_javascript (host crate: ohos).
//
// Each `include!` below pastes the generator's per-component NAPI-OHOS bridge
// into this crate. UniFFI's built-in OHOS builder invokes Cargo to produce the
// final Harmony/OpenHarmony `lib*.so` cdylib; no `ohrs` executable is needed.

// HarmonyOS indexes environment cleanup hooks by the `arg` pointer alone.
// napi-ohos uses a null key for its Tokio runtime hook, which collides with
// another native module doing the same. The linker wrappers substitute stable
// callback-specific keys owned by this cdylib and preserve them for removal.
mod __uniffi_napi_cleanup_hook_key {
    use napi_ohos::sys::{napi_env, napi_status};
    use std::collections::BTreeMap;
    use std::ffi::c_void;
    use std::sync::{Mutex, OnceLock};

    type CleanupHook = Option<unsafe extern "C" fn(*mut c_void)>;

    // The boxes never move or get removed, so their addresses are stable for
    // the lifetime of this cdylib.  Keying the map by the callback pointer
    // gives different null-arg callbacks different keys while the per-cdylib
    // static keeps otherwise identical callbacks in different DSOs distinct.
    static CLEANUP_HOOK_KEYS: OnceLock<Mutex<BTreeMap<usize, Box<u8>>>> = OnceLock::new();

    // `--wrap` rewrites calls to these definitions. Mark only the two wrapper
    // symbols protected so the dynamic linker cannot interpose a main-program
    // or earlier-DSO definition on calls originating in this cdylib.
    core::arch::global_asm!(
        ".protected __wrap_napi_add_env_cleanup_hook",
        ".protected __wrap_napi_remove_env_cleanup_hook",
    );

    unsafe extern "C" {
        fn __real_napi_add_env_cleanup_hook(
            env: napi_env,
            fun: CleanupHook,
            arg: *mut c_void,
        ) -> napi_status;
        fn __real_napi_remove_env_cleanup_hook(
            env: napi_env,
            fun: CleanupHook,
            arg: *mut c_void,
        ) -> napi_status;
    }

    fn unique_arg(fun: CleanupHook, arg: *mut c_void) -> *mut c_void {
        if !arg.is_null() {
            return arg;
        }

        let callback = fun.map_or(0, |callback| callback as *const () as usize);
        let keys = CLEANUP_HOOK_KEYS.get_or_init(|| Mutex::new(BTreeMap::new()));
        let mut keys = keys.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = keys.entry(callback).or_insert_with(|| Box::new(0));
        core::ptr::from_mut(key.as_mut()).cast()
    }

    #[no_mangle]
    unsafe extern "C" fn __wrap_napi_add_env_cleanup_hook(
        env: napi_env,
        fun: CleanupHook,
        arg: *mut c_void,
    ) -> napi_status {
        unsafe { __real_napi_add_env_cleanup_hook(env, fun, unique_arg(fun, arg)) }
    }

    #[no_mangle]
    unsafe extern "C" fn __wrap_napi_remove_env_cleanup_hook(
        env: napi_env,
        fun: CleanupHook,
        arg: *mut c_void,
    ) -> napi_status {
        unsafe { __real_napi_remove_env_cleanup_hook(env, fun, unique_arg(fun, arg)) }
    }
}

"#,
    );
    for component in &plan.components {
        let crate_name = &component.identity.crate_name;
        let namespace = &component.identity.namespace;
        let rs_path = logical_out_dir
            .join("components")
            .join(namespace)
            .join("harmony")
            .join(format!("{crate_name}.rs"));
        let rel = relative_path(&logical_src_dir, &rs_path);
        lib_rs.push_str(&component_module_include(crate_name, &rel));
    }
    // No output-tree mutation happens before the facade bundle, contract,
    // sidecar, Rust host source, and every host manifest fragment are ready.
    fs::create_dir_all(&src_dir)?;
    fs::write(crate_dir.join("Cargo.toml"), cargo_toml)?;
    fs::write(
        crate_dir.join("uniffi-ohos-facade-bundle.json"),
        bundle_text,
    )?;
    fs::write(crate_dir.join("build.rs"), build_rs)?;
    fs::write(src_dir.join("lib.rs"), lib_rs)?;
    Ok(())
}

fn preflight_ohos_host_emission(
    logical_host_dir: &Utf8Path,
    actual_out_dir: &Utf8Path,
    logical_out_dir: &Utf8Path,
    plan: &HostCratePlan,
    ohos_rs_dir: Option<&Utf8PathBuf>,
) -> Result<()> {
    let logical_crate_dir = logical_host_dir.join("ohos");
    let package_name = plan.ohos_host_package_name();
    let lib_name = plan.ohos_artifact_stem();
    let host_composite_identity = plan.ohos_composite_identity()?;

    // These are the only fallible render steps outside the facade bundle.
    // Execute them now, before `emit` creates even the top-level host dir.
    let _ = render_ohos_dependencies(ohos_rs_dir, &logical_crate_dir)?;
    let _ = render_uniffi_dependency(plan.meta.uniffi_dep.as_ref(), &logical_crate_dir)?;
    let _ = render_host_dependencies(plan, &logical_crate_dir)?;
    for component in &plan.components {
        let crate_name = &component.identity.crate_name;
        let namespace = &component.identity.namespace;
        let rs_path = logical_out_dir
            .join("components")
            .join(namespace)
            .join("harmony")
            .join(format!("{crate_name}.rs"));
        let _ = relative_path(&logical_crate_dir.join("src"), &rs_path);
    }
    let _ = render_ohos_facade_bundle(
        actual_out_dir,
        &plan.components,
        &package_name,
        &lib_name,
        &host_composite_identity,
    )?;
    Ok(())
}

/// Render the complete invocation bundle from exact generated inputs.  This
/// remains pure so callers can validate it before creating any host tree.
fn render_ohos_facade_bundle(
    actual_out_dir: &Utf8Path,
    planned_components: &[PlannedComponentDependency],
    package_name: &str,
    lib_name: &str,
    host_composite_identity: &str,
) -> Result<String> {
    let mut contracts = Vec::new();
    let mut type_sidecars = Vec::new();
    let mut components = Vec::new();
    for planned in planned_components {
        let crate_name = &planned.identity.crate_name;
        let expected_namespace = &planned.identity.namespace;
        let contract_file = format!("{crate_name}.ohos-facade.json");
        let contract_path = actual_out_dir
            .join("components")
            .join(expected_namespace)
            .join("harmony")
            .join(&contract_file);
        let contract_content = fs::read_to_string(&contract_path)
            .with_context(|| format!("reading generated OHOS facade contract {contract_path}"))?;
        let contract = crate::flavors::napi::parse_ohos_facade_contract(&contract_content)
            .with_context(|| {
                format!("validating generated OHOS facade contract {contract_path}")
            })?;
        let (component, namespace, native_export_prefix) =
            crate::flavors::napi::facade_contract_identity(&contract);
        if component != crate_name
            || namespace != expected_namespace
            || native_export_prefix != planned.identity.native_export_prefix
        {
            bail!(
                "generated OHOS facade contract {contract_path} does not match its selected component identity"
            );
        }
        let contract_digest = sha256_text(&contract_content);
        let identity_export = crate::flavors::napi::ohos_bridge_identity_export_for_prefix(
            &native_export_prefix,
            &contract_digest,
        );
        let sidecar_file = format!("{crate_name}.ohos-extra-types.d.ts");
        let sidecar_path = actual_out_dir
            .join("components")
            .join(expected_namespace)
            .join("harmony")
            .join(&sidecar_file);
        let sidecar_content = fs::read_to_string(&sidecar_path)
            .with_context(|| format!("reading generated OHOS type sidecar {sidecar_path}"))?;
        crate::flavors::napi::validate_ohos_extra_types(
            &sidecar_content,
            &identity_export,
            &contract,
        )
        .with_context(|| format!("validating generated OHOS type sidecar {sidecar_path}"))?;

        contracts.push(serde_json::json!({
            "file": contract_file.clone(),
            "sha256": contract_digest,
            "content": contract_content,
        }));
        components.push(serde_json::json!({
            "component": component,
            "namespace": namespace,
            "nativeExportPrefix": native_export_prefix,
            "contractFile": contract_file,
            "contractSha256": contract_digest,
            "identityExport": identity_export,
        }));
        type_sidecars.push(serde_json::json!({
            "file": sidecar_file,
            "sha256": sha256_text(&sidecar_content),
            "content": sidecar_content,
        }));
    }
    contracts.sort_by(|left, right| left["file"].as_str().cmp(&right["file"].as_str()));
    type_sidecars.sort_by(|left, right| left["file"].as_str().cmp(&right["file"].as_str()));
    components.sort_by(|left, right| {
        (
            left["component"].as_str(),
            left["namespace"].as_str(),
            left["nativeExportPrefix"].as_str(),
        )
            .cmp(&(
                right["component"].as_str(),
                right["namespace"].as_str(),
                right["nativeExportPrefix"].as_str(),
            ))
    });
    let payload = serde_json::json!({
        "packageName": package_name,
        "libTarget": lib_name,
        "hostCompositeIdentity": host_composite_identity,
        "components": components,
        "contracts": contracts,
        "typeSidecars": type_sidecars,
    });
    let payload_bytes = serde_json::to_vec(&payload)?;
    let bundle = serde_json::json!({
        "hostBundleSchemaVersion": HOST_BUNDLE_SCHEMA_VERSION,
        "fingerprint": sha256_bytes(&payload_bytes),
        "packageName": payload["packageName"],
        "libTarget": payload["libTarget"],
        "hostCompositeIdentity": payload["hostCompositeIdentity"],
        "components": payload["components"],
        "contracts": payload["contracts"],
        "typeSidecars": payload["typeSidecars"],
    });
    let mut bundle_text = serde_json::to_string_pretty(&bundle)?;
    bundle_text.push('\n');
    Ok(bundle_text)
}

fn sha256_text(value: &str) -> String {
    sha256_bytes(value.as_bytes())
}

fn sha256_bytes(value: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(value);
    format!("{:x}", digest.finalize())
}

fn render_ohos_package_metadata(meta: &CoreCrateMetadata) -> String {
    let mut out = format!("version = {}\n", toml_string_literal(&meta.package_version));
    if let Some(description) = &meta.description {
        out.push_str(&format!(
            "description = {}\n",
            toml_string_literal(description)
        ));
    }
    if !meta.authors.is_empty() {
        let authors = meta
            .authors
            .iter()
            .map(|author| toml_string_literal(author))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("authors = [{authors}]\n"));
    }
    if let Some(license) = &meta.license {
        out.push_str(&format!("license = {}\n", toml_string_literal(license)));
    }
    out
}

fn toml_string_literal(value: &str) -> String {
    use std::fmt::Write as _;

    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '\"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                let _ = write!(out, "\\u{:04X}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('\"');
    out
}

fn render_ohos_dependencies(
    ohos_rs_dir: Option<&Utf8PathBuf>,
    crate_dir: &Utf8Path,
) -> Result<String> {
    if let Some(ohos_rs_dir) = ohos_rs_dir {
        let abs = ohos_rs_dir
            .canonicalize_utf8()
            .unwrap_or_else(|_| ohos_rs_dir.clone());
        let napi = relative_path(crate_dir, &abs.join("crates/napi"));
        let derive = relative_path(crate_dir, &abs.join("crates/macro"));
        let build = relative_path(crate_dir, &abs.join("crates/build"));
        return Ok(format!(
            "napi-ohos = {{ path = \"{napi}\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }}\n\
             napi-derive-ohos = {{ path = \"{derive}\", default-features = false, features = [\"strict\", \"type-def\"] }}\n\
             \n\
             [build-dependencies]\n\
             napi-build-ohos = {{ path = \"{build}\" }}\n"
        ));
    }
    Ok(
        "napi-ohos = { version = \"1.1.6\", default-features = false, features = [\"napi8\", \"tokio_rt\"] }\n\
         napi-derive-ohos = { version = \"1.1.6\", default-features = false, features = [\"strict\", \"type-def\"] }\n\
         \n\
         [build-dependencies]\n\
         napi-build-ohos = \"1.1.6\"\n"
            .to_string(),
    )
}

/// Compute a relative path from `from_dir` (a directory) to `to` (any
/// path). Both inputs should already be canonicalized (absolute) so the
/// result has no surprises from `.` / `..` components in the original.
fn relative_path(from_dir: &Utf8Path, to: &Utf8Path) -> Utf8PathBuf {
    let from: Vec<&str> = from_dir.components().map(|c| c.as_str()).collect();
    let to_vec: Vec<&str> = to.components().map(|c| c.as_str()).collect();
    let mut i = 0;
    while i < from.len() && i < to_vec.len() && from[i] == to_vec[i] {
        i += 1;
    }
    let mut result = Utf8PathBuf::new();
    for _ in i..from.len() {
        result.push("..");
    }
    for c in &to_vec[i..] {
        result.push(c);
    }
    if result.as_str().is_empty() {
        result.push(".");
    }
    result
}

/// Resolve a not-yet-created output path without creating it.  Canonicalizing
/// the nearest existing ancestor preserves macOS `/var` → `/private/var`
/// aliases, so logical include paths stay identical to the historical
/// create-then-canonicalize behavior while preflight remains side-effect free.
fn canonicalize_planned_path(path: &Utf8Path) -> Result<Utf8PathBuf> {
    let mut missing = Vec::new();
    let mut ancestor = path;
    loop {
        if let Ok(mut canonical) = ancestor.canonicalize_utf8() {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Ok(canonical);
        }
        let component = ancestor
            .file_name()
            .with_context(|| format!("no existing ancestor for planned path {path}"))?;
        missing.push(component.to_string());
        ancestor = ancestor
            .parent()
            .with_context(|| format!("no existing ancestor for planned path {path}"))?;
    }
}

fn resolve_uniffi_dependency(
    manifest_path: &Utf8Path,
    value: &toml::Value,
) -> Result<Option<UniffiDependency>> {
    let manifest_dir = manifest_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| Utf8PathBuf::from("."));
    let Some(dep) = value
        .get("dependencies")
        .and_then(|deps| deps.get("uniffi"))
    else {
        return Ok(None);
    };
    if dep
        .get("workspace")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return resolve_workspace_uniffi_dependency(&manifest_dir).map(Some);
    }
    parse_uniffi_dependency(dep, &manifest_dir).map(Some)
}

fn resolve_workspace_uniffi_dependency(start_dir: &Utf8Path) -> Result<UniffiDependency> {
    let mut current = Some(start_dir);
    while let Some(dir) = current {
        let manifest = dir.join("Cargo.toml");
        if manifest.exists() {
            let text = fs::read_to_string(&manifest)
                .with_context(|| format!("reading workspace manifest at {manifest}"))?;
            let value: toml::Value =
                toml::from_str(&text).with_context(|| format!("parsing {manifest}"))?;
            if let Some(dep) = value
                .get("workspace")
                .and_then(|ws| ws.get("dependencies"))
                .and_then(|deps| deps.get("uniffi"))
            {
                return parse_uniffi_dependency(dep, dir);
            }
        }
        current = dir.parent();
    }
    bail!("could not resolve workspace dependency for `uniffi` starting from {start_dir}")
}

fn parse_uniffi_dependency(dep: &toml::Value, base_dir: &Utf8Path) -> Result<UniffiDependency> {
    match dep {
        toml::Value::String(version) => Ok(UniffiDependency {
            base_dir: base_dir.to_path_buf(),
            version: Some(version.clone()),
            path: None,
            git: None,
            branch: None,
            tag: None,
            rev: None,
            package: None,
            default_features: None,
            features: Vec::new(),
        }),
        toml::Value::Table(table) => Ok(UniffiDependency {
            base_dir: base_dir.to_path_buf(),
            version: table
                .get("version")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            path: table
                .get("path")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            git: table
                .get("git")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            branch: table
                .get("branch")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            tag: table
                .get("tag")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            rev: table
                .get("rev")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            package: table
                .get("package")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            default_features: table.get("default-features").and_then(|v| v.as_bool()),
            features: table
                .get("features")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        }),
        _ => bail!("unsupported `uniffi` dependency form in Cargo.toml"),
    }
}

fn render_uniffi_dependency(
    dep: Option<&UniffiDependency>,
    crate_dir: &Utf8Path,
) -> Result<String> {
    let Some(dep) = dep else {
        return Ok(String::new());
    };
    let mut fields: Vec<(String, String)> = Vec::new();
    if let Some(path) = &dep.path {
        let abs = dep
            .base_dir
            .join(path)
            .canonicalize_utf8()
            .unwrap_or_else(|_| dep.base_dir.join(path));
        let rel = relative_path(crate_dir, &abs);
        fields.push(("path".into(), format!("\"{rel}\"")));
    }
    if let Some(version) = &dep.version {
        fields.push(("version".into(), format!("\"{version}\"")));
    }
    if let Some(git) = &dep.git {
        fields.push(("git".into(), format!("\"{git}\"")));
    }
    if let Some(branch) = &dep.branch {
        fields.push(("branch".into(), format!("\"{branch}\"")));
    }
    if let Some(tag) = &dep.tag {
        fields.push(("tag".into(), format!("\"{tag}\"")));
    }
    if let Some(rev) = &dep.rev {
        fields.push(("rev".into(), format!("\"{rev}\"")));
    }
    if let Some(package) = &dep.package {
        fields.push(("package".into(), format!("\"{package}\"")));
    }
    if let Some(default_features) = dep.default_features {
        fields.push(("default-features".into(), default_features.to_string()));
    }
    if !dep.features.is_empty() {
        let features = dep
            .features
            .iter()
            .map(|feature| format!("\"{feature}\""))
            .collect::<Vec<_>>()
            .join(", ");
        fields.push(("features".into(), format!("[{features}]")));
    }
    if fields.is_empty() {
        bail!("resolved `uniffi` dependency has no renderable fields");
    }
    let fields = fields
        .into_iter()
        .map(|(key, value)| format!("{key} = {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    Ok(format!("uniffi = {{ {fields} }}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "uniffi-js-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn write_minimal_lib(crate_dir: &std::path::Path) {
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
    }

    fn single_component_plan(
        meta: &CoreCrateMetadata,
        crate_name: &str,
        namespace: &str,
    ) -> HostCratePlan {
        let identity = HostComponentIdentity {
            crate_name: crate_name.to_string(),
            namespace: namespace.to_string(),
            native_export_prefix: uniffi_bindgen::interface::native_export_prefix_for_component(
                crate_name,
            ),
        };
        let dependency = PlannedPackageDependency {
            dependency_key: rust_crate_key(crate_name),
            package_id: PackageId {
                repr: format!("fixture {}", meta.package_name),
            },
            package_name: meta.package_name.clone(),
            package_dir: meta.crate_dir.clone(),
        };
        HostCratePlan {
            meta: meta.clone(),
            root_dependency: dependency.clone(),
            components: vec![PlannedComponentDependency {
                package: dependency,
                identity: identity.clone(),
            }],
        }
    }

    fn fixture_dependency(key: &str, package_name: &str) -> PlannedPackageDependency {
        PlannedPackageDependency {
            dependency_key: key.to_string(),
            package_id: PackageId {
                repr: format!("fixture {package_name} {key}"),
            },
            package_name: package_name.to_string(),
            package_dir: Utf8PathBuf::from(format!("/fixture/{package_name}")),
        }
    }

    fn fixture_component(key: &str, package_name: &str) -> PlannedComponentDependency {
        PlannedComponentDependency {
            identity: HostComponentIdentity {
                crate_name: key.to_string(),
                namespace: "fixture".to_string(),
                native_export_prefix: uniffi_bindgen::interface::native_export_prefix_for_component(
                    key,
                ),
            },
            package: fixture_dependency(key, package_name),
        }
    }

    #[test]
    fn fixed_runtime_dependency_keys_are_scoped_to_requested_host_flavors() {
        let root = fixture_dependency("root_bridge", "root-package");
        assert!(validate_fixed_runtime_dependency_keys(
            &root,
            &[fixture_component("napi", "component-package")],
            true,
            false,
            false,
        )
        .is_ok());

        let napi_error = validate_fixed_runtime_dependency_keys(
            &root,
            &[fixture_component("napi_build", "component-package")],
            false,
            true,
            false,
        )
        .unwrap_err();
        assert!(format!("{napi_error:#}").contains("napi_build"));

        let ohos_error = validate_fixed_runtime_dependency_keys(
            &root,
            &[fixture_component("napi_build_ohos", "component-package")],
            false,
            false,
            true,
        )
        .unwrap_err();
        assert!(format!("{ohos_error:#}").contains("napi_build_ohos"));

        let host_target = composite_host_lib_target("root-package");
        let host_target_error = validate_fixed_runtime_dependency_keys(
            &root,
            &[fixture_component(&host_target, "component-package")],
            false,
            false,
            false,
        )
        .unwrap_err();
        assert!(format!("{host_target_error:#}").contains(&host_target));
    }

    #[test]
    fn plan_resolves_feature_gated_aliases_and_reuses_the_root_dependency_once() {
        let root = test_root("host-plan-alias");
        let core = root.join("core");
        let component = root.join("component");
        write_minimal_lib(&core);
        write_minimal_lib(&component);
        std::fs::write(
            component.join("Cargo.toml"),
            r#"[package]
name = "component-package"
version = "0.1.0"
edition = "2021"

[lib]
name = "component_bridge"
"#,
        )
        .unwrap();
        std::fs::write(
            core.join("Cargo.toml"),
            r#"[package]
name = "root-package"
version = "0.1.0"
edition = "2021"

[lib]
name = "root_bridge"

[features]
host-gate = ["dep:component_source_alias"]

[dependencies]
component_source_alias = { package = "component-package", path = "../component", optional = true }
"#,
        )
        .unwrap();
        let core = Utf8PathBuf::from_path_buf(core).unwrap();
        let host_output = Utf8PathBuf::from_path_buf(root.join("host-must-not-exist")).unwrap();
        let options = HostCrateOptions {
            manifest_path: core.join("Cargo.toml"),
            host_crates_dir: host_output.clone(),
            logical_host_crates_dir: None,
            logical_out_dir: None,
            ohos_rs_dir: None,
        };
        let identities = vec![
            HostComponentIdentity {
                crate_name: "component_bridge".to_string(),
                namespace: "component".to_string(),
                native_export_prefix: uniffi_bindgen::interface::native_export_prefix_for_component(
                    "component_bridge",
                ),
            },
            HostComponentIdentity {
                crate_name: "root_bridge".to_string(),
                namespace: "root".to_string(),
                native_export_prefix: uniffi_bindgen::interface::native_export_prefix_for_component(
                    "root_bridge",
                ),
            },
        ];
        let plan = plan(&options, &identities, true, true, true).unwrap();
        assert!(
            !host_output.exists(),
            "read-only host planning must not create the configured host output"
        );
        let dependencies = render_host_dependencies(&plan, Utf8Path::new("/fixture/host")).unwrap();
        let parsed: toml::Value =
            toml::from_str(&format!("[dependencies]\n{dependencies}")).unwrap();
        assert_eq!(
            parsed["dependencies"]["root_bridge"]["package"].as_str(),
            Some("root-package")
        );
        assert!(
            parsed["dependencies"]["root_bridge"]
                .get("default-features")
                .is_none(),
            "the root feature owner must retain its normal default-feature behavior"
        );
        assert_eq!(
            parsed["dependencies"]["component_bridge"]["package"].as_str(),
            Some("component-package")
        );
        assert_eq!(
            parsed["dependencies"]["component_bridge"]["default-features"].as_bool(),
            Some(false)
        );
        assert!(!dependencies.contains("component_source_alias"));
        assert_eq!(dependencies.matches("root_bridge =").count(), 1);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn all_javascript_flavors_share_one_logical_composite_identity() {
        let forward = vec![
            (
                "alpha_core".to_string(),
                "alpha".to_string(),
                "ffi_alpha_core".to_string(),
            ),
            (
                "beta_core".to_string(),
                "beta".to_string(),
                "ffi_beta_core".to_string(),
            ),
        ];
        let mut reverse = forward.clone();
        reverse.reverse();
        let package = composite_host_package_name("composite-core");
        let lib_target = composite_host_lib_target("composite-core");
        let forward_identity = composite_host_identity(&package, &lib_target, &forward).unwrap();
        let reverse_identity = composite_host_identity(&package, &lib_target, &reverse).unwrap();

        assert_eq!(forward_identity, reverse_identity);
        assert_eq!(package, "composite-core-uniffi-js-host");
        assert_eq!(lib_target, "composite_core_uniffi_js_host");
        // The digest uses the same tuple even for invocations that never
        // request Harmony; package manifests must not borrow an OHOS-only
        // package/lib identity for a NAPI/Wasm-only build.
        assert_eq!(
            composite_host_identity(
                &composite_host_package_name("composite-core"),
                &composite_host_lib_target("composite-core"),
                &forward,
            )
            .unwrap(),
            forward_identity
        );
    }

    #[test]
    fn carries_core_cargo_metadata_into_ohos_host_manifest() {
        let root = test_root("host-meta");
        std::fs::create_dir_all(&root).unwrap();
        write_minimal_lib(&root);
        let manifest = root.join("Cargo.toml");
        std::fs::write(
            &manifest,
            r#"[package]
name = "demo-core"
version = "1.2.3"
description = "quoted \"metadata\""
authors = ["First Author <first@example.com>", "Second Author"]
license = "MPL-2.0"
edition = "2021"
"#,
        )
        .unwrap();
        let manifest = Utf8PathBuf::from_path_buf(manifest).unwrap();
        let metadata = load_metadata(&manifest).unwrap();
        assert_eq!(metadata.package_name, "demo-core");
        assert_eq!(metadata.package_version, "1.2.3");
        assert_eq!(metadata.description.as_deref(), Some("quoted \"metadata\""));
        assert_eq!(
            metadata.authors,
            vec!["First Author <first@example.com>", "Second Author"]
        );
        assert_eq!(metadata.license.as_deref(), Some("MPL-2.0"));

        let rendered = format!(
            "[package]\nname = \"demo-core-ohos\"\n{}edition = \"2021\"\n",
            render_ohos_package_metadata(&metadata)
        );
        let parsed: toml::Value = toml::from_str(&rendered).unwrap();
        assert_eq!(parsed["package"]["version"].as_str(), Some("1.2.3"));
        assert_eq!(
            parsed["package"]["description"].as_str(),
            Some("quoted \"metadata\"")
        );
        assert_eq!(parsed["package"]["authors"].as_array().unwrap().len(), 2);
        assert_eq!(parsed["package"]["license"].as_str(), Some("MPL-2.0"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn resolves_workspace_inherited_metadata_into_real_ohos_host_manifest() {
        let root = test_root("host-workspace-meta");
        let core = root.join("core");
        let out = root.join("generated");
        let host = root.join("host");
        write_minimal_lib(&core);
        let harmony = out.join("components/demo_core/harmony");
        std::fs::create_dir_all(&harmony).unwrap();
        std::fs::write(harmony.join("demo_core.rs"), "").unwrap();
        for (name, body) in [
            ("native-facade.ets", "export default {};\n"),
            ("Index.ets", "export default {};\n"),
            (
                "demo_core.ohos-extra-types.d.ts",
                "type_def:{\"kind\":\"fn\",\"name\":\"uniffiohosbridgeidentityfixture\",\"def\":\"function uniffiohosbridgeidentityfixture(): string\",\"typeParameters\":[]}\n",
            ),
            (
                "demo_core.ohos-facade.json",
                "{\"facadeContractSchemaVersion\":4,\"component\":\"demo_core\",\"namespace\":\"demo_core\",\"nativeExportPrefix\":\"ffi_demo_core\",\"outputStreams\":[],\"inputStreams\":[]}",
            ),
        ] {
            std::fs::write(harmony.join(name), body).unwrap();
        }
        let contract_content =
            std::fs::read_to_string(harmony.join("demo_core.ohos-facade.json")).unwrap();
        let identity_export = crate::flavors::napi::ohos_bridge_identity_export_for_prefix(
            "ffi_demo_core",
            &sha256_text(&contract_content),
        );
        std::fs::write(
            harmony.join("demo_core.ohos-extra-types.d.ts"),
            format!(
                "type_def:{{\"kind\":\"fn\",\"name\":\"{identity_export}\",\"def\":\"function {identity_export}(): string\",\"typeParameters\":[]}}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["core"]
resolver = "2"

[workspace.package]
version = "4.5.6"
description = "inherited description"
authors = ["Workspace Author <author@example.com>"]
license = "Apache-2.0"
edition = "2021"
"#,
        )
        .unwrap();
        std::fs::write(
            core.join("Cargo.toml"),
            r#"[package]
name = "demo-core"
version.workspace = true
description.workspace = true
authors.workspace = true
license.workspace = true
edition.workspace = true
"#,
        )
        .unwrap();

        let manifest = Utf8PathBuf::from_path_buf(core.join("Cargo.toml")).unwrap();
        let metadata = load_metadata(&manifest).unwrap();
        assert_eq!(metadata.package_version, "4.5.6");
        assert_eq!(
            metadata.description.as_deref(),
            Some("inherited description")
        );
        assert_eq!(metadata.authors, ["Workspace Author <author@example.com>"]);
        assert_eq!(metadata.license.as_deref(), Some("Apache-2.0"));

        let host = Utf8PathBuf::from_path_buf(host).unwrap();
        let out = Utf8PathBuf::from_path_buf(out).unwrap();
        let logical_host = Utf8PathBuf::from_path_buf(root.join("published/host")).unwrap();
        let logical_out = Utf8PathBuf::from_path_buf(root.join("published/generated")).unwrap();
        let plan = single_component_plan(&metadata, "demo_core", "demo_core");
        std::fs::create_dir_all(&host).unwrap();
        emit_ohos(&host, &logical_host, &out, &logical_out, &plan, None).unwrap();
        let generated: toml::Value =
            toml::from_str(&std::fs::read_to_string(host.join("ohos/Cargo.toml")).unwrap())
                .unwrap();
        let cargo_toml = std::fs::read_to_string(host.join("ohos/Cargo.toml")).unwrap();
        let build_rs = std::fs::read_to_string(host.join("ohos/build.rs")).unwrap();
        let lib_rs = std::fs::read_to_string(host.join("ohos/src/lib.rs")).unwrap();
        assert!(!build_rs.contains("demo_core.ohos-facade.json"));
        assert!(!build_rs.contains("uniffi-ohos-facade-inventory.json"));
        assert!(!build_rs.contains("let _ = std::fs::write"));
        assert!(build_rs.contains("--wrap=napi_add_env_cleanup_hook"));
        assert!(build_rs.contains("--wrap=napi_remove_env_cleanup_hook"));
        assert!(
            lib_rs.contains("static CLEANUP_HOOK_KEYS: OnceLock<Mutex<BTreeMap<usize, Box<u8>>>>")
        );
        assert!(lib_rs.contains(".protected __wrap_napi_add_env_cleanup_hook"));
        assert!(lib_rs.contains(".protected __wrap_napi_remove_env_cleanup_hook"));
        assert!(lib_rs.contains("__wrap_napi_add_env_cleanup_hook"));
        assert!(lib_rs.contains("__wrap_napi_remove_env_cleanup_hook"));
        assert!(lib_rs.contains("unique_arg(fun, arg)"));
        assert!(
            lib_rs.contains(
                "include!(\"../../../generated/components/demo_core/harmony/demo_core.rs\");"
            ),
            "logical publication paths were not used for include!: {lib_rs}"
        );
        let bundle_path = host.join("ohos/uniffi-ohos-facade-bundle.json");
        let bundle: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&bundle_path).unwrap()).unwrap();
        assert_eq!(bundle["hostBundleSchemaVersion"], 3);
        assert_eq!(bundle["packageName"], "demo-core-uniffi-js-host");
        assert_eq!(bundle["libTarget"], "demo_core_uniffi_js_host");
        let recomputed_identity = composite_host_identity(
            bundle["packageName"].as_str().unwrap(),
            bundle["libTarget"].as_str().unwrap(),
            &[(
                "demo_core".to_string(),
                "demo_core".to_string(),
                "ffi_demo_core".to_string(),
            )],
        )
        .unwrap();
        assert_eq!(
            bundle["hostCompositeIdentity"].as_str(),
            Some(recomputed_identity.as_str()),
            "OHOS bundle must use the canonical host identity algorithm"
        );
        assert_eq!(bundle["components"][0]["component"], "demo_core");
        assert_eq!(bundle["components"][0]["namespace"], "demo_core");
        assert_eq!(
            bundle["components"][0]["nativeExportPrefix"],
            "ffi_demo_core"
        );
        assert!(bundle["components"][0]["identityExport"]
            .as_str()
            .unwrap()
            .starts_with("ffi_demo_core_uniffiohosbridgeidentity"));
        assert_eq!(bundle["contracts"][0]["file"], "demo_core.ohos-facade.json");
        assert_eq!(
            bundle["typeSidecars"][0]["file"],
            "demo_core.ohos-extra-types.d.ts"
        );
        assert_eq!(bundle["fingerprint"].as_str().unwrap().len(), 64);
        assert!(
            cargo_toml.contains(
                "napi-derive-ohos = { version = \"1.1.6\", default-features = false, features = [\"strict\", \"type-def\"] }"
            ),
            "OHOS host must retain strict and explicitly enable the upstream type-def compatibility feature:\n{cargo_toml}"
        );
        assert!(
            !cargo_toml.contains("napi-derive-backend-ohos"),
            "OHOS host must not add an independent type-definition backend:\n{cargo_toml}"
        );
        assert!(
            cargo_toml.contains("features = [\"strict\", \"type-def\"]"),
            "OHOS host must use the upstream compile-only compatibility feature:\n{cargo_toml}"
        );
        assert_eq!(generated["package"]["version"].as_str(), Some("4.5.6"));
        assert_eq!(
            generated["package"]["description"].as_str(),
            Some("inherited description")
        );
        assert_eq!(generated["package"]["authors"].as_array().unwrap().len(), 1);
        assert_eq!(generated["package"]["license"].as_str(), Some("Apache-2.0"));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn malformed_ohos_sidecar_is_rejected_before_any_host_flavor_is_written() {
        let root = test_root("ohos-preflight-no-write");
        let core = root.join("core");
        let generated = root.join("generated");
        let host = root.join("host-must-not-exist");
        write_minimal_lib(&core);
        let harmony = generated.join("components/demo_core/harmony");
        std::fs::create_dir_all(&harmony).unwrap();
        std::fs::write(harmony.join("demo_core.rs"), "").unwrap();
        std::fs::write(
            harmony.join("demo_core.ohos-facade.json"),
            "{\"facadeContractSchemaVersion\":4,\"component\":\"demo_core\",\"namespace\":\"demo_core\",\"nativeExportPrefix\":\"ffi_demo_core\",\"outputStreams\":[],\"inputStreams\":[]}",
        )
        .unwrap();
        std::fs::write(
            harmony.join("demo_core.ohos-extra-types.d.ts"),
            "not canonical OHOS type metadata\n",
        )
        .unwrap();

        let core = Utf8PathBuf::from_path_buf(core).unwrap();
        let generated = Utf8PathBuf::from_path_buf(generated).unwrap();
        let host = Utf8PathBuf::from_path_buf(host).unwrap();
        let options = HostCrateOptions {
            manifest_path: core.join("Cargo.toml"),
            host_crates_dir: host.clone(),
            logical_host_crates_dir: None,
            logical_out_dir: None,
            ohos_rs_dir: None,
        };
        let metadata = CoreCrateMetadata {
            package_name: "demo-core".into(),
            package_version: "1.0.0".into(),
            description: None,
            authors: Vec::new(),
            license: None,
            crate_dir: core,
            uniffi_dep: None,
        };
        let plan = single_component_plan(&metadata, "demo_core", "demo_core");
        let error = emit(&options, &generated, &plan, true, true, true).unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("type_def:"), "{error}");
        assert!(
            !host.exists(),
            "OHOS preflight created host output before rejecting malformed sidecar"
        );
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn leaves_missing_optional_cargo_metadata_absent() {
        let root = test_root("host-minimal-meta");
        write_minimal_lib(&root);
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"minimal-core\"\nversion = \"0.7.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        let manifest = Utf8PathBuf::from_path_buf(root.join("Cargo.toml")).unwrap();
        let metadata = load_metadata(&manifest).unwrap();
        assert_eq!(metadata.package_version, "0.7.0");
        assert!(metadata.description.is_none());
        assert!(metadata.authors.is_empty());
        assert!(metadata.license.is_none());
        let rendered = render_ohos_package_metadata(&metadata);
        assert!(!rendered.contains("description ="));
        assert!(!rendered.contains("authors ="));
        assert!(!rendered.contains("license ="));
        std::fs::remove_dir_all(root).ok();
    }
}
