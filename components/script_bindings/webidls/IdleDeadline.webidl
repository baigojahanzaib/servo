/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/requestidlecallback/
callback IdleRequestCallback = undefined (IdleDeadline deadline);

dictionary IdleRequestOptions {
  unsigned long timeout;
};

[Exposed=Window]
interface IdleDeadline {
  double timeRemaining();
  readonly attribute boolean didTimeout;
};

partial interface Window {
  [Throws] unsigned long requestIdleCallback(IdleRequestCallback callback, optional IdleRequestOptions options = {});
  [Throws] undefined cancelIdleCallback(unsigned long handle);
};
