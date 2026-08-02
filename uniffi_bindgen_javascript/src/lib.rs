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
//!   shared/runtime.ts
//!   components/<namespace>/common/
//!   components/<namespace>/{browser,node,electron,harmony}/
//!   {browser,node,electron,harmony}/index.ts
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
    /// Output directory. Component sources live below
    /// `components/<namespace>/`; each requested flavor also gets one stable,
    /// namespace-only root entrypoint.
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

    components.sort_by_key(ComponentIdentity::from_component);
    preflight_component_layout(components, options)?;
    fs::create_dir_all(&options.out_dir)?;

    api_module::emit_shared_runtime(&options.out_dir)?;

    let mut emitted_crate_names: Vec<String> = Vec::new();
    let mut emitted_namespaces: Vec<String> = Vec::new();
    for component in &*components {
        emit_component(component, components, &options)?;
        emitted_crate_names.push(component.ci.crate_name().to_string());
        emitted_namespaces.push(component.ci.namespace().to_string());
    }

    emit_platform_roots(&options.out_dir, components, &options.flavors)?;

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

/// Validate every component identity and every selected cross-component owner
/// before generation creates the output root.  The namespaced tree gives each
/// component disjoint paths, but unsafe or duplicate namespace identifiers
/// would still make a root export ambiguous or unsafe.
fn preflight_component_layout(
    components: &[Component<JsConfig>],
    options: &GenerateJsOptions,
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
    let mut diagnostics = Vec::new();
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
        diagnostics.push(message);
    }

    let mut unsafe_namespaces = identities
        .iter()
        .filter(|identity| !is_safe_component_namespace(&identity.namespace))
        .cloned()
        .collect::<Vec<_>>();
    unsafe_namespaces.sort();
    if !unsafe_namespaces.is_empty() {
        let mut message = String::from(
            "unsafe UniFFI component namespace(s); namespaces must be one safe TypeScript identifier path segment:\n",
        );
        for identity in unsafe_namespaces {
            message.push_str(&format!("- {}\n", identity.describe()));
        }
        diagnostics.push(message);
    }

    // The loader's crate-to-namespace metadata is keyed by this normalized
    // crate root.  Selecting two components that collide here would make an
    // owner lookup inherently ambiguous, even when their output namespaces
    // are distinct.  Reject it before attempting any owner resolution rather
    // than letting a later `.find()` silently choose the first component.
    let mut crate_root_owners = BTreeMap::<String, Vec<ComponentIdentity>>::new();
    for identity in &identities {
        crate_root_owners
            .entry(crate_root(&identity.crate_name))
            .or_default()
            .push(identity.clone());
    }
    let duplicate_crate_roots = crate_root_owners
        .into_iter()
        .filter_map(|(root, mut owners)| {
            (owners.len() > 1).then(|| {
                owners.sort();
                (root, owners)
            })
        })
        .collect::<Vec<_>>();
    if !duplicate_crate_roots.is_empty() {
        let mut message =
            String::from("ambiguous normalized UniFFI component crate root owner(s):\n");
        for (root, owners) in duplicate_crate_roots {
            let owners = owners
                .iter()
                .map(ComponentIdentity::describe)
                .collect::<Vec<_>>()
                .join(", ");
            message.push_str(&format!("- normalized crate root `{root}`: {owners}\n"));
        }
        diagnostics.push(message);
    }

    if !diagnostics.is_empty() {
        bail!(diagnostics.join(""));
    }

    validate_selected_external_type_owners(components)?;

    // 05C owns composite host/artifact identities.  Keep existing single
    // component host workflows alive, but reject a plural host before source,
    // host, or artifact output can be created.
    if options.host_crates.is_some() && components.len() != 1 {
        let namespaces = components
            .iter()
            .map(|component| component.ci.namespace())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "JavaScript host-crate generation currently supports one component during namespaced-layout stage 05B; selected namespaces: {namespaces}. Multi-component host composition is deferred to stage 05C"
        );
    }
    Ok(())
}

fn is_safe_component_namespace(namespace: &str) -> bool {
    if namespace.is_empty()
        || namespace == "."
        || namespace == ".."
        || namespace == "__proto__"
        || namespace.contains('/')
        || namespace.contains('\\')
        || is_typescript_reserved_word(namespace)
    {
        return false;
    }
    let mut chars = namespace.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn is_typescript_reserved_word(name: &str) -> bool {
    matches!(
        name,
        "break"
            | "case"
            | "catch"
            | "class"
            | "const"
            | "continue"
            | "debugger"
            | "default"
            | "delete"
            | "do"
            | "else"
            | "enum"
            | "export"
            | "extends"
            | "false"
            | "finally"
            | "for"
            | "function"
            | "if"
            | "import"
            | "in"
            | "instanceof"
            | "new"
            | "null"
            | "return"
            | "super"
            | "switch"
            | "this"
            | "throw"
            | "true"
            | "try"
            | "typeof"
            | "var"
            | "void"
            | "while"
            | "with"
            | "yield"
            | "let"
            | "static"
            | "implements"
            | "interface"
            | "package"
            | "private"
            | "protected"
            | "public"
            | "await"
            | "abstract"
            | "accessor"
            | "as"
            | "asserts"
            | "any"
            | "bigint"
            | "boolean"
            | "constructor"
            | "declare"
            | "from"
            | "get"
            | "infer"
            | "is"
            | "keyof"
            | "module"
            | "namespace"
            | "never"
            | "number"
            | "object"
            | "of"
            | "override"
            | "readonly"
            | "require"
            | "satisfies"
            | "set"
            | "string"
            | "symbol"
            | "type"
            | "undefined"
            | "unique"
            | "unknown"
            | "using"
    )
}

fn crate_root(module_path: &str) -> String {
    module_path
        .split("::")
        .next()
        .unwrap_or(module_path)
        .replace('-', "_")
}

fn type_owner_component<'a>(
    components: &'a [Component<JsConfig>],
    type_: &uniffi_bindgen::interface::Type,
) -> Option<&'a Component<JsConfig>> {
    let module_path = type_.module_path()?;
    let crate_name = crate_root(module_path);
    let mut owners = components
        .iter()
        .filter(|component| crate_root(component.ci.crate_name()) == crate_name);
    let owner = owners.next()?;
    owners.next().is_none().then_some(owner)
}

