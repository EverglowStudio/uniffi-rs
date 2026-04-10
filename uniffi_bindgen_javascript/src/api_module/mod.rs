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

use std::collections::BTreeSet;

use anyhow::Result;
use camino::Utf8Path;
use fs_err as fs;
use heck::{ToLowerCamelCase, ToSnakeCase};
use uniffi_bindgen::{
    interface::{AsType, ComponentInterface, Enum, Function, Object, ObjectImpl, Record, Type},
    Component,
};

use crate::JsConfig;

/// The shared runtime module, shipped verbatim into every generated tree.
const RUNTIME_TS: &str =
    include_str!("../../../uniffi_runtime_javascript/typescript/src/runtime.ts");

pub fn emit(common_dir: &Utf8Path, component: &Component<JsConfig>) -> Result<()> {
    let ci = &component.ci;

    fs::write(common_dir.join("runtime.ts"), RUNTIME_TS)?;
    fs::write(common_dir.join("records.ts"), render_records(ci))?;
    fs::write(common_dir.join("enums.ts"), render_enums(ci))?;
    fs::write(common_dir.join("errors.ts"), render_errors(ci))?;
    fs::write(common_dir.join("callbacks.ts"), render_callbacks(ci))?;
    fs::write(common_dir.join("objects.ts"), render_objects(ci))?;
    fs::write(common_dir.join("api.ts"), render_api(ci))?;
    fs::write(common_dir.join("public-types.ts"), render_public_types(ci))?;
    Ok(())
}

// -----------------------------------------------------------------------
// records.ts
// -----------------------------------------------------------------------

fn render_records(ci: &ComponentInterface) -> String {
    let mut out = header("records");
    for record in ci.record_definitions() {
        out.push_str(&render_record(record));
        out.push('\n');
    }
    out
}

fn render_record(record: &Record) -> String {
    let mut s = format!("export interface {} {{\n", record.name());
    for f in record.fields() {
        s.push_str(&format!(
            "    {}: {};\n",
            js_field_name(f.name()),
            ts_type(&f.as_type())
        ));
    }
    s.push_str("}\n");
    s
}

// -----------------------------------------------------------------------
// enums.ts
// -----------------------------------------------------------------------

fn render_enums(ci: &ComponentInterface) -> String {
    let mut out = header("enums");
    for e in ci.enum_definitions() {
        if ci.is_name_used_as_error(e.name()) {
            // Error enums are emitted as classes in errors.ts instead.
            continue;
        }
        out.push_str(&render_enum(e));
        out.push('\n');
    }
    out
}

