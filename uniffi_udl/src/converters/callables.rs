/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use super::APIConverter;
use crate::attributes::ArgumentAttributes;
use crate::attributes::{ConstructorAttributes, FunctionAttributes, MethodAttributes};
use crate::converters::convert_docstring;
use crate::literal::convert_default_value;
use crate::InterfaceCollector;
use anyhow::{bail, Result};

use uniffi_meta::{
    CallbackContract, CallbackOperationKind, CallbackReentrancy, CallbackRetention,
    CallbackThreading, CallbackUseSiteMetadata, CallbackValuePath, CallbackValuePathSegment,
    ConstructorMetadata, DefaultValueMetadata, FieldMetadata, FnMetadata, FnParamMetadata,
    MethodMetadata, TraitMethodMetadata,
};

pub(crate) fn callback_use_site_metadata<'a>(
    ci: &InterfaceCollector,
    operation_kind: CallbackOperationKind,
    owner: Option<&str>,
    operation_name: &str,
    args: &[weedle::argument::Argument<'_>],
    _return_type: Option<&uniffi_meta::Type>,
    operation_contracts: impl IntoIterator<Item = &'a str>,
) -> Result<Vec<CallbackUseSiteMetadata>> {
    let mut specs = Vec::new();
    let mut seen_paths = std::collections::BTreeSet::new();
    for (index, argument) in args.iter().enumerate() {
        let weedle::argument::Argument::Single(argument) = argument else {
            continue;
        };
        let attrs = ArgumentAttributes::try_from(argument.attributes.as_ref())?;
        if let Some(spec) = attrs.callback_contract() {
            specs.push((format!("argument[{index}]"), spec.to_owned()));
        }
    }
    specs.extend(
        operation_contracts
            .into_iter()
            .map(|spec| (String::new(), spec.to_owned())),
    );

    specs
        .into_iter()
        .map(|(default_path, spec)| {
            let parts: Vec<_> = spec.split(',').map(str::trim).collect();
            let (path, values) = match parts.as_slice() {
                [retention, threading, reentrancy] => {
                    (default_path, [*retention, *threading, *reentrancy])
                }
                [path, retention, threading, reentrancy] => (
                    (*path).to_owned(),
                    [*retention, *threading, *reentrancy],
                ),
                _ => bail!(
                    "CallbackContract must be `retention,threading,reentrancy` on an argument or `path,retention,threading,reentrancy` on an operation"
                ),
            };
            let path = parse_callback_path(&path)?;
            match path.segments().first() {
                Some(CallbackValuePathSegment::Argument(index)) if (*index as usize) < args.len() => {}
                Some(CallbackValuePathSegment::Argument(index)) => {
                    bail!("callback contract argument index {index} is out of range")
                }
                Some(CallbackValuePathSegment::Return) => {}
                _ => bail!(
                    "callback contract path must start with `argument[index]` or `return`"
                ),
            }
            let path_key = path.to_string();
            if !seen_paths.insert(path_key.clone()) {
                bail!("duplicate CallbackContract path `{path_key}`");
            }
            let retention = match values[0] {
                "scoped" => CallbackRetention::Scoped,
                "retained" => CallbackRetention::Retained,
                other => bail!("invalid callback retention `{other}`"),
            };
            let threading = match values[1] {
                "calling_thread" => CallbackThreading::CallingThread,
                "may_cross_thread" => CallbackThreading::MayCrossThread,
                other => bail!("invalid callback threading `{other}`"),
            };
            let reentrancy = match values[2] {
                "forbidden" => CallbackReentrancy::Forbidden,
                "allowed" => CallbackReentrancy::Allowed,
                other => bail!("invalid callback reentrancy `{other}`"),
            };
            Ok(CallbackUseSiteMetadata {
                module_path: ci.module_path(),
                operation_kind,
                owner: owner.map(str::to_owned),
                operation_name: operation_name.to_owned(),
                path,
                contract: CallbackContract {
                    retention,
                    threading,
                    reentrancy,
                },
            })
        })
        .collect()
}

