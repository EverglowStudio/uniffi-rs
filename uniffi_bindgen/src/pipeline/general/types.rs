/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::*;

pub fn map_type_node(ty: Type, context: &Context) -> Result<TypeNode> {
    Ok(TypeNode {
        id: context.get_type_id(&ty)?,
        canonical_name: canonical_name(&ty),
        is_used_as_error: context.type_is_used_as_error(&ty),
        ffi_type: ffi_types::ffi_type(&ty, context)?,
        has_from_unexpected_callback_error_impl: context
            .type_has_from_unexpected_callback_error_impl(&ty),
        ty: ty.map_node(context)?,
    })
}

pub fn canonical_name(ty: &Type) -> String {
    match ty {
        Type::UInt8 => "UInt8".to_string(),
        Type::Int8 => "Int8".to_string(),
        Type::UInt16 => "UInt16".to_string(),
        Type::Int16 => "Int16".to_string(),
        Type::UInt32 => "UInt32".to_string(),
        Type::Int32 => "Int32".to_string(),
        Type::UInt64 => "UInt64".to_string(),
        Type::Int64 => "Int64".to_string(),
        Type::Float32 => "Float32".to_string(),
        Type::Float64 => "Float64".to_string(),
        Type::Boolean => "Boolean".to_string(),
        Type::String => "String".to_string(),
        Type::Bytes => "Bytes".to_string(),
        Type::Timestamp => "Timestamp".to_string(),
        Type::Duration => "Duration".to_string(),
        Type::Interface { name, .. }
        | Type::CallbackInterface { name, .. }
        | Type::Record { name, .. }
        | Type::Enum { name, .. }
        | Type::Custom { name, .. } => format!("Type{name}"),
        Type::Optional { inner_type } => {
            format!("Optional{}", canonical_name(inner_type))
        }
        Type::Sequence { inner_type } => {
            format!("Sequence{}", canonical_name(inner_type))
        }
        // Note: this is currently guaranteed to be unique because keys can only be primitive
        // types.  If we allowed user-defined types, there would be potential collisions.  For
        // example "MapTypeFooTypeTypeBar" could be "Foo" -> "TypeBar" or "FooType" -> "Bar".
        Type::Map {
            key_type,
            value_type,
        } => format!(
            "Map{}{}",
            canonical_name(key_type),
            canonical_name(value_type),
        ),
        Type::Box { inner_type } => format!("Box{}", canonical_name(inner_type)),
        Type::Set { inner_type } => {
            format!("Set{}", canonical_name(inner_type))
        }
        Type::Stream {
            item_type,
            error_type,
            ..
        } => format!(
            "Stream{}{}",
            canonical_name(item_type),
            canonical_name(error_type),
        ),
        Type::InputStream {
            item_type,
            error_type,
            ..
        } => format!(
            "InputStream{}{}",
            canonical_name(item_type),
            canonical_name(error_type),
        ),
    }
}

pub fn map_type(mut ty: Type, context: &Context) -> Result<Type> {
    Ok(match ty {
        // Map names for top-level types
        Type::Record {
            ref namespace,
            ref mut name,
            ..
        }
        | Type::Enum {
            ref namespace,
            ref mut name,
            ..
        }
        | Type::Interface {
            ref namespace,
            ref mut name,
            ..
        }
        | Type::CallbackInterface {
            ref namespace,
            ref mut name,
            ..
        }
        | Type::Custom {
            ref namespace,
            ref mut name,
            ..
        } => {
            *name = rename::type_(namespace, name.clone(), context)?;
            ty
        }
        // Map inner types
        Type::Optional { inner_type } => Type::Optional {
            inner_type: Box::new(map_type(*inner_type, context)?),
        },
        Type::Sequence { inner_type } => Type::Sequence {
            inner_type: Box::new(map_type(*inner_type, context)?),
        },
        Type::Map {
            key_type,
            value_type,
        } => Type::Map {
            key_type: Box::new(map_type(*key_type, context)?),
            value_type: Box::new(map_type(*value_type, context)?),
        },
        Type::Set { inner_type } => Type::Set {
            inner_type: Box::new(map_type(*inner_type, context)?),
        },
        Type::Stream {
            item_type,
            error_type,
            is_send,
        } => Type::Stream {
            item_type: Box::new(map_type(*item_type, context)?),
            error_type: Box::new(map_type(*error_type, context)?),
            is_send,
        },
        Type::InputStream {
            item_type,
            error_type,
            is_send,
        } => Type::InputStream {
            item_type: Box::new(map_type(*item_type, context)?),
            error_type: Box::new(map_type(*error_type, context)?),
            is_send,
        },
        // All other types can be returned unchanged
        _ => ty,
    })
}

