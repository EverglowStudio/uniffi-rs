/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared callback-trait metadata helpers for JavaScript targets.
//!
//! These helpers keep the high-level TS callback marker and the backend
//! callback bridges aligned without introducing a larger JS backend IR.

use std::collections::BTreeSet;

use uniffi_bindgen::interface::{ComponentInterface, ObjectImpl, Type};

pub(crate) fn is_callback_return_type(ty: &Type) -> bool {
    matches!(
        ty,
        Type::Object {
            imp: ObjectImpl::CallbackTrait,
            ..
        } | Type::CallbackInterface { .. }
    )
}

pub(crate) fn contains_callback_return_type(ty: &Type) -> bool {
    match ty {
        Type::Object {
            imp: ObjectImpl::CallbackTrait,
            ..
        }
        | Type::CallbackInterface { .. } => true,
        Type::Optional { inner_type } | Type::Sequence { inner_type } => {
            contains_callback_return_type(inner_type)
        }
        Type::Map { value_type, .. } => contains_callback_return_type(value_type),
        _ => false,
    }
}

pub(crate) fn callback_error_enum_names(ci: &ComponentInterface) -> BTreeSet<String> {
    ci.callback_interface_definitions()
        .iter()
        .flat_map(|callback| callback.methods())
        .chain(
            ci.object_definitions()
                .iter()
                .filter(|obj| matches!(obj.imp(), ObjectImpl::CallbackTrait))
                .flat_map(|obj| obj.methods()),
        )
        .filter_map(|method| match method.throws_type() {
            Some(Type::Enum { name, .. }) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_direct_and_nested_callback_returns() {
        let cb = Type::CallbackInterface {
            name: "Logger".to_string(),
            module_path: String::new(),
        };
        let nested = Type::Sequence {
            inner_type: Box::new(Type::Optional {
                inner_type: Box::new(cb.clone()),
            }),
        };
        let mapped = Type::Map {
            key_type: Box::new(Type::String),
            value_type: Box::new(cb.clone()),
        };

        assert!(is_callback_return_type(&cb));
        assert!(contains_callback_return_type(&cb));
        assert!(contains_callback_return_type(&nested));
        assert!(contains_callback_return_type(&mapped));
        assert!(!is_callback_return_type(&nested));
        assert!(!contains_callback_return_type(&Type::String));
    }
}
