//! In-memory JavaScript package preparation.
//!
//! A package is the smallest publishable unit: public facade, declarations,
//! private engine bridge and host source are prepared together and written by
//! one call to [`GeneratedPackage::write_to`].  No manifest, sidecar, hash or
//! identity file is produced.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use fs_err as fs;
use uniffi_js_abi::PublicTarget;

use crate::engines;
use crate::frontend::NormalizedPackage;
use crate::{FlavorTarget, GenerateJsOptions, WasmPostLinkTarget};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PackageFileRole {
    Source,
    Declaration,
    NativeHost,
    PlatformConfig,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PackageFile {
    pub path: String,
    pub bytes: Vec<u8>,
    pub role: PackageFileRole,
}

/// The build-time information a target builder needs after package
/// preparation.  All paths are relative to the package root and all names
/// come from the same host plan that rendered the host Cargo project.  This
/// is intentionally a small in-memory value: it is never serialized into a
/// manifest or recovered by parsing generated source.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostBuildSpec {
    /// The canonical public target served by this host crate.
    pub target: PublicTarget,
    /// Package-relative host crate directory containing `Cargo.toml`.
    pub crate_root: Utf8PathBuf,
    /// Rust library target name used by Cargo and the native artifact.
    pub lib_target: String,
    /// Package-relative destination for the final native artifact.
    pub native_artifact: Utf8PathBuf,
    /// Dependency key used to scope downstream Cargo features for this host.
    pub core_dependency_key: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedPackage {
    pub normalized: NormalizedPackage,
    pub files: Vec<PackageFile>,
    /// Frozen host build inputs produced alongside `files`.
    pub host_specs: Vec<HostBuildSpec>,
}

impl GeneratedPackage {
    /// Prepare all package bytes without mutating the destination tree.
    pub fn prepare(normalized: &NormalizedPackage, options: &GenerateJsOptions) -> Result<Self> {
        let host_options = &options.host_crates;
        let facade = uniffi_js_facade::build(&normalized.api, &normalized.bridge, &normalized.rust)
            .map_err(|error| anyhow!("JavaScript facade construction failed: {error}"))?;
        let source_prefix = source_prefix(&options.package_root, &options.out_dir)?;

        let mut files = Vec::new();
        let selected_targets = options
            .flavors
            .iter()
            .map(|flavor| match flavor {
                FlavorTarget::Napi | FlavorTarget::Electron => PublicTarget::NodeNapi,
                FlavorTarget::Wasm => PublicTarget::BrowserWasm,
                FlavorTarget::Harmony => PublicTarget::OhosNapi,
            })
            .collect::<BTreeSet<_>>();
        let wants_electron = options
            .flavors
            .iter()
            .any(|flavor| matches!(flavor, FlavorTarget::Electron));

        // Plan host projects before rendering any platform entrypoint.  The
        // same plan supplies deterministic artifact stems and loader paths
        // to every wrapper below; no wrapper is allowed to rediscover those
        // values from a generated directory.
        let effective_artifact_dir = options
            .artifact_dir
            .as_deref()
            .unwrap_or(options.out_dir.as_path());
        let selected_target_vec = selected_targets.iter().copied().collect::<Vec<_>>();
        let rendered_hosts = crate::host_crates::render_host_package(
            &normalized,
            host_options,
            &selected_target_vec,
            &options.package_root,
            Some(effective_artifact_dir),
        )
        .context("planning package host crates")?;
        let host_plan = &rendered_hosts.plan;
        let host_specs = selected_target_vec
            .iter()
            .filter_map(|target| {
                host_plan.flavor(*target).map(|flavor| HostBuildSpec {
                    target: *target,
                    crate_root: flavor.crate_root.clone(),
                    lib_target: flavor.lib_target.clone(),
                    native_artifact: flavor.native_artifact.clone(),
                    core_dependency_key: host_plan.core_dependency_key().to_owned(),
                })
            })
            .collect::<Vec<_>>();

        // Shared ECMAScript inventory is physically reused by Node and Web.
        // A target root only contains namespace re-exports and never copies a
        // second implementation.
        if selected_targets.contains(&PublicTarget::NodeNapi)
            || selected_targets.contains(&PublicTarget::BrowserWasm)
        {
            for file in facade.shared_files() {
                files.push(PackageFile {
                    path: file.path.clone(),
                    bytes: file.bytes.clone(),
                    role: match file.role {
                        uniffi_js_facade::PublicFileRole::Declaration => {
                            PackageFileRole::Declaration
                        }
                        uniffi_js_facade::PublicFileRole::Runtime
                        | uniffi_js_facade::PublicFileRole::Implementation => {
                            PackageFileRole::Source
                        }
                    },
                });
            }
            if selected_targets.contains(&PublicTarget::NodeNapi) {
                let node = render_node_entrypoint(
                    &normalized,
                    host_plan
                        .flavor(PublicTarget::NodeNapi)
                        .context("N-API host plan missing for Node entrypoint")?,
                    &source_prefix,
                )?;
                files.push(source("node/index.js", node.0));
                files.push(declaration("node/index.d.ts", node.1));
            }
            if wants_electron {
                let electron = render_electron_entrypoint(
                    &normalized,
                    host_plan
                        .flavor(PublicTarget::NodeNapi)
                        .context("N-API host plan missing for Electron entrypoint")?,
                    &source_prefix,
                );
                files.push(source("electron/preload.cjs", electron.0));
                files.push(source("electron/index.js", electron.1));
                files.push(declaration("electron/index.d.ts", electron.2));
            }
            if selected_targets.contains(&PublicTarget::BrowserWasm) {
                let browser = render_browser_entrypoint(
                    &normalized,
                    host_plan
                        .flavor(PublicTarget::BrowserWasm)
                        .context("Wasm host plan missing for browser entrypoint")?,
                    &source_prefix,
                )?;
                // Keep the stable public entrypoint separate from the
                // generated backend adapter.  The wasm build command may
                // replace `browser/index.js` with an auto-init wrapper after
                // post-link; it must never overwrite the package's actual
                // namespace/session implementation or make it self-import.
                files.push(source("browser/backend.js", browser.0));
                files.push(source("browser/index.js", browser.1));
                files.push(declaration("browser/index.d.ts", browser.2));
            }
        }

        if selected_targets.contains(&PublicTarget::OhosNapi) {
            for file in facade.ark_files() {
                files.push(PackageFile {
                    path: file.path.clone(),
                    bytes: file.bytes.clone(),
                    role: match file.role {
                        uniffi_js_facade::PublicFileRole::Declaration => {
                            PackageFileRole::Declaration
                        }
                        uniffi_js_facade::PublicFileRole::Runtime
                        | uniffi_js_facade::PublicFileRole::Implementation => {
                            PackageFileRole::Source
                        }
                    },
                });
            }
            files.push(PackageFile {
                path: "native/index.d.ts".into(),
                bytes: b"export function __uniffi_backend_factory(host: unknown): unknown;\n"
                    .to_vec(),
                role: PackageFileRole::Declaration,
            });

            // ArkTS's public Index.ets is the platform composition root.  The
            // strict facade remains the sole implementation of API lowering;
            // this small composition opens the native factory once and binds
            // the same namespace values to that session.  ArkTS requires all
            // imports to precede executable declarations, so prepend the
            // private native import and append only the executable suffix.
            if let Some(index) = files.iter_mut().find(|file| file.path == "Index.ets") {
                let flavor = host_plan
                    .flavor(PublicTarget::OhosNapi)
                    .context("OHOS host plan missing for Harmony entrypoint")?;
                let (native_import, suffix) =
                    render_harmony_binding(&normalized, flavor, &source_prefix);
                let mut composed = native_import.into_bytes();
                composed.extend_from_slice(&index.bytes);
                index.bytes = composed;
                index.bytes.extend_from_slice(suffix.as_bytes());
            }
            if let Some(declarations) = files.iter_mut().find(|file| file.path == "Index.d.ets") {
                declarations
                    .bytes
                    .extend_from_slice(render_harmony_declarations(&normalized).as_bytes());
            }
        }

        if selected_targets.contains(&PublicTarget::BrowserWasm) {
            let wasm_flavor = host_plan
                .flavor(PublicTarget::BrowserWasm)
                .context("Wasm host plan missing for expansion context")?;
            let wasm_manifest_dir =
                absolute_lexical(&options.package_root)?.join(&wasm_flavor.crate_root);
            let expansion_context = wasm_bindgen_uniffi_engine::ExpansionContext::new(
                wasm_manifest_dir.as_std_path(),
                &wasm_flavor.lib_target,
                &wasm_flavor.crate_version,
                ["target_arch=wasm".to_owned()],
                "wasm32-unknown-unknown",
            )
            .map_err(|error| anyhow!("building the explicit Wasm expansion context: {error:?}"))?;
            files.push(PackageFile {
                path: "native/wasm.rs".into(),
                bytes: engines::wasm_source(&normalized, expansion_context)
                    .context("rendering the Wasm engine adapter")?
                    .into_bytes(),
                role: PackageFileRole::NativeHost,
            });
        }

        // Engine source is private host input.  It is never re-read from a
        // generated directory; the host planner receives these bytes from the
        // same package value.
        if selected_targets.contains(&PublicTarget::NodeNapi) {
            files.push(PackageFile {
                path: "native/node.rs".into(),
                bytes: engines::napi_source(&normalized)
                    .context("rendering the N-API engine adapter")?
                    .into_bytes(),
                role: PackageFileRole::NativeHost,
            });
        }
        if selected_targets.contains(&PublicTarget::OhosNapi) {
            let source =
                engines::ohos_source(&normalized).context("rendering the OHOS engine adapter")?;
            files.push(PackageFile {
                path: "native/ohos.rs".into(),
                bytes: source.into_bytes(),
                role: PackageFileRole::NativeHost,
            });
        }

        // Source and bridge files belong below the requested source prefix;
        // host crates are package-level inputs and remain at package-root
        // paths so Cargo can consume them directly.
        prefix_source_files(&mut files, &source_prefix);
        files.extend(rendered_hosts.files.clone());

        validate_files(&files)?;
        Ok(Self {
            normalized: normalized.clone(),
            files,
            host_specs,
        })
    }

    /// Publish an already prepared package.  This is the only public writer;
    /// callers should stage beside the final root and replace the root after
    /// this method succeeds.
    pub fn write_to(&self, root: &Utf8Path) -> Result<()> {
        for file in &self.files {
            let path = safe_join(root, &file.path)?;
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, &file.bytes)?;
        }
        Ok(())
    }

    /// Run the pinned wasm-bindgen post-link engine from this package's frozen
    /// normalized plan and publish its loader/wasm pair to `out_dir`.
    pub fn emit_wasm_post_link(
        &self,
        wasm_path: &Utf8Path,
        module_name: &str,
        target: WasmPostLinkTarget,
        out_dir: &Utf8Path,
    ) -> Result<()> {
        if !self.target_enabled(PublicTarget::BrowserWasm) {
            bail!("wasm post-link requires a package with the Wasm target enabled");
        }
        let target = match target {
            WasmPostLinkTarget::Web => wasm_bindgen_uniffi_engine::PostLinkTarget::Web,
            WasmPostLinkTarget::Bundler => wasm_bindgen_uniffi_engine::PostLinkTarget::Bundler,
            WasmPostLinkTarget::Node => wasm_bindgen_uniffi_engine::PostLinkTarget::Node,
        };
        let output = engines::wasm_post_link(
            &self.normalized,
            wasm_path.as_std_path(),
            module_name,
            target,
        )?;
        output
            .emit(out_dir.as_std_path())
            .map_err(|error| anyhow!("emitting wasm post-link output: {error}"))
    }

    pub fn file(&self, path: &str) -> Option<&PackageFile> {
        self.files.iter().find(|file| file.path == path)
    }

    pub fn target_enabled(&self, target: PublicTarget) -> bool {
        self.normalized.build_targets.contains(&target)
    }

    /// Return the exact host build inputs planned during preparation.
    pub fn host_spec(&self, target: PublicTarget) -> Option<&HostBuildSpec> {
        self.host_specs.iter().find(|spec| spec.target == target)
    }

    pub fn node_host_spec(&self) -> Option<&HostBuildSpec> {
        self.host_spec(PublicTarget::NodeNapi)
    }

    pub fn wasm_host_spec(&self) -> Option<&HostBuildSpec> {
        self.host_spec(PublicTarget::BrowserWasm)
    }

    pub fn ohos_host_spec(&self) -> Option<&HostBuildSpec> {
        self.host_spec(PublicTarget::OhosNapi)
    }
}

