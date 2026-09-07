/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::{convert::Infallible, sync::Arc};
use uniffi::async_runtime::{BoxFuture, BoxStream};
use uniffi::deps::{futures_core::Stream, futures_util::stream};
use uniffi_meta::{read_metadata, Metadata, Type};

#[uniffi::export]
pub fn shape_plain_value() -> u32 {
    1
}

#[uniffi::export]
pub fn shape_result_value() -> Result<u32, Infallible> {
    Ok(1)
}

#[uniffi::export]
pub fn shape_future_value() -> BoxFuture<'static, Result<u32, Infallible>> {
    Box::pin(async { Ok(1) })
}

#[uniffi::export]
pub fn shape_plain_items() -> impl Stream<Item = u32> {
    stream::iter([1])
}

#[uniffi::export]
pub fn shape_result_items() -> impl Stream<Item = Result<u32, Infallible>> {
    stream::iter([Ok(1)])
}

#[uniffi::export]
pub fn shape_canonical_items() -> uniffi::UniFfiStream<u32, Infallible> {
    Box::pin(stream::iter([Ok(1)]))
}

#[uniffi::export]
pub async fn shape_result_factory() -> Result<uniffi::UniFfiStream<u32, Infallible>, Infallible> {
    Ok(shape_canonical_items())
}

#[derive(uniffi::Object)]
pub struct ShapeObject;

#[uniffi::export]
impl ShapeObject {
    #[uniffi::constructor]
    pub fn new() -> Result<Arc<Self>, Infallible> {
        Ok(Arc::new(Self))
    }

    pub fn plain_items(&self) -> BoxStream<'static, u32> {
        Box::pin(stream::iter([1]))
    }

    pub fn result_items(&self) -> uniffi::UniFfiStream<u32, Infallible> {
        shape_canonical_items()
    }

    pub async fn result_factory(
        &self,
    ) -> Result<uniffi::UniFfiStream<u32, Infallible>, Infallible> {
        Ok(shape_canonical_items())
    }
}

#[uniffi::export(with_foreign)]
pub trait ShapeTrait:
    uniffi::async_runtime::SendOutsideWasm + uniffi::async_runtime::SyncOutsideWasm
{
    fn value(&self) -> Result<u32, Infallible>;
    fn plain_items(&self) -> BoxStream<'static, u32>;
    fn result_items(&self) -> uniffi::UniFfiStream<u32, Infallible>;
    fn factory(
        &self,
    ) -> BoxFuture<'static, Result<BoxStream<'static, Result<u32, Infallible>>, Infallible>>;
}

fn assert_shape(bytes: &[u8], expected: (bool, bool)) {
    let metadata = read_metadata(bytes).unwrap();
    let (shape, return_type, throws) = match &metadata {
        Metadata::Func(f) => (
            (f.rust_return_is_result, f.rust_stream_item_is_result),
            f.return_type.as_ref(),
            &f.throws,
        ),
        Metadata::Method(m) => (
            (m.rust_return_is_result, m.rust_stream_item_is_result),
            m.return_type.as_ref(),
            &m.throws,
        ),
        Metadata::TraitMethod(m) => (
            (m.rust_return_is_result, m.rust_stream_item_is_result),
            m.return_type.as_ref(),
            &m.throws,
        ),
        Metadata::Constructor(c) => (
            (c.rust_return_is_result, c.rust_stream_item_is_result),
            c.self_type.as_ref(),
            &c.throws,
        ),
        _ => panic!("expected callable metadata"),
    };
    assert_eq!(shape, expected, "{metadata:?}");
    assert_eq!(
        throws, &None,
        "Rust Result must not invent a host business error"
    );
    if let Some(Type::Stream { error_type, .. }) = return_type {
        assert_eq!(
            error_type, &None,
            "Result Item<Infallible> has no business error"
        );
    }
}

#[test]
fn compiled_function_metadata_preserves_infallible_result_shapes() {
    for (bytes, expected) in [
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_FUNC_SHAPE_PLAIN_VALUE[..],
            (false, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_FUNC_SHAPE_RESULT_VALUE[..],
            (true, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_FUNC_SHAPE_FUTURE_VALUE[..],
            (true, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_FUNC_SHAPE_PLAIN_ITEMS[..],
            (false, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_FUNC_SHAPE_RESULT_ITEMS[..],
            (false, true),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_FUNC_SHAPE_CANONICAL_ITEMS[..],
            (false, true),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_FUNC_SHAPE_RESULT_FACTORY[..],
            (true, true),
        ),
    ] {
        assert_shape(bytes, expected);
    }
}

#[test]
fn compiled_method_constructor_and_trait_metadata_preserve_result_shapes() {
    for (bytes, expected) in [
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_CONSTRUCTOR_SHAPEOBJECT_NEW[..],
            (true, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_METHOD_SHAPEOBJECT_PLAIN_ITEMS[..],
            (false, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_METHOD_SHAPEOBJECT_RESULT_ITEMS[..],
            (false, true),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_METHOD_SHAPEOBJECT_RESULT_FACTORY[..],
            (true, true),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_METHOD_SHAPETRAIT_VALUE[..],
            (true, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_METHOD_SHAPETRAIT_PLAIN_ITEMS[..],
            (false, false),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_METHOD_SHAPETRAIT_RESULT_ITEMS[..],
            (false, true),
        ),
        (
            &UNIFFI_META_UNIFFI_FIXTURE_METADATA_METHOD_SHAPETRAIT_FACTORY[..],
            (true, true),
        ),
    ] {
        assert_shape(bytes, expected);
    }
}
