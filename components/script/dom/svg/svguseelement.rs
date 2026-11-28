/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use stylo_atoms::Atom;
use html5ever::{local_name, ns, LocalName, Prefix};
use js::rust::HandleObject;

use crate::dom::bindings::codegen::Bindings::AttrBinding::AttrMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::codegen::Bindings::ShadowRootBinding::{
    ShadowRootMode, SlotAssignmentMode,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::document::Document;
use crate::dom::element::element::Element;
use crate::dom::element::AttributeMutation;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{BindContext, CloneChildrenFlag, Node, SuppressObserver};
use crate::dom::shadowroot::shadowroot::IsUserAgentWidget;
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;
use crate::dom::svg::svgsvgelement::SVGSVGElement;

#[dom_struct]
pub(crate) struct SVGUseElement {
    svggraphicselement: SVGGraphicsElement,
}

impl SVGUseElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGUseElement {
        SVGUseElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
        }
    }

    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGUseElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGUseElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    /// Resolve a same-document `href="#id"` (or legacy `xlink:href`) reference
    /// into a closed shadow tree containing a deep clone of the referenced
    /// element, per <https://svgwg.org/svg2-draft/struct.html#UseShadowTree>.
    ///
    /// v1 limitations: no cross-document references, no re-resolution when
    /// the referenced subtree mutates after the use bound, no chained
    /// use-in-use resolution beyond the direct containment guard.
    pub(crate) fn resolve_instance(&self, cx: &mut js::context::JSContext) {
        let element = self.upcast::<Element>();
        let mut href = element.get_string_attribute(&local_name!("href"));
        if href.str().is_empty() {
            if let Some(attr) =
                element.get_attribute_with_namespace(cx, &ns!(xlink), &local_name!("href"))
            {
                href = attr.Value();
            }
        }
        let Some(id) = href.str().strip_prefix("#").map(str::to_owned) else {
            return;
        };
        if id.is_empty() {
            return;
        }
        let node = self.upcast::<Node>();
        let document = node.owner_doc();
        let Some(referenced) = document.get_element_by_id(cx.no_gc(), &Atom::from(id.as_str())) else {
            return;
        };
        let referenced_node = referenced.upcast::<Node>();

        // Only content inside an `<svg>` is valid use-source.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        }
        // Circularity guard: never clone a subtree containing this use
        // element, nor when the reference is inside our own shadow output.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == node) ||
            node.inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
                .any(|ancestor| *ancestor == referenced_node)
        {
            return;
        }

        let cloned = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );

        // Per SVG2, a referenced `<symbol>` instances as an `<svg>` element:
        // the clone root is converted — same attributes and children — and
        // the use element's width/height transfer onto it (they define the
        // viewport the symbol's viewBox scales into). Without this, sprite
        // icons render unscaled at their viewBox unit size.
        let cloned = if cloned
            .downcast::<Element>()
            .is_some_and(|e| e.local_name() == &local_name!("symbol"))
        {
            let svg = crate::dom::element::create::create_element(
                cx,
                html5ever::QualName::new(None, ns!(svg), local_name!("svg")),
                None,
                &document,
                crate::dom::element::ElementCreator::ScriptCreated,
                crate::dom::element::CustomElementCreationMode::Asynchronous,
                None,
            );
            let cloned_element = cloned.downcast::<Element>().unwrap();
            let copies: Vec<(DOMString, DOMString)> = cloned_element
                .attrs()
                .borrow()
                .iter()
                .filter(|attr| *attr.namespace() == ns!())
                .map(|attr| {
                    (
                        DOMString::from(&**attr.local_name()),
                        attr.to_dom_string(),
                    )
                })
                .collect();
            for (name, value) in copies {
                svg.set_attribute_from_parser(
                    cx,
                    html5ever::QualName::new(
                        None,
                        ns!(),
                        LocalName::from(&*name.str()),
                    ),
                    value,
                );
            }
            // use width/height transfer; explicit use attributes win,
            // otherwise the instance is viewport-sized (100%).
            for dim in ["width", "height"] {
                let use_val = element.get_string_attribute(&LocalName::from(dim));
                let value = if use_val.str().is_empty() {
                    DOMString::from("100%")
                } else {
                    use_val
                };
                svg.set_attribute_from_parser(
                    cx,
                    html5ever::QualName::new(None, ns!(), LocalName::from(dim)),
                    value,
                );
            }
            let kids: Vec<DomRoot<Node>> = cloned.children().collect();
            for child in kids {
                child.remove_self(cx);
                let _ = svg.upcast::<Node>().AppendChild(cx, &child);
            }
            DomRoot::from_ref(svg.upcast::<Node>())
        } else {
            cloned
        };

        let shadow_root = match element.shadow_root() {
            Some(root) => root,
            None => match element.attach_shadow(
                cx,
                IsUserAgentWidget::Yes,
                ShadowRootMode::Closed,
                false,
                false,
                false,
                SlotAssignmentMode::Manual,
            ) {
                Ok(root) => root,
                Err(_) => return,
            },
        };
        let shadow_node = shadow_root.upcast::<Node>();
        let existing: Vec<DomRoot<Node>> = shadow_node.children().collect();
        for child in existing {
            child.remove_self(cx);
        }
        if Node::pre_insert(cx, &cloned, shadow_node, None).is_ok() {
            node.dirty(cx.no_gc(), crate::dom::node::NodeDamage::Other);
        }
    }
}

impl VirtualMethods for SVGUseElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn bind_to_tree(&self, cx: &mut js::context::JSContext, context: &BindContext) {
        if let Some(s) = self.super_type() {
            s.bind_to_tree(cx, context);
        }
        if context.tree_connected {
            self.resolve_instance(cx);
        }
    }

    fn attribute_mutated(&self, cx: &mut js::context::JSContext, attr: AttrRef<'_>, mutation: AttributeMutation) {
        if let Some(s) = self.super_type() {
            s.attribute_mutated(cx, attr, mutation);
        }
        if attr.local_name() == &local_name!("href") &&
            matches!(attr.namespace(), &ns!() | &ns!(xlink)) &&
            self.upcast::<Node>().is_connected()
        {
            self.resolve_instance(cx);
        }
    }
}
