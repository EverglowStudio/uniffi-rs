/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared `tag` ↔ `type` shape helpers for the napi + electron edge.
//!
//! Problem: `common/enums.ts` emits enum values with a `tag`
//! discriminant (`{ tag: "Click", ... }`), but `napi-rs` ships every
//! `#[napi(discriminant = "type")]` enum using `type`. Nothing on the
//! boundary was translating between the two, so Electron renderer code
//! round-tripped `{ tag: "Circle" }` into the addon and got
//! `UniffiError: Missing field 'type'`, and addon return values came
//! back as `{ type: "Click" }` which `common/enums.ts` consumers did
//! not recognise.
//!
//! Fix: at the backend boundary (the napi backend Proxy and the
//! electron preload `dispatch*`), deep-walk every value crossing in or
//! out and swap the discriminant key.
//!
//! Scope & limitation: this is a minimal universal rename on plain
//! objects. Any plain object with own key `tag` (and no `type`) is
//! lowered to `type`; lifted symmetrically. It does NOT consult enum
//! type names from the IR — doing so would require per-argument shape
//! tracking across sequences / options / records. The universal rule is
//! acceptable because the generated JS contract already reserves `tag`
//! and `type` for enum discriminants. Class instances and opaque object
//! handles are left untouched (we only rewrite objects whose prototype
//! is `Object.prototype` or null).
//!
//! Covered:
//! - enum arg / return (payload, unit, errors-with-data)
//! - enum nested inside sequence / optional / record
//! - thrown errors (napi throws objects whose `.error` has `type`)
//!
//! Both backend adapters include the same literal string below so
//! there is exactly one source of truth. The helper is plain JS (no
//! TS types), so it works unchanged inside `preload.cjs`.

/// TypeScript flavour of the helpers for `backend-napi.ts`. Strict-mode
/// clean: everything is typed `unknown`.
pub fn helper_ts() -> &'static str {
    r#"// ---- enum shape bridge (tag <-> type) -----------------------------
// See uniffi_bindgen_javascript/src/enum_shape.rs for the rationale.
function __uniffiIsPlainObject(v: unknown): v is Record<string, unknown> {
    if (v === null || typeof v !== "object") return false;
    if (Array.isArray(v)) return false;
    const proto = Object.getPrototypeOf(v);
    return proto === Object.prototype || proto === null;
}
function __uniffiLowerShape(v: unknown): unknown {
    if (Array.isArray(v)) return v.map(__uniffiLowerShape);
    if (__uniffiIsPlainObject(v)) {
        const out: Record<string, unknown> = {};
        const hasType = Object.prototype.hasOwnProperty.call(v, "type");
        for (const k of Object.keys(v)) {
            const nk = k === "tag" && !hasType ? "type" : k;
            out[nk] = __uniffiLowerShape((v as Record<string, unknown>)[k]);
        }
        return out;
    }
    return v;
}
function __uniffiLiftShape(v: unknown): unknown {
    if (Array.isArray(v)) return v.map(__uniffiLiftShape);
    if (__uniffiIsPlainObject(v)) {
        const out: Record<string, unknown> = {};
        const hasTag = Object.prototype.hasOwnProperty.call(v, "tag");
        for (const k of Object.keys(v)) {
            const nk = k === "type" && !hasTag ? "tag" : k;
            out[nk] = __uniffiLiftShape((v as Record<string, unknown>)[k]);
        }
        return out;
    }
    return v;
}
// -------------------------------------------------------------------
"#
}

/// Plain-JS flavour for `preload.cjs` (no TS syntax).
pub fn helper_js() -> &'static str {
    r#"// ---- enum shape bridge (tag <-> type) -----------------------------
// See uniffi_bindgen_javascript/src/enum_shape.rs for the rationale.
function __uniffiIsPlainObject(v) {
    if (v === null || typeof v !== "object") return false;
    if (Array.isArray(v)) return false;
    var proto = Object.getPrototypeOf(v);
    return proto === Object.prototype || proto === null;
}
function __uniffiLowerShape(v) {
    if (Array.isArray(v)) {
        var outA = new Array(v.length);
        for (var i = 0; i < v.length; i++) outA[i] = __uniffiLowerShape(v[i]);
        return outA;
    }
    if (__uniffiIsPlainObject(v)) {
        var out = {};
        var keys = Object.keys(v);
        var hasType = Object.prototype.hasOwnProperty.call(v, "type");
        for (var j = 0; j < keys.length; j++) {
            var k = keys[j];
            var nk = (k === "tag" && !hasType) ? "type" : k;
            out[nk] = __uniffiLowerShape(v[k]);
        }
        return out;
    }
    return v;
}
function __uniffiLiftShape(v) {
    if (Array.isArray(v)) {
        var outA = new Array(v.length);
        for (var i = 0; i < v.length; i++) outA[i] = __uniffiLiftShape(v[i]);
        return outA;
    }
    if (__uniffiIsPlainObject(v)) {
        var out = {};
        var keys = Object.keys(v);
        var hasTag = Object.prototype.hasOwnProperty.call(v, "tag");
        for (var j = 0; j < keys.length; j++) {
            var k = keys[j];
            var nk = (k === "type" && !hasTag) ? "tag" : k;
            out[nk] = __uniffiLiftShape(v[k]);
        }
        return out;
    }
    return v;
}
// -------------------------------------------------------------------
"#
}
