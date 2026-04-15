/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

pub fn map_type(ty: uniffi_meta::Type, context: &Context) -> Result<Type> {
    Ok(match ty {
        uniffi_meta::Type::UInt8 => Type::UInt8,
        uniffi_meta::Type::Int8 => Type::Int8,
        uniffi_meta::Type::UInt16 => Type::UInt16,
        uniffi_meta::Type::Int16 => Type::Int16,
        uniffi_meta::Type::UInt32 => Type::UInt32,
        uniffi_meta::Type::Int32 => Type::Int32,
        uniffi_meta::Type::UInt64 => Type::UInt64,
        uniffi_meta::Type::Int64 => Type::Int64,
        uniffi_meta::Type::Float32 => Type::Float32,
        uniffi_meta::Type::Float64 => Type::Float64,
        uniffi_meta::Type::Boolean => Type::Boolean,
        uniffi_meta::Type::String => Type::String,
        uniffi_meta::Type::Bytes => Type::Bytes,
        uniffi_meta::Type::Timestamp => Type::Timestamp,
        uniffi_meta::Type::Duration => Type::Duration,
        uniffi_meta::Type::Optional { inner_type } => Type::Optional {
            inner_type: inner_type.map_node(context)?,
        },
        uniffi_meta::Type::Sequence { inner_type } => Type::Sequence {
            inner_type: inner_type.map_node(context)?,
        },
        uniffi_meta::Type::Map {
            key_type,
            value_type,
        } => Type::Map {
            key_type: key_type.map_node(context)?,
            value_type: value_type.map_node(context)?,
        },
        uniffi_meta::Type::Stream {
            item_type,
            error_type,
            is_send,
        } => Type::Stream {
            item_type: item_type.map_node(context)?,
            error_type: error_type.map_node(context)?,
            is_send,
        },
        uniffi_meta::Type::InputStream {
            item_type,
            error_type,
            is_send,
        } => Type::InputStream {
            item_type: item_type.map_node(context)?,
            error_type: error_type.map_node(context)?,
            is_send,
        },
        uniffi_meta::Type::Object {
            module_path,
            name,
            imp,
        } => Type::Interface {
            namespace: context.get_namespace_name(&module_path)?,
            name,
            imp: imp.map_node(context)?,
        },
        uniffi_meta::Type::Record { module_path, name } => Type::Record {
            namespace: context.get_namespace_name(&module_path)?,
            name,
        },
        uniffi_meta::Type::Enum { module_path, name } => Type::Enum {
            namespace: context.get_namespace_name(&module_path)?,
            name,
        },
        uniffi_meta::Type::CallbackInterface { module_path, name } => Type::CallbackInterface {
            namespace: context.get_namespace_name(&module_path)?,
            name,
        },
        uniffi_meta::Type::Custom {
            module_path,
            name,
            builtin,
        } => Type::Custom {
            namespace: context.get_namespace_name(&module_path)?,
            name,
            builtin: builtin.map_node(context)?,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_stream_type_from_metadata_to_initial_ir() {
        let context = Context {
            module_path_map: Default::default(),
            constructors: Default::default(),
            methods: Default::default(),
            trait_methods: Default::default(),
            uniffi_traits: Default::default(),
            trait_impls: Default::default(),
        };
        let ty = uniffi_meta::Type::Stream {
            item_type: Box::new(uniffi_meta::Type::UInt32),
            error_type: Box::new(uniffi_meta::Type::String),
            is_send: true,
        };

        assert_eq!(
            map_type(ty, &context).unwrap(),
            Type::Stream {
                item_type: Box::new(Type::UInt32),
                error_type: Box::new(Type::String),
                is_send: true,
            }
        );
    }

    #[test]
    fn maps_input_stream_type_from_metadata_to_initial_ir() {
        let context = Context {
            module_path_map: Default::default(),
            constructors: Default::default(),
            methods: Default::default(),
            trait_methods: Default::default(),
            uniffi_traits: Default::default(),
            trait_impls: Default::default(),
        };
        let ty = uniffi_meta::Type::InputStream {
            item_type: Box::new(uniffi_meta::Type::UInt32),
            error_type: Box::new(uniffi_meta::Type::String),
            is_send: true,
        };

        assert_eq!(
            map_type(ty, &context).unwrap(),
            Type::InputStream {
                item_type: Box::new(Type::UInt32),
                error_type: Box::new(Type::String),
                is_send: true,
            }
        );
    }
}
