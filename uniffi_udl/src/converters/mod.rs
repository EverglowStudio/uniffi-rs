/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{
    attributes::{DictionaryAttributes, MethodAttributes},
    literal::convert_default_value,
    InterfaceCollector,
};
use anyhow::{bail, Result};

use uniffi_meta::{
    CallbackInterfaceMetadata, DefaultValueMetadata, FieldMetadata, FnParamMetadata,
    MethodMetadata, RecordMetadata, TraitMethodMetadata, Type, UniffiTraitMetadata,
    VariantMetadata,
};

mod callables;
mod enum_;
mod interface;

use callables::callback_use_site_metadata;

/// Trait to help convert WedIDL syntax nodes into `InterfaceCollector` objects.
///
/// This trait does structural matching on the various weedle AST nodes and converts
/// them into appropriate structs that we can use to build up the contents of a
/// `InterfaceCollector`. It is basically the `TryFrom` trait except that the conversion
/// always happens in the context of a given `InterfaceCollector`, which is used for
/// resolving e.g. type definitions.
///
/// The difference between this trait and `APIBuilder` is that `APIConverter` treats the
/// `InterfaceCollector` as a read-only data source for resolving types, while `APIBuilder`
/// actually mutates the `InterfaceCollector` to add new definitions.
pub(crate) trait APIConverter<T> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<T>;
}

// Convert UDL docstring into metadata docstring
pub(crate) fn convert_docstring(docstring: &str) -> String {
    textwrap::dedent(docstring)
}

/// Convert a list of weedle items into a list of `InterfaceCollector` items,
/// by doing a direct item-by-item mapping.
impl<U, T: APIConverter<U>> APIConverter<Vec<U>> for Vec<T> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<Vec<U>> {
        self.iter().map(|v| v.convert(ci)).collect::<Result<_>>()
    }
}

impl APIConverter<VariantMetadata> for weedle::interface::OperationInterfaceMember<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<VariantMetadata> {
        if self.special.is_some() {
            bail!("special operations not supported");
        }
        if let Some(weedle::interface::StringifierOrStatic::Stringifier(_)) = self.modifier {
            bail!("stringifiers are not supported");
        }
        // OK, so this is a little weird.
        // The syntax we use for enum interface members is `Name(type arg, ...);`, which parses
        // as an anonymous operation where `Name` is the return type. We re-interpret it to
        // use `Name` as the name of the variant.
        if self.identifier.is_some() {
            bail!("enum interface members must not have a method name");
        }
        let name: String = {
            use weedle::types::{
                NonAnyType::{self, Identifier},
                ReturnType,
                SingleType::NonAny,
                Type::Single,
            };
            match &self.return_type {
                ReturnType::Type(Single(NonAny(Identifier(id)))) => id.type_.0.to_owned(),
                // Using recognized/parsed types as enum variant names can lead to the bail error because they match
                // before `Identifier`. `Error` is one that's likely to be common, so we're circumventing what is
                // likely a parsing issue here. As an example of the issue `Promise` (`Promise(PromiseType<'a>)`) as
                // a variant matches the `Identifier` arm, but `DataView` (`DataView(MayBeNull<term!(DataView)>)`)
                // fails.
                ReturnType::Type(Single(NonAny(NonAnyType::Error(_)))) => "Error".to_string(),
                _ => bail!("enum interface members must have plain identifiers as names"),
            }
        };
        Ok(VariantMetadata {
            name,
            orig_name: None,
            discr: None,
            fields: self
                .args
                .body
                .list
                .iter()
                .map(|arg| arg.convert(ci))
                .collect::<Result<Vec<_>>>()?,
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
        })
    }
}

impl APIConverter<RecordMetadata> for weedle::DictionaryDefinition<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<RecordMetadata> {
        let attributes = DictionaryAttributes::try_from(self.attributes.as_ref())?;
        if self.inheritance.is_some() {
            bail!("dictionary inheritance is not supported");
        }
        let other = Type::Record {
            module_path: ci.module_path().to_string(),
            name: self.identifier.0.to_string(),
        };
        for ut in make_uniffi_traits(
            &ci.module_path(),
            self.identifier.0,
            &attributes.get_uniffi_traits(),
            &other,
        )? {
            ci.items.insert(ut.into());
        }

