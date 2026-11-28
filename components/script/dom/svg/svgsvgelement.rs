/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use atomic_refcell::AtomicRefCell;
use base64::Engine as _;
use cssparser::Parser;
use dom_struct::dom_struct;
use html5ever::{LocalName, Prefix, QualName, local_name, ns};
use js::context::{JSContext, NoGC};
use js::rust::HandleObject;
use layout_api::SVGElementData;
use servo_url::ServoUrl;
use style::attr::{AttrValue, parse_double};
use style::parser::ParserContext;
use style::str::char_is_whitespace;
use style::stylesheets::Origin;
use style::values::specified::LengthPercentage;
use style_traits::ParsingMode;
use uuid::Uuid;
use xml5ever::serialize::TraversalScope;

use crate::dom::bindings::codegen::Bindings::AttrBinding::AttrMethods;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::DocumentMethods;
use crate::dom::bindings::codegen::Bindings::NodeBinding::NodeMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{DomRoot, LayoutDom};
use crate::dom::bindings::str::DOMString;
use crate::dom::console::Console;
use crate::dom::document::Document;
use crate::dom::element::attributes::storage::AttrRef;
use crate::dom::element::{AttributeMutation, Element};
use crate::dom::iterators::ShadowIncluding;
use crate::dom::node::virtualmethods::VirtualMethods;
use crate::dom::node::{
    ChildrenMutation, CloneChildrenFlag, Node, NodeDamage, NodeTraits, UnbindContext,
};
use crate::dom::svg::svggraphicselement::SVGGraphicsElement;

#[dom_struct]
pub(crate) struct SVGSVGElement {
    svggraphicselement: SVGGraphicsElement,
    #[no_trace]
    uuid: Uuid,
    viewbox_ratio: Cell<Option<f32>>,
    // The XML source of subtree rooted at this SVG element, serialized into
    // a base64 encoded `data:` url. This is cached to avoid recomputation
    // on each layout and must be invalidated when the subtree changes.
    #[no_trace]
    cached_serialized_data_url: AtomicRefCell<Option<Result<ServoUrl, ()>>>,
}

impl SVGSVGElement {
    fn new_inherited(
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
    ) -> SVGSVGElement {
        SVGSVGElement {
            svggraphicselement: SVGGraphicsElement::new_inherited(local_name, prefix, document),
            uuid: Uuid::new_v4(),
            viewbox_ratio: Default::default(),
            cached_serialized_data_url: Default::default(),
        }
    }

    #[cfg_attr(crown, allow(crown::unrooted_must_root))]
    pub(crate) fn new(
        cx: &mut js::context::JSContext,
        local_name: LocalName,
        prefix: Option<Prefix>,
        document: &Document,
        proto: Option<HandleObject>,
    ) -> DomRoot<SVGSVGElement> {
        Node::reflect_node_with_proto(
            cx,
            Box::new(SVGSVGElement::new_inherited(local_name, prefix, document)),
            document,
            proto,
        )
    }

    pub(crate) fn serialize_and_cache_subtree(&self, cx: &mut js::context::JSContext) {
        let document_fragment = self.owner_document().CreateDocumentFragment(cx);
        let cloned_node = Node::clone(
            cx,
            self.upcast(),
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );
        if document_fragment
            .upcast::<Node>()
            .AppendChild(cx, &cloned_node)
            .is_err()
        {
            error!("Unable to clone SVG tree");
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        }

        self.process_use_elements(cx, &cloned_node);

        let Ok(xml_source) = cloned_node.xml_serialize(TraversalScope::IncludeNode) else {
            *self.cached_serialized_data_url.borrow_mut() = Some(Err(()));
            return;
        };

        let xml_source: String = xml_source.into();
        let base64_encoded_source = base64::engine::general_purpose::STANDARD.encode(xml_source);
        let data_url = format!("data:image/svg+xml;base64,{base64_encoded_source}");
        match ServoUrl::parse(&data_url) {
            Ok(url) => *self.cached_serialized_data_url.borrow_mut() = Some(Ok(url)),
            Err(error) => error!("Unable to parse serialized SVG data url: {error}"),
        };
    }

