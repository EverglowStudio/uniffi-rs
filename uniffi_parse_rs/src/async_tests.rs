/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{attrs, CompileEnv, Function, Impl, Ir, Item, MetadataGroup, ReturnType};
use uniffi_meta::{Metadata, ObjectImpl, Type};

fn metadata(target: &str, features: Vec<String>) -> MetadataGroup {
    let ir = Ir::new_for_test_with_env(
        &["async_declarations"],
        CompileEnv::new(target.parse().unwrap(), features),
    );
    ir.into_metadata_group_map()
        .unwrap()
        .remove("async_declarations")
        .unwrap()
}

fn function<'a>(group: &'a MetadataGroup, name: &str) -> &'a uniffi_meta::FnMetadata {
    group
        .items
        .iter()
        .find_map(|m| match m {
            Metadata::Func(f) if f.name == name => Some(f),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing function {name}"))
}

fn error(name: &str) -> Type {
    Type::Enum {
        module_path: "async_declarations".into(),
        name: name.into(),
    }
}

fn stream(fallible: bool, send: bool) -> Type {
    Type::Stream {
        item_type: Box::new(Type::Optional {
            inner_type: Box::new(Type::UInt32),
        }),
        error_type: fallible.then(|| Box::new(error("ItemError"))),
        is_send: send,
    }
}

#[test]
fn equivalent_declarations_emit_identical_metadata() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    for name in [
        "async_value",
        "impl_value",
        "boxed_value",
        "alias_value",
        "local_alias_value",
        "future_result_alias",
    ] {
        let f = function(&group, name);
        assert!(f.is_async, "{name}");
        assert_eq!(f.return_type, Some(Type::UInt32));
        assert_eq!(f.throws, Some(error("FactoryError")));
    }
    for name in [
        "values",
        "boxed_values",
        "alias_values",
        "local_alias_values",
    ] {
        let f = function(&group, name);
        assert!(!f.is_async);
        assert_eq!(f.return_type, Some(stream(false, true)), "{name}");
        assert_eq!(f.throws, None);
    }
    for name in ["factory", "future_factory"] {
        let f = function(&group, name);
        assert!(f.is_async);
        assert_eq!(f.return_type, Some(stream(true, true)));
        assert_eq!(f.throws, Some(error("FactoryError")));
    }
    let ir = Ir::new_for_test(&["async_declarations"]);
    let future_alias = ir
        .crate_roots()
        .next()
        .unwrap()
        .items
        .iter()
        .find_map(|i| match i {
            Item::Fn(f) if f.ident == "future_result_alias" => Some(f),
            _ => None,
        })
        .unwrap();
    assert_eq!(future_alias.return_ty.signature.await_count, 1);
    assert!(future_alias.return_ty.signature.future.is_some());
}

#[test]
fn methods_and_traits_share_factory_and_input_stream_semantics() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    let mut checked = 0;
    for meta in &group.items {
        match meta {
            Metadata::Method(m) if m.self_name == "Shared" && m.name == "factory" => {
                assert!(m.is_async);
                assert_eq!(m.return_type, Some(stream(true, true)));
                assert_eq!(m.throws, Some(error("FactoryError")));
                checked += 1;
            }
            Metadata::TraitMethod(m) if m.name == "factory" => {
                assert!(m.is_async);
                assert_eq!(m.return_type, Some(stream(true, true)));
                assert_eq!(m.throws, Some(error("FactoryError")));
                checked += 1;
            }
            Metadata::Method(m) if m.self_name == "Shared" && m.name == "consume" => {
                assert!(m.is_async);
                assert_eq!(
                    m.inputs[0].ty,
                    Type::InputStream {
                        item_type: Box::new(Type::UInt32),
                        error_type: None,
                        is_send: true
                    }
                );
                checked += 1;
            }
            Metadata::TraitMethod(m) if m.name == "consume" => {
                assert!(m.is_async);
                assert_eq!(
                    m.inputs[0].ty,
                    Type::InputStream {
                        item_type: Box::new(Type::UInt32),
                        error_type: Some(Box::new(error("ItemError"))),
                        is_send: true
                    }
                );
                checked += 1;
            }
            Metadata::TraitMethod(m) if m.name == "stream" => {
                assert!(!m.is_async);
                assert_eq!(m.return_type, Some(stream(false, true)));
                checked += 1;
            }
            _ => (),
        }
    }
    assert_eq!(checked, 5);
    assert_eq!(
        function(&group, "consume").inputs[0].ty,
        Type::InputStream {
            item_type: Box::new(Type::UInt32),
            error_type: None,
            is_send: true
        }
    );
}

