/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Flavor-agnostic high-level TypeScript API emitter.
//!
//! Owns the `common/*.ts` output described in
//! `docs/manual/src/javascript/contract.md`. This
//! pass walks the `ComponentInterface` directly and emits plain
//! TypeScript — no templates, no IR pipeline, no AbiFlavor awareness.
//!
//! Layering:
//!
//! - `common/runtime.ts` is **copied verbatim** from
//!   `uniffi_runtime_javascript/typescript/src/runtime.ts`. The generator
//!   itself no longer inlines runtime helpers.
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

use crate::{callback_metadata, JsConfig};

/// The shared runtime module, shipped verbatim into every generated tree.
const RUNTIME_TS: &str =
    include_str!("../../../uniffi_runtime_javascript/typescript/src/runtime.ts");

pub fn emit(common_dir: &Utf8Path, component: &Component<JsConfig>) -> Result<()> {
    let ci = &component.ci;
    let config = &component.config;

    write_generated(common_dir.join("runtime.ts"), RUNTIME_TS.to_string())?;
    write_generated(
        common_dir.join("custom-types.ts"),
        render_custom_types(ci, config),
    )?;
    write_generated(common_dir.join("records.ts"), render_records(ci, config))?;
    write_generated(common_dir.join("enums.ts"), render_enums(ci, config))?;
    write_generated(common_dir.join("errors.ts"), render_errors(ci))?;
    write_generated(
        common_dir.join("callbacks.ts"),
        render_callbacks(ci, config),
    )?;
    write_generated(common_dir.join("objects.ts"), render_objects(ci, config))?;
    write_generated(common_dir.join("api.ts"), render_api(ci, config))?;
    write_generated(
        common_dir.join("public-types.ts"),
        render_public_types(ci, config),
    )?;
    Ok(())
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

fn render_records(ci: &ComponentInterface, config: &JsConfig) -> String {
    let mut out = header("records");
    let mut usage = Usage::default();
    let mut has_sync = false;
    let mut has_async = false;
    for record in ci.record_definitions() {
        for field in record.fields() {
            usage.see(ci, &field.as_type(), UsagePos::TypeOnly, config);
        }
        for constructor in record.constructors() {
            if constructor.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for arg in constructor.arguments() {
                usage.see(ci, &arg.as_type(), UsagePos::Arg, config);
            }
            usage.see(ci, &record.as_type(), UsagePos::Ret, config);
        }
        for method in record.methods() {
            if method.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            usage.see(ci, &record.as_type(), UsagePos::Arg, config);
            for arg in method.arguments() {
                usage.see(ci, &arg.as_type(), UsagePos::Arg, config);
            }
            if let Some(ret) = method.return_type() {
                usage.see(ci, ret, UsagePos::Ret, config);
            }
        }
    }
    emit_value_module_imports(&mut out, ci, config, &usage, has_sync, has_async, "records");
    if !usage.customs.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./custom-types.ts\";\n\n",
            join_sorted(&usage.customs.iter().cloned().collect::<Vec<_>>())
        ));
    }
    for record in ci.record_definitions() {
        out.push_str(&render_record(ci, record, config));
        out.push('\n');
    }
    out
}

fn render_record(ci: &ComponentInterface, record: &Record, config: &JsConfig) -> String {
    let mut s = format!("export interface {} {{\n", record.name());
    for f in record.fields() {
        s.push_str(&format!(
            "    {}: {};\n",
            js_field_name(f.name()),
            ts_type(&f.as_type(), config)
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
                ci,
                record.name(),
                &record.as_type(),
                constructor,
                config,
            ));
        }
        for method in record.methods() {
            s.push_str(&render_value_method(
                ci,
                record.name(),
                &record.as_type(),
                method,
                config,
            ));
        }
        s.push_str("});\n");
    }
    s
}

// -----------------------------------------------------------------------
// enums.ts
// -----------------------------------------------------------------------

fn render_enums(ci: &ComponentInterface, config: &JsConfig) -> String {
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
                usage.see(ci, &field.as_type(), UsagePos::TypeOnly, config);
            }
        }
        for constructor in enum_.constructors() {
            if constructor.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for arg in constructor.arguments() {
                usage.see(ci, &arg.as_type(), UsagePos::Arg, config);
            }
            usage.see(ci, &enum_.as_type(), UsagePos::Ret, config);
        }
        for method in enum_.methods() {
            if method.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            usage.see(ci, &enum_.as_type(), UsagePos::Arg, config);
            for arg in method.arguments() {
                usage.see(ci, &arg.as_type(), UsagePos::Arg, config);
            }
            if let Some(ret) = method.return_type() {
                usage.see(ci, ret, UsagePos::Ret, config);
            }
        }
    }
    emit_value_module_imports(&mut out, ci, config, &usage, has_sync, has_async, "enums");
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
        out.push_str(&render_enum(ci, e, config));
        out.push('\n');
    }
    out
}

// -----------------------------------------------------------------------
// custom-types.ts
// -----------------------------------------------------------------------