fn component_defines_named_type(
    component: &Component<JsConfig>,
    type_: &uniffi_bindgen::interface::Type,
) -> bool {
    use uniffi_bindgen::interface::Type;
    match type_ {
        Type::Record { name, .. } => component.ci.get_record_definition(name).is_some(),
        Type::Enum { name, .. } => component.ci.get_enum_definition(name).is_some(),
        Type::Object { name, .. } => component.ci.get_object_definition(name).is_some(),
        Type::CallbackInterface { name, .. } => component
            .ci
            .get_callback_interface_definition(name)
            .is_some(),
        Type::Custom { name, .. } => {
            matches!(component.ci.get_type(name), Some(Type::Custom { .. }))
        }
        _ => true,
    }
}

fn validate_selected_external_type_owners(components: &[Component<JsConfig>]) -> Result<()> {
    use uniffi_bindgen::interface::Type;

    let mut diagnostics = std::collections::BTreeSet::new();
    for component in components {
        for type_ in component.ci.iter_external_types() {
            for nested in type_.iter_types() {
                if !matches!(
                    nested,
                    Type::Record { .. }
                        | Type::Enum { .. }
                        | Type::Object { .. }
                        | Type::CallbackInterface { .. }
                        | Type::Custom { .. }
                ) {
                    continue;
                }
                let Some(module_path) = nested.module_path() else {
                    continue;
                };
                let display = format!("{nested:?}");
                let resolved_namespace = component
                    .ci
                    .namespace_for_type(nested)
                    .map(str::to_string)
                    .unwrap_or_else(|error| format!("<unresolved: {error}>"));
                let Some(owner) = type_owner_component(components, nested) else {
                    diagnostics.insert(format!(
                        "- component `{}` references external type {display} from `{module_path}`, but that owner is not selected for JavaScript generation",
                        component.ci.namespace()
                    ));
                    continue;
                };
                let is_exact_local_owner = owner.ci.namespace() == component.ci.namespace()
                    && resolved_namespace == component.ci.namespace()
                    && component_defines_named_type(owner, nested);
                if is_exact_local_owner {
                    continue;
                }
                if resolved_namespace != owner.ci.namespace()
                    || !component_defines_named_type(owner, nested)
                {
                    diagnostics.insert(format!(
                        "- component `{}` cannot resolve external owner for type {display}: expected namespace `{}`, resolved `{resolved_namespace}`",
                        component.ci.namespace(),
                        owner.ci.namespace(),
                    ));
                }
            }
        }
    }
    if diagnostics.is_empty() {
        Ok(())
    } else {
        bail!(
            "unresolved external UniFFI JavaScript type owner(s) before output mutation:\n{}",
            diagnostics.into_iter().collect::<Vec<_>>().join("\n")
        )
    }
}

