/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use dom_struct::dom_struct;
use js::context::{JSContext, NoGC};
use rustc_hash::FxHashMap;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use script_bindings::serializable::Serializable;
use script_bindings::structuredclone::StructuredData;
use servo_base::id::{MediaSourceHandleId, MediaSourceHandleIndex};
use servo_constellation_traits::SerializableMediaSourceHandle;

use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::core::{MediaSourceCore, lookup_core};

/// <https://w3c.github.io/media-source/#dom-mediasourcehandle>
///
/// A serializable reference to a MediaSource's shared core. The object itself
/// is a thin shell: all state lives in the core, which is shared in-process
/// between the worker that owns the MediaSource and the media element the
/// handle gets attached to.
#[dom_struct]
pub(crate) struct MediaSourceHandle {
    reflector_: Reflector,
    #[no_trace]
    #[ignore_malloc_size_of = "cross-thread shared core, Arc-owned"]
    core: Arc<MediaSourceCore>,
}

impl MediaSourceHandle {
    fn new_inherited(core: Arc<MediaSourceCore>) -> MediaSourceHandle {
        MediaSourceHandle {
            reflector_: Reflector::new(),
            core,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        core: Arc<MediaSourceCore>,
    ) -> DomRoot<MediaSourceHandle> {
        reflect_dom_object(
            cx,
            Box::new(MediaSourceHandle::new_inherited(core)),
            global,
        )
    }

    pub(crate) fn core(&self) -> Arc<MediaSourceCore> {
        self.core.clone()
    }
}

impl Serializable<crate::DomTypeHolder> for MediaSourceHandle {
    type Index = MediaSourceHandleIndex;
    type Data = SerializableMediaSourceHandle;

    fn serialize(&self, _no_gc: &NoGC) -> Result<(MediaSourceHandleId, Self::Data), ()> {
        Ok((
            MediaSourceHandleId::new(),
            SerializableMediaSourceHandle { id: self.core.id },
        ))
    }

    fn deserialize(
        cx: &mut JSContext,
        owner: &GlobalScope,
        serialized: Self::Data,
    ) -> Result<DomRoot<Self>, ()> {
        // The serialized form is just the core id: resolve it against the
        // process-global registry. Failure means the owning MediaSource (and
        // every handle) was garbage-collected before deserialization.
        let core = lookup_core(&serialized.id).ok_or(())?;
        Ok(MediaSourceHandle::new(cx, owner, core))
    }

    fn serialized_storage<'a>(
        data: StructuredData<'a, '_>,
    ) -> &'a mut Option<FxHashMap<MediaSourceHandleId, Self::Data>> {
        match data {
            StructuredData::Reader(reader) => &mut reader.media_source_handles,
            StructuredData::Writer(writer) => &mut writer.media_source_handles,
        }
    }
}