fn source(path: impl Into<String>, source: String) -> PackageFile {
    PackageFile {
        path: path.into(),
        bytes: source.into_bytes(),
        role: PackageFileRole::Source,
    }
}

fn declaration(path: impl Into<String>, source: String) -> PackageFile {
    PackageFile {
        path: path.into(),
        bytes: source.into_bytes(),
        role: PackageFileRole::Declaration,
    }
}

fn render_node_entrypoint(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    source_prefix: &str,
) -> Result<(String, String)> {
    let from_dir = prefixed_path(source_prefix, "node");
    let runtime = relative_module_specifier(
        &from_dir,
        &prefixed_path(source_prefix, "shared/uniffi_runtime.js"),
    );
    let addon = relative_module_specifier(&from_dir, flavor.native_artifact.as_str());
    let mut source = String::from(
        "// AUTOGENERATED UniFFI Node platform entry; the addon and facade share one session.\n",
    );
    source.push_str(&format!(
        "import {{ createRequire }} from \"node:module\";\nimport {{ BackendSession, Host }} from \"{runtime}\";\nconst __require = createRequire(import.meta.url);\nconst __native = __require({addon:?});\nif (typeof __native.__uniffi_backend_factory !== \"function\") throw new Error(\"UniFFI native host does not export __uniffi_backend_factory\");\nconst __host = new Host();\nconst __backend = __native.__uniffi_backend_factory(__host);\nexport const session = new BackendSession(__backend, __host);\n"
    ));
    let mut declaration = format!(
        "import type {{ BackendSession }} from \"{runtime}\";\nexport declare const session: BackendSession;\n"
    );
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        let component_path = relative_module_specifier(
            &from_dir,
            &prefixed_path(
                source_prefix,
                &format!("components/{}/index.js", component.public_namespace),
            ),
        );
        source.push_str(&format!(
            "import * as __{namespace}Module from {component_path:?};\nexport const {namespace} = __{namespace}Module.createNamespace(session);\n"
        ));
        declaration.push_str(&format!(
            "export declare const {namespace}: import({component_path:?}).Namespace;\n"
        ));
    }
    source.push_str("export async function close() { return session.close(); }\n");
    declaration.push_str("export declare function close(): Promise<void>;\n");
    Ok((source, declaration))
}

