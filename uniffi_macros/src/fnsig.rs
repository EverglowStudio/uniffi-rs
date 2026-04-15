/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{
    default::{default_value_metadata_calls, DefaultValue},
    export::{AsyncRuntime, DefaultMap, ExportFnArgs},
    ffiops,
    util::{
        create_metadata_items, ident_to_string, mod_path, orig_name_metadata,
        try_metadata_value_from_usize,
    },
};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{spanned::Spanned, FnArg, Ident, Pat, Receiver, ReturnType, Type};

/// Syntactic check for `&[u8]`. Matches the bare identifier `u8` only —
/// fully-qualified paths like `&[::std::primitive::u8]` or user-defined
/// type aliases named `u8` are not recognized. In practice these forms
/// are vanishingly rare for byte slice arguments.
fn is_u8_slice(ty: &Type) -> bool {
    if let Type::Slice(s) = ty {
        if let Type::Path(p) = &*s.elem {
            return p.path.is_ident("u8");
        }
    }
    false
}

pub(crate) struct FnSignature {
    pub kind: FnKind,
    pub span: Span,
    pub mod_path: String,
    // The identifier of the Rust function.
    pub ident: Ident,
    // The foreign name for this function, usually == ident.
    pub name: String,
    // Did `self.name` come from an attribute
    pub name_from_attrs: bool,
    pub is_async: bool,
    pub async_runtime: Option<AsyncRuntime>,
    pub receiver: Option<ReceiverArg>,
    pub args: Vec<NamedArg>,
    pub return_ty: TokenStream,
    pub stream_return: Option<StreamReturnType>,
    // Does this the return type look like a result?
    // Only use this in UDL mode.
    // In general, it's not reliable because it fails for type aliases.
    pub looks_like_result: bool,
    pub docstring: String,
}

impl FnSignature {
    pub(crate) fn new_function(
        sig: syn::Signature,
        args: ExportFnArgs,
        docstring: String,
    ) -> syn::Result<Self> {
        Self::new(FnKind::Function, sig, args, docstring)
    }

    pub(crate) fn new_method(
        self_ident: Ident,
        foreign_self_ident: Ident,
        sig: syn::Signature,
        args: ExportFnArgs,
        docstring: String,
    ) -> syn::Result<Self> {
        Self::new(
            FnKind::Method {
                self_ident,
                foreign_self_ident,
            },
            sig,
            args,
            docstring,
        )
    }

    pub(crate) fn new_constructor(
        self_ident: Ident,
        foreign_self_ident: Ident,
        sig: syn::Signature,
        args: ExportFnArgs,
        docstring: String,
    ) -> syn::Result<Self> {
        Self::new(
            FnKind::Constructor {
                self_ident,
                foreign_self_ident,
            },
            sig,
            args,
            docstring,
        )
    }

    pub(crate) fn new_trait_method(
        self_ident: Ident,
        sig: syn::Signature,
        args: ExportFnArgs,
        index: u32,
        docstring: String,
    ) -> syn::Result<Self> {
        Self::new(
            FnKind::TraitMethod { self_ident, index },
            sig,
            args,
            docstring,
        )
    }

