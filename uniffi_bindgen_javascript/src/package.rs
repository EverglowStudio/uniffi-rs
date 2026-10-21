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
            normalized,
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
                    normalized,
                    host_plan
                        .flavor(PublicTarget::NodeNapi)
                        .context("N-API host plan missing for Node entrypoint")?,
                    &source_prefix,
                )?;
                files.push(source("node/platform.js", node.0));
                files.push(declaration("node/platform.d.ts", node.1));
                files.push(source(
                    "node/index.js",
                    render_business_entrypoint(normalized, false),
                ));
                files.push(declaration(
                    "node/index.d.ts",
                    render_business_entrypoint(normalized, true),
                ));
            }
            if wants_electron {
                let electron = render_electron_entrypoint(
                    normalized,
                    host_plan
                        .flavor(PublicTarget::NodeNapi)
                        .context("N-API host plan missing for Electron entrypoint")?,
                    &source_prefix,
                );
                files.push(source("electron/preload.cjs", electron.0));
                files.push(source("electron/platform.js", electron.1));
                files.push(declaration("electron/platform.d.ts", electron.2));
                files.push(source(
                    "electron/index.js",
                    render_business_entrypoint(normalized, false),
                ));
                files.push(declaration(
                    "electron/index.d.ts",
                    render_business_entrypoint(normalized, true),
                ));
            }
            if selected_targets.contains(&PublicTarget::BrowserWasm) {
                let browser = render_browser_entrypoint(
                    normalized,
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
                files.push(source("browser/platform.js", browser.0));
                files.push(source("browser/index.js", browser.1));
                files.push(declaration("browser/platform.d.ts", browser.2));
                files.push(declaration(
                    "browser/index.d.ts",
                    render_business_entrypoint(normalized, true),
                ));
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
            if let Some(index) = files.iter_mut().find(|file| file.path == "platform.ets") {
                let flavor = host_plan
                    .flavor(PublicTarget::OhosNapi)
                    .context("OHOS host plan missing for Harmony entrypoint")?;
                let (native_import, suffix) =
                    render_harmony_binding(normalized, flavor, &source_prefix);
                let mut composed = native_import.into_bytes();
                composed.extend_from_slice(&index.bytes);
                index.bytes = composed;
                index.bytes.extend_from_slice(suffix.as_bytes());
            }
            if let Some(declarations) = files.iter_mut().find(|file| file.path == "platform.d.ets")
            {
                declarations
                    .bytes
                    .extend_from_slice(render_harmony_declarations(normalized).as_bytes());
            }
        }

        if selected_targets.contains(&PublicTarget::OhosNapi) {
            let names = normalized
                .api
                .components
                .iter()
                .map(|component| js_ident(&component.public_namespace))
                .collect::<Vec<_>>()
                .join(", ");
            for file in files
                .iter_mut()
                .filter(|file| file.path == "Index.ets" || file.path == "Index.d.ets")
            {
                file.bytes.extend_from_slice(
                    format!("export {{ {names} }} from \"./platform\";\n").as_bytes(),
                );
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
                bytes: engines::wasm_source(normalized, expansion_context)
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
                bytes: engines::napi_source(normalized)
                    .context("rendering the N-API engine adapter")?
                    .into_bytes(),
                role: PackageFileRole::NativeHost,
            });
        }
        if selected_targets.contains(&PublicTarget::OhosNapi) {
            let source =
                engines::ohos_source(normalized).context("rendering the OHOS engine adapter")?;
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

fn render_call_control(package: &NormalizedPackage) -> String {
    let engine = package
        .rust
        .engines
        .values()
        .next()
        .expect("normalized package has an engine");
    for candidate in package.rust.engines.values() {
        assert!(
            candidate
                .operations
                .iter()
                .map(|operation| operation.operation_id)
                .eq(engine
                    .operations
                    .iter()
                    .map(|operation| operation.operation_id)),
            "engine operation IDs must agree for a shared session protocol"
        );
    }
    let create = engine.operations.len();
    let operations = engine
        .operations
        .iter()
        .filter(|operation| {
            operation.async_kind == uniffi_js_abi::AsyncKind::Async
                && matches!(
                    operation.call_target,
                    uniffi_js_engine_schema::RustCallTarget::FreeFunction { .. }
                        | uniffi_js_engine_schema::RustCallTarget::Constructor { .. }
                        | uniffi_js_engine_schema::RustCallTarget::Method { .. }
                )
        })
        .map(|operation| operation.operation_id.index().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{{ create: {create}, cancel: {}, release: {}, operations: [{operations}] }}",
        create + 1,
        create + 2,
    )
}

fn render_session_options(package: &NormalizedPackage) -> String {
    format!("{{ callControl: {} }}", render_call_control(package))
}

fn render_stream_close_wrapper(package: &NormalizedPackage) -> String {
    let engine = package.rust.engines.values().next().expect("engine plan");
    let close_id = engine.operations.len() + 3;
    let pull_ids = engine
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation.call_target,
                uniffi_js_engine_schema::RustCallTarget::StreamHook {
                    hook: uniffi_js_engine_schema::RustResourceHook::PullOutputStream,
                    ..
                }
            )
        })
        .map(|operation| operation.operation_id.index().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let cancel_ids = engine
        .operations
        .iter()
        .filter(|operation| {
            matches!(
                operation.call_target,
                uniffi_js_engine_schema::RustCallTarget::StreamHook {
                    hook: uniffi_js_engine_schema::RustResourceHook::CancelOutputStream,
                    ..
                }
            )
        })
        .map(|operation| operation.operation_id.index().to_string())
        .collect::<Vec<_>>()
        .join(",");
    let output_ids = engine
        .operations
        .iter()
        .filter(|operation| {
            operation.return_value.as_ref().is_some_and(|binding| {
                matches!(
                    binding.conversion,
                    uniffi_js_engine_schema::ConversionRecipe::OutputStream { .. }
                )
            })
        })
        .map(|operation| operation.operation_id.index().to_string())
        .collect::<Vec<_>>()
        .join(",");
    format!(
        r#"
function __withStreamClose(backend) {{
  const cancelling = new Set([{cancel_ids}]);
  const pulling = new Set([{pull_ids}]);
  const producing = new Set([{output_ids}]);
  const live = new Map();
  const closing = new Map();
  const value = result => result?.kind === "value" ? result.value : result;
  const key = handle => typeof handle === "object" && handle !== null ? handle.handle : handle;
  const remember = (id, result) => {{
    if (producing.has(id)) {{ const handle = value(result), raw = key(handle); if (typeof raw === "number" && Number.isSafeInteger(raw) && raw > 0) live.set(raw, handle); }}
    return result;
  }};
  const close = (handle, request = false) => {{
    const raw = key(handle);
    if (closing.has(raw)) return closing.get(raw);
    // Only live subscriptions need an acknowledgement. Once acknowledged,
    // absence is the tombstone, including WASM leases already freed by next.
    if (!live.has(raw)) return Promise.resolve();
    let resolve, reject;
    const pending = new Promise((yes, no) => {{ resolve = yes; reject = no; }});
    closing.set(raw, pending);
    pending.catch(() => {{}});
    // Resource cancellation already waits for the Rust close hook and the
    // engine's active next frame. Publish its acknowledgement before reentry.
    // Failed next still uses the private ack: its lease may already be gone.
    try {{
      const cleanup = request ? backend.cancelOutputStream(live.get(raw)) : backend.invokeAsync({close_id}, [raw]);
      Promise.resolve(cleanup).then(result => {{
        try {{
          if (!request && value(result) !== 1) {{ const error = new Error("stream cleanup failed"); error.name = error.errorName = "UniffiCleanupFailed"; throw error; }}
          live.delete(raw);
          closing.delete(raw);
          resolve();
        }} catch (error) {{ reject(error); }}
      }}, reject);
    }} catch (error) {{ reject(error); }}
    return pending;
  }};
  const invokeStream = async (id, args) => {{
      if (cancelling.has(id)) {{ await close(args[0], true); return {{ kind: "value", value: undefined }}; }}
      let result;
      try {{ result = await backend.invokeAsync(id, args); }}
      catch (error) {{
        // The engine may already have released its lease on a failed next.
        // Confirm Rust cleanup once, before exposing that terminal error.
        if (pulling.has(id)) await close(args[0]);
        throw error;
      }}
      if (pulling.has(id)) {{
        const kind = value(result)?.kind;
        if (kind === "done" || kind === "error") {{
          const raw = key(args[0]);
          // A fully returned typed terminal has awaited Rust subscription
          // cleanup. A close that already owns the acknowledgement still wins,
          // including an earlier failure; absence alone never erases it.
          if (closing.has(raw)) await closing.get(raw);
          else live.delete(raw);
        }}
      }}
      return remember(id, result);
  }};
  const methods = {{
    invokeSync: (id, args) => remember(id, backend.invokeSync(id, args)),
    // Ordinary operations already carry their complete managed-call lifecycle.
    // Only the schema's stream operations need this additional acknowledgement.
    invokeAsync: (id, args) => pulling.has(id) || cancelling.has(id) || producing.has(id)
      ? invokeStream(id, args) : backend.invokeAsync(id, args),
    cancelOutputStream: handle => close(handle, true),
    releaseOutputStream: handle => {{
      const raw = key(handle);
      // Completed subscriptions need no empty asynchronous close operation.
      if (live.has(raw) || closing.has(raw)) return close(handle).then(() => backend.releaseOutputStream(handle));
      try {{ return backend.releaseOutputStream(handle); }} catch (error) {{ return Promise.reject(error); }}
    }},
    close: async (...args) => {{ await Promise.all([...live.values()].map(handle => close(handle))); return backend.close(...args); }},
  }};
  return new Proxy(Object.create(null), {{ get(_target, name) {{
    if (Object.prototype.hasOwnProperty.call(methods, name)) return methods[name];
    const member = backend[name];
    return typeof member === "function" ? member.bind(backend) : member;
  }} }});
}}
"#
    )
}

