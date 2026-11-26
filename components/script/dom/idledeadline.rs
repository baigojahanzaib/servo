/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::codegen::GenericBindings::IdleRequestBinding::IdleDeadlineMethods;
use script_bindings::num::Finite;
use script_bindings::reflector::{Reflector, reflect_dom_object};

use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;

/// <https://w3c.github.io/requestidlecallback/#idledeadline>
///
/// v1 approximation: idle callbacks are driven by the animation-frame
/// pipeline (they fire on the next rendering opportunity rather than in
/// spec-exact idle periods), so `timeRemaining` reports the spec-mandated
/// 50ms maximum budget and `didTimeout` is always false -- callbacks fire
/// within a frame regardless of any requested timeout.
#[dom_struct]
pub(crate) struct IdleDeadline {
    reflector_: Reflector,
    time_remaining: f64,
    did_timeout: bool,
}

impl IdleDeadline {
    fn new_inherited(time_remaining: f64, did_timeout: bool) -> IdleDeadline {
        IdleDeadline {
            reflector_: Reflector::new(),
            time_remaining,
            did_timeout,
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        time_remaining: f64,
        did_timeout: bool,
    ) -> DomRoot<IdleDeadline> {
        reflect_dom_object(
            cx,
            Box::new(IdleDeadline::new_inherited(time_remaining, did_timeout)),
            global,
        )
    }
}

impl IdleDeadlineMethods<crate::DomTypeHolder> for IdleDeadline {
    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-timeremaining>
    fn TimeRemaining(&self) -> Finite<f64> {
        Finite::wrap(self.time_remaining)
    }

    /// <https://w3c.github.io/requestidlecallback/#dom-idledeadline-didtimeout>
    fn DidTimeout(&self) -> bool {
        self.did_timeout
    }
}