    fn process_use_elements(&self, cx: &mut JSContext, root_node: &Node) {
        for node in root_node.traverse_preorder(ShadowIncluding::No) {
            if let Some(element) = node.downcast::<Element>() &&
                element.local_name() == &local_name!("use")
            {
                self.process_single_use_element(cx, element, root_node)
            }
        }
    }

    fn process_single_use_element(
        &self,
        cx: &mut JSContext,
        use_element: &Element,
        root_node: &Node,
    ) {
        let mut href = use_element.get_string_attribute(&local_name!("href"));
        if href.str().is_empty() {
            // Legacy sprite sheets use the namespaced `xlink:href` — it carries
            // the same reference per SVG2 and must be honoured when `href` is
            // absent, otherwise those `<use>` elements render nothing at all.
            if let Some(attr) =
                use_element.get_attribute_with_namespace(cx, &ns!(xlink), &local_name!("href"))
            {
                href = attr.Value();
            }
        }
        let Some(id_string) = href.str().strip_prefix("#").map(DOMString::from) else {
            return;
        };

        let document = self.upcast::<Node>().owner_doc();
        let Some(referenced_element) = document.GetElementById(cx, id_string) else {
            return;
        };
        let referenced_node = referenced_element.upcast::<Node>();

        // Don't use this node if it doesn't have an `<svg>` ancestor.
        if !referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| ancestor.is::<SVGSVGElement>())
        {
            return;
        };

        // Don't use this node if it already exists within the same `<svg>` element.
        if referenced_node
            .inclusive_ancestors_unrooted(cx.no_gc(), ShadowIncluding::No)
            .any(|ancestor| *ancestor == self.upcast())
        {
            return;
        };

        let cloned_node = Node::clone(
            cx,
            referenced_node,
            None,
            CloneChildrenFlag::CloneChildren,
            None,
        );