fn render_custom_types(ci: &ComponentInterface, config: &JsConfig) -> String {
    use std::fmt::Write;

    let mut out = header("custom-types");
    let customs = all_custom_types(ci, config);

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
    if !emitted_imports.is_empty() {
        out.push('\n');
    }

    for (name, builtin) in customs {
        let builtin_ts = ts_type(&builtin, config);
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
    out
}

fn render_enum(ci: &ComponentInterface, e: &Enum, config: &JsConfig) -> String {
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
                ci,
                e.name(),
                &e.as_type(),
                constructor,
                config,
            ));
        }
        for method in e.methods() {
            s.push_str(&render_value_method(
                ci,
                e.name(),
                &e.as_type(),
                method,
                config,
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
                        ts_type(&f.as_type(), config)
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
                ci,
                e.name(),
                &e.as_type(),
                constructor,
                config,
            ));
        }
        for method in e.methods() {
            s.push_str(&render_value_method(
                ci,
                e.name(),
                &e.as_type(),
                method,
                config,
            ));
        }
        s.push_str("});\n");
    }
    s
}

fn emit_value_module_imports(
    out: &mut String,
    ci: &ComponentInterface,
    config: &JsConfig,
    usage: &Usage,
    has_sync: bool,
    has_async: bool,
    local_module: &str,
) {
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
    let callback_helpers = value_type_callback_helpers(ci, local_module);
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
            join_sorted(&object_values.into_iter().collect::<Vec<_>>())
        ));
    }
    if !object_types.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&object_types)
        ));
    }

    let helper_customs = value_type_custom_helpers(ci, config, local_module);
    if !helper_customs.is_empty() {
        let helpers = helper_customs
            .iter()
            .flat_map(|name| {
                [
                    format!("__uniffiLowerCustom{name}"),
                    format!("__uniffiLiftCustom{name}"),
                ]
            })
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helpers)
        ));
    }

    if has_sync
        || has_async
        || !usage.named.is_empty()
        || !callback_helpers.is_empty()
        || !helper_customs.is_empty()
    {
        out.push('\n');
    }
}

fn render_value_method(
    ci: &ComponentInterface,
    owner_name: &str,
    owner_ty: &Type,
    method: &Method,
    config: &JsConfig,
) -> String {
    let js_name = js_fn_name(method.name());
    let fn_name = crate::dispatch_key::method_key(owner_name, method);
    let (arg_decls, arg_pass) = lowered_args(
        ci,
        config,
        method.arguments().iter().map(|a| (a.name(), a.as_type())),
    );
    let self_pass = ts_lower_expr(ci, config, owner_ty, "self_", 0);
    let pass = if arg_pass.is_empty() {
        self_pass
    } else {
        format!("{self_pass}, {arg_pass}")
    };
    let decls = if arg_decls.is_empty() {
        format!("self_: {}", ts_type(owner_ty, config))
    } else {
        format!("self_: {}, {arg_decls}", ts_type(owner_ty, config))
    };
    let ret_ty = method.return_type();
    let ret_ts = ret_ty
        .map(|t| ts_type(t, config))
        .unwrap_or_else(|| "void".to_string());
    let call_g = call_generic(ret_ty);
    if method.is_async() {
        if let Some(ret_ty) = ret_ty {
            format!(
                "    async {js_name}({decls}): Promise<{ret_ts}> {{\n        \
                 const __ret = await __callAsync<{call_g}>(\"{fn_name}\", {pass});\n        \
                 return {lift} as {ret_ts};\n    }},\n",
                lift = ts_lift_expr(ci, config, ret_ty, "__ret", 0),
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
            lift = ts_lift_expr(ci, config, ret_ty, "__ret", 0),
        )
    } else {
        format!(
            "    {js_name}({decls}): void {{\n        \
             __call<void>(\"{fn_name}\", {pass});\n    }},\n"
        )
    }
}

fn render_value_constructor(
    ci: &ComponentInterface,
    owner_name: &str,
    owner_ty: &Type,
    constructor: &Constructor,
    config: &JsConfig,
) -> String {
    let js_name = js_fn_name(constructor.name());
    let fn_name = crate::dispatch_key::constructor_key(owner_name, constructor);
    let (arg_decls, arg_pass) = lowered_args(
        ci,
        config,
        constructor
            .arguments()
            .iter()
            .map(|a| (a.name(), a.as_type())),
    );
    let ret_ts = ts_type(owner_ty, config);
    let call_g = call_generic(Some(owner_ty));
    let sep = if arg_pass.is_empty() { "" } else { ", " };
    if constructor.is_async() {
        format!(
            "    async {js_name}({arg_decls}): Promise<{ret_ts}> {{\n        \
             const __ret = await __callAsync<{call_g}>(\"{fn_name}\"{sep}{arg_pass});\n        \
             return {lift} as {ret_ts};\n    }},\n",
            lift = ts_lift_expr(ci, config, owner_ty, "__ret", 0),
        )
    } else {
        format!(
            "    {js_name}({arg_decls}): {ret_ts} {{\n        \
             const __ret = __call<{call_g}>(\"{fn_name}\"{sep}{arg_pass});\n        \
             return {lift} as {ret_ts};\n    }},\n",
            lift = ts_lift_expr(ci, config, owner_ty, "__ret", 0),
        )
    }
}

// -----------------------------------------------------------------------
// errors.ts
// -----------------------------------------------------------------------

fn render_errors(ci: &ComponentInterface) -> String {
    let mut out = header("errors");
    out.push_str("import { UniffiError } from \"./runtime.ts\";\n\n");
    for e in ci.enum_definitions() {
        if !ci.is_name_used_as_error(e.name()) {
            continue;
        }
        out.push_str(&format!(
            "export class {name} extends UniffiError {{\n    \
             constructor(message: string, variant: string | null = null, data: unknown = null) {{\n        \
             super({{ errorName: \"{name}\", variant, data, message }});\n    \
             }}\n}}\n\n",
            name = e.name()
        ));
    }
    out
}

// -----------------------------------------------------------------------
// callbacks.ts
// -----------------------------------------------------------------------

fn render_callbacks(ci: &ComponentInterface, config: &JsConfig) -> String {
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
                usage.see(ci, &arg.as_type(), UsagePos::Arg, config);
            }
            if let Some(ret) = method.return_type() {
                needs_callback_return_unwrap |= needs_object_callback_return_unwrap(ret);
                usage.see(ci, ret, UsagePos::Ret, config);
            }
        }
    }
    for callback in ci.callback_interface_definitions() {
        for method in callback.methods() {
            for arg in method.arguments() {
                usage.see(ci, &arg.as_type(), UsagePos::Arg, config);
            }
            if let Some(ret) = method.return_type() {
                needs_callback_return_unwrap |= needs_object_callback_return_unwrap(ret);
                usage.see(ci, ret, UsagePos::Ret, config);
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
    if !grouped.objects.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./objects.ts\";\n",
            join_sorted(&grouped.objects)
        ));
    }
    let custom_type_imports = usage.customs.iter().cloned().collect::<Vec<_>>();
    if !custom_type_imports.is_empty() {
        out.push_str(&format!(
            "import type {{ {} }} from \"./custom-types.ts\";\n\n",
            join_sorted(&custom_type_imports)
        ));
    }
    let helper_customs = callback_custom_helpers(ci, config);
    if !helper_customs.is_empty() {
        let helpers = helper_customs
            .iter()
            .flat_map(|name| {
                [
                    format!("__uniffiLowerCustom{name}"),
                    format!("__uniffiLiftCustom{name}"),
                ]
            })
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helpers)
        ));
    }
    if !grouped.records.is_empty()
        || !grouped.enums.is_empty()
        || !grouped.errors.is_empty()
        || !grouped.objects.is_empty()
        || !custom_type_imports.is_empty()
        || !helper_customs.is_empty()
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
            ci,
            config,
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
        out.push_str(&render_callback_definition(
            ci,
            config,
            obj.name(),
            &methods,
        ));
    }
    out
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
    ci: &ComponentInterface,
    config: &JsConfig,
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
                    ts_type(&a.as_type(), config)
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let ret = match m.return_type() {
            Some(t) => ts_type(t, config),
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
    out.push_str(&render_callback_lowerer(ci, config, name, methods));
    out
}