fn render_node_entrypoint(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    source_prefix: &str,
) -> Result<(String, String)> {
    let session_options = render_session_options(package);
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
        "import {{ createRequire }} from \"node:module\";\nimport {{ BackendSession, Host }} from \"{runtime}\";\nconst __require = createRequire(import.meta.url);\nconst __native = __require({addon:?});\nif (typeof __native.__uniffi_backend_factory !== \"function\") throw new Error(\"UniFFI native host does not export __uniffi_backend_factory\");\nconst __host = new Host();\nconst __backend = __withStreamClose(__native.__uniffi_backend_factory(__host));\nexport const session = new BackendSession(__backend, __host, {session_options});\nexport {{ disposeObject as releaseReference }} from {runtime:?};\n"
    ));
    source.push_str(&render_stream_close_wrapper(package));
    let mut declaration = format!(
        "import type {{ BackendSession }} from \"{runtime}\";\nexport declare const session: BackendSession;\nexport {{ disposeObject as releaseReference }} from {runtime:?};\n"
    );
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        let component_path = relative_module_specifier(
            &from_dir,
            &prefixed_path(
                source_prefix,
                &format!("components/{}/internal.js", component.public_namespace),
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
    declaration.push_str(&format!(
        "export type {{ CallOptions }} from {runtime:?};\n"
    ));
    Ok((source, declaration))
}

fn render_business_entrypoint(package: &NormalizedPackage, declaration: bool) -> String {
    let names = package
        .api
        .components
        .iter()
        .map(|component| js_ident(&component.public_namespace))
        .collect::<Vec<_>>()
        .join(", ");
    let mut source = format!("// AUTOGENERATED business API.\nexport {{ {names} }} from \"./platform.js\";\nexport {{ UniffiError }} from \"../shared/uniffi_runtime.js\";\n");
    if declaration {
        source.push_str("export type { CallOptions } from \"../shared/uniffi_runtime.js\";\n");
    }
    source
}

fn render_browser_entrypoint(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    source_prefix: &str,
) -> Result<(String, String, String)> {
    let session_options = render_session_options(package);
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
    let mut source = format!(
        "// AUTOGENERATED platform host; one lazy WASM instance.\nimport {{ BackendSession, Host, UniffiError }} from {runtime:?};\nconst __defaultWasm = new URL({wasm:?}, import.meta.url);\nexport let session;\nexport {{ disposeObject as releaseReference }} from {runtime:?};\nlet __bootPromise;\nlet __wasmExports;\nexport function initialize(input) {{ return initializeWithGlue(() => import({glue:?}), input === undefined ? __defaultWasm : input); }}\n"
    );
    source.push_str(&render_stream_close_wrapper(package));
    source.push_str(&format!(r#"
export function initializeWithGlue(customGlue, input) {{
  if (__bootPromise !== undefined) return __bootPromise;
  __bootPromise = Promise.resolve().then(async () => {{
    const glue = await (typeof customGlue === "function" ? customGlue() : customGlue);
    const initialized = typeof glue.default === "function" ? await glue.default(input) : glue;
    const exports = initialized && typeof initialized.__uniffi_backend_factory === "function" ? initialized : glue;
    if (typeof exports.__uniffi_backend_factory !== "function") throw new UniffiError({{ errorName: "UniffiBackendFactory", message: "WASM host does not export __uniffi_backend_factory" }});
    const __host = new Host();
    const __backend = __withStreamClose(exports.__uniffi_backend_factory(__host));
    session = new BackendSession(__backend, __host, {session_options});
    __wasmExports = glue;
  }});
  __bootPromise.catch(() => {{}});
  return __bootPromise;
}}
function __requireSession() {{
  if (!session) throw new UniffiError({{ errorName: "UniffiNotInitialized", message: "await an asynchronous constructor or platform.initialize() first" }});
  return session;
}}
const __sessionProvider = Object.assign(() => initialize().then(__requireSession), {{ sync: __requireSession }});
export function getWasmExports() {{ __requireSession(); return __wasmExports; }}
export async function close() {{
  if (__bootPromise === undefined) return;
  await __bootPromise;
  return session.close();
}}
"#));
    let wasm_exports_type = if package.api.resolved_config.wasm_platform_exports.is_empty() {
        "Record<string, unknown>".to_owned()
    } else {
        format!("typeof import({glue:?})")
    };
    let mut declaration = format!("import type {{ BackendSession }} from {runtime:?};\nexport declare let session: BackendSession | undefined;\nexport {{ disposeObject as releaseReference }} from {runtime:?};\nexport function initialize(input?: unknown): Promise<void>;\nexport function initializeWithGlue(glue: unknown, input?: unknown): Promise<void>;\nexport function getWasmExports(): {wasm_exports_type};\nexport type {{ CallOptions }} from {runtime:?};\nexport function close(): Promise<void>;\n");
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        let component_path = relative_module_specifier(
            &from_dir,
            &prefixed_path(
                source_prefix,
                &format!("components/{}/internal.js", component.public_namespace),
            ),
        );
        source.push_str(&format!("import * as __{namespace}Module from {component_path:?};\nexport const {namespace} = __{namespace}Module.createNamespace(__sessionProvider);\n"));
        declaration.push_str(&format!(
            "export declare const {namespace}: import({component_path:?}).Namespace;\n"
        ));
    }
    Ok((
        source,
        render_business_entrypoint(package, false),
        declaration,
    ))
}

fn render_electron_entrypoint(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    source_prefix: &str,
) -> (String, String, String) {
    let session_options = render_session_options(package);
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
// contextBridge returns frozen cross-realm carriers that N-API cannot inspect directly.\n\
function __napiCarrier(value) {\n\
  if (Object.prototype.toString.call(value) === \"[object Uint8Array]\") return Uint8Array.from(value);\n\
  if (Array.isArray(value)) return value.map(__napiCarrier);\n\
  if (value && typeof value === \"object\" && typeof value.then !== \"function\") {\n\
    const result = {};\n\
    for (const [key, item] of Object.entries(value)) result[key] = __napiCarrier(item);\n\
    return result;\n\
  }\n\
  return value;\n\
}\n\
function __hostCall(method, args) {\n\
  if (__rendererHost === null) throw __plainError({ errorName: \"UniffiElectronHost\", message: \"renderer Host is not bound\" });\n\
  try { return __napiCarrier(__rendererHost[method](...args)); } catch (error) { throw __plainError(error); }\n\
}\n\
function __hostCallAsync(method, args) {\n\
  try { return Promise.resolve(__hostCall(method, args)).then(__napiCarrier).catch((error) => Promise.reject(__plainError(error))); }\n\
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
  releaseInputStream: (...args) => __hostCall(\"releaseInputStream\", args),\n};\nconst __backend = __withStreamClose(__native.__uniffi_backend_factory(__nativeHost));\nconst __requiredBackendMethods = [\"invokeSync\", \"invokeAsync\", \"releaseObject\", \"cancelOutputStream\", \"releaseOutputStream\", \"close\"];\nfunction __backendMethod(name) {\n  if (!__backend || typeof __backend[name] !== \"function\") throw __plainError({ errorName: \"UniffiBackendProtocol\", message: \"native backend is missing \" + name });\n  return __backend[name];\n}\nfor (const method of __requiredBackendMethods) __backendMethod(method);\nfunction __backendCall(name, args) {\n  try { args = __restoreArgs(name, args); const result = __backendMethod(name).apply(__backend, args); const finish = value => __finishBackendCall(name, args, value); return result && typeof result.then === \"function\" ? result.then(finish) : finish(result); } catch (error) { throw __plainError(error); }\n}\nfunction __backendCallAsync(name, args) {\n  try { return Promise.resolve(__backendCall(name, args)).catch((error) => Promise.reject(__plainError(error))); }\n  catch (error) { return Promise.reject(__plainError(error)); }\n}\ncontextBridge.exposeInMainWorld(\"__uniffiBackend\", Object.freeze({\n  bindHost(host) { __bindRendererHost(host); },\n  invokeSync: (...args) => __backendCall(\"invokeSync\", args),\n  invokeAsync: (...args) => __backendCallAsync(\"invokeAsync\", args),\n  releaseObject: (...args) => __backendCall(\"releaseObject\", args),\n  cancelOutputStream: (...args) => __backendCallAsync(\"cancelOutputStream\", args),\n  releaseOutputStream: (...args) => __backendCall(\"releaseOutputStream\", args),\n  close: (...args) => __backendCallAsync(\"close\", args),\n}));\n",
    );
    preload.push_str(&format!(
        "const __leasePaths = {};\n",
        crate::engines::electron_lease_paths(package)
    ));
    preload.push_str(r#"
// contextBridge copies objects; the native engine tracks leases by identity.
// Paths come from the same resource plan as the generated Rust adapters.
const __leases = new Map();
const __leaseKey = value => (typeof value.surfaceId === "string" ? "object:" + value.surfaceId : "stream") + ":" + value.handle;
const __rememberLease = value => { __leases.set(__leaseKey(value), value); return value; };
const __restoreLease = value => __leases.get(__leaseKey(value)) ?? value;
function __atLease(value, path, visit) {
  if (path.length === 0) return visit(value);
  const [step, ...rest] = path;
  switch (step[0]) {
    case "field": {
      const copy = Array.isArray(value) ? value.slice() : { ...value };
      copy[step[1]] = __atLease(value[step[1]], rest, visit);
      return copy;
    }
    case "optional": return value == null ? value : __atLease(value, rest, visit);
    case "variant": return value.tag === step[1] ? __atLease(value, rest, visit) : value;
    case "step": return value.kind === step[1] ? { ...value, [step[2]]: __atLease(value[step[2]], rest, visit) } : value;
    case "sequence": return value.map(item => __atLease(item, rest, visit));
    case "set": return new Set([...value].map(item => __atLease(item, rest, visit)));
    case "mapKey": return new Map([...value].map(([key, item]) => [__atLease(key, rest, visit), item]));
    case "mapValue": return new Map([...value].map(([key, item]) => [key, __atLease(item, rest, visit)]));
    default: throw new Error("invalid generated lease path");
  }
}
function __restoreArgs(name, args) {
  if (name === "invokeSync" || name === "invokeAsync") {
    let values = args[1];
    for (const path of __leasePaths[args[0]]?.inputs ?? []) values = __atLease(values, path, __restoreLease);
    return [args[0], values];
  }
  if (["releaseObject", "cancelOutputStream", "releaseOutputStream"].includes(name)) return [__restoreLease(args[0])];
  return args;
}
function __finishBackendCall(name, args, result) {
  if (name === "releaseObject" || name === "releaseOutputStream") __leases.delete(__leaseKey(args[0]));
  if (name === "close") __leases.clear();
  if ((name === "invokeSync" || name === "invokeAsync") && result?.kind === "value") {
    for (const path of __leasePaths[args[0]]?.outputs ?? []) __atLease(result.value, path, __rememberLease);
  }
  return result;
}
"#);
    preload.push_str(&render_stream_close_wrapper(package));
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
        "// AUTOGENERATED Electron renderer entry; the shared facade owns conversion and lifecycle.\nimport {{ BackendSession, Host }} from {runtime:?};\nconst __bridge = window.__uniffiBackend;\nconst __host = new Host();\n__bridge.bindHost({{\n  retainCallback: (...args) => __host.retainCallback(...args),\n  releaseCallback: (...args) => __host.releaseCallback(...args),\n  invokeCallbackSync: (...args) => __host.invokeCallbackSync(...args),\n  invokeCallbackAsync: (...args) => __host.invokeCallbackAsync(...args),\n  invokeCallbackSyncResult: (...args) => __host.invokeCallbackSyncResult(...args),\n  invokeCallbackAsyncResult: (...args) => __host.invokeCallbackAsyncResult(...args),\n  pullInputStream: (...args) => __host.pullInputStream(...args),\n  cancelInputStream: (...args) => __host.cancelInputStream(...args),\n  releaseInputStream: (...args) => __host.releaseInputStream(...args),\n}});\nconst __backend = {{ invokeSync: (...args) => __bridge.invokeSync(...args), invokeAsync: (...args) => __bridge.invokeAsync(...args), releaseObject: (...args) => __bridge.releaseObject(...args), cancelOutputStream: (...args) => __bridge.cancelOutputStream(...args), releaseOutputStream: (...args) => __bridge.releaseOutputStream(...args), close: (...args) => __bridge.close(...args) }};\nexport const session = new BackendSession(__backend, __host, {session_options});\nexport {{ disposeObject as releaseReference }} from {runtime:?};\n"
    );
    renderer = format!(
        "const __uniffiBridgeCheck = globalThis.window && globalThis.window.__uniffiBackend;\nif (!__uniffiBridgeCheck || typeof __uniffiBridgeCheck !== \"object\") throw new Error(\"UniFFI Electron preload bridge is unavailable\");\nfor (const __method of [\"bindHost\", \"invokeSync\", \"invokeAsync\", \"releaseObject\", \"cancelOutputStream\", \"releaseOutputStream\", \"close\"]) if (typeof __uniffiBridgeCheck[__method] !== \"function\") throw new Error(\"UniFFI Electron preload bridge is missing \" + __method);\n{}",
        renderer
    );
    let mut declaration = format!(
        "import type {{ BackendSession, Host }} from {runtime:?};\nexport interface ElectronBackendBridge {{ bindHost(host: unknown): void; invokeSync(operationId: number, args: unknown[]): unknown; invokeAsync(operationId: number, args: unknown[]): Promise<unknown>; releaseObject(handle: unknown): void; cancelOutputStream(handle: unknown): Promise<void>; releaseOutputStream(handle: unknown): void; close(): Promise<void>; }}\ndeclare global {{ interface Window {{ __uniffiBackend: ElectronBackendBridge; }} }}\nexport declare const session: BackendSession;\nexport {{ disposeObject as releaseReference }} from {runtime:?};\n"
    );
    for component in &package.api.components {
        let namespace = js_ident(&component.public_namespace);
        let component_path = relative_module_specifier(
            &from_dir,
            &prefixed_path(
                source_prefix,
                &format!("components/{}/internal.js", component.public_namespace),
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
    declaration.push_str(&format!(
        "export type {{ CallOptions }} from {runtime:?};\n"
    ));
    (preload, renderer, declaration)
}

fn render_harmony_binding(
    package: &NormalizedPackage,
    flavor: &crate::host_crates::HostFlavorPlan,
    _source_prefix: &str,
) -> (String, String) {
    let call_control = render_call_control(package);
    let native_module = format!("lib{}.so", flavor.lib_target);
    let native_import = format!("import * as __uniffiNative from {native_module:?};\n");
    let mut source = format!(
        "\n// AUTOGENERATED Harmony platform composition.\nconst __uniffiHost = new Host();\nconst __uniffiBackend: ArkBackend = __uniffiNative.__uniffi_backend_factory(__uniffiHost) as ArkBackend;\nconst __callControl: CallControl = {call_control};\nconst __sessionOptions: BackendSessionOptions = {{ callControl: __callControl }};\nexport const session = new BackendSession(__uniffiBackend, __uniffiHost, __sessionOptions);\nconst __uniffiApi = createNamespace(session);\n",
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
    fn browser_business_entry_initializes_lazily_and_keeps_platform_private() {
        use crate::frontend::{normalize, BindingInput};
        use uniffi_bindgen::{interface::ComponentInterface, Component};
        let config = crate::JsConfig::from_root_toml(
            toml::from_str(
                r#"
[bindings.lifecycle]
close_methods = { Client = "close", View = "close" }
owned_methods = ["Client.service", "Client.conversation", "Client.view"]
cached_methods = ["Client.service"]
[bindings.javascript]
wasmPlatformExports = ["PlatformStorage"]
"#,
            )
            .unwrap(),
        )
        .unwrap();
        let mut ci = ComponentInterface::from_webidl("namespace lifecycle { string version(); }; interface Service { string name(); [Async] string work(); }; interface View { string snapshot(); [Async] void close(); }; interface Client { [Async] constructor(); Service service(); Service conversation(string id); View view(); [Async] void close(); };", "lifecycle").unwrap();
        let mut streams = uniffi_meta::MetadataGroup {
            namespace: uniffi_meta::NamespaceMetadata {
                crate_name: "lifecycle".into(),
                name: "lifecycle".into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        for (name, is_async) in [("events", false), ("late_events", true)] {
            streams.add_item(
                uniffi_meta::MethodMetadata {
                    module_path: "lifecycle".into(),
                    self_name: "Client".into(),
                    name: name.into(),
                    orig_name: None,
                    is_async,
                    rust_async: is_async,
                    rust_return_is_result: false,
                    rust_stream_item_is_result: false,
                    inputs: vec![],
                    return_type: Some(uniffi_meta::Type::Stream {
                        item_type: Box::new(uniffi_meta::Type::UInt32),
                        error_type: None,
                        is_send: false,
                    }),
                    throws: None,
                    takes_self_by_arc: false,
                    checksum: None,
                    docstring: None,
                }
                .into(),
            );
        }
        ci.add_metadata(streams).unwrap();
        let components = [Component { ci, config }];
        let package = normalize(BindingInput::new(&components)).unwrap();
        assert_eq!(package.api.resolved_config.lifecycle.close_methods.len(), 2);
        assert_eq!(
            package
                .api
                .resolved_config
                .wasm_platform_exports
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            ["PlatformStorage"]
        );
        let facade = uniffi_js_facade::build(&package.api, &package.bridge, &package.rust).unwrap();
        let root = std::env::temp_dir().join(format!(
            "uniffi-browser-lifecycle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("browser/pkg")).unwrap();
        std::fs::write(root.join("package.json"), "{\"type\":\"module\"}").unwrap();
        for file in facade.shared_files() {
            let path = root.join(&file.path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, &file.bytes).unwrap();
        }
        let flavor = crate::host_crates::HostFlavorPlan {
            crate_version: "0.0.0".into(),
            lib_target: "lifecycle".into(),
            crate_root: "native/host".into(),
            native_artifact: "browser/pkg/host_bg.wasm".into(),
            loader_artifact: Some("browser/pkg/host.js".into()),
        };
        let (platform, business, declarations) =
            render_browser_entrypoint(&package, &flavor, "").unwrap();
        std::fs::write(root.join("browser/platform.js"), platform).unwrap();
        std::fs::write(root.join("browser/platform.d.ts"), declarations).unwrap();
        std::fs::write(root.join("browser/index.js"), business).unwrap();
        std::fs::write(
            root.join("browser/index.d.ts"),
            render_business_entrypoint(&package, true),
        )
        .unwrap();
        let mut ids = package
            .api
            .operations
            .iter()
            .map(|operation| {
                let owner = match operation.source_key.owner() {
                    uniffi_js_abi::OperationOwner::Object(owner) => owner.name(),
                    _ => "",
                };
                (
                    format!("{owner}.{}", operation.source_key.name()),
                    operation.id.index(),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        for (alias, source) in [
            ("service", "Client.service"),
            ("name", "Service.name"),
            ("version", ".version"),
            ("close", "Client.close"),
        ] {
            ids.insert(alias.into(), ids[source]);
        }
        std::fs::write(
            root.join("browser/pkg/host.d.ts"),
            "export declare class PlatformStorage { readonly marker: string; }\n",
        )
        .unwrap();
        let constructor = package
            .api
            .operations
            .iter()
            .find(|op| op.kind == uniffi_js_abi::OperationKind::Constructor)
            .unwrap();
        // Stream hooks extend the engine operation table beyond bridge calls.
        // Both fixtures must use the same control IDs as the generated platform.
        let control_start = package
            .rust
            .engines
            .values()
            .next()
            .unwrap()
            .operations
            .len();
        std::fs::write(root.join("browser/pkg/host.js"), format!(r#"
export let boots = 0, closes = 0, resources = 0, releases = 0;
export let streamCancels = 0, streamReleases = 0;
export const controls = [], calls = [], objectReleases = [];
export const cleanupFailure = Object.assign(Error("native stream cleanup failed"), {{errorName:"CleanupProbe"}});
let resolveLateFactory;
export function finishLateFactory() {{ resolveLateFactory(9001); }}
const callControl = {call_control};
export class PlatformStorage {{}}
export default async function initialize() {{ boots++; if (globalThis.failBoot) throw Error("boot failed"); }}
export function __uniffi_backend_factory() {{ return {{
  invokeSync(id, args) {{
    if(id===callControl.create) {{ controls.push(["create"]); return 1; }}
    if(id===callControl.cancel || id===callControl.release) {{ controls.push([id===callControl.cancel ? "cancel" : "release", ...args]); return null; }}
    if(id==={service}) return 20; if(id==={name}) return "service"; if(id==={version}) return "1"; throw Error("unexpected sync "+id);
  }},
  async invokeAsync(id, args) {{
    if(callControl.operations.includes(id)) calls.push(args.at(-1));
    if(id==={construct}) return 10;
    if(id==={late_events}) return new Promise(resolve => {{ resolveLateFactory = resolve; }});
    if(id==={close}) {{ resources++; return null; }} throw Error("unexpected async "+id);
  }},
  cancelOutputStream(handle) {{ if(handle!==9001) throw Error("unexpected stream "+handle); streamCancels++; return Promise.reject(cleanupFailure); }},
  releaseOutputStream() {{ streamReleases++; }},
  releaseObject(handle) {{ objectReleases.push(handle); releases++; }}, close() {{ closes++; }}
}}; }}
"#, call_control=render_call_control(&package), service=ids["service"],name=ids["name"],version=ids["version"], construct=constructor.id.index(),close=ids["close"],late_events=ids["Client.late_events"])).unwrap();
        std::fs::write(root.join("check.mjs"), format!(r#"
import assert from 'node:assert/strict';
import * as business from './browser/index.js';
import * as platform from './browser/platform.js';
import * as component from './components/lifecycle/index.js';
import * as glue from './browser/pkg/host.js';
assert.equal(glue.boots,0);
for (const name of ['session','close','initialize','ready','BackendSession','ObjectLease','createNamespace']) {{ assert.equal(name in business,false,name); assert.equal(name in component,false,name); }}
assert.throws(()=>business.lifecycle.version(),{{errorName:'UniffiNotInitialized'}});
assert.throws(()=>platform.getWasmExports(),{{errorName:'UniffiNotInitialized'}});
if (process.argv[2]==='fail') {{
  globalThis.failBoot=true;
  await assert.rejects(business.lifecycle.Client.{constructor}(), /boot failed/);
  await assert.rejects(business.lifecycle.Client.{constructor}(), /boot failed/);
  assert.equal(glue.boots,1);
}} else if (process.argv[2]==='late-stream-failure') {{
  const client=await business.lifecycle.Client.{constructor}();
  const state=[...platform.session.objects][0];
  const late=client.lateEvents();
  const rejected=assert.rejects(late,error=>error===glue.cleanupFailure);
  const closing=client.close();
  assert.equal(client.close(),closing);
  await new Promise(resolve=>setImmediate(resolve));
  assert.equal(state.active,1,'close must wait for the unresolved factory frame');
  assert.equal(glue.releases,0);
  glue.finishLateFactory();
  let timeout;
  try {{
    await Promise.race([
      Promise.all([rejected,closing]),
      new Promise((_,reject)=>{{ timeout=setTimeout(()=>reject(Error('owner.close did not settle after stream cleanup failed')),2000); }}),
    ]);
  }} finally {{ clearTimeout(timeout); }}
  assert.equal(state.active,0);
  assert.equal(platform.session.objects.size,0);
  assert.deepEqual(glue.objectReleases,[10],'owner handle releases exactly once');
  assert.equal(glue.streamCancels,1);
  assert.equal(glue.streamReleases,0,'failed cleanup must not pretend to release the stream');
  assert.equal(client.close(),closing); await client.close();
  assert.equal(glue.releases,1);
  assert.deepEqual(glue.controls,[["create"]]);
  assert.deepEqual(glue.calls,[1,2,3]);
  await assert.rejects(platform.close(),error=>error instanceof business.UniffiError && error.errorName==='CleanupProbe' && error.data===glue.cleanupFailure);
  assert.equal(glue.closes,0);
}} else {{
  const client=await business.lifecycle.Client.{constructor}();
  assert.equal(glue.boots,1); assert.equal(platform.getWasmExports().PlatformStorage,glue.PlatformStorage);
  assert.equal(business.lifecycle.version(),'1');
  const service=client.service(); for(let i=0;i<100;i++) assert.equal(client.service(),service);
  assert.equal(service.name(),'service');
  assert.equal('dispose' in client,false); assert.equal(Symbol.dispose in client,false);
  const closing=client.close(); assert.equal(closing,client.close()); assert.equal(closing,client[Symbol.asyncDispose]());
  await closing; assert.equal(glue.resources,1); assert.equal(glue.releases,2); assert.equal(glue.closes,0);
  assert.deepEqual(glue.controls,[["create"]]); assert.deepEqual(glue.calls,[1,2]);
  assert.throws(()=>client.service(),{{errorName:'UniffiObjectClosed'}});
  await platform.close(); assert.equal(glue.closes,1);
}}
"#,constructor=constructor.public_name)).unwrap();
        for mode in ["ok", "fail", "late-stream-failure"] {
            let output = std::process::Command::new("node")
                .args(["check.mjs", mode])
                .current_dir(&root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}: {}",
                root.display(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::write(
            root.join("types.mts"),
            format!(
                r#"
import {{ lifecycle }} from './browser/index.js';
import {{ initialize, getWasmExports, releaseReference }} from './browser/platform.js';
// @ts-expect-error host state is not a business export
import {{ session }} from './browser/index.js';
// @ts-expect-error factories are internal
import {{ createNamespace }} from './components/lifecycle/index.js';
const client = await lifecycle.Client.{constructor}();
await client[Symbol.asyncDispose]();
// @ts-expect-error resources expose close, not dispose
client.dispose();
await initialize(); const storage = new (getWasmExports().PlatformStorage)(); storage.marker;
"#,
                constructor = constructor.public_name
            ),
        )
        .unwrap();
        let compiler =
            std::env::var("UNIFFI_TEST_TYPESCRIPT_COMPILER").unwrap_or_else(|_| "tsc".into());
        let output = std::process::Command::new(compiler)
            .args([
                "--strict",
                "--noEmit",
                "--target",
                "ES2022",
                "--lib",
                "ES2022,DOM,ESNext.Disposable",
                "--module",
                "NodeNext",
                "--moduleResolution",
                "NodeNext",
                "types.mts",
            ])
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}{}",
            root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // Exercise the strict printer's actual output with the installed TS
        // compiler and Node. This is portable verification, not Harmony CI.
        let ark_root = root.join("ark");
        std::fs::create_dir_all(&ark_root).unwrap();
        std::fs::write(ark_root.join("package.json"), "{\"type\":\"commonjs\"}").unwrap();
        for file in facade.ark_files() {
            std::fs::write(ark_root.join(file.path.replace(".ets", ".ts")), &file.bytes).unwrap();
        }
        let compiler =
            std::env::var("UNIFFI_TEST_TYPESCRIPT_COMPILER").unwrap_or_else(|_| "tsc".into());
        let output = std::process::Command::new(compiler)
            .args([
                "--strict",
                "--useUnknownInCatchVariables",
                "false",
                "--target",
                "ES2022",
                "--module",
                "commonjs",
                "--outDir",
                "out",
                "platform.ts",
                "Index.ts",
            ])
            .current_dir(&ark_root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}{}",
            ark_root.display(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::write(
            ark_root.join("check.cjs"),
            include_str!("../tests/ark-lifecycle.cjs"),
        )
        .unwrap();
        let managed = package
            .api
            .operations
            .iter()
            .filter(|op| op.async_kind == uniffi_js_abi::AsyncKind::Async)
            .map(|op| op.id.index())
            .collect::<Vec<_>>();
        let streams = facade.ast.operations.iter().filter(|op| matches!(&op.return_type, Some(uniffi_js_abi::ValueType::OutputStream { .. }))).map(|op| {
            let slots = &op.stream_resources[0].slot_operation_ids;
            (op.name.clone(), serde_json::json!({"start":op.id.index(), "next":slots[&uniffi_js_abi::OperationKind::OutputStreamNext].index(), "cancel":slots[&uniffi_js_abi::OperationKind::OutputStreamCancel].index()}))
        }).collect::<std::collections::BTreeMap<_,_>>();
        std::fs::write(ark_root.join("fixture.json"), serde_json::json!({
            "operations": ids,
            "constructor": constructor.public_name,
            "construct": constructor.id.index(),
                "streams": streams,
            "callControl": { "create": control_start, "cancel": control_start+1, "release": control_start+2, "operations": managed }
        }).to_string()).unwrap();
        let output = std::process::Command::new("node")
            .args(["--expose-gc", "check.cjs"])
            .current_dir(&ark_root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}: {}",
            ark_root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn every_platform_session_uses_canonical_call_control_ids() {
        use crate::frontend::{normalize, BindingInput};
        use uniffi_bindgen::{interface::ComponentInterface, Component};
        let components = [Component {
            ci: ComponentInterface::from_webidl(
                "namespace control_test { string echo(string value); };",
                "control_test",
            )
            .unwrap(),
            config: crate::JsConfig::default(),
        }];
        let package = normalize(BindingInput::new(&components)).unwrap();
        let count = package.bridge.operations().len();
        assert_eq!(count, 1);
        let flavor = crate::host_crates::HostFlavorPlan {
            crate_version: "0.0.0".into(),
            lib_target: "control_test".into(),
            crate_root: "native/host".into(),
            native_artifact: "native/host.node".into(),
            loader_artifact: Some("browser/pkg/host.js".into()),
        };
        let options = "{ callControl: { create: 1, cancel: 2, release: 3, operations: [] } }";
        let (node, node_types) = render_node_entrypoint(&package, &flavor, "").unwrap();
        let (browser, _, browser_types) = render_browser_entrypoint(&package, &flavor, "").unwrap();
        let (_, electron, electron_types) = render_electron_entrypoint(&package, &flavor, "");
        let (_, harmony) = render_harmony_binding(&package, &flavor, "");
        for source in [&node, &browser, &electron] {
            assert!(source.contains(&format!("new BackendSession(__backend, __host, {options})")));
        }
        assert!(harmony.contains(&format!(
            "const __callControl: CallControl = {}",
            render_call_control(&package)
        )));
        assert!(
            harmony.contains("new BackendSession(__uniffiBackend, __uniffiHost, __sessionOptions)")
        );
        for declaration in [node_types, browser_types, electron_types] {
            assert!(declaration.contains("export type { CallOptions }"));
        }
    }

    fn stream_control_package() -> NormalizedPackage {
        use crate::frontend::{normalize, BindingInput};
        use uniffi_bindgen::{interface::ComponentInterface, Component};
        use uniffi_meta::{FnMetadata, MetadataGroup, NamespaceMetadata, Type};
        let mut group = MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: "control_test".into(),
                name: "control_test".into(),
            },
            namespace_docstring: None,
            items: Default::default(),
        };
        group.add_item(
            FnMetadata {
                module_path: "control_test".into(),
                name: "produce".into(),
                orig_name: None,
                is_async: true,
                rust_async: true,
                rust_return_is_result: false,
                rust_stream_item_is_result: true,
                inputs: vec![],
                return_type: Some(Type::Stream {
                    item_type: Box::new(Type::String),
                    error_type: Some(Box::new(Type::String)),
                    is_send: false,
                }),
                throws: None,
                checksum: None,
                docstring: None,
            }
            .into(),
        );
        let components = [Component {
            ci: ComponentInterface::from_metadata(group).unwrap(),
            config: crate::JsConfig::default(),
        }];
        normalize(BindingInput::new(&components)).unwrap()
    }

    #[test]
    fn managed_operations_include_async_factories_but_exclude_stream_hooks() {
        let package = stream_control_package();
        let factory = package
            .api
            .operations
            .iter()
            .find(|operation| operation.public_name == "produce")
            .unwrap();
        assert_eq!(package.bridge.operations().len(), 1);
        assert_eq!(
            package
                .rust
                .engines
                .values()
                .next()
                .unwrap()
                .operations
                .len(),
            3
        );
        assert_eq!(
            render_call_control(&package),
            format!(
                "{{ create: 3, cancel: 4, release: 5, operations: [{}] }}",
                factory.id.index()
            )
        );
    }

    #[test]
    fn electron_callback_bytes_are_copied_into_preload_typed_arrays() {
        let package = stream_control_package();
        let flavor = crate::host_crates::HostFlavorPlan {
            crate_version: "0.0.0".into(),
            lib_target: "control_test".into(),
            crate_root: "native/host".into(),
            native_artifact: "native/host.node".into(),
            loader_artifact: None,
        };
        let (preload, _, _) = render_electron_entrypoint(&package, &flavor, "");
        let start = preload.find("function __napiCarrier(").unwrap();
        let end = preload.find("function __hostCall(").unwrap();
        let mut script = format!(
            "const source = {};\n",
            serde_json::to_string(&preload[start..end]).unwrap()
        );
        script.push_str(
            r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const convert = vm.runInThisContext(source + '; __napiCarrier');
const foreign = vm.runInNewContext(`(() => {
  const bytes = new Uint8Array([171, 3, 5, 171]).subarray(1, 3);
  return Object.freeze({ value: Object.freeze({ value: bytes }), items: [bytes, null] });
})()`);
assert.equal(foreign.value.value instanceof Uint8Array, false);
const owned = convert(foreign);
assert.ok(owned.value.value instanceof Uint8Array);
assert.deepEqual([...owned.value.value], [3, 5]);
assert.ok(owned.items[0] instanceof Uint8Array);
assert.equal(owned.items[1], null);
foreign.value.value[0] = 9;
assert.equal(owned.value.value[0], 3);
assert.equal(owned.items[0][0], 3);
"#,
        );
        let output = std::process::Command::new("node")
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn stream_close_ack_is_bounded_and_never_recancels_a_released_lease() {
        let package = stream_control_package();
        let operations = &package.rust.engines.values().next().unwrap().operations;
        let id = |kind| {
            operations
                .iter()
                .find(|op| op.kind == kind)
                .unwrap()
                .operation_id
                .index()
        };
        let ids = [
            id(uniffi_js_abi::OperationKind::OutputStreamStart),
            id(uniffi_js_abi::OperationKind::OutputStreamNext),
            id(uniffi_js_abi::OperationKind::OutputStreamCancel),
            operations.len() as u32 + 3,
        ];
        let mut script = format!(
            "const [factory, pull, cancel, ack] = {}; const source = {};\n",
            serde_json::to_string(&ids).unwrap(),
            serde_json::to_string(&render_stream_close_wrapper(&package)).unwrap()
        );
        script.push_str(r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const maps = [];
class ObservedMap extends Map { constructor() { super(); maps.push(this); } }
let nextHandle = 1, acknowledgements = 0, cancellationRequests = 0, releaseAck;
const ackStarted = () => new Promise(resolve => { releaseAck = resolve; });
let gate = null;
const backend = {
  invokeSync(id) { assert.equal(id, factory); return {kind:'value', value:{handle:nextHandle++}}; },
  async invokeAsync(id, args) {
    if (id === ack) { acknowledgements++; if (gate) await gate; return {kind:'value', value:1}; }
    if (id === pull) throw Object.assign(Error('released WASM lease'), {errorName:'UniffiCancelled'});
    if (id === cancel) { cancellationRequests++; return {kind:'value'}; }
    throw Error('unexpected operation');
  },
  cancelOutputStream() { cancellationRequests++; throw Error('cannot re-cancel a released WASM lease'); },
  releaseOutputStream() {}, close() {},
};
const wrapped = vm.runInNewContext(source + '; __withStreamClose(backend)', {backend, Map:ObservedMap, Promise});
const ordinary = Promise.resolve({kind:'value',value:9});
const direct = vm.runInNewContext(source + '; __withStreamClose(backend)', {backend:{invokeAsync:()=>ordinary}, Promise});
assert.equal(direct.invokeAsync(10000, []), ordinary, 'non-stream operation keeps the existing managed-call promise');
assert.equal(direct.invokeAsync, direct.invokeAsync, 'operation wrappers are stable across lookups');
(async () => {
  for (let i = 0; i < 1000; i++) {
    const handle = wrapped.invokeSync(factory, []).value;
    await assert.rejects(wrapped.invokeAsync(pull,[handle]), {errorName:'UniffiCancelled'});
    await wrapped.invokeAsync(cancel,[handle]); await wrapped.cancelOutputStream(handle);
    await wrapped.releaseOutputStream(handle);
    assert.deepEqual(maps.map(map => map.size), [0,0], 'no historical resolved close promises');
  }
  assert.equal(acknowledgements,1000); assert.equal(cancellationRequests,0);
  gate = ackStarted(); const handle = wrapped.invokeSync(factory,[]).value;
  let readSettled=false, closeSettled=false;
  const reading = assert.rejects(wrapped.invokeAsync(pull,[handle]), {errorName:'UniffiCancelled'}).then(()=>{readSettled=true;});
  // Wait for the acknowledgement to be entered, not an elapsed-time guess.
  while (acknowledgements !== 1001) await Promise.resolve();
  const closing = wrapped.invokeAsync(cancel,[handle]).then(()=>{closeSettled=true;});
  await Promise.resolve(); assert.equal(readSettled || closeSettled, false);
  releaseAck(); await Promise.all([reading,closing]);
  assert.equal(acknowledgements,1001); assert.equal(cancellationRequests,0);
  await wrapped.releaseOutputStream(handle); await wrapped.close();
  assert.deepEqual(maps.map(map => map.size), [0,0]);
})().catch(error=>{console.error(error);process.exitCode=1;});
"#);
        let output = std::process::Command::new("node")
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn stream_typed_terminal_reuses_ack_without_overriding_an_existing_close() {
        let package = stream_control_package();
        let operations = &package.rust.engines.values().next().unwrap().operations;
        let id = |kind| {
            operations
                .iter()
                .find(|op| op.kind == kind)
                .unwrap()
                .operation_id
                .index()
        };
        let ids = [
            id(uniffi_js_abi::OperationKind::OutputStreamStart),
            id(uniffi_js_abi::OperationKind::OutputStreamNext),
            operations.len() as u32 + 3,
        ];
        let mut script = format!(
            "const [factory, pull, ack] = {}; const source = {};\n",
            serde_json::to_string(&ids).unwrap(),
            serde_json::to_string(&render_stream_close_wrapper(&package)).unwrap()
        );
        script.push_str(r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => {resolve=yes; reject=no;});
  return {promise,resolve,reject};
};
const tick = () => new Promise(resolve=>setImmediate(resolve));
(async () => {
  for (const kind of ['done','error']) {
    const maps = [];
    class ObservedMap extends Map { constructor() {super();maps.push(this);} }
    let handleId=1, acknowledgements=0, cancellations=0, releases=0;
    const step=kind === 'done' ? {kind} : {kind,error:{variant:'Boom'}};
    const backend = {
      invokeSync:()=>({kind:'value',value:{handle:handleId++}}),
      invokeAsync(id) {
        if(id === pull) return Promise.resolve({kind:'value',value:step});
        assert.equal(id,ack); acknowledgements++; return Promise.resolve({kind:'value',value:1});
      },
      cancelOutputStream() {cancellations++;},
      releaseOutputStream() {releases++;},close() {},
    };
    const wrapped=vm.runInNewContext(source+'; __withStreamClose(backend)',{backend,Map:ObservedMap,Promise});
    for(let i=0;i<32;i++) {
      const handle=wrapped.invokeSync(factory,[]).value;
      assert.equal((await wrapped.invokeAsync(pull,[handle])).value,step);
      await wrapped.cancelOutputStream(handle);
      const releasing = wrapped.releaseOutputStream(handle);
      assert.equal(releases,i+1,'acknowledged terminal release enters without an empty close hop');
      await releasing;
      assert.deepEqual(maps.map(map=>map.size),[0,0]);
    }
    await wrapped.close();
    assert.deepEqual([acknowledgements,cancellations,releases],[0,0,32],
      'a fully returned typed terminal already acknowledged Rust cleanup');

    for(const failure of [null,Error('retained cleanup failure')]) {
      for(const alreadySettled of [false,true]) {
        const raw=deferred(), cleanup=deferred();
        let attempts=0, extraAcks=0, returned=false;
        const ownerBackend={
          invokeSync:()=>({kind:'value',value:{handle:71}}),
          invokeAsync(id) {
            if(id === pull) return raw.promise;
            extraAcks++; return Promise.resolve({kind:'value',value:1});
          },
          cancelOutputStream() {attempts++; return cleanup.promise;},
          releaseOutputStream() {},close() {},
        };
        const owner=vm.runInNewContext(source+'; __withStreamClose(backend)',{backend:ownerBackend,Promise});
        const handle=owner.invokeSync(factory,[]).value;
        const reading=owner.invokeAsync(pull,[handle]).then(result=>{returned=true;return result;});
        reading.catch(()=>{});
        const closing=owner.cancelOutputStream(handle);
        const settle=()=>failure ? cleanup.reject(failure) : cleanup.resolve();
        if(alreadySettled) {settle();await tick();}
        raw.resolve({kind:'value',value:step});
        if(!alreadySettled) {
          await tick(); assert.equal(returned,false,'typed terminal waits for the existing close owner');
          settle();
        }
        if(failure) {
          for(const promise of [reading,closing,owner.cancelOutputStream(handle),owner.close()]) {
            await assert.rejects(promise,error=>error === failure);
          }
        } else {
          assert.equal((await reading).value,step); await closing; await owner.close();
        }
        assert.deepEqual([attempts,extraAcks],[1,0]);
      }
    }
  }
})().catch(error=>{console.error(error);process.exitCode=1;});
"#);
        let output = std::process::Command::new("node")
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn stream_resource_cancel_shares_its_cleanup_ack_before_backend_reentry() {
        let package = stream_control_package();
        let operations = &package.rust.engines.values().next().unwrap().operations;
        let id = |kind| {
            operations
                .iter()
                .find(|op| op.kind == kind)
                .unwrap()
                .operation_id
                .index()
        };
        let ids = [
            id(uniffi_js_abi::OperationKind::OutputStreamStart),
            id(uniffi_js_abi::OperationKind::OutputStreamNext),
            id(uniffi_js_abi::OperationKind::OutputStreamCancel),
            operations.len() as u32 + 3,
        ];
        let mut script = format!(
            "const [factory, pull, cancel, ack] = {}; const source = {}; const runtimeSource = {};\n",
            serde_json::to_string(&ids).unwrap(),
            serde_json::to_string(&render_stream_close_wrapper(&package)).unwrap(),
            serde_json::to_string(uniffi_js_facade::RUNTIME_SOURCE).unwrap()
        );
        script.push_str(include_str!("../tests/stream-owner-close.cjs"));
        script.push_str(r#"
const assert = require('node:assert/strict');
const vm = require('node:vm');
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return {promise, resolve, reject};
};
const tick = () => new Promise(resolve => setImmediate(resolve));
(async () => {
  const maps = [];
  class ObservedMap extends Map { constructor() { super(); maps.push(this); } }
  const cleanup = deferred(), rawFrame = deferred(), read = deferred();
  const cancelled = Object.assign(Error('cancelled'), {errorName:'UniffiCancelled'});
  const carrier = {handle:1};
  let resourceCancels = 0, extraAcks = 0, requestOnlyCancels = 0, releases = 0, closes = 0;
  let wrapped, reentrant;
  const backend = {
    invokeSync(id) { assert.equal(id,factory); return {kind:'value',value:carrier}; },
    invokeAsync(id) {
      if (id === pull) return read.promise;
      if (id === ack) { extraAcks++; return Promise.resolve({kind:'value',value:1}); }
      if (id === cancel) { requestOnlyCancels++; return Promise.resolve({kind:'value'}); }
      throw Error('unexpected operation');
    },
    cancelOutputStream(handle) {
      assert.equal(handle,carrier,'resource hooks receive the original backend lease carrier');
      resourceCancels++;
      if (resourceCancels === 1) reentrant = wrapped.cancelOutputStream(carrier);
      return Promise.all([cleanup.promise,rawFrame.promise]).then(() => { read.reject(cancelled); });
    },
    releaseOutputStream(handle) { assert.equal(handle,carrier); releases++; },
    close() { closes++; },
  };
  wrapped = vm.runInNewContext(source + '; __withStreamClose(backend)', {backend, Map:ObservedMap, Promise});
  const handle = wrapped.invokeSync(factory,[]).value;
  let readDone=false, cancelDone=false, releaseDone=false, closeDone=false;
  const reading = assert.rejects(wrapped.invokeAsync(pull,[handle]), {errorName:'UniffiCancelled'}).then(()=>{readDone=true;});
  const cancelling = wrapped.invokeAsync(cancel,[handle]).then(()=>{cancelDone=true;});
  const repeated = wrapped.cancelOutputStream(handle);
  const releasing = wrapped.releaseOutputStream(handle).then(()=>{releaseDone=true;});
  const closing = wrapped.close().then(()=>{closeDone=true;});
  await tick();
  assert.equal(resourceCancels,1,'publish the shared close before synchronous backend reentry');
  assert.equal(repeated,reentrant,'all resource cancel callers share the same acknowledgement');
  assert.deepEqual([readDone,cancelDone,releaseDone,closeDone],[false,false,false,false]);
  cleanup.resolve(); await tick();
  assert.deepEqual([readDone,cancelDone,releaseDone,closeDone],[false,false,false,false],'cleanup alone is insufficient while the raw next frame is active');
  rawFrame.resolve();
  await Promise.all([reading,cancelling,repeated,reentrant,releasing,closing]);
  assert.deepEqual([resourceCancels,extraAcks,requestOnlyCancels,releases,closes],[1,0,0,1,1]);
  assert.deepEqual(maps.map(map=>map.size),[0,0]);
  await wrapped.invokeAsync(cancel,[handle]); await wrapped.cancelOutputStream(handle);
  assert.equal(resourceCancels,1);

  // Failed acknowledgements remain shared and observable; release/session close
  // must not claim success or retry a hook whose cleanup result already failed.
  for (const synchronous of [true,false]) {
    const failure = Error(synchronous ? 'sync cleanup failure' : 'async cleanup failure');
    let attempts=0, released=0, closed=0;
    const failedBackend = {
      invokeSync:()=>({kind:'value',value:{handle:2}}),
      invokeAsync:()=>{throw Error('unexpected second acknowledgement');},
      cancelOutputStream() { attempts++; if (synchronous) throw failure; return Promise.reject(failure); },
      releaseOutputStream() { released++; }, close() { closed++; },
    };
    const failed = vm.runInNewContext(source + '; __withStreamClose(backend)', {backend:failedBackend, Promise});
    const lease = failed.invokeSync(factory,[]).value;
    const first = failed.cancelOutputStream(lease), second = failed.cancelOutputStream(lease);
    assert.equal(first,second);
    const results = await Promise.allSettled([first,second,failed.invokeAsync(cancel,[lease]),failed.releaseOutputStream(lease),failed.close()]);
    for (const result of results) { assert.equal(result.status,'rejected'); assert.equal(result.reason,failure); }
    assert.deepEqual([attempts,released,closed],[1,0,0]);
  }
  await checkOwnedStreamClose();
})().catch(error=>{console.error(error);process.exitCode=1;});
"#);
        let output = std::process::Command::new("node")
            .args(["-e", &script])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn platform_roots_require_runtime_and_declaration_companions() {
        let files = vec![
            source("node/index.js", "export {};".to_owned()),
            declaration("node/index.d.ts", "export {};".to_owned()),
            source("browser/platform.js", "export {};".to_owned()),
            source("browser/index.js", "export {};".to_owned()),
            declaration("browser/index.d.ts", "export {};".to_owned()),
        ];
        validate_files(&files).expect("platform entrypoint companions are valid");
        for path in [
            "node/index.js",
            "node/index.d.ts",
            "browser/platform.js",
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