        // The `<use>` element's own attributes must apply to the referenced
        // content: per SVG2 the clone is generated inside a shadow tree that
        // inherits presentation attributes (`fill`, `stroke`, ...) from the
        // `<use>`, and the use's `x`/`y` translate it. Since this serialized
        // rasterization has no shadow tree, approximate it by copying the
        // use's attributes onto the clone — `set_attribute_from_parser`
        // never overwrites, so the referenced element's own attributes keep
        // winning, exactly like the spec's inheritance order. Without this,
        // sprite-sheet icons rendered black regardless of the use's fill.
        if let Some(clone_element) = cloned_node.downcast::<Element>() {
            let mut copies: Vec<(DOMString, Option<Prefix>, DOMString)> = Vec::new();
            let mut x = DOMString::new();
            let mut y = DOMString::new();
            let mut use_transform = DOMString::new();
            for attr in use_element.attrs().borrow().iter() {
                let local = attr.local_name().clone();
                if *attr.namespace() != ns!() {
                    continue;
                }
                if local == local_name!("href") || local == local_name!("id") {
                    continue;
                }
                let value = attr.to_dom_string();
                if local == local_name!("x") {
                    x = value;
                    continue;
                }
                if local == local_name!("y") {
                    y = value;
                    continue;
                }
                if local == local_name!("transform") {
                    // The use's transform applies to the clone too — merged
                    // below rather than copied verbatim.
                    use_transform = value;
                    continue;
                }
                copies.push((
                    DOMString::from(&**attr.local_name()),
                    attr.prefix().cloned(),
                    value,
                ));
            }
            for (name, prefix, value) in copies {
                // Namespaced attributes were filtered out above, so the
                // prefix is always None here in practice.
                let local = LocalName::from(&*name.str());
                clone_element.set_attribute_from_parser(
                    cx,
                    QualName::new(prefix, ns!(), local),
                    value,
                );
            }
            // `x`/`y` on `<use>` translate the clone; fold them into its
            // transform attribute ahead of any transform it already has.
            let x_off = parse_double(&x.str()).unwrap_or(0.0);
            let y_off = parse_double(&y.str()).unwrap_or(0.0);
            if clone_element
                .get_string_attribute(&local_name!("transform"))
                .str()
                .is_empty()
            {
                let mut parts: Vec<String> = Vec::new();
                if x_off != 0.0 || y_off != 0.0 {
                    parts.push(format!("translate({x_off} {y_off})"));
                }
                if !use_transform.str().is_empty() {
                    parts.push(use_transform.str().to_string());
                }
                if !parts.is_empty() {
                    clone_element.set_attribute_from_parser(
                        cx,
                        QualName::new(None, ns!(), local_name!("transform")),
                        DOMString::from(parts.join(" ")),
                    );
                }
            }
        }
        let _ = root_node.AppendChild(cx, &cloned_node);
    }

    pub(crate) fn invalidate_cached_serialized_subtree_and_rasterization_result(
        &self,
        no_gc: &NoGC,
    ) {
        let owner_window = self.owner_window();
        owner_window
            .image_cache()
            .evict_rasterized_image(&self.uuid);
        if let Some(Ok(url)) = &*self.cached_serialized_data_url.borrow() {
            owner_window.layout_mut().remove_cached_image(url);
            owner_window.image_cache().evict_completed_image(
                url,
                owner_window.origin().immutable(),
                &None,
            );
        }

        *self.cached_serialized_data_url.borrow_mut() = None;
        self.upcast::<Node>().dirty(no_gc, NodeDamage::Other);
    }

    /// <https://www.w3.org/TR/SVG11/coords.html#ViewBoxAttribute>
    pub fn ratio_from_view_box(&self, view_box: Option<&DOMString>) -> Option<f32> {
        let view_box = view_box?;

        let view_box_str = view_box.str();
        let mut iter = view_box_str
            .split(|c| char_is_whitespace(c) || c == ',')
            .filter(|part| !part.is_empty());
        let _min_x = iter.next().and_then(|string| parse_double(string).ok())?;
        let _min_y = iter.next().and_then(|string| parse_double(string).ok())?;

        let width = iter.next().and_then(|string| parse_double(string).ok())?;
        if width == 0f64 {
            return None;
        }
        let height = iter.next().and_then(|string| parse_double(string).ok())?;
        if height == 0f64 {
            return None;
        }
        if width < 0f64 || height < 0f64 {
            Console::error_to_console(
                &self.global(),
                "ViewBox does not have valid dimension".into(),
            );
            return None;
        }

        if iter.next().is_none() {
            Some(width as f32 / height as f32)
        } else {
            None
        }
    }
}

impl<'dom> LayoutDom<'dom, SVGSVGElement> {
    pub(crate) fn data(self) -> SVGElementData<'dom> {
        let svg_id = self.unsafe_get().uuid;
        let element = self.upcast::<Element>();
        let width = element.get_attr_for_layout(&ns!(), &local_name!("width"));
        let height = element.get_attr_for_layout(&ns!(), &local_name!("height"));
        let view_box_ratio = self.unsafe_get().viewbox_ratio.get();
        SVGElementData {
            source: self
                .unsafe_get()
                .cached_serialized_data_url
                .borrow()
                .clone(),
            width,
            height,
            view_box_ratio,
            svg_id,
        }
    }
}

impl VirtualMethods for SVGSVGElement {
    fn super_type(&self) -> Option<&dyn VirtualMethods> {
        Some(self.upcast::<SVGGraphicsElement>() as &dyn VirtualMethods)
    }

