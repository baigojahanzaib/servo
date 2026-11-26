/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::reflect_dom_object;
use servo_arc::Arc;
use style::shared_lock::ToCssWithGuard;
use style::stylesheets::{ContainerRule, CssRuleType};
use style_traits::ToCss;

use super::cssconditionrule::CSSConditionRule;
use super::cssrule::SpecificCSSRule;
use super::cssstylesheet::CSSStyleSheet;
use crate::dom::bindings::codegen::Bindings::CSSContainerRuleBinding::CSSContainerRuleMethods;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::css::cssgroupingrule::CSSGroupingRule;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct CSSContainerRule {
    css_condition_rule: CSSConditionRule,
    #[ignore_malloc_size_of = "Stylo"]
    #[no_trace]
    container_rule: RefCell<Arc<ContainerRule>>,
}

impl CSSContainerRule {
    fn new_inherited(
        parent_rule: Option<&CSSGroupingRule>,
        parent_stylesheet: &CSSStyleSheet,
        containerrule: Arc<ContainerRule>,
    ) -> CSSContainerRule {
        let list = containerrule.rules.clone();
        CSSContainerRule {
            css_condition_rule: CSSConditionRule::new_inherited(
                parent_rule,
                parent_stylesheet,
                list,
            ),
            container_rule: RefCell::new(containerrule),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        parent_rule: Option<&CSSGroupingRule>,
        parent_stylesheet: &CSSStyleSheet,
        containerrule: Arc<ContainerRule>,
    ) -> DomRoot<CSSContainerRule> {
        reflect_dom_object(
            cx,
            Box::new(CSSContainerRule::new_inherited(
                parent_rule,
                parent_stylesheet,
                containerrule,
            )),
            window,
        )
    }

    pub(crate) fn update_rule(
        &self,
        containerrule: Arc<ContainerRule>,
        guard: &style::shared_lock::SharedRwLockReadGuard,
    ) {
        self.css_condition_rule
            .update_rules(containerrule.rules.clone(), guard);
        *self.container_rule.borrow_mut() = containerrule;
    }

    /// <https://drafts.csswg.org/css-conditional-5/#csscontainerrule>
    pub(crate) fn get_condition_text(&self) -> DOMString {
        self.container_rule
            .borrow()
            .conditions
            .to_css_string()
            .into()
    }
}

impl SpecificCSSRule for CSSContainerRule {
    fn ty(&self) -> CssRuleType {
        CssRuleType::Container
    }

    fn get_css(&self) -> DOMString {
        let guard = self.css_condition_rule.shared_lock().read();
        self.container_rule.borrow().to_css_string(&guard).into()
    }
}

impl CSSContainerRuleMethods<crate::DomTypeHolder> for CSSContainerRule {
    /// <https://drafts.csswg.org/css-conditional-5/#dom-csscontainerrule-containername>
    fn ContainerName(&self) -> DOMString {
        // The name is shared by every condition in the prelude list in
        // practice (the grammar produces one condition); serialize the first
        // named one if present.
        for condition in self.container_rule.borrow().conditions.0.iter() {
            let name = condition.name();
            if !name.is_none() {
                return name.to_css_string().into();
            }
        }
        DOMString::from("")
    }

    /// <https://drafts.csswg.org/css-conditional-5/#dom-csscontainerrule-containerquery>
    fn ContainerQuery(&self) -> DOMString {
        self.container_rule
            .borrow()
            .conditions
            .0
            .iter()
            .filter_map(|condition| condition.query_condition().map(|query| query.to_css_string()))
            .collect::<Vec<_>>()
            .join(", ")
            .into()
    }
}
