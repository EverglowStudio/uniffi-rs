/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use syn::{ext::IdentExt, Ident, ItemTrait, TraitItem, TraitItemFn};
use uniffi_meta::{CallbackInterfaceMetadata, ObjectMetadata};

use crate::{
    attrs::{MethodAttributes, TraitAttributes, TraitExportType},
    paths::LookupCache,
    Argument, CompileEnv, Ir, RPath, Result, ReturnType, SelfArg, Visibility,
};

pub struct Trait {
    pub attrs: TraitAttributes,
    pub ident: Ident,
    pub vis: Visibility,
    pub methods: Vec<TraitMethod>,
}

#[derive(Clone)]
pub struct TraitMethod {
    pub attrs: MethodAttributes,
    pub is_async: bool,
    pub ident: Ident,
    pub self_arg: SelfArg,
    pub args: Vec<Argument>,
    pub return_type: ReturnType,
}

impl Trait {
    pub fn parse(env: &CompileEnv, attrs: TraitAttributes, tr: ItemTrait) -> syn::Result<Self> {
        uniffi_meta::async_signature::validate_generics(&tr.generics)?;
        let foreign = match attrs.export_ty {
            TraitExportType::CallbackInterface => true,
            TraitExportType::TraitInterface(kind) => kind.has_foreign(),
        };
        let mut methods = vec![];
        for item in tr.items {
            if let TraitItem::Fn(f) = item {
                if let Some(method_attrs) = MethodAttributes::parse(env, &f.attrs)? {
                    if foreign {
                        uniffi_meta::async_signature::validate_foreign_async_bounds(
                            &f.sig,
                            env.is_wasm(),
                        )?;
                    }
                    methods.push(TraitMethod::parse(method_attrs, f)?);
                }
            }
        }

        Ok(Self {
            attrs,
            ident: tr.ident,
            vis: tr.vis.into(),
            methods,
        })
    }

    pub fn name(&self) -> String {
        self.ident.unraw().to_string()
    }

    pub fn self_ty(&self, module_path: &RPath<'_>) -> uniffi_meta::Type {
        match self.attrs.export_ty {
            TraitExportType::TraitInterface(kind) => uniffi_meta::Type::Object {
                module_path: module_path.path_string(),
                name: self.name(),
                imp: uniffi_meta::ObjectImpl::Trait(kind),
            },
            TraitExportType::CallbackInterface => uniffi_meta::Type::CallbackInterface {
                module_path: module_path.path_string(),
                name: self.name(),
            },
        }
    }

    pub fn trait_metadata<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        item_path: RPath<'ir>,
    ) -> Result<Vec<uniffi_meta::Metadata>> {
        let module_path = item_path.parent()?;
        let names = item_path.public_path_to_item(ir, cache)?;
        let mut items = vec![];

        let self_ty = match self.attrs.export_ty {
            TraitExportType::TraitInterface(kind) => {
                items.push(
                    ObjectMetadata {
                        module_path: names.module_path.clone(),
                        name: names.name.clone(),
                        orig_name: names.orig_name,
                        docstring: self.attrs.docstring.clone(),
                        imp: uniffi_meta::ObjectImpl::Trait(kind),
                        remote: false,
                    }
                    .into(),
                );
                uniffi_meta::Type::Object {
                    module_path: names.module_path,
                    name: names.name.clone(),
                    imp: uniffi_meta::ObjectImpl::Trait(kind),
                }
            }
            TraitExportType::CallbackInterface => {
                items.push(
                    CallbackInterfaceMetadata {
                        module_path: names.module_path.clone(),
                        name: names.name.clone(),
                        docstring: self.attrs.docstring.clone(),
                    }
                    .into(),
                );
                uniffi_meta::Type::CallbackInterface {
                    module_path: names.module_path,
                    name: names.name.clone(),
                }
            }
        };
        for (i, m) in self.methods.iter().enumerate() {
            items.push(
                m.to_trait_method_metadata(ir, cache, &module_path, &names.name, &self_ty, i)?
                    .into(),
            );
        }
        Ok(items)
    }
}

impl TraitMethod {
    pub fn parse(attrs: MethodAttributes, f: TraitItemFn) -> syn::Result<Self> {
        let return_type = ReturnType::from_signature(&f.sig)?;
        let mut inputs = f.sig.inputs.into_iter();
        let self_arg = SelfArg::parse(inputs.next(), f.sig.ident.span())?;

        Ok(Self {
            attrs,
            ident: f.sig.ident,
            is_async: return_type.signature.is_async(),
            self_arg,
            args: inputs
                .map(Argument::parse)
                .collect::<syn::Result<Vec<_>>>()?,
            return_type,
        })
    }

    pub fn to_trait_method_metadata<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        module_path: &RPath<'ir>,
        trait_name: &str,
        self_ty: &uniffi_meta::Type,
        index: usize,
    ) -> Result<uniffi_meta::TraitMethodMetadata> {
        let crate::types::ReturnType {
            ok: return_type,
            err: throws,
            rust_return_is_result,
            rust_stream_item_is_result,
        } = self
            .return_type
            .return_type_and_throws_for_method(ir, cache, module_path, self_ty)?;
        let item_name = self.ident.unraw().to_string();
        let (name, orig_name) = match &self.attrs.name {
            None => (item_name, None),
            Some(name) => (name.clone(), Some(item_name)),
        };

        Ok(uniffi_meta::TraitMethodMetadata {
            module_path: module_path.path_string(),
            trait_name: trait_name.to_string(),
            index: index as u32,
            name,
            orig_name,
            docstring: self.attrs.docstring.clone(),
            is_async: self.is_async,
            rust_async: self.return_type.signature.rust_async,
            rust_return_is_result,
            rust_stream_item_is_result,
            takes_self_by_arc: self.self_arg.takes_self_by_arc(ir, cache, module_path)?,
            inputs: self
                .args
                .iter()
                .map(|arg| {
                    arg.create_method_metadata(
                        ir,
                        cache,
                        module_path,
                        &self.attrs.defaults,
                        self_ty,
                    )
                })
                .collect::<Result<Vec<_>>>()?,
            return_type,
            throws,
            // Method checksums are not supported, we can implement an improved system by
            // checksumming the entire interface and having a single checksum
            checksum: None,
        })
    }
}
