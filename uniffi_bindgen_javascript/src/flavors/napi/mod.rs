/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Napi flavor backend.
//!
//! The heavy lifting (Rust codegen via proc-macro2/quote, type lowering/
//! lifting, callback trait bridging, async handling) lives in
//! `codegen.rs`; this module owns the file layout and the thin JS
//! adapter that satisfies the high-level TS API.

use anyhow::{Context, Result};
use camino::Utf8Path;
use fs_err as fs;
use heck::ToUpperCamelCase;
use serde::Serialize;
use sha2::{Digest, Sha256};
use uniffi_bindgen::interface::{AsType, Callable, Method, Type};
use uniffi_bindgen::interface::{ObjectImpl, TraitKind};
use uniffi_bindgen::Component;

use crate::JsConfig;

pub(crate) mod codegen;

pub fn emit(
    dir: &Utf8Path,
    component: &Component<JsConfig>,
    default_addon_path: Option<&str>,
) -> Result<()> {
    let ci = &component.ci;

    // 1. Rust napi bridge — one .rs file per component crate, to be included
    //    by the host `.node` crate via `include!` or `mod`.
    let rust_source = codegen::render_napi_rust(ci)?;
    let rust_path = dir.join(format!("{}.rs", ci.crate_name()));
    fs::write(rust_path, rust_source)?;

    // 2. JS adapter that imports the compiled `.node` addon and re-exports
    //    it under the names the high-level common/*.ts layer expects. The
    //    adapter is intentionally thin: every non-trivial conversion lives
    //    in `uniffi_runtime_javascript`.
    let adapter = render_backend_adapter(ci, BackendAdapterTarget::Node { default_addon_path });
    fs::write(dir.join("backend-napi.ts"), adapter)?;

    // 3. Flavor entry point — the only file the application imports.
    //    Re-exports the high-level API from ../common wired through the
    //    backend adapter above.
    let index = render_index(ci);
    fs::write(dir.join("index.ts"), index)?;
    Ok(())
}

pub fn emit_ohos(dir: &Utf8Path, component: &Component<JsConfig>) -> Result<()> {
    let ci = &component.ci;

    let facade_contract = render_ohos_facade_contract(ci)?;
    let contract_digest = sha256_text(&facade_contract);
    let identity_export = ohos_bridge_identity_export(&contract_digest);
    let rust_source = codegen::render_ohos_rust(ci, &identity_export, &contract_digest)?;
    let rust_path = dir.join(format!("{}.rs", ci.crate_name()));
    fs::write(rust_path, rust_source)?;

    let adapter = render_ohos_backend_adapter(ci);
    fs::write(dir.join("backend-ohos.ts"), adapter)?;

    let extra_types = render_ohos_extra_types(ci)?;
    fs::write(
        dir.join(format!("{}.ohos-extra-types.d.ts", ci.crate_name())),
        extra_types,
    )?;

    fs::write(
        dir.join(format!("{}.ohos-facade.json", ci.crate_name())),
        facade_contract,
    )?;

    let stream_helpers = render_ohos_stream_helpers(ci);
    fs::write(dir.join("stream.ts"), stream_helpers)?;

    let index = render_ohos_index(ci);
    fs::write(dir.join("index.ts"), index)?;
    Ok(())
}

fn sha256_text(value: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(value.as_bytes());
    format!("{:x}", digest.finalize())
}

pub(crate) fn ohos_bridge_identity_export(contract_digest: &str) -> String {
    let encoded = contract_digest
        .bytes()
        .map(|byte| match byte {
            b'0'..=b'9' => (b'a' + (byte - b'0')) as char,
            b'a'..=b'f' => (b'k' + (byte - b'a')) as char,
            b'A'..=b'F' => (b'k' + (byte - b'A')) as char,
            _ => '_',
        })
        .collect::<String>();
    format!("uniffiohosbridgeidentity{encoded}")
}

enum BackendAdapterTarget<'a> {
    Node { default_addon_path: Option<&'a str> },
    Ohos { native_module: &'a str },
}

impl BackendAdapterTarget<'_> {
    fn banner(&self) -> &'static str {
        match self {
            Self::Node { .. } => "napi flavor",
            Self::Ohos { .. } => "harmony/ohos flavor",
        }
    }

    fn bridge_label(&self) -> &'static str {
        match self {
            Self::Node { .. } => "N-API bridge",
            Self::Ohos { .. } => "NAPI-OHOS bridge",
        }
    }

    fn native_artifact(&self) -> &'static str {
        match self {
            Self::Node { .. } => "compiled `.node` addon",
            Self::Ohos { .. } => "compiled `lib*.so` native module",
        }
    }

    fn runtime_label(&self) -> &'static str {
        match self {
            Self::Node { .. } => "napi-rs",
            Self::Ohos { .. } => "napi-ohos",
        }
    }

    fn dynamic_value_type(&self) -> &'static str {
        match self {
            Self::Node { .. } => "unknown",
            Self::Ohos { .. } => "UniffiValue",
        }
    }

    fn prelude(&self, namespace: &str) -> String {
        match self {
            Self::Node { default_addon_path } => {
                let env_var = napi_path_env_var(namespace);
                let default_addon_path = default_addon_path
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| format!("./{namespace}.node"));
                format!(
                    "import {{ createRequire }} from \"node:module\";\n\
                     import {{ resolve }} from \"node:path\";\n\
                     const require = createRequire(import.meta.url);\n\
                     const __uniffiNamespace = \"{namespace}\";\n\
                     const __uniffiSpecificNapiPathEnv = \"{env_var}\";\n\
                     const __uniffiDefaultAddonPath = \"{default_addon_path}\";\n\
                     \n\
                     function __uniffiResolveEnvAddonPath(path: string): string {{\n\
                         return path.startsWith(\"/\") || /^[A-Za-z]:[\\\\/]/.test(path) ? path : resolve(path);\n\
                     }}\n\
                     \n\
                     function __uniffiAddonCandidate(): {{ label: string; specifier: string }} {{\n\
                         const specific = process.env[__uniffiSpecificNapiPathEnv];\n\
                         if (specific && specific.length > 0) {{\n\
                             return {{ label: __uniffiSpecificNapiPathEnv, specifier: __uniffiResolveEnvAddonPath(specific) }};\n\
                         }}\n\
                         const generic = process.env.UNIFFI_NAPI_PATH;\n\
                         if (generic && generic.length > 0) {{\n\
                             return {{ label: \"UNIFFI_NAPI_PATH\", specifier: __uniffiResolveEnvAddonPath(generic) }};\n\
                         }}\n\
                         return {{ label: \"default\", specifier: __uniffiDefaultAddonPath }};\n\
                     }}\n\
                     \n\
                     function __uniffiLoadNativeAddon(): Record<string, unknown> {{\n\
                         const candidate = __uniffiAddonCandidate();\n\
                         try {{\n\
                             return require(candidate.specifier) as Record<string, unknown>;\n\
                         }} catch (error) {{\n\
                             const cause = error instanceof Error ? `${{error.name}}: ${{error.message}}` : String(error);\n\
                             throw new Error(\n\
                                 `failed to load UniFFI N-API addon for namespace \"${{__uniffiNamespace}}\" from ${{candidate.label}} (${{candidate.specifier}}). ` +\n\
                                     `Run \"uniffi-bindgen javascript build-napi --manifest-path <Cargo.toml> --out-dir <generated>\" ` +\n\
                                     `so ${{__uniffiDefaultAddonPath}} exists, or set ${{__uniffiSpecificNapiPathEnv}}=/absolute/path/to/${{__uniffiNamespace}}.node ` +\n\
                                     `or UNIFFI_NAPI_PATH=/absolute/path/to/addon.node. Cause: ${{cause}}`,\n\
                             );\n\
                         }}\n\
                     }}\n\
                     \n\
                     const native = __uniffiLoadNativeAddon();\n\n"
                )
            }
            Self::Ohos { native_module } => format!(
                "import * as native from \"{native_module}\";\n\
                 \n\
                 type UniffiPrimitive = null | undefined | string | number | boolean | bigint;\n\
                 type UniffiValue = UniffiPrimitive | object;\n\
                 \n\
                 // Harmony/OpenHarmony loads NAPI-OHOS modules through native\n\
                 // `lib*.so` imports declared in the consuming application's oh-package.json5.\n\n"
            ),
        }
    }
}

