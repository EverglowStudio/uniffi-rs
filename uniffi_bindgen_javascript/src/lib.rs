/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! First-class JavaScript/TypeScript bindings generator for uniffi-rs.
//!
//! This crate emits a single high-level TypeScript API plus one or more
//! flavor-specific backend adapters (wasm, napi) that satisfy a shared
//! low-level FFI contract. Electron preload + renderer facades are emitted
//! as a consumption form of the napi flavor.
//!
//! See `docs/manual/src/javascript/contract.md` for the stable contract
//! these emitters target.
//!
//! ```text
//! generated/
//!   common/    api.ts records.ts enums.ts errors.ts objects.ts callbacks.ts runtime.ts
//!   browser/   index.ts backend-wasm.ts
//!   node/      index.ts backend-napi.ts
//!   electron/  preload.cjs renderer.ts
//!   harmony/   index.ts backend-ohos.ts
//! ```

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fs_err as fs;
use uniffi_bindgen::{BindgenLoader, Component};

pub mod api_module;
pub mod callback_metadata;
pub mod dispatch_key;
pub mod electron;
pub mod enum_shape;
pub mod flavors;
pub mod host_crates;
pub mod js_names;
pub mod name_map;

/// Exact internal ABI spoken by the generated JavaScript runtime and every
/// generated backend adapter. This is deliberately not a public binding API.
pub(crate) const JS_RUNTIME_ABI_VERSION: u32 = 2;

pub use host_crates::HostCrateOptions;

/// Which low-level ABI flavor a generated backend speaks.
///
/// The high-level TS API is identical across flavors; only the backend
/// adapter differs. `Electron` is not its own flavor — it's a consumption
/// form of `Napi` that additionally emits preload + renderer entries.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum AbiFlavor {
    Wasm,
    Napi,
    Ohos,
}

/// Top-level options for the JavaScript target.
#[derive(Clone, Debug)]
pub struct GenerateJsOptions {
    /// Path to UDL file or compiled cdylib.
    pub source: Utf8PathBuf,
    /// Output directory; the generator creates `common/` plus one
    /// subdirectory per requested flavor (`browser/`, `node/`, `electron/`).
    pub out_dir: Utf8PathBuf,
    /// Optional directory used by artifact-building commands for non-source
    /// outputs such as `.node` addons. When set, generated backend entrypoints
    /// use this location as their default load path.
    pub artifact_dir: Option<Utf8PathBuf>,
    /// Optional uniffi.toml override.
    pub config_override: Option<Utf8PathBuf>,
    /// Limit generation to a single crate.
    pub crate_filter: Option<String>,
    /// Exclude transitive deps when running cargo metadata.
    pub metadata_no_deps: bool,
    /// Which flavors / consumption forms to emit.
    pub flavors: Vec<FlavorTarget>,
    /// Opt-in: also emit Rust host crates (`rust_modules/wasm`,
    /// `rust_modules/napi`) that wrap the per-component bridge files.
    /// When `None`, behaviour is identical to pre-host-crates invocations.
    pub host_crates: Option<HostCrateOptions>,
}

/// What to emit. `Electron` implies `Napi` plus preload+renderer files.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FlavorTarget {
    Wasm,
    Napi,
    Electron,
    Harmony,
}

impl FlavorTarget {
    fn abi(self) -> AbiFlavor {
        match self {
            FlavorTarget::Wasm => AbiFlavor::Wasm,
            FlavorTarget::Napi | FlavorTarget::Electron => AbiFlavor::Napi,
            FlavorTarget::Harmony => AbiFlavor::Ohos,
        }
    }
}

