/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Shared, source-name lifecycle declarations. Validate before language renaming.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::{interface::Type, ComponentInterface, Method};

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LifecycleConfig {
    pub close_methods: BTreeMap<String, String>,
    pub owned_methods: BTreeSet<String>,
    pub cached_methods: BTreeSet<String>,
}

impl LifecycleConfig {
    pub fn from_root_toml(root: &toml::Value) -> Result<Self> {
        match root
            .get("bindings")
            .and_then(|bindings| bindings.get("lifecycle"))
        {
            Some(value) => value
                .clone()
                .try_into()
                .context("invalid [bindings.lifecycle]"),
            None => Ok(Self::default()),
        }
    }

    pub fn close_method(&self, object: &str) -> Option<&str> {
        self.close_methods.get(object).map(String::as_str)
    }

    pub fn is_owned(&self, object: &str, method: &str) -> bool {
        self.owned_methods.contains(&format!("{object}.{method}"))
    }

    pub fn is_cached(&self, object: &str, method: &str) -> bool {
        self.cached_methods.contains(&format!("{object}.{method}"))
    }

    pub fn validate(&self, ci: &ComponentInterface) -> Result<()> {
        for (object, method) in &self.close_methods {
            let name = format!("{object}.{method}");
            let method = Self::resolve_method(ci, &name)?;
            if !method.is_async()
                || !method.arguments().is_empty()
                || method.return_type().is_some()
            {
                bail!("lifecycle close method {name} must be async, take no arguments, and return unit or Result<unit, E>");
            }
        }
        for name in &self.owned_methods {
            let method = Self::resolve_method(ci, name)?;
            if !matches!(method.return_type(), Some(Type::Object { .. })) {
                bail!("lifecycle owned method {name} must return an object");
            }
        }
        for name in &self.cached_methods {
            if !self.owned_methods.contains(name) {
                bail!("lifecycle cached method {name} must also be an owned method");
            }
            let method = Self::resolve_method(ci, name)?;
            if method.is_async() || !method.arguments().is_empty() {
                bail!("lifecycle cached method {name} must be synchronous and take no arguments");
            }
        }
        Ok(())
    }

    /// The JNI generator uses the initial pipeline IR instead of ComponentInterface.
    /// Run this before its general pipeline applies exclusions or language renames.
    pub fn validate_initial(&self, namespace: &crate::pipeline::initial::Namespace) -> Result<()> {
        use crate::pipeline::initial::{Method, Type, TypeDefinition};
        let resolve = |name: &str| -> Result<&Method> {
            let (object, method) = name.split_once('.').context("expected Object.method")?;
            namespace
                .type_definitions
                .iter()
                .find_map(|definition| {
                    let TypeDefinition::Interface(interface) = definition else {
                        return None;
                    };
                    (interface.name == object)
                        .then(|| interface.methods.iter().find(|m| m.name == method))
                        .flatten()
                })
                .with_context(|| format!("unknown lifecycle method {name}"))
        };
        for (object, method) in &self.close_methods {
            let name = format!("{object}.{method}");
            let method = resolve(&name)?;
            if !method.is_async || !method.inputs.is_empty() || method.return_type.is_some() {
                bail!("lifecycle close method {name} must be async, take no arguments, and return unit or Result<unit, E>");
            }
        }
        for name in &self.owned_methods {
            if !matches!(resolve(name)?.return_type, Some(Type::Interface { .. })) {
                bail!("lifecycle owned method {name} must return an object");
            }
        }
        for name in &self.cached_methods {
            if !self.owned_methods.contains(name) {
                bail!("lifecycle cached method {name} must also be an owned method");
            }
            let method = resolve(name)?;
            if method.is_async || !method.inputs.is_empty() {
                bail!("lifecycle cached method {name} must be synchronous and take no arguments");
            }
        }
        Ok(())
    }

    fn resolve_method<'a>(ci: &'a ComponentInterface, name: &str) -> Result<&'a Method> {
        let (object, method) = name.split_once('.').with_context(|| {
            format!("lifecycle method {name} must use the source Object.method name")
        })?;
        ci.get_object_definition(object)
            .and_then(|object| {
                object
                    .methods()
                    .into_iter()
                    .find(|candidate| candidate.name() == method)
            })
            .with_context(|| format!("unknown lifecycle method {name}"))
    }

    /// Resolve once before renaming; FFI symbols survive language name changes.
    pub fn close_ffi_functions(&self, ci: &ComponentInterface) -> Result<BTreeSet<String>> {
        self.validate(ci)?;
        self.close_methods
            .iter()
            .map(|(object, method)| {
                Ok(Self::resolve_method(ci, &format!("{object}.{method}"))?
                    .ffi_func()
                    .name()
                    .to_owned())
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interface() -> ComponentInterface {
        ComponentInterface::from_webidl(
            r#"
            namespace lifecycle_test {};
            interface Service {};
            [Error] enum CloseError { "Failed" };
            interface Client {
                [Async, Throws=CloseError] void close();
                void sync_close();
                [Async] void close_arg(u32 value);
                [Async] u32 close_value();
                Service service();
                Service lookup(u32 value);
                [Async] Service async_service();
            };
        "#,
            "lifecycle_test",
        )
        .unwrap()
    }

    #[test]
    fn parses_and_validates_shared_source_contract() {
        let config = LifecycleConfig::from_root_toml(
            &toml::from_str(
                r#"
            [bindings.lifecycle]
            close_methods = { Client = "close" }
            owned_methods = ["Client.service", "Client.lookup"]
            cached_methods = ["Client.service"]
        "#,
            )
            .unwrap(),
        )
        .unwrap();
        let ci = interface();
        config.validate(&ci).unwrap();
        assert_eq!(config.close_method("Client"), Some("close"));
        assert!(config.is_owned("Client", "lookup"));
        assert!(config.is_cached("Client", "service"));
        assert_eq!(config.close_ffi_functions(&ci).unwrap().len(), 1);
    }

    #[test]
    fn rejects_invalid_close_and_cached_contracts() {
        let ci = interface();
        for method in ["sync_close", "close_arg", "close_value", "missing"] {
            let config = LifecycleConfig {
                close_methods: [("Client".into(), method.into())].into(),
                ..Default::default()
            };
            assert!(config.validate(&ci).is_err(), "{method}");
        }
        for method in ["lookup", "async_service"] {
            let name = format!("Client.{method}");
            let config = LifecycleConfig {
                owned_methods: [name.clone()].into(),
                cached_methods: [name].into(),
                ..Default::default()
            };
            assert!(config.validate(&ci).is_err(), "{method}");
        }
        let unowned_cache = LifecycleConfig {
            cached_methods: ["Client.service".into()].into(),
            ..Default::default()
        };
        assert!(unowned_cache.validate(&ci).is_err());
        let non_object = LifecycleConfig {
            owned_methods: ["Client.close_value".into()].into(),
            ..Default::default()
        };
        assert!(non_object.validate(&ci).is_err());
    }

    #[test]
    fn absent_table_defaults_but_unknown_keys_fail() {
        let empty = toml::Value::Table(Default::default());
        assert!(LifecycleConfig::from_root_toml(&empty)
            .unwrap()
            .close_methods
            .is_empty());
        let typo = toml::from_str("[bindings.lifecycle]\nclose_method = {}\n").unwrap();
        assert!(LifecycleConfig::from_root_toml(&typo).is_err());
    }
}