fn parse_callback_path(path: &str) -> Result<CallbackValuePath> {
    let mut segments = Vec::new();
    for segment in path.split('.') {
        if segment == "return" {
            segments.push(CallbackValuePathSegment::Return);
        } else if let Some(value) = segment
            .strip_prefix("argument[")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            segments.push(CallbackValuePathSegment::Argument(value.parse().map_err(
                |_| anyhow::anyhow!("invalid callback argument path `{segment}`"),
            )?));
        } else if let Some(value) = segment
            .strip_prefix("field[")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            segments.push(CallbackValuePathSegment::Field(value.to_owned()));
        } else if let Some(value) = segment
            .strip_prefix("variant[")
            .and_then(|rest| rest.strip_suffix(']'))
        {
            segments.push(CallbackValuePathSegment::Variant(value.to_owned()));
        } else {
            segments.push(match segment {
                "item" => CallbackValuePathSegment::SequenceItem,
                "set-item" => CallbackValuePathSegment::SetItem,
                "key" => CallbackValuePathSegment::MapKey,
                "value" => CallbackValuePathSegment::MapValue,
                _ => bail!("invalid callback value path segment `{segment}`"),
            });
        }
    }
    if segments.is_empty() {
        bail!("callback contract path is empty");
    }
    Ok(CallbackValuePath(segments))
}

impl APIConverter<FieldMetadata> for weedle::argument::Argument<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<FieldMetadata> {
        match self {
            weedle::argument::Argument::Single(t) => t.convert(ci),
            weedle::argument::Argument::Variadic(_) => bail!("variadic arguments not supported"),
        }
    }
}

impl APIConverter<FieldMetadata> for weedle::argument::SingleArgument<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<FieldMetadata> {
        let type_ = ci.resolve_type_expression(&self.type_)?;
        if self.default.is_some() {
            bail!("enum interface variant fields must not have default values");
        }
        if self.attributes.is_some() {
            bail!("enum interface variant fields must not have attributes");
        }
        Ok(FieldMetadata {
            name: self.identifier.0.to_string(),
            orig_name: None,
            ty: type_,
            default: None,
            docstring: None,
        })
    }
}

impl APIConverter<FnParamMetadata> for weedle::argument::Argument<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<FnParamMetadata> {
        match self {
            weedle::argument::Argument::Single(t) => t.convert(ci),
            weedle::argument::Argument::Variadic(_) => bail!("variadic arguments not supported"),
        }
    }
}

impl APIConverter<FnParamMetadata> for weedle::argument::SingleArgument<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<FnParamMetadata> {
        let type_ = ci.resolve_type_expression(&self.type_)?;
        let default = match self.default {
            None => None,
            Some(v) => Some(DefaultValueMetadata::Literal(convert_default_value(
                &v.value, &type_,
            )?)),
        };
        let by_ref = ArgumentAttributes::try_from(self.attributes.as_ref())?.by_ref();
        Ok(FnParamMetadata {
            name: self.identifier.0.to_string(),
            ty: type_,
            by_ref,
            optional: self.optional.is_some(),
            default,
        })
    }
}

impl APIConverter<FnMetadata> for weedle::namespace::NamespaceMember<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<FnMetadata> {
        match self {
            weedle::namespace::NamespaceMember::Operation(f) => f.convert(ci),
            _ => bail!("no support for namespace member type {:?} yet", self),
        }
    }
}

impl APIConverter<FnMetadata> for weedle::namespace::OperationNamespaceMember<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<FnMetadata> {
        let return_type = ci.resolve_return_type_expression(&self.return_type)?;
        let name = match self.identifier {
            None => bail!("anonymous functions are not supported {:?}", self),
            Some(id) => id.0.to_string(),
        };
        let attrs = FunctionAttributes::try_from(self.attributes.as_ref())?;
        let is_async = attrs.is_async();
        let throws = match attrs.get_throws_err() {
            None => None,
            Some(name) => match ci.get_type(name) {
                Some(t) => Some(t),
                None => bail!("unknown type for error: {name}"),
            },
        };
        let metadata = FnMetadata {
            module_path: ci.module_path(),
            name,
            orig_name: None,
            is_async,
            return_type,
            inputs: self.args.body.list.convert(ci)?,
            throws,
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
            checksum: None,
        };
        for contract in callback_use_site_metadata(
            ci,
            CallbackOperationKind::Function,
            None,
            &metadata.name,
            &self.args.body.list,
            metadata.return_type.as_ref(),
            attrs.callback_contracts(),
        )? {
            ci.items.insert(contract.into());
        }
        Ok(metadata)
    }
}