/// Entry point invoked by the uniffi-bindgen CLI.
pub fn generate(loader: &BindgenLoader, options: GenerateJsOptions) -> Result<()> {
    if options.flavors.is_empty() {
        bail!("uniffi-bindgen javascript: at least one --flavor must be specified");
    }

    let metadata = loader.load_metadata(&options.source)?;
    if let Some(crate_filter) = &options.crate_filter {
        if !metadata.contains_key(crate_filter) {
            bail!("No UniFFI metadata found for crate {crate_filter}");
        }
    }
    let mut cis = loader.load_cis(&options.source, metadata)?;
    if let Some(crate_filter) = &options.crate_filter {
        cis.retain(|ci| ci.crate_name() == crate_filter);
    }
    let override_toml = load_override_toml(options.config_override.as_ref())?;
    let mut components: Vec<Component<JsConfig>> =
        loader.load_components(cis, |_ci, mut toml| {
            if let Some(override_toml) = &override_toml {
                merge_toml(&mut toml, override_toml.clone());
            }
            JsConfig::from_root_toml(toml)
        })?;
    generate_components(&mut components, &options)
}

/// Perform the part of JavaScript generation that owns the output tree.
///
/// Keeping selection, ownership validation, and the first filesystem mutation
/// together makes the no-partial-output guarantee testable independently of
/// metadata loading.
fn generate_components(
    components: &mut Vec<Component<JsConfig>>,
    options: &GenerateJsOptions,
) -> Result<()> {
    if let Some(crate_filter) = &options.crate_filter {
        components.retain(|component| component.ci.crate_name() == crate_filter);
    }
    if components.is_empty() {
        bail!("No UniFFI components selected for JavaScript generation");
    }
    for c in components.iter_mut() {
        c.ci.derive_ffi_funcs()?;
    }

    preflight_output_ownership(components, &options.flavors)?;
    fs::create_dir_all(&options.out_dir)?;

    let mut emitted_crate_names: Vec<String> = Vec::new();
    let mut emitted_namespaces: Vec<String> = Vec::new();
    for component in &*components {
        emit_component(component, &options)?;
        emitted_crate_names.push(component.ci.crate_name().to_string());
        emitted_namespaces.push(component.ci.namespace().to_string());
    }

    if let Some(host_opts) = &options.host_crates {
        let meta = host_crates::load_metadata(&host_opts.manifest_path)?;
        let want_wasm = options.flavors.iter().any(|f| f.abi() == AbiFlavor::Wasm);
        // electron reuses the napi host crate — no separate electron crate.
        let want_napi = options.flavors.iter().any(|f| f.abi() == AbiFlavor::Napi);
        let want_ohos = options.flavors.iter().any(|f| f.abi() == AbiFlavor::Ohos);
        host_crates::emit(
            host_opts,
            &options.out_dir,
            &emitted_crate_names,
            &meta,
            want_wasm,
            want_napi,
            want_ohos,
            &emitted_namespaces,
        )?;
    }
    Ok(())
}

/// A stable component description suitable for deterministic preflight
/// diagnostics. Deliberately excludes source paths and loader ordering.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ComponentIdentity {
    crate_name: String,
    namespace: String,
}

impl ComponentIdentity {
    fn from_component(component: &Component<JsConfig>) -> Self {
        Self {
            crate_name: component.ci.crate_name().to_string(),
            namespace: component.ci.namespace().to_string(),
        }
    }

    fn describe(&self) -> String {
        format!(
            "crate `{}`, namespace `{}`",
            self.crate_name, self.namespace
        )
    }
}

/// Reject multiple components before they can overwrite the fixed JavaScript
/// output tree. The current public layout has shared `common/*` files and
/// fixed flavor entrypoints, so safe composition needs a future namespaced
/// layout rather than a best-effort merge here.
fn preflight_output_ownership(
    components: &[Component<JsConfig>],
    flavors: &[FlavorTarget],
) -> Result<()> {
    let identities = components
        .iter()
        .map(ComponentIdentity::from_component)
        .collect::<Vec<_>>();

    let mut namespace_owners = BTreeMap::<String, Vec<ComponentIdentity>>::new();
    for identity in &identities {
        namespace_owners
            .entry(identity.namespace.clone())
            .or_default()
            .push(identity.clone());
    }
    let duplicate_namespaces = namespace_owners
        .into_iter()
        .filter_map(|(namespace, mut owners)| {
            (owners.len() > 1).then(|| {
                owners.sort();
                (namespace, owners)
            })
        })
        .collect::<Vec<_>>();
    if !duplicate_namespaces.is_empty() {
        let mut message = String::from("duplicate UniFFI component namespace(s):\n");
        for (namespace, owners) in duplicate_namespaces {
            let owners = owners
                .iter()
                .map(ComponentIdentity::describe)
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!("- namespace `{namespace}`: {owners}\n"));
        }
        bail!("{message}");
    }

    let mut path_owners = BTreeMap::<String, std::collections::BTreeSet<ComponentIdentity>>::new();
    for component in components {
        let owner = ComponentIdentity::from_component(component);
        for path in component_output_claims(component, flavors) {
            path_owners.entry(path).or_default().insert(owner.clone());
        }
    }
    let conflicts = path_owners
        .into_iter()
        .filter(|(_, owners)| owners.len() > 1)
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        let mut message = String::from("conflicting UniFFI JavaScript output path ownership:\n");
        for (path, owners) in conflicts {
            let owners = owners
                .iter()
                .map(ComponentIdentity::describe)
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!("- `{path}`: {owners}\n"));
        }
        bail!("{message}");
    }
    Ok(())
}

