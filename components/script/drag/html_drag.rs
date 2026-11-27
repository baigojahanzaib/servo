/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Implementation of the drag-and-drop processing model from
//! <https://html.spec.whatwg.org/multipage/dnd.html#dndevents>,
//! cross-referenced against Ladybird's `DragAndDropEventHandler`.

use std::cell::RefCell;

use script_bindings::cell::DomRefCell;
use std::rc::Rc;

use embedder_traits::MouseButton;
use euclid::Point2D;
use js::context::JSContext;
use script_traits::{ConstellationInputEvent, MouseButtons};
use style_traits::CSSPixel;

use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::datatransfer::DataTransfer;
use crate::dom::dragevent::DragEvent;
use crate::dom::element::Element;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::html::htmlanchorelement::HTMLAnchorElement;
use crate::dom::html::htmlimageelement::HTMLImageElement;
use crate::dom::inputevent::HitTestResult;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::Node;
use crate::dom::window::Window;
use crate::drag::drag_data_store::{DragDataStore, Mode};

/// The distance in CSS pixels the pointer must travel with the primary button
/// held before a drag session is initiated. This mirrors the platform
/// drag-threshold behavior of other engines.
const DRAG_THRESHOLD_PX: f32 = 4.0;

/// <https://html.spec.whatwg.org/multipage/dnd.html#dndevents>
///
/// The state machine for an HTML5 drag-and-drop session: created in a pending
/// state when the primary mouse button is pressed over a draggable element,
/// promoted to an active session once the pointer moves past the drag
/// threshold, and completed by the primary button's release.
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct HtmlDragHandler {
    /// The source node: the draggable element the drag started on.
    source_node: Dom<Node>,
    /// The drag data store backing the session's `DataTransfer` object.
    /// Emptied when the session ends, which makes the object inert per
    /// specification.
    #[conditional_malloc_size_of]
    #[no_trace]
    data_store: Rc<RefCell<Option<DragDataStore>>>,
    /// The single `DataTransfer` object shared by every event of this
    /// session, as required by the specification.
    data_transfer: DomRefCell<Option<Dom<DataTransfer>>>,
    /// The element currently being dragged over (`immediate user selection`,
    /// resolved to an element).
    current_target: Option<Dom<Element>>,
    /// The node last hit by the pointer, before resolution to an element.
    current_node: Option<Dom<Node>>,
    /// Whether the session has begun (dragstart was fired).
    started: bool,
    /// Whether the session can no longer produce events (dragstart canceled or
    /// session finished).
    finished: bool,
    /// Where the initial mousedown happened, in client coordinates.
    #[no_trace]
    start_point: Point2D<f32, CSSPixel>,
    /// The current drag operation chosen during `dragover` processing.
    current_operation: DragOperation,
}

#[derive(Clone, Copy, Eq, JSTraceable, MallocSizeOf, PartialEq)]
enum DragOperation {
    None,
    Copy,
    Link,
    Move,
}

impl HtmlDragHandler {
    pub(crate) fn new(source_node: &Node, start_point: Point2D<f32, CSSPixel>) -> Self {
        HtmlDragHandler {
            source_node: Dom::from_ref(source_node),
            data_store: Rc::new(RefCell::new(Some(DragDataStore::new()))),
            data_transfer: DomRefCell::new(None),
            current_target: None,
            current_node: None,
            started: false,
            finished: false,
            start_point,
            current_operation: DragOperation::None,
        }
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.finished
    }

    fn still_connected(&self) -> bool {
        self.source_node.is_connected()
    }

    fn window(&self) -> DomRoot<Window> {
        DomRoot::from_ref(self.source_node.owner_doc().window())
    }

    /// Find the first inclusive ancestor of `node` that is draggable, per
    /// <https://html.spec.whatwg.org/multipage/dnd.html#draggable-elements>:
    /// elements with `draggable=true`, plus images and links with `href`,
    /// which are draggable by default.
    pub(crate) fn first_draggable_inclusive_ancestor(node: &Node) -> Option<DomRoot<Element>> {
        use crate::dom::html::htmlelement::HTMLElement;
        node.inclusive_ancestors(ShadowIncluding::Yes)
            .filter_map(DomRoot::downcast::<Element>)
            .find(|element| {
                let is_html = element.downcast::<HTMLElement>().is_some();
                if !is_html {
                    return false;
                }
                if element
                    .get_string_attribute(&html5ever::local_name!("draggable"))
                    .str()
                    .eq_ignore_ascii_case("true")
                {
                    return true;
                }
                if element
                    .has_attribute(&html5ever::local_name!("draggable"))
                {
                    return false;
                }
                // Implicitly draggable elements in the `auto` state.
                element.downcast::<HTMLImageElement>().is_some() ||
                    (element.downcast::<HTMLAnchorElement>().is_some() &&
                        element.has_attribute(&html5ever::local_name!("href")))
            })
    }

