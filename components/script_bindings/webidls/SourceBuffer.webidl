/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/media-source/#dom-sourcebuffer

enum AppendMode { "segments", "sequence" };

[Exposed=(Window,DedicatedWorker)]
interface SourceBuffer : EventTarget {
  [Throws] attribute AppendMode mode;
  readonly attribute boolean updating;
  readonly attribute TimeRanges buffered;
  [Throws] attribute double timestampOffset;
  readonly attribute double appendWindowStart;
  [Throws] attribute double appendWindowEnd;
           attribute EventHandler onupdatestart;
           attribute EventHandler onupdate;
           attribute EventHandler onupdateend;
           attribute EventHandler onerror;
           attribute EventHandler onabort;

  [Throws] undefined appendBuffer(BufferSource data);
  [Throws] undefined abort();
  [Throws] undefined remove(double start, double end);
};
