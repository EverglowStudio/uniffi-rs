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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lower_camel_names_match_public_contract() {
        assert_eq!(function_name("run_job"), "runJob");
        assert_eq!(method_name("slow_add"), "slowAdd");
        assert_eq!(field_name("created_at"), "createdAt");
    }
}
