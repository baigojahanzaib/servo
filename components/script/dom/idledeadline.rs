/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::codegen::GenericBindings::IdleDeadlineBinding::IdleDeadlineMethods;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_base::cross_process_instant::CrossProcessInstant;

use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;

/// <https://w3c.github.io/requestidlecallback/#idledeadline>
///
/// v1 note: the deadline instant is captured when the callback batch starts
/// firing; timeRemaining() counts down against the 50ms spec budget from then.
#[dom_struct]
pub(crate) struct IdleDeadline {
    reflector_: Reflector,
    #[no_trace]
    deadline: CrossProcessInstant,
    did_timeout: bool,
}

impl IdleDeadline {
    fn new_inherited(deadline: CrossProcessInstant, did_timeout: bool) -> IdleDeadline {
        IdleDeadline {
            reflector_: Reflector::new(),
            deadline,
            did_timeout,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        deadline: CrossProcessInstant,
        did_timeout: bool,
    ) -> DomRoot<IdleDeadline> {
        reflect_dom_object(
            cx,
            Box::new(IdleDeadline::new_inherited(deadline, did_timeout)),
            global,
        )
    }
}

impl IdleDeadlineMethods<crate::DomTypeHolder> for IdleDeadline {
    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-timeremaining>
    fn TimeRemaining(&self) -> script_bindings::num::Finite<f64> {
        let now = CrossProcessInstant::now();
        if self.deadline <= now {
            return script_bindings::num::Finite::wrap(0.0);
        }
        script_bindings::num::Finite::wrap((self.deadline - now).whole_milliseconds() as f64)
    }

    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-didtimeout>
    fn DidTimeout(&self) -> bool {
        self.did_timeout
    }
}