fn render_browser_entrypoint(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    source_prefix: &str,
) -> Result<(String, String, String)> {
    let from_dir = prefixed_path(source_prefix, "browser");
    let runtime = relative_module_specifier(
        &from_dir,
        &prefixed_path(source_prefix, "shared/uniffi_runtime.js"),
    );
    let glue = relative_module_specifier(
        &from_dir,
        flavor
            .loader_artifact
            .as_ref()
            .context("Wasm host plan has no wasm-bindgen loader path")?
            .as_str(),
    );
    let wasm = relative_module_specifier(&from_dir, flavor.native_artifact.as_str());
    // `backend.js` is deliberately inert on module evaluation.  The stable
    // platform index (or Mini Program entry) supplies the planned glue and
    // calls this one coordinator, so importing a package never starts a
    // second wasm instance with an unpatched/default input.
    let mut source = format!(
        "// AUTOGENERATED UniFFI browser backend; initialization is explicit and idempotent.\nimport {{ BackendSession, Host }} from {runtime:?};\n"
    );
    source.push_str("export let session;\n");
    let mut declaration = format!(
        "import type {{ BackendSession }} from {runtime:?};\nexport interface ReadyApi {{ readonly session: BackendSession; close(): Promise<void>;\n"
    );
    let mut component_bindings = Vec::new();
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        let component_path = relative_module_specifier(
            &from_dir,
            &prefixed_path(
                source_prefix,
                &format!("components/{}/index.js", component.public_namespace),
            ),
        );
        source.push_str(&format!("export let {namespace};\n"));
        component_bindings.push((namespace.clone(), component_path.clone()));
        source.push_str(&format!(
            "import * as __{namespace}Module from {component_path:?};\n"
        ));
        declaration.push_str(&format!(
            "  readonly {namespace}: import({component_path:?}).Namespace;\n"
        ));
    }
    source.push_str("let __bootPromise;\nexport function initWithGlue(customGlue, input) {\n  if (__bootPromise !== undefined) return __bootPromise;\n  __bootPromise = (async () => {\n    const __glue = await customGlue;\n    const __initialized = typeof __glue.default === \"function\" ? await __glue.default(input) : __glue;\n    const __exports = __initialized && typeof __initialized.__uniffi_backend_factory === \"function\" ? __initialized : __glue;\n    if (typeof __exports.__uniffi_backend_factory !== \"function\") throw new Error(\"UniFFI wasm host does not export __uniffi_backend_factory\");\n    const __host = new Host();\n    const __backend = __exports.__uniffi_backend_factory(__host);\n    session = new BackendSession(__backend, __host);\n    const __api = { session, close: () => session.close() };\n");
    for (namespace, _) in &component_bindings {
        source.push_str(&format!(
            "    {namespace} = __{namespace}Module.createNamespace(session);\n    __api.{namespace} = {namespace};\n"
        ));
    }
    source.push_str("    return Object.freeze(__api);\n  })();\n  return __bootPromise;\n}\nexport async function close() {\n  await __bootPromise;\n  return session.close();\n}\n");
    declaration.push_str("}\n");
    for (namespace, component_path) in &component_bindings {
        declaration.push_str(&format!(
            "export declare let {namespace}: import({component_path:?}).Namespace;\n"
        ));
    }
    declaration.push_str("export declare let session: BackendSession;\nexport const ready: Promise<ReadyApi>;\nexport function init(input?: unknown): Promise<ReadyApi>;\nexport function close(): Promise<void>;\n");
    // The browser index is the only loader.  It imports the planned glue
    // once, starts the backend coordinator, and explicitly re-exports its
    // public bindings without exposing `initWithGlue` as a compatibility
    // alias.
    let mut index = format!(
        "// AUTOGENERATED UniFFI browser loader.\nimport * as __backend from \"./backend.js\";\nimport * as __glue from {glue:?};\nconst __defaultWasm = new URL({wasm:?}, import.meta.url);\nexport const ready = __backend.initWithGlue(__glue, __defaultWasm);\nexport function init(input) {{ return __backend.initWithGlue(__glue, input === undefined ? __defaultWasm : input); }}\nexport {{ session, close"
    );
    for (namespace, _) in &component_bindings {
        index.push_str(&format!(", {namespace}"));
    }
    index.push_str(" } from \"./backend.js\";\n");
    Ok((source, index, declaration))
}

