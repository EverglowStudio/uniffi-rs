/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Flavor-agnostic high-level TypeScript API emitter.
//!
//! Owns the per-component `common/*.ts` output described in
//! `docs/manual/src/javascript/contract.md`. This
//! pass walks the `ComponentInterface` directly and emits plain
//! TypeScript — no templates, no IR pipeline, no AbiFlavor awareness.
//!
//! Layering:
//!
//! - `shared/runtime.ts` is **copied verbatim** from
//!   `uniffi_runtime_javascript/typescript/src/runtime.ts`. Each component
//!   gets only a tiny `common/runtime.ts` wrapper that binds its namespace to
//!   the shared runtime's backend slot.
//! - `records.ts`, `enums.ts`, `errors.ts`, `callbacks.ts`, `objects.ts`,
//!   `api.ts` are generated from the CI, and all import their helpers
//!   from `./runtime`.
//!
//! Name conventions: snake_case functions/methods are surfaced as
//! camelCase to the TypeScript consumer, following normal TypeScript
//! expectations. Type names (records, enums, objects) stay in their
//! native PascalCase.

use std::collections::{BTreeSet, HashSet};

use anyhow::Result;
use camino::Utf8Path;
use fs_err as fs;
use heck::{ToSnakeCase, ToUpperCamelCase};
use uniffi_bindgen::{
    interface::{
        AsType, ComponentInterface, Constructor, Enum, Function, Method, Object, ObjectImpl,
        Record, TraitKind, Type,
    },
    Component,
};

use crate::{callback_metadata, crate_root, JsConfig};

/// The shared runtime module, shipped verbatim into every generated tree.
const RUNTIME_TS: &str =
    include_str!("../../../uniffi_runtime_javascript/typescript/src/runtime.ts");

#[cfg(test)]
mod runtime_abi_contract_tests {
    use super::RUNTIME_TS;
    use crate::JS_RUNTIME_ABI_VERSION;

    #[test]
    fn copied_runtime_uses_the_exact_rust_abi_constant_and_no_legacy_sentinel() {
        assert!(
            RUNTIME_TS.contains(&format!(
                "const JS_RUNTIME_ABI_VERSION = {JS_RUNTIME_ABI_VERSION};"
            )),
            "copied runtime ABI constant drifted from the Rust generator"
        );
        assert!(
            !RUNTIME_TS.contains("__uniffiAbiVersion")
                && !RUNTIME_TS.contains("__UNIFFI_JS_ABI_VERSION"),
            "copied runtime still accepts a legacy ABI sentinel"
        );
    }
}

/// Emit the one implementation copy shared by every generated component.
pub fn emit_shared_runtime(out_dir: &Utf8Path) -> Result<()> {
    let shared_dir = out_dir.join("shared");
    fs::create_dir_all(&shared_dir)?;
    write_generated(shared_dir.join("runtime.ts"), RUNTIME_TS.to_string())
}

pub fn emit(
    common_dir: &Utf8Path,
    component: &Component<JsConfig>,
    all_components: &[Component<JsConfig>],
) -> Result<()> {
    let context = RenderContext {
        component,
        all_components,
    };
    let ci = context.ci();

    write_generated(
        common_dir.join("runtime.ts"),
        render_component_runtime_wrapper(ci.namespace()),
    )?;
    write_generated(
        common_dir.join("custom-types.ts"),
        render_custom_types(&context),
    )?;
    write_generated(common_dir.join("records.ts"), render_records(&context))?;
    write_generated(common_dir.join("enums.ts"), render_enums(&context))?;
    write_generated(common_dir.join("errors.ts"), render_errors(&context))?;
    write_generated(common_dir.join("callbacks.ts"), render_callbacks(&context))?;
    write_generated(common_dir.join("objects.ts"), render_objects(&context))?;
    write_generated(common_dir.join("api.ts"), render_api(&context))?;
    write_generated(
        common_dir.join("public-types.ts"),
        render_public_types(&context),
    )?;
    Ok(())
}

/// Rendering context for one component.  `ComponentInterface` carries linked
/// interfaces but not their JavaScript configs, so retain the selected
/// component list as the authoritative owner/config lookup too.
struct RenderContext<'a> {
    component: &'a Component<JsConfig>,
    all_components: &'a [Component<JsConfig>],
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct ExternalModule {
    namespace: String,
    module: &'static str,
}

impl ExternalModule {
    fn alias(&self) -> String {
        format!(
            "__uniffi_{}_{}",
            self.namespace,
            self.module.replace('-', "_")
        )
    }

    fn specifier(&self) -> String {
        format!("../../{}/common/{}.ts", self.namespace, self.module)
    }
}

impl<'a> RenderContext<'a> {
    fn ci(&self) -> &'a ComponentInterface {
        &self.component.ci
    }

    fn config(&self) -> &'a JsConfig {
        &self.component.config
    }

    fn is_local_named_type(&self, ty: &Type) -> bool {
        if ty.module_path().is_none() {
            return true;
        }
        self.owner_component(ty).is_some_and(|owner| {
            owner.ci.namespace() == self.ci().namespace()
                && crate_root(owner.ci.crate_name()) == crate_root(self.ci().crate_name())
        })
    }

    fn owner_component(&self, ty: &Type) -> Option<&'a Component<JsConfig>> {
        let module_path = ty.module_path()?;
        let crate_name = crate_root(module_path);
        let mut owners = self
            .all_components
            .iter()
            .filter(|component| crate_root(component.ci.crate_name()) == crate_name);
        let owner = owners.next()?;
        owners.next().is_none().then_some(owner)
    }

    fn owner_ci(&self, ty: &Type) -> Option<&'a ComponentInterface> {
        self.owner_component(ty).map(|component| &component.ci)
    }

    fn owner_config(&self, ty: &Type) -> Option<&'a JsConfig> {
        self.owner_component(ty).map(|component| &component.config)
    }

    fn module_for_named_type(&self, ty: &Type) -> Option<&'static str> {
        let owner = self.owner_ci(ty)?;
        match ty {
            Type::Record { .. } => Some("records"),
            Type::Enum { name, .. } if owner.is_name_used_as_error(name) => Some("errors"),
            Type::Enum { .. } => Some("enums"),
            Type::Object { name, imp, .. }
                if matches!(
                    imp,
                    ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
                ) || owner
                    .get_object_definition(name)
                    .is_some_and(|object| object.has_callback_interface()) =>
            {
                Some("callbacks")
            }
            Type::Object { .. } => Some("objects"),
            Type::CallbackInterface { .. } => Some("callbacks"),
            Type::Custom { .. } => Some("custom-types"),
            _ => None,
        }
    }

    fn external_module_for(&self, ty: &Type) -> Option<ExternalModule> {
        if self.is_local_named_type(ty) {
            return None;
        }
        Some(ExternalModule {
            namespace: self.owner_component(ty)?.ci.namespace().to_string(),
            module: self.module_for_named_type(ty)?,
        })
    }

    fn named_type_reference(&self, ty: &Type) -> String {
        let Some(name) = ty.name() else {
            return "unknown".to_string();
        };
        self.external_module_for(ty)
            .map(|module| format!("{}.{}", module.alias(), name))
            .unwrap_or_else(|| name.to_string())
    }

    fn custom_config_for(&self, ty: &Type, name: &str) -> Option<&'a crate::CustomTypeConfig> {
        self.owner_config(ty)?.custom_type(name)
    }
}

fn render_component_runtime_wrapper(namespace: &str) -> String {
    format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript (component runtime wrapper).\n\
         // This file binds component namespace `{namespace}` to the single shared runtime.\n\
         // Do not edit by hand; regenerate via `uniffi-bindgen generate --language javascript`.\n\n\
         import {{ createComponentRuntime }} from \"../../../shared/runtime.ts\";\n\
         export {{\n\
             UniffiError,\n\
             serializeUniffiError,\n\
             createUniFfiStream,\n\
             inputStreamErrorPayload,\n\
             createUniffiInputStream,\n\
             nextUniffiInputStream,\n\
             cancelUniffiInputStream,\n\
             toI64,\n\
             toU64,\n\
             fromI64,\n\
             fromU64,\n\
             HandleMap,\n\
             UniffiObjectHandle,\n\
             registerCallback,\n\
             lookupCallback,\n\
             releaseCallback,\n\
         }} from \"../../../shared/runtime.ts\";\n\
         export type {{\n\
             UniffiErrorInit,\n\
             SerializedUniffiError,\n\
             UniFfiStream,\n\
             UniffiInputStreamErrorShape,\n\
             UniffiInputStreamOptions,\n\
             UniffiInputStreamNext,\n\
             UniffiInputStreamMarker,\n\
         }} from \"../../../shared/runtime.ts\";\n\n\
         const __uniffiComponentRuntime = createComponentRuntime(\"{namespace}\");\n\
         export const __installBackend = __uniffiComponentRuntime.__installBackend;\n\
         export const __call = __uniffiComponentRuntime.__call;\n\
         export const __callAsync = __uniffiComponentRuntime.__callAsync;\n"
    )
}

fn write_generated(path: impl AsRef<Utf8Path>, text: String) -> Result<()> {
    fs::write(path.as_ref(), normalize_generated_text(text))?;
    Ok(())
}

fn normalize_generated_text(mut text: String) -> String {
    while text.ends_with("\n\n") {
        text.pop();
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text
}

// -----------------------------------------------------------------------
// records.ts
// -----------------------------------------------------------------------

fn render_records(context: &RenderContext<'_>) -> String {
    let ci = context.ci();
    let mut out = header("records");
    let mut usage = Usage::default();
    let mut has_sync = false;
    let mut has_async = false;
    for record in ci.record_definitions() {
        for field in record.fields() {
            usage.see(context, &field.as_type(), UsagePos::TypeOnly);
        }
        for constructor in record.constructors() {
            if constructor.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for arg in constructor.arguments() {
                usage.see(context, &arg.as_type(), UsagePos::Arg);
            }
            usage.see(context, &record.as_type(), UsagePos::Ret);
        }
        for method in record.methods() {
            if method.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            usage.see(context, &record.as_type(), UsagePos::Arg);
            for arg in method.arguments() {
                usage.see(context, &arg.as_type(), UsagePos::Arg);
            }
            if let Some(ret) = method.return_type() {
                usage.see(context, ret, UsagePos::Ret);
            }
        }
    }
    emit_value_module_imports(&mut out, context, &usage, has_sync, has_async, "records");
    if !usage.customs.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./custom-types.ts\";\n\n",
            join_sorted(&usage.customs.iter().cloned().collect::<Vec<_>>())
        ));
    }
    for record in ci.record_definitions() {
        out.push_str(&render_record(context, record));
        out.push('\n');
    }
    finish_module(out)
}