fn render_callback_lowerer(
    ci: &ComponentInterface,
    config: &JsConfig,
    name: &str,
    methods: &[&Method],
) -> String {
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
                ts_lift_expr(ci, config, &arg.as_type(), &js, 0)
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
                    _ => ts_lower_expr(ci, config, ret, "__ret", 0),
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
                _ => ts_lower_expr(ci, config, ret, "__ret", 0),
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

fn render_objects(ci: &ComponentInterface, config: &JsConfig) -> String {
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
                usage.see(ci, &a.as_type(), UsagePos::Arg, config);
            }
        }
        for m in obj.methods() {
            if m.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for a in m.arguments() {
                usage.see(ci, &a.as_type(), UsagePos::Arg, config);
            }
            if let Some(t) = m.return_type() {
                usage.see(ci, t, UsagePos::Ret, config);
            }
        }
    }

    if !any_object {
        return out;
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
    let callback_helpers = object_callback_helpers(ci);
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
    let helper_customs = object_custom_helpers(ci, config);
    if !helper_customs.is_empty() {
        let helpers = helper_customs
            .iter()
            .flat_map(|name| {
                [
                    format!("__uniffiLowerCustom{name}"),
                    format!("__uniffiLiftCustom{name}"),
                ]
            })
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helpers)
        ));
    }
    out.push('\n');

    for obj in ci.object_definitions() {
        if matches!(
            obj.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            continue;
        }
        out.push_str(&render_object_class(ci, obj, config));
        out.push('\n');
    }
    out
}