fn emit_component(
    component: &Component<JsConfig>,
    all_components: &[Component<JsConfig>],
    options: &GenerateJsOptions,
) -> Result<()> {
    let component_dir = options
        .out_dir
        .join("components")
        .join(component.ci.namespace());
    let common_dir = component_dir.join("common");
    fs::create_dir_all(&common_dir)?;
    api_module::emit(&common_dir, component, all_components)?;

    for target in &options.flavors {
        let subdir = match target {
            FlavorTarget::Wasm => "browser",
            FlavorTarget::Napi => "node",
            FlavorTarget::Electron => "electron",
            FlavorTarget::Harmony => "harmony",
        };
        let dir = component_dir.join(subdir);
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

fn emit_platform_roots(
    out_dir: &Utf8Path,
    components: &[Component<JsConfig>],
    flavors: &[FlavorTarget],
) -> Result<()> {
    let has = |target| flavors.iter().any(|flavor| *flavor == target);
    if has(FlavorTarget::Wasm) {
        emit_platform_root(
            out_dir,
            components,
            "browser",
            "index.ts",
            PlatformRoot::Standard,
        )?;
    }
    if has(FlavorTarget::Napi) {
        emit_platform_root(
            out_dir,
            components,
            "node",
            "index.ts",
            PlatformRoot::Standard,
        )?;
    }
    if has(FlavorTarget::Harmony) {
        emit_platform_root(
            out_dir,
            components,
            "harmony",
            "index.ts",
            PlatformRoot::Standard,
        )?;
    }
    if has(FlavorTarget::Electron) {
        emit_platform_root(
            out_dir,
            components,
            "electron",
            "index.ts",
            PlatformRoot::Electron,
        )?;
        emit_electron_aggregate_preload(out_dir, components)?;
    }
    Ok(())
}

#[derive(Copy, Clone)]
enum PlatformRoot {
    Standard,
    Electron,
}

fn emit_platform_root(
    out_dir: &Utf8Path,
    components: &[Component<JsConfig>],
    flavor: &str,
    file_name: &str,
    kind: PlatformRoot,
) -> Result<()> {
    let dir = out_dir.join(flavor);
    fs::create_dir_all(&dir)?;
    let mut source = format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript ({flavor} namespace root).\n// Do not edit by hand; regenerate via `uniffi-bindgen generate --language javascript`.\n\n"
    );
    for component in components {
        let namespace = component.ci.namespace();
        match kind {
            PlatformRoot::Standard => source.push_str(&format!(
                "export * as {namespace} from \"../components/{namespace}/{flavor}/index.ts\";\n"
            )),
            PlatformRoot::Electron => {
                source.push_str(&format!(
                    "export * as {namespace} from \"../components/{namespace}/electron/renderer.ts\";\n"
                ));
            }
        }
    }
    if matches!(kind, PlatformRoot::Electron) {
        source.push_str("\nconst electronEntrypoints = Object.freeze({\n");
        for component in components {
            let namespace = component.ci.namespace();
            source.push_str(&format!(
                "    {namespace}: Object.freeze({{\n        main: () => import(\"../components/{namespace}/electron/index.ts\"),\n        preload: new URL(\"./preload.cjs\", import.meta.url),\n    }}),\n"
            ));
        }
        source.push_str("});\nexport default electronEntrypoints;\n");
    }
    fs::write(dir.join(file_name), source)?;
    Ok(())
}

/// Electron admits one preload script per window.  Component preloads are
/// modules that construct isolated bridges; this aggregate is the single
/// place that publishes them to the renderer, keyed by component namespace.
fn emit_electron_aggregate_preload(
    out_dir: &Utf8Path,
    components: &[Component<JsConfig>],
) -> Result<()> {
    let mut source = String::from(
        "// AUTOGENERATED by uniffi_bindgen_javascript (electron aggregate preload).\n\
         // Do not edit by hand; regenerate via `uniffi-bindgen generate --language javascript`.\n\n\
         const { contextBridge } = require(\"electron\");\n",
    );
    for component in components {
        let namespace = component.ci.namespace();
        source.push_str(&format!(
            "const {namespace} = require(\"../components/{namespace}/electron/preload.cjs\");\n"
        ));
    }
    source.push_str("\nconst components = Object.freeze({\n");
    for component in components {
        let namespace = component.ci.namespace();
        source.push_str(&format!("    {namespace},\n"));
    }
    source.push_str(
        "});\n\
         contextBridge.exposeInMainWorld(\"__uniffi__\", Object.freeze({ components }));\n\
         module.exports = components;\n",
    );
    fs::write(out_dir.join("electron").join("preload.cjs"), source)?;
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
    use std::collections::BTreeMap;
    use uniffi_bindgen::interface::{ComponentInterface, ObjectImpl};
    use uniffi_meta::{
        CallbackInterfaceMetadata, CustomTypeMetadata, EnumMetadata, EnumShape, FieldMetadata,
        FnMetadata, FnParamMetadata, Metadata, MetadataGroup, NamespaceMetadata, ObjectMetadata,
        RecordMetadata, Type, VariantMetadata,
    };

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

    fn record_metadata(module_path: &str, name: &str, field_name: &str, ty: Type) -> Metadata {
        record_with_fields_metadata(module_path, name, vec![field_metadata(field_name, ty)])
    }

    fn field_metadata(name: &str, ty: Type) -> FieldMetadata {
        FieldMetadata {
            name: name.to_string(),
            orig_name: None,
            ty,
            default: None,
            docstring: None,
        }
    }

    fn record_with_fields_metadata(
        module_path: &str,
        name: &str,
        fields: Vec<FieldMetadata>,
    ) -> Metadata {
        Metadata::Record(RecordMetadata {
            module_path: module_path.to_string(),
            name: name.to_string(),
            orig_name: None,
            rust_path: None,
            remote: false,
            fields,
            docstring: None,
        })
    }

    fn function_metadata(
        module_path: &str,
        name: &str,
        inputs: Vec<FnParamMetadata>,
        return_type: Option<Type>,
    ) -> Metadata {
        Metadata::Func(FnMetadata {
            module_path: module_path.to_string(),
            name: name.to_string(),
            orig_name: None,
            is_async: false,
            inputs,
            return_type,
            throws: None,
            checksum: None,
            docstring: None,
        })
    }

    /// Two components deliberately share public API names.  `AlphaOwned` is
    /// only defined by alpha and is used from beta, exercising the owner-aware
    /// imports/lowering path independently of local name collisions.
    fn linked_external_record_components() -> Vec<Component<JsConfig>> {
        let alpha_group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: "alpha".to_string(),
                name: "alpha".to_string(),
            },
            namespace_docstring: None,
            items: [
                record_metadata("alpha", "Shared", "label", Type::String),
                record_metadata("alpha", "AlphaOwned", "id", Type::UInt32),
                function_metadata("alpha", "same_api", Vec::new(), Some(Type::String)),
            ]
            .into_iter()
            .collect(),
        };
        let alpha_owned = Type::Record {
            module_path: "alpha".to_string(),
            name: "AlphaOwned".to_string(),
        };
        let beta_group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: "beta".to_string(),
                name: "beta".to_string(),
            },
            namespace_docstring: None,
            items: [
                record_metadata("beta", "Shared", "count", Type::UInt32),
                function_metadata("beta", "same_api", Vec::new(), Some(Type::String)),
                function_metadata(
                    "beta",
                    "round_trip_alpha_owned",
                    vec![FnParamMetadata::simple("value", alpha_owned.clone())],
                    Some(alpha_owned),
                ),
            ]
            .into_iter()
            .collect(),
        };
        let mut cis = vec![
            ComponentInterface::from_metadata(alpha_group).unwrap(),
            ComponentInterface::from_metadata(beta_group).unwrap(),
        ];
        let namespaces = BTreeMap::from([
            (
                "alpha".to_string(),
                NamespaceMetadata {
                    crate_name: "alpha".to_string(),
                    name: "alpha".to_string(),
                },
            ),
            (
                "beta".to_string(),
                NamespaceMetadata {
                    crate_name: "beta".to_string(),
                    name: "beta".to_string(),
                },
            ),
        ]);
        for ci in &mut cis {
            ci.set_crate_to_namespace_map(namespaces.clone());
        }
        let all = cis.clone();
        for ci in &mut cis {
            ci.set_all_component_interfaces(all.clone());
        }
        cis.into_iter()
            .map(|ci| Component {
                ci,
                config: JsConfig::default(),
            })
            .collect()
    }

    /// A linked alpha/beta fixture whose beta API exercises every external
    /// helper that can appear while lowering or lifting a named payload.
    /// Alpha owns all nested definitions and the custom conversion config.
    /// Beta also configures an unrelated local custom and a colliding
    /// `AlphaCustom` name, so generated imports must follow the type owner
    /// rather than the consuming component's config lookup.
    fn linked_nested_external_payload_components() -> Vec<Component<JsConfig>> {
        let alpha_object = Type::Object {
            module_path: "alpha".to_string(),
            name: "AlphaObject".to_string(),
            imp: ObjectImpl::Struct,
        };
        let alpha_callback = Type::CallbackInterface {
            module_path: "alpha".to_string(),
            name: "AlphaCallback".to_string(),
        };
        let alpha_custom = Type::Custom {
            module_path: "alpha".to_string(),
            name: "AlphaCustom".to_string(),
            builtin: Box::new(Type::String),
        };
        let alpha_choice = Type::Enum {
            module_path: "alpha".to_string(),
            name: "AlphaChoice".to_string(),
        };
        let alpha_error = Type::Enum {
            module_path: "alpha".to_string(),
            name: "AlphaFailure".to_string(),
        };
        let alpha_envelope = Type::Record {
            module_path: "alpha".to_string(),
            name: "Envelope".to_string(),
        };
        let beta_callback = Type::CallbackInterface {
            module_path: "beta".to_string(),
            name: "BetaCallback".to_string(),
        };
        let beta_custom = Type::Custom {
            module_path: "beta".to_string(),
            name: "BetaCustom".to_string(),
            builtin: Box::new(Type::String),
        };
        let payload_fields = || {
            vec![
                field_metadata("target", alpha_object.clone()),
                field_metadata("callback", alpha_callback.clone()),
                field_metadata("custom", alpha_custom.clone()),
            ]
        };
        let alpha_group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: "alpha".to_string(),
                name: "alpha".to_string(),
            },
            namespace_docstring: None,
            items: vec![
                Metadata::Object(ObjectMetadata {
                    module_path: "alpha".to_string(),
                    name: "AlphaObject".to_string(),
                    orig_name: None,
                    remote: false,
                    imp: ObjectImpl::Struct,
                    docstring: None,
                }),
                Metadata::CallbackInterface(CallbackInterfaceMetadata {
                    module_path: "alpha".to_string(),
                    name: "AlphaCallback".to_string(),
                    docstring: None,
                }),
                Metadata::CustomType(CustomTypeMetadata {
                    module_path: "alpha".to_string(),
                    name: "AlphaCustom".to_string(),
                    orig_name: None,
                    builtin: Type::String,
                    docstring: None,
                }),
                Metadata::Enum(EnumMetadata {
                    module_path: "alpha".to_string(),
                    name: "AlphaChoice".to_string(),
                    orig_name: None,
                    rust_path: None,
                    shape: EnumShape::Enum,
                    remote: false,
                    variants: vec![VariantMetadata {
                        name: "payload".to_string(),
                        orig_name: None,
                        discr: None,
                        fields: payload_fields(),
                        docstring: None,
                    }],
                    discr_type: None,
                    non_exhaustive: false,
                    docstring: None,
                }),
                Metadata::Enum(EnumMetadata {
                    module_path: "alpha".to_string(),
                    name: "AlphaFailure".to_string(),
                    orig_name: None,
                    rust_path: None,
                    shape: EnumShape::Error { flat: false },
                    remote: false,
                    variants: vec![VariantMetadata {
                        name: "payload".to_string(),
                        orig_name: None,
                        discr: None,
                        fields: payload_fields(),
                        docstring: None,
                    }],
                    discr_type: None,
                    non_exhaustive: false,
                    docstring: None,
                }),
                record_with_fields_metadata(
                    "alpha",
                    "Envelope",
                    vec![
                        field_metadata("target", alpha_object.clone()),
                        field_metadata("callback", alpha_callback.clone()),
                        field_metadata("custom", alpha_custom.clone()),
                        field_metadata("choice", alpha_choice.clone()),
                    ],
                ),
            ]
            .into_iter()
            .collect(),
        };
        let beta_group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: "beta".to_string(),
                name: "beta".to_string(),
            },
            namespace_docstring: None,
            items: vec![
                Metadata::CallbackInterface(CallbackInterfaceMetadata {
                    module_path: "beta".to_string(),
                    name: "BetaCallback".to_string(),
                    docstring: None,
                }),
                Metadata::CustomType(CustomTypeMetadata {
                    module_path: "beta".to_string(),
                    name: "BetaCustom".to_string(),
                    orig_name: None,
                    builtin: Type::String,
                    docstring: None,
                }),
                function_metadata(
                    "beta",
                    "round_trip_envelope",
                    vec![FnParamMetadata::simple("value", alpha_envelope.clone())],
                    Some(alpha_envelope.clone()),
                ),
                function_metadata(
                    "beta",
                    "round_trip_choice",
                    vec![FnParamMetadata::simple("value", alpha_choice.clone())],
                    Some(alpha_choice.clone()),
                ),
                function_metadata(
                    "beta",
                    "round_trip_callback",
                    vec![FnParamMetadata::simple("value", alpha_callback.clone())],
                    Some(alpha_callback.clone()),
                ),
                function_metadata(
                    "beta",
                    "round_trip_custom",
                    vec![FnParamMetadata::simple("value", alpha_custom.clone())],
                    Some(alpha_custom.clone()),
                ),
                function_metadata(
                    "beta",
                    "send_beta_callback",
                    vec![FnParamMetadata::simple("value", beta_callback)],
                    None,
                ),
                function_metadata(
                    "beta",
                    "round_trip_beta_custom",
                    vec![FnParamMetadata::simple("value", beta_custom.clone())],
                    Some(beta_custom),
                ),
                function_metadata(
                    "beta",
                    "stream_envelope",
                    Vec::new(),
                    Some(Type::Stream {
                        item_type: Box::new(alpha_envelope.clone()),
                        error_type: Box::new(alpha_error.clone()),
                        is_send: true,
                    }),
                ),
                function_metadata(
                    "beta",
                    "send_input",
                    vec![FnParamMetadata::simple(
                        "input",
                        Type::InputStream {
                            item_type: Box::new(alpha_envelope),
                            error_type: Box::new(alpha_error),
                            is_send: true,
                        },
                    )],
                    None,
                ),
            ]
            .into_iter()
            .collect(),
        };
        let mut cis = vec![
            ComponentInterface::from_metadata(alpha_group).unwrap(),
            ComponentInterface::from_metadata(beta_group).unwrap(),
        ];
        let namespaces = BTreeMap::from([
            (
                "alpha".to_string(),
                NamespaceMetadata {
                    crate_name: "alpha".to_string(),
                    name: "alpha".to_string(),
                },
            ),
            (
                "beta".to_string(),
                NamespaceMetadata {
                    crate_name: "beta".to_string(),
                    name: "beta".to_string(),
                },
            ),
        ]);
        for ci in &mut cis {
            ci.set_crate_to_namespace_map(namespaces.clone());
        }
        let all = cis.clone();
        for ci in &mut cis {
            ci.set_all_component_interfaces(all.clone());
        }
        cis.into_iter()
            .enumerate()
            .map(|(index, ci)| Component {
                ci,
                config: JsConfig {
                    custom_types: if index == 0 {
                        BTreeMap::from([(
                            "AlphaCustom".to_string(),
                            CustomTypeConfig {
                                into_custom: "String({})".to_string(),
                                from_custom: "String({})".to_string(),
                                ..Default::default()
                            },
                        )])
                    } else {
                        BTreeMap::from([
                            (
                                "AlphaCustom".to_string(),
                                CustomTypeConfig {
                                    into_custom: "String({})".to_string(),
                                    from_custom: "String({})".to_string(),
                                    ..Default::default()
                                },
                            ),
                            (
                                "BetaCustom".to_string(),
                                CustomTypeConfig {
                                    into_custom: "String({})".to_string(),
                                    from_custom: "String({})".to_string(),
                                    ..Default::default()
                                },
                            ),
                        ])
                    },
                },
            })
            .collect()
    }

    fn duplicate_normalized_root_components() -> Vec<Component<JsConfig>> {
        let mut cis = vec![
            ComponentInterface::from_metadata(MetadataGroup {
                namespace: NamespaceMetadata {
                    crate_name: "shared".to_string(),
                    name: "alpha".to_string(),
                },
                namespace_docstring: None,
                items: [record_metadata("shared", "Shared", "alpha", Type::String)]
                    .into_iter()
                    .collect(),
            })
            .unwrap(),
            ComponentInterface::from_metadata(MetadataGroup {
                namespace: NamespaceMetadata {
                    crate_name: "shared".to_string(),
                    name: "beta".to_string(),
                },
                namespace_docstring: None,
                items: [record_metadata("shared", "Shared", "beta", Type::UInt32)]
                    .into_iter()
                    .collect(),
            })
            .unwrap(),
            ComponentInterface::from_metadata(MetadataGroup {
                namespace: NamespaceMetadata {
                    crate_name: "a-b".to_string(),
                    name: "hyphen".to_string(),
                },
                namespace_docstring: None,
                items: Default::default(),
            })
            .unwrap(),
            ComponentInterface::from_metadata(MetadataGroup {
                namespace: NamespaceMetadata {
                    crate_name: "a_b".to_string(),
                    name: "underscore".to_string(),
                },
                namespace_docstring: None,
                items: Default::default(),
            })
            .unwrap(),
        ];
        let all = cis.clone();
        for ci in &mut cis {
            ci.set_all_component_interfaces(all.clone());
        }
        cis.into_iter()
            .map(|ci| Component {
                ci,
                config: JsConfig::default(),
            })
            .collect()
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

    fn generated_tree_snapshot(root: &Utf8Path) -> BTreeMap<String, Vec<u8>> {
        fn collect(root: &Utf8Path, dir: &Utf8Path, out: &mut BTreeMap<String, Vec<u8>>) {
            let mut entries = std::fs::read_dir(dir)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = Utf8PathBuf::from_path_buf(entry.path()).unwrap();
                if entry.file_type().unwrap().is_dir() {
                    collect(root, &path, out);
                } else {
                    out.insert(
                        path.strip_prefix(root).unwrap().to_string(),
                        std::fs::read(&path).unwrap(),
                    );
                }
            }
        }

        let mut snapshot = BTreeMap::new();
        collect(root, root, &mut snapshot);
        snapshot
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
    fn multi_component_namespaced_layout_is_stable_and_non_conflicting() {
        let forward_components = components();
        let forward = preflight_component_layout(
            &forward_components,
            &options(test_path("preflight-forward"), None),
        )
        .map(|_| "ok".to_string())
        .unwrap();
        let mut reverse = components();
        reverse.reverse();
        let reversed =
            preflight_component_layout(&reverse, &options(test_path("preflight-reverse"), None))
                .map(|_| "ok".to_string())
                .unwrap();

        assert_eq!(forward, reversed);
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

        let forward_error =
            preflight_component_layout(&forward, &options(test_path("duplicate-forward"), None))
                .unwrap_err()
                .to_string();
        let reversed_error =
            preflight_component_layout(&reversed, &options(test_path("duplicate-reverse"), None))
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
        let mut duplicate_components = vec![
            component("component_a", "shared_namespace"),
            component("component_b", "shared_namespace"),
        ];
        let error = generate_components(
            &mut duplicate_components,
            &options(missing_out_dir.clone(), None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("duplicate UniFFI component namespace(s)"));
        assert!(
            !missing_out_dir.exists(),
            "preflight failure must not create {missing_out_dir}"
        );

        let existing_out_dir = test_path("sentinel-output");
        fs::create_dir_all(&existing_out_dir).unwrap();
        let sentinel = existing_out_dir.join("sentinel.txt");
        fs::write(&sentinel, "preserve me").unwrap();
        let mut duplicate_components = vec![
            component("component_a", "shared_namespace"),
            component("component_b", "shared_namespace"),
        ];
        let error = generate_components(
            &mut duplicate_components,
            &options(existing_out_dir.clone(), None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("duplicate UniFFI component namespace(s)"));
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

        assert!(out_dir
            .join("components/namespace_a/common/api.ts")
            .is_file());
        assert!(out_dir
            .join("components/namespace_a/node/component_a.rs")
            .is_file());
        assert!(!out_dir
            .join("components/namespace_b/node/component_b.rs")
            .exists());
        let node_root = fs::read_to_string(out_dir.join("node/index.ts")).unwrap();
        assert!(node_root.contains("export * as namespace_a"));
        assert!(!node_root.contains("namespace_b"));
        assert!(!node_root.contains("export * from"));
    }

    #[test]
    fn multi_component_generation_emits_one_shared_runtime_and_sorted_namespace_roots() {
        let out_dir = test_path("namespaced-tree");
        let mut selected = linked_external_record_components();
        let mut opts = options(out_dir.clone(), None);
        opts.flavors = vec![
            FlavorTarget::Electron,
            FlavorTarget::Harmony,
            FlavorTarget::Napi,
            FlavorTarget::Wasm,
        ];
        generate_components(&mut selected, &opts).unwrap();

        assert!(out_dir.join("shared/runtime.ts").is_file());
        for namespace in ["alpha", "beta"] {
            for path in [
                format!("components/{namespace}/common/runtime.ts"),
                format!("components/{namespace}/browser/index.ts"),
                format!("components/{namespace}/node/index.ts"),
                format!("components/{namespace}/electron/renderer.ts"),
                format!("components/{namespace}/harmony/index.ts"),
            ] {
                assert!(out_dir.join(path).is_file());
            }
        }
        let node_root = fs::read_to_string(out_dir.join("node/index.ts")).unwrap();
        assert!(node_root.contains("export * as alpha from \"../components/alpha/node/index.ts\";"));
        assert!(node_root.contains("export * as beta from \"../components/beta/node/index.ts\";"));
        assert!(
            node_root.find("alpha").unwrap() < node_root.rfind("beta").unwrap(),
            "root exports must be sorted by namespace: {node_root}"
        );
        assert!(!node_root.contains("export * from"));
        assert!(
            !node_root.contains("\\n"),
            "root must contain real newlines: {node_root}"
        );

        for (root, component_entry) in [
            ("browser", "browser/index.ts"),
            ("node", "node/index.ts"),
            ("electron", "electron/renderer.ts"),
            ("harmony", "harmony/index.ts"),
        ] {
            let source = fs::read_to_string(out_dir.join(root).join("index.ts")).unwrap();
            for namespace in ["alpha", "beta"] {
                assert!(
                    source.contains(&format!(
                        "export * as {namespace} from \"../components/{namespace}/{component_entry}\";"
                    )),
                    "{root} root does not expose namespace {namespace}:\n{source}"
                );
            }
            assert!(
                source.find("alpha").unwrap() < source.rfind("beta").unwrap(),
                "{root} root exports must be sorted: {source}"
            );
            assert!(
                !source.contains("export * from"),
                "{root} root must not flatten component exports: {source}"
            );
        }

        let electron_preload = fs::read_to_string(out_dir.join("electron/preload.cjs")).unwrap();
        assert_eq!(
            electron_preload
                .matches("contextBridge.exposeInMainWorld")
                .count(),
            1
        );
        assert!(electron_preload.contains("components"));
        let electron_root = fs::read_to_string(out_dir.join("electron/index.ts")).unwrap();
        assert_eq!(
            electron_root
                .matches("preload: new URL(\"./preload.cjs\", import.meta.url)")
                .count(),
            2,
            "each namespace must publish the stable aggregate preload entry: {electron_root}"
        );
        assert!(!electron_root.contains("components/alpha/electron/preload.cjs"));
        for namespace in ["alpha", "beta"] {
            let preload = fs::read_to_string(
                out_dir.join(format!("components/{namespace}/electron/preload.cjs")),
            )
            .unwrap();
            assert!(!preload.contains("contextBridge.exposeInMainWorld"));
            assert!(preload.contains("module.exports = Object.freeze"));
        }

        let snapshot = generated_tree_snapshot(&out_dir);
        for namespace in ["alpha", "beta"] {
            let api = std::str::from_utf8(
                snapshot
                    .get(&format!("components/{namespace}/common/api.ts"))
                    .unwrap(),
            )
            .unwrap();
            assert!(api.contains("export function sameApi()"));
        }
        let runtime_implementations = snapshot
            .iter()
            .filter_map(|(path, contents)| {
                std::str::from_utf8(contents)
                    .ok()
                    .filter(|contents| contents.contains("const BACKENDS_BY_NAMESPACE"))
                    .map(|_| path.clone())
            })
            .collect::<Vec<_>>();
        assert_eq!(runtime_implementations, ["shared/runtime.ts"]);
        for namespace in ["alpha", "beta"] {
            let wrapper = std::str::from_utf8(
                snapshot
                    .get(&format!("components/{namespace}/common/runtime.ts"))
                    .unwrap(),
            )
            .unwrap();
            assert!(wrapper.contains("import { createComponentRuntime }"));
            assert!(wrapper.contains(&format!("createComponentRuntime(\"{namespace}\")")));
            assert!(!wrapper.contains("const BACKENDS_BY_NAMESPACE"));
            assert!(!wrapper.contains("function createComponentRuntime"));
        }
    }

    #[test]
    fn forward_and_reverse_component_input_emit_byte_identical_trees() {
        let forward_dir = test_path("forward-tree");
        let reverse_dir = test_path("reverse-tree");
        let mut forward = linked_external_record_components();
        let mut reverse = linked_external_record_components();
        reverse.reverse();
        let mut forward_options = options(forward_dir.clone(), None);
        forward_options.flavors = vec![
            FlavorTarget::Wasm,
            FlavorTarget::Napi,
            FlavorTarget::Electron,
            FlavorTarget::Harmony,
        ];
        let mut reverse_options = forward_options.clone();
        reverse_options.out_dir = reverse_dir.clone();
        generate_components(&mut forward, &forward_options).unwrap();
        generate_components(&mut reverse, &reverse_options).unwrap();
        assert_eq!(
            generated_tree_snapshot(&forward_dir),
            generated_tree_snapshot(&reverse_dir),
            "component input order must not affect any generated byte"
        );
    }

    #[test]
    fn node_runtime_routes_same_api_to_each_component_in_both_install_orders() {
        let out_dir = test_path("node-runtime-routing");
        let mut selected = linked_external_record_components();
        generate_components(&mut selected, &options(out_dir.clone(), None)).unwrap();
        let script = out_dir.join("runtime-routing.mjs");
        fs::write(
            &script,
            r#"import assert from "node:assert/strict";

const alphaRuntime = await import("./components/alpha/common/runtime.ts");
const betaRuntime = await import("./components/beta/common/runtime.ts");
const alphaApi = await import("./components/alpha/common/api.ts");
const betaApi = await import("./components/beta/common/api.ts");

const backend = (value) => ({
  __uniffiJsRuntimeAbiVersion: 2,
  same_api: () => value,
});

alphaRuntime.__installBackend(backend("alpha-first"));
betaRuntime.__installBackend(backend("beta-second"));
assert.equal(alphaApi.sameApi(), "alpha-first");
assert.equal(betaApi.sameApi(), "beta-second");

betaRuntime.__installBackend(backend("beta-first"));
alphaRuntime.__installBackend(backend("alpha-second"));
assert.equal(alphaApi.sameApi(), "alpha-second");
assert.equal(betaApi.sameApi(), "beta-first");

assert.throws(
  () => alphaRuntime.__installBackend({ __uniffiJsRuntimeAbiVersion: 1, same_api: () => "wrong" }),
  /incompatible UniFFI JavaScript backend/,
);
assert.equal(alphaApi.sameApi(), "alpha-second");
"#,
        )
        .unwrap();
        let output = std::process::Command::new("node")
            .arg("--experimental-strip-types")
            .arg(&script)
            .output()
            .expect("Node.js is required for the JavaScript generator test suite");
        assert!(
            output.status.success(),
            "Node runtime routing test failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    #[test]
    fn unsafe_namespace_preflight_fails_before_output_mutation() {
        for (label, namespace) in [
            ("path", "../outside"),
            ("reserved", "class"),
            ("identifier", "1starts_with_a_digit"),
        ] {
            let missing_out_dir = test_path(&format!("unsafe-{label}-missing"));
            let mut selected = vec![component("component_a", namespace)];
            let error = generate_components(&mut selected, &options(missing_out_dir.clone(), None))
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("unsafe UniFFI component namespace"),
                "{error}"
            );
            assert!(
                !missing_out_dir.exists(),
                "{label} namespace created output"
            );

            let sentinel_out_dir = test_path(&format!("unsafe-{label}-sentinel"));
            fs::create_dir_all(&sentinel_out_dir).unwrap();
            let sentinel = sentinel_out_dir.join("keep.txt");
            fs::write(&sentinel, "keep").unwrap();
            let mut selected = vec![component("component_a", namespace)];
            let error =
                generate_components(&mut selected, &options(sentinel_out_dir.clone(), None))
                    .unwrap_err()
                    .to_string();
            assert!(
                error.contains("unsafe UniFFI component namespace"),
                "{error}"
            );
            assert_eq!(fs::read_to_string(&sentinel).unwrap(), "keep");
            assert_eq!(
                fs::read_dir(&sentinel_out_dir)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect::<Vec<_>>(),
                ["keep.txt"],
            );
        }
    }

    #[test]
    fn external_type_references_are_owner_qualified_and_import_the_owner_module() {
        let mut components = linked_external_record_components();
        let out_dir = test_path("external-owner");
        generate_components(&mut components, &options(out_dir.clone(), None)).unwrap();
        let beta_common = out_dir.join("components/beta/common");
        let api = fs::read_to_string(beta_common.join("api.ts")).unwrap();
        assert!(api.contains(
            "import * as __uniffi_alpha_records from \"../../alpha/common/records.ts\";"
        ));
        assert!(api.contains("value: __uniffi_alpha_records.AlphaOwned"));
        assert!(api.contains("return { id: __ret.id } as __uniffi_alpha_records.AlphaOwned;"));
        let alpha_records =
            fs::read_to_string(out_dir.join("components/alpha/common/records.ts")).unwrap();
        let beta_records =
            fs::read_to_string(out_dir.join("components/beta/common/records.ts")).unwrap();
        assert!(alpha_records.contains("export interface Shared {\n    label: string;"));
        assert!(beta_records.contains("export interface Shared {\n    count: number;"));
    }

    #[test]
    fn nested_external_payload_helpers_are_imported_for_lower_lift_and_streams() {
        let mut components = linked_nested_external_payload_components();
        let out_dir = test_path("nested-external-payloads");
        let mut opts = options(out_dir.clone(), None);
        // This is a pure linked-CI source-generation regression.  Host/addon
        // composition is intentionally outside the 05B/05C boundary.
        opts.flavors.clear();
        generate_components(&mut components, &opts).unwrap();

        let api = fs::read_to_string(out_dir.join("components/beta/common/api.ts")).unwrap();
        let custom_types =
            fs::read_to_string(out_dir.join("components/beta/common/custom-types.ts")).unwrap();
        for (alias, module, references) in [
            (
                "__uniffi_alpha_records",
                "records",
                vec!["__uniffi_alpha_records.Envelope"],
            ),
            (
                "__uniffi_alpha_enums",
                "enums",
                vec!["__uniffi_alpha_enums.AlphaChoice"],
            ),
            (
                "__uniffi_alpha_errors",
                "errors",
                vec!["new __uniffi_alpha_errors.AlphaFailure("],
            ),
            (
                "__uniffi_alpha_objects",
                "objects",
                vec!["__uniffi_alpha_objects.AlphaObject.__fromHandle("],
            ),
            (
                "__uniffi_alpha_callbacks",
                "callbacks",
                vec!["__uniffi_alpha_callbacks.__uniffiLowerCallbackAlphaCallback("],
            ),
            (
                "__uniffi_alpha_custom_types",
                "custom-types",
                vec![
                    "__uniffi_alpha_custom_types.__uniffiLowerCustomAlphaCustom(",
                    "__uniffi_alpha_custom_types.__uniffiLiftCustomAlphaCustom(",
                ],
            ),
        ] {
            assert!(
                api.contains(&format!(
                    "import * as {alias} from \"../../alpha/common/{module}.ts\";"
                )),
                "missing owner-qualified {module} import:\n{api}"
            );
            for reference in references {
                assert!(
                    api.contains(reference),
                    "{alias} import has no matching generated reference `{reference}`:\n{api}"
                );
            }
        }

        assert!(
            api.contains("import { __uniffiLowerCallbackBetaCallback } from \"./callbacks.ts\";")
        );
        assert!(api.contains("__uniffiLowerCallbackBetaCallback(value)"));
        assert!(
            !api.lines()
                .any(|line| line.contains("from \"./callbacks.ts\"")
                    && line.contains("AlphaCallback")),
            "external callback helper must not be imported from beta callbacks.ts:\n{api}"
        );

        assert!(
            api.lines().any(|line| {
                line.contains("from \"./custom-types.ts\"")
                    && line.contains("__uniffiLowerCustomBetaCustom")
                    && line.contains("__uniffiLiftCustomBetaCustom")
            }),
            "local custom helpers are missing from beta custom-types.ts:\n{api}"
        );
        assert!(api.contains("__uniffiLowerCustomBetaCustom("));
        assert!(api.contains("__uniffiLiftCustomBetaCustom("));
        assert!(
            !api.lines()
                .any(|line| line.contains("from \"./custom-types.ts\"")
                    && line.contains("AlphaCustom")),
            "external custom helper must not be imported from beta custom-types.ts:\n{api}"
        );
        assert!(custom_types.contains("export type BetaCustom"));
        assert!(
            !custom_types.contains("AlphaCustom"),
            "beta must not generate a colliding AlphaCustom conversion:\n{custom_types}"
        );

        let stream_start = api.find("export function streamEnvelope").unwrap();
        let input_start = api.find("export function sendInput").unwrap();
        let stream = &api[stream_start..];
        assert!(stream.contains("createUniFfiStream<__uniffi_alpha_records.Envelope"));
        assert!(stream.contains("__uniffi_alpha_objects.AlphaObject.__fromHandle("));
        assert!(stream.contains("new __uniffi_alpha_errors.AlphaFailure("));
        let input = &api[input_start..];
        assert!(input.contains("createUniffiInputStream(input"));
        assert!(input.contains("__uniffi_alpha_callbacks.__uniffiLowerCallbackAlphaCallback("));
        assert!(input.contains("__uniffi_alpha_custom_types.__uniffiLowerCustomAlphaCustom("));
    }

    #[test]
    fn duplicate_normalized_crate_roots_fail_before_output_mutation() {
        let forward = duplicate_normalized_root_components();
        let mut reverse = duplicate_normalized_root_components();
        reverse.reverse();
        let forward_error = preflight_component_layout(
            &forward,
            &options(test_path("duplicate-root-forward"), None),
        )
        .unwrap_err()
        .to_string();
        let reverse_error = preflight_component_layout(
            &reverse,
            &options(test_path("duplicate-root-reverse"), None),
        )
        .unwrap_err()
        .to_string();

        assert_eq!(forward_error, reverse_error);
        assert!(forward_error.contains("ambiguous normalized UniFFI component crate root owner(s)"));
        for owner in [
            "normalized crate root `shared`: crate `shared`, namespace `alpha`, crate `shared`, namespace `beta`",
            "normalized crate root `a_b`: crate `a-b`, namespace `hyphen`, crate `a_b`, namespace `underscore`",
        ] {
            assert!(forward_error.contains(owner), "missing `{owner}`:\n{forward_error}");
        }
        let shared = Type::Record {
            module_path: "shared".to_string(),
            name: "Shared".to_string(),
        };
        assert!(type_owner_component(&forward, &shared).is_none());

        let missing_out_dir = test_path("duplicate-root-missing");
        let mut selected = duplicate_normalized_root_components();
        let error = generate_components(&mut selected, &options(missing_out_dir.clone(), None))
            .unwrap_err()
            .to_string();
        assert_eq!(error, forward_error);
        assert!(!missing_out_dir.exists());

        let sentinel_out_dir = test_path("duplicate-root-sentinel");
        fs::create_dir_all(&sentinel_out_dir).unwrap();
        fs::write(sentinel_out_dir.join("keep.txt"), "keep").unwrap();
        let before = generated_tree_snapshot(&sentinel_out_dir);
        let mut selected = duplicate_normalized_root_components();
        let error = generate_components(&mut selected, &options(sentinel_out_dir.clone(), None))
            .unwrap_err()
            .to_string();
        assert_eq!(error, forward_error);
        assert_eq!(generated_tree_snapshot(&sentinel_out_dir), before);
    }

    #[test]
    fn unselected_external_type_owner_fails_before_output_mutation() {
        let mut components = linked_external_record_components();
        let beta = components.pop().unwrap();
        let out_dir = test_path("missing-external-owner");
        let error = preflight_component_layout(
            std::slice::from_ref(&beta),
            &options(out_dir.clone(), None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("owner is not selected"), "{error}");
        assert!(!out_dir.exists());
    }

    #[test]
    fn plural_host_generation_is_deferred_before_source_or_host_output_mutation() {
        let out_dir = test_path("plural-host-source");
        let host_dir = test_path("plural-host-host");
        let mut opts = options(out_dir.clone(), None);
        opts.host_crates = Some(HostCrateOptions {
            manifest_path: test_path("plural-host-manifest").join("Cargo.toml"),
            host_crates_dir: host_dir.clone(),
            logical_host_crates_dir: None,
            logical_out_dir: None,
            ohos_rs_dir: None,
        });
        let error = generate_components(&mut components(), &opts)
            .unwrap_err()
            .to_string();
        assert!(error.contains("deferred to stage 05C"), "{error}");
        assert!(!out_dir.exists());
        assert!(!host_dir.exists());
    }
}