pub fn type_for_record(rec: &initial::Record, context: &Context) -> Result<Type> {
    Ok(Type::Record {
        namespace: context.namespace_name()?,
        name: rec.name.clone(),
        orig_name: rec.orig_name.clone(),
    })
}

pub fn type_for_enum(en: &initial::Enum, context: &Context) -> Result<Type> {
    Ok(Type::Enum {
        namespace: context.namespace_name()?,
        name: en.name.clone(),
        orig_name: en.orig_name.clone(),
    })
}

pub fn type_for_interface(int: &initial::Interface, context: &Context) -> Result<Type> {
    Ok(Type::Interface {
        namespace: context.namespace_name()?,
        name: int.name.clone(),
        orig_name: int.orig_name.clone(),
        imp: int.imp,
    })
}

pub fn type_for_callback_interface(
    cbi: &initial::CallbackInterface,
    context: &Context,
) -> Result<Type> {
    Ok(Type::CallbackInterface {
        namespace: context.namespace_name()?,
        name: cbi.name.clone(),
        orig_name: cbi.orig_name.clone(),
    })
}

pub fn type_for_custom_type(custom: &initial::CustomType, context: &Context) -> Result<Type> {
    Ok(Type::Custom {
        namespace: context.namespace_name()?,
        name: custom.name.clone(),
        orig_name: custom.orig_name.clone(),
        builtin: Box::new(custom.builtin.clone()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_context_with_type(ty: &Type) -> Context {
        let mut context = Context::new("test");
        context.type_id_map.insert(ty.clone(), 0);
        context
    }

    #[test]
    fn maps_stream_type_into_general_ir() {
        let stream_type = Type::Stream {
            item_type: Box::new(Type::UInt32),
            error_type: Box::new(Type::String),
            is_send: true,
        };
        let context = test_context_with_type(&stream_type);

        let type_node: TypeNode = stream_type.map_node(&context).unwrap();
        assert_eq!(type_node.canonical_name, "StreamUInt32String");
        assert_eq!(type_node.ffi_type, FfiType::Handle(HandleKind::RustStream));
        assert_eq!(
            type_node.ty,
            Type::Stream {
                item_type: Box::new(Type::UInt32),
                error_type: Box::new(Type::String),
                is_send: true,
            }
        );
    }

    #[test]
    fn maps_input_stream_type_into_general_ir() {
        let stream_type = Type::InputStream {
            item_type: Box::new(Type::UInt32),
            error_type: Box::new(Type::String),
            is_send: true,
        };
        let context = test_context_with_type(&stream_type);

        let type_node: TypeNode = stream_type.map_node(&context).unwrap();
        assert_eq!(type_node.canonical_name, "InputStreamUInt32String");
        assert_eq!(
            type_node.ffi_type,
            FfiType::Handle(HandleKind::ForeignStream)
        );
        assert_eq!(
            type_node.ty,
            Type::InputStream {
                item_type: Box::new(Type::UInt32),
                error_type: Box::new(Type::String),
                is_send: true,
            }
        );
    }
}
