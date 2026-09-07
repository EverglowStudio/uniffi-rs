/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{
    rust_stream_new, Handle, LowerReturn, MetadataBuffer, RustCallError, RustStreamRegistry,
    TypeId, UniFfiStream, UniffiCompatibleStreamValue,
};

/// A stream awaiting delivery through a generated return converter.
///
/// Keeping the stream here, rather than registering a handle when its factory
/// finishes, lets cancellation drop an undelivered subscription in Rust.
#[doc(hidden)]
pub struct RustStreamOutput<T: 'static, E: 'static> {
    registry: &'static RustStreamRegistry<T, E>,
    stream: UniFfiStream<T, E>,
}

impl<T: 'static, E: 'static> RustStreamOutput<T, E> {
    pub fn new(registry: &'static RustStreamRegistry<T, E>, stream: UniFfiStream<T, E>) -> Self {
        Self { registry, stream }
    }
}

// SAFETY: the typed registry owns the stream after lowering, and returns the
// same opaque handle consumed by the generated typed next/close entry points.
unsafe impl<UT, T, E> LowerReturn<UT> for RustStreamOutput<T, E>
where
    T: UniffiCompatibleStreamValue,
    E: UniffiCompatibleStreamValue,
{
    type ReturnType = Handle;

    fn lower_return(value: Self) -> Result<Handle, RustCallError> {
        Ok(rust_stream_new(value.registry, value.stream))
    }
}

impl<UT, T: 'static, E: 'static> TypeId<UT> for RustStreamOutput<T, E>
where
    UniFfiStream<T, E>: TypeId<UT>,
{
    const TYPE_ID_META: MetadataBuffer = <UniFfiStream<T, E> as TypeId<UT>>::TYPE_ID_META;
}