fn render_electron_entrypoint(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    source_prefix: &str,
) -> (String, String, String) {
    let from_dir = prefixed_path(source_prefix, "electron");
    let addon = relative_module_specifier(&from_dir, flavor.native_artifact.as_str());
    let mut preload = String::from(
        "// AUTOGENERATED Electron preload; the raw N-API backend stays here.\n\
const { contextBridge } = require(\"electron\");\n\
const { createRequire } = require(\"node:module\");\n\
const __require = createRequire(__filename);\n\
const __native = __require(",
    );
    preload.push_str(&format!("{addon:?}"));
    preload.push_str(
        ");\n\
const __plainError = (raw) => {\n\
  if (raw && typeof raw === \"object\") return {\n\
    errorName: typeof raw.errorName === \"string\" ? raw.errorName : \"UniffiUnknownError\",\n\
    variant: typeof raw.variant === \"string\" ? raw.variant : (typeof raw.tag === \"string\" ? raw.tag : null),\n\
    data: Object.prototype.hasOwnProperty.call(raw, \"data\") ? raw.data : raw,\n\
    message: typeof raw.message === \"string\" ? raw.message : String(raw),\n\
    descriptor: raw.descriptor ?? null,\n\
    stack: typeof raw.stack === \"string\" ? raw.stack : undefined,\n\
  };\n\
  return { errorName: \"UniffiUnknownError\", variant: null, data: null, message: String(raw), descriptor: null };\n\
};\n\
if (typeof __native.__uniffi_backend_factory !== \"function\") throw __plainError({ errorName: \"UniffiBackendFactory\", message: \"UniFFI native host does not export __uniffi_backend_factory\" });\n\
let __rendererHost = null;\n\
const __requiredHostMethods = [\"retainCallback\", \"releaseCallback\", \"invokeCallbackSync\", \"invokeCallbackAsync\", \"invokeCallbackSyncResult\", \"invokeCallbackAsyncResult\", \"pullInputStream\", \"cancelInputStream\", \"releaseInputStream\"];\n\
function __bindRendererHost(host) {\n\
  if (!host || typeof host !== \"object\") throw __plainError({ errorName: \"UniffiElectronHost\", message: \"renderer Host must be an object\" });\n\
  for (const method of __requiredHostMethods) if (typeof host[method] !== \"function\") throw __plainError({ errorName: \"UniffiElectronHost\", message: \"renderer Host is missing \" + method });\n\
  if (__rendererHost !== null) throw __plainError({ errorName: \"UniffiElectronHost\", message: \"renderer Host can only be bound once\" });\n\
  __rendererHost = host;\n\
}\n\
function __hostCall(method, args) {\n\
  if (__rendererHost === null) throw __plainError({ errorName: \"UniffiElectronHost\", message: \"renderer Host is not bound\" });\n\
  try { return __rendererHost[method](...args); } catch (error) { throw __plainError(error); }\n\
}\n\
function __hostCallAsync(method, args) {\n\
  try { return Promise.resolve(__hostCall(method, args)).catch((error) => Promise.reject(__plainError(error))); }\n\
  catch (error) { return Promise.reject(__plainError(error)); }\n\
}\n\
const __nativeHost = {\n\
  retainCallback: (...args) => __hostCall(\"retainCallback\", args),\n\
  releaseCallback: (...args) => __hostCall(\"releaseCallback\", args),\n\
  invokeCallbackSync: (...args) => __hostCall(\"invokeCallbackSync\", args),\n\
  invokeCallbackAsync: (...args) => __hostCallAsync(\"invokeCallbackAsync\", args),\n\
  invokeCallbackSyncResult: (...args) => __hostCall(\"invokeCallbackSyncResult\", args),\n\
  invokeCallbackAsyncResult: (...args) => __hostCallAsync(\"invokeCallbackAsyncResult\", args),\n\
  pullInputStream: (...args) => __hostCallAsync(\"pullInputStream\", args),\n\
  cancelInputStream: (...args) => __hostCallAsync(\"cancelInputStream\", args),\n\
  releaseInputStream: (...args) => __hostCall(\"releaseInputStream\", args),\n};\nconst __backend = __native.__uniffi_backend_factory(__nativeHost);\nconst __requiredBackendMethods = [\"invokeSync\", \"invokeAsync\", \"releaseObject\", \"cancelOutputStream\", \"releaseOutputStream\", \"close\"];\nfunction __backendMethod(name) {\n  if (!__backend || typeof __backend[name] !== \"function\") throw __plainError({ errorName: \"UniffiBackendProtocol\", message: \"native backend is missing \" + name });\n  return __backend[name];\n}\nfor (const method of __requiredBackendMethods) __backendMethod(method);\nfunction __backendCall(name, args) {\n  try { return __backendMethod(name).apply(__backend, args); } catch (error) { throw __plainError(error); }\n}\nfunction __backendCallAsync(name, args) {\n  try { return Promise.resolve(__backendCall(name, args)).catch((error) => Promise.reject(__plainError(error))); }\n  catch (error) { return Promise.reject(__plainError(error)); }\n}\ncontextBridge.exposeInMainWorld(\"__uniffiBackend\", Object.freeze({\n  bindHost(host) { __bindRendererHost(host); },\n  invokeSync: (...args) => __backendCall(\"invokeSync\", args),\n  invokeAsync: (...args) => __backendCallAsync(\"invokeAsync\", args),\n  releaseObject: (...args) => __backendCall(\"releaseObject\", args),\n  cancelOutputStream: (...args) => __backendCallAsync(\"cancelOutputStream\", args),\n  releaseOutputStream: (...args) => __backendCall(\"releaseOutputStream\", args),\n  close: (...args) => __backendCallAsync(\"close\", args),\n}));\n",
    );
    // Keep the generated preload opaque: renderer code never receives the
    // native addon or BackendSession object, only an invoke capability.
    if package.api.components.is_empty() {
        preload.push_str("// no namespaces selected\n");
    }
    let runtime = relative_module_specifier(
        &from_dir,
        &prefixed_path(source_prefix, "shared/uniffi_runtime.js"),
    );
    let mut renderer = format!(
        "// AUTOGENERATED Electron renderer entry; the shared facade owns conversion and lifecycle.\nimport {{ BackendSession, Host }} from {runtime:?};\nconst __bridge = window.__uniffiBackend;\nconst __host = new Host();\n__bridge.bindHost({{\n  retainCallback: (...args) => __host.retainCallback(...args),\n  releaseCallback: (...args) => __host.releaseCallback(...args),\n  invokeCallbackSync: (...args) => __host.invokeCallbackSync(...args),\n  invokeCallbackAsync: (...args) => __host.invokeCallbackAsync(...args),\n  invokeCallbackSyncResult: (...args) => __host.invokeCallbackSyncResult(...args),\n  invokeCallbackAsyncResult: (...args) => __host.invokeCallbackAsyncResult(...args),\n  pullInputStream: (...args) => __host.pullInputStream(...args),\n  cancelInputStream: (...args) => __host.cancelInputStream(...args),\n  releaseInputStream: (...args) => __host.releaseInputStream(...args),\n}});\nconst __backend = {{ invokeSync: (...args) => __bridge.invokeSync(...args), invokeAsync: (...args) => __bridge.invokeAsync(...args), releaseObject: (...args) => __bridge.releaseObject(...args), cancelOutputStream: (...args) => __bridge.cancelOutputStream(...args), releaseOutputStream: (...args) => __bridge.releaseOutputStream(...args), close: (...args) => __bridge.close(...args) }};\nexport const session = new BackendSession(__backend, __host);\n"
    );
    renderer = format!(
        "const __uniffiBridgeCheck = globalThis.window && globalThis.window.__uniffiBackend;\nif (!__uniffiBridgeCheck || typeof __uniffiBridgeCheck !== \"object\") throw new Error(\"UniFFI Electron preload bridge is unavailable\");\nfor (const __method of [\"bindHost\", \"invokeSync\", \"invokeAsync\", \"releaseObject\", \"cancelOutputStream\", \"releaseOutputStream\", \"close\"]) if (typeof __uniffiBridgeCheck[__method] !== \"function\") throw new Error(\"UniFFI Electron preload bridge is missing \" + __method);\n{}",
        renderer
    );
    let mut declaration = format!(
        "import type {{ BackendSession, Host }} from {runtime:?};\nexport interface ElectronBackendBridge {{ bindHost(host: unknown): void; invokeSync(operationId: number, args: unknown[]): unknown; invokeAsync(operationId: number, args: unknown[]): Promise<unknown>; releaseObject(handle: unknown): void; cancelOutputStream(handle: unknown): Promise<void>; releaseOutputStream(handle: unknown): void; close(): Promise<void>; }}\ndeclare global {{ interface Window {{ __uniffiBackend: ElectronBackendBridge; }} }}\nexport declare const session: BackendSession;\n"
    );
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        let component_path = relative_module_specifier(
            &from_dir,
            &prefixed_path(
                source_prefix,
                &format!("components/{}/index.js", component.public_namespace),
            ),
        );
        renderer.push_str(&format!(
            "import * as __{namespace}Module from {component_path:?};\nexport const {namespace} = __{namespace}Module.createNamespace(session);\n"
        ));
        declaration.push_str(&format!(
            "export declare const {namespace}: import({component_path:?}).Namespace;\n"
        ));
    }
    renderer.push_str("export async function close() { return session.close(); }\n");
    declaration.push_str("export declare function close(): Promise<void>;\n");
    (preload, renderer, declaration)
}

