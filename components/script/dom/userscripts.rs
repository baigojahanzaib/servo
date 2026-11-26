/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use js::context::JSContext;

use crate::dom::html::htmlheadelement::HTMLHeadElement;
use crate::dom::node::NodeTraits;
use crate::realms::enter_auto_realm;

/// Evaluate user scripts synchronously when the `<head>` element is inserted,
/// which is the document-start moment: before any page script in the document
/// can have run. Scripts must not be deferred to a delayed task here; the
/// parser continues into the document's own scripts immediately after head
/// insertion, so anything queued runs strictly too late (after page scripts
/// have already observed the unmodified global object).
pub(crate) fn load_script(cx: &mut JSContext, head: &HTMLHeadElement) {
    let doc = head.owner_document();
    let userscripts = doc.window().userscripts().to_owned();
    if userscripts.is_empty() {
        return;
    }
    let global_scope = doc.window().as_global_scope();
    let mut realm = enter_auto_realm(cx, global_scope);
    let cx = &mut realm.current_realm();

    for user_script in userscripts {
        _ = global_scope.evaluate_js_on_global(
            cx,
            user_script.script().into(),
            &user_script
                .source_file()
                .map(|path| path.to_string_lossy().to_string())
                .unwrap_or_default(),
            None,
            None,
        );
    }
}
