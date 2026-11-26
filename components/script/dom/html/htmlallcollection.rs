/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use html5ever::{local_name, namespace_url, ns};
use js::context::JSContext;
use malloc_size_of::MallocSizeOf;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::HTMLAllCollectionBinding::HTMLAllCollectionMethods;
use crate::dom::bindings::codegen::Bindings::HTMLCollectionBinding::HTMLCollectionMethods;
use crate::dom::bindings::codegen::UnionTypes::HTMLCollectionOrElement;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use js::context::NoGC;
use crate::dom::bindings::trace::JSTraceable;
use crate::dom::element::Element;
use crate::dom::html::htmlcollection::{CollectionFilter, HTMLCollection};
use crate::dom::node::{Node, NodeTraits};
use crate::dom::window::Window;

/// <https://html.spec.whatwg.org/multipage/common-dom-interfaces.html#htmlallcollection>
///
/// A live collection of all elements in the document. Instances emulate
/// `undefined` for typeof/boolean/loose-equality purposes (wired through the
/// JSCLASS_EMULATES_UNDEFINED class flag) while remaining ordinary objects
/// for identity and strict equality.
#[dom_struct]
pub(crate) struct HTMLAllCollection {
    reflector_: Reflector,
    collection: Dom<HTMLCollection>,
}

impl HTMLAllCollection {
    fn new_inherited(collection: &HTMLCollection) -> HTMLAllCollection {
        HTMLAllCollection {
            reflector_: Reflector::new(),
            collection: Dom::from_ref(collection),
        }
    }

    pub(crate) fn new(cx: &mut JSContext, window: &Window, root: &Node) -> DomRoot<Self> {
        #[derive(JSTraceable, MallocSizeOf)]
        struct AllElementsFilter;
        impl CollectionFilter for AllElementsFilter {
            fn filter(&self, _elem: &Element, _root: &Node) -> bool {
                true
            }
        }
        let collection = HTMLCollection::create(cx, window, root, Box::new(AllElementsFilter));
        reflect_dom_object(cx, Box::new(Self::new_inherited(&collection)), window)
    }

    fn matching_elements(&self, cx: &JSContext, key: &Atom) -> Vec<DomRoot<Element>> {
        self.collection
            .elements_iter(cx.no_gc())
            .filter(|elem| {
                elem.get_id().is_some_and(|id| id == *key) ||
                    (elem.namespace() == &ns!(html) &&
                        elem.get_name().is_some_and(|name| name == *key))
            })
            .map(|elem| elem.as_rooted())
            .collect()
    }
}

impl HTMLAllCollectionMethods<crate::DomTypeHolder> for HTMLAllCollection {
    /// <https://html.spec.whatwg.org/multipage/common-dom-interfaces.html#dom-htmlcollection-length>
    fn Length(&self, cx: &JSContext) -> u32 {
        self.collection.Length(cx)
    }

    /// <https://html.spec.whatwg.org/multipage/common-dom-interfaces.html#dom-htmlallcollection-item>
    fn Item(&self, cx: &JSContext, index: u32) -> Option<DomRoot<Element>> {
        self.collection.Item(cx, index)
    }

    /// <https://html.spec.whatwg.org/multipage/common-dom-interfaces.html#dom-htmlallcollection-nameditem>
    fn NamedItem(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        if name.is_empty() {
            return None;
        }
        let key = Atom::from(name);
        let matches = self.matching_elements(cx, &key);
        match matches.len() {
            0 => None,
            1 => Some(HTMLCollectionOrElement::Element(
                matches.into_iter().next().unwrap(),
            )),
            _ => {
                // Multiple matches: return a live sub-collection of the matching elements.
                #[derive(JSTraceable, MallocSizeOf)]
                struct NamedFilter {
                    #[no_trace]
                    key: Atom,
                }
                impl CollectionFilter for NamedFilter {
                    fn filter(&self, elem: &Element, _root: &Node) -> bool {
                        elem.get_id().is_some_and(|id| id == self.key) ||
                            (elem.namespace() == &ns!(html) &&
                                elem.get_name().is_some_and(|name| name == self.key))
                    }
                }
                let root = self.collection.root_node();
                let window = root.owner_window();
                let sub = HTMLCollection::create(cx, &window, &root, Box::new(NamedFilter { key }));
                Some(HTMLCollectionOrElement::HTMLCollection(sub))
            },
        }
    }

    /// <https://html.spec.whatwg.org/multipage/common-dom-interfaces.html#dom-htmlallcollection-item>
    fn IndexedGetter(&self, cx: &JSContext, index: u32) -> Option<DomRoot<Element>> {
        self.Item(cx, index)
    }

    // check-tidy: no specs after this line
    fn NamedGetter(&self, cx: &mut JSContext, name: DOMString) -> Option<HTMLCollectionOrElement> {
        self.NamedItem(cx, name)
    }

    fn SupportedPropertyNames(&self, no_gc: &NoGC) -> Vec<DOMString> {
        self.collection.SupportedPropertyNames(no_gc)
    }
}