fn render_record(context: &RenderContext<'_>, record: &Record) -> String {
    let mut s = format!("export interface {} {{\n", record.name());
    for f in record.fields() {
        s.push_str(&format!(
            "    {}: {};\n",
            js_field_name(f.name()),
            ts_type(context, &f.as_type())
        ));
    }
    s.push_str("}\n");
    if !record.constructors().is_empty() || !record.methods().is_empty() {
        s.push_str(&format!(
            "\nexport const {} = Object.freeze({{\n",
            record.name()
        ));
        for constructor in record.constructors() {
            s.push_str(&render_value_constructor(
                context,
                record.name(),
                &record.as_type(),
                constructor,
            ));
        }
        for method in record.methods() {
            s.push_str(&render_value_method(
                context,
                record.name(),
                &record.as_type(),
                method,
            ));
        }
        s.push_str("});\n");
    }
    s
}

// -----------------------------------------------------------------------
// enums.ts
// -----------------------------------------------------------------------

fn render_enums(context: &RenderContext<'_>) -> String {
    let ci = context.ci();
    let mut out = header("enums");
    let mut usage = Usage::default();
    let mut has_sync = false;
    let mut has_async = false;
    for enum_ in ci
        .enum_definitions()
        .iter()
        .filter(|e| !ci.is_name_used_as_error(e.name()))
    {
        for variant in enum_.variants() {
            for field in variant.fields() {
                usage.see(context, &field.as_type(), UsagePos::TypeOnly);
            }
        }
        for constructor in enum_.constructors() {
            if constructor.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for arg in constructor.arguments() {
                usage.see(context, &arg.as_type(), UsagePos::Arg);
            }
            usage.see(context, &enum_.as_type(), UsagePos::Ret);
        }
        for method in enum_.methods() {
            if method.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            usage.see(context, &enum_.as_type(), UsagePos::Arg);
            for arg in method.arguments() {
                usage.see(context, &arg.as_type(), UsagePos::Arg);
            }
            if let Some(ret) = method.return_type() {
                usage.see(context, ret, UsagePos::Ret);
            }
        }
    }
    emit_value_module_imports(&mut out, context, &usage, has_sync, has_async, "enums");
    if !usage.customs.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./custom-types.ts\";\n\n",
            join_sorted(&usage.customs.iter().cloned().collect::<Vec<_>>())
        ));
    }
    for e in ci.enum_definitions() {
        if ci.is_name_used_as_error(e.name()) {
            // Error enums are emitted as classes in errors.ts instead.
            continue;
        }
        out.push_str(&render_enum(context, e));
        out.push('\n');
    }
    finish_module(out)
}

// -----------------------------------------------------------------------
// custom-types.ts
// -----------------------------------------------------------------------

fn render_custom_types(context: &RenderContext<'_>) -> String {
    let config = context.config();
    use std::fmt::Write;

    let mut out = header("custom-types");
    let customs = all_custom_types(context);
    let mut usage = Usage::default();
    for (_, builtin) in &customs {
        usage.see(context, builtin, UsagePos::TypeOnly);
    }

    let mut emitted_imports = HashSet::new();
    for (name, _) in &customs {
        let Some(custom) = config.custom_type(name) else {
            continue;
        };
        for import in &custom.imports {
            let line = render_import_statement(import);
            if emitted_imports.insert(line.clone()) {
                writeln!(out, "{line}").unwrap();
            }
        }
    }
    let has_custom_imports = !emitted_imports.is_empty();
    let has_external_imports = emit_external_module_imports(&mut out, &usage);
    if has_custom_imports || has_external_imports {
        out.push('\n');
    }

    for (name, builtin) in customs {
        let builtin_ts = ts_type(context, &builtin);
        let custom_cfg = config.custom_type(&name);
        let public_ty = custom_cfg
            .map(|cfg| cfg.public_type(&builtin_ts))
            .unwrap_or(builtin_ts.as_str());
        writeln!(out, "export type {name} = {public_ty};").unwrap();
        let lower_expr = custom_cfg
            .map(|cfg| cfg.from_custom_expr("value"))
            .unwrap_or_else(|| "value".to_string());
        let lift_expr = custom_cfg
            .map(|cfg| cfg.into_custom_expr("value"))
            .unwrap_or_else(|| "value".to_string());
        writeln!(
            out,
            "export function __uniffiLowerCustom{name}(value: {name}): {builtin_ts} {{\n    return {lower_expr} as {builtin_ts};\n}}\n"
        )
        .unwrap();
        writeln!(
            out,
            "export function __uniffiLiftCustom{name}(value: {builtin_ts}): {name} {{\n    return {lift_expr} as {name};\n}}\n"
        )
        .unwrap();
    }
    finish_module(out)
}

fn render_enum(context: &RenderContext<'_>, e: &Enum) -> String {
    let all_unit = e.variants().iter().all(|v| v.fields().is_empty());
    if all_unit {
        // Emit a const object + string-literal union instead of a TS
        // `enum`, so the output is compatible with Node's strip-types
        // mode (which forbids `enum`). `Color.Red` still works; the
        // type `Color` widens to `"Red" | "Green" | "Blue"`.
        let mut s = format!("export const {name} = {{\n", name = e.name());
        for v in e.variants() {
            s.push_str(&format!("    {name}: \"{name}\",\n", name = v.name()));
        }
        for constructor in e.constructors() {
            s.push_str(&render_value_constructor(
                context,
                e.name(),
                &e.as_type(),
                constructor,
            ));
        }
        for method in e.methods() {
            s.push_str(&render_value_method(
                context,
                e.name(),
                &e.as_type(),
                method,
            ));
        }
        s.push_str("} as const;\n");
        let variants = e
            .variants()
            .iter()
            .map(|v| {
                format!(
                    "typeof {name}.{variant}",
                    name = e.name(),
                    variant = v.name()
                )
            })
            .collect::<Vec<_>>()
            .join(" | ");
        s.push_str(&format!(
            "export type {name} = {variants};\n",
            name = e.name()
        ));
        return s;
    }
    let mut arms = Vec::new();
    for v in e.variants() {
        if v.fields().is_empty() {
            arms.push(format!("  | {{ tag: \"{}\" }}", v.name()));
        } else {
            let fields = v
                .fields()
                .iter()
                .map(|f| {
                    format!(
                        "{}: {}",
                        js_field_name(f.name()),
                        ts_type(context, &f.as_type())
                    )
                })
                .collect::<Vec<_>>()
                .join("; ");
            arms.push(format!("  | {{ tag: \"{}\"; {} }}", v.name(), fields));
        }
    }
    let mut s = format!("export type {} =\n{};\n", e.name(), arms.join("\n"));
    if !e.constructors().is_empty() || !e.methods().is_empty() {
        s.push_str(&format!("\nexport const {} = Object.freeze({{\n", e.name()));
        for constructor in e.constructors() {
            s.push_str(&render_value_constructor(
                context,
                e.name(),
                &e.as_type(),
                constructor,
            ));
        }
        for method in e.methods() {
            s.push_str(&render_value_method(
                context,
                e.name(),
                &e.as_type(),
                method,
            ));
        }
        s.push_str("});\n");
    }
    s
}

fn emit_value_module_imports(
    out: &mut String,
    context: &RenderContext<'_>,
    usage: &Usage,
    has_sync: bool,
    has_async: bool,
    local_module: &str,
) {
    let ci = context.ci();
    let mut runtime = Vec::new();
    if has_sync {
        runtime.push("__call");
    }
    if has_async {
        runtime.push("__callAsync");
    }
    if usage.needs_to_i64 {
        runtime.push("toI64");
    }
    if usage.needs_to_u64 {
        runtime.push("toU64");
    }
    if usage.needs_input_stream {
        runtime.push("createUniffiInputStream");
    }
    if !runtime.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./runtime.ts\";\n",
            runtime.join(", ")
        ));
    }

    let grouped = group_named_types(ci, &usage.named);
    if local_module != "records" && !grouped.records.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./records.ts\";\n",
            join_sorted(&grouped.records)
        ));
    }
    if local_module != "enums" && !grouped.enums.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./enums.ts\";\n",
            join_sorted(&grouped.enums)
        ));
    }
    if !grouped.errors.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./errors.ts\";\n",
            join_sorted(&grouped.errors)
        ));
    }
    if !grouped.callbacks.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./callbacks.ts\";\n",
            join_sorted(&grouped.callbacks)
        ));
    }
    let callback_helpers = &usage.callback_lower_helpers;
    if !callback_helpers.is_empty() {
        let helpers = callback_helpers
            .iter()
            .map(|name| format!("__uniffiLowerCallback{name}"))
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "import {{ {} }} from \"./callbacks.ts\";\n",
            join_sorted(&helpers)
        ));
    }

    let object_values = usage
        .objects_in_ret
        .iter()
        .filter(|name| grouped.objects.contains(*name))
        .cloned()
        .collect::<BTreeSet<_>>();
    let object_types = grouped
        .objects
        .iter()
        .filter(|name| !object_values.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !object_values.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&object_values.iter().cloned().collect::<Vec<_>>())
        ));
    }
    if !object_types.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&object_types)
        ));
    }

    let helper_customs = custom_helper_imports(usage);
    if !helper_customs.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helper_customs.iter().cloned().collect::<Vec<_>>())
        ));
    }

    let has_external_imports = emit_external_module_imports(out, usage);

    if has_sync
        || has_async
        || !usage.named.is_empty()
        || !callback_helpers.is_empty()
        || !helper_customs.is_empty()
        || has_external_imports
    {
        out.push('\n');
    }
}

fn emit_external_module_imports(out: &mut String, usage: &Usage) -> bool {
    if usage.external_modules.is_empty() {
        return false;
    }
    for module in &usage.external_modules {
        out.push_str(&format!(
            "import * as {} from \"{}\";\n",
            module.alias(),
            module.specifier()
        ));
    }
    true
}

fn render_value_method(
    context: &RenderContext<'_>,
    owner_name: &str,
    owner_ty: &Type,
    method: &Method,
) -> String {
    let js_name = js_fn_name(method.name());
    let fn_name = crate::dispatch_key::method_key(owner_name, method);
    let (arg_decls, arg_pass) = lowered_args(
        context,
        method.arguments().iter().map(|a| (a.name(), a.as_type())),
    );
    let self_pass = ts_lower_expr(context, owner_ty, "self_", 0);
    let pass = if arg_pass.is_empty() {
        self_pass
    } else {
        format!("{self_pass}, {arg_pass}")
    };
    let decls = if arg_decls.is_empty() {
        format!("self_: {}", ts_type(context, owner_ty))
    } else {
        format!("self_: {}, {arg_decls}", ts_type(context, owner_ty))
    };
    let ret_ty = method.return_type();
    let ret_ts = ret_ty
        .map(|t| ts_type(context, t))
        .unwrap_or_else(|| "void".to_string());
    let call_g = call_generic(ret_ty);
    if method.is_async() {
        if let Some(ret_ty) = ret_ty {
            format!(
                "    async {js_name}({decls}): Promise<{ret_ts}> {{\n        \
                 const __ret = await __callAsync<{call_g}>(\"{fn_name}\", {pass});\n        \
                 return {lift} as {ret_ts};\n    }},\n",
                lift = ts_lift_expr(context, ret_ty, "__ret", 0),
            )
        } else {
            format!(
                "    async {js_name}({decls}): Promise<void> {{\n        \
                 await __callAsync<void>(\"{fn_name}\", {pass});\n    }},\n"
            )
        }
    } else if let Some(ret_ty) = ret_ty {
        format!(
            "    {js_name}({decls}): {ret_ts} {{\n        \
             const __ret = __call<{call_g}>(\"{fn_name}\", {pass});\n        \
             return {lift} as {ret_ts};\n    }},\n",
            lift = ts_lift_expr(context, ret_ty, "__ret", 0),
        )
    } else {
        format!(
            "    {js_name}({decls}): void {{\n        \
             __call<void>(\"{fn_name}\", {pass});\n    }},\n"
        )
    }
}