fn render_object_class(ci: &ComponentInterface, obj: &Object, config: &JsConfig) -> String {
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
            ci,
            config,
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
            ci,
            config,
            m.arguments().iter().map(|a| (a.name(), a.as_type())),
        );
        let comma_pass = if arg_pass.is_empty() { "" } else { ", " };
        let ret_ty = m.return_type();
        let ret_ts = match ret_ty {
            Some(t) => ts_type(t, config),
            None => "void".to_string(),
        };
        let call_g = call_generic(ret_ty);
        if m.is_async() {
            if let Some(ret_ty) = ret_ty {
                s.push_str(&format!(
                    "    async {m_js}({arg_decls}): Promise<{ret_ts}> {{\n        \
                     const __ret = await __callAsync<{call_g}>(\"{fn_name}\", this.__uniffi.raw{comma_pass}{arg_pass});\n        \
                     return {lift} as {ret_ts};\n    }}\n",
                    lift = ts_lift_expr(ci, config, ret_ty, "__ret", 0),
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
                    lift = ts_lift_expr(ci, config, ret_ty, "__ret", 0),
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

fn render_api(ci: &ComponentInterface, config: &JsConfig) -> String {
    let mut out = header("api");
    out.push_str(
        "export * from \"./records.ts\";\n\
         export * from \"./enums.ts\";\n\
         export * from \"./errors.ts\";\n\
         export * from \"./callbacks.ts\";\n\
         export * from \"./objects.ts\";\n",
    );
    let customs = all_custom_types(ci, config)
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
        "export {\n    \
         UniffiError,\n    \
         UNIFFI_JS_CONTRACT_VERSION,\n    \
         __installBackend,\n\
         } from \"./runtime.ts\";\n\n",
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
            usage.see(ci, &a.as_type(), UsagePos::Arg, config);
        }
        if let Some(t) = f.return_type() {
            usage.see(ci, t, UsagePos::Ret, config);
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
        runtime.push("createUniffiAsyncIterable");
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
    let callback_helpers = function_callback_helpers(ci);
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
    let helper_customs = function_custom_helpers(ci, config);
    if !helper_customs.is_empty() {
        let helpers = helper_customs
            .iter()
            .flat_map(|name| {
                [
                    format!("__uniffiLowerCustom{name}"),
                    format!("__uniffiLiftCustom{name}"),
                ]
            })
            .collect::<Vec<_>>();
        out.push_str(&format!(
            "import {{ {} }} from \"./custom-types.ts\";\n",
            join_sorted(&helpers)
        ));
    }
    out.push('\n');
    for f in ci.function_definitions() {
        out.push_str(&render_free_function(ci, config, &f));
        out.push('\n');
    }
    out
}

fn render_free_function(ci: &ComponentInterface, config: &JsConfig, f: &Function) -> String {
    let (arg_decls, arg_pass) = lowered_args(
        ci,
        config,
        f.arguments().iter().map(|a| (a.name(), a.as_type())),
    );
    let ret_ty = f.return_type();
    let ret_ts = match ret_ty {
        Some(t) => ts_type(t, config),
        None => "void".to_string(),
    };
    let call_g = call_generic(ret_ty);
    let rust_name = crate::dispatch_key::free_function_key(f.name());
    let js_name = js_fn_name(f.name());
    if let Some(Type::Stream { item_type, .. }) = ret_ty {
        let next_name = crate::dispatch_key::stream_next_key(f.name());
        let cancel_name = crate::dispatch_key::stream_cancel_key(f.name());
        let item_ts = ts_type(item_type, config);
        let item_call_g = call_generic(Some(item_type));
        let item_lift = ts_lift_expr(ci, config, item_type, "__next.value", 0);
        return format!(
            "export function {js_name}({arg_decls}): {ret_ts} {{\n    \
             const __handle = __call<any>(\"{rust_name}\"{sep}{arg_pass});\n    \
             return createUniffiAsyncIterable<{item_ts}>({{\n        \
             handle: __handle,\n        \
             next: async (__streamHandle: unknown): Promise<{item_ts} | null> => {{\n            \
             const __next = await __callAsync<{{ done: boolean; value?: {item_call_g} }}>(\"{next_name}\", __streamHandle);\n            \
             if (__next == null || __next.done === true) return null;\n            \
             return {item_lift} as {item_ts};\n        \
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
                lift = ts_lift_expr(ci, config, ret_ty, "__ret", 0),
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
                lift = ts_lift_expr(ci, config, ret_ty, "__ret", 0),
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

fn js_fn_name(rust: &str) -> String {
    crate::js_names::function_name(rust)
}

fn js_field_name(rust: &str) -> String {
    crate::js_names::field_name(rust)
}

/// Returns `(decls, pass)` where `decls` is the comma-separated TS
/// parameter list as seen by the caller and `pass` lowers each value
/// into what the backend expects.
fn lowered_args<'a, I>(ci: &ComponentInterface, config: &JsConfig, args: I) -> (String, String)
where
    I: IntoIterator<Item = (&'a str, Type)>,
{
    let mut decls = Vec::new();
    let mut pass = Vec::new();
    for (raw_name, ty) in args {
        let js = js_field_name(raw_name);
        decls.push(format!("{js}: {}", ts_type(&ty, config)));
        pass.push(ts_lower_expr(ci, config, &ty, &js, 0));
    }
    (decls.join(", "), pass.join(", "))
}

fn ts_lower_expr(
    ci: &ComponentInterface,
    config: &JsConfig,
    ty: &Type,
    ident: &str,
    depth: usize,
) -> String {
    match ty {
        Type::Int64 => format!("toI64({ident})"),
        Type::UInt64 => format!("toU64({ident})"),
        Type::Optional { inner_type } => format!(
            "({ident} == null ? undefined : {})",
            ts_lower_expr(ci, config, inner_type, ident, depth + 1)
        ),
        Type::Sequence { inner_type } => {
            let item = format!("__item{depth}");
            let lowered =
                ts_arrow_expr_body(ts_lower_expr(ci, config, inner_type, &item, depth + 1));
            format!("{ident}.map(({item}) => {})", lowered)
        }
        Type::Enum { name, .. } => ts_lower_enum(ci, config, name, ident, depth),
        Type::Record { name, .. } => {
            let record = ci
                .record_definitions()
                .iter()
                .find(|record| record.name() == name)
                .expect("record should resolve");
            let fields = record
                .fields()
                .iter()
                .map(|field| {
                    let field_name = js_field_name(field.name());
                    format!(
                        "{field_name}: {}",
                        ts_lower_expr(
                            ci,
                            config,
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
        Type::Custom { name, builtin, .. } => match config.custom_type(name) {
            Some(_) => {
                let custom = format!("__uniffiLowerCustom{name}({ident})");
                ts_lower_expr(ci, config, builtin, &custom, depth + 1)
            }
            None => ts_lower_expr(ci, config, builtin, ident, depth + 1),
        },
        Type::InputStream {
            item_type,
            error_type,
            ..
        } => {
            let value = format!("__uniffiInputValue{depth}");
            let error = format!("__uniffiInputError{depth}");
            let lowered_value =
                ts_arrow_expr_body(ts_lower_expr(ci, config, item_type, &value, depth + 1));
            let lowered_error =
                ts_arrow_expr_body(ts_lower_expr(ci, config, error_type, &error, depth + 1));
            let error_shape = input_stream_error_shape(ci, error_type);
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
            let fallible_methods = callback_fallible_methods(ci, name);
            let async_methods = callback_async_methods(ci, name);
            let callback_return_methods = callback_return_methods(ci, name);
            let object = format!("__uniffiLowerCallback{name}({ident})");
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

fn callback_async_methods(ci: &ComponentInterface, name: &str) -> Vec<String> {
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

fn callback_return_methods(ci: &ComponentInterface, name: &str) -> Vec<String> {
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

fn callback_fallible_methods(ci: &ComponentInterface, name: &str) -> Vec<(String, &'static str)> {
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

fn ts_lift_expr(
    ci: &ComponentInterface,
    config: &JsConfig,
    ty: &Type,
    ident: &str,
    depth: usize,
) -> String {
    match ty {
        Type::Int64 | Type::UInt64 => ident.to_string(),
        Type::Optional { inner_type } => format!(
            "({ident} == null ? {ident} : {})",
            ts_lift_expr(ci, config, inner_type, ident, depth + 1)
        ),
        Type::Sequence { inner_type } => {
            let item = format!("__item{depth}");
            let lifted = ts_arrow_expr_body(ts_lift_expr(ci, config, inner_type, &item, depth + 1));
            format!("{ident}.map(({item}: any) => {})", lifted)
        }
        Type::Enum { name, .. } => ts_lift_enum(ci, config, name, ident, depth),
        Type::Record { name, .. } => {
            let record = ci
                .record_definitions()
                .iter()
                .find(|record| record.name() == name)
                .expect("record should resolve");
            let fields = record
                .fields()
                .iter()
                .map(|field| {
                    let field_name = js_field_name(field.name());
                    format!(
                        "{field_name}: {}",
                        ts_lift_expr(
                            ci,
                            config,
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
        Type::Custom { name, builtin, .. } => match config.custom_type(name) {
            Some(_) => format!(
                "__uniffiLiftCustom{name}({})",
                ts_lift_expr(ci, config, builtin, ident, depth + 1)
            ),
            None => ts_lift_expr(ci, config, builtin, ident, depth + 1),
        },
        Type::Object { name, .. } => format!("{name}.__fromHandle({ident})"),
        _ => ident.to_string(),
    }
}

fn ts_arrow_expr_body(expr: String) -> String {
    if expr.trim_start().starts_with('{') {
        format!("({expr})")
    } else {
        expr
    }
}

fn input_stream_error_shape(ci: &ComponentInterface, error_type: &Type) -> &'static str {
    match error_type {
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
fn ts_type(ty: &Type, config: &JsConfig) -> String {
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
        Type::Record { name, .. } | Type::Enum { name, .. } => name.clone(),
        Type::Object { name, .. } => name.clone(),
        Type::Optional { inner_type } => format!("{} | null", ts_type(inner_type, config)),
        Type::Sequence { inner_type } => format!("Array<{}>", ts_type(inner_type, config)),
        Type::Map {
            key_type,
            value_type,
        } => format!(
            "Record<{}, {}>",
            ts_type(key_type, config),
            ts_type(value_type, config)
        ),
        Type::Box { inner_type } => ts_type(inner_type, config),
        Type::Set { inner_type } => format!("Set<{}>", ts_type(inner_type, config)),
        Type::Stream { item_type, .. } => {
            format!("AsyncIterable<{}>", ts_type(item_type, config))
        }
        Type::InputStream { item_type, .. } => {
            format!("AsyncIterable<{}>", ts_type(item_type, config))
        }
        Type::Timestamp => "Date".to_string(),
        Type::Duration => "number".to_string(),
        Type::CallbackInterface { name, .. } => name.clone(),
        Type::Custom { name, builtin, .. } => config
            .custom_type(name)
            .map(|_| name.clone())
            .unwrap_or_else(|| ts_type(builtin, config)),
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
    /// Every named type (record/enum/error/object/callback/trait) touched.
    named: BTreeSet<String>,
    /// Object names appearing as return types — their class value (not
    /// just the type) must be imported because `render_free_function`
    /// emits `{Name}.__fromHandle(...)`.
    objects_in_ret: BTreeSet<String>,
    customs: BTreeSet<String>,
    seen_arg_payloads: BTreeSet<String>,
}

impl Usage {
    fn see(&mut self, ci: &ComponentInterface, ty: &Type, pos: UsagePos, config: &JsConfig) {
        match ty {
            Type::Int64 => {
                if matches!(pos, UsagePos::Arg) {
                    self.needs_to_i64 = true;
                }
                // Return: bigint passes through, no fromI64 needed.
            }
            Type::UInt64 => {
                if matches!(pos, UsagePos::Arg) {
                    self.needs_to_u64 = true;
                }
            }
            Type::Object {
                name,
                imp: ObjectImpl::Trait(TraitKind::ForeignOnly),
                ..
            } => {
                let _ = pos;
                self.named.insert(name.clone());
            }
            Type::CallbackInterface { name, .. } => {
                self.named.insert(name.clone());
            }
            Type::Object { name, .. } => {
                self.named.insert(name.clone());
                if matches!(pos, UsagePos::Ret) {
                    self.objects_in_ret.insert(name.clone());
                }
            }
            Type::Record { name, .. } | Type::Enum { name, .. } => {
                self.named.insert(name.clone());
                if matches!(pos, UsagePos::Arg) {
                    self.see_named_payloads(ci, name, config);
                }
            }
            Type::Custom { name, builtin, .. } => {
                if config.custom_type(name).is_some() {
                    self.customs.insert(name.clone());
                }
                self.see(ci, builtin, pos, config);
            }
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
                self.see(ci, inner_type, pos, config)
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                self.see(ci, key_type, pos, config);
                self.see(ci, value_type, pos, config);
            }
            Type::Stream { item_type, .. } => {
                self.needs_stream = true;
                self.see(ci, item_type, pos, config);
            }
            Type::InputStream {
                item_type,
                error_type,
                ..
            } => {
                self.needs_input_stream = true;
                self.see(ci, item_type, pos, config);
                self.see(ci, error_type, pos, config);
            }
            _ => {}
        }
    }

    fn see_named_payloads(&mut self, ci: &ComponentInterface, name: &str, config: &JsConfig) {
        if !self.seen_arg_payloads.insert(name.to_string()) {
            return;
        }

        if let Some(record) = ci
            .record_definitions()
            .iter()
            .find(|record| record.name() == name)
        {
            for field in record.fields() {
                self.see_lower_helpers(ci, &field.as_type(), config);
            }
            return;
        }

        if let Some(enum_) = ci
            .enum_definitions()
            .iter()
            .find(|enum_| enum_.name() == name)
        {
            for variant in enum_.variants() {
                for field in variant.fields() {
                    self.see_lower_helpers(ci, &field.as_type(), config);
                }
            }
        }
    }

    fn see_lower_helpers(&mut self, ci: &ComponentInterface, ty: &Type, config: &JsConfig) {
        match ty {
            Type::Int64 => self.needs_to_i64 = true,
            Type::UInt64 => self.needs_to_u64 = true,
            Type::Custom { name, builtin, .. } => {
                if config.custom_type(name).is_some() {
                    self.customs.insert(name.clone());
                }
                self.see_lower_helpers(ci, builtin, config);
            }
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
                self.see_lower_helpers(ci, inner_type, config)
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                self.see_lower_helpers(ci, key_type, config);
                self.see_lower_helpers(ci, value_type, config);
            }
            Type::Record { name, .. } | Type::Enum { name, .. } => {
                self.see_named_payloads(ci, name, config);
            }
            Type::Stream { item_type, .. } => {
                self.needs_stream = true;
                self.see_lower_helpers(ci, item_type, config);
            }
            Type::InputStream {
                item_type,
                error_type,
                ..
            } => {
                self.needs_input_stream = true;
                self.see_lower_helpers(ci, item_type, config);
                self.see_lower_helpers(ci, error_type, config);
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
fn render_public_types(ci: &ComponentInterface, config: &JsConfig) -> String {
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
    // Always re-export UniffiError so downstream can catch it.
    writeln!(out, "export {{ UniffiError }} from \"./runtime.ts\";").unwrap();
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

    let customs: Vec<String> = all_custom_types(ci, config)
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

fn all_custom_types(ci: &ComponentInterface, config: &JsConfig) -> BTreeSet<(String, Type)> {
    let mut customs = BTreeSet::new();
    for record in ci.record_definitions() {
        for field in record.fields() {
            collect_custom_types(&field.as_type(), config, &mut customs);
        }
        for constructor in record.constructors() {
            for arg in constructor.arguments() {
                collect_custom_types(&arg.as_type(), config, &mut customs);
            }
        }
        collect_custom_types(&record.as_type(), config, &mut customs);
        for method in record.methods() {
            for arg in method.arguments() {
                collect_custom_types(&arg.as_type(), config, &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(ret, config, &mut customs);
            }
        }
    }
    for enum_ in ci.enum_definitions() {
        for variant in enum_.variants() {
            for field in variant.fields() {
                collect_custom_types(&field.as_type(), config, &mut customs);
            }
        }
        for constructor in enum_.constructors() {
            for arg in constructor.arguments() {
                collect_custom_types(&arg.as_type(), config, &mut customs);
            }
        }
        collect_custom_types(&enum_.as_type(), config, &mut customs);
        for method in enum_.methods() {
            for arg in method.arguments() {
                collect_custom_types(&arg.as_type(), config, &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(ret, config, &mut customs);
            }
        }
    }
    for object in ci.object_definitions() {
        for constructor in object.constructors() {
            for arg in constructor.arguments() {
                collect_custom_types(&arg.as_type(), config, &mut customs);
            }
        }
        for method in object.methods() {
            for arg in method.arguments() {
                collect_custom_types(&arg.as_type(), config, &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(ret, config, &mut customs);
            }
        }
    }
    for callback in ci.callback_interface_definitions() {
        for method in callback.methods() {
            for arg in method.arguments() {
                collect_custom_types(&arg.as_type(), config, &mut customs);
            }
            if let Some(ret) = method.return_type() {
                collect_custom_types(ret, config, &mut customs);
            }
        }
    }
    for function in ci.function_definitions() {
        for arg in function.arguments() {
            collect_custom_types(&arg.as_type(), config, &mut customs);
        }
        if let Some(ret) = function.return_type() {
            collect_custom_types(ret, config, &mut customs);
        }
    }
    customs
}

fn collect_custom_types(ty: &Type, config: &JsConfig, customs: &mut BTreeSet<(String, Type)>) {
    match ty {
        Type::Custom { name, builtin, .. } => {
            if config.custom_type(name).is_some() {
                customs.insert((name.clone(), builtin.as_ref().clone()));
            }
            collect_custom_types(builtin, config, customs);
        }
        Type::Optional { inner_type }
        | Type::Sequence { inner_type }
        | Type::Stream {
            item_type: inner_type,
            ..
        } => collect_custom_types(inner_type, config, customs),
        Type::InputStream {
            item_type,
            error_type,
            ..
        } => {
            collect_custom_types(item_type, config, customs);
            collect_custom_types(error_type, config, customs);
        }
        Type::Map {
            key_type,
            value_type,
        } => {
            collect_custom_types(key_type, config, customs);
            collect_custom_types(value_type, config, customs);
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

fn function_custom_helpers(ci: &ComponentInterface, config: &JsConfig) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for function in ci.function_definitions() {
        for arg in function.arguments() {
            collect_public_customs(ci, config, &arg.as_type(), &mut names, &mut HashSet::new());
        }
        if let Some(ret) = function.return_type() {
            collect_public_customs(ci, config, ret, &mut names, &mut HashSet::new());
        }
    }
    names
}

fn object_custom_helpers(ci: &ComponentInterface, config: &JsConfig) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for object in ci.object_definitions() {
        if matches!(
            object.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            continue;
        }
        for constructor in object.constructors() {
            for arg in constructor.arguments() {
                collect_public_customs(ci, config, &arg.as_type(), &mut names, &mut HashSet::new());
            }
        }
        for method in object.methods() {
            for arg in method.arguments() {
                collect_public_customs(ci, config, &arg.as_type(), &mut names, &mut HashSet::new());
            }
            if let Some(ret) = method.return_type() {
                collect_public_customs(ci, config, ret, &mut names, &mut HashSet::new());
            }
        }
    }
    names
}

fn value_type_custom_helpers(
    ci: &ComponentInterface,
    config: &JsConfig,
    local_module: &str,
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    match local_module {
        "records" => {
            for record in ci.record_definitions() {
                if record.constructors().is_empty() && record.methods().is_empty() {
                    continue;
                }
                collect_public_customs(
                    ci,
                    config,
                    &record.as_type(),
                    &mut names,
                    &mut HashSet::new(),
                );
                for constructor in record.constructors() {
                    for arg in constructor.arguments() {
                        collect_public_customs(
                            ci,
                            config,
                            &arg.as_type(),
                            &mut names,
                            &mut HashSet::new(),
                        );
                    }
                }
                for method in record.methods() {
                    for arg in method.arguments() {
                        collect_public_customs(
                            ci,
                            config,
                            &arg.as_type(),
                            &mut names,
                            &mut HashSet::new(),
                        );
                    }
                    if let Some(ret) = method.return_type() {
                        collect_public_customs(ci, config, ret, &mut names, &mut HashSet::new());
                    }
                }
            }
        }
        "enums" => {
            for enum_ in ci
                .enum_definitions()
                .iter()
                .filter(|e| !ci.is_name_used_as_error(e.name()))
            {
                if enum_.constructors().is_empty() && enum_.methods().is_empty() {
                    continue;
                }
                collect_public_customs(
                    ci,
                    config,
                    &enum_.as_type(),
                    &mut names,
                    &mut HashSet::new(),
                );
                for constructor in enum_.constructors() {
                    for arg in constructor.arguments() {
                        collect_public_customs(
                            ci,
                            config,
                            &arg.as_type(),
                            &mut names,
                            &mut HashSet::new(),
                        );
                    }
                }
                for method in enum_.methods() {
                    for arg in method.arguments() {
                        collect_public_customs(
                            ci,
                            config,
                            &arg.as_type(),
                            &mut names,
                            &mut HashSet::new(),
                        );
                    }
                    if let Some(ret) = method.return_type() {
                        collect_public_customs(ci, config, ret, &mut names, &mut HashSet::new());
                    }
                }
            }
        }
        _ => {}
    }
    names
}

fn function_callback_helpers(ci: &ComponentInterface) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for function in ci.function_definitions() {
        for arg in function.arguments() {
            collect_callback_helpers(&arg.as_type(), &mut names);
        }
    }
    names
}

fn object_callback_helpers(ci: &ComponentInterface) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for object in ci.object_definitions() {
        if matches!(
            object.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        ) {
            continue;
        }
        for constructor in object.constructors() {
            for arg in constructor.arguments() {
                collect_callback_helpers(&arg.as_type(), &mut names);
            }
        }
        for method in object.methods() {
            for arg in method.arguments() {
                collect_callback_helpers(&arg.as_type(), &mut names);
            }
        }
    }
    names
}

fn value_type_callback_helpers(ci: &ComponentInterface, local_module: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    match local_module {
        "records" => {
            for record in ci.record_definitions() {
                for constructor in record.constructors() {
                    for arg in constructor.arguments() {
                        collect_callback_helpers(&arg.as_type(), &mut names);
                    }
                }
                for method in record.methods() {
                    for arg in method.arguments() {
                        collect_callback_helpers(&arg.as_type(), &mut names);
                    }
                }
            }
        }
        "enums" => {
            for enum_ in ci
                .enum_definitions()
                .iter()
                .filter(|e| !ci.is_name_used_as_error(e.name()))
            {
                for constructor in enum_.constructors() {
                    for arg in constructor.arguments() {
                        collect_callback_helpers(&arg.as_type(), &mut names);
                    }
                }
                for method in enum_.methods() {
                    for arg in method.arguments() {
                        collect_callback_helpers(&arg.as_type(), &mut names);
                    }
                }
            }
        }
        _ => {}
    }
    names
}

fn callback_custom_helpers(ci: &ComponentInterface, config: &JsConfig) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for object in ci.object_definitions().iter().filter(|obj| {
        matches!(
            obj.imp(),
            ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly)
        )
    }) {
        for method in object.methods() {
            for arg in method.arguments() {
                collect_public_customs(ci, config, &arg.as_type(), &mut names, &mut HashSet::new());
            }
            if let Some(ret) = method.return_type() {
                collect_public_customs(ci, config, ret, &mut names, &mut HashSet::new());
            }
        }
    }
    names
}

fn collect_callback_helpers(ty: &Type, out: &mut BTreeSet<String>) {
    match ty {
        Type::Object {
            name,
            imp: ObjectImpl::Trait(TraitKind::Both | TraitKind::ForeignOnly),
            ..
        }
        | Type::CallbackInterface { name, .. } => {
            out.insert(name.clone());
        }
        Type::Optional { inner_type } | Type::Sequence { inner_type } => {
            collect_callback_helpers(inner_type, out)
        }
        Type::Map {
            key_type,
            value_type,
        } => {
            collect_callback_helpers(key_type, out);
            collect_callback_helpers(value_type, out);
        }
        _ => {}
    }
}

fn collect_public_customs(
    ci: &ComponentInterface,
    config: &JsConfig,
    ty: &Type,
    out: &mut BTreeSet<String>,
    seen_named: &mut HashSet<String>,
) {
    match ty {
        Type::Custom { name, builtin, .. } => {
            if config.custom_type(name).is_some() {
                out.insert(name.clone());
            }
            collect_public_customs(ci, config, builtin, out, seen_named);
        }
        Type::Optional { inner_type }
        | Type::Sequence { inner_type }
        | Type::Stream {
            item_type: inner_type,
            ..
        } => collect_public_customs(ci, config, inner_type, out, seen_named),
        Type::InputStream {
            item_type,
            error_type,
            ..
        } => {
            collect_public_customs(ci, config, item_type, out, seen_named);
            collect_public_customs(ci, config, error_type, out, seen_named);
        }
        Type::Map {
            key_type,
            value_type,
        } => {
            collect_public_customs(ci, config, key_type, out, seen_named);
            collect_public_customs(ci, config, value_type, out, seen_named);
        }
        Type::Record { name, .. } => {
            if seen_named.insert(format!("record:{name}")) {
                if let Some(record) = ci.record_definitions().iter().find(|r| r.name() == name) {
                    for field in record.fields() {
                        collect_public_customs(ci, config, &field.as_type(), out, seen_named);
                    }
                }
            }
        }
        Type::Enum { name, .. } => {
            if seen_named.insert(format!("enum:{name}")) {
                if let Some(enum_) = ci.enum_definitions().iter().find(|e| e.name() == name) {
                    for variant in enum_.variants() {
                        for field in variant.fields() {
                            collect_public_customs(ci, config, &field.as_type(), out, seen_named);
                        }
                    }
                }
            }
        }
        _ => {}
    }
}

fn ts_lower_enum(
    ci: &ComponentInterface,
    config: &JsConfig,
    name: &str,
    ident: &str,
    depth: usize,
) -> String {
    let enum_ = ci
        .enum_definitions()
        .iter()
        .find(|enum_| enum_.name() == name)
        .expect("enum should resolve");
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
                        ci,
                        config,
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

fn ts_lift_enum(
    ci: &ComponentInterface,
    config: &JsConfig,
    name: &str,
    ident: &str,
    depth: usize,
) -> String {
    let enum_ = ci
        .enum_definitions()
        .iter()
        .find(|enum_| enum_.name() == name)
        .expect("enum should resolve");
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
                        ci,
                        config,
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
