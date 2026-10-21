/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

mod callables;
mod callbacks;
mod compounds;
mod context;
mod custom;
mod defaults;
mod enums;
mod ffi_buffer;
mod ffi_types;
mod interfaces;
mod names;
mod nodes;
mod packages;
mod records;
mod root;
mod types;
mod utraits;

use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, bail, Result};
use context::Context;
use ffi_buffer::FfiBufferLayoutBuilder;
use ffi_types::FfiTypeOracle;
use heck::{ToLowerCamelCase, ToShoutySnakeCase, ToUpperCamelCase};
use indexmap::IndexSet;
use uniffi_bindgen::pipeline::{general, initial};
use uniffi_pipeline::{MapNode, Node, Pipeline};

use crate::config::{Config, CustomTypeConfig};
pub use initial::Root as InitialRoot;
pub use nodes::*;

pub fn pipeline() -> Pipeline<initial::Root, Root> {
    lifecycle_pipeline().pass::<Root, Context>(Context::default())
}

pub fn pipeline_for_scaffolding(crate_name: String) -> Pipeline<initial::Root, Root> {
    let context = Context {
        scaffolding_crate_name: Some(crate_name),
        ..Context::default()
    };

    lifecycle_pipeline().pass::<Root, Context>(context)
}

struct ValidateLifecycle;

impl MapNode<initial::Root, ValidateLifecycle> for initial::Root {
    fn map_node(self, _: &ValidateLifecycle) -> Result<Self> {
        for namespace in self.namespaces.values() {
            if let Some(config) = &namespace.config_toml {
                uniffi_bindgen::LifecycleConfig::from_root_toml(&toml::from_str(config)?)?
                    .validate_initial(namespace)?;
            }
        }
        Ok(self)
    }
}

fn lifecycle_pipeline() -> Pipeline<initial::Root, general::Root> {
    uniffi_pipeline::new_pipeline()
        .pass::<initial::Root, _>(ValidateLifecycle)
        .pass::<general::Root, _>(general::Context::new("kotlin"))
}