fn render_value_constructor(
    context: &RenderContext<'_>,
    owner_name: &str,
    owner_ty: &Type,
    constructor: &Constructor,
) -> String {
    let js_name = js_fn_name(constructor.name());
    let fn_name = crate::dispatch_key::constructor_key(owner_name, constructor);
    let (arg_decls, arg_pass) = lowered_args(
        context,
        constructor
            .arguments()
            .iter()
            .map(|a| (a.name(), a.as_type())),
    );
    let ret_ts = ts_type(context, owner_ty);
    let call_g = call_generic(Some(owner_ty));
    let sep = if arg_pass.is_empty() { "" } else { ", " };
    if constructor.is_async() {
        format!(
            "    async {js_name}({arg_decls}): Promise<{ret_ts}> {{\n        \
             const __ret = await __callAsync<{call_g}>(\"{fn_name}\"{sep}{arg_pass});\n        \
             return {lift} as {ret_ts};\n    }},\n",
            lift = ts_lift_expr(context, owner_ty, "__ret", 0),
        )
    } else {
        format!(
            "    {js_name}({arg_decls}): {ret_ts} {{\n        \
             const __ret = __call<{call_g}>(\"{fn_name}\"{sep}{arg_pass});\n        \
             return {lift} as {ret_ts};\n    }},\n",
            lift = ts_lift_expr(context, owner_ty, "__ret", 0),
        )
    }
}

// -----------------------------------------------------------------------
// errors.ts
// -----------------------------------------------------------------------

fn render_errors(context: &RenderContext<'_>) -> String {
    let ci = context.ci();
    let mut out = header("errors");
    let errors = ci
        .enum_definitions()
        .iter()
        .filter(|error| ci.is_name_used_as_error(error.name()))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return finish_module(out);
    }

    out.push_str("import { UniffiError } from \"./runtime.ts\";\n\n");
    for e in errors {
        out.push_str(&format!(
            "export class {name} extends UniffiError {{\n    \
             constructor(message: string, variant: string | null = null, data: unknown = null) {{\n        \
             super({{ errorName: \"{name}\", variant, data, message }});\n    \
             }}\n}}\n\n",
            name = e.name()
        ));
    }
    finish_module(out)
}

// -----------------------------------------------------------------------
// callbacks.ts
// -----------------------------------------------------------------------

fn render_callbacks(context: &RenderContext<'_>) -> String {
    let ci = context.ci();
    let mut out = header("callbacks");
    let mut usage = Usage::default();
    let mut needs_callback_return_unwrap = false;
    for obj in ci.object_definitions().iter().filter(|obj| {
        matches!(
            obj.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        )
    }) {
        for method in obj.methods() {
            for arg in method.arguments() {
                // Native callback arguments are lifted before they reach the
                // JavaScript implementation; its result is lowered on the
                // way back to Rust.
                usage.see(context, &arg.as_type(), UsagePos::Ret);
            }
            if let Some(ret) = method.return_type() {
                needs_callback_return_unwrap |= needs_object_callback_return_unwrap(ret);
                usage.see(context, ret, UsagePos::Arg);
            }
        }
    }
    for callback in ci.callback_interface_definitions() {
        for method in callback.methods() {
            for arg in method.arguments() {
                usage.see(context, &arg.as_type(), UsagePos::Ret);
            }
            if let Some(ret) = method.return_type() {
                needs_callback_return_unwrap |= needs_object_callback_return_unwrap(ret);
                usage.see(context, ret, UsagePos::Arg);
            }
        }
    }

    let grouped = group_named_types(ci, &usage.named);
    if !grouped.records.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./records.ts\";\n",
            join_sorted(&grouped.records)
        ));
    }
    if !grouped.enums.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./enums.ts\";\n",
            join_sorted(&grouped.enums)
        ));
    }
    if !grouped.errors.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./errors.ts\";\n",
            join_sorted(&grouped.errors)
        ));
    }
    let object_values = usage
        .objects_in_ret
        .iter()
        .filter(|name| grouped.objects.contains(*name))
        .cloned()
        .collect::<BTreeSet<_>>();
    let object_types = grouped
        .objects
        .iter()
        .filter(|name| !object_values.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    if !object_values.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&object_values.iter().cloned().collect::<Vec<_>>())
        ));
    }
    if !object_types.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&object_types)
        ));
    }
    let custom_type_imports = usage.customs.iter().cloned().collect::<Vec<_>>();
    if !custom_type_imports.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./custom-types.ts\";\n\n",
            join_sorted(&custom_type_imports)
        ));
    }
    let helper_customs = custom_helper_imports(&usage);
    if !helper_customs.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helper_customs.iter().cloned().collect::<Vec<_>>())
        ));
    }
    let has_external_imports = emit_external_module_imports(&mut out, &usage);
    if !grouped.records.is_empty()
        || !grouped.enums.is_empty()
        || !grouped.errors.is_empty()
        || !object_values.is_empty()
        || !object_types.is_empty()
        || !custom_type_imports.is_empty()
        || !helper_customs.is_empty()
        || has_external_imports
    {
        out.push('\n');
    }
    if needs_callback_return_unwrap {
        out.push_str(
            "function __uniffiUnwrapCallbackReturn(value: unknown): unknown {\n    let current = value;\n    while (current !== null && typeof current === \"object\") {\n        const uniffi = (current as Record<string, unknown>).__uniffi;\n        if (uniffi === null || typeof uniffi !== \"object\") break;\n        const raw = (uniffi as Record<string, unknown>).raw;\n        if (raw === current) break;\n        current = raw;\n    }\n    return current;\n}\n\n",
        );
    }

    let mut rendered = BTreeSet::new();
    for callback in ci.callback_interface_definitions() {
        let methods = callback.methods();
        rendered.insert(callback.name().to_string());
        out.push_str(&render_callback_definition(
            context,
            callback.name(),
            &methods,
        ));
    }
    for obj in ci.object_definitions() {
        if !matches!(
            obj.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            continue;
        }
        if !rendered.insert(obj.name().to_string()) {
            continue;
        }
        let methods = obj.methods();
        out.push_str(&render_callback_definition(context, obj.name(), &methods));
    }
    finish_module(out)
}

fn needs_object_callback_return_unwrap(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Object {
            imp: ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly),
            ..
        }
    )
}

fn render_callback_definition(
    context: &RenderContext<'_>,
    name: &str,
    methods: &[&Method],
) -> String {
    let mut out = format!("export interface {name} {{\n");
    for m in methods {
        let args = m
            .arguments()
            .iter()
            .map(|a| {
                format!(
                    "{}: {}",
                    js_field_name(a.name()),
                    ts_type(context, &a.as_type())
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let ret = match m.return_type() {
            Some(t) => ts_type(context, t),
            None => "void".to_string(),
        };
        let ret = if m.is_async() {
            format!("{ret} | Promise<{ret}>")
        } else {
            ret
        };
        out.push_str(&format!(
            "    {}({}): {};\n",
            js_fn_name(m.name()),
            args,
            ret
        ));
    }
    out.push_str("}\n\n");
    out.push_str(&render_callback_lowerer(context, name, methods));
    out
}

fn render_callback_lowerer(context: &RenderContext<'_>, name: &str, methods: &[&Method]) -> String {
    let mut out = format!(
        "export function __uniffiLowerCallback{name}(__uniffiCallbackObject: {name}): Record<string, unknown> {{\n    return {{\n"
    );
    for method in methods {
        let method_name = js_fn_name(method.name());
        let args = method
            .arguments()
            .iter()
            .map(|arg| format!("{}: any", js_field_name(arg.name())))
            .collect::<Vec<_>>()
            .join(", ");
        let pass = method
            .arguments()
            .iter()
            .map(|arg| {
                let js = js_field_name(arg.name());
                ts_lift_expr(context, &arg.as_type(), &js, 0)
            })
            .collect::<Vec<_>>()
            .join(", ");
        if method.is_async() {
            if let Some(ret) = method.return_type() {
                let lower = match ret {
                    Type::Object {
                        imp: ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly),
                        ..
                    } => "__uniffiUnwrapCallbackReturn(__ret)".to_string(),
                    _ => ts_lower_expr(context, ret, "__ret", 0),
                };
                out.push_str(&format!(
                    "        {method_name}({args}): Promise<any> {{\n            return Promise.resolve(__uniffiCallbackObject.{method_name}({pass})).then((__ret) => {{ return {lower}; }});\n        }},\n"
                ));
            } else {
                out.push_str(&format!(
                    "        {method_name}({args}): Promise<void> {{\n            return Promise.resolve(__uniffiCallbackObject.{method_name}({pass})).then(() => undefined);\n        }},\n"
                ));
            }
        } else if let Some(ret) = method.return_type() {
            let lower = match ret {
                Type::Object {
                    imp: ObjectImpl::Struct | ObjectImpl::Trait(TraitKind::RustOnly),
                    ..
                } => "__uniffiUnwrapCallbackReturn(__ret)".to_string(),
                _ => ts_lower_expr(context, ret, "__ret", 0),
            };
            out.push_str(&format!(
                "        {method_name}({args}): any {{\n            const __ret = __uniffiCallbackObject.{method_name}({pass});\n            return {lower};\n        }},\n"
            ));
        } else {
            out.push_str(&format!(
                "        {method_name}({args}): void {{\n            __uniffiCallbackObject.{method_name}({pass});\n        }},\n"
            ));
        }
    }
    out.push_str("    };\n}\n\n");
    out
}

// -----------------------------------------------------------------------
// objects.ts
// -----------------------------------------------------------------------

fn render_objects(context: &RenderContext<'_>) -> String {
    let ci = context.ci();
    let mut out = header("objects");

    // Collect every type referenced by constructors/methods so we can
    // decide (a) which runtime helpers to import and (b) which named
    // types to bring into local scope explicitly.
    let mut usage = Usage::default();
    let mut any_object = false;
    let mut has_async = false;
    let mut has_sync = false;
    for obj in ci.object_definitions() {
        if matches!(
            obj.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            continue;
        }
        any_object = true;
        for c in obj.constructors() {
            if c.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for a in c.arguments() {
                usage.see(context, &a.as_type(), UsagePos::Arg);
            }
        }
        for m in obj.methods() {
            if m.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for a in m.arguments() {
                usage.see(context, &a.as_type(), UsagePos::Arg);
            }
            if let Some(t) = m.return_type() {
                usage.see(context, t, UsagePos::Ret);
            }
        }
    }

    if !any_object {
        return finish_module(out);
    }

    // Runtime import line — only the helpers actually needed.
    let mut runtime = vec!["UniffiObjectHandle"];
    if has_sync {
        runtime.push("__call");
    }
    if has_async {
        runtime.push("__callAsync");
    }
    if usage.needs_to_i64 {
        runtime.push("toI64");
    }
    if usage.needs_to_u64 {
        runtime.push("toU64");
    }
    if usage.needs_from_i64 {
        runtime.push("fromI64");
    }
    if usage.needs_from_u64 {
        runtime.push("fromU64");
    }
    if usage.needs_input_stream {
        runtime.push("createUniffiInputStream");
    }
    out.push_str(&format!(
        "import {{ {} }} from \"./runtime.ts\";\n",
        runtime.join(", ")
    ));

    // Explicit local imports for type names referenced in signatures
    // (avoids relying on any ambient DOM globals like `Event`).
    let grouped = group_named_types(ci, &usage.named);
    if !grouped.records.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./records.ts\";\n",
            join_sorted(&grouped.records)
        ));
    }
    if !grouped.enums.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./enums.ts\";\n",
            join_sorted(&grouped.enums)
        ));
    }
    if !grouped.errors.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./errors.ts\";\n",
            join_sorted(&grouped.errors)
        ));
    }
    if !grouped.callbacks.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./callbacks.ts\";\n",
            join_sorted(&grouped.callbacks)
        ));
    }
    let callback_helpers = &usage.callback_lower_helpers;
    if !callback_helpers.is_empty() {
        let helpers = callback_helpers
            .iter()
            .map(|name| format!("__uniffiLowerCallback{name}"))
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "import {{ {} }} from \"./callbacks.ts\";\n",
            join_sorted(&helpers)
        ));
    }
    let custom_type_imports = usage.customs.iter().cloned().collect::<Vec<_>>();
    if !custom_type_imports.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&custom_type_imports)
        ));
    }
    let helper_customs = custom_helper_imports(&usage);
    if !helper_customs.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helper_customs.iter().cloned().collect::<Vec<_>>())
        ));
    }
    emit_external_module_imports(&mut out, &usage);
    out.push('\n');

    for obj in ci.object_definitions() {
        if matches!(
            obj.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            continue;
        }
        out.push_str(&render_object_class(context, obj));
        out.push('\n');
    }
    finish_module(out)
}

