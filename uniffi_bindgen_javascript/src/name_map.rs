/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared low-level-key → napi-export-name map.
//!
//! Problem: `common/api.ts` and `common/objects.ts` dispatch against
//! low-level `snake_case` keys that track the Rust source names:
//!
//! - free function: Rust name verbatim (`greet_with`, `slow_add`)
//! - constructor:   `{object_snake}_{ctor_snake}` (`counter_new`)
//! - method:        `{object_snake}_{method_snake}` (`counter_get`)
//!
//! But `napi-rs` rewrites every `#[napi] pub fn <snake>` into
//! `lowerCamelCase` on the JS side (`greetWith`, `counterNew`, …).
//! Before this module, both `backend-napi.ts` and `electron/preload.cjs`
//! indexed the addon directly by the low-level key and crashed with
//! `unknown uniffi method: counter_new` on first call.
//!
//! Fix: walk the same IR shape `api_module/` walks, emit the exact
//! low-level keys it emits, and pair each with its `napi-rs` export
//! name. Both the node backend and the electron preload consume this
//! map via [`render_name_map_js_literal`], so they can never drift from
//! `common/api.ts`'s view of the dispatch key.
//!
//! Scope: functions that actually exist on the napi addon. Object
//! destructors (`__uniffi_<obj>_object_free`) are NOT included — napi
//! manages class lifetime through Rust `Drop`, and the electron bridge
//! handles `dispose` via its `drop` message kind, not through
//! `addon[method]`.

use heck::{ToLowerCamelCase, ToSnakeCase};
use uniffi_bindgen::ComponentInterface;

/// `snake_case` → `lowerCamelCase`. Pure string transform; no IR.
///
/// Kept as a free function so both the generator and the test suite
/// can pin representative conversions directly in code.
pub fn snake_to_camel(snake: &str) -> String {
    // `heck`'s `to_lower_camel_case` already handles:
    //   add              -> add
    //   greet_with       -> greetWith
    //   slow_add         -> slowAdd
    //   counter_new      -> counterNew
    //   counter_with_initial -> counterWithInitial
    //   run_job          -> runJob
    //
    // Going through `to_snake_case` first normalises input that is
    // already camel/Pascal/mixed, so the function is idempotent.
    snake.to_snake_case().to_lower_camel_case()
}

/// Collect every `(low_level_key, napi_export_name)` pair the generated
/// node/electron backends will need to look up. Sorted and deduplicated
/// so the emitted literal is stable across runs.
pub fn collect(ci: &ComponentInterface) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = Vec::new();

    // Free functions — `api_module/mod.rs` emits `f.name()` verbatim.
    for f in ci.function_definitions() {
        let key = f.name().to_string();
        pairs.push((key.clone(), snake_to_camel(&key)));
    }

    // Value-type constructors/methods — `api_module/mod.rs` emits
    // `{type_snake}_{member_snake}`.
    for record in ci.record_definitions() {
        let record_snake = record.name().to_snake_case();
        for c in record.constructors() {
            let key = format!("{record_snake}_{}", c.name().to_snake_case());
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
        for m in record.methods() {
            let key = format!("{record_snake}_{}", m.name().to_snake_case());
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
    }
    for enum_ in ci.enum_definitions() {
        if ci.is_name_used_as_error(enum_.name()) {
            continue;
        }
        let enum_snake = enum_.name().to_snake_case();
        for c in enum_.constructors() {
            let key = format!("{enum_snake}_{}", c.name().to_snake_case());
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
        for m in enum_.methods() {
            let key = format!("{enum_snake}_{}", m.name().to_snake_case());
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
    }

    // Object constructors & methods — `api_module/mod.rs` emits
    // `{obj_snake}_{member_snake}`.
    for obj in ci.object_definitions() {
        let obj_snake = obj.name().to_snake_case();
        for c in obj.constructors() {
            let key = format!("{obj_snake}_{}", c.name().to_snake_case());
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
        for m in obj.methods() {
            let key = format!("{obj_snake}_{}", m.name().to_snake_case());
            pairs.push((key.clone(), snake_to_camel(&key)));
        }
    }

    pairs.sort();
    pairs.dedup();
    pairs
}

/// Emit the map as a JS object literal (`{ "counter_new": "counterNew", ... }`).
/// Used by both `backend-napi.ts` and `electron/preload.cjs` so they
/// share exactly one source of truth.
pub fn render_name_map_js_literal(pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return "{}".to_string();
    }
    let mut out = String::from("{\n");
    for (k, v) in pairs {
        // Keys and values are identifiers drawn from uniffi IR names;
        // they cannot contain quotes or backslashes, so simple
        // interpolation is safe.
        out.push_str(&format!("    \"{k}\": \"{v}\",\n"));
    }
    out.push_str("}");
    out
}

#[cfg(test)]
mod tests {
    use super::snake_to_camel;

    #[test]
    fn converts_representative_dispatch_keys() {
        assert_eq!(snake_to_camel("add"), "add");
        assert_eq!(snake_to_camel("greet_with"), "greetWith");
        assert_eq!(snake_to_camel("slow_add"), "slowAdd");
        assert_eq!(snake_to_camel("counter_new"), "counterNew");
        assert_eq!(snake_to_camel("counter_with_initial"), "counterWithInitial");
        assert_eq!(snake_to_camel("counter_get"), "counterGet");
        assert_eq!(snake_to_camel("greeter_greet"), "greeterGreet");
        assert_eq!(snake_to_camel("run_job"), "runJob");
    }
}