fn render_harmony_binding(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    _source_prefix: &str,
) -> (String, String) {
    let native_module = format!("lib{}.so", flavor.lib_target);
    let native_import = format!("import * as __uniffiNative from {native_module:?};\n");
    let mut source = String::from(
        "\n// AUTOGENERATED Harmony platform composition.\nconst __uniffiHost = new Host();\nconst __uniffiBackend: ArkBackend = __uniffiNative.__uniffi_backend_factory(__uniffiHost) as ArkBackend;\nexport const session = new BackendSession(__uniffiBackend, __uniffiHost);\nconst __uniffiApi = createNamespace(session);\n",
    );
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        source.push_str(&format!(
            "export const {namespace} = __uniffiApi.{namespace};\n"
        ));
    }
    source.push_str("export async function close() { return session.close(); }\n");
    (native_import, source)
}

fn render_harmony_declarations(package: &NormalizedPackage) -> String {
    let mut declarations = String::from("\nexport declare const session: BackendSession;\n");
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        declarations.push_str(&format!(
            "export declare const {namespace}: {namespace}Api;\n"
        ));
    }
    declarations.push_str("export declare function close(): Promise<void>;\n");
    declarations
}

fn prefixed_path(prefix: &str, path: &str) -> String {
    if prefix.is_empty() {
        path.to_owned()
    } else {
        format!("{prefix}/{path}")
    }
}