fn render_object_class(context: &RenderContext<'_>, obj: &Object) -> String {
    let name = obj.name();
    let snake = obj.name().to_snake_case();
    let drop_fn = format!("__uniffi_{snake}_object_free");
    let mut s = format!(
        "export class {name} {{\n    \
         readonly __uniffi: UniffiObjectHandle;\n    \
         private constructor(handle: unknown) {{\n        \
         this.__uniffi = new UniffiObjectHandle(handle, (h) => __call<void>(\"{drop_fn}\", h));\n    \
         }}\n\n    \
         /** Internal: wrap a raw handle returned by a Rust free function. */\n    \
         static __fromHandle(handle: unknown): {name} {{\n        \
         return new {name}(handle);\n    \
         }}\n\n"
    );
    for c in obj.constructors() {
        let c_js = js_fn_name(c.name());
        let fn_name = crate::dispatch_key::constructor_key(obj.name(), c);
        let (arg_decls, arg_pass) = lowered_args(
            context,
            c.arguments().iter().map(|a| (a.name(), a.as_type())),
        );
        let comma = if arg_pass.is_empty() { "" } else { ", " };
        if c.is_async() {
            s.push_str(&format!(
                "    static async {c_js}({arg_decls}): Promise<{name}> {{\n        \
                 const handle = await __callAsync<unknown>(\"{fn_name}\"{comma}{arg_pass});\n        \
                 return new {name}(handle);\n    }}\n",
            ));
        } else {
            s.push_str(&format!(
                "    static {c_js}({arg_decls}): {name} {{\n        \
                 const handle = __call<unknown>(\"{fn_name}\"{comma}{arg_pass});\n        \
                 return new {name}(handle);\n    }}\n",
            ));
        }
    }
    for m in obj.methods() {
        let m_js = js_fn_name(m.name());
        let fn_name = crate::dispatch_key::method_key(obj.name(), m);
        let (arg_decls, arg_pass) = lowered_args(
            context,
            m.arguments().iter().map(|a| (a.name(), a.as_type())),
        );
        let comma_pass = if arg_pass.is_empty() { "" } else { ", " };
        let ret_ty = m.return_type();
        let ret_ts = match ret_ty {
            Some(t) => ts_type(context, t),
            None => "void".to_string(),
        };
        let call_g = call_generic(ret_ty);
        if m.is_async() {
            if let Some(ret_ty) = ret_ty {
                s.push_str(&format!(
                    "    async {m_js}({arg_decls}): Promise<{ret_ts}> {{\n        \
                     const __ret = await __callAsync<{call_g}>(\"{fn_name}\", this.__uniffi.raw{comma_pass}{arg_pass});\n        \
                     return {lift} as {ret_ts};\n    }}\n",
                    lift = ts_lift_expr(context, ret_ty, "__ret", 0),
                ));
            } else {
                s.push_str(&format!(
                    "    async {m_js}({arg_decls}): Promise<void> {{\n        \
                     await __callAsync<void>(\"{fn_name}\", this.__uniffi.raw{comma_pass}{arg_pass});\n    }}\n"
                ));
            }
        } else {
            if let Some(ret_ty) = ret_ty {
                s.push_str(&format!(
                    "    {m_js}({arg_decls}): {ret_ts} {{\n        \
                     const __ret = __call<{call_g}>(\"{fn_name}\", this.__uniffi.raw{comma_pass}{arg_pass});\n        \
                     return {lift} as {ret_ts};\n    }}\n",
                    lift = ts_lift_expr(context, ret_ty, "__ret", 0),
                ));
            } else {
                s.push_str(&format!(
                    "    {m_js}({arg_decls}): void {{\n        \
                     __call<void>(\"{fn_name}\", this.__uniffi.raw{comma_pass}{arg_pass});\n    }}\n"
                ));
            }
        }
    }
    s.push_str(
        "    dispose(): void {\n        \
         this.__uniffi.dispose();\n    }\n",
    );
    s.push_str("}\n");
    s
}

// -----------------------------------------------------------------------
// api.ts — entry point re-exporting everything the app sees
// -----------------------------------------------------------------------

fn render_api(context: &RenderContext<'_>) -> String {
    let ci = context.ci();
    let mut out = header("api");
    out.push_str(
        "export * from \"./records.ts\";\n\
         export * from \"./enums.ts\";\n\
         export * from \"./errors.ts\";\n\
         export * from \"./callbacks.ts\";\n\
         export * from \"./objects.ts\";\n",
    );
    let customs = all_custom_types(context)
        .into_iter()
        .map(|(name, _)| name)
        .collect::<Vec<_>>();
    if !customs.is_empty() {
        out.push_str(&format!(
            "export type {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&customs)
        ));
    }
    out.push_str(
        "export { UniffiError } from \"./runtime.ts\";\n\
         export type { UniFfiStream } from \"./runtime.ts\";\n\n",
    );

    let mut usage = Usage::default();
    let mut has_async = false;
    let mut has_sync = false;
    for f in ci.function_definitions() {
        if matches!(f.return_type(), Some(Type::Stream { .. })) {
            // Stream start/cancel are synchronous, but next is async.
            has_sync = true;
            has_async = true;
            usage.needs_stream = true;
        } else if f.is_async() {
            has_async = true;
        } else {
            has_sync = true;
        }
        for a in f.arguments() {
            usage.see(context, &a.as_type(), UsagePos::Arg);
        }
        if let Some(t) = f.return_type() {
            usage.see(context, t, UsagePos::Ret);
        }
    }

    let mut runtime = Vec::new();
    if has_sync {
        runtime.push("__call");
    }
    if has_async {
        runtime.push("__callAsync");
    }
    if usage.needs_to_i64 {
        runtime.push("toI64");
    }
    if usage.needs_to_u64 {
        runtime.push("toU64");
    }
    if usage.needs_from_i64 {
        runtime.push("fromI64");
    }
    if usage.needs_from_u64 {
        runtime.push("fromU64");
    }
    if usage.needs_stream {
        runtime.push("createUniFfiStream");
        runtime.push("UniffiError");
    }
    if usage.needs_input_stream {
        runtime.push("createUniffiInputStream");
    }
    if !runtime.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./runtime.ts\";\n",
            runtime.join(", ")
        ));
    }
    // A type re-export does not create a local binding that the generated
    // stream function signatures can use. Keep the public re-export above,
    // but only import the local type when this component actually emits an
    // output stream so strict/noUnusedLocals builds remain clean.
    if usage.needs_stream {
        out.push_str("import type { UniFfiStream } from \"./runtime.ts\";\n");
    }

    // Explicit named imports. Strict TS refuses unused imports, and
    // relying on `export *` would leave names like `Event` silently
    // binding to the DOM global.
    let grouped = group_named_types(ci, &usage.named);
    let obj_value_needed: BTreeSet<String> = usage
        .objects_in_ret
        .iter()
        .filter(|n| grouped.objects.contains(*n))
        .cloned()
        .collect();
    let obj_type_only: Vec<String> = grouped
        .objects
        .iter()
        .filter(|n| !obj_value_needed.contains(*n))
        .cloned()
        .collect();
    if !obj_value_needed.is_empty() {
        // Values — needed for `{Name}.__fromHandle(...)` at runtime.
        let v: Vec<String> = obj_value_needed.into_iter().collect();
        out.push_str(&format!(
            "import {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&v)
        ));
    }
    if !obj_type_only.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&obj_type_only)
        ));
    }
    if !grouped.records.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./records.ts\";\n",
            join_sorted(&grouped.records)
        ));
    }
    if !grouped.enums.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./enums.ts\";\n",
            join_sorted(&grouped.enums)
        ));
    }
    let error_value_needed: BTreeSet<String> = usage
        .stream_error_values
        .iter()
        .filter(|name| grouped.errors.contains(*name))
        .cloned()
        .collect();
    let error_type_only: Vec<String> = grouped
        .errors
        .iter()
        .filter(|name| !error_value_needed.contains(*name))
        .cloned()
        .collect();
    if !error_value_needed.is_empty() {
        let values: Vec<String> = error_value_needed.into_iter().collect();
        out.push_str(&format!(
            "import {{ {} }} from \"./errors.ts\";\n",
            join_sorted(&values)
        ));
    }
    if !error_type_only.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./errors.ts\";\n",
            join_sorted(&error_type_only)
        ));
    }
    if !grouped.callbacks.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./callbacks.ts\";\n",
            join_sorted(&grouped.callbacks)
        ));
    }
    let callback_helpers = &usage.callback_lower_helpers;
    if !callback_helpers.is_empty() {
        let helpers = callback_helpers
            .iter()
            .map(|name| format!("__uniffiLowerCallback{name}"))
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "import {{ {} }} from \"./callbacks.ts\";\n",
            join_sorted(&helpers)
        ));
    }
    let custom_type_imports = usage.customs.iter().cloned().collect::<Vec<_>>();
    if !custom_type_imports.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&custom_type_imports)
        ));
    }
    let helper_customs = custom_helper_imports(&usage);
    if !helper_customs.is_empty() {
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helper_customs.iter().cloned().collect::<Vec<_>>())
        ));
    }
    emit_external_module_imports(&mut out, &usage);
    out.push('\n');
    for f in ci.function_definitions() {
        out.push_str(&render_free_function(context, &f));
        out.push('\n');
    }
    finish_module(out)
}