    fn data_transfer_for(&self, cx: &mut JSContext, mode: Mode) -> DomRoot<DataTransfer> {
        if let Some(store) = self.data_store.borrow_mut().as_mut() {
            store.set_mode(mode);
        }
        if self.data_transfer.borrow().is_some() {
            let borrow = self.data_transfer.borrow();
            let transfer = borrow.as_ref().unwrap();
            return DomRoot::from_ref(&**transfer);
        }
        let window = self.window();
        let transfer = DataTransfer::new(cx, &window, Rc::clone(&self.data_store));
        *self.data_transfer.borrow_mut() = Some(Dom::from_ref(&transfer));
        transfer
    }

    /// Fire a drag event of `type` at `target` with positional information
    /// taken from the hit test.
    #[expect(clippy::too_many_arguments)]
    fn fire_drag_event(
        &self,
        cx: &mut JSContext,
        target: &EventTarget,
        type_: &str,
        hit_test: &HitTestResult,
        input_event: &ConstellationInputEvent,
        mode: Mode,
        related_target: Option<&EventTarget>,
    ) -> bool {
        let window = self.window();
        let data_transfer = self.data_transfer_for(cx, mode);
        let client_point = hit_test.point_in_frame;
        let event = DragEvent::new(
            cx,
            &window,
            style::Atom::from(type_),
            EventBubbles::Bubbles,
            EventCancelable::Cancelable,
            Some(&window),
            0,
            Point2D::new(
                hit_test.point_in_frame.x as i32,
                hit_test.point_in_frame.y as i32,
            ),
            Point2D::new(client_point.x as i32, client_point.y as i32),
            Point2D::new(client_point.x as i32, client_point.y as i32),
            input_event.active_keyboard_modifiers,
            MouseButton::Primary,
            input_event.pressed_mouse_buttons,
            related_target,
            Some(hit_test.point_in_frame),
            Some(&data_transfer),
        );
        !event.upcast::<Event>().fire(cx, target)
    }

    /// Begin the session: fire `dragstart` at the source node. Returns false
    /// if the event was canceled, aborting the session.
    fn begin(&mut self, cx: &mut JSContext, hit_test: &HitTestResult, input_event: &ConstellationInputEvent) -> bool {
        self.started = true;
        let window = self.window();
        let source: DomRoot<Node> = DomRoot::from_ref(&*self.source_node);
        let target = source.upcast::<EventTarget>();
        let canceled = self.fire_drag_event(
            cx,
            target,
            "dragstart",
            hit_test,
            input_event,
            Mode::ReadWrite,
            None,
        );
        let _ = window;
        if canceled {
            self.finish();
            return false;
        }
        true
    }

    fn finish(&mut self) {
        self.finished = true;
        // Drop the data store: this makes every `DataTransfer` object from
        // this session inert per specification.
        *self.data_store.borrow_mut() = None;
    }

