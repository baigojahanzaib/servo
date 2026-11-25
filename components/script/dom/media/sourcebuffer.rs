/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;

use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    mime_type: DOMString,
    updating: Cell<bool>,
    mode: Cell<AppendMode>,
    timestamp_offset: Cell<f64>,
    buffered: Dom<TimeRanges>,
    media_source: script_bindings::cell::DomRefCell<Option<Dom<crate::dom::media::mediasource::MediaSource>>>,
}

impl SourceBuffer {
    fn new_inherited(mime_type: DOMString, buffered: &TimeRanges) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            mime_type,
            updating: Cell::new(false),
            mode: Cell::new(AppendMode::Segments),
            timestamp_offset: Cell::new(0.0),
            buffered: Dom::from_ref(buffered),
            media_source: script_bindings::cell::DomRefCell::new(None),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        mime_type: DOMString,
    ) -> DomRoot<SourceBuffer> {
        let buffered = TimeRanges::new(cx, global.as_window(), TimeRangesContainer::default());
        reflect_dom_object_with_proto(
            cx,
            Box::new(SourceBuffer::new_inherited(mime_type, &buffered)),
            global,
            None,
        )
    }

    pub(crate) fn set_media_source(&self, ms: &crate::dom::media::mediasource::MediaSource) {
        *self.media_source.borrow_mut() = Some(Dom::from_ref(ms));
    }

    pub(crate) fn mime_type(&self) -> String {
        self.mime_type.str().to_string()
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn GetMode(&self) -> Fallible<AppendMode> {
        Ok(self.mode.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, value: AppendMode) -> ErrorResult {
        self.mode.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn Buffered(&self, cx: &mut JSContext) -> DomRoot<TimeRanges> {
        DomRoot::from_ref(&*self.buffered)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn GetTimestampOffset(&self) -> Fallible<Finite<f64>> {
        Ok(Finite::wrap(self.timestamp_offset.get()))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, value: Finite<f64>) -> ErrorResult {
        self.timestamp_offset.set(*value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(0.0)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn GetAppendWindowEnd(&self) -> Fallible<Finite<f64>> {
        Ok(Finite::wrap(f64::INFINITY))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, _value: Finite<f64>) -> ErrorResult {
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, cx: &mut JSContext, data: ArrayBufferViewOrArrayBuffer) -> Fallible<()> {
        let _ = cx;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        let bytes = crate::dom::bindings::buffer_source::get_buffer_source_copy((&data).into());
        if let Some(ref ms) = *self.media_source.borrow() {
            ms.push_appended_data(bytes);
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> Fallible<()> {
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, _start: Finite<f64>, _end: Finite<f64>) -> Fallible<()> {
        Ok(())
    }

    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);
    event_handler!(update, GetOnupdate, SetOnupdate);
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);
    event_handler!(error, GetOnerror, SetOnerror);
    event_handler!(abort, GetOnabort, SetOnabort);
}
