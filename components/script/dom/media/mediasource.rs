/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use servo_media::{ServoMedia, SupportsMediaType};
use stylo_atoms::Atom;
use uuid::Uuid;

use crate::dom::bindings::codegen::Bindings::MediaSourceBinding::{
    EndOfStreamError, MediaSourceMethods, MediaSourceReadyState,
};
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::media::sourcebuffer::SourceBuffer;
use crate::dom::media::sourcebufferlist::SourceBufferList;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct MediaSource {
    eventtarget: EventTarget,
    ready_state: Cell<MediaSourceReadyState>,
    source_buffers: Dom<SourceBufferList>,
    active_source_buffers: Dom<SourceBufferList>,
    duration: Cell<f64>,
    #[no_trace]
    id: Uuid,
    media_element: script_bindings::cell::DomRefCell<Option<Dom<crate::dom::html::embedded_content::htmlmediaelement::HTMLMediaElement>>>,
}

impl MediaSource {
    fn new_inherited(
        source_buffers: &SourceBufferList,
        active_source_buffers: &SourceBufferList,
    ) -> MediaSource {
        MediaSource {
            eventtarget: EventTarget::new_inherited(),
            ready_state: Cell::new(MediaSourceReadyState::Closed),
            source_buffers: Dom::from_ref(source_buffers),
            active_source_buffers: Dom::from_ref(active_source_buffers),
            duration: Cell::new(f64::NAN),
            id: Uuid::new_v4(),
            media_element: script_bindings::cell::DomRefCell::new(None),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<MediaSource> {
        let global = window.upcast::<crate::dom::globalscope::GlobalScope>();
        let source_buffers = SourceBufferList::new(cx, global);
        let active_source_buffers = SourceBufferList::new(cx, global);
        reflect_dom_object_with_proto(
            cx,
            Box::new(MediaSource::new_inherited(&source_buffers, &active_source_buffers)),
            global,
            proto,
        )
    }
}

impl MediaSource {
    pub(crate) fn url_id(&self) -> Uuid {
        self.id
    }

    pub(crate) fn set_media_element(&self, element: &crate::dom::html::embedded_content::htmlmediaelement::HTMLMediaElement) {
        *self.media_element.borrow_mut() = Some(Dom::from_ref(element));
    }

    pub(crate) fn clear_media_element(&self) {
        self.media_element.borrow_mut().take();
    }

    /// Route appended bytes into the attached element's push player.
    /// Returns false when nothing is attached (bytes are dropped).
    pub(crate) fn push_appended_data(&self, bytes: Vec<u8>) -> bool {
        if let Some(ref element) = *self.media_element.borrow() {
            return element.push_media_source_data(bytes);
        }
        false
    }

    pub(crate) fn signal_end_of_stream(&self) {
        if let Some(ref element) = *self.media_element.borrow() {
            element.push_media_source_eos();
        }
    }

    /// Merged buffered ranges across all source buffers (union).
    pub(crate) fn buffered_value(&self) -> crate::dom::timeranges::TimeRangesContainer {
        let mut merged = crate::dom::timeranges::TimeRangesContainer::default();
        self.source_buffers.for_each_buffer(|sb| {
            for &(start, end) in &sb.buffered_ranges() {
                let _ = merged.add(start, end);
            }
        });
        merged
    }

    pub(crate) fn duration_value(&self) -> f64 {
        self.duration.get()
    }

    pub(crate) fn is_open(&self) -> bool {
        self.ready_state.get() != MediaSourceReadyState::Closed
    }

    /// Attachment steps, per <https://w3c.github.io/media-source/#mediasource-attach>:
    /// the readyState transitions to open and a sourceopen event is queued.
    pub(crate) fn attach_to_media_element(&self) {
        if self.ready_state.get() == MediaSourceReadyState::Open {
            return;
        }
        self.ready_state.set(MediaSourceReadyState::Open);
        self.queue_simple_event(Atom::from("sourceopen"));
    }

    /// Detachment steps: readyState transitions to closed, sourceclose is queued.
    pub(crate) fn detach_from_media_element(&self) {
        if self.ready_state.get() == MediaSourceReadyState::Closed {
            return;
        }
        self.ready_state.set(MediaSourceReadyState::Closed);
        self.queue_simple_event(Atom::from("sourceclose"));
    }

    fn queue_simple_event(&self, name: Atom) {
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_simple_event: move |cx| {
                let this = this.root();
                let event = Event::new(
                    cx,
                    &this.global(),
                    name.clone(),
                    EventBubbles::DoesNotBubble,
                    EventCancelable::NotCancelable,
                );
                event.upcast::<Event>().fire(cx, this.upcast());
            }));
    }
}

impl MediaSourceMethods<crate::DomTypeHolder> for MediaSource {
    /// <https://w3c.github.io/media-source/#dom-mediasource-constructor>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> Fallible<DomRoot<MediaSource>> {
        Ok(MediaSource::new(cx, window, proto))
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-istypesupported>
    fn IsTypeSupported(_global: &Window, type_: DOMString) -> bool {
        // The honest answer is whatever the machine can decode: consult the
        // real backend registry (gstreamer caps) rather than a static table.
        !type_.str().is_empty() &&
            !matches!(
                ServoMedia::get().can_play_type(&type_.str()),
                SupportsMediaType::No
            )
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-sourcebuffers>
    fn SourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-activesourcebuffers>
    fn ActiveSourceBuffers(&self) -> DomRoot<SourceBufferList> {
        DomRoot::from_ref(&*self.active_source_buffers)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-readystate>
    fn ReadyState(&self) -> MediaSourceReadyState {
        self.ready_state.get()
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn GetDuration(&self) -> Fallible<f64> {
        Ok(self.duration.get())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-duration>
    fn SetDuration(&self, value: f64) -> ErrorResult {
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        self.duration.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-addsourcebuffer>
    fn AddSourceBuffer(&self, cx: &mut JSContext, type_: DOMString) -> Fallible<DomRoot<SourceBuffer>> {
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        if !Self::IsTypeSupported(self.global().as_window(), type_.clone()) {
            return Err(Error::NotSupported(None));
        }
        let buffer = SourceBuffer::new(cx, &self.global(), type_);
        buffer.set_media_source(self);
        self.source_buffers.add(&buffer);
        Ok(buffer)
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-removesourcebuffer>
    fn RemoveSourceBuffer(&self, source_buffer: &SourceBuffer) -> Fallible<()> {
        self.source_buffers.remove(source_buffer);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-endofstream>
    fn EndOfStream(&self, _error: Option<EndOfStreamError>) -> Fallible<()> {
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        self.ready_state.set(MediaSourceReadyState::Ended);
        self.signal_end_of_stream();
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-setliveseekablerange>
    fn SetLiveSeekableRange(&self, _start: Finite<f64>, _end: Finite<f64>) -> Fallible<()> {
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-mediasource-clearliveseekablerange>
    fn ClearLiveSeekableRange(&self) -> Fallible<()> {
        if self.ready_state.get() != MediaSourceReadyState::Open {
            return Err(Error::InvalidState(None));
        }
        Ok(())
    }

    event_handler!(sourceopen, GetOnsourceopen, SetOnsourceopen);
    event_handler!(sourceended, GetOnsourceended, SetOnsourceended);
    event_handler!(sourceclose, GetOnsourceclose, SetOnsourceclose);
}