/// Return every path emitted below `out_dir` for one component and the
/// requested flavors. A set intentionally collapses duplicate flavor flags
/// for the same owner.
fn component_output_claims(
    component: &Component<JsConfig>,
    flavors: &[FlavorTarget],
) -> std::collections::BTreeSet<String> {
    const COMMON_FILES: &[&str] = &[
        "runtime.ts",
        "custom-types.ts",
        "records.ts",
        "enums.ts",
        "errors.ts",
        "callbacks.ts",
        "objects.ts",
        "api.ts",
        "public-types.ts",
    ];

    let mut claims = COMMON_FILES
        .iter()
        .map(|file| format!("common/{file}"))
        .collect::<std::collections::BTreeSet<_>>();
    let crate_name = component.ci.crate_name();
    for flavor in flavors {
        match flavor {
            FlavorTarget::Wasm => {
                claims.insert(format!("browser/{crate_name}.rs"));
                claims.insert("browser/backend-wasm.ts".to_string());
                claims.insert("browser/index.ts".to_string());
            }
            FlavorTarget::Napi => {
                claims.insert(format!("node/{crate_name}.rs"));
                claims.insert("node/backend-napi.ts".to_string());
                claims.insert("node/index.ts".to_string());
            }
            FlavorTarget::Electron => {
                claims.insert(format!("electron/{crate_name}.rs"));
                claims.insert("electron/backend-napi.ts".to_string());
                claims.insert("electron/index.ts".to_string());
                claims.insert("electron/preload.cjs".to_string());
                claims.insert("electron/renderer.ts".to_string());
            }
            FlavorTarget::Harmony => {
                claims.insert(format!("harmony/{crate_name}.rs"));
                claims.insert("harmony/backend-ohos.ts".to_string());
                claims.insert(format!("harmony/{crate_name}.ohos-extra-types.d.ts"));
                claims.insert(format!("harmony/{crate_name}.ohos-facade.json"));
                claims.insert("harmony/stream.ts".to_string());
                claims.insert("harmony/index.ts".to_string());
            }
        }
    }
    claims
}

fn emit_component(component: &Component<JsConfig>, options: &GenerateJsOptions) -> Result<()> {
    let common_dir = options.out_dir.join("common");
    fs::create_dir_all(&common_dir)?;
    api_module::emit(&common_dir, component)?;

    for target in &options.flavors {
        let subdir = match target {
            FlavorTarget::Wasm => "browser",
            FlavorTarget::Napi => "node",
            FlavorTarget::Electron => "electron",
            FlavorTarget::Harmony => "harmony",
        };
        let dir = options.out_dir.join(subdir);
        fs::create_dir_all(&dir)?;
        let addon_path = if matches!(target.abi(), AbiFlavor::Napi) {
            default_addon_path(&dir, options.artifact_dir.as_deref(), subdir, component)?
        } else {
            None
        };
        flavors::emit(
            &dir,
            target.abi(),
            component,
            &flavors::FlavorEmitOptions {
                default_addon_path: addon_path,
            },
        )?;
        if matches!(target, FlavorTarget::Electron) {
            electron::emit(
                &dir,
                component,
                default_addon_path(&dir, options.artifact_dir.as_deref(), subdir, component)?
                    .as_deref(),
            )?;
        }
    }
    Ok(())
}