        Ok(RecordMetadata {
            module_path: ci.module_path(),
            name: self.identifier.0.to_string(),
            orig_name: None,
            rust_path: None,
            remote: attributes.contains_remote(),
            fields: self.members.body.convert(ci)?,
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
        })
    }
}

impl APIConverter<FieldMetadata> for weedle::dictionary::DictionaryMember<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<FieldMetadata> {
        if self.attributes.is_some() {
            bail!("dictionary member attributes are not supported yet");
        }
        let type_ = ci.resolve_type_expression(&self.type_)?;
        let default = match self.default {
            None => None,
            Some(v) => Some(DefaultValueMetadata::Literal(convert_default_value(
                &v.value, &type_,
            )?)),
        };
        Ok(FieldMetadata {
            name: self.identifier.0.to_string(),
            orig_name: None,
            ty: type_,
            default,
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
        })
    }
}

impl APIConverter<CallbackInterfaceMetadata> for weedle::CallbackInterfaceDefinition<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<CallbackInterfaceMetadata> {
        if self.attributes.is_some() {
            bail!("callback interface attributes are not supported yet");
        }
        if self.inheritance.is_some() {
            bail!("callback interface inheritance is not supported");
        }
        let object_name = self.identifier.0;
        for (index, member) in self.members.body.iter().enumerate() {
            match member {
                weedle::interface::InterfaceMember::Operation(t) => {
                    let mut method: TraitMethodMetadata = t.convert(ci)?;
                    // A CallbackInterface is described in Rust as a trait, but uniffi
                    // generates a struct implementing the trait and passes the concrete version
                    // of that.
                    // This really just reflects the fact that CallbackInterface and Object
                    // should be merged; we'd still need a way to ask for a struct delegating to
                    // foreign implementations be done.
                    // But currently they are passed as a concrete type with no associated types.
                    method.trait_name = object_name.to_string();
                    method.index = index as u32;
                    let attributes = MethodAttributes::try_from(t.attributes.as_ref())?;
                    for contract in callback_use_site_metadata(
                        ci,
                        uniffi_meta::CallbackOperationKind::CallbackMethod,
                        Some(object_name),
                        &method.name,
                        &t.args.body.list,
                        method.return_type.as_ref(),
                        attributes.callback_contracts(),
                    )? {
                        ci.items.insert(contract.into());
                    }
                    ci.items.insert(method.into());
                }
                _ => bail!(
                    "no support for callback interface member type {:?} yet",
                    member
                ),
            }
        }
        Ok(CallbackInterfaceMetadata {
            module_path: ci.module_path(),
            name: object_name.to_string(),
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
        })
    }
}

fn make_uniffi_traits(
    module_path: &str,
    self_name: &str,
    names: &[String],
    other: &Type,
) -> Result<Vec<UniffiTraitMetadata>> {
    // A helper for our trait methods
    let make_trait_method = |name: &str,
                             inputs: Vec<FnParamMetadata>,
                             return_type: Option<Type>|
     -> Result<MethodMetadata> {
        Ok(MethodMetadata {
            module_path: module_path.to_string(),
            self_name: self_name.to_string(),
            name: name.to_string(),
            orig_name: None,
            is_async: false,
            inputs,
            return_type,
            throws: None,
            takes_self_by_arc: false,
            checksum: None,
            docstring: None,
        })
    };

    names
        .iter()
        .map(|trait_name| {
            Ok(match trait_name.as_str() {
                "Debug" => UniffiTraitMetadata::Debug {
                    fmt: make_trait_method("uniffi_trait_debug", vec![], Some(Type::String))?,
                },
                "Display" => UniffiTraitMetadata::Display {
                    fmt: make_trait_method("uniffi_trait_display", vec![], Some(Type::String))?,
                },
                "Eq" => UniffiTraitMetadata::Eq {
                    eq: make_trait_method(
                        "uniffi_trait_eq_eq",
                        vec![FnParamMetadata {
                            name: "other".to_string(),
                            ty: other.clone(),
                            by_ref: true,
                            default: None,
                            optional: false,
                        }],
                        Some(Type::Boolean),
                    )?,
                    ne: make_trait_method(
                        "uniffi_trait_eq_ne",
                        vec![FnParamMetadata {
                            name: "other".to_string(),
                            ty: other.clone(),
                            by_ref: true,
                            default: None,
                            optional: false,
                        }],
                        Some(Type::Boolean),
                    )?,
                },
                "Hash" => UniffiTraitMetadata::Hash {
                    hash: make_trait_method("uniffi_trait_hash", vec![], Some(Type::UInt64))?,
                },
                "Ord" => UniffiTraitMetadata::Ord {
                    cmp: make_trait_method(
                        "uniffi_trait_ord_cmp",
                        vec![FnParamMetadata {
                            name: "other".to_string(),
                            ty: other.clone(),
                            by_ref: true,
                            default: None,
                            optional: false,
                        }],
                        Some(Type::Int8),
                    )?,
                },
                _ => bail!("Invalid trait name: {}", trait_name),
            })
        })
        .collect::<Result<Vec<_>>>()
}