fn relative_module_specifier(from_dir: &str, target: &str) -> String {
    let from = from_dir
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let target = target
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let mut common = 0usize;
    while common < from.len() && common < target.len() && from[common] == target[common] {
        common += 1;
    }
    let mut parts = Vec::new();
    for _ in common..from.len() {
        parts.push("..".to_owned());
    }
    parts.extend(target[common..].iter().map(|part| (*part).to_owned()));
    let result = parts.join("/");
    if result.starts_with('.') {
        result
    } else {
        format!("./{result}")
    }
}

fn js_ident(value: &str) -> String {
    let mut output = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if output.is_empty() {
        output.push('_');
    }
    if output.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        output.insert(0, '_');
    }
    output
}

/// Compute the lexical source prefix below a package root. Generation runs
/// before either directory exists, so canonicalization would make output
/// depend on ambient filesystem state and is intentionally avoided.
fn source_prefix(package_root: &Utf8Path, out_dir: &Utf8Path) -> Result<String> {
    let root = absolute_lexical(package_root)?;
    let out = absolute_lexical(out_dir)?;
    let relative = out.strip_prefix(&root).map_err(|_| {
        anyhow!(
            "JavaScript output directory `{out_dir}` must be inside package root `{package_root}`"
        )
    })?;
    Ok(relative.as_str().trim_matches('/').replace('\\', "/"))
}

