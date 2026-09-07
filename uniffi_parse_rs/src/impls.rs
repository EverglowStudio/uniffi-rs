/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use syn::{spanned::Spanned, ImplItem, ItemImpl};

use crate::{
    attrs::{ConstructorAttributes, ImplAttributes, MethodAttributes},
    paths::LookupCache,
    CompileEnv, Constructor, Error,
    ErrorKind::*,
    Ir, Method, RPath, Result,
};

pub struct Impl {
    pub attrs: ImplAttributes,
    pub self_type: syn::Type,
    pub constructors: Vec<Constructor>,
    pub methods: Vec<Method>,
}

impl Impl {
    pub fn parse(env: &CompileEnv, attrs: ImplAttributes, imp: ItemImpl) -> syn::Result<Self> {
        uniffi_meta::async_signature::validate_generics(&imp.generics)?;
        let mut constructors = vec![];
        let mut methods = vec![];
        for item in imp.items {
            if let ImplItem::Fn(f) = item {
                if let Some(fn_attrs) = ConstructorAttributes::parse(env, &f.attrs)? {
                    let constructor = Constructor::parse(fn_attrs, f)?;
                    if attrs.local && !constructor.is_async {
                        return Err(syn::Error::new(constructor.ident.span(), "local constructors must be async and return Arc<LocalObject<Self>> from an explicit owner"));
                    }
                    constructors.push(constructor);
                } else if let Some(fn_attrs) = MethodAttributes::parse(env, &f.attrs)? {
                    let mut method = Method::parse(fn_attrs, f)?;
                    if attrs.local {
                        method.self_arg.validate_local()?;
                        if method.ident == "close" || method.attrs.name.as_deref() == Some("close")
                        {
                            return Err(syn::Error::new(
                                method.ident.span(),
                                "close is generated for local Objects and waits for owner cleanup",
                            ));
                        }
                        method.is_async = true;
                    }
                    methods.push(method);
                }
            }
        }

        Ok(Self {
            attrs,
            self_type: *imp.self_ty,
            constructors,
            methods,
        })
    }

    pub fn impl_metadata<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        module_path: &RPath<'ir>,
    ) -> Result<Vec<uniffi_meta::Metadata>> {
        let self_ty = module_path.resolve_uniffi_meta_type(ir, cache, &self.self_type, None)?;
        let is_local = matches!(
            &self_ty,
            uniffi_meta::Type::Object {
                imp: uniffi_meta::ObjectImpl::LocalStruct,
                ..
            }
        );
        if self.attrs.local != is_local {
            return Err(Error::new(
                module_path.file_id(),
                self.self_type.span(),
                SynError("local Object and export(local) impl markers must match".into()),
            ));
        }
        let self_name = match (&self.attrs.name, self_ty.name()) {
            (Some(n), _) => n.to_string(),
            (None, Some(n)) => n.to_string(),
            (None, None) => {
                return Err(Error::new(
                    module_path.file_id(),
                    self.self_type.span(),
                    InvalidSelfType,
                ))
            }
        };
        let mut items = vec![];
        for c in self.constructors.iter() {
            if is_local {
                module_path.validate_local_constructor(
                    ir,
                    cache,
                    &c.return_type.signature.output,
                )?;
            }
            items.push(
                c.to_constructor_metadata(ir, cache, module_path, &self_name, &self_ty)?
                    .into(),
            );
        }
        for m in self.methods.iter() {
            items.push(
                m.to_method_metadata(ir, cache, module_path, &self_name, &self_ty)?
                    .into(),
            );
        }
        Ok(items)
    }
}