    fn bind_to_tree(&self, cx: &mut js::context::JSContext, context: &crate::dom::node::BindContext) {
        if let Some(s) = self.super_type() {
            s.bind_to_tree(cx, context);
        }
        // Sprite sheets are svg-rooted and are often injected AFTER the use
        // elements that reference them (gmail/YouTube insert icons before the
        // defs arrive). A freshly-connected svg subtree may carry the missing
        // targets, so re-resolve every use in the document. Resolution is
        // idempotent: already-resolved uses cheaply reclone, unresolved ones
        // get their first chance at content.
        // Skip shadow-tree insertions: use-instance clones are svg elements
        // inserted into the use's shadow root — re-resolving from there is
        // infinite recursion (clone insertion triggers re-resolution triggers
        // clone insertion).
        if context.tree_connected && !self.upcast::<Node>().is_in_a_shadow_tree() {
            let document = self.upcast::<Node>().owner_doc();
            let this_node = self.upcast::<Node>();
            for node in document
                .upcast::<Node>()
                .traverse_preorder(ShadowIncluding::No)
            {
                if let Some(use_element) = node.downcast::<crate::dom::svg::svguseelement::SVGUseElement>() {
                    use_element.resolve_instance(cx);
                }
                // Rasterization reads the live document at serialization
                // time, so freshly-injected sprite content only needs every
                // OTHER svg's cached serialization dropped — re-resolution at
                // bind time can't work anyway since the injected subtree's ids
                // register after this hook runs.
                if !std::ptr::eq(&*node as &crate::dom::node::Node, this_node) &&
                    let Some(svg) = node.downcast::<SVGSVGElement>()
                {
                    svg.invalidate_cached_serialized_subtree_and_rasterization_result(
                        cx.no_gc(),
                    );
                }
            }
        }
    }

    fn attribute_mutated(
        &self,
        cx: &mut js::context::JSContext,
        attr: AttrRef<'_>,
        mutation: AttributeMutation,
    ) {
        self.super_type()
            .unwrap()
            .attribute_mutated(cx, attr, mutation);

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn attribute_affects_presentational_hints(&self, attr: AttrRef<'_>) -> bool {
        match attr.local_name() {
            &local_name!("width") | &local_name!("height") => true,
            _ => self
                .super_type()
                .unwrap()
                .attribute_affects_presentational_hints(attr),
        }
    }

    fn parse_plain_attribute(&self, name: &LocalName, value: DOMString) -> AttrValue {
        match *name {
            local_name!("width") | local_name!("height") => {
                let value = &value.str();
                let parser = &mut Parser::new(value);
                let doc = self.owner_document();
                let url = doc.url().into_url().into();
                let context = ParserContext::new(
                    Origin::Author,
                    &url,
                    None,
                    ParsingMode::ALLOW_UNITLESS_LENGTH,
                    doc.quirks_mode(),
                    /* namespaces = */ Default::default(),
                    None,
                    None,
                    /* attr_taint = */ Default::default(),
                );
                let val = LengthPercentage::parse_quirky(
                    &context,
                    parser,
                    style::values::specified::AllowQuirks::Always,
                );
                AttrValue::LengthPercentage(value.to_string(), val.ok())
            },

            local_name!("viewBox") => {
                let viewbox_ratio = self.ratio_from_view_box(Some(&value));
                self.viewbox_ratio.set(viewbox_ratio);
                self.super_type()
                    .unwrap()
                    .parse_plain_attribute(name, value)
            },
            _ => self
                .super_type()
                .unwrap()
                .parse_plain_attribute(name, value),
        }
    }

    fn children_changed(&self, cx: &mut JSContext, mutation: &ChildrenMutation) {
        if let Some(super_type) = self.super_type() {
            super_type.children_changed(cx, mutation);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }

    fn unbind_from_tree(&self, cx: &mut js::context::JSContext, context: &UnbindContext<'_>) {
        if let Some(s) = self.super_type() {
            s.unbind_from_tree(cx, context);
        }

        self.invalidate_cached_serialized_subtree_and_rasterization_result(cx.no_gc());
    }
}