fn render_free_function(context: &RenderContext<'_>, f: &Function) -> String {
    let (arg_decls, arg_pass) = lowered_args(
        context,
        f.arguments().iter().map(|a| (a.name(), a.as_type())),
    );
    let ret_ty = f.return_type();
    let ret_ts = match ret_ty {
        Some(t) => ts_type(context, t),
        None => "void".to_string(),
    };
    let call_g = call_generic(ret_ty);
    let rust_name = crate::dispatch_key::free_function_key(f.name());
    let js_name = js_fn_name(f.name());
    if let Some(Type::Stream { item_type, .. }) = ret_ty {
        let next_name = crate::dispatch_key::stream_next_key(f.name());
        let cancel_name = crate::dispatch_key::stream_cancel_key(f.name());
        let item_ts = ts_type(context, item_type);
        let item_lift = ts_lift_expr(context, item_type, "__next.value", 0);
        let error_type = match ret_ty {
            Some(Type::Stream { error_type, .. }) => error_type,
            _ => unreachable!("stream branch must have a stream return type"),
        };
        let error_expr = ts_stream_error_expr(context, error_type, "__next.error", 0);
        return format!(
            "export function {js_name}({arg_decls}): {ret_ts} {{\n    \
             return createUniFfiStream<{item_ts}, unknown>({{\n        \
             start: () => __call<any>(\"{rust_name}\"{sep}{arg_pass}),\n        \
             next: async (__streamHandle: unknown) => {{\n            \
             const __next = await __callAsync<any>(\"{next_name}\", __streamHandle);\n            \
             const __hasOwn = (key: string): boolean => Object.prototype.hasOwnProperty.call(__next, key);\n            \
             const __hasOnly = (...keys: string[]): boolean => Object.keys(__next).every((key) => keys.includes(key));\n            \
             if (__next === null || typeof __next !== \"object\" || !__hasOwn(\"kind\")) {{\n                \
             throw new UniffiError({{ errorName: \"UniffiStreamProtocolError\", message: \"uniffi stream next returned an invalid tagged native step\" }});\n            \
             }}\n            \
             switch (__next.kind) {{\n                \
             case \"item\":\n                    \
             if (!__hasOwn(\"value\") || __hasOwn(\"error\") || !__hasOnly(\"kind\", \"value\")) {{\n                        \
             throw new UniffiError({{ errorName: \"UniffiStreamProtocolError\", message: \"uniffi stream Item step is malformed\" }});\n                    \
             }}\n                    \
             return {{ kind: \"item\", value: {item_lift} as {item_ts} }};\n                \
             case \"done\":\n                    \
             if (!__hasOnly(\"kind\")) {{\n                        \
             throw new UniffiError({{ errorName: \"UniffiStreamProtocolError\", message: \"uniffi stream Done step is malformed\" }});\n                    \
             }}\n                    \
             return {{ kind: \"done\" }};\n                \
             case \"error\":\n                    \
             if (!__hasOwn(\"error\") || __hasOwn(\"value\") || !__hasOnly(\"kind\", \"error\")) {{\n                        \
             throw new UniffiError({{ errorName: \"UniffiStreamProtocolError\", message: \"uniffi stream Error step is malformed\" }});\n                    \
             }}\n                    \
             return {{ kind: \"error\", error: {error_expr} }};\n                \
             default:\n                    \
             throw new UniffiError({{ errorName: \"UniffiStreamProtocolError\", message: \"uniffi stream next returned an unknown step kind\" }});\n            \
             }}\n        \
             }},\n        \
             cancel: (__streamHandle: unknown): void => {{\n            \
             __call<void>(\"{cancel_name}\", __streamHandle);\n        \
             }},\n    \
             }});\n\
             }}\n",
            sep = if arg_pass.is_empty() { "" } else { ", " },
        );
    }
    if f.is_async() {
        if let Some(ret_ty) = ret_ty {
            format!(
                "export async function {js_name}({arg_decls}): Promise<{ret_ts}> {{\n    \
                 const __ret = await __callAsync<{call_g}>(\"{rust_name}\"{sep}{arg_pass});\n    \
                 return {lift} as {ret_ts};\n\
                 }}\n",
                sep = if arg_pass.is_empty() { "" } else { ", " },
                lift = ts_lift_expr(context, ret_ty, "__ret", 0),
            )
        } else {
            format!(
                "export async function {js_name}({arg_decls}): Promise<void> {{\n    \
                 await __callAsync<void>(\"{rust_name}\"{sep}{arg_pass});\n\
                 }}\n",
                sep = if arg_pass.is_empty() { "" } else { ", " },
            )
        }
    } else {
        if let Some(ret_ty) = ret_ty {
            format!(
                "export function {js_name}({arg_decls}): {ret_ts} {{\n    \
                 const __ret = __call<{call_g}>(\"{rust_name}\"{sep}{arg_pass});\n    \
                 return {lift} as {ret_ts};\n\
                 }}\n",
                sep = if arg_pass.is_empty() { "" } else { ", " },
                lift = ts_lift_expr(context, ret_ty, "__ret", 0),
            )
        } else {
            format!(
                "export function {js_name}({arg_decls}): void {{\n    \
                 __call<void>(\"{rust_name}\"{sep}{arg_pass});\n\
                 }}\n",
                sep = if arg_pass.is_empty() { "" } else { ", " },
            )
        }
    }
}

// -----------------------------------------------------------------------
// helpers
// -----------------------------------------------------------------------

fn header(module: &str) -> String {
    format!(
        "// AUTOGENERATED by uniffi_bindgen_javascript (common/{module}).\n\
         // Do not edit by hand; regenerate via `uniffi-bindgen generate --language javascript`.\n\n"
    )
}

/// Every generated common file is an ES module, including files for component
/// categories that happen to be empty.  TypeScript otherwise treats a
/// comment-only file as a script and rejects type-only re-exports from it
/// under strict module resolution (TS2306).
fn finish_module(mut out: String) -> String {
    let is_module = out.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with("import ") || line.starts_with("export ")
    });
    if !is_module {
        out.push_str("export {};\n");
    }
    out
}

fn js_fn_name(rust: &str) -> String {
    crate::js_names::function_name(rust)
}

fn js_field_name(rust: &str) -> String {
    crate::js_names::field_name(rust)
}

