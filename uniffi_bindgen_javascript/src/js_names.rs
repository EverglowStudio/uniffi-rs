/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared JavaScript surface naming helpers.

use heck::ToLowerCamelCase;

pub(crate) fn member_name(rust: &str) -> String {
    rust.to_lower_camel_case()
}

pub(crate) fn function_name(rust: &str) -> String {
    member_name(rust)
}

pub(crate) fn method_name(rust: &str) -> String {
    member_name(rust)
}

pub(crate) fn field_name(rust: &str) -> String {
    member_name(rust)
}

pub(crate) fn native_library_stem(namespace: &str) -> String {
    let mut out = String::new();
    for ch in namespace.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push_str("uniffi");
    }
    if out.starts_with(|ch: char| ch.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

pub(crate) fn ohos_native_library_stem(namespace: &str) -> String {
    format!("{}_ohos", native_library_stem(namespace))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_camel_names_match_public_contract() {
        assert_eq!(function_name("run_job"), "runJob");
        assert_eq!(method_name("slow_add"), "slowAdd");
        assert_eq!(field_name("created_at"), "createdAt");
    }

    #[test]
    fn native_library_stem_is_rust_lib_name_safe() {
        assert_eq!(native_library_stem("uni_core"), "uni_core");
        assert_eq!(native_library_stem("uni-core"), "uni_core");
        assert_eq!(native_library_stem("1core"), "_1core");
    }

    #[test]
    fn ohos_native_library_stem_avoids_core_cdylib_collision() {
        assert_eq!(ohos_native_library_stem("uni_core"), "uni_core_ohos");
        assert_eq!(ohos_native_library_stem("uni-core"), "uni_core_ohos");
        assert_eq!(ohos_native_library_stem("1core"), "_1core_ohos");
    }
}