fn render_backend_adapter(
    ci: &uniffi_bindgen::ComponentInterface,
    target: BackendAdapterTarget<'_>,
) -> String {
    let namespace = ci.namespace();
    let banner = target.banner();
    let bridge_label = target.bridge_label();
    let native_artifact = target.native_artifact();
    let runtime_label = target.runtime_label();
    let dynamic_type = target.dynamic_value_type();
    let prelude = target.prelude(namespace);
    let name_map_literal =
        crate::name_map::render_name_map_js_literal(&crate::name_map::collect(ci));
    let enum_shape_helpers = crate::enum_shape::helper_ts(dynamic_type);
    format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript ({banner}).\n\
         //\n\
         // This file loads the {native_artifact} produced from the\n\
         // generated `{namespace}.rs` {bridge_label} and exposes its symbols under\n\
         // the low-level FFI contract defined in\n\
         // `uniffi_runtime_javascript`. The high-level API in\n\
         // `../common/api.ts` is identical to every other flavor and\n\
         // imports this adapter only through that contract.\n\
         \n\
         {prelude}\
         \n\
         // Generator-computed map from `common/api.ts` low-level\n\
         // snake_case dispatch keys to the `lowerCamelCase` names that\n\
         // {runtime_label} actually exports on the native module. Keeping this\n\
         // as a static literal (not a runtime heuristic) means the two\n\
         // sides can't drift: `name_map::collect` walks the same IR\n\
         // shape `api_module/mod.rs` walks.\n\
         const __uniffiNameMap: Record<string, string> = {name_map_literal} as Record<string, string>;\n\
         \n\
         {enum_shape_helpers}\n\
         \n\
         function __uniffiIsObjectFreeKey(name: string): boolean {{\n\
             return name.startsWith(\"__uniffi_\") && name.endsWith(\"_object_free\");\n\
         }}\n\
         \n\
         function __uniffiIsStreamNextKey(name: string): boolean {{\n\
             return name.endsWith(\"_stream_next\");\n\
         }}\n\
         \n\
         // napi-rs represents optional object fields with the host's optional\n\
         // property rules. Project the generated bridge struct into the exact\n\
         // internal tagged union before common/api.ts sees it, so Done has no\n\
         // payload and Item(None) remains an own value:null field.\n\
         function __uniffiNormalizeStreamStep(value: {dynamic_type}): {dynamic_type} {{\n\
             if (value === null || typeof value !== \"object\") return value;\n\
             const raw = value as Record<string, {dynamic_type}>;\n\
             if (raw.kind === \"item\" && Object.prototype.hasOwnProperty.call(raw, \"value\")) {{\n\
                 return {{ kind: \"item\", value: raw.value }};\n\
             }}\n\
             if (raw.kind === \"done\") {{\n\
                 return {{ kind: \"done\" }};\n\
             }}\n\
             if (raw.kind === \"error\" && Object.prototype.hasOwnProperty.call(raw, \"error\")) {{\n\
                 return {{ kind: \"error\", error: raw.error }};\n\
             }}\n\
             return value;\n\
         }}\n\
         \n\
         function __uniffiCallbackErrorPayload(error: {dynamic_type}, shape: {dynamic_type}): {dynamic_type} {{\n\
             if (error !== null && typeof error === \"object\") {{\n\
                 const raw = error as Record<string, {dynamic_type}>;\n\
                 if (shape === \"flat\") {{\n\
                     if (typeof raw.variant === \"string\") return raw.variant;\n\
                     if (typeof raw.tag === \"string\") return raw.tag;\n\
                     if (typeof raw.type === \"string\") return raw.type;\n\
                 }}\n\
                 if (typeof raw.tag === \"string\" || typeof raw.type === \"string\") {{\n\
                     return __uniffiLowerShape(raw);\n\
                 }}\n\
                 if (typeof raw.variant === \"string\") {{\n\
                     const data = raw.data;\n\
                     const payload: Record<string, {dynamic_type}> = {{ tag: raw.variant }};\n\
                     if (data !== null && typeof data === \"object\" && !Array.isArray(data)) {{\n\
                         Object.assign(payload, data as Record<string, {dynamic_type}>);\n\
                     }}\n\
                     return __uniffiLowerShape(payload);\n\
                 }}\n\
             }}\n\
             throw error;\n\
         }}\n\
         \n\
         const __uniffiReturnedCallbacks = new Map<number, Record<string, {dynamic_type}>>();\n\
         let __uniffiNextReturnedCallbackId = 1;\n\
         \n\
         function __uniffiDispatchReturnedCallback(...rawArgs: {dynamic_type}[]): {dynamic_type} {{\n\
             const args = rawArgs.length >= 3 && (rawArgs[0] === null || rawArgs[0] === undefined || rawArgs[0] instanceof Error)\n\
                 ? rawArgs.slice(1)\n\
                 : rawArgs;\n\
             const id = args[0];\n\
             const method = args[1];\n\
             if (typeof id !== \"number\" || typeof method !== \"string\") {{\n\
                 throw new Error(\"invalid uniffi returned-callback dispatch arguments\");\n\
             }}\n\
             const obj = __uniffiReturnedCallbacks.get(id);\n\
             if (!obj) {{\n\
                 throw new Error(`uniffi returned callback ${{id}} is not available`);\n\
             }}\n\
             const fn = obj[method];\n\
             if (typeof fn !== \"function\") {{\n\
                 throw new Error(`uniffi returned callback ${{id}} has no method ${{method}}`);\n\
             }}\n\
             return (fn as (...a: {dynamic_type}[]) => {dynamic_type})(...args.slice(2).map(__uniffiLiftShape));\n\
         }}\n\
         \n\
         function __uniffiStoreCallbackReturn(value: {dynamic_type}): {{ id: number }} {{\n\
             const marker = value !== null && typeof value === \"object\" && (value as {{ __uniffiCallback?: boolean }}).__uniffiCallback === true\n\
                 ? value as {{ object: {dynamic_type}, fallibleMethods?: Record<string, string>, asyncMethods?: Record<string, boolean>, callbackReturnMethods?: Record<string, boolean> }}\n\
                 : undefined;\n\
             const obj = marker === undefined\n\
                 ? __uniffiNormalizeCallbackObject(value)\n\
                 : __uniffiNormalizeCallbackObject(marker.object, marker);\n\
             const id = __uniffiNextReturnedCallbackId++;\n\
             __uniffiReturnedCallbacks.set(id, obj as Record<string, {dynamic_type}>);\n\
             return {{ id }};\n\
         }}\n\
         \n\
         function __uniffiNormalizeCallbackObject(obj: {dynamic_type}, marker?: {{ fallibleMethods?: Record<string, string>, asyncMethods?: Record<string, boolean>, callbackReturnMethods?: Record<string, boolean> }}): {dynamic_type} {{\n\
             if (obj === null || typeof obj !== \"object\") return obj;\n\
             const fallibleMethods = marker?.fallibleMethods ?? {{}};\n\
             const asyncMethods = marker?.asyncMethods ?? {{}};\n\
             const callbackReturnMethods = marker?.callbackReturnMethods ?? {{}};\n\
             const out: Record<string, {dynamic_type}> = {{}};\n\
             for (const [k, v] of Object.entries(obj as Record<string, {dynamic_type}>)) {{\n\
                 if (typeof v === \"function\") {{\n\
                     out[k] = (...args: {dynamic_type}[]) => {{\n\
                         const callArgs = args.length >= 2 && (args[0] === null || args[0] === undefined || args[0] instanceof Error)\n\
                             ? args.slice(1)\n\
                             : args;\n\
                         const liftedArgs = callArgs.map(__uniffiLiftShape);\n\
                         const fn = v as (...a: {dynamic_type}[]) => {dynamic_type};\n\
                         const errorShape = fallibleMethods[k];\n\
                         const isAsync = asyncMethods[k] === true;\n\
                         const returnsCallback = callbackReturnMethods[k] === true;\n\
                         if (!errorShape) {{\n\
                             if (isAsync) {{\n\
                                 return Promise.resolve(fn(...liftedArgs)).then((value) => returnsCallback ? __uniffiStoreCallbackReturn(value) : __uniffiLowerShape(value));\n\
                             }}\n\
                             return __uniffiLowerShape(fn(...liftedArgs));\n\
                         }}\n\
                         if (isAsync) {{\n\
                             return Promise.resolve(fn(...liftedArgs)).then(\n\
                                 (value) => ({{ ok: true, value: returnsCallback ? __uniffiStoreCallbackReturn(value) : __uniffiLowerShape(value) }}),\n\
                                 (error) => ({{ ok: false, error: __uniffiCallbackErrorPayload(error, errorShape) }}),\n\
                             );\n\
                         }}\n\
                         try {{\n\
                             return {{ ok: true, value: __uniffiLowerShape(fn(...liftedArgs)) }};\n\
                         }} catch (error) {{\n\
                             return {{ ok: false, error: __uniffiCallbackErrorPayload(error, errorShape) }};\n\
                         }}\n\
                     }};\n\
                 }} else {{\n\
                     out[k] = v;\n\
                 }}\n\
             }}\n\
             out.__uniffiCallbackDispatcher = __uniffiDispatchReturnedCallback;\n\
             return out;\n\
         }}\n\
         \n\
         // Unwrap the tagged callback marker emitted by `common/api.ts`.\n\
         // For the {bridge_label}, `#[napi(object)]` structs with `ThreadsafeFunction`\n\
         // fields want the raw JS object (e.g. `{{ log: fn }}`), not a\n\
         // numeric handle. Other backends (wasm) translate the marker\n\
         // differently; see their adapters.\n\
         const __uniffiCoerce = (a: {dynamic_type}): {dynamic_type} => {{\n\
             if (\n\
                 a !== null &&\n\
                 typeof a === \"object\" &&\n\
                 (a as {{ __uniffiInputStream?: boolean }}).__uniffiInputStream === true\n\
             ) {{\n\
                 const marker = a as {{ handle: number; next: {dynamic_type}; cancel: {dynamic_type} }};\n\
                 return {{ handle: marker.handle, next: marker.next, cancel: marker.cancel }};\n\
             }}\n\
             if (\n\
                 a !== null &&\n\
                 typeof a === \"object\" &&\n\
                 (a as {{ __uniffiCallback?: boolean }}).__uniffiCallback === true\n\
             ) {{\n\
                 return __uniffiNormalizeCallbackObject((a as {{ object: {dynamic_type} }}).object, a as {{ fallibleMethods?: Record<string, string>, asyncMethods?: Record<string, boolean>, callbackReturnMethods?: Record<string, boolean> }});\n\
             }}\n\
             return a;\n\
         }};\n\
         \n\
         const backend: Record<string, {dynamic_type}> = new Proxy(\n\
             {{}} as Record<string, {dynamic_type}>,\n\
             {{\n\
                 get(_t, name: string) {{\n\
                     if (name === \"__uniffiAbiVersion\") return 2;\n\
                     // `common/objects.ts` uses the wasm registry destructor\n\
                     // key for every flavor. N-API object wrappers are native\n\
                     // class instances whose lifetime is owned by {runtime_label},\n\
                     // so the generated backend intentionally treats those\n\
                     // destructor calls as idempotent no-ops.\n\
                     if (__uniffiIsObjectFreeKey(name)) return (_handle: {dynamic_type}): void => {{}};\n\
                     // Translate low-level key → addon export name. Fall\n\
                     // back to the raw name so anything the generator didn't\n\
                     // put in the map still surfaces a clear `undefined`\n\
                     // rather than silently hitting an unrelated member.\n\
                     const exportName = __uniffiNameMap[name] ?? name;\n\
                     const v = (native as Record<string, {dynamic_type}>)[exportName];\n\
                     if (typeof v !== \"function\") return v;\n\
                     const fn = v as (...args: {dynamic_type}[]) => {dynamic_type};\n\
                     return (...args: {dynamic_type}[]) => {{\n\
                         // Lower args on the way in: tag -> type for each\n\
                         // plain object (enum, nested in seq/opt/record).\n\
                         // Callback markers are still unwrapped first so\n\
                         // the lowering skips the `object` payload.\n\
                         const lowered = args.map((a) =>\n\
                             __uniffiLowerShape(__uniffiCoerce(a)),\n\
                         );\n\
                         let result: {dynamic_type};\n\
                         try {{\n\
                             result = fn.apply(native, lowered);\n\
                         }} catch (err) {{\n\
                             // {bridge_label} errors can carry an enum payload with `type`;\n\
                             // lift it symmetrically so `common/errors.ts`\n\
                             // consumers see `tag`.\n\
                             throw __uniffiLiftShape(err);\n\
                         }}\n\
                         if (\n\
                             result !== null &&\n\
                             typeof result === \"object\" &&\n\
                             typeof (result as {{ then?: {dynamic_type} }}).then === \"function\"\n\
                         ) {{\n\
                             return (result as Promise<{dynamic_type}>).then(\n\
                                 (value) => {{\n\
                                     const lifted = __uniffiLiftShape(value);\n\
                                     return __uniffiIsStreamNextKey(name)\n\
                                         ? __uniffiNormalizeStreamStep(lifted)\n\
                                         : lifted;\n\
                                 }},\n\
                                 (err) => {{\n\
                                     throw __uniffiLiftShape(err);\n\
                                 }},\n\
                             );\n\
                         }}\n\
                         const lifted = __uniffiLiftShape(result);\n\
                         return __uniffiIsStreamNextKey(name)\n\
                             ? __uniffiNormalizeStreamStep(lifted)\n\
                             : lifted;\n\
                     }};\n\
                 }},\n\
             }},\n\
         );\n\
         \n\
         export default backend;\n"
    )
}

fn render_ohos_backend_adapter(ci: &uniffi_bindgen::ComponentInterface) -> String {
    let namespace = ci.namespace();
    let native_module = format!(
        "lib{}.so",
        crate::js_names::ohos_native_library_stem(namespace)
    );
    render_backend_adapter(
        ci,
        BackendAdapterTarget::Ohos {
            native_module: &native_module,
        },
    )
}

pub(crate) fn napi_path_env_var(namespace: &str) -> String {
    let mut out = String::from("UNIFFI_");
    let mut last_was_sep = false;
    for ch in namespace.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_uppercase());
            last_was_sep = false;
        } else if !last_was_sep {
            out.push('_');
            last_was_sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out.push_str("_NAPI_PATH");
    out
}

fn render_index(ci: &uniffi_bindgen::ComponentInterface) -> String {
    let namespace = ci.namespace();
    format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript (napi flavor).\n\
         //\n\
         // Node/napi entry for namespace `{namespace}`. Applications\n\
         // import from this file and see the same high-level API as the\n\
         // wasm and electron entries. The backend installs itself on\n\
         // first import so callers never have to plumb it through.\n\
         \n\
         import backend from \"./backend-napi.ts\";\n\
         import {{ __installBackend }} from \"../common/runtime.ts\";\n\
         __installBackend(backend);\n\
         export * from \"../common/api.ts\";\n"
    )
}

