/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto;

use crate::dom::bindings::codegen::Bindings::SourceBufferListBinding::SourceBufferListMethods;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;
use crate::dom::media::sourcebuffer::SourceBuffer;

#[dom_struct]
pub(crate) struct SourceBufferList {
    eventtarget: EventTarget,
    buffers: DomRefCell<Vec<Dom<SourceBuffer>>>,
}

impl SourceBufferList {
    fn new_inherited() -> SourceBufferList {
        SourceBufferList {
            eventtarget: EventTarget::new_inherited(),
            buffers: DomRefCell::new(vec![]),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, global: &GlobalScope) -> DomRoot<SourceBufferList> {
        reflect_dom_object_with_proto(
            cx,
            Box::new(SourceBufferList::new_inherited()),
            global,
            None,
        )
    }

    pub(crate) fn add(&self, buffer: &SourceBuffer) {
        self.buffers.borrow_mut().push(Dom::from_ref(buffer));
    }

    pub(crate) fn remove(&self, buffer: &SourceBuffer) {
        self.buffers
            .borrow_mut()
            .retain(|b| std::ptr::eq(&**b, buffer));
    }
}

impl SourceBufferListMethods<crate::DomTypeHolder> for SourceBufferList {
    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-length>
    fn Length(&self) -> u32 {
        self.buffers.borrow().len() as u32
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebufferlist-index>
    fn IndexedGetter(&self, index: u32) -> Option<DomRoot<SourceBuffer>> {
        self.buffers
            .borrow()
            .get(index as usize)
            .map(|b| DomRoot::from_ref(&**b))
    }

    event_handler!(addsourcebuffer, GetOnaddsourcebuffer, SetOnaddsourcebuffer);
    event_handler!(removesourcebuffer, GetOnremovesourcebuffer, SetOnremovesourcebuffer);
}