/// Returns `(decls, pass)` where `decls` is the comma-separated TS
/// parameter list as seen by the caller and `pass` lowers each value
/// into what the backend expects.
fn lowered_args<'a, I>(context: &RenderContext<'_>, args: I) -> (String, String)
where
    I: IntoIterator<Item = (&'a str, Type)>,
{
    let mut decls = Vec::new();
    let mut pass = Vec::new();
    for (raw_name, ty) in args {
        let js = js_field_name(raw_name);
        decls.push(format!("{js}: {}", ts_type(context, &ty)));
        pass.push(ts_lower_expr(context, &ty, &js, 0));
    }
    (decls.join(", "), pass.join(", "))
}

fn ts_lower_expr(context: &RenderContext<'_>, ty: &Type, ident: &str, depth: usize) -> String {
    match ty {
        Type::Int64 => format!("toI64({ident})"),
        Type::UInt64 => format!("toU64({ident})"),
        Type::Optional { inner_type } => format!(
            "({ident} == null ? undefined : {})",
            ts_lower_expr(context, inner_type, ident, depth + 1)
        ),
        Type::Sequence { inner_type } => {
            let item = format!("__item{depth}");
            let lowered = ts_arrow_expr_body(ts_lower_expr(context, inner_type, &item, depth + 1));
            format!("{ident}.map(({item}) => {})", lowered)
        }
        Type::Enum { .. } => ts_lower_enum(context, ty, ident, depth),
        Type::Record { name, .. } => {
            let record = context
                .owner_ci(ty)
                .and_then(|owner| owner.get_record_definition(name));
            let Some(record) = record else {
                return ident.to_string();
            };
            let fields = record
                .fields()
                .iter()
                .map(|field| {
                    let field_name = js_field_name(field.name());
                    format!(
                        "{field_name}: {}",
                        ts_lower_expr(
                            context,
                            &field.as_type(),
                            &format!("{ident}.{field_name}"),
                            depth + 1,
                        )
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {fields} }}")
        }
        Type::Custom { name, builtin, .. } => match context.custom_config_for(ty, name) {
            Some(_) => {
                let helper = context
                    .external_module_for(ty)
                    .map(|module| format!("{}.__uniffiLowerCustom{name}", module.alias()))
                    .unwrap_or_else(|| format!("__uniffiLowerCustom{name}"));
                let custom = format!("{helper}({ident})");
                ts_lower_expr(context, builtin, &custom, depth + 1)
            }
            None => ts_lower_expr(context, builtin, ident, depth + 1),
        },
        Type::InputStream {
            item_type,
            error_type,
            ..
        } => {
            let value = format!("__uniffiInputValue{depth}");
            let error = format!("__uniffiInputError{depth}");
            let lowered_value =
                ts_arrow_expr_body(ts_lower_expr(context, item_type, &value, depth + 1));
            let lowered_error =
                ts_arrow_expr_body(ts_lower_expr(context, error_type, &error, depth + 1));
            let error_shape = input_stream_error_shape(context, error_type);
            format!(
                "createUniffiInputStream({ident}, {{ lowerItem: ({value}: any) => {lowered_value}, lowerError: ({error}: unknown) => {lowered_error}, errorShape: \"{error_shape}\" }})"
            )
        }
        // Callback traits / `with_foreign` traits are lowered as a
        // tagged marker. Each backend adapter intercepts the marker and
        // translates it into whatever its native layer wants:
        // - wasm: calls `registerCallback(marker.object)` → u32 handle
        // - napi: strips the wrapper and passes `marker.object` through
        //         to the `#[napi(object)]` struct with tsfn fields
        // - electron preload: unwraps to the plain JS object before
        //         dispatching to the underlying napi addon
        // common/api.ts itself stays backend-agnostic.
        Type::Object {
            imp: ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly),
            name,
            ..
        }
        | Type::CallbackInterface { name, .. } => {
            let fallible_methods = callback_fallible_methods(context, ty);
            let async_methods = callback_async_methods(context, ty);
            let callback_return_methods = callback_return_methods(context, ty);
            let helper = context
                .external_module_for(ty)
                .map(|module| format!("{}.__uniffiLowerCallback{name}", module.alias()))
                .unwrap_or_else(|| format!("__uniffiLowerCallback{name}"));
            let object = format!("{helper}({ident})");
            let mut extras = Vec::new();
            if !fallible_methods.is_empty() {
                let methods = fallible_methods
                    .iter()
                    .map(|(m, shape)| format!("\"{m}\": \"{shape}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                extras.push(format!("fallibleMethods: {{ {methods} }}"));
            }
            if !async_methods.is_empty() {
                let methods = async_methods
                    .iter()
                    .map(|m| format!("\"{m}\": true"))
                    .collect::<Vec<_>>()
                    .join(", ");
                extras.push(format!("asyncMethods: {{ {methods} }}"));
            }
            if !callback_return_methods.is_empty() {
                let methods = callback_return_methods
                    .iter()
                    .map(|m| format!("\"{m}\": true"))
                    .collect::<Vec<_>>()
                    .join(", ");
                extras.push(format!("callbackReturnMethods: {{ {methods} }}"));
            }
            if extras.is_empty() {
                format!("{{ __uniffiCallback: true, object: {object} }}")
            } else {
                format!(
                    "{{ __uniffiCallback: true, object: {object}, {} }}",
                    extras.join(", ")
                )
            }
        }
        // Opaque objects: pass the u32 handle stored on the JS wrapper.
        Type::Object { .. } => format!("{ident}.__uniffi.raw"),
        _ => ident.to_string(),
    }
}

fn callback_async_methods(context: &RenderContext<'_>, ty: &Type) -> Vec<String> {
    let Some(name) = ty.name() else {
        return Vec::new();
    };
    let Some(ci) = context.owner_ci(ty) else {
        return Vec::new();
    };
    let methods = ci
        .object_definitions()
        .iter()
        .find(|obj| {
            obj.name() == name
                && matches!(
                    obj.imp(),
                    ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
                )
        })
        .map(|obj| obj.methods())
        .or_else(|| {
            ci.callback_interface_definitions()
                .iter()
                .find(|callback| callback.name() == name)
                .map(|callback| callback.methods())
        });
    methods
        .into_iter()
        .flatten()
        .filter(|method| method.is_async())
        .map(|method| js_fn_name(method.name()))
        .collect()
}

fn callback_return_methods(context: &RenderContext<'_>, ty: &Type) -> Vec<String> {
    let Some(name) = ty.name() else {
        return Vec::new();
    };
    let Some(ci) = context.owner_ci(ty) else {
        return Vec::new();
    };
    let methods = ci
        .object_definitions()
        .iter()
        .find(|obj| {
            obj.name() == name
                && matches!(
                    obj.imp(),
                    ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
                )
        })
        .map(|obj| obj.methods())
        .or_else(|| {
            ci.callback_interface_definitions()
                .iter()
                .find(|callback| callback.name() == name)
                .map(|callback| callback.methods())
        });
    methods
        .into_iter()
        .flatten()
        .filter(|method| {
            method
                .return_type()
                .is_some_and(callback_metadata::is_callback_return_type)
        })
        .map(|method| js_fn_name(method.name()))
        .collect()
}

fn callback_fallible_methods(
    context: &RenderContext<'_>,
    ty: &Type,
) -> Vec<(String, &'static str)> {
    let Some(name) = ty.name() else {
        return Vec::new();
    };
    let Some(ci) = context.owner_ci(ty) else {
        return Vec::new();
    };
    let methods = ci
        .object_definitions()
        .iter()
        .find(|obj| {
            obj.name() == name
                && matches!(
                    obj.imp(),
                    ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
                )
        })
        .map(|obj| obj.methods())
        .or_else(|| {
            ci.callback_interface_definitions()
                .iter()
                .find(|callback| callback.name() == name)
                .map(|callback| callback.methods())
        });
    let mut out = methods
        .into_iter()
        .flatten()
        .filter_map(|method| {
            method.throws_type().map(|throws| {
                let shape = match throws {
                    Type::Enum { name, .. } => ci
                        .enum_definitions()
                        .iter()
                        .find(|enum_| enum_.name() == name)
                        .filter(|enum_| {
                            enum_
                                .variants()
                                .iter()
                                .all(|variant| variant.fields().is_empty())
                        })
                        .map(|_| "flat")
                        .unwrap_or("shape"),
                    _ => "shape",
                };
                (js_fn_name(method.name()), shape)
            })
        })
        .collect::<Vec<_>>();
    out.sort();
    out
}

fn ts_lift_expr(context: &RenderContext<'_>, ty: &Type, ident: &str, depth: usize) -> String {
    match ty {
        Type::Int64 | Type::UInt64 => ident.to_string(),
        Type::Optional { inner_type } => format!(
            "({ident} == null ? {ident} : {})",
            ts_lift_expr(context, inner_type, ident, depth + 1)
        ),
        Type::Sequence { inner_type } => {
            let item = format!("__item{depth}");
            let lifted = ts_arrow_expr_body(ts_lift_expr(context, inner_type, &item, depth + 1));
            format!("{ident}.map(({item}: any) => {})", lifted)
        }
        Type::Enum { .. } => ts_lift_enum(context, ty, ident, depth),
        Type::Record { name, .. } => {
            let record = context
                .owner_ci(ty)
                .and_then(|owner| owner.get_record_definition(name));
            let Some(record) = record else {
                return ident.to_string();
            };
            let fields = record
                .fields()
                .iter()
                .map(|field| {
                    let field_name = js_field_name(field.name());
                    format!(
                        "{field_name}: {}",
                        ts_lift_expr(
                            context,
                            &field.as_type(),
                            &format!("{ident}.{field_name}"),
                            depth + 1,
                        )
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {fields} }}")
        }
        Type::Custom { name, builtin, .. } => match context.custom_config_for(ty, name) {
            Some(_) => format!(
                "{}({})",
                context
                    .external_module_for(ty)
                    .map(|module| format!("{}.__uniffiLiftCustom{name}", module.alias()))
                    .unwrap_or_else(|| format!("__uniffiLiftCustom{name}")),
                ts_lift_expr(context, builtin, ident, depth + 1)
            ),
            None => ts_lift_expr(context, builtin, ident, depth + 1),
        },
        Type::Object { .. } => {
            format!("{}.__fromHandle({ident})", context.named_type_reference(ty))
        }
        _ => ident.to_string(),
    }
}

fn ts_stream_error_expr(
    context: &RenderContext<'_>,
    ty: &Type,
    ident: &str,
    depth: usize,
) -> String {
    let lifted = ts_lift_expr(context, ty, ident, depth);
    match ty {
        Type::Enum { name, .. }
            if context
                .owner_ci(ty)
                .is_some_and(|owner| owner.is_name_used_as_error(name)) =>
        format!(
            "(() => {{ const __streamError = {lifted}; const __variant = typeof __streamError === \"string\" ? __streamError : (__streamError !== null && typeof __streamError === \"object\" && typeof (__streamError as {{ tag?: unknown }}).tag === \"string\" ? (__streamError as {{ tag: string }}).tag : null); return new {}(`uniffi stream error${{__variant === null ? \"\" : `: ${{__variant}}`}}`, __variant, __streamError); }})()",
            context.named_type_reference(ty),
        ),
        _ => format!(
            "(() => {{ const __streamError = {lifted}; return new UniffiError({{ errorName: \"UniffiStreamError\", data: __streamError, message: String(__streamError) }}); }})()"
        ),
    }
}

fn ts_arrow_expr_body(expr: String) -> String {
    if expr.trim_start().starts_with('{') {
        format!("({expr})")
    } else {
        expr
    }
}

fn input_stream_error_shape(context: &RenderContext<'_>, error_type: &Type) -> &'static str {
    match error_type {
        Type::Enum { name, .. } => context
            .owner_ci(error_type)
            .into_iter()
            .flat_map(|owner| owner.enum_definitions())
            .find(|enum_| enum_.name() == name)
            .filter(|enum_| {
                enum_
                    .variants()
                    .iter()
                    .all(|variant| variant.fields().is_empty())
            })
            .map(|_| "flat")
            .unwrap_or("shape"),
        _ => "shape",
    }
}

/// Map a uniffi `Type` to its TypeScript surface type. `i64`/`u64` are
/// surfaced as `bigint` — the 64-bit integer contract is bigint-first
/// so there is no silent precision loss for values beyond
/// `Number.MAX_SAFE_INTEGER`. The generator inserts `toI64`/`toU64`
/// coercions on the argument side; return values are already `bigint`
/// from the backend and need no conversion.
///
/// Chronological builtins follow the existing JavaScript conventions used
/// by `uniffi-bindgen-react-native`:
///
/// - `timestamp` -> `Date`
/// - `duration` -> `number` milliseconds
fn ts_type(context: &RenderContext<'_>, ty: &Type) -> String {
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
        Type::Bytes => "Uint8Array".to_string(),
        Type::Record { .. } | Type::Enum { .. } => context.named_type_reference(ty),
        Type::Object { .. } => context.named_type_reference(ty),
        Type::Optional { inner_type } => format!("{} | null", ts_type(context, inner_type)),
        Type::Sequence { inner_type } => format!("Array<{}>", ts_type(context, inner_type)),
        Type::Map {
            key_type,
            value_type,
        } => format!(
            "Record<{}, {}>",
            ts_type(context, key_type),
            ts_type(context, value_type)
        ),
        Type::Box { inner_type } => ts_type(context, inner_type),
        Type::Set { inner_type } => format!("Set<{}>", ts_type(context, inner_type)),
        Type::Stream { item_type, .. } => {
            format!("UniFfiStream<{}>", ts_type(context, item_type))
        }
        Type::InputStream { item_type, .. } => {
            format!("AsyncIterable<{}>", ts_type(context, item_type))
        }
        Type::Timestamp => "Date".to_string(),
        Type::Duration => "number".to_string(),
        Type::CallbackInterface { .. } => context.named_type_reference(ty),
        Type::Custom { name, builtin, .. } => context
            .custom_config_for(ty, name)
            .map(|_| context.named_type_reference(ty))
            .unwrap_or_else(|| ts_type(context, builtin)),
    }
}

/// The TS generic passed to `__call<_>` / `__callAsync<_>`. For i64/u64
/// the backend yields `bigint`, which the high-level API exposes directly.
fn call_generic(ty: Option<&Type>) -> &'static str {
    match ty {
        Some(Type::Int64 | Type::UInt64) => "bigint",
        _ => "any",
    }
}

#[derive(Copy, Clone)]
enum UsagePos {
    Arg,
    Ret,
    TypeOnly,
}

/// Scan over the types referenced by a set of function-like signatures
/// and record what runtime helpers and named types need to be imported.
#[derive(Default)]
struct Usage {
    needs_to_i64: bool,
    needs_to_u64: bool,
    needs_from_i64: bool,
    needs_from_u64: bool,
    needs_stream: bool,
    needs_input_stream: bool,
    /// Error classes that a stream-step Error arm constructs at runtime.
    stream_error_values: BTreeSet<String>,
    /// Every named type (record/enum/error/object/callback/trait) touched.
    named: BTreeSet<String>,
    /// Owner-qualified modules for names from another selected component.
    /// Keeping this independent of `named` prevents a local `Shared` from
    /// shadowing a foreign `Shared` during import classification.
    external_modules: BTreeSet<ExternalModule>,
    /// Object names appearing as return types — their class value (not
    /// just the type) must be imported because `render_free_function`
    /// emits `{Name}.__fromHandle(...)`.
    objects_in_ret: BTreeSet<String>,
    customs: BTreeSet<String>,
    /// Local callback lowerers needed by conversion expressions nested in
    /// the selected component.  Owner-aware collection keeps external
    /// callbacks on their owner-qualified module alias.
    callback_lower_helpers: BTreeSet<String>,
    /// Local configured custom conversions used while lowering arguments.
    custom_lower_helpers: BTreeSet<String>,
    /// Local configured custom conversions used while lifting returns.
    custom_lift_helpers: BTreeSet<String>,
    seen_named_payloads: BTreeSet<(PayloadDirection, String)>,
}

#[derive(Copy, Clone, Eq, Ord, PartialEq, PartialOrd)]
enum PayloadDirection {
    Lower,
    Lift,
}

impl Usage {
    fn see(&mut self, context: &RenderContext<'_>, ty: &Type, pos: UsagePos) {
        self.see_type(context, ty);
        match pos {
            UsagePos::Arg => self.see_lower_helpers(context, ty),
            UsagePos::Ret => {
                if let Type::Stream {
                    item_type,
                    error_type,
                    ..
                } = ty
                {
                    // `render_free_function` handles output streams outside
                    // `ts_lift_expr`, so collect both expressions explicitly.
                    self.see_lift_helpers(context, item_type);
                    self.see_type(context, error_type);
                    self.see_lift_helpers(context, error_type);
                    if let Type::Enum { name, .. } = error_type.as_ref() {
                        if context.is_local_named_type(error_type)
                            && context
                                .owner_ci(error_type)
                                .is_some_and(|owner| owner.is_name_used_as_error(name))
                        {
                            self.stream_error_values.insert(name.clone());
                        }
                    }
                } else {
                    self.see_lift_helpers(context, ty);
                }
            }
            UsagePos::TypeOnly => {}
        }
    }

    /// Collect names appearing in generated TypeScript surface types.  This
    /// follows `ts_type`, independently of whether the same type has a
    /// lower/lift conversion expression.
    fn see_type(&mut self, context: &RenderContext<'_>, ty: &Type) {
        match ty {
            Type::Record { name, .. }
            | Type::Enum { name, .. }
            | Type::Object { name, .. }
            | Type::CallbackInterface { name, .. } => {
                self.see_named(context, ty, name);
            }
            Type::Custom { name, builtin, .. } => {
                if context.custom_config_for(ty, name).is_some() {
                    self.see_named(context, ty, name);
                    if context.is_local_named_type(ty) {
                        self.customs.insert(name.clone());
                    }
                } else {
                    self.see_type(context, builtin);
                }
            }
            Type::Optional { inner_type }
            | Type::Sequence { inner_type }
            | Type::Set { inner_type }
            | Type::Box { inner_type } => self.see_type(context, inner_type),
            Type::Map {
                key_type,
                value_type,
            } => {
                self.see_type(context, key_type);
                self.see_type(context, value_type);
            }
            Type::Stream { item_type, .. } => {
                self.needs_stream = true;
                self.see_type(context, item_type);
            }
            Type::InputStream { item_type, .. } => self.see_type(context, item_type),
            _ => {}
        }
    }

    fn see_named(&mut self, context: &RenderContext<'_>, ty: &Type, name: &str) {
        if let Some(module) = context.external_module_for(ty) {
            self.external_modules.insert(module);
        } else {
            self.named.insert(name.to_string());
        }
    }

    fn see_named_payloads(
        &mut self,
        context: &RenderContext<'_>,
        ty: &Type,
        direction: PayloadDirection,
    ) {
        let key = format!("{ty:?}");
        if !self.seen_named_payloads.insert((direction, key)) {
            return;
        }

        let Some(owner) = context.owner_ci(ty) else {
            return;
        };

        match ty {
            Type::Record { name, .. } => {
                if let Some(record) = owner.get_record_definition(name) {
                    for field in record.fields() {
                        match direction {
                            PayloadDirection::Lower => {
                                self.see_lower_helpers(context, &field.as_type())
                            }
                            PayloadDirection::Lift => {
                                self.see_lift_helpers(context, &field.as_type())
                            }
                        }
                    }
                    return;
                }
            }
            Type::Enum { name, .. } => {
                if let Some(enum_) = owner.get_enum_definition(name) {
                    for variant in enum_.variants() {
                        for field in variant.fields() {
                            match direction {
                                PayloadDirection::Lower => {
                                    self.see_lower_helpers(context, &field.as_type())
                                }
                                PayloadDirection::Lift => {
                                    self.see_lift_helpers(context, &field.as_type())
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    fn see_lower_helpers(&mut self, context: &RenderContext<'_>, ty: &Type) {
        match ty {
            Type::Int64 => self.needs_to_i64 = true,
            Type::UInt64 => self.needs_to_u64 = true,
            Type::Custom { name, builtin, .. } => {
                if context.custom_config_for(ty, name).is_some() {
                    self.see_named(context, ty, name);
                    if context.is_local_named_type(ty) {
                        self.custom_lower_helpers.insert(name.clone());
                    }
                }
                self.see_lower_helpers(context, builtin);
            }
            Type::Object {
                name,
                imp: ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly),
                ..
            }
            | Type::CallbackInterface { name, .. } => {
                if context.is_local_named_type(ty) {
                    self.callback_lower_helpers.insert(name.clone());
                } else {
                    self.see_named(context, ty, name);
                }
            }
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
                self.see_lower_helpers(context, inner_type)
            }
            Type::Record { .. } | Type::Enum { .. } => {
                self.see_named_payloads(context, ty, PayloadDirection::Lower);
            }
            Type::InputStream {
                item_type,
                error_type,
                ..
            } => {
                self.needs_input_stream = true;
                self.see_lower_helpers(context, item_type);
                self.see_lower_helpers(context, error_type);
            }
            _ => {}
        }
    }

    fn see_lift_helpers(&mut self, context: &RenderContext<'_>, ty: &Type) {
        match ty {
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
                self.see_lift_helpers(context, inner_type)
            }
            Type::Custom { name, builtin, .. } => {
                if context.custom_config_for(ty, name).is_some() {
                    self.see_named(context, ty, name);
                    if context.is_local_named_type(ty) {
                        self.custom_lift_helpers.insert(name.clone());
                    }
                }
                self.see_lift_helpers(context, builtin);
            }
            Type::Object { name, .. } => {
                self.see_named(context, ty, name);
                if context.is_local_named_type(ty) {
                    self.objects_in_ret.insert(name.clone());
                }
            }
            Type::Record { .. } | Type::Enum { .. } => {
                self.see_named_payloads(context, ty, PayloadDirection::Lift);
            }
            _ => {}
        }
    }
}

#[derive(Default)]
struct GroupedNames {
    records: Vec<String>,
    enums: Vec<String>,
    errors: Vec<String>,
    callbacks: Vec<String>,
    objects: Vec<String>,
    customs: Vec<String>,
}

/// Classify a set of type names against the CI so we know which source
/// file to import each one from.
fn group_named_types(ci: &ComponentInterface, names: &BTreeSet<String>) -> GroupedNames {
    let mut g = GroupedNames::default();
    for n in names {
        if ci.is_name_used_as_error(n) {
            g.errors.push(n.clone());
        } else if ci.record_definitions().iter().any(|r| r.name() == n) {
            g.records.push(n.clone());
        } else if ci.enum_definitions().iter().any(|e| e.name() == n) {
            g.enums.push(n.clone());
        } else if ci.object_definitions().iter().any(|o| {
            o.name() == n
                && matches!(
                    o.imp(),
                    ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
                )
        }) {
            g.callbacks.push(n.clone());
        } else if ci.object_definitions().iter().any(|o| o.name() == n) {
            g.objects.push(n.clone());
        } else if ci
            .callback_interface_definitions()
            .iter()
            .any(|c| c.name() == n)
        {
            g.callbacks.push(n.clone());
        } else if matches!(ci.get_type(n), Some(Type::Custom { .. })) {
            g.customs.push(n.clone());
        }
    }
    g
}

fn join_sorted(xs: &[String]) -> String {
    let mut v: Vec<&String> = xs.iter().collect();
    v.sort();
    v.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
}

// -----------------------------------------------------------------------
// public-types.ts
// -----------------------------------------------------------------------

/// Emit a stable facade that re-exports public contract types and the
/// small number of type values needed to access static helpers
/// (record/enum constructors/methods, object constructors).
fn render_public_types(context: &RenderContext<'_>) -> String {
    let ci = context.ci();
    use std::fmt::Write;
    let mut out = String::new();
    let mut api_entries: Vec<(String, &'static str)> = Vec::new();
    writeln!(
        out,
        "// AUTOGENERATED by uniffi_bindgen_javascript — do not edit.\n\
         //\n\
         // Stable public contract for the `{}` component.\n\
         // Import from this file to get high-level types and type values without\n\
         // depending on implementation-detail modules.\n",
        ci.namespace()
    )
    .unwrap();

    // Records
    let record_values: Vec<String> = ci
        .record_definitions()
        .iter()
        .filter(|r| !r.constructors().is_empty() || !r.methods().is_empty())
        .map(|r| r.name().to_string())
        .collect();
    if !record_values.is_empty() {
        writeln!(
            out,
            "export {{ {} }} from \"./records.ts\";",
            join_sorted(&record_values)
        )
        .unwrap();
        api_entries.extend(
            record_values
                .iter()
                .map(|name| (name.clone(), "./records.ts")),
        );
    }
    let records: Vec<String> = ci
        .record_definitions()
        .iter()
        .filter(|r| r.constructors().is_empty() && r.methods().is_empty())
        .map(|r| r.name().to_string())
        .collect();
    if !records.is_empty() {
        writeln!(
            out,
            "export type {{ {} }} from \"./records.ts\";",
            join_sorted(&records)
        )
        .unwrap();
    }

    // Enums (non-error)
    let enum_values: Vec<String> = ci
        .enum_definitions()
        .iter()
        .filter(|e| !ci.is_name_used_as_error(e.name()))
        .filter(|e| !e.constructors().is_empty() || !e.methods().is_empty())
        .map(|e| e.name().to_string())
        .collect();
    if !enum_values.is_empty() {
        writeln!(
            out,
            "export {{ {} }} from \"./enums.ts\";",
            join_sorted(&enum_values)
        )
        .unwrap();
        api_entries.extend(enum_values.iter().map(|name| (name.clone(), "./enums.ts")));
    }
    let enums: Vec<String> = ci
        .enum_definitions()
        .iter()
        .filter(|e| !ci.is_name_used_as_error(e.name()))
        .filter(|e| e.constructors().is_empty() && e.methods().is_empty())
        .map(|e| e.name().to_string())
        .collect();
    if !enums.is_empty() {
        writeln!(
            out,
            "export type {{ {} }} from \"./enums.ts\";",
            join_sorted(&enums)
        )
        .unwrap();
    }

    // Errors
    let errors: Vec<String> = ci
        .enum_definitions()
        .iter()
        .filter(|e| ci.is_name_used_as_error(e.name()))
        .map(|e| e.name().to_string())
        .collect();
    if !errors.is_empty() {
        writeln!(
            out,
            "export {{ {} }} from \"./errors.ts\";",
            join_sorted(&errors)
        )
        .unwrap();
        api_entries.extend(errors.iter().map(|name| (name.clone(), "./errors.ts")));
    }
    // Always re-export the stable JavaScript runtime contract types, but never
    // expose its backend installation or raw stream-step implementation.
    writeln!(out, "export {{ UniffiError }} from \"./runtime.ts\";").unwrap();
    writeln!(out, "export type {{ UniFfiStream }} from \"./runtime.ts\";").unwrap();
    api_entries.push(("UniffiError".to_string(), "./runtime.ts"));

    // Callbacks (callback interfaces + callback traits)
    let mut callbacks: Vec<String> = ci
        .callback_interface_definitions()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    for o in ci.object_definitions() {
        if matches!(
            o.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            callbacks.push(o.name().to_string());
        }
    }
    if !callbacks.is_empty() {
        writeln!(
            out,
            "export type {{ {} }} from \"./callbacks.ts\";",
            join_sorted(&callbacks)
        )
        .unwrap();
    }

    // Objects — re-export the class (value, not just type) so downstream
    // can construct instances and access static methods.
    let objects: Vec<String> = ci
        .object_definitions()
        .iter()
        .filter(|o| {
            !matches!(
                o.imp(),
                ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
            )
        })
        .map(|o| o.name().to_string())
        .collect();
    if !objects.is_empty() {
        writeln!(
            out,
            "export {{ {} }} from \"./objects.ts\";",
            join_sorted(&objects)
        )
        .unwrap();
        api_entries.extend(objects.iter().map(|name| (name.clone(), "./objects.ts")));
    }

    let customs: Vec<String> = all_custom_types(context)
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if !customs.is_empty() {
        writeln!(
            out,
            "export type {{ {} }} from \"./custom-types.ts\";",
            join_sorted(&customs)
        )
        .unwrap();
    }

    // Free functions — re-export from api.ts so downstream gets the
    // full public surface from one import.
    let fns: Vec<String> = ci
        .function_definitions()
        .iter()
        .map(|f| js_fn_name(f.name()))
        .collect();
    if !fns.is_empty() {
        writeln!(out, "export {{ {} }} from \"./api.ts\";", join_sorted(&fns)).unwrap();
        api_entries.extend(fns.iter().map(|name| (name.clone(), "./api.ts")));
    }

    api_entries.sort_by(|a, b| a.0.cmp(&b.0));
    api_entries.dedup_by(|a, b| a.0 == b.0 && a.1 == b.1);

    let module_type_name = public_module_type_name(ci.namespace());
    let api_type_name = public_api_type_name(ci.namespace());
    writeln!(out, "\nexport interface {module_type_name} {{").unwrap();
    for (name, module) in &api_entries {
        writeln!(out, "    {name}: typeof import(\"{module}\").{name};").unwrap();
    }
    writeln!(out, "}}\n").unwrap();
    writeln!(out, "export type {api_type_name} = {module_type_name};").unwrap();
    writeln!(out, "export type UniffiPublicApi = {module_type_name};").unwrap();

    out
}

fn public_module_type_name(namespace: &str) -> String {
    let mut name = namespace.to_upper_camel_case();
    if name.is_empty() {
        name.push_str("Uniffi");
    }
    if name.starts_with(|ch: char| ch.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name.push_str("Module");
    name
}

fn public_api_type_name(namespace: &str) -> String {
    let mut name = namespace.to_upper_camel_case();
    if name.is_empty() {
        name.push_str("Uniffi");
    }
    if name.starts_with(|ch: char| ch.is_ascii_digit()) {
        name.insert(0, '_');
    }
    name.push_str("Api");
    name
}

fn all_custom_types(context: &RenderContext<'_>) -> BTreeSet<(String, Type)> {
    let ci = context.ci();
    let mut customs = BTreeSet::new();
    for record in ci.record_definitions() {
        for field in record.fields() {
            collect_custom_types(context, &field.as_type(), &mut customs);
        }
        for constructor in record.constructors() {
            for arg in constructor.arguments() {
                collect_custom_types(context, &arg.as_type(), &mut customs);
            }
        }
        collect_custom_types(context, &record.as_type(), &mut customs);
        for method in record.methods() {
            for arg in method.arguments() {
                collect_custom_types(context, &arg.as_type(), &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(context, ret, &mut customs);
            }
        }
    }
    for enum_ in ci.enum_definitions() {
        for variant in enum_.variants() {
            for field in variant.fields() {
                collect_custom_types(context, &field.as_type(), &mut customs);
            }
        }
        for constructor in enum_.constructors() {
            for arg in constructor.arguments() {
                collect_custom_types(context, &arg.as_type(), &mut customs);
            }
        }
        collect_custom_types(context, &enum_.as_type(), &mut customs);
        for method in enum_.methods() {
            for arg in method.arguments() {
                collect_custom_types(context, &arg.as_type(), &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(context, ret, &mut customs);
            }
        }
    }
    for object in ci.object_definitions() {
        for constructor in object.constructors() {
            for arg in constructor.arguments() {
                collect_custom_types(context, &arg.as_type(), &mut customs);
            }
        }
        for method in object.methods() {
            for arg in method.arguments() {
                collect_custom_types(context, &arg.as_type(), &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(context, ret, &mut customs);
            }
        }
    }
    for callback in ci.callback_interface_definitions() {
        for method in callback.methods() {
            for arg in method.arguments() {
                collect_custom_types(context, &arg.as_type(), &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(context, ret, &mut customs);
            }
        }
    }
    for function in ci.function_definitions() {
        for arg in function.arguments() {
            collect_custom_types(context, &arg.as_type(), &mut customs);
        }
        if let Some(ret) = function.return_type() {
            collect_custom_types(context, ret, &mut customs);
        }
    }
    customs
}

fn collect_custom_types(
    context: &RenderContext<'_>,
    ty: &Type,
    customs: &mut BTreeSet<(String, Type)>,
) {
    match ty {
        Type::Custom { name, builtin, .. } => {
            if context.is_local_named_type(ty) && context.custom_config_for(ty, name).is_some() {
                customs.insert((name.clone(), builtin.as_ref().clone()));
            }
            collect_custom_types(context, builtin, customs);
        }
        Type::Optional { inner_type }
        | Type::Sequence { inner_type }
        | Type::Stream {
            item_type: inner_type,
            ..
        } => collect_custom_types(context, inner_type, customs),
        Type::InputStream {
            item_type,
            error_type,
            ..
        } => {
            collect_custom_types(context, item_type, customs);
            collect_custom_types(context, error_type, customs);
        }
        Type::Map {
            key_type,
            value_type,
        } => {
            collect_custom_types(context, key_type, customs);
            collect_custom_types(context, value_type, customs);
        }
        _ => {}
    }
}

fn render_import_statement(import: &str) -> String {
    let trimmed = import.trim().trim_end_matches(';');
    if trimmed.starts_with("import ") {
        format!("{trimmed};")
    } else {
        format!("import {trimmed};")
    }
}

fn custom_helper_imports(usage: &Usage) -> BTreeSet<String> {
    usage
        .custom_lower_helpers
        .iter()
        .map(|name| format!("__uniffiLowerCustom{name}"))
        .chain(
            usage
                .custom_lift_helpers
                .iter()
                .map(|name| format!("__uniffiLiftCustom{name}")),
        )
        .collect()
}

fn ts_lower_enum(context: &RenderContext<'_>, ty: &Type, ident: &str, depth: usize) -> String {
    let Some(name) = ty.name() else {
        return ident.to_string();
    };
    let Some(enum_) = context.owner_ci(ty).and_then(|owner| {
        owner
            .enum_definitions()
            .iter()
            .find(|enum_| enum_.name() == name)
    }) else {
        return ident.to_string();
    };
    if enum_.variants().iter().all(|v| v.fields().is_empty()) {
        return ident.to_string();
    }
    let tag_expr = format!("{ident}.tag");
    let mut cases = Vec::new();
    for variant in enum_.variants() {
        if variant.fields().is_empty() {
            cases.push(format!(
                "case \"{name}\": return {{ tag: \"{name}\" }};",
                name = variant.name()
            ));
            continue;
        }
        let fields = variant
            .fields()
            .iter()
            .map(|field| {
                let field_name = js_field_name(field.name());
                format!(
                    "{field_name}: {}",
                    ts_lower_expr(
                        context,
                        &field.as_type(),
                        &format!("{ident}.{field_name}"),
                        depth + 1,
                    )
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        cases.push(format!(
            "case \"{name}\": return {{ tag: \"{name}\", {fields} }};",
            name = variant.name()
        ));
    }
    format!(
        "(() => {{ switch ({tag_expr}) {{ {} default: return {ident}; }} }})()",
        cases.join(" ")
    )
}

fn ts_lift_enum(context: &RenderContext<'_>, ty: &Type, ident: &str, depth: usize) -> String {
    let Some(name) = ty.name() else {
        return ident.to_string();
    };
    let Some(enum_) = context.owner_ci(ty).and_then(|owner| {
        owner
            .enum_definitions()
            .iter()
            .find(|enum_| enum_.name() == name)
    }) else {
        return ident.to_string();
    };
    if enum_.variants().iter().all(|v| v.fields().is_empty()) {
        return ident.to_string();
    }
    let tag_expr = format!("{ident}.tag");
    let mut cases = Vec::new();
    for variant in enum_.variants() {
        if variant.fields().is_empty() {
            cases.push(format!(
                "case \"{name}\": return {{ tag: \"{name}\" }};",
                name = variant.name()
            ));
            continue;
        }
        let fields = variant
            .fields()
            .iter()
            .map(|field| {
                let field_name = js_field_name(field.name());
                format!(
                    "{field_name}: {}",
                    ts_lift_expr(
                        context,
                        &field.as_type(),
                        &format!("{ident}.{field_name}"),
                        depth + 1,
                    )
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        cases.push(format!(
            "case \"{name}\": return {{ tag: \"{name}\", {fields} }};",
            name = variant.name()
        ));
    }
    format!(
        "(() => {{ switch ({tag_expr}) {{ {} default: return {ident}; }} }})()",
        cases.join(" ")
    )
}
