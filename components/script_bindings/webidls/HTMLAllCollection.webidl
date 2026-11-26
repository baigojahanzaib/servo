/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://html.spec.whatwg.org/multipage/common-dom-interfaces.html#htmlallcollection

// NOTE: instances must emulate undefined (typeof, ToBoolean, loose equality)
// while remaining ordinary objects for identity/strict equality. This is
// provided by the JSCLASS_EMULATES_UNDEFINED class flag (see codegen).

[Exposed=Window, LegacyOverrideBuiltIns]
interface HTMLAllCollection {
  readonly attribute unsigned long length;
  getter Element? item(unsigned long index);
  getter (HTMLCollection or Element)? namedItem(DOMString name);
};