fn default_addon_path(
    from_dir: &Utf8Path,
    artifact_dir: Option<&Utf8Path>,
    subdir: &str,
    component: &Component<JsConfig>,
) -> Result<Option<String>> {
    let Some(artifact_dir) = artifact_dir else {
        return Ok(None);
    };
    let addon = artifact_dir
        .join(subdir)
        .join(format!("{}.node", component.ci.namespace()));
    Ok(Some(relative_module_specifier(from_dir, &addon)?))
}

fn relative_module_specifier(from_dir: &Utf8Path, to: &Utf8Path) -> Result<String> {
    let from_abs = absolutize(from_dir)?;
    let to_abs = absolutize(to)?;
    let rel = relative_path_from_dir(&from_abs, &to_abs)
        .to_string()
        .replace('\\', "/");
    let rel = if rel.is_empty() {
        ".".to_string()
    } else if rel.starts_with('.') {
        rel
    } else {
        format!("./{rel}")
    };
    Ok(rel.replace('"', "\\\""))
}

fn absolutize(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|p| anyhow::anyhow!("cwd is not utf8: {}", p.display()))?
        .join(path))
}

fn relative_path_from_dir(from_dir: &Utf8Path, to: &Utf8Path) -> Utf8PathBuf {
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
    result
}

/// Per-component config loaded from `uniffi.toml`.
///
/// Empty for now; add fields here only when they are part of the
/// supported long-term JavaScript target configuration surface.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct JsConfig {
    #[serde(default)]
    pub custom_types: BTreeMap<String, CustomTypeConfig>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CustomTypeConfig {
    #[serde(default)]
    pub imports: Vec<String>,
    #[serde(default)]
    pub type_name: Option<String>,
    #[serde(default)]
    pub into_custom: String,
    #[serde(default)]
    pub from_custom: String,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
struct RootConfig {
    bindings: BindingsConfig,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(default)]
struct BindingsConfig {
    #[serde(default)]
    javascript: JsConfig,
}

impl JsConfig {
    fn from_root_toml(toml: toml::Value) -> Result<Self> {
        let root: RootConfig = toml.try_into()?;
        Ok(root.bindings.javascript)
    }

    pub fn custom_type(&self, name: &str) -> Option<&CustomTypeConfig> {
        self.custom_types.get(name)
    }
}

impl CustomTypeConfig {
    pub fn public_type<'a>(&'a self, fallback: &'a str) -> &'a str {
        self.type_name.as_deref().unwrap_or(fallback)
    }

    pub fn into_custom_expr(&self, builtin_expr: &str) -> String {
        if self.into_custom.is_empty() {
            builtin_expr.to_string()
        } else {
            self.into_custom.replace("{}", builtin_expr)
        }
    }

    pub fn from_custom_expr(&self, custom_expr: &str) -> String {
        if self.from_custom.is_empty() {
            custom_expr.to_string()
        } else {
            self.from_custom.replace("{}", custom_expr)
        }
    }
}

fn load_override_toml(path: Option<&Utf8PathBuf>) -> Result<Option<toml::Value>> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text =
        fs::read_to_string(path).with_context(|| format!("reading config override {path}"))?;
    let value = toml::from_str(&text).with_context(|| format!("parsing config override {path}"))?;
    Ok(Some(value))
}

fn merge_toml(into: &mut toml::Value, from: toml::Value) {
    match (into, from) {
        (toml::Value::Table(into), toml::Value::Table(from)) => {
            for (key, value) in from {
                match into.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        into.insert(key, value);
                    }
                }
            }
        }
        (into, from) => *into = from,
    }
}

#[cfg(test)]
mod config_tests {
    use super::JsConfig;

    fn parse(input: &str) -> anyhow::Result<JsConfig> {
        JsConfig::from_root_toml(toml::from_str(input)?)
    }