    /// Handle a mouse move during the gesture. Returns false when the gesture
    /// should be dropped by the caller.
    pub(crate) fn moved(
        &mut self,
        cx: &mut JSContext,
        hit_test: &HitTestResult,
        input_event: &ConstellationInputEvent,
    ) -> bool {
        if self.finished || !self.still_connected() {
            return false;
        }
        if !input_event.primary_button_is_pressed() {
            return false;
        }

        let point: Point2D<f32, CSSPixel> = hit_test.point_in_frame.cast_unit();
        if !self.started {
            let delta = point - self.start_point;
            if delta.x.abs() < DRAG_THRESHOLD_PX && delta.y.abs() < DRAG_THRESHOLD_PX {
                return true;
            }
            if !self.begin(cx, hit_test, input_event) {
                return false;
            }
        }

        // Fire `drag` at the source node.
        let source: DomRoot<Node> = DomRoot::from_ref(&*self.source_node);
        let drag_canceled = self.fire_drag_event(
            cx,
            source.upcast(),
            "drag",
            hit_test,
            input_event,
            Mode::Protected,
            None,
        );
        if drag_canceled {
            self.current_operation = DragOperation::None;
        }

        // Track the immediate user selection and fire the boundary events.
        let node = hit_test.node.clone();
        let node_changed = self
            .current_node
            .as_ref()
            .is_none_or(|old| **old != *node);
        if node_changed {
            let previous_target = self.current_target.as_ref().map(|t| DomRoot::from_ref(&**t));
            self.current_node = Some(Dom::from_ref(&node));

            // Fire dragenter at the new node, then resolve the target element.
            self.fire_drag_event(
                cx,
                node.upcast(),
                "dragenter",
                hit_test,
                input_event,
                Mode::Protected,
                previous_target.as_deref().map(|t| t.upcast()),
            );
            self.current_target = node
                .inclusive_ancestors(ShadowIncluding::Yes)
                .filter_map(DomRoot::downcast::<Element>)
                .next()
                .as_deref()
                .map(Dom::from_ref);

            if let Some(previous) = previous_target {
                let new_target_ref = self
                    .current_target
                    .as_ref()
                    .map(|t| DomRoot::from_ref(&**t));
                if new_target_ref.as_deref() != Some(&*previous) {
                    self.fire_drag_event(
                        cx,
                        previous.upcast(),
                        "dragleave",
                        hit_test,
                        input_event,
                        Mode::Protected,
                        new_target_ref.as_deref().map(|t| t.upcast()),
                    );
                }
            }
        }

        // Fire `dragover` at the current target element and compute the
        // current drag operation from effectAllowed/dropEffect.
        if let Some(target) = self.current_target.as_ref().map(|t| DomRoot::from_ref(&**t)) {
            let over_canceled = self.fire_drag_event(
                cx,
                target.upcast(),
                "dragover",
                hit_test,
                input_event,
                Mode::Protected,
                None,
            );
            self.current_operation = if !over_canceled {
                DragOperation::None
            } else {
                let (effect_allowed, drop_effect) = self
                    .data_transfer
                    .borrow()
                    .as_ref()
                    .map(|transfer| {
                        (
                            transfer.effect_allowed_string().to_string(),
                            transfer.drop_effect_string().to_string(),
                        )
                    })
                    .unwrap_or_default();
                compute_drag_operation(&effect_allowed, &drop_effect)
            };
        }
        true
    }

    /// Handle the primary button release: fire `drop` at the current target
    /// if a drag operation was chosen, then always fire `dragend` at the
    /// source node. The gesture always ends here.
    pub(crate) fn released(
        &mut self,
        cx: &mut JSContext,
        hit_test: &HitTestResult,
        input_event: &ConstellationInputEvent,
    ) {
        if self.finished {
            return;
        }
        if self.started {
            if self.current_operation != DragOperation::None {
                if let Some(target) = self.current_target.as_ref().map(|t| DomRoot::from_ref(&**t)) {
                    self.fire_drag_event(
                        cx,
                        target.upcast(),
                        "drop",
                        hit_test,
                        input_event,
                        Mode::ReadOnly,
                        None,
                    );
                }
            }
            let source: DomRoot<Node> = DomRoot::from_ref(&*self.source_node);
            self.fire_drag_event(
                cx,
                source.upcast(),
                "dragend",
                hit_test,
                input_event,
                Mode::Protected,
                None,
            );
        }
        self.finish();
    }
}

/// <https://html.spec.whatwg.org/multipage/dnd.html#drag-operation>
fn compute_drag_operation(effect_allowed: &str, drop_effect: &str) -> DragOperation {
    let allows = |effect: &str| {
        matches!(
            effect_allowed,
            "uninitialized" | "all"
        ) || effect_allowed.split_whitespace().any(|token| {
            token.eq_ignore_ascii_case(effect) ||
                (effect == "copy" && token.eq_ignore_ascii_case("copylink")) ||
                (effect == "copy" && token.eq_ignore_ascii_case("copymove")) ||
                (effect == "link" && token.eq_ignore_ascii_case("copylink")) ||
                (effect == "link" && token.eq_ignore_ascii_case("linkmove")) ||
                (effect == "move" && token.eq_ignore_ascii_case("copymove")) ||
                (effect == "move" && token.eq_ignore_ascii_case("linkmove"))
        })
    };
    match drop_effect {
        "copy" if allows("copy") => DragOperation::Copy,
        "link" if allows("link") => DragOperation::Link,
        "move" if allows("move") => DragOperation::Move,
        _ => DragOperation::None,
    }
}
