/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use embedder_traits::MouseButton;
use euclid::Point2D;
use js::context::JSContext;
use js::rust::HandleObject;
use keyboard_types::Modifiers;
use script_bindings::cell::DomRefCell;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_traits::MouseButtons;
use style::Atom;
use style_traits::CSSPixel;

use crate::dom::bindings::codegen::Bindings::MouseEventBinding::MouseEventMethods;
use crate::dom::bindings::codegen::Bindings::DragEventBinding::{
    DragEventInit, DragEventMethods,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::datatransfer::DataTransfer;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::mouseevent::MouseEvent;
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/dnd.html#the-dragevent-interface>
#[dom_struct]
pub(crate) struct DragEvent {
    mouseevent: MouseEvent,
    data_transfer: DomRefCell<Option<Dom<DataTransfer>>>,
}

impl DragEvent {
    fn new_inherited() -> DragEvent {
        DragEvent {
            mouseevent: MouseEvent::new_inherited(),
            data_transfer: DomRefCell::new(None),
        }
    }

    fn new_uninitialized_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<DragEvent> {
        reflect_dom_object_with_proto(cx, Box::new(DragEvent::new_inherited()), window, proto)
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        event_type: Atom,
        can_bubble: EventBubbles,
        cancelable: EventCancelable,
        view: Option<&Window>,
        detail: i32,
        screen_point: Point2D<i32, CSSPixel>,
        client_point: Point2D<i32, CSSPixel>,
        page_point: Point2D<i32, CSSPixel>,
        modifiers: Modifiers,
        button: MouseButton,
        buttons: MouseButtons,
        related_target: Option<&EventTarget>,
        point_in_target: Option<Point2D<f32, CSSPixel>>,
        data_transfer: Option<&DataTransfer>,
    ) -> DomRoot<DragEvent> {
        let ev = Self::new_uninitialized_with_proto(cx, window, None);
        ev.mouseevent.initialize_mouse_event(
            event_type,
            can_bubble,
            cancelable,
            view,
            detail,
            screen_point,
            client_point,
            page_point,
            modifiers,
            button,
            buttons,
            related_target,
            point_in_target,
        );
        *ev.data_transfer.borrow_mut() = data_transfer.map(Dom::from_ref);
        ev
    }

    pub(crate) fn data_transfer(&self) -> Option<DomRoot<DataTransfer>> {
        self.data_transfer.borrow().as_deref().map(DomRoot::from_ref)
    }
}

impl DragEventMethods<crate::DomTypeHolder> for DragEvent {
    /// <https://html.spec.whatwg.org/multipage/dnd.html#dom-dragevent-dragevent>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        event_type: DOMString,
        init: &DragEventInit,
    ) -> DomRoot<DragEvent> {
        let bubbles = EventBubbles::from(init.parent.parent.parent.parent.bubbles);
        let cancelable = EventCancelable::from(init.parent.parent.parent.parent.cancelable);
        let scroll_offset = window.scroll_offset(cx);
        let page_point = Point2D::new(
            scroll_offset.x as i32 + init.parent.clientX,
            scroll_offset.y as i32 + init.parent.clientY,
        );
        let ev = DragEvent::new_uninitialized_with_proto(cx, window, proto);
        ev.mouseevent.initialize_mouse_event(
            event_type.into(),
            bubbles,
            cancelable,
            init.parent.parent.parent.view.as_deref(),
            init.parent.parent.parent.detail,
            Point2D::new(init.parent.screenX, init.parent.screenY),
            Point2D::new(init.parent.clientX, init.parent.clientY),
            page_point,
            init.parent.parent.modifiers(),
            init.parent.button.into(),
            MouseButtons::from_bits_retain(init.parent.buttons),
            init.parent.relatedTarget.as_deref(),
            None,
        );
        ev.upcast::<Event>()
            .set_composed(init.parent.parent.parent.parent.composed);
        *ev.data_transfer.borrow_mut() = init.dataTransfer.as_deref().map(Dom::from_ref);
        ev
    }

    /// <https://html.spec.whatwg.org/multipage/dnd.html#dom-dragevent-datatransfer>
    fn GetDataTransfer(&self) -> Option<DomRoot<DataTransfer>> {
        self.data_transfer()
    }

    fn IsTrusted(&self) -> bool {
        self.mouseevent.IsTrusted()
    }
}