    #[test]
    fn javascript_config_accepts_only_the_canonical_custom_type_shape() {
        let config = parse(
            r#"
                [bindings.javascript.customTypes.MyType]
                typeName = "MyType"
                imports = []
                intoCustom = "{}"
                fromCustom = "{}"
            "#,
        )
        .unwrap();

        let custom = config.custom_type("MyType").unwrap();
        assert_eq!(custom.public_type("Fallback"), "MyType");
        assert_eq!(custom.into_custom_expr("value"), "value");
        assert_eq!(custom.from_custom_expr("value"), "value");
    }

    #[test]
    fn javascript_config_rejects_legacy_aliases_and_unknown_subtree_fields() {
        for (label, input) in [
            ("custom_types", "[bindings.javascript]\ncustom_types = {}\n"),
            (
                "type_name",
                "[bindings.javascript.customTypes.MyType]\ntype_name = \"MyType\"\n",
            ),
            (
                "into_custom",
                "[bindings.javascript.customTypes.MyType]\ninto_custom = \"{}\"\n",
            ),
            (
                "from_custom",
                "[bindings.javascript.customTypes.MyType]\nfrom_custom = \"{}\"\n",
            ),
            (
                "lift",
                "[bindings.javascript.customTypes.MyType]\nlift = \"{}\"\n",
            ),
            (
                "lower",
                "[bindings.javascript.customTypes.MyType]\nlower = \"{}\"\n",
            ),
            (
                "unknown javascript field",
                "[bindings.javascript]\nunknown = true\n",
            ),
            (
                "unknown custom type field",
                "[bindings.javascript.customTypes.MyType]\nunknown = true\n",
            ),
        ] {
            let error = parse(input).unwrap_err().to_string();
            assert!(
                error.contains(label.split(' ').next().unwrap()),
                "{label} must be rejected with a useful parse error: {error}"
            );
        }
    }

    #[test]
    fn javascript_config_remains_optional_and_does_not_reject_other_bindings() {
        assert!(parse("[bindings.swift]\nmoduleName = \"Example\"\n")
            .unwrap()
            .custom_types
            .is_empty());
        assert!(parse("[bindings.kotlin]\npackageName = \"example\"\n")
            .unwrap()
            .custom_types
            .is_empty());
    }
}

#[cfg(test)]
mod output_ownership_tests {
    use super::*;
    use uniffi_bindgen::interface::ComponentInterface;
    use uniffi_meta::{MetadataGroup, NamespaceMetadata};