impl APIConverter<ConstructorMetadata> for weedle::interface::ConstructorInterfaceMember<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<ConstructorMetadata> {
        let attributes = match &self.attributes {
            Some(attr) => ConstructorAttributes::try_from(attr)?,
            None => Default::default(),
        };
        let throws = attributes
            .get_throws_err()
            .map(|name| ci.get_type(name).expect("invalid throws type"));
        let metadata = ConstructorMetadata {
            module_path: ci.module_path(),
            name: String::from(attributes.get_name().unwrap_or("new")),
            orig_name: None,
            // We don't know the name of the containing `Object` at this point, fill it in later.
            self_name: Default::default(),
            self_type: None,
            is_async: attributes.is_async(),
            // Also fill in checksum_fn_name later, since it depends on object_name
            inputs: self.args.body.list.convert(ci)?,
            throws,
            checksum: None,
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
        };
        Ok(metadata)
    }
}

impl APIConverter<MethodMetadata> for weedle::interface::OperationInterfaceMember<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<MethodMetadata> {
        if self.special.is_some() {
            bail!("special operations not supported");
        }
        if self.modifier.is_some() {
            bail!("method modifiers are not supported")
        }
        let return_type = ci.resolve_return_type_expression(&self.return_type)?;
        let attributes = MethodAttributes::try_from(self.attributes.as_ref())?;
        let is_async = attributes.is_async();

        let throws = match attributes.get_throws_err() {
            Some(name) => match ci.get_type(name) {
                Some(t) => Some(t),
                None => bail!("unknown type for error: {name}"),
            },
            None => None,
        };

        let takes_self_by_arc = attributes.get_self_by_arc();
        let metadata = MethodMetadata {
            module_path: ci.module_path(),
            // We don't know the name of the containing `Object` at this point, fill it in later.
            self_name: Default::default(),
            name: match self.identifier {
                None => bail!("anonymous methods are not supported {:?}", self),
                Some(id) => {
                    let name = id.0.to_string();
                    if name == "new" {
                        bail!("the method name \"new\" is reserved for the default constructor");
                    }
                    name
                }
            },
            orig_name: None,
            is_async,
            inputs: self.args.body.list.convert(ci)?,
            return_type,
            throws,
            takes_self_by_arc,
            checksum: None,
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
        };
        Ok(metadata)
    }
}

impl APIConverter<TraitMethodMetadata> for weedle::interface::OperationInterfaceMember<'_> {
    fn convert(&self, ci: &mut InterfaceCollector) -> Result<TraitMethodMetadata> {
        if self.special.is_some() {
            bail!("special operations not supported");
        }
        if self.modifier.is_some() {
            bail!("method modifiers are not supported")
        }
        let return_type = ci.resolve_return_type_expression(&self.return_type)?;
        let attributes = MethodAttributes::try_from(self.attributes.as_ref())?;
        let is_async = attributes.is_async();

        let throws = match attributes.get_throws_err() {
            Some(name) => match ci.get_type(name) {
                Some(t) => Some(t),
                None => bail!("unknown type for error: {name}"),
            },
            None => None,
        };

        let takes_self_by_arc = attributes.get_self_by_arc();
        let metadata = TraitMethodMetadata {
            module_path: ci.module_path(),
            trait_name: Default::default(), // we'll fill these in later.
            index: Default::default(),
            name: match self.identifier {
                None => bail!("anonymous methods are not supported {:?}", self),
                Some(id) => {
                    let name = id.0.to_string();
                    if name == "new" {
                        bail!("the method name \"new\" is reserved for the default constructor");
                    }
                    name
                }
            },
            orig_name: None,
            is_async,
            inputs: self.args.body.list.convert(ci)?,
            return_type,
            throws,
            takes_self_by_arc,
            checksum: None,
            docstring: self.docstring.as_ref().map(|v| convert_docstring(&v.0)),
        };
        Ok(metadata)
    }
}