fn render_enum(e: &Enum) -> String {
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
        s.push_str("} as const;\n");
        s.push_str(&format!(
            "export type {name} = typeof {name}[keyof typeof {name}];\n",
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
                .map(|f| format!("{}: {}", js_field_name(f.name()), ts_type(&f.as_type())))
                .collect::<Vec<_>>()
                .join("; ");
            arms.push(format!("  | {{ tag: \"{}\"; {} }}", v.name(), fields));
        }
    }
    format!("export type {} =\n{};\n", e.name(), arms.join("\n"))
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

fn render_callbacks(ci: &ComponentInterface) -> String {
    let mut out = header("callbacks");
    for obj in ci.object_definitions() {
        if !matches!(obj.imp(), ObjectImpl::CallbackTrait) {
            continue;
        }
        out.push_str(&format!("export interface {} {{\n", obj.name()));
        for m in obj.methods() {
            let args = m
                .arguments()
                .iter()
                .map(|a| format!("{}: {}", js_field_name(a.name()), ts_type(&a.as_type())))
                .collect::<Vec<_>>()
                .join(", ");
            let ret = match m.return_type() {
                Some(t) => ts_type(t),
                None => "void".to_string(),
            };
            out.push_str(&format!(
                "    {}({}): {};\n",
                js_fn_name(m.name()),
                args,
                ret
            ));
        }
        out.push_str("}\n\n");
    }
    out
}

// -----------------------------------------------------------------------
// objects.ts
// -----------------------------------------------------------------------

fn render_objects(ci: &ComponentInterface) -> String {
    let mut out = header("objects");

    // Collect every type referenced by constructors/methods so we can
    // decide (a) which runtime helpers to import and (b) which named
    // types to bring into local scope explicitly.
    let mut usage = Usage::default();
    let mut any_object = false;
    let mut has_async = false;
    let mut has_sync = false;
    for obj in ci.object_definitions() {
        if matches!(obj.imp(), ObjectImpl::CallbackTrait) {
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
                usage.see(&a.as_type(), UsagePos::Arg);
            }
        }
        for m in obj.methods() {
            if m.is_async() {
                has_async = true;
            } else {
                has_sync = true;
            }
            for a in m.arguments() {
                usage.see(&a.as_type(), UsagePos::Arg);
            }
            if let Some(t) = m.return_type() {
                usage.see(t, UsagePos::Ret);
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
    out.push('\n');

    for obj in ci.object_definitions() {
        if matches!(obj.imp(), ObjectImpl::CallbackTrait) {
            continue;
        }
        out.push_str(&render_object_class(obj));
        out.push('\n');
    }
    out
}

fn render_object_class(obj: &Object) -> String {
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
        let fn_name = format!("{snake}_{}", c.name().to_snake_case());
        let (arg_decls, arg_pass) =
            lowered_args(c.arguments().iter().map(|a| (a.name(), a.as_type())));
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
        let fn_name = format!("{snake}_{}", m.name().to_snake_case());
        let (arg_decls, arg_pass) =
            lowered_args(m.arguments().iter().map(|a| (a.name(), a.as_type())));
        let comma_pass = if arg_pass.is_empty() { "" } else { ", " };
        let ret_ty = m.return_type();
        let ret_ts = match ret_ty {
            Some(t) => ts_type(t),
            None => "void".to_string(),
        };
        let lift_open = ret_ty.map(|t| lift_open(t)).unwrap_or_default();
        let lift_close = ret_ty.map(|t| lift_close(t)).unwrap_or_default();
        let call_g = call_generic(ret_ty);
        if m.is_async() {
            s.push_str(&format!(
                "    async {m_js}({arg_decls}): Promise<{ret_ts}> {{\n        \
                 return {lift_open}(await __callAsync<{call_g}>(\"{fn_name}\", this.__uniffi.raw{comma_pass}{arg_pass})){lift_close} as {ret_ts};\n    }}\n",
            ));
        } else {
            s.push_str(&format!(
                "    {m_js}({arg_decls}): {ret_ts} {{\n        \
                 return {lift_open}__call<{call_g}>(\"{fn_name}\", this.__uniffi.raw{comma_pass}{arg_pass}){lift_close} as {ret_ts};\n    }}\n",
            ));
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

fn render_api(ci: &ComponentInterface) -> String {
    let mut out = header("api");
    out.push_str(
        "export * from \"./records.ts\";\n\
         export * from \"./enums.ts\";\n\
         export * from \"./errors.ts\";\n\
         export * from \"./callbacks.ts\";\n\
         export * from \"./objects.ts\";\n\
         export {\n    \
         UniffiError,\n    \
         UNIFFI_JS_CONTRACT_VERSION,\n    \
         __installBackend,\n\
         } from \"./runtime.ts\";\n\n",
    );

    let mut usage = Usage::default();
    let mut has_async = false;
    let mut has_sync = false;
    for f in ci.function_definitions() {
        if f.is_async() {
            has_async = true;
        } else {
            has_sync = true;
        }
        for a in f.arguments() {
            usage.see(&a.as_type(), UsagePos::Arg);
        }
        if let Some(t) = f.return_type() {
            usage.see(t, UsagePos::Ret);
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
    out.push('\n');
    for f in ci.function_definitions() {
        out.push_str(&render_free_function(&f));
        out.push('\n');
    }
    out
}

fn render_free_function(f: &Function) -> String {
    let (arg_decls, arg_pass) = lowered_args(f.arguments().iter().map(|a| (a.name(), a.as_type())));
    let ret_ty = f.return_type();
    let ret_ts = match ret_ty {
        Some(t) => ts_type(t),
        None => "void".to_string(),
    };
    let lift_open = ret_ty.map(|t| lift_open(t)).unwrap_or_default();
    let lift_close = ret_ty.map(|t| lift_close(t)).unwrap_or_default();
    let call_g = call_generic(ret_ty);
    let rust_name = f.name();
    let js_name = js_fn_name(rust_name);
    if f.is_async() {
        format!(
            "export async function {js_name}({arg_decls}): Promise<{ret_ts}> {{\n    \
             return {lift_open}(await __callAsync<{call_g}>(\"{rust_name}\"{sep}{arg_pass})){lift_close} as {ret_ts};\n\
             }}\n",
            sep = if arg_pass.is_empty() { "" } else { ", " },
        )
    } else {
        format!(
            "export function {js_name}({arg_decls}): {ret_ts} {{\n    \
             return {lift_open}__call<{call_g}>(\"{rust_name}\"{sep}{arg_pass}){lift_close} as {ret_ts};\n\
             }}\n",
            sep = if arg_pass.is_empty() { "" } else { ", " },
        )
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
    rust.to_lower_camel_case()
}

fn js_field_name(rust: &str) -> String {
    rust.to_lower_camel_case()
}

/// Returns `(decls, pass)` where `decls` is the comma-separated TS
/// parameter list as seen by the caller (numbers for i64/u64) and `pass`
/// is the argument-passing expression that lowers each value into what
/// the backend expects (bigint for i64/u64).
fn lowered_args<'a, I>(args: I) -> (String, String)
where
    I: IntoIterator<Item = (&'a str, Type)>,
{
    let mut decls = Vec::new();
    let mut pass = Vec::new();
    for (raw_name, ty) in args {
        let js = js_field_name(raw_name);
        decls.push(format!("{js}: {}", ts_type(&ty)));
        pass.push(lower_expr(&ty, &js));
    }
    (decls.join(", "), pass.join(", "))
}

fn lower_expr(ty: &Type, ident: &str) -> String {
    match ty {
        Type::Int64 => format!("toI64({ident})"),
        Type::UInt64 => format!("toU64({ident})"),
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
            imp: ObjectImpl::CallbackTrait,
            ..
        }
        | Type::CallbackInterface { .. } => {
            format!("{{ __uniffiCallback: true, object: {ident} }}")
        }
        // Opaque objects: pass the u32 handle stored on the JS wrapper.
        Type::Object { .. } => format!("{ident}.__uniffi.raw"),
        _ => ident.to_string(),
    }
}

fn lift_open(ty: &Type) -> String {
    match ty {
        // i64/u64: the backend already returns `bigint`, so no wrapping.
        Type::Int64 | Type::UInt64 => String::new(),
        Type::Object { name, .. } => format!("{name}.__fromHandle("),
        _ => String::new(),
    }
}

fn lift_close(ty: &Type) -> String {
    match ty {
        Type::Int64 | Type::UInt64 => String::new(),
        Type::Object { .. } => ")".to_string(),
        _ => String::new(),
    }
}

/// Map a uniffi `Type` to its TypeScript surface type. `i64`/`u64` are
/// surfaced as `bigint` — the 64-bit integer contract is bigint-first
/// so there is no silent precision loss for values beyond
/// `Number.MAX_SAFE_INTEGER`. The generator inserts `toI64`/`toU64`
/// coercions on the argument side; return values are already `bigint`
/// from the backend and need no conversion.
fn ts_type(ty: &Type) -> String {
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
        Type::Optional { inner_type } => format!("{} | null", ts_type(inner_type)),
        Type::Sequence { inner_type } => format!("Array<{}>", ts_type(inner_type)),
        Type::Map {
            key_type,
            value_type,
        } => format!("Record<{}, {}>", ts_type(key_type), ts_type(value_type)),
        Type::Timestamp | Type::Duration => "unknown /* timestamp/duration TBD */".to_string(),
        Type::CallbackInterface { name, .. } => name.clone(),
        Type::Custom { name, .. } => format!("unknown /* custom: {name} */"),
    }
}

/// The TS generic passed to `__call<_>` / `__callAsync<_>`. For i64/u64
/// the backend yields `bigint`, which the high-level API exposes directly.
fn call_generic(ty: Option<&Type>) -> &'static str {
    match ty {
        Some(Type::Int64 | Type::UInt64) => "bigint",
        _ => "unknown",
    }
}

#[derive(Copy, Clone)]
enum UsagePos {
    Arg,
    Ret,
}

/// Scan over the types referenced by a set of function-like signatures
/// and record what runtime helpers and named types need to be imported.
#[derive(Default)]
struct Usage {
    needs_to_i64: bool,
    needs_to_u64: bool,
    needs_from_i64: bool,
    needs_from_u64: bool,
    /// Every named type (record/enum/error/object/callback/trait) touched.
    named: BTreeSet<String>,
    /// Object names appearing as return types — their class value (not
    /// just the type) must be imported because `render_free_function`
    /// emits `{Name}.__fromHandle(...)`.
    objects_in_ret: BTreeSet<String>,
}

impl Usage {
    fn see(&mut self, ty: &Type, pos: UsagePos) {
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
                imp: ObjectImpl::CallbackTrait,
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
            }
            Type::Optional { inner_type } | Type::Sequence { inner_type } => {
                self.see(inner_type, pos)
            }
            Type::Map {
                key_type,
                value_type,
            } => {
                self.see(key_type, pos);
                self.see(value_type, pos);
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
        } else if ci
            .object_definitions()
            .iter()
            .any(|o| o.name() == n && matches!(o.imp(), ObjectImpl::CallbackTrait))
        {
            g.callbacks.push(n.clone());
        } else if ci.object_definitions().iter().any(|o| o.name() == n) {
            g.objects.push(n.clone());
        } else if ci
            .callback_interface_definitions()
            .iter()
            .any(|c| c.name() == n)
        {
            g.callbacks.push(n.clone());
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

/// Emit a type-only facade that re-exports every public type from the
/// common API surface. Downstream UI code should import from this file
/// instead of reaching into `records.ts` / `enums.ts` / … directly.
fn render_public_types(ci: &ComponentInterface) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    writeln!(
        out,
        "// AUTOGENERATED by uniffi_bindgen_javascript — do not edit.\n\
         //\n\
         // Stable, type-only public contract for the `{}` component.\n\
         // Import from this file to get all high-level types without\n\
         // depending on implementation-detail modules.\n",
        ci.namespace()
    )
    .unwrap();

    // Records
    let records: Vec<String> = ci
        .record_definitions()
        .iter()
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
    let enums: Vec<String> = ci
        .enum_definitions()
        .iter()
        .filter(|e| !ci.is_name_used_as_error(e.name()))
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
    }
    // Always re-export UniffiError so downstream can catch it.
    writeln!(out, "export {{ UniffiError }} from \"./runtime.ts\";").unwrap();

    // Callbacks (callback interfaces + callback traits)
    let mut callbacks: Vec<String> = ci
        .callback_interface_definitions()
        .iter()
        .map(|c| c.name().to_string())
        .collect();
    for o in ci.object_definitions() {
        if matches!(o.imp(), ObjectImpl::CallbackTrait) {
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
        .filter(|o| !matches!(o.imp(), ObjectImpl::CallbackTrait))
        .map(|o| o.name().to_string())
        .collect();
    if !objects.is_empty() {
        writeln!(
            out,
            "export {{ {} }} from \"./objects.ts\";",
            join_sorted(&objects)
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
    }

    out
}