fn absolute_lexical(path: &Utf8Path) -> Result<Utf8PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = Utf8PathBuf::from_path_buf(std::env::current_dir()?)
        .map_err(|path| anyhow!("current directory is not UTF-8: {}", path.display()))?;
    Ok(cwd.join(path))
}

fn prefix_source_files(files: &mut [PackageFile], prefix: &str) {
    if prefix.is_empty() {
        return;
    }
    for file in files {
        // Native adapters and the minimal native declaration are package
        // inputs consumed by Cargo/OHOS readers.  They intentionally stay at
        // package-root `native/*`; only public facade/platform files follow
        // the requested generated source prefix.
        if file.path == "native/index.d.ts"
            || file.path == "native/node.rs"
            || file.path == "native/wasm.rs"
            || file.path == "native/ohos.rs"
        {
            continue;
        }
        file.path = format!("{prefix}/{}", file.path);
    }
}

fn validate_files(files: &[PackageFile]) -> Result<()> {
    let mut paths = BTreeSet::new();
    for file in files {
        if !paths.insert(file.path.clone()) {
            bail!("duplicate generated package path `{}`", file.path);
        }
        if file.path.ends_with(".ts") && !file.path.ends_with(".d.ts") {
            bail!(
                "generated package contains a runtime TypeScript file `{}`",
                file.path
            );
        }
        if file.path.starts_with('/')
            || file
                .path
                .split('/')
                .any(|segment| segment == ".." || segment.is_empty())
        {
            bail!("unsafe generated package path `{}`", file.path);
        }
    }
    Ok(())
}

