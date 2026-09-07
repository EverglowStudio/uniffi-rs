/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use syn::{ext::IdentExt, spanned::Spanned, FnArg, Ident, ItemFn, Pat};

use crate::{
    attrs::{DefaultMap, FunctionAttributes},
    paths::LookupCache,
    ErrorKind::*,
    Ir, RPath, Result, Visibility,
};

#[derive(Clone)]
pub struct Function {
    pub attrs: FunctionAttributes,
    pub is_async: bool,
    pub ident: Ident,
    pub vis: Visibility,
    pub args: Vec<Argument>,
    pub return_ty: ReturnType,
}

#[derive(Clone)]
pub struct Argument {
    pub ident: Ident,
    pub ty: syn::Type,
}

#[derive(Clone)]
pub struct ReturnType {
    pub return_ty: syn::ReturnType,
    pub signature: uniffi_meta::async_signature::ReturnSignature,
}

impl Function {
    pub fn parse(attrs: FunctionAttributes, f: ItemFn) -> syn::Result<Self> {
        let return_type = ReturnType::from_signature(&f.sig)?;
        Ok(Self {
            attrs,
            ident: f.sig.ident,
            vis: f.vis.into(),
            is_async: return_type.signature.is_async(),
            args: f
                .sig
                .inputs
                .into_iter()
                .map(Argument::parse)
                .collect::<syn::Result<Vec<_>>>()?,
            return_ty: return_type,
        })
    }

    pub fn fn_metadata<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        item_path: RPath<'ir>,
    ) -> Result<uniffi_meta::FnMetadata> {
        let module_path = item_path.parent()?;
        let names = item_path.public_path_to_item(ir, cache)?;
        let crate::types::ReturnType {
            ok: return_type,
            err: throws,
            rust_return_is_result,
            rust_stream_item_is_result,
        } = self
            .return_ty
            .return_type_and_throws(ir, cache, &module_path)?;

        Ok(uniffi_meta::FnMetadata {
            module_path: names.module_path,
            name: names.name,
            orig_name: names.orig_name,
            is_async: self.is_async,
            rust_async: self.return_ty.signature.rust_async,
            rust_return_is_result,
            rust_stream_item_is_result,
            docstring: self.attrs.docstring.clone(),
            checksum: None,
            inputs: self
                .args
                .iter()
                .map(|arg| arg.create_fn_metadata(ir, cache, &module_path, &self.attrs.defaults))
                .collect::<Result<Vec<_>>>()?,
            return_type,
            throws,
        })
    }
}

impl Argument {
    pub fn parse(arg: FnArg) -> syn::Result<Self> {
        let span = arg.span();
        let pat_ty = match arg {
            FnArg::Receiver(_) => return Err(syn::Error::new(span, InvalidArgType)),
            FnArg::Typed(pat_ty) => pat_ty,
        };
        let ident = match *pat_ty.pat {
            Pat::Ident(p) => p.ident,
            _ => return Err(syn::Error::new(span, InvalidArgType)),
        };
        Ok(Self {
            ident,
            ty: *pat_ty.ty,
        })
    }

    pub fn create_fn_metadata<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        path: &RPath<'ir>,
        defaults: &DefaultMap,
    ) -> Result<uniffi_meta::FnParamMetadata> {
        self.create_metadata(ir, cache, path, defaults, None)
    }

    pub fn create_method_metadata<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        path: &RPath<'ir>,
        defaults: &DefaultMap,
        self_type: &uniffi_meta::Type,
    ) -> Result<uniffi_meta::FnParamMetadata> {
        self.create_metadata(ir, cache, path, defaults, Some(self_type))
    }

    pub fn create_metadata<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        path: &RPath<'ir>,
        defaults: &DefaultMap,
        self_ty: Option<&uniffi_meta::Type>,
    ) -> Result<uniffi_meta::FnParamMetadata> {
        let arg = path.resolve_arg(ir, cache, &self.ty, self_ty)?;
        let default = defaults.get_uniffi_meta(path.file_id(), &self.ident, &arg.ty)?;

        Ok(uniffi_meta::FnParamMetadata {
            name: self.ident.unraw().to_string(),
            ty: arg.ty,
            pass_by: arg.pass_by,
            default,
            optional: false,
        })
    }
}

impl ReturnType {
    pub fn from_signature(sig: &syn::Signature) -> syn::Result<Self> {
        Ok(Self {
            return_ty: sig.output.clone(),
            signature: uniffi_meta::async_signature::classify_signature(sig)?,
        })
    }

    pub fn return_type_and_throws<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        module_path: &RPath<'ir>,
    ) -> Result<crate::types::ReturnType> {
        self._return_type_and_throws(ir, cache, module_path, None)
    }

    pub fn return_type_and_throws_for_method<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        module_path: &RPath<'ir>,
        self_type: &uniffi_meta::Type,
    ) -> Result<crate::types::ReturnType> {
        self._return_type_and_throws(ir, cache, module_path, Some(self_type))
    }

    fn _return_type_and_throws<'ir>(
        &self,
        ir: &'ir Ir,
        cache: &mut LookupCache<'ir>,
        module_path: &RPath<'ir>,
        self_ty: Option<&uniffi_meta::Type>,
    ) -> Result<crate::types::ReturnType> {
        module_path.resolve_return_type(ir, cache, &self.signature.output, self_ty)
    }
}
