/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! This module contains implementations of structured data as described in
//! <https://html.spec.whatwg.org/multipage/#safe-passing-of-structured-data>

mod serializable;
mod transferable;

use log::warn;
use malloc_size_of_derive::MallocSizeOf;
use rustc_hash::{FxBuildHasher, FxHashMap};
use serde::{Deserialize, Serialize};
pub use serializable::*;
use servo_base::id::{
    BlobId, CryptoKeyId, DomExceptionId, DomMatrixId, DomPointId, DomQuadId, DomRectId, FileId,
    FileListId, ImageBitmapId, ImageDataId, MessagePortId, OffscreenCanvasId, QuotaExceededErrorId,
};
use strum::IntoEnumIterator;
pub use transferable::*;

/// A data-holder for serialized data and transferred objects.
/// <https://html.spec.whatwg.org/multipage/#structuredserializewithtransfer>
#[derive(Debug, Default, Deserialize, MallocSizeOf, Serialize)]
pub struct StructuredSerializedData {
    /// Data serialized by SpiderMonkey.
    pub serialized: Vec<u8>,
    /// Key into the script crate's process-global registry of
    /// SpiderMonkey-owned clone buffers. Present only when the stream was
    /// serialized with `SameProcess` scope: such streams embed references to
    /// engine-owned memory and must never be transposed into bytes, so the
    /// buffer itself is parked in-process and only this key travels.
    /// Invariant: exactly one reader may consume the key.
    #[serde(default)]
    #[ignore_malloc_size_of = "process-global registry ownership"]
    pub sm_buffer_key: Option<u64>,
    /// Serialized in a structured callback,
    pub blobs: Option<FxHashMap<BlobId, BlobImpl>>,
    /// Serialized files.
    pub files: Option<FxHashMap<FileId, SerializableFile>>,
    /// Serialized file lists.
    pub file_lists: Option<FxHashMap<FileListId, SerializableFileList>>,
    /// Serialized point objects.
    pub points: Option<FxHashMap<DomPointId, DomPoint>>,
    /// Serialized rect objects.
    pub rects: Option<FxHashMap<DomRectId, DomRect>>,
    /// Serialized quad objects.
    pub quads: Option<FxHashMap<DomQuadId, DomQuad>>,
    /// Serialized matrix objects.
    pub matrices: Option<FxHashMap<DomMatrixId, DomMatrix>>,
    /// Serialized exception objects.
    pub exceptions: Option<FxHashMap<DomExceptionId, DomException>>,
    /// Serialized quota exceeded errors.
    pub quota_exceeded_errors:
        Option<FxHashMap<QuotaExceededErrorId, SerializableQuotaExceededError>>,
    /// Transferred objects.
    pub ports: Option<FxHashMap<MessagePortId, MessagePortImpl>>,
    /// Transform streams transferred objects.
    pub transform_streams: Option<FxHashMap<MessagePortId, TransformStreamData>>,
    /// Serialized image bitmap objects.
    pub image_bitmaps: Option<FxHashMap<ImageBitmapId, SerializableImageBitmap>>,
    /// Transferred image bitmap objects.
    pub transferred_image_bitmaps: Option<FxHashMap<ImageBitmapId, SerializableImageBitmap>>,
    /// Transferred offscreen canvas objects.
    pub offscreen_canvases: Option<FxHashMap<OffscreenCanvasId, TransferableOffscreenCanvas>>,
    /// Serialized image data objects.
    pub image_data: Option<FxHashMap<ImageDataId, SerializableImageData>>,
    /// Serialized crypto key objects.
    pub crypto_keys: Option<FxHashMap<CryptoKeyId, SerializableCryptoKey>>,
}

impl StructuredSerializedData {
    fn is_empty(&self, val: Transferrable) -> bool {
        fn is_field_empty<K, V>(field: &Option<FxHashMap<K, V>>) -> bool {
            field.as_ref().is_none_or(|h| h.is_empty())
        }
        match val {
            Transferrable::ImageBitmap => is_field_empty(&self.transferred_image_bitmaps),
            Transferrable::MessagePort => is_field_empty(&self.ports),
            Transferrable::OffscreenCanvas => is_field_empty(&self.offscreen_canvases),
            Transferrable::ReadableStream => is_field_empty(&self.ports),
            Transferrable::WritableStream => is_field_empty(&self.ports),
            Transferrable::TransformStream => is_field_empty(&self.ports),
        }
    }

    /// Clone all values of the same type stored in this StructuredSerializedData
    /// into another instance.
    fn clone_all_of_type<T: BroadcastClone>(&self, cloned: &mut StructuredSerializedData) {
        let existing = T::source(self);
        let Some(existing) = existing else { return };
        let mut clones = FxHashMap::with_capacity_and_hasher(existing.len(), FxBuildHasher);

        for (original_id, obj) in existing.iter() {
            if let Some(clone) = obj.clone_for_broadcast() {
                clones.insert(*original_id, clone);
            }
        }

        *T::destination(cloned) = Some(clones);
    }

    /// Clone the serialized data for use with broadcast-channels.
    pub fn clone_for_broadcast(&self) -> StructuredSerializedData {
        for transferrable in Transferrable::iter() {
            if !self.is_empty(transferrable) {
                // Not panicking only because this is called from the constellation.
                warn!(
                    "Attempt to broadcast structured serialized data including {:?} (should never happen).",
                    transferrable,
                );
            }
        }

        if self.sm_buffer_key.is_some() {
            // Not panicking only because this is called from the constellation.
            // Broadcast writes use `DifferentProcess` scope, so a carried
            // single-owner buffer should never reach this fanout path.
            warn!("Attempt to broadcast a SameProcess-scope clone buffer (should never happen).");
        }

        let serialized = self.serialized.clone();

        let mut cloned = StructuredSerializedData {
            serialized,
            ..Default::default()
        };

        for serializable in Serializable::iter() {
            let clone_impl = serializable.clone_values();
            clone_impl(self, &mut cloned);
        }

        cloned
    }
}