fn safe_join(root: &Utf8Path, relative: &str) -> Result<Utf8PathBuf> {
    if relative.starts_with('/')
        || relative
            .split('/')
            .any(|segment| segment == ".." || segment.is_empty())
    {
        bail!("unsafe generated package path `{relative}`");
    }
    Ok(root.join(relative))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_roots_require_runtime_and_declaration_companions() {
        let files = vec![
            source("node/index.js", "export {};".to_owned()),
            declaration("node/index.d.ts", "export {};".to_owned()),
            source("browser/backend.js", "export {};".to_owned()),
            source("browser/index.js", "export {};".to_owned()),
            declaration("browser/index.d.ts", "export {};".to_owned()),
        ];
        validate_files(&files).expect("platform entrypoint companions are valid");
        for path in [
            "node/index.js",
            "node/index.d.ts",
            "browser/backend.js",
            "browser/index.js",
            "browser/index.d.ts",
        ] {
            assert!(files.iter().any(|file| file.path == path));
        }
    }

    #[test]
    fn duplicate_or_unsafe_package_paths_fail_before_publish() {
        let duplicate = vec![
            source("node/index.js", String::new()),
            source("node/index.js", String::new()),
        ];
        assert!(validate_files(&duplicate)
            .expect_err("duplicate paths must be rejected")
            .to_string()
            .contains("duplicate generated package path"));

        let unsafe_path = vec![source("../node/index.js", String::new())];
        assert!(validate_files(&unsafe_path)
            .expect_err("path traversal must be rejected")
            .to_string()
            .contains("unsafe generated package path"));
    }

    #[test]
    fn source_prefix_does_not_move_native_host_inputs() {
        let mut files = vec![
            source("components/demo/index.js", String::new()),
            source("node/index.js", String::new()),
            source("native/node.rs", String::new()),
            source("native/wasm.rs", String::new()),
            declaration("native/index.d.ts", String::new()),
        ];
        prefix_source_files(&mut files, "src/ffi");
        assert!(files
            .iter()
            .any(|file| file.path == "src/ffi/components/demo/index.js"));
        assert!(files
            .iter()
            .any(|file| file.path == "src/ffi/node/index.js"));
        for path in ["native/node.rs", "native/wasm.rs", "native/index.d.ts"] {
            assert!(
                files.iter().any(|file| file.path == path),
                "{path} must stay package-root"
            );
        }
    }

    #[test]
    fn platform_paths_are_relative_to_the_package_root() {
        assert_eq!(
            relative_module_specifier("src/ffi/node", "artifacts/node/host.node"),
            "../../../artifacts/node/host.node"
        );
        assert_eq!(
            relative_module_specifier("browser", "browser/pkg/host.js"),
            "./pkg/host.js"
        );
    }
}
