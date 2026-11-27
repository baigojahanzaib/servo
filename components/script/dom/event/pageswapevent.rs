/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::codegen::Bindings::PageSwapEventBinding;
use crate::dom::bindings::codegen::Bindings::PageSwapEventBinding::PageSwapEventMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::Event;
use crate::dom::pagetransitionevent::PageTransitionEvent;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/#the-pageswapevent-interface>
/// NOTE: the activation and viewTransition attributes are not yet
/// implemented (NavigationActivation and ViewTransition do not exist in
/// the engine); the interface shape is forward-compatible.
#[dom_struct]
pub(crate) struct PageSwapEvent {
    pagetransitionevent: PageTransitionEvent,
}

impl PageSwapEvent {
    fn new_inherited() -> PageSwapEvent {
        PageSwapEvent {
            pagetransitionevent: PageTransitionEvent::new_inherited(),
        }
    }

    fn new_uninitialized(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<PageSwapEvent> {
        reflect_dom_object_with_proto(
            cx,
            Box::new(PageSwapEvent::new_inherited()),
            window,
            proto,
        )
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        type_: Atom,
        bubbles: bool,
        cancelable: bool,
        persisted: bool,
    ) -> DomRoot<PageSwapEvent> {
        Self::new_with_proto(cx, window, None, type_, bubbles, cancelable, persisted)
    }

    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: Atom,
        bubbles: bool,
        cancelable: bool,
        persisted: bool,
    ) -> DomRoot<PageSwapEvent> {
        let ev = PageSwapEvent::new_uninitialized(cx, window, proto);
        ev.pagetransitionevent.set_persisted(persisted);
        {
            let event = ev.upcast::<Event>();
            event.init_event(type_, bubbles, cancelable);
        }
        ev
    }
}

impl PageSwapEventMethods<crate::DomTypeHolder> for PageSwapEvent {
    /// <https://html.spec.whatwg.org/multipage/#the-pageswapevent-interface>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &PageSwapEventBinding::PageSwapEventInit,
    ) -> Fallible<DomRoot<PageSwapEvent>> {
        Ok(PageSwapEvent::new_with_proto(
            cx,
            window,
            proto,
            Atom::from(type_),
            init.parent.parent.bubbles,
            init.parent.parent.cancelable,
            init.parent.persisted,
        ))
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.upcast::<Event>().IsTrusted()
    }
}