#[cfg(test)]
mod test {
    use super::*;
    use uniffi_meta::{LiteralMetadata, Metadata, Radix, Type};

    #[test]
    fn test_multiple_record_types() {
        const UDL: &str = r#"
            namespace test{};
            dictionary Empty {};
            dictionary Simple {
                u32 field;
            };
            dictionary Complex {
                string? key;
                u32 value = 0;
                required boolean spin;
            };
        "#;
        let mut ci = InterfaceCollector::from_webidl(UDL, "crate-name").unwrap();
        assert_eq!(ci.items.len(), 3);
        match &ci.items.pop_first().unwrap() {
            Metadata::Record(record) => {
                assert_eq!(record.name, "Complex");
                assert_eq!(record.fields.len(), 3);
                assert_eq!(record.fields[0].name, "key");
                assert_eq!(
                    record.fields[0].ty,
                    Type::Optional {
                        inner_type: Box::new(Type::String)
                    }
                );
                assert!(record.fields[0].default.is_none());
                assert_eq!(record.fields[1].name, "value");
                assert_eq!(record.fields[1].ty, Type::UInt32);
                assert!(matches!(
                    record.fields[1].default,
                    Some(DefaultValueMetadata::Literal(LiteralMetadata::UInt(
                        0,
                        Radix::Decimal,
                        Type::UInt32
                    )))
                ));
                assert_eq!(record.fields[2].name, "spin");
                assert_eq!(record.fields[2].ty, Type::Boolean);
                assert!(record.fields[2].default.is_none());
            }
            _ => unreachable!(),
        }

        match &ci.items.pop_first().unwrap() {
            Metadata::Record(record) => {
                assert_eq!(record.name, "Empty");
                assert_eq!(record.fields.len(), 0);
            }
            _ => unreachable!(),
        }

        match &ci.items.pop_first().unwrap() {
            Metadata::Record(record) => {
                assert_eq!(record.name, "Simple");
                assert_eq!(record.fields.len(), 1);
                assert_eq!(record.fields[0].name, "field");
                assert_eq!(record.fields[0].ty, Type::UInt32);
                assert!(record.fields[0].default.is_none());
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn callback_contracts_use_one_metadata_representation_and_keep_method_owners() {
        const UDL: &str = r#"
            namespace test {
                Logger make_logger();
                [CallbackContract="return,scoped,calling_thread,forbidden"] Logger make_logger_checked();
                [CallbackContract="argument[0].field[callback],retained,calling_thread,allowed"] void consume_wrapper(Wrapper value);
                void consume_arg([CallbackContract="retained,calling_thread,forbidden"] Logger callback);
            };
            callback interface Logger { void log(); };
            callback interface Relay {
                [CallbackContract="argument[0],retained,may_cross_thread,allowed"] void invoke(Logger callback);
            };
            dictionary Wrapper { Logger callback; };
            interface First {
                [CallbackContract="argument[0],retained,calling_thread,forbidden"] constructor(Logger callback);
                [CallbackContract="argument[0],retained,calling_thread,forbidden"] void notify(Logger cb);
            };
            interface Second {
                [CallbackContract="argument[0],retained,calling_thread,allowed"] void notify(Logger cb);
            };
        "#;
        let ci = InterfaceCollector::from_webidl(UDL, "crate-name").unwrap();
        let contracts: Vec<_> = ci
            .items
            .iter()
            .filter_map(|item| match item {
                Metadata::CallbackUseSite(contract) => Some(contract),
                _ => None,
            })
            .collect();
        assert_eq!(contracts.len(), 7);
        assert!(contracts
            .iter()
            .all(|contract| contract.module_path == "crate-name"));
        assert!(contracts.iter().any(|contract| {
            contract.owner.as_deref() == Some("First")
                && contract.operation_name == "notify"
                && contract.contract.reentrancy == uniffi_meta::CallbackReentrancy::Forbidden
        }));
        assert!(contracts.iter().any(|contract| {
            contract.owner.as_deref() == Some("First")
                && contract.operation_kind == uniffi_meta::CallbackOperationKind::Constructor
                && contract.operation_name == "new"
                && contract.path == uniffi_meta::CallbackValuePath::argument(0)
        }));
        assert!(contracts.iter().any(|contract| {
            contract.owner.as_deref() == Some("Second")
                && contract.operation_name == "notify"
                && contract.contract.reentrancy == uniffi_meta::CallbackReentrancy::Allowed
        }));
        assert!(contracts.iter().any(|contract| {
            contract.owner.as_deref() == Some("Relay")
                && contract.operation_kind == uniffi_meta::CallbackOperationKind::CallbackMethod
                && contract.operation_name == "invoke"
                && contract.path == uniffi_meta::CallbackValuePath::argument(0)
        }));
        assert!(contracts.iter().any(|contract| {
            contract.owner.is_none()
                && contract.operation_name == "make_logger_checked"
                && contract.path == uniffi_meta::CallbackValuePath::return_value()
        }));
        assert!(contracts.iter().any(|contract| {
            contract.owner.is_none()
                && contract.operation_name == "consume_wrapper"
                && contract.path
                    == uniffi_meta::CallbackValuePath(vec![
                        uniffi_meta::CallbackValuePathSegment::Argument(0),
                        uniffi_meta::CallbackValuePathSegment::Field("callback".into()),
                    ])
        }));
        assert!(contracts.iter().any(|contract| {
            contract.owner.is_none()
                && contract.operation_name == "consume_arg"
                && contract.path == uniffi_meta::CallbackValuePath::argument(0)
        }));
    }

    #[test]
    fn callback_contracts_reject_duplicate_and_invalid_specs() {
        let duplicate = r#"
            namespace test {
                [CallbackContract="argument[0],retained,calling_thread,forbidden"]
                void consume([CallbackContract="retained,calling_thread,forbidden"] Logger callback);
            };
            callback interface Logger { void log(); };
        "#;
        let error = InterfaceCollector::from_webidl(duplicate, "crate-name").unwrap_err();
        assert!(error
            .to_string()
            .contains("duplicate CallbackContract path"));

        let invalid_path = r#"
            namespace test {
                [CallbackContract="argument[bad],retained,calling_thread,forbidden"]
                void consume(Logger callback);
            };
            callback interface Logger { void log(); };
        "#;
        let error = InterfaceCollector::from_webidl(invalid_path, "crate-name").unwrap_err();
        assert!(error.to_string().contains("invalid callback argument path"));

        let invalid_index = r#"
            namespace test {
                [CallbackContract="argument[1],retained,calling_thread,forbidden"]
                void consume(Logger callback);
            };
            callback interface Logger { void log(); };
        "#;
        let error = InterfaceCollector::from_webidl(invalid_index, "crate-name").unwrap_err();
        assert!(error
            .to_string()
            .contains("callback contract argument index 1 is out of range"));

        let invalid_value = r#"
            namespace test {
                [CallbackContract="argument[0],retained,wrong_thread,forbidden"]
                void consume(Logger callback);
            };
            callback interface Logger { void log(); };
        "#;
        let error = InterfaceCollector::from_webidl(invalid_value, "crate-name").unwrap_err();
        assert!(error.to_string().contains("invalid callback threading"));
    }
}
