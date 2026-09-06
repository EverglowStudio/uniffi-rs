/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! FFI handles contain an owner proxy, never the thread-affine object itself.

use std::sync::Arc;

use bytes::{Buf, BufMut};

use crate::{
    async_runtime::LocalObject, check_remaining, FfiConverterArc, Handle, LiftRef, MetadataBuffer,
    Result,
};

/// Metadata supplied by `#[derive(Object)] #[uniffi(local)]`.
///
/// # Safety
/// The metadata must identify this local object's generated clone/free functions.
/// Those functions must use handles of `Arc<LocalObject<Self>>` exclusively.
#[doc(hidden)]
pub unsafe trait LocalObjectType<UT>: 'static {
    const TYPE_ID_META: MetadataBuffer;
}

// SAFETY: Each handle transfers one Arc of the proxy. T stays in its owner and is
// never dereferenced by the converter or the foreign handle registry.
unsafe impl<UT, T: LocalObjectType<UT>> FfiConverterArc<UT> for LocalObject<T> {
    type FfiType = Handle;

    fn lower(value: Arc<Self>) -> Handle {
        Handle::from_arc(value)
    }

    fn try_lift(handle: Handle) -> Result<Arc<Self>> {
        // SAFETY: Generated bindings transfer a handle for this exact proxy type.
        Ok(unsafe { handle.into_arc() })
    }

    fn write(value: Arc<Self>, buf: &mut Vec<u8>) {
        buf.put_u64(Self::lower(value).as_raw());
    }

    fn try_read(buf: &mut &[u8]) -> Result<Arc<Self>> {
        check_remaining(buf, 8)?;
        Self::try_lift(Handle::from_raw_unchecked(buf.get_u64()))
    }

    const TYPE_ID_META: MetadataBuffer = T::TYPE_ID_META;
}

// SAFETY: Lifting yields an owned Arc of the proxy, never a reference to T.
unsafe impl<UT, T: LocalObjectType<UT>> LiftRef<UT> for LocalObject<T> {
    type LiftType = Arc<Self>;
}