#[test]
fn stream_transport_tracks_target_and_enabled_feature() {
    for (target, features, send) in [
        (
            "aarch64-apple-darwin",
            vec!["wasm-unstable-single-threaded".into()],
            true,
        ),
        ("wasm32-unknown-unknown", vec![], true),
        (
            "wasm32-unknown-unknown",
            vec!["wasm-unstable-single-threaded".into()],
            false,
        ),
    ] {
        let group = metadata(target, features);
        assert_eq!(
            function(&group, "values").return_type,
            Some(stream(false, send))
        );
        assert_eq!(
            function(&group, "boxed_values").return_type,
            Some(stream(false, send))
        );
        assert_eq!(
            function(&group, "factory").return_type,
            Some(stream(true, send))
        );
        assert!(
            matches!(&function(&group, "consume").inputs[0].ty, Type::InputStream { is_send, .. } if *is_send == send)
        );
    }
}

#[test]
fn local_objects_keep_definition_paths_and_generate_close_once() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    let object = group
        .items
        .iter()
        .find_map(|m| match m {
            Metadata::Object(o) if o.name == "Counter" => Some(o),
            _ => None,
        })
        .unwrap();
    assert_eq!(object.imp, ObjectImpl::LocalStruct);
    assert_eq!(object.module_path, "async_declarations::owner");
    let constructor = group
        .items
        .iter()
        .find_map(|m| match m {
            Metadata::Constructor(c) if c.self_name == "Counter" && c.name == "new" => Some(c),
            _ => None,
        })
        .unwrap();
    assert!(constructor.is_async);
    assert_eq!(
        constructor.self_type,
        Some(Type::Object {
            module_path: object.module_path.clone(),
            name: "Counter".into(),
            imp: ObjectImpl::LocalStruct
        })
    );
    assert_eq!(constructor.throws, Some(error("FactoryError")));
    let from_owner = group
        .items
        .iter()
        .find_map(|m| match m {
            Metadata::Constructor(c) if c.self_name == "Counter" && c.name == "from_owner" => {
                Some(c)
            }
            _ => None,
        })
        .unwrap();
    assert_eq!(from_owner.module_path, "async_declarations::factory_module");
    assert_eq!(from_owner.self_type, constructor.self_type);
    let methods = group
        .items
        .iter()
        .filter_map(|m| match m {
            Metadata::Method(m) if m.self_name == "Counter" => Some(m),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(methods.len(), 6);
    assert!(methods.iter().all(|m| m.is_async));
    assert_eq!(methods.iter().filter(|m| m.name == "close").count(), 1);
}

#[test]
fn unsupported_returns_generics_and_records_have_source_diagnostics() {
    for source in [
        "pub fn bad<T>() -> T { todo!() }",
        "pub fn bad() -> impl Future<Output = &str> { todo!() }",
        "pub fn bad() -> impl Stream<Item = BoxStream<'static, u32>> { todo!() }",
        "pub fn bad() -> Vec<LocalBoxFuture<'static, u32>> { todo!() }",
    ] {
        let f = syn::parse_str(source).unwrap();
        let err = Function::parse(attrs::FunctionAttributes::default(), f)
            .err()
            .expect(source);
        assert!(err.span().start().column > 0, "{source}");
    }
    let field: syn::Field = syn::parse_quote!(pub future: BoxFuture<'static, u32>);
    assert!(crate::Field::parse(attrs::FieldAttributes::default(), field).is_err());
    let output: syn::Signature = syn::parse_quote!(fn items() -> impl Future<Output = Result<impl Stream<Item = Option<u32>>, Error>>);
    let rt = ReturnType::from_signature(&output).unwrap();
    assert_eq!(rt.return_ty, output.output);
}

#[test]
fn local_declaration_rejections_match_macro_contract() {
    for source in [
        "impl Counter { pub fn close(&self) {} }",
        "impl Counter { #[uniffi::method(name = \"close\")] pub fn renamed(&self) {} }",
        "impl Counter { pub fn wrong(self: Arc<Self>) {} }",
        "impl Counter { pub fn wrong(&mut self) {} }",
        "impl Counter { #[uniffi::constructor] pub fn new() -> Arc<LocalObject<Self>> { todo!() } }",
        "impl<T> Counter<T> { pub fn wrong(&self) {} }",
    ] {
        let attrs = attrs::ImplAttributes { local: true, ..Default::default() };
        let err = Impl::parse(&CompileEnv::new_for_test(), attrs, syn::parse_str(source).unwrap()).err().expect(source);
        assert!(!err.to_compile_error().is_empty());
    }
}

#[test]
fn aliases_cannot_hide_nested_streams_or_record_fields() {
    let ir = Ir::new_for_test(&["async_declarations"]);
    let path = crate::paths::tests::path_for_module(&ir, "async_declarations");
    let mut cache = crate::paths::LookupCache::default();
    for ty in [
        syn::parse_quote!(HiddenNested),
        syn::parse_quote!(impl Stream<Item = HiddenStream>),
    ] {
        let err = path
            .resolve_uniffi_meta_type(&ir, &mut cache, &ty, None)
            .unwrap_err();
        assert!(err.to_string().contains("nested streams"));
    }
    let borrowed: syn::Type = syn::parse_quote!(&UniFfiInputStream<Value, ItemError>);
    assert!(path
        .resolve_arg(&ir, &mut cache, &borrowed, None)
        .unwrap_err()
        .to_string()
        .contains("ownership"));
    let field = crate::Field::parse(
        attrs::FieldAttributes::default(),
        syn::parse_quote!(pub items: HiddenStream),
    )
    .unwrap();
    let err = field
        .create_field_metadata(&ir, &mut cache, &path)
        .err()
        .unwrap();
    assert!(err.to_string().contains("record fields"));
}

#[test]
fn local_constructor_shape_and_export_markers_are_checked() {
    let ir = Ir::new_for_test(&["async_declarations"]);
    let path = crate::paths::tests::path_for_module(&ir, "async_declarations::owner");
    let mut cache = crate::paths::LookupCache::default();
    for (ty, valid) in [
        (syn::parse_quote!(Arc<LocalObject<Self>>), true),
        (
            syn::parse_quote!(Result<Arc<LocalObject<Self>>, FactoryError>),
            true,
        ),
        (syn::parse_quote!(Arc<Self>), false),
        (syn::parse_quote!(Self), false),
        (syn::parse_quote!(LocalObject<Self>), false),
    ] {
        assert_eq!(
            path.validate_local_constructor(&ir, &mut cache, &ty)
                .is_ok(),
            valid
        );
    }
    let attrs = attrs::ImplAttributes::default();
    let imp = Impl::parse(
        &CompileEnv::new_for_test(),
        attrs,
        syn::parse_quote!(impl Counter { pub fn value(&self) -> u32 { 0 } }),
    )
    .unwrap();
    let err = imp.impl_metadata(&ir, &mut cache, &path).unwrap_err();
    assert!(err.to_string().contains("markers must match"));
}

#[test]
fn lifetime_parameters_preserve_borrowed_inputs() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    let f = function(&group, "borrowed_input");
    assert!(f.is_async);
    assert!(f.inputs[0].by_ref);
    assert_eq!(f.inputs[0].ty, Type::String);
    assert_eq!(f.return_type, Some(Type::UInt32));
}

#[test]
fn functions_methods_and_traits_reject_noncanonical_stream_arguments() {
    let ir = Ir::new_for_test(&["async_declarations"]);
    let path = crate::paths::tests::path_for_module(&ir, "async_declarations");
    let mut cache = crate::paths::LookupCache::default();
    let self_ty = Type::Object {
        module_path: "async_declarations".into(),
        name: "Shared".into(),
        imp: ObjectImpl::Struct,
    };
    let trait_ty = Type::Object {
        module_path: "async_declarations".into(),
        name: "Service".into(),
        imp: ObjectImpl::Trait(uniffi_meta::TraitKind::Both),
    };
    for source in [
        "impl Stream<Item = Value>",
        "Pin<Box<dyn Stream<Item = Value> + Send>>",
        "BoxStream<'static, Value>",
        "LocalBoxStream<'static, Result<Value, ItemError>>",
        "UniFfiStream<Value, ItemError>",
        "HiddenStream",
        "&HiddenStream",
    ] {
        let ty: syn::Type = syn::parse_str(source).unwrap();
        let function = Function::parse(
            attrs::FunctionAttributes::default(),
            syn::parse_quote!(pub fn bad(input: #ty) {}),
        )
        .unwrap();
        let method = crate::Method::parse(
            attrs::MethodAttributes::default(),
            syn::parse_quote!(pub fn bad(&self, input: #ty) {}),
        )
        .unwrap();
        let trait_method = crate::traits::TraitMethod::parse(
            attrs::MethodAttributes::default(),
            syn::parse_quote!(fn bad(&self, input: #ty);),
        )
        .unwrap();
        let errors = [
            function.args[0]
                .create_fn_metadata(&ir, &mut cache, &path, &attrs::DefaultMap::default())
                .unwrap_err(),
            method
                .to_method_metadata(&ir, &mut cache, &path, "Shared", &self_ty)
                .unwrap_err(),
            trait_method
                .to_trait_method_metadata(&ir, &mut cache, &path, "Service", &trait_ty, 0)
                .unwrap_err(),
        ];
        for err in errors {
            assert_eq!(err.kind, crate::ErrorKind::SynError("stream parameters must use uniffi::UniFfiInputStream<T, E> directly; nested streams and Pin<Box<dyn Stream<...>>> parameters are not supported".into()), "{source}");
        }
    }
}

#[test]
fn infallible_errors_normalize_streams_results_and_all_callable_kinds() {
    for (target, features, send) in [
        ("aarch64-apple-darwin", vec![], true),
        (
            "wasm32-unknown-unknown",
            vec!["wasm-unstable-single-threaded".into()],
            false,
        ),
    ] {
        let group = metadata(target, features);
        for (name, is_async, rust_async) in [
            ("canonical_values", false, false),
            ("canonical_values_imported", false, false),
            ("canonical_values_aliased", false, false),
            ("infallible_items", false, false),
            ("infallible_factory", true, true),
            ("infallible_future_factory", true, false),
        ] {
            let f = function(&group, name);
            assert_eq!(f.return_type, Some(stream(false, send)), "{name}");
            assert_eq!(f.throws, None, "{name}");
            assert_eq!((f.is_async, f.rust_async), (is_async, rust_async), "{name}");
        }
        for (name, is_async, rust_async) in [
            ("infallible_value", false, false),
            ("infallible_async_value", true, true),
            ("infallible_future_value", true, false),
        ] {
            let f = function(&group, name);
            assert_eq!(f.return_type, Some(Type::UInt32), "{name}");
            assert_eq!(f.throws, None, "{name}");
            assert_eq!((f.is_async, f.rust_async), (is_async, rust_async), "{name}");
        }
        let unit = function(&group, "infallible_unit");
        assert_eq!((&unit.return_type, &unit.throws), (&None, &None));
        let input = Type::InputStream {
            item_type: Box::new(Type::UInt32),
            error_type: None,
            is_send: send,
        };
        assert_eq!(function(&group, "consume").inputs[0].ty, input);
        let mut checked = 0;
        for meta in &group.items {
            match meta {
                Metadata::Constructor(c) if c.self_name == "Shared" => {
                    assert_eq!(c.throws, None);
                    checked += 1;
                }
                Metadata::Method(m) if m.self_name == "Shared" && m.name == "consume" => {
                    assert_eq!(m.throws, None);
                    assert_eq!(m.return_type, Some(Type::UInt32));
                    assert_eq!(m.inputs[0].ty, input);
                    checked += 1;
                }
                Metadata::TraitMethod(m) if m.name == "consume_infallible" => {
                    assert_eq!(m.throws, None);
                    assert_eq!(m.return_type, Some(Type::UInt32));
                    assert_eq!(m.inputs[0].ty, input);
                    checked += 1;
                }
                _ => (),
            }
        }
        assert_eq!(checked, 3);
    }
}

#[test]
fn infallible_metadata_normalization_keeps_the_original_rust_result_item() {
    let sig: syn::Signature = syn::parse_quote!(
        fn values() -> impl Future<Output = Result<impl Stream<Item = Result<Option<u32>, std::convert::Infallible>>, core::convert::Infallible>>
    );
    let rt = ReturnType::from_signature(&sig).unwrap();
    assert_eq!(rt.return_ty, sig.output);
    assert_eq!(rt.signature.rust_return, sig.output);
    assert_eq!(rt.signature.await_count, 1);
    assert_eq!(
        rt.signature.throws,
        Some(syn::parse_quote!(core::convert::Infallible))
    );
    assert_eq!(
        rt.signature.stream.as_ref().unwrap().error,
        Some(syn::parse_quote!(std::convert::Infallible))
    );
    assert_eq!(
        rt.signature.stream.as_ref().unwrap().rust_type,
        syn::parse_quote!(impl Stream<Item = Result<Option<u32>, std::convert::Infallible>>)
    );
    let ir = Ir::new_for_test(&["async_declarations"]);
    let path = crate::paths::tests::path_for_module(&ir, "async_declarations");
    assert_eq!(
        rt.return_type_and_throws(&ir, &mut Default::default(), &path)
            .unwrap(),
        crate::types::ReturnType {
            ok: Some(stream(false, true)),
            err: None,
            rust_return_is_result: true,
            rust_stream_item_is_result: true,
        }
    );
}

#[test]
fn rust_result_shapes_survive_infallible_metadata() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    let mut collapsed = vec![];
    for (plain, wrapped) in [
        ("plain_value", "infallible_value"),
        ("values", "canonical_values"),
        ("values", "infallible_items"),
        ("plain_factory", "infallible_plain_factory"),
    ] {
        let mut plain = function(&group, plain).clone();
        let wrapped = function(&group, wrapped);
        assert_eq!(plain.return_type, wrapped.return_type);
        assert_eq!(plain.throws, None);
        assert_eq!(wrapped.throws, None);
        plain.name.clone_from(&wrapped.name);
        if &plain == wrapped {
            collapsed.push(wrapped.name.as_str());
        }
    }
    assert!(
        collapsed.is_empty(),
        "Rust Result shapes disappeared from metadata: {collapsed:?}"
    );
}

#[test]
fn resolved_rust_result_shapes_are_independent_of_host_errors() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    for (name, result, item) in [
        ("plain_value", false, false),
        ("infallible_value", true, false),
        ("infallible_async_value", true, false),
        ("infallible_future_value", true, false),
        ("infallible_unit", true, false),
        ("values", false, false),
        ("canonical_values", false, true),
        ("canonical_values_aliased", false, true),
        ("infallible_items", false, true),
        ("infallible_factory", true, true),
        ("infallible_future_factory", true, true),
        ("plain_factory", false, false),
        ("infallible_plain_factory", true, false),
    ] {
        let f = function(&group, name);
        assert_eq!(
            (f.rust_return_is_result, f.rust_stream_item_is_result),
            (result, item),
            "{name}"
        );
        assert_eq!(f.throws, None);
    }
    let mut checked = 0;
    for item in &group.items {
        match item {
            Metadata::Constructor(c) if c.self_name == "Shared" => {
                assert_eq!(
                    (c.rust_return_is_result, c.rust_stream_item_is_result),
                    (true, false)
                );
                assert_eq!(c.throws, None);
                checked += 1;
            }
            Metadata::Method(m) if m.self_name == "Shared" && m.name == "consume" => {
                assert_eq!(
                    (m.rust_return_is_result, m.rust_stream_item_is_result),
                    (true, false)
                );
                assert_eq!(m.throws, None);
                checked += 1;
            }
            Metadata::TraitMethod(m) if m.name == "consume_infallible" => {
                assert_eq!(
                    (m.rust_return_is_result, m.rust_stream_item_is_result),
                    (true, false)
                );
                assert_eq!(m.throws, None);
                checked += 1;
            }
            Metadata::TraitMethod(m) if m.name == "stream" => {
                assert_eq!(
                    (m.rust_return_is_result, m.rust_stream_item_is_result),
                    (false, false)
                );
                checked += 1;
            }
            Metadata::Method(m) if m.name == "close" => {
                assert_eq!(
                    (m.rust_return_is_result, m.rust_stream_item_is_result),
                    (false, false)
                );
                checked += 1;
            }
            _ => (),
        }
    }
    assert_eq!(checked, 5);
}

#[test]
fn async_fn_returning_future_reports_the_return_span() {
    use syn::spanned::Spanned;

    for source in [
        "pub async fn bad() -> impl Future<Output = u32> { todo!() }",
        "pub async fn bad() -> Pin<Box<dyn Future<Output = u32> + Send>> { todo!() }",
        "pub async fn bad() -> BoxFuture<'static, u32> { todo!() }",
        "pub async fn bad() -> LocalBoxFuture<'static, u32> { todo!() }",
        "pub async fn bad() -> impl Future<Output = Result<impl Stream<Item = u32>, Error>> { todo!() }",
    ] {
        let function: syn::ItemFn = syn::parse_str(source).unwrap();
        let syn::ReturnType::Type(_, ty) = &function.sig.output else { unreachable!() };
        let return_start = ty.span().start();
        let err = Function::parse(attrs::FunctionAttributes::default(), function).err().expect(source);
        assert_eq!(err.to_string(), "nested Future returns are not supported; await the inner future in Rust");
        assert_eq!(err.span().start(), return_start, "{source}");
    }
}

#[test]
fn rust_async_preserves_declaration_form_for_all_callable_kinds() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    for (name, rust_async) in [
        ("async_value", true),
        ("impl_value", false),
        ("alias_value", false),
        ("factory", true),
        ("future_factory", false),
    ] {
        let f = function(&group, name);
        assert!(f.is_async);
        assert_eq!(f.rust_async, rust_async, "{name}");
    }
    for meta in &group.items {
        match meta {
            Metadata::Constructor(c) if c.self_name == "Counter" => {
                assert!(c.is_async);
                assert_eq!(c.rust_async, c.name == "new");
            }
            Metadata::Method(m) if m.self_name == "Counter" => {
                assert!(m.is_async);
                assert_eq!(
                    m.rust_async,
                    m.name == "close" || m.name == "trait_future",
                    "{}",
                    m.name
                );
            }
            Metadata::TraitMethod(m) if m.trait_name == "Service" => {
                assert_eq!(m.rust_async, m.name == "plain");
            }
            _ => (),
        }
    }
}

#[test]
fn plain_trait_local_impl_exports_only_owner_object_methods() {
    let group = metadata("aarch64-apple-darwin", vec![]);
    assert!(!group.items.iter().any(|m| matches!(m,
        Metadata::Object(o) if o.name == "PlainRustTrait")
        || matches!(m,
        Metadata::TraitMethod(m) if m.trait_name == "PlainRustTrait")));
    let methods = group
        .items
        .iter()
        .filter_map(|m| match m {
            Metadata::Method(m) if m.self_name == "Counter" && m.name.starts_with("trait_") => {
                Some(m)
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(methods.len(), 3);
    assert!(methods.iter().all(|m| m.is_async));
    let stream = methods.iter().find(|m| m.name == "trait_stream").unwrap();
    assert!(matches!(
        stream.return_type,
        Some(Type::Stream {
            error_type: None,
            ..
        })
    ));
    assert!(!stream.rust_async);
}

fn parse_foreign_bounds_trait(
    source: &str,
    export_ty: attrs::TraitExportType,
    target: &str,
    features: Vec<String>,
) -> syn::Result<crate::Trait> {
    crate::Trait::parse(
        &CompileEnv::new(target.parse().unwrap(), features),
        attrs::TraitAttributes {
            name: None,
            export_ty,
            docstring: None,
        },
        syn::parse_str(source).unwrap(),
    )
}

#[test]
fn foreign_trait_async_bounds_use_real_target_and_point_to_the_bound() {
    use attrs::TraitExportType;
    use uniffi_meta::TraitKind;
    for export in [
        TraitExportType::CallbackInterface,
        TraitExportType::TraitInterface(TraitKind::Both),
        TraitExportType::TraitInterface(TraitKind::ForeignOnly),
    ] {
        for (source, marker, rejected_native, rejected_wasm) in [
            ("trait Test { fn run(&self) -> impl Future<Output = u32> + Sync; }", "Sync", true, true),
            ("trait Test { fn run(&self) -> Pin<Box<dyn Future<Output = u32> + Send>>; }", "Send", false, true),
            ("trait Test { fn run(&self) -> impl Stream<Item = u32> + SyncOutsideWasm; }", "SyncOutsideWasm", true, false),
            ("trait Test { fn run(&self) -> BoxFuture<'static, Result<impl Stream<Item = u32> + Send, Error>>; }", "Send", false, true),
            ("trait Test { async fn run(&self) -> Result<impl Stream<Item = u32> + Sync, Error>; }", "Sync", true, true),
        ] {
            for (target, rejected) in [("aarch64-apple-darwin", rejected_native), ("wasm32-unknown-unknown", rejected_wasm)] {
                // Architecture, not runtime-feature detection, controls this declaration boundary.
                for features in [vec![], vec!["wasm-unstable-single-threaded".into()]] {
                    let result = parse_foreign_bounds_trait(source, export, target, features);
                    if rejected {
                        let err = result.err().expect(source);
                        assert_eq!(err.span().start().column, source.rfind(marker).unwrap(), "{source}");
                    } else {
                        assert!(result.is_ok(), "{source}: {target}");
                    }
                }
            }
        }
    }
}

#[test]
fn foreign_trait_portable_markers_and_project_aliases_work_on_both_targets() {
    use attrs::TraitExportType;
    use uniffi_meta::TraitKind;
    for target in ["aarch64-apple-darwin", "wasm32-unknown-unknown"] {
        for source in [
            "trait Test { fn run(&self) -> impl Future<Output = u32> + SendOutsideWasm; }",
            "trait Test { fn run(&self) -> impl Stream<Item = u32> + SendOutsideWasm; }",
            "trait Test { fn run(&self) -> uniffi::async_runtime::BoxFuture<'static, Result<uniffi::async_runtime::BoxStream<'static, u32>, Error>>; }",
            "trait Test { fn run(&self) -> uniffi::async_runtime::BoxStream<'static, u32>; }",
        ] {
            assert!(parse_foreign_bounds_trait(source, TraitExportType::TraitInterface(TraitKind::Both), target, vec![]).is_ok(), "{source}: {target}");
        }
    }
}

#[test]
fn rust_only_free_and_inherent_declarations_keep_explicit_send_sync() {
    use attrs::TraitExportType;
    use uniffi_meta::TraitKind;
    let declaration = "trait Test { fn run(&self) -> impl Future<Output = impl Stream<Item = u32> + Send + Sync> + Send + Sync; }";
    for target in ["aarch64-apple-darwin", "wasm32-unknown-unknown"] {
        assert!(parse_foreign_bounds_trait(
            declaration,
            TraitExportType::TraitInterface(TraitKind::RustOnly),
            target,
            vec![]
        )
        .is_ok());
        let env = CompileEnv::new(target.parse().unwrap(), vec![]);
        assert!(Impl::parse(
            &env,
            attrs::ImplAttributes::default(),
            syn::parse_quote!(impl Shared {
                pub fn run(&self) -> impl Future<Output = u32> + Send + Sync { todo!() }
            })
        )
        .is_ok());
    }
    assert!(
        Function::parse(
            attrs::FunctionAttributes::default(),
            syn::parse_quote!(
                pub fn run(
                ) -> impl Future<Output = impl Stream<Item = u32> + Send + Sync> + Send + Sync
                {
                    todo!()
                }
            )
        )
        .is_ok()
    );
}