    pub(crate) fn new(
        kind: FnKind,
        sig: syn::Signature,
        mut export_fn_args: ExportFnArgs,
        docstring: String,
    ) -> syn::Result<Self> {
        let span = sig.span();
        let ident = sig.ident;
        let looks_like_result = looks_like_result(&sig.output);
        let stream_return = stream_return_type(&sig.output)?;
        if stream_return.is_some() && !matches!(&kind, FnKind::Function) {
            return Err(syn::Error::new(
                span,
                "native stream returns are currently only supported for top-level functions",
            ));
        }
        if stream_return.is_some() && sig.asyncness.is_some() {
            return Err(syn::Error::new(
                span,
                "native stream-returning functions must be synchronous; the stream itself is asynchronous",
            ));
        }

        let output = match sig.output {
            ReturnType::Default => quote! { () },
            ReturnType::Type(_, ty) => quote! { #ty },
        };
        let is_async = sig.asyncness.is_some();

        let mut input_iter = sig
            .inputs
            .into_iter()
            .map(|a| Arg::new(a, &mut export_fn_args.defaults))
            .peekable();

        let receiver = input_iter
            .next_if(|a| matches!(a, Ok(a) if a.is_receiver()))
            .map(|a| match a {
                Ok(Arg {
                    kind: ArgKind::Receiver(r),
                    ..
                }) => r,
                _ => unreachable!(),
            });
        let args = input_iter
            .map(|a| {
                a.and_then(|a| match a.kind {
                    ArgKind::Named(named) => Ok(named),
                    ArgKind::Receiver(_) => {
                        Err(syn::Error::new(a.span, "Unexpected receiver argument"))
                    }
                })
            })
            .collect::<syn::Result<Vec<_>>>()?;
        let has_input_stream_arg = args.iter().any(|arg| arg.input_stream.is_some());
        if has_input_stream_arg && !matches!(&kind, FnKind::Function) {
            return Err(syn::Error::new(
                span,
                "input stream parameters are currently only supported for top-level functions",
            ));
        }
        if has_input_stream_arg && stream_return.is_some() {
            return Err(syn::Error::new(
                span,
                "bidirectional streams are not supported yet",
            ));
        }

        if let Some(ident) = export_fn_args.defaults.idents().first() {
            return Err(syn::Error::new(
                ident.span(),
                format!("Unknown default argument: {}", ident),
            ));
        }

        if !is_async && export_fn_args.async_runtime.is_some() {
            return Err(syn::Error::new(
                export_fn_args.async_runtime.span(),
                "Function not async".to_string(),
            ));
        }

        Ok(Self {
            kind,
            span,
            mod_path: mod_path()?,
            name_from_attrs: export_fn_args.name.is_some(),
            name: export_fn_args
                .name
                .unwrap_or_else(|| ident_to_string(&ident)),
            ident,
            is_async,
            async_runtime: export_fn_args.async_runtime,
            receiver,
            args,
            return_ty: output,
            stream_return,
            looks_like_result,
            docstring,
        })
    }

    /// Generate a closure that tries to lift all arguments into a tuple.
    ///
    /// The closure moves all scaffolding arguments into itself and returns:
    ///   - The lifted argument tuple on success
    ///   - The field name and error on failure (`Err(&'static str, anyhow::Error>`)
    pub fn lift_closure(&self, self_lift: Option<TokenStream>) -> TokenStream {
        let arg_lifts = self.args.iter().map(|arg| {
            let ident = &arg.ident;
            let name = &arg.name;
            if let Some(input_stream) = &arg.input_stream {
                let cell_ident = self.input_stream_callback_cell_ident(arg);
                let item_ty = &input_stream.item_ty;
                let error_ty = &input_stream.error_ty;
                quote! {
                    {
                        if #ident.as_raw() == 0 {
                            return ::std::result::Result::Err((
                                #name,
                                ::uniffi::deps::anyhow::anyhow!("input stream handle was null")
                            ));
                        }
                        let (uniffi_input_stream_next, uniffi_input_stream_cancel) =
                            match #cell_ident.get() {
                                ::std::option::Option::Some(callbacks) => *callbacks,
                                ::std::option::Option::None => {
                                    return ::std::result::Result::Err((
                                        #name,
                                        ::uniffi::deps::anyhow::anyhow!(
                                            "input stream callbacks were not registered"
                                        )
                                    ));
                                }
                            };
                        ::uniffi::UniFfiInputStream::<#item_ty, #error_ty>::from_foreign_callbacks::<crate::UniFfiTag>(
                            #ident,
                            uniffi_input_stream_next,
                            uniffi_input_stream_cancel,
                        )
                    }
                }
            } else {
                let try_lift = ffiops::try_lift(&arg.ty);
                quote! {
                    match #try_lift(#ident) {
                        ::std::result::Result::Ok(v) => v,
                        ::std::result::Result::Err(e) => {
                            return ::std::result::Result::Err((#name, e))
                        }
                    }
                }
            }
        });
        let all_lifts = self_lift.into_iter().chain(arg_lifts);
        quote! {
            move || ::std::result::Result::Ok((
                #(#all_lifts,)*
            ))
        }
    }

    pub(crate) fn input_stream_callback_cell_ident(&self, arg: &NamedArg) -> Ident {
        let ffi_name = uniffi_meta::fn_symbol_name(&self.mod_path, &self.name);
        Ident::new(
            &format!("UNIFFI_INPUT_STREAM_CALLBACKS_{}_{}", ffi_name, arg.name)
                .to_ascii_uppercase(),
            Span::call_site(),
        )
    }

    pub(crate) fn input_stream_init_fn_ident(&self, arg: &NamedArg) -> Ident {
        Ident::new(
            &uniffi_meta::fn_input_stream_init_symbol_name(&self.mod_path, &self.name, &arg.name),
            Span::call_site(),
        )
    }

    pub(crate) fn input_stream_next_callback_type(
        input_stream: &InputStreamArgType,
    ) -> TokenStream {
        let item_ty = &input_stream.item_ty;
        let error_ty = &input_stream.error_ty;
        quote! {
            ::uniffi::ForeignInputStreamNextCallback<
                <::std::result::Result<::std::option::Option<#item_ty>, #error_ty> as ::uniffi::LiftReturn<crate::UniFfiTag>>::ReturnType
            >
        }
    }

    pub(crate) fn input_stream_callback_tuple_type(
        input_stream: &InputStreamArgType,
    ) -> TokenStream {
        let next_ty = Self::input_stream_next_callback_type(input_stream);
        quote! {
            (#next_ty, ::uniffi::ForeignInputStreamCancelCallback)
        }
    }

    /// Call a Rust function from a [Self::lift_closure] success.
    ///
    /// This takes an Ok value returned by `lift_closure` with the name `uniffi_args` and generates
    /// a series of parameters to pass to the Rust function.
    pub fn rust_call_params(&self, self_lift: bool) -> TokenStream {
        let start_idx = if self_lift { 1 } else { 0 };
        let args = self.args.iter().enumerate().map(|(i, arg)| {
            let idx = syn::Index::from(i + start_idx);
            let ty = &arg.ty;
            match &arg.ref_type {
                None => quote! { uniffi_args.#idx },
                Some(ref_type) => quote! {
                    <#ty as ::std::borrow::Borrow<#ref_type>>::borrow(&uniffi_args.#idx)
                },
            }
        });
        quote! { #(#args),* }
    }

    pub fn require_receiver(&self) -> syn::Result<ReceiverArg> {
        self.receiver
            .clone()
            .ok_or_else(|| syn::Error::new(self.span, "Expected receiver argument"))
    }

    /// Parameters expressions for each of our arguments
    pub fn params(&self) -> impl Iterator<Item = TokenStream> + '_ {
        self.args.iter().map(NamedArg::param)
    }

    /// Name of the scaffolding function to generate for this function
    pub fn scaffolding_fn_ident(&self) -> syn::Result<Ident> {
        let name = &self.name;
        let name = match &self.kind {
            FnKind::Function => uniffi_meta::fn_symbol_name(&self.mod_path, name),
            FnKind::Method {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                uniffi_meta::method_symbol_name(&self.mod_path, &object_name, name)
            }
            FnKind::TraitMethod { self_ident, .. } => {
                uniffi_meta::method_symbol_name(&self.mod_path, &ident_to_string(self_ident), name)
            }
            FnKind::Constructor {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                uniffi_meta::constructor_symbol_name(&self.mod_path, &object_name, name)
            }
        };
        Ok(Ident::new(&name, Span::call_site()))
    }

    /// Scaffolding parameters expressions for each of our arguments
    pub fn scaffolding_param_names(&self) -> impl Iterator<Item = TokenStream> + '_ {
        self.args.iter().map(|a| {
            let ident = &a.ident;
            quote! { #ident }
        })
    }

    pub fn scaffolding_param_types(&self) -> impl Iterator<Item = TokenStream> + '_ {
        self.args.iter().map(|a| ffiops::lift_type(&a.ty))
    }

    /// Generate metadata items for this function
    pub(crate) fn metadata_expr(&self) -> syn::Result<TokenStream> {
        let Self {
            name,
            name_from_attrs,
            return_ty,
            is_async,
            docstring,
            ..
        } = &self;
        let args_len = try_metadata_value_from_usize(
            // Use param_lifts to calculate this instead of sig.inputs to avoid counting any self
            // params
            self.args.len(),
            "UniFFI limits functions to 256 arguments",
        )?;
        let arg_metadata_calls = self
            .args
            .iter()
            .map(NamedArg::arg_metadata)
            .collect::<syn::Result<Vec<_>>>()?;

        let type_id_meta = ffiops::type_id_meta(return_ty);

        let orig_name = orig_name_metadata(*name_from_attrs, &self.ident);
        match &self.kind {
            FnKind::Function => Ok(quote! {
                ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::FUNC)
                    .concat_str(module_path!())
                    .concat_str(#name)
                    #orig_name
                    .concat_bool(#is_async)
                    .concat_value(#args_len)
                    #(#arg_metadata_calls)*
                    .concat(#type_id_meta)
                    .concat_long_str(#docstring)
            }),

            FnKind::Method {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                Ok(quote! {
                    ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::METHOD)
                        .concat_str(module_path!())
                        .concat_str(#object_name)
                        .concat_str(#name)
                        #orig_name
                        .concat_bool(#is_async)
                        .concat_value(#args_len)
                        #(#arg_metadata_calls)*
                        .concat(#type_id_meta)
                        .concat_long_str(#docstring)
                })
            }

            FnKind::TraitMethod { self_ident, index } => {
                let object_name = ident_to_string(self_ident);
                Ok(quote! {
                    ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::TRAIT_METHOD)
                        .concat_str(module_path!())
                        .concat_str(#object_name)
                        .concat_u32(#index)
                        .concat_str(#name)
                        #orig_name
                        .concat_bool(#is_async)
                        .concat_value(#args_len)
                        #(#arg_metadata_calls)*
                        .concat(#type_id_meta)
                        .concat_long_str(#docstring)
                })
            }

            FnKind::Constructor {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                Ok(quote! {
                    ::uniffi::MetadataBuffer::from_code(::uniffi::metadata::codes::CONSTRUCTOR)
                        .concat_str(module_path!())
                        .concat_str(#object_name)
                        .concat_str(#name)
                        #orig_name
                        .concat_bool(#is_async)
                        .concat_value(#args_len)
                        #(#arg_metadata_calls)*
                        .concat(#type_id_meta)
                        .concat_long_str(#docstring)
                })
            }
        }
    }

    pub(crate) fn metadata_items(&self) -> syn::Result<TokenStream> {
        let Self { name, .. } = &self;
        match &self.kind {
            FnKind::Function => Ok(create_metadata_items(
                "func",
                name,
                self.metadata_expr()?,
                Some(self.checksum_symbol_name()),
            )),

            FnKind::Method {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                Ok(create_metadata_items(
                    "method",
                    &format!("{object_name}_{name}"),
                    self.metadata_expr()?,
                    Some(self.checksum_symbol_name()),
                ))
            }

            FnKind::TraitMethod { self_ident, .. } => {
                let object_name = ident_to_string(self_ident);
                Ok(create_metadata_items(
                    "method",
                    &format!("{object_name}_{name}"),
                    self.metadata_expr()?,
                    Some(self.checksum_symbol_name()),
                ))
            }

            FnKind::Constructor {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                Ok(create_metadata_items(
                    "constructor",
                    &format!("{object_name}_{name}"),
                    self.metadata_expr()?,
                    Some(self.checksum_symbol_name()),
                ))
            }
        }
    }

    pub(crate) fn checksum_symbol_name(&self) -> String {
        let name = &self.name;
        match &self.kind {
            FnKind::Function => uniffi_meta::fn_checksum_symbol_name(&self.mod_path, name),
            FnKind::Method {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                uniffi_meta::method_checksum_symbol_name(&self.mod_path, &object_name, name)
            }
            FnKind::TraitMethod { self_ident, .. } => uniffi_meta::method_checksum_symbol_name(
                &self.mod_path,
                &ident_to_string(self_ident),
                name,
            ),
            FnKind::Constructor {
                foreign_self_ident, ..
            } => {
                let object_name = ident_to_string(foreign_self_ident);
                uniffi_meta::constructor_checksum_symbol_name(&self.mod_path, &object_name, name)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct StreamReturnType {
    pub(crate) item_ty: TokenStream,
    pub(crate) error_ty: TokenStream,
    pub(crate) is_send: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct InputStreamArgType {
    pub(crate) item_ty: TokenStream,
    pub(crate) error_ty: TokenStream,
    pub(crate) is_send: bool,
}

pub(crate) struct Arg {
    pub(crate) span: Span,
    pub(crate) kind: ArgKind,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum ArgKind {
    Receiver(ReceiverArg),
    Named(NamedArg),
}

impl Arg {
    fn new(syn_arg: FnArg, defaults: &mut DefaultMap) -> syn::Result<Self> {
        let span = syn_arg.span();
        let kind = match syn_arg {
            FnArg::Typed(p) => match *p.pat {
                Pat::Ident(i) => Ok(ArgKind::Named(NamedArg::new(i.ident, &p.ty, defaults)?)),
                _ => Err(syn::Error::new_spanned(p, "Argument name missing")),
            },
            FnArg::Receiver(receiver) => Ok(ArgKind::Receiver(ReceiverArg::from(receiver))),
        }?;

        Ok(Self { span, kind })
    }

    pub(crate) fn is_receiver(&self) -> bool {
        matches!(self.kind, ArgKind::Receiver(_))
    }
}

#[derive(Clone)]
pub(crate) enum ReceiverArg {
    Ref,
    Arc,
}

impl From<Receiver> for ReceiverArg {
    fn from(receiver: Receiver) -> Self {
        if let Type::Path(p) = *receiver.ty {
            if let Some(segment) = p.path.segments.last() {
                // This comparison will fail if a user uses a typedef for Arc.  Maybe we could
                // implement some system like TYPE_ID_META to figure this out from the type system.
                // However, this seems good enough for now.
                if segment.ident == "Arc" {
                    return ReceiverArg::Arc;
                }
            }
        }
        Self::Ref
    }
}

pub(crate) struct NamedArg {
    pub(crate) ident: Ident,
    pub(crate) name: String,
    pub(crate) ty: TokenStream,
    pub(crate) ref_type: Option<Type>,
    pub(crate) input_stream: Option<InputStreamArgType>,
    pub(crate) default: Option<DefaultValue>,
}

impl NamedArg {
    pub(crate) fn new(ident: Ident, ty: &Type, defaults: &mut DefaultMap) -> syn::Result<Self> {
        if let Some(input_stream) = input_stream_arg_type(ty)? {
            return Ok(Self {
                name: ident_to_string(&ident),
                ty: quote! { #ty },
                ref_type: None,
                input_stream: Some(input_stream),
                default: defaults.remove(&ident),
                ident,
            });
        }
        reject_stream_argument(ty)?;
        Ok(match ty {
            Type::Reference(r) => {
                let inner = &r.elem;
                let ty = if is_u8_slice(inner) {
                    quote! { ::uniffi::ForeignBytes }
                } else {
                    ffiops::lift_ref_type(inner)
                };
                Self {
                    name: ident_to_string(&ident),
                    ty,
                    ref_type: Some(*inner.clone()),
                    input_stream: None,
                    default: defaults.remove(&ident),
                    ident,
                }
            }
            _ => Self {
                name: ident_to_string(&ident),
                ty: quote! { #ty },
                ref_type: None,
                input_stream: None,
                default: defaults.remove(&ident),
                ident,
            },
        })
    }

    /// Generate the parameter for this Arg
    pub(crate) fn param(&self) -> TokenStream {
        let ident = &self.ident;
        let ty = &self.ty;
        quote! { #ident: #ty }
    }

    pub(crate) fn arg_metadata(&self) -> syn::Result<TokenStream> {
        let name = &self.name;
        let type_id_meta = ffiops::type_id_meta(&self.ty);
        let default_calls = default_value_metadata_calls(&self.default)?;
        let by_ref = self.ref_type.is_some();
        Ok(quote! {
            .concat_str(#name)
            .concat(#type_id_meta)
            .concat_bool(#by_ref)
            #default_calls
        })
    }
}

fn reject_stream_argument(ty: &Type) -> syn::Result<()> {
    if type_contains_stream_path(ty) {
        Err(syn::Error::new_spanned(
            ty,
            "stream parameters must use uniffi::UniFfiInputStream<T, E> directly; nested streams and Pin<Box<dyn Stream<...>>> parameters are not supported",
        ))
    } else {
        Ok(())
    }
}

fn type_contains_stream_path(ty: &Type) -> bool {
    match ty {
        Type::Path(path) => {
            path.path.segments.iter().any(|segment| {
                segment.ident == "Stream"
                    || segment.ident == "UniFfiStream"
                    || segment.ident == "UniFfiInputStream"
            }) || path.path.segments.iter().any(|segment| {
                if let syn::PathArguments::AngleBracketed(args) = &segment.arguments {
                    args.args.iter().any(|arg| match arg {
                        syn::GenericArgument::Type(ty) => type_contains_stream_path(ty),
                        syn::GenericArgument::AssocType(assoc) => {
                            type_contains_stream_path(&assoc.ty)
                        }
                        _ => false,
                    })
                } else {
                    false
                }
            })
        }
        Type::TraitObject(obj) => obj.bounds.iter().any(|bound| match bound {
            syn::TypeParamBound::Trait(trait_bound) => trait_bound
                .path
                .segments
                .iter()
                .any(|segment| segment.ident == "Stream"),
            _ => false,
        }),
        Type::Reference(r) => type_contains_stream_path(&r.elem),
        Type::Group(g) => type_contains_stream_path(&g.elem),
        Type::Paren(p) => type_contains_stream_path(&p.elem),
        Type::Tuple(t) => t.elems.iter().any(type_contains_stream_path),
        _ => false,
    }
}

fn stream_return_type(return_type: &ReturnType) -> syn::Result<Option<StreamReturnType>> {
    let ReturnType::Type(_, ty) = return_type else {
        return Ok(None);
    };
    if let Some((item_ty, error_ty)) = uniffi_stream_alias_args(ty)? {
        return Ok(Some(StreamReturnType {
            item_ty: quote! { #item_ty },
            error_ty: quote! { #error_ty },
            is_send: true,
        }));
    }
    if !type_contains_stream_path(ty) {
        return Ok(None);
    }

    let pin_inner = single_type_arg(ty, "Pin")?;
    let box_inner = single_type_arg(pin_inner, "Box")?;
    let Type::TraitObject(trait_object) = strip_grouped_type(box_inner) else {
        return Err(syn::Error::new_spanned(
            box_inner,
            "stream returns must use Pin<Box<dyn futures_core::Stream<Item = Result<T, E>> + Send + 'static>>",
        ));
    };

    let has_static = trait_object.bounds.iter().any(|bound| match bound {
        syn::TypeParamBound::Lifetime(lifetime) => lifetime.ident == "static",
        _ => false,
    });
    let has_send = trait_object.bounds.iter().any(|bound| match bound {
        syn::TypeParamBound::Trait(trait_bound) => trait_bound
            .path
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "Send"),
        _ => false,
    });
    if !has_static || !has_send {
        return Err(syn::Error::new_spanned(
            trait_object,
            "stream returns must be Send + 'static in the native ABI slice",
        ));
    }

    let item_assoc = trait_object.bounds.iter().find_map(|bound| {
        let syn::TypeParamBound::Trait(trait_bound) = bound else {
            return None;
        };
        let stream_segment = trait_bound
            .path
            .segments
            .iter()
            .find(|segment| segment.ident == "Stream")?;
        let syn::PathArguments::AngleBracketed(args) = &stream_segment.arguments else {
            return None;
        };
        args.args.iter().find_map(|arg| match arg {
            syn::GenericArgument::AssocType(assoc) if assoc.ident == "Item" => Some(&assoc.ty),
            _ => None,
        })
    });
    let item_assoc = item_assoc.ok_or_else(|| {
        syn::Error::new_spanned(
            trait_object,
            "stream returns must specify Stream<Item = Result<T, E>>",
        )
    })?;
    let (item_ty, error_ty) = result_type_args(item_assoc)?;
    Ok(Some(StreamReturnType {
        item_ty: quote! { #item_ty },
        error_ty: quote! { #error_ty },
        is_send: true,
    }))
}

fn uniffi_stream_alias_args(ty: &Type) -> syn::Result<Option<(&Type, &Type)>> {
    let Type::Path(path) = strip_grouped_type(ty) else {
        return Ok(None);
    };
    let Some(segment) = path.path.segments.last() else {
        return Ok(None);
    };
    if segment.ident != "UniFfiStream" {
        return Ok(None);
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "UniFfiStream returns must use UniFfiStream<T, E>",
        ));
    };
    let mut type_args = args.args.iter().filter_map(|arg| match arg {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    });
    let item_ty = type_args.next().ok_or_else(|| {
        syn::Error::new_spanned(ty, "UniFfiStream returns must use UniFfiStream<T, E>")
    })?;
    let error_ty = type_args.next().ok_or_else(|| {
        syn::Error::new_spanned(ty, "UniFfiStream returns must use UniFfiStream<T, E>")
    })?;
    if type_args.next().is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "UniFfiStream returns must have exactly two type arguments",
        ));
    }
    Ok(Some((item_ty, error_ty)))
}

fn input_stream_arg_type(ty: &Type) -> syn::Result<Option<InputStreamArgType>> {
    let Some((item_ty, error_ty)) = uniffi_input_stream_alias_args(ty)? else {
        return Ok(None);
    };
    Ok(Some(InputStreamArgType {
        item_ty: quote! { #item_ty },
        error_ty: quote! { #error_ty },
        is_send: true,
    }))
}

fn uniffi_input_stream_alias_args(ty: &Type) -> syn::Result<Option<(&Type, &Type)>> {
    let Type::Path(path) = strip_grouped_type(ty) else {
        return Ok(None);
    };
    let Some(segment) = path.path.segments.last() else {
        return Ok(None);
    };
    if segment.ident != "UniFfiInputStream" {
        return Ok(None);
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "UniFfiInputStream parameters must use UniFfiInputStream<T, E>",
        ));
    };
    let mut type_args = args.args.iter().filter_map(|arg| match arg {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    });
    let item_ty = type_args.next().ok_or_else(|| {
        syn::Error::new_spanned(
            ty,
            "UniFfiInputStream parameters must use UniFfiInputStream<T, E>",
        )
    })?;
    let error_ty = type_args.next().ok_or_else(|| {
        syn::Error::new_spanned(
            ty,
            "UniFfiInputStream parameters must use UniFfiInputStream<T, E>",
        )
    })?;
    if type_args.next().is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "UniFfiInputStream parameters must have exactly two type arguments",
        ));
    }
    Ok(Some((item_ty, error_ty)))
}

fn strip_grouped_type(ty: &Type) -> &Type {
    match ty {
        Type::Group(g) => strip_grouped_type(&g.elem),
        Type::Paren(p) => strip_grouped_type(&p.elem),
        _ => ty,
    }
}

fn single_type_arg<'a>(ty: &'a Type, expected: &str) -> syn::Result<&'a Type> {
    let Type::Path(path) = strip_grouped_type(ty) else {
        return Err(syn::Error::new_spanned(
            ty,
            format!("stream returns must be wrapped in {expected}<...>"),
        ));
    };
    let segment = path.path.segments.last().ok_or_else(|| {
        syn::Error::new_spanned(ty, format!("stream returns must use {expected}<...>"))
    })?;
    if segment.ident != expected {
        return Err(syn::Error::new_spanned(
            ty,
            format!("stream returns must use {expected}<...>"),
        ));
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            format!("stream returns must use {expected}<...>"),
        ));
    };
    let mut type_args = args.args.iter().filter_map(|arg| match arg {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    });
    let Some(inner) = type_args.next() else {
        return Err(syn::Error::new_spanned(
            ty,
            format!("stream returns must use {expected}<T>"),
        ));
    };
    if type_args.next().is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            format!("stream returns must use {expected}<T> with one type argument"),
        ));
    }
    Ok(inner)
}

fn result_type_args(ty: &Type) -> syn::Result<(&Type, &Type)> {
    let Type::Path(path) = strip_grouped_type(ty) else {
        return Err(syn::Error::new_spanned(
            ty,
            "stream Item must be Result<T, E>",
        ));
    };
    let segment = path
        .path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(ty, "stream Item must be Result<T, E>"))?;
    if segment.ident != "Result" {
        return Err(syn::Error::new_spanned(
            ty,
            "stream Item must be Result<T, E>; infallible Stream<Item = T> is not supported yet",
        ));
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(syn::Error::new_spanned(
            ty,
            "stream Item must be Result<T, E>",
        ));
    };
    let mut type_args = args.args.iter().filter_map(|arg| match arg {
        syn::GenericArgument::Type(ty) => Some(ty),
        _ => None,
    });
    let item_ty = type_args
        .next()
        .ok_or_else(|| syn::Error::new_spanned(ty, "stream Item must be Result<T, E>"))?;
    let error_ty = type_args
        .next()
        .ok_or_else(|| syn::Error::new_spanned(ty, "stream Item must be Result<T, E>"))?;
    if type_args.next().is_some() {
        return Err(syn::Error::new_spanned(
            ty,
            "stream Item Result must have exactly two type arguments",
        ));
    }
    Ok((item_ty, error_ty))
}

fn looks_like_result(return_type: &ReturnType) -> bool {
    if let ReturnType::Type(_, ty) = return_type {
        if let Type::Path(p) = &**ty {
            if let Some(seg) = p.path.segments.last() {
                if seg.ident == "Result" {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::{stream_return_type, FnKind, FnSignature, NamedArg};
    use crate::export::ExportFnArgs;
    use quote::quote;
    use syn::parse_quote;

    #[test]
    fn parses_send_static_result_stream_return() {
        let return_type: syn::ReturnType = parse_quote! {
            -> std::pin::Pin<
                Box<
                    dyn futures_core::Stream<Item = Result<u32, MyError>> + Send + 'static
                >
            >
        };
        let stream = stream_return_type(&return_type).unwrap().unwrap();
        assert_eq!(stream.item_ty.to_string(), quote! { u32 }.to_string());
        assert_eq!(stream.error_ty.to_string(), quote! { MyError }.to_string());
        assert!(stream.is_send);
    }

    #[test]
    fn parses_imported_stream_return_path() {
        let return_type: syn::ReturnType = parse_quote! {
            -> Pin<Box<dyn Stream<Item = Result<MyEvent, MyError>> + Send + 'static>>
        };
        let stream = stream_return_type(&return_type).unwrap().unwrap();
        assert_eq!(stream.item_ty.to_string(), quote! { MyEvent }.to_string());
        assert_eq!(stream.error_ty.to_string(), quote! { MyError }.to_string());
        assert!(stream.is_send);
    }

    #[test]
    fn parses_uniffi_stream_return_alias() {
        let return_type: syn::ReturnType = parse_quote! {
            -> uniffi::UniFfiStream<u32, MyError>
        };
        let stream = stream_return_type(&return_type).unwrap().unwrap();
        assert_eq!(stream.item_ty.to_string(), quote! { u32 }.to_string());
        assert_eq!(stream.error_ty.to_string(), quote! { MyError }.to_string());
        assert!(stream.is_send);
    }

    #[test]
    fn parses_bare_uniffi_stream_return_alias() {
        let return_type: syn::ReturnType = parse_quote! {
            -> UniFfiStream<MyRecord, crate::MyError>
        };
        let stream = stream_return_type(&return_type).unwrap().unwrap();
        assert_eq!(stream.item_ty.to_string(), quote! { MyRecord }.to_string());
        assert_eq!(
            stream.error_ty.to_string(),
            quote! { crate :: MyError }.to_string()
        );
        assert!(stream.is_send);
    }

    #[test]
    fn rejects_infallible_stream_return() {
        let return_type: syn::ReturnType = parse_quote! {
            -> std::pin::Pin<
                Box<
                    dyn futures_core::Stream<Item = u32> + Send + 'static
                >
            >
        };
        let error = stream_return_type(&return_type).unwrap_err().to_string();
        assert!(error.contains("Stream<Item = T> is not supported"));
    }

    #[test]
    fn rejects_stream_return_without_static() {
        let return_type: syn::ReturnType = parse_quote! {
            -> Pin<Box<dyn Stream<Item = Result<u32, MyError>> + Send>>
        };
        let error = stream_return_type(&return_type).unwrap_err().to_string();
        assert!(error.contains("Send + 'static"));
    }

    #[test]
    fn rejects_stream_return_without_send() {
        let return_type: syn::ReturnType = parse_quote! {
            -> Pin<Box<dyn Stream<Item = Result<u32, MyError>> + 'static>>
        };
        let error = stream_return_type(&return_type).unwrap_err().to_string();
        assert!(error.contains("Send + 'static"));
    }

    #[test]
    fn rejects_stream_parameters() {
        let ty: syn::Type = parse_quote! {
            Pin<Box<dyn Stream<Item = Result<u32, MyError>> + Send + 'static>>
        };
        let err = match NamedArg::new(parse_quote! { events }, &ty, &mut Default::default()) {
            Ok(_) => panic!("expected stream parameter rejection"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("Pin<Box<dyn Stream<...>>> parameters are not supported"));
    }

    #[test]
    fn parses_input_stream_parameter_alias() {
        let ty: syn::Type = parse_quote! {
            uniffi::UniFfiInputStream<u32, MyError>
        };
        let arg = NamedArg::new(parse_quote! { events }, &ty, &mut Default::default()).unwrap();
        let input_stream = arg.input_stream.unwrap();
        assert_eq!(input_stream.item_ty.to_string(), quote! { u32 }.to_string());
        assert_eq!(
            input_stream.error_ty.to_string(),
            quote! { MyError }.to_string()
        );
        assert!(input_stream.is_send);
    }

    #[test]
    fn parses_async_function_with_input_stream_parameter() {
        let sig: syn::Signature = parse_quote! {
            async fn sum_events(events: uniffi::UniFfiInputStream<u32, MyError>) -> Result<u64, MyError>
        };
        let sig = FnSignature::new(
            FnKind::Function,
            sig,
            ExportFnArgs::default(),
            String::new(),
        )
        .unwrap();
        assert_eq!(sig.args.len(), 1);
        assert!(sig.args[0].input_stream.is_some());
    }

    #[test]
    fn rejects_nested_input_stream_parameter() {
        let ty: syn::Type = parse_quote! {
            Option<uniffi::UniFfiInputStream<u32, MyError>>
        };
        let err = match NamedArg::new(parse_quote! { events }, &ty, &mut Default::default()) {
            Ok(_) => panic!("expected nested input stream parameter rejection"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("must use uniffi::UniFfiInputStream"));
    }

    #[test]
    fn rejects_method_input_stream_parameters() {
        let sig: syn::Signature = parse_quote! {
            fn consume(&self, events: uniffi::UniFfiInputStream<u32, MyError>)
        };
        let err = match FnSignature::new(
            FnKind::Method {
                self_ident: parse_quote! { MyObject },
                foreign_self_ident: parse_quote! { MyObject },
            },
            sig,
            ExportFnArgs::default(),
            String::new(),
        ) {
            Ok(_) => panic!("expected method input stream parameter rejection"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("only supported for top-level functions"));
    }

    #[test]
    fn rejects_input_stream_bidirectional_signature() {
        let sig: syn::Signature = parse_quote! {
            fn bidi(
                events: uniffi::UniFfiInputStream<u32, MyError>,
            ) -> uniffi::UniFfiStream<u32, MyError>
        };
        let err = match FnSignature::new(
            FnKind::Function,
            sig,
            ExportFnArgs::default(),
            String::new(),
        ) {
            Ok(_) => panic!("expected bidi stream rejection"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("bidirectional streams are not supported"));
    }

    #[test]
    fn rejects_method_stream_returns() {
        let sig: syn::Signature = parse_quote! {
            fn events(&self) -> Pin<Box<dyn Stream<Item = Result<u32, MyError>> + Send + 'static>>
        };
        let err = match FnSignature::new(
            FnKind::Method {
                self_ident: parse_quote! { MyObject },
                foreign_self_ident: parse_quote! { MyObject },
            },
            sig,
            ExportFnArgs::default(),
            String::new(),
        ) {
            Ok(_) => panic!("expected method stream return rejection"),
            Err(err) => err,
        };
        assert!(err
            .to_string()
            .contains("only supported for top-level functions"));
    }
}

#[derive(Debug)]
pub(crate) enum FnKind {
    Function,
    Constructor {
        self_ident: Ident,
        foreign_self_ident: Ident,
    },
    Method {
        self_ident: Ident,
        foreign_self_ident: Ident,
    },
    TraitMethod {
        self_ident: Ident,
        index: u32,
    },
}