    fn component(crate_name: &str, namespace: &str) -> Component<JsConfig> {
        let ci = ComponentInterface::from_metadata(MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: crate_name.to_string(),
                name: namespace.to_string(),
            },
            namespace_docstring: None,
            items: Default::default(),
        })
        .unwrap();
        Component {
            ci,
            config: JsConfig::default(),
        }
    }

    fn components() -> Vec<Component<JsConfig>> {
        vec![
            component("component_a", "namespace_a"),
            component("component_b", "namespace_b"),
        ]
    }

    fn test_path(label: &str) -> Utf8PathBuf {
        let unique = format!(
            "uniffi-js-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        Utf8PathBuf::from_path_buf(std::env::temp_dir().join(unique)).unwrap()
    }

    fn options(out_dir: Utf8PathBuf, crate_filter: Option<&str>) -> GenerateJsOptions {
        GenerateJsOptions {
            source: Utf8PathBuf::from("multi-component-test.udl"),
            out_dir,
            artifact_dir: None,
            config_override: None,
            crate_filter: crate_filter.map(str::to_string),
            metadata_no_deps: true,
            flavors: vec![FlavorTarget::Napi],
            host_crates: None,
        }
    }

    #[test]
    fn multi_component_conflicts_are_stable_and_list_all_owners_and_paths() {
        let forward = preflight_output_ownership(&components(), &[FlavorTarget::Napi])
            .unwrap_err()
            .to_string();
        let mut reverse = components();
        reverse.reverse();
        let reversed = preflight_output_ownership(&reverse, &[FlavorTarget::Napi])
            .unwrap_err()
            .to_string();

        assert_eq!(forward, reversed);
        assert!(forward.contains("conflicting UniFFI JavaScript output path ownership"));
        assert!(forward.contains("`common/api.ts`"));
        assert!(forward.contains("`node/backend-napi.ts`"));
        for owner in [
            "crate `component_a`, namespace `namespace_a`",
            "crate `component_b`, namespace `namespace_b`",
        ] {
            assert!(
                forward.contains(owner),
                "missing owner `{owner}`:\n{forward}"
            );
        }
    }

    #[test]
    fn multi_component_duplicate_namespace_is_explicit_and_stable() {
        let forward = vec![
            component("component_a", "shared_namespace"),
            component("component_b", "shared_namespace"),
        ];
        let reversed = vec![
            component("component_b", "shared_namespace"),
            component("component_a", "shared_namespace"),
        ];

        let forward_error = preflight_output_ownership(&forward, &[FlavorTarget::Harmony])
            .unwrap_err()
            .to_string();
        let reversed_error = preflight_output_ownership(&reversed, &[FlavorTarget::Harmony])
            .unwrap_err()
            .to_string();

        assert_eq!(forward_error, reversed_error);
        assert!(forward_error.contains("duplicate UniFFI component namespace(s)"));
        assert!(forward_error.contains("namespace `shared_namespace`"));
        assert!(forward_error.contains("crate `component_a`, namespace `shared_namespace`"));
        assert!(forward_error.contains("crate `component_b`, namespace `shared_namespace`"));
    }

    #[test]
    fn multi_component_preflight_happens_before_output_mutation() {
        let missing_out_dir = test_path("missing-output");
        let error = generate_components(&mut components(), &options(missing_out_dir.clone(), None))
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicting UniFFI JavaScript output path ownership"));
        assert!(
            !missing_out_dir.exists(),
            "preflight failure must not create {missing_out_dir}"
        );

        let existing_out_dir = test_path("sentinel-output");
        fs::create_dir_all(&existing_out_dir).unwrap();
        let sentinel = existing_out_dir.join("sentinel.txt");
        fs::write(&sentinel, "preserve me").unwrap();
        let error =
            generate_components(&mut components(), &options(existing_out_dir.clone(), None))
                .unwrap_err()
                .to_string();
        assert!(error.contains("conflicting UniFFI JavaScript output path ownership"));
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "preserve me");
        let entries = fs::read_dir(&existing_out_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(entries, ["sentinel.txt"]);
    }

    #[test]
    fn crate_filter_selects_one_component_before_preflight_and_emits_normally() {
        let out_dir = test_path("crate-filter");
        generate_components(
            &mut components(),
            &options(out_dir.clone(), Some("component_a")),
        )
        .unwrap();

        assert!(out_dir.join("common/api.ts").is_file());
        assert!(out_dir.join("node/component_a.rs").is_file());
        assert!(!out_dir.join("node/component_b.rs").exists());
    }

    #[test]
    fn component_output_claims_match_all_flavor_emitters() {
        let component = component("fixture", "fixture_namespace");
        preflight_output_ownership(
            std::slice::from_ref(&component),
            &[FlavorTarget::Harmony, FlavorTarget::Harmony],
        )
        .unwrap();
        let claims = component_output_claims(
            &component,
            &[
                FlavorTarget::Wasm,
                FlavorTarget::Napi,
                FlavorTarget::Electron,
                FlavorTarget::Harmony,
                FlavorTarget::Harmony,
            ],
        );
        for path in [
            "common/api.ts",
            "common/runtime.ts",
            "browser/fixture.rs",
            "browser/backend-wasm.ts",
            "node/fixture.rs",
            "node/backend-napi.ts",
            "electron/fixture.rs",
            "electron/backend-napi.ts",
            "electron/preload.cjs",
            "electron/renderer.ts",
            "harmony/fixture.rs",
            "harmony/backend-ohos.ts",
            "harmony/fixture.ohos-extra-types.d.ts",
            "harmony/fixture.ohos-facade.json",
            "harmony/stream.ts",
            "harmony/index.ts",
        ] {
            assert!(claims.contains(path), "missing output claim `{path}`");
        }
        assert_eq!(
            claims.len(),
            26,
            "duplicate flavors must not add duplicate claims: {claims:#?}"
        );
    }
}