fn render_ohos_index(ci: &uniffi_bindgen::ComponentInterface) -> String {
    let namespace = ci.namespace();
    let native_module = format!(
        "lib{}.so",
        crate::js_names::ohos_native_library_stem(namespace)
    );
    format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript (harmony/ohos flavor).\n\
         //\n\
         // Harmony/OpenHarmony entry for namespace `{namespace}`. Applications\n\
         // import from this file and see the same high-level API as the wasm,\n\
         // node, and electron entries. The backend installs itself on first\n\
         // import and expects the consuming Harmony app to declare\n\
         // `{native_module}` in oh-package.json5.\n\
         \n\
         import backend from \"./backend-ohos.ts\";\n\
         import {{ __installBackend }} from \"../common/runtime.ts\";\n\
         __installBackend(backend);\n\
         export * from \"../common/api.ts\";\n\
         export * from \"./stream.ts\";\n"
    )
}

fn render_ohos_stream_helpers(ci: &uniffi_bindgen::ComponentInterface) -> String {
    let mut fn_names = Vec::new();
    let mut type_names = std::collections::BTreeSet::new();
    let mut wrappers = String::new();

    for func in ci.function_definitions() {
        let Some(Type::Stream { item_type, .. }) = func.return_type() else {
            continue;
        };

        let fn_name = crate::js_names::function_name(func.name());
        fn_names.push(fn_name.clone());
        collect_ohos_type_names(item_type, &mut type_names);

        let args = func
            .arguments()
            .iter()
            .map(|arg| {
                let ty = arg.as_type();
                collect_ohos_type_names(&ty, &mut type_names);
                format!(
                    "{}: {}",
                    crate::js_names::field_name(arg.name()),
                    ohos_native_ts_type(&ty)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let pass = func
            .arguments()
            .iter()
            .map(|arg| crate::js_names::field_name(arg.name()))
            .collect::<Vec<_>>()
            .join(", ");
        let item_ts = ohos_native_ts_type(item_type);
        let wrapper_name = format!("{fn_name}Stream");
        wrappers.push_str(&format!(
            "\nexport function {wrapper_name}({args}): UniFfiStream<{item_ts}> {{\n    return {fn_name}({pass});\n}}\n"
        ));
    }

    fn_names.sort();
    fn_names.dedup();
    let function_import = if fn_names.is_empty() {
        String::new()
    } else {
        format!(
            "import {{ {} }} from \"../common/api.ts\";\n",
            fn_names.join(", ")
        )
    };
    let type_import = if type_names.is_empty() {
        String::new()
    } else {
        format!(
            "import type {{ {} }} from \"../common/public-types.ts\";\n",
            type_names.into_iter().collect::<Vec<_>>().join(", ")
        )
    };

    format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript (harmony/ohos stream helpers).\n\
         //\n\
         // Harmony/OpenHarmony consumers can use this small wrapper when ArkTS\n\
         // tooling is stricter than standard JavaScript `for await` syntax.\n\
         \n\
         {function_import}{type_import}import type {{ UniFfiStream }} from \"../common/runtime.ts\";\n\
         {wrappers}"
    )
}

fn collect_ohos_type_names(ty: &Type, out: &mut std::collections::BTreeSet<String>) {
    match ty {
        Type::Record { name, .. }
        | Type::Enum { name, .. }
        | Type::Object { name, .. }
        | Type::CallbackInterface { name, .. } => {
            out.insert(name.clone());
        }
        Type::Optional { inner_type }
        | Type::Sequence { inner_type }
        | Type::Box { inner_type }
        | Type::Set { inner_type }
        | Type::Stream {
            item_type: inner_type,
            ..
        }
        | Type::InputStream {
            item_type: inner_type,
            ..
        } => {
            collect_ohos_type_names(inner_type, out);
        }
        Type::Map {
            key_type,
            value_type,
        } => {
            collect_ohos_type_names(key_type, out);
            collect_ohos_type_names(value_type, out);
        }
        Type::Custom { builtin, .. } => {
            collect_ohos_type_names(builtin, out);
        }
        Type::UInt8
        | Type::Int8
        | Type::UInt16
        | Type::Int16
        | Type::UInt32
        | Type::Int32
        | Type::UInt64
        | Type::Int64
        | Type::Float32
        | Type::Float64
        | Type::Boolean
        | Type::String
        | Type::Bytes
        | Type::Timestamp
        | Type::Duration => {}
    }
}

fn render_ohos_extra_types(ci: &uniffi_bindgen::ComponentInterface) -> Result<String> {
    let mut out = String::from(
        "// AUTOGENERATED by uniffi_bindgen_javascript (harmony/ohos type sidecar).\n",
    );
    if !codegen::collect_input_stream_descriptors(ci)?.is_empty() {
        out.push_str(
            "export interface __UniffiInputStream<T> {\n\
             handle: number;\n\
             next(error: Error | null, handle: number): Promise<T>;\n\
             cancel(error: Error | null, handle: number): void;\n\
             }\n",
        );
    }
    for callback in ci.callback_interface_definitions() {
        out.push_str(&render_ohos_callback_type(
            callback.name(),
            &callback.methods(),
        ));
    }
    for object in ci.object_definitions() {
        if matches!(
            object.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            out.push_str(&render_ohos_callback_type(object.name(), &object.methods()));
        }
    }
    Ok(out)
}

const OHOS_FACADE_SCHEMA_VERSION: u32 = 4;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OhosFacadeContract {
    schema_version: u32,
    component: String,
    output_streams: Vec<OhosOutputStreamContract>,
    input_streams: Vec<OhosInputStreamContract>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OhosOutputStreamContract {
    function: String,
    next_function: String,
    cancel_function: String,
    stream_factory: String,
    pull_class: String,
    step_type: String,
    item_type: OhosTypeDescriptor,
    error_type: OhosTypeDescriptor,
    arguments: Vec<OhosFacadeArgument>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OhosInputStreamContract {
    suffix: String,
    canonical: String,
    fingerprint: String,
    item_type: OhosTypeDescriptor,
    error_type: OhosTypeDescriptor,
    next_type: String,
    writer_class: String,
    source_class: String,
    channel_class: String,
    factory: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OhosFacadeArgument {
    name: String,
    r#type: OhosTypeDescriptor,
}

#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
enum OhosTypeDescriptor {
    Number,
    Bigint,
    Boolean,
    String,
    ArrayBuffer,
    Named {
        name: String,
    },
    Optional {
        inner: Box<OhosTypeDescriptor>,
    },
    Sequence {
        inner: Box<OhosTypeDescriptor>,
    },
    Set {
        inner: Box<OhosTypeDescriptor>,
    },
    InputSource {
        suffix: String,
        #[serde(rename = "nextType")]
        next_type: String,
    },
}

fn render_ohos_facade_contract(ci: &uniffi_bindgen::ComponentInterface) -> Result<String> {
    validate_ohos_stream_facade_type_graphs(ci)?;

    let input_streams = codegen::collect_input_stream_descriptors(ci)?
        .into_iter()
        .map(|descriptor| {
            let suffix = descriptor.suffix().to_string();
            Ok(OhosInputStreamContract {
                canonical: descriptor.canonical().to_string(),
                fingerprint: descriptor.fingerprint().to_string(),
                item_type: ohos_facade_type_descriptor(descriptor.item_type())?,
                error_type: ohos_facade_type_descriptor(descriptor.error_type())?,
                next_type: format!("__UniffiInputStream{suffix}Next"),
                writer_class: format!("{suffix}InputWriter"),
                source_class: format!("{suffix}InputSource"),
                channel_class: format!("{suffix}InputChannel"),
                factory: format!("create{suffix}InputChannel"),
                suffix,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    let mut output_streams = Vec::new();
    for function in ci.function_definitions() {
        let Some(Type::Stream {
            item_type,
            error_type,
            ..
        }) = function.return_type()
        else {
            continue;
        };
        let function_name = crate::js_names::function_name(function.name());
        let class_prefix = function.name().to_upper_camel_case();
        let arguments = function
            .arguments()
            .iter()
            .map(|argument| {
                Ok(OhosFacadeArgument {
                    name: crate::js_names::field_name(argument.name()),
                    r#type: ohos_facade_type_descriptor(&argument.as_type())?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        output_streams.push(OhosOutputStreamContract {
            next_function: crate::js_names::function_name(&crate::dispatch_key::stream_next_key(
                function.name(),
            )),
            cancel_function: crate::js_names::function_name(
                &crate::dispatch_key::stream_cancel_key(function.name()),
            ),
            stream_factory: format!("{function_name}Stream"),
            pull_class: format!("{class_prefix}PullStream"),
            step_type: format!("Uniffi{class_prefix}StreamNext"),
            item_type: ohos_facade_type_descriptor(item_type)?,
            error_type: ohos_facade_type_descriptor(error_type)?,
            arguments,
            function: function_name,
        });
    }

    let contract = OhosFacadeContract {
        schema_version: OHOS_FACADE_SCHEMA_VERSION,
        component: ci.crate_name().to_string(),
        output_streams,
        input_streams,
    };
    validate_ohos_facade_contract_names(ci, &contract)?;
    serde_json::to_string_pretty(&contract).map_err(Into::into)
}

fn validate_ohos_stream_facade_type_graphs(ci: &uniffi_bindgen::ComponentInterface) -> Result<()> {
    for function in ci.function_definitions() {
        if let Some(Type::Stream {
            item_type,
            error_type,
            ..
        }) = function.return_type()
        {
            validate_ohos_facade_type_graph(
                ci,
                item_type,
                &format!("function `{}` output item", function.name()),
            )?;
            validate_ohos_facade_type_graph(
                ci,
                error_type,
                &format!("function `{}` output error", function.name()),
            )?;
            for argument in function.arguments() {
                validate_ohos_facade_type_graph(
                    ci,
                    &argument.as_type(),
                    &format!(
                        "function `{}` output start argument `{}`",
                        function.name(),
                        argument.name()
                    ),
                )?;
            }
        }
        validate_ohos_input_callable_type_graphs(
            ci,
            function,
            &format!("function `{}`", function.name()),
        )?;
    }

    for object in ci.object_definitions() {
        if !matches!(
            object.imp(),
            ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly)
        ) {
            continue;
        }
        for constructor in object.constructors() {
            validate_ohos_input_callable_type_graphs(
                ci,
                constructor,
                &format!(
                    "object `{}` constructor `{}`",
                    object.name(),
                    constructor.name()
                ),
            )?;
        }
        for method in object.methods() {
            validate_ohos_input_callable_type_graphs(
                ci,
                method,
                &format!("object `{}` method `{}`", object.name(), method.name()),
            )?;
        }
    }

    for record in ci.record_definitions() {
        for constructor in record.constructors() {
            validate_ohos_input_callable_type_graphs(
                ci,
                constructor,
                &format!(
                    "record `{}` constructor `{}`",
                    record.name(),
                    constructor.name()
                ),
            )?;
        }
        for method in record.methods() {
            validate_ohos_input_callable_type_graphs(
                ci,
                method,
                &format!("record `{}` method `{}`", record.name(), method.name()),
            )?;
        }
    }

    for enum_ in ci.enum_definitions() {
        for constructor in enum_.constructors() {
            validate_ohos_input_callable_type_graphs(
                ci,
                constructor,
                &format!(
                    "enum `{}` constructor `{}`",
                    enum_.name(),
                    constructor.name()
                ),
            )?;
        }
        for method in enum_.methods() {
            validate_ohos_input_callable_type_graphs(
                ci,
                method,
                &format!("enum `{}` method `{}`", enum_.name(), method.name()),
            )?;
        }
    }
    Ok(())
}

fn validate_ohos_input_callable_type_graphs(
    ci: &uniffi_bindgen::ComponentInterface,
    callable: &dyn Callable,
    callable_path: &str,
) -> Result<()> {
    if !callable
        .arguments()
        .iter()
        .any(|argument| matches!(argument.as_type(), Type::InputStream { .. }))
    {
        return Ok(());
    }
    for argument in callable.arguments() {
        validate_ohos_facade_type_graph(
            ci,
            &argument.as_type(),
            &format!("{callable_path} input-call argument `{}`", argument.name()),
        )?;
    }
    Ok(())
}

fn validate_ohos_facade_type_graph(
    ci: &uniffi_bindgen::ComponentInterface,
    ty: &Type,
    root_path: &str,
) -> Result<()> {
    fn visit(
        root: &uniffi_bindgen::ComponentInterface,
        ty: &Type,
        path: &str,
        visited: &mut std::collections::BTreeSet<(String, String, String)>,
    ) -> Result<()> {
        match ty {
            Type::Map { .. } => anyhow::bail!(
                "Harmony stream facade type `{path}` contains a map; ArkTS forbids Record/index-signature public types"
            ),
            Type::Record { module_path, name } => {
                let key = ("record".to_string(), module_path.clone(), name.clone());
                if !visited.insert(key) {
                    return Ok(());
                }
                let component = ohos_named_type_component(root, module_path, path)?;
                let expected = ty.clone();
                let record = component
                    .record_definitions()
                    .iter()
                    .find(|record| record.as_type() == expected)
                    .with_context(|| {
                        format!(
                            "resolving record `{module_path}::{name}` while validating Harmony stream facade type `{path}`"
                        )
                    })?;
                for field in record.fields() {
                    visit(
                        root,
                        &field.as_type(),
                        &format!("{path} -> record `{module_path}::{name}` field `{}`", field.name()),
                        visited,
                    )?;
                }
            }
            Type::Enum { module_path, name } => {
                let key = ("enum".to_string(), module_path.clone(), name.clone());
                if !visited.insert(key) {
                    return Ok(());
                }
                let component = ohos_named_type_component(root, module_path, path)?;
                let expected = ty.clone();
                let enum_ = component
                    .enum_definitions()
                    .iter()
                    .find(|enum_| enum_.as_type() == expected)
                    .with_context(|| {
                        format!(
                            "resolving enum `{module_path}::{name}` while validating Harmony stream facade type `{path}`"
                        )
                    })?;
                for variant in enum_.variants() {
                    for field in variant.fields() {
                        visit(
                            root,
                            &field.as_type(),
                            &format!(
                                "{path} -> enum `{module_path}::{name}` variant `{}` field `{}`",
                                variant.name(),
                                field.name()
                            ),
                            visited,
                        )?;
                    }
                }
            }
            Type::Optional { inner_type }
            | Type::Sequence { inner_type }
            | Type::Set { inner_type }
            | Type::Box { inner_type } => {
                visit(root, inner_type, &format!("{path} -> nested value"), visited)?;
            }
            Type::Custom {
                module_path,
                name,
                builtin,
            } => {
                visit(
                    root,
                    builtin,
                    &format!("{path} -> custom `{module_path}::{name}` builtin"),
                    visited,
                )?;
            }
            Type::InputStream {
                item_type,
                error_type,
                ..
            } => {
                visit(root, item_type, &format!("{path} -> input item"), visited)?;
                visit(
                    root,
                    error_type,
                    &format!("{path} -> input error"),
                    visited,
                )?;
            }
            Type::Stream { .. } => anyhow::bail!(
                "Harmony stream facade type `{path}` contains a nested output stream"
            ),
            Type::UInt8
            | Type::Int8
            | Type::UInt16
            | Type::Int16
            | Type::UInt32
            | Type::Int32
            | Type::UInt64
            | Type::Int64
            | Type::Float32
            | Type::Float64
            | Type::Boolean
            | Type::String
            | Type::Bytes
            | Type::Timestamp
            | Type::Duration
            | Type::Object { .. }
            | Type::CallbackInterface { .. } => {}
        }
        Ok(())
    }

    visit(ci, ty, root_path, &mut std::collections::BTreeSet::new())
}

fn ohos_named_type_component<'a>(
    root: &'a uniffi_bindgen::ComponentInterface,
    module_path: &str,
    type_path: &str,
) -> Result<&'a uniffi_bindgen::ComponentInterface> {
    let crate_name = module_path
        .split("::")
        .next()
        .context("named Harmony facade type has an empty module path")?;
    if root.crate_name() == crate_name {
        return Ok(root);
    }
    root.find_component_interface(crate_name).with_context(|| {
        format!(
            "resolving component `{crate_name}` for named Harmony stream facade type `{type_path}`"
        )
    })
}

fn validate_ohos_facade_contract_names(
    ci: &uniffi_bindgen::ComponentInterface,
    contract: &OhosFacadeContract,
) -> Result<()> {
    let mut occupied = crate::name_map::collect(ci)
        .into_iter()
        .map(|(_, export)| export)
        .collect::<std::collections::BTreeSet<_>>();
    for name in ci
        .record_definitions()
        .iter()
        .map(|definition| definition.name())
        .chain(
            ci.enum_definitions()
                .iter()
                .map(|definition| definition.name()),
        )
        .chain(
            ci.object_definitions()
                .iter()
                .map(|definition| definition.name()),
        )
        .chain(
            ci.callback_interface_definitions()
                .iter()
                .map(|definition| definition.name()),
        )
    {
        occupied.insert(name.to_string());
    }
    let mut generated = std::collections::BTreeSet::new();
    if !contract.output_streams.is_empty() || !contract.input_streams.is_empty() {
        for name in [
            "UniFfiStream",
            "UniFfiStreamResult",
            "UniFfiInputFailureData",
            "UniFfiInputFailure",
        ] {
            validate_generated_ohos_name(name, &occupied, &mut generated)?;
        }
    }
    for output in &contract.output_streams {
        for name in [output.stream_factory.as_str(), output.pull_class.as_str()] {
            validate_generated_ohos_name(name, &occupied, &mut generated)?;
        }
    }
    for input in &contract.input_streams {
        for name in [
            input.writer_class.as_str(),
            input.source_class.as_str(),
            input.channel_class.as_str(),
            input.factory.as_str(),
        ] {
            validate_generated_ohos_name(name, &occupied, &mut generated)?;
        }
    }
    Ok(())
}

fn validate_generated_ohos_name(
    name: &str,
    occupied: &std::collections::BTreeSet<String>,
    generated: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    if occupied.contains(name) || !generated.insert(name.to_string()) {
        anyhow::bail!("Harmony stream facade export name collision for `{name}`");
    }
    Ok(())
}

fn ohos_facade_type_descriptor(ty: &Type) -> Result<OhosTypeDescriptor> {
    Ok(match ty {
        Type::UInt8
        | Type::Int8
        | Type::UInt16
        | Type::Int16
        | Type::UInt32
        | Type::Int32
        | Type::Float32
        | Type::Float64
        | Type::Timestamp
        | Type::Duration => OhosTypeDescriptor::Number,
        Type::UInt64 | Type::Int64 => OhosTypeDescriptor::Bigint,
        Type::Boolean => OhosTypeDescriptor::Boolean,
        Type::String => OhosTypeDescriptor::String,
        Type::Bytes => OhosTypeDescriptor::ArrayBuffer,
        Type::Record { name, .. }
        | Type::Enum { name, .. }
        | Type::Object { name, .. }
        | Type::CallbackInterface { name, .. } => {
            OhosTypeDescriptor::Named { name: name.clone() }
        }
        Type::Optional { inner_type } => OhosTypeDescriptor::Optional {
            inner: Box::new(ohos_facade_type_descriptor(inner_type)?),
        },
        Type::Sequence { inner_type } => OhosTypeDescriptor::Sequence {
            inner: Box::new(ohos_facade_type_descriptor(inner_type)?),
        },
        Type::Set { inner_type } => OhosTypeDescriptor::Set {
            inner: Box::new(ohos_facade_type_descriptor(inner_type)?),
        },
        Type::Box { inner_type } | Type::Custom { builtin: inner_type, .. } => {
            ohos_facade_type_descriptor(inner_type)?
        }
        Type::InputStream { .. } => {
            let descriptor = codegen::describe_input_stream_type(ty)?;
            OhosTypeDescriptor::InputSource {
                suffix: descriptor.suffix().to_string(),
                next_type: format!("__UniffiInputStream{}Next", descriptor.suffix()),
            }
        }
        Type::Map { .. } => anyhow::bail!(
            "Harmony stream facade does not support map values because ArkTS forbids Record/index-signature public types"
        ),
        Type::Stream { .. } => anyhow::bail!(
            "Harmony stream facade does not support a nested output stream value or argument"
        ),
    })
}

fn render_ohos_callback_type(name: &str, methods: &[&Method]) -> String {
    let mut out = format!("export interface {name} {{\n");
    for method in methods {
        let args = method
            .arguments()
            .iter()
            .map(|arg| {
                format!(
                    "{}: {}",
                    crate::js_names::field_name(arg.name()),
                    ohos_native_ts_type(&arg.as_type())
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let ret = if method.throws_type().is_some() {
            format!(
                "Uniffi{}{}CallbackResult",
                name,
                method.name().to_upper_camel_case()
            )
        } else {
            method
                .return_type()
                .map(ohos_native_ts_type)
                .unwrap_or_else(|| "void".to_string())
        };
        let ret = if method.is_async() {
            format!("Promise<{ret}>")
        } else {
            ret
        };
        out.push_str(&format!(
            "{}?: ({args}) => {ret}\n",
            crate::js_names::method_name(method.name())
        ));
    }
    out.push_str("}\n");
    out
}

fn ohos_native_ts_type(ty: &Type) -> String {
    match ty {
        Type::UInt8
        | Type::Int8
        | Type::UInt16
        | Type::Int16
        | Type::UInt32
        | Type::Int32
        | Type::Float32
        | Type::Float64 => "number".to_string(),
        Type::UInt64 | Type::Int64 => "bigint".to_string(),
        Type::Boolean => "boolean".to_string(),
        Type::String => "string".to_string(),
        Type::Bytes => "ArrayBuffer".to_string(),
        Type::Record { name, .. }
        | Type::Enum { name, .. }
        | Type::Object { name, .. }
        | Type::CallbackInterface { name, .. } => name.clone(),
        Type::Optional { inner_type } => format!("{} | null", ohos_native_ts_type(inner_type)),
        Type::Sequence { inner_type } => format!("Array<{}>", ohos_native_ts_type(inner_type)),
        Type::Map { value_type, .. } => {
            format!("Record<string, {}>", ohos_native_ts_type(value_type))
        }
        Type::Box { inner_type } => ohos_native_ts_type(inner_type),
        Type::Set { inner_type } => format!("Set<{}>", ohos_native_ts_type(inner_type)),
        Type::Stream { item_type, .. } => {
            format!("AsyncIterable<{}>", ohos_native_ts_type(item_type))
        }
        Type::InputStream { item_type, .. } => {
            format!("AsyncIterable<{}>", ohos_native_ts_type(item_type))
        }
        Type::Timestamp | Type::Duration => "number".to_string(),
        Type::Custom { builtin, .. } => ohos_native_ts_type(builtin),
    }
}

#[cfg(test)]
mod ohos_facade_type_tests {
    use super::*;
    use uniffi_bindgen::ComponentInterface;
    use uniffi_meta::{
        EnumMetadata, EnumShape, FieldMetadata, FnMetadata, FnParamMetadata, MetadataGroup,
        NamespaceMetadata, RecordMetadata, VariantMetadata,
    };

    fn field(name: &str, ty: Type) -> FieldMetadata {
        FieldMetadata {
            name: name.to_string(),
            orig_name: None,
            ty,
            default: None,
            docstring: None,
        }
    }

    fn group(module_path: &str) -> MetadataGroup {
        MetadataGroup {
            namespace: NamespaceMetadata {
                crate_name: module_path.to_string(),
                name: module_path.to_string(),
            },
            namespace_docstring: None,
            items: Default::default(),
        }
    }

    fn record_type(module_path: &str, name: &str) -> Type {
        Type::Record {
            module_path: module_path.to_string(),
            name: name.to_string(),
        }
    }

    fn enum_type(module_path: &str, name: &str) -> Type {
        Type::Enum {
            module_path: module_path.to_string(),
            name: name.to_string(),
        }
    }

    fn map_type() -> Type {
        Type::Map {
            key_type: Box::new(Type::String),
            value_type: Box::new(Type::UInt32),
        }
    }

    fn output_stream(item_type: Type, error_type: Type) -> Type {
        Type::Stream {
            item_type: Box::new(item_type),
            error_type: Box::new(error_type),
            is_send: true,
        }
    }

    fn input_stream(item_type: Type, error_type: Type) -> Type {
        Type::InputStream {
            item_type: Box::new(item_type),
            error_type: Box::new(error_type),
            is_send: true,
        }
    }

    fn function(name: &str, inputs: Vec<FnParamMetadata>, return_type: Option<Type>) -> FnMetadata {
        FnMetadata {
            module_path: "facade_graph".to_string(),
            name: name.to_string(),
            orig_name: None,
            is_async: true,
            inputs,
            return_type,
            throws: None,
            checksum: None,
            docstring: None,
        }
    }

    fn record(module_path: &str, name: &str, fields: Vec<FieldMetadata>) -> RecordMetadata {
        RecordMetadata {
            module_path: module_path.to_string(),
            name: name.to_string(),
            orig_name: None,
            rust_path: None,
            remote: false,
            fields,
            docstring: None,
        }
    }

    #[test]
    fn structured_facade_types_cover_arkts_safe_shapes() {
        let ty = Type::Optional {
            inner_type: Box::new(Type::Sequence {
                inner_type: Box::new(Type::Custom {
                    module_path: "fixture::types".into(),
                    name: "UserId".into(),
                    builtin: Box::new(Type::UInt64),
                }),
            }),
        };
        let descriptor = ohos_facade_type_descriptor(&ty).unwrap();
        let value = serde_json::to_value(descriptor).unwrap();
        assert_eq!(value["kind"], "optional");
        assert_eq!(value["inner"]["kind"], "sequence");
        assert_eq!(value["inner"]["inner"]["kind"], "bigint");

        let object = Type::Object {
            module_path: "fixture::types".into(),
            name: "Counter".into(),
            imp: ObjectImpl::Struct,
        };
        let value = serde_json::to_value(ohos_facade_type_descriptor(&object).unwrap()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({ "kind": "named", "name": "Counter" })
        );
    }

    #[test]
    fn map_facade_type_fails_before_record_index_signature_reaches_arkts() {
        let error = ohos_facade_type_descriptor(&Type::Map {
            key_type: Box::new(Type::String),
            value_type: Box::new(Type::UInt32),
        })
        .err()
        .expect("Harmony map facade must be rejected")
        .to_string();
        assert!(error.contains("Record/index-signature"), "{error}");
    }

    #[test]
    fn named_record_and_enum_maps_fail_with_focused_paths() {
        let mut record_group = group("facade_graph");
        record_group.add_item(
            record(
                "facade_graph",
                "MapRecord",
                vec![field("values", map_type())],
            )
            .into(),
        );
        record_group.add_item(
            function(
                "record_events",
                vec![],
                Some(output_stream(
                    record_type("facade_graph", "MapRecord"),
                    Type::String,
                )),
            )
            .into(),
        );
        let record_ci = ComponentInterface::from_metadata(record_group).unwrap();
        let error = render_ohos_facade_contract(&record_ci)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("function `record_events` output item"),
            "{error}"
        );
        assert!(
            error.contains("record `facade_graph::MapRecord` field `values`"),
            "{error}"
        );
        assert!(error.contains("Record/index-signature"), "{error}");

        let mut enum_group = group("facade_graph");
        enum_group.add_item(
            EnumMetadata {
                module_path: "facade_graph".to_string(),
                name: "MapEvent".to_string(),
                orig_name: None,
                rust_path: None,
                shape: EnumShape::Enum,
                remote: false,
                variants: vec![VariantMetadata {
                    name: "Payload".to_string(),
                    orig_name: None,
                    discr: None,
                    fields: vec![field("values", map_type())],
                    docstring: None,
                }],
                discr_type: None,
                non_exhaustive: false,
                docstring: None,
            }
            .into(),
        );
        enum_group.add_item(
            function(
                "enum_events",
                vec![],
                Some(output_stream(
                    enum_type("facade_graph", "MapEvent"),
                    Type::String,
                )),
            )
            .into(),
        );
        let enum_ci = ComponentInterface::from_metadata(enum_group).unwrap();
        let error = render_ohos_facade_contract(&enum_ci)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("function `enum_events` output item"),
            "{error}"
        );
        assert!(
            error.contains("enum `facade_graph::MapEvent` variant `Payload` field `values`"),
            "{error}"
        );
    }

    #[test]
    fn input_item_error_other_argument_and_custom_maps_are_transitively_rejected() {
        let mut metadata = group("facade_graph");
        for name in ["InputMap", "ErrorMap", "ArgumentMap"] {
            metadata
                .add_item(record("facade_graph", name, vec![field("values", map_type())]).into());
        }
        metadata.add_item(
            function(
                "consume",
                vec![
                    FnParamMetadata::simple("source", input_stream(Type::UInt32, Type::String)),
                    FnParamMetadata::simple(
                        "options",
                        Type::Optional {
                            inner_type: Box::new(record_type("facade_graph", "ArgumentMap")),
                        },
                    ),
                ],
                None,
            )
            .into(),
        );
        let ci = ComponentInterface::from_metadata(metadata).unwrap();

        let error = validate_ohos_facade_type_graph(
            &ci,
            &input_stream(record_type("facade_graph", "InputMap"), Type::String),
            "input item fixture",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("input item fixture -> input item"),
            "{error}"
        );
        assert!(error.contains("InputMap"), "{error}");

        let error = validate_ohos_facade_type_graph(
            &ci,
            &input_stream(Type::UInt32, record_type("facade_graph", "ErrorMap")),
            "input error fixture",
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("input error fixture -> input error"),
            "{error}"
        );
        assert!(error.contains("ErrorMap"), "{error}");

        let error = render_ohos_facade_contract(&ci).unwrap_err().to_string();
        assert!(
            error.contains("function `consume` input-call argument `options`"),
            "{error}"
        );
        assert!(error.contains("ArgumentMap"), "{error}");

        let custom = Type::Custom {
            module_path: "facade_graph::types".to_string(),
            name: "MapAlias".to_string(),
            builtin: Box::new(Type::Optional {
                inner_type: Box::new(map_type()),
            }),
        };
        let error = validate_ohos_facade_type_graph(&ci, &custom, "custom fixture")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("custom `facade_graph::types::MapAlias` builtin"),
            "{error}"
        );
    }

    #[test]
    fn recursive_named_shapes_terminate_and_cross_component_names_keep_identity() {
        let mut recursive_group = group("facade_graph");
        recursive_group.add_item(
            record(
                "facade_graph",
                "Recursive",
                vec![field(
                    "next",
                    Type::Optional {
                        inner_type: Box::new(Type::Box {
                            inner_type: Box::new(record_type("facade_graph", "Recursive")),
                        }),
                    },
                )],
            )
            .into(),
        );
        recursive_group.add_item(
            function(
                "recursive_events",
                vec![],
                Some(output_stream(
                    record_type("facade_graph", "Recursive"),
                    Type::String,
                )),
            )
            .into(),
        );
        let recursive = ComponentInterface::from_metadata(recursive_group).unwrap();
        render_ohos_facade_contract(&recursive).unwrap();

        let mut external_group = group("external_component");
        external_group.add_item(
            record(
                "external_component::types",
                "Shared",
                vec![field("external_values", map_type())],
            )
            .into(),
        );
        let external = ComponentInterface::from_metadata(external_group).unwrap();

        let mut decoy_group = group("decoy_component");
        decoy_group.add_item(
            record(
                "decoy_component::types",
                "Shared",
                vec![field("id", Type::UInt32)],
            )
            .into(),
        );
        let decoy = ComponentInterface::from_metadata(decoy_group).unwrap();

        let mut local_group = group("facade_graph");
        local_group.add_item(
            function(
                "external_events",
                vec![],
                Some(output_stream(
                    record_type("external_component::types", "Shared"),
                    Type::String,
                )),
            )
            .into(),
        );
        let mut local = ComponentInterface::from_metadata(local_group).unwrap();
        local.set_all_component_interfaces(vec![decoy, external]);
        let error = render_ohos_facade_contract(&local).unwrap_err().to_string();
        assert!(
            error.contains("external_component::types::Shared"),
            "{error}"
        );
        assert!(error.contains("external_values"), "{error}");
        assert!(!error.contains("field `id`"), "{error}");
    }

    #[test]
    fn backend_adapter_targets_keep_hostile_user_identifiers_and_runtime_types_separate() {
        let mut metadata = group("unknown_chat");
        for name in ["napi_service", "napi_ohos_bridge", "runtime_unknown_value"] {
            metadata.add_item(function(name, Vec::new(), Some(Type::String)).into());
        }
        let ci = ComponentInterface::from_metadata(metadata).unwrap();

        let node = render_backend_adapter(
            &ci,
            BackendAdapterTarget::Node {
                default_addon_path: None,
            },
        );
        assert!(node.contains("import { createRequire } from \"node:module\";"));
        assert!(node.contains("process.env"));
        assert!(node.contains("./unknown_chat.node"));
        assert!(node.contains("Record<string, unknown>"));
        assert!(node.contains("__uniffiAbiVersion"));

        let ohos = render_ohos_backend_adapter(&ci);
        assert!(ohos.contains("import * as native from \"libunknown_chat_ohos.so\";"));
        assert!(ohos.contains("type UniffiValue = UniffiPrimitive | object;"));
        assert!(ohos.contains("__uniffiAbiVersion"));
        for node_loader_token in ["createRequire", "process.env", ".node"] {
            assert!(
                !ohos.contains(node_loader_token),
                "OHOS adapter unexpectedly contains `{node_loader_token}`:\n{ohos}"
            );
        }
        for node_dynamic_type in [": unknown", "Record<string, unknown>", ": any"] {
            assert!(
                !ohos.contains(node_dynamic_type),
                "OHOS adapter unexpectedly contains `{node_dynamic_type}`:\n{ohos}"
            );
        }

        // These are user-controlled IR names, not target/runtime vocabulary.
        for identifier in [
            "unknown_chat",
            "napi_service",
            "napi_ohos_bridge",
            "runtime_unknown_value",
        ] {
            assert!(
                node.contains(identifier),
                "missing from node adapter: {identifier}"
            );
            assert!(
                ohos.contains(identifier),
                "missing from OHOS adapter: {identifier}"
            );
        }
    }
}
