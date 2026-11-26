/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;

use crate::dom::bindings::codegen::Bindings::SourceBufferBinding::{
    AppendMode, SourceBufferMethods,
};
use crate::dom::bindings::codegen::UnionTypes::ArrayBufferViewOrArrayBuffer;
use crate::dom::bindings::error::{Error, ErrorResult, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use stylo_atoms::Atom;
use crate::dom::globalscope::GlobalScope;
use crate::dom::timeranges::{TimeRanges, TimeRangesContainer};

#[dom_struct]
pub(crate) struct SourceBuffer {
    eventtarget: EventTarget,
    mime_type: DOMString,
    updating: Cell<bool>,
    mode: Cell<AppendMode>,
    timestamp_offset: Cell<f64>,
    buffered: script_bindings::cell::DomRefCell<TimeRangesContainer>,
    /// Cached init segment (bytes before the first media cluster/moof), needed
    /// to re-sequence the demuxer after a seek reset — post-seek appends from
    /// real players are media-only and a fresh demuxer can't decode them.
    init_cache: script_bindings::cell::DomRefCell<Vec<u8>>,
    /// Set by seek: the next append must re-feed the cached init first.
    needs_init_refeed: std::cell::Cell<bool>,
    track_id: Cell<u64>,
    media_source: script_bindings::cell::DomRefCell<Option<Dom<crate::dom::media::mediasource::MediaSource>>>,
    /// Streaming webm indexer — parses the concatenated byte stream with
    /// cross-append state, immune to clusters split across appendBuffer calls.
    webm_indexer: script_bindings::cell::DomRefCell<crate::dom::media::ebml::WebmIndexer>,
    /// Retained media fragments (start_secs, end_secs, bytes) — the MSE-spec
    /// behavior: appended data persists across seeks, so a backward seek into
    /// buffered media needs NO new appends from the page. The push pipeline
    /// genuinely discards everything on seek reset, so the engine must own
    /// retention and re-feed. Without this, spec-correct players (youtube)
    /// never re-append post-seek and playback starves (sec1zt119/123).
    retained: script_bindings::cell::DomRefCell<Vec<(f64, f64, Vec<u8>)>>,
}

/// Cap on retained media bytes per track — mirrors the engine-side appsrc
/// queue cap. Eviction drops the oldest-ending fragments first, which matches
/// what playback consumes first.
const MAX_RETAINED_BYTES: usize = 48 * 1024 * 1024;

impl SourceBuffer {
    fn new_inherited(mime_type: DOMString, _buffered: &TimeRanges) -> SourceBuffer {
        SourceBuffer {
            eventtarget: EventTarget::new_inherited(),
            mime_type,
            updating: Cell::new(false),
            mode: Cell::new(AppendMode::Segments),
            timestamp_offset: Cell::new(0.0),
            buffered: script_bindings::cell::DomRefCell::new(TimeRangesContainer::default()),
            init_cache: script_bindings::cell::DomRefCell::new(Vec::new()),
            needs_init_refeed: std::cell::Cell::new(false),
            media_source: script_bindings::cell::DomRefCell::new(None),
            track_id: Cell::new(0),
            webm_indexer: script_bindings::cell::DomRefCell::new(crate::dom::media::ebml::WebmIndexer::new()),
            retained: script_bindings::cell::DomRefCell::new(Vec::new()),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        global: &GlobalScope,
        mime_type: DOMString,
    ) -> DomRoot<SourceBuffer> {
        let buffered = TimeRanges::new(cx, global.as_window(), TimeRangesContainer::default());
        reflect_dom_object_with_proto(
            cx,
            Box::new(SourceBuffer::new_inherited(mime_type, &buffered)),
            global,
            None,
        )
    }

    /// MSE seek support: the pipeline's flush+reset genuinely drops all
    /// pipelined media, but the RETAINED fragment cache survives — re-feed it
    /// so spec-correct players (which never re-append, since per spec buffered
    /// data persists across seeks) keep playing. The buffered index is kept
    /// intact: it describes the retention, and the re-feed restores exactly
    /// what it promises. The re-feed itself retries as a task chain because
    /// the engine pipeline reset is asynchronous — pushes landing mid-reset
    /// are rejected by the not-yet-playing appsrc.
    pub(crate) fn handle_seek(&self, seek_target: f64) {
        self.needs_init_refeed.set(true);
        self.webm_indexer.borrow_mut().reset();
        self.refeed_retained(seek_target, 0, 0);
    }

    /// Push init + retained fragments one at a time, in time order, each as a
    /// chained task. Per-fragment pushes matter: the engine enforces a per-
    /// track byte quota and a single mega-push of the whole retention would
    /// exceed it and permanently fail. Failures retry the same fragment —
    /// early attempts commonly land while the pipeline is mid-reset (not-yet-
    /// playing appsrcs reject) or momentarily full (backpressure drains as
    /// playback resumes). Each chain link yields the event loop.
    fn refeed_retained(&self, seek_target: f64, fragment_index: usize, retries: u32) {
        if retries >= 200 {
            return;
        }
        let payload = {
            let init = self.init_cache.borrow().clone();
            let retained = self.retained.borrow();
            let mut frags: Vec<(f64, f64, Vec<u8>)> = retained
                .iter()
                .filter(|(_, end, _)| *end > seek_target)
                .cloned()
                .collect();
            frags.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            if fragment_index == 0 && !init.is_empty() {
                // The first push carries the init segment ahead of the first
                // media fragment so the fresh demuxer sees stream headers.
                frags.first().map(|(fs, fe, data)| {
                    let mut joined = init.clone();
                    joined.extend_from_slice(data);
                    (0usize, (*fs, *fe, joined), frags.len())
                })
            } else {
                frags.get(fragment_index).map(|f| (fragment_index, f.clone(), frags.len()))
            }
        };
        let Some((idx, (_, _, bytes), total_frags)) = payload else {
            return;
        };
        if fragment_index >= total_frags && fragment_index != 0 {
            return;
        }
        let pushed = if let Some(ref ms) = *self.media_source.borrow() {
            ms.push_appended_data(self.track_id.get(), bytes)
        } else {
            false
        };
        let (next_index, next_retries) = if pushed {
            (idx + 1, 0)
        } else {
            (idx, retries + 1)
        };
        if pushed && idx + 1 >= total_frags {
            return;
        }
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_refeed_retry: move |_cx| {
                let this = this.root();
                this.refeed_retained(seek_target, next_index, next_retries);
            }));
    }

    pub(crate) fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        let mut out = Vec::new();
        let ranges = self.buffered.borrow();
        for i in 0..ranges.len() {
            if let (Ok(start), Ok(end)) = (ranges.start(i), ranges.end(i)) {
                out.push((start, end));
            }
        }
        out
    }

    pub(crate) fn fire_simple_event_cx(&self, cx: &mut JSContext, name: Atom) {
        let event = Event::new(
            cx,
            &self.global(),
            name,
            EventBubbles::DoesNotBubble,
            EventCancelable::NotCancelable,
        );
        event.upcast::<Event>().fire(cx, self.upcast());
    }

    pub(crate) fn set_track_id(&self, track: u64) {
        self.track_id.set(track);
    }

    pub(crate) fn set_media_source(&self, ms: &crate::dom::media::mediasource::MediaSource) {
        *self.media_source.borrow_mut() = Some(Dom::from_ref(ms));
    }

    pub(crate) fn mime_type(&self) -> String {
        self.mime_type.str().to_string()
    }
}

impl SourceBufferMethods<crate::DomTypeHolder> for SourceBuffer {
    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn GetMode(&self) -> Fallible<AppendMode> {
        Ok(self.mode.get())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-mode>
    fn SetMode(&self, value: AppendMode) -> ErrorResult {
        self.mode.set(value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-updating>
    fn Updating(&self) -> bool {
        self.updating.get()
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-buffered>
    fn Buffered(&self, cx: &mut JSContext) -> DomRoot<TimeRanges> {
        TimeRanges::new(cx, self.global().as_window(), self.buffered.borrow().clone())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn GetTimestampOffset(&self) -> Fallible<Finite<f64>> {
        Ok(Finite::wrap(self.timestamp_offset.get()))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-timestampoffset>
    fn SetTimestampOffset(&self, value: Finite<f64>) -> ErrorResult {
        self.timestamp_offset.set(*value);
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowstart>
    fn AppendWindowStart(&self) -> Finite<f64> {
        Finite::wrap(0.0)
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn GetAppendWindowEnd(&self) -> Fallible<Finite<f64>> {
        Ok(Finite::wrap(f64::INFINITY))
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendwindowend>
    fn SetAppendWindowEnd(&self, _value: Finite<f64>) -> ErrorResult {
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
    fn AppendBuffer(&self, cx: &mut JSContext, data: ArrayBufferViewOrArrayBuffer) -> Fallible<()> {
        let _ = cx;
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        let mut bytes = crate::dom::bindings::buffer_source::get_buffer_source_copy((&data).into());
        // Init segment caching: detect webm (EBML magic 0x1A45DFA3, init ends at
        // the first Cluster 0x1F43B675) or fmp4 (init ends at the first moof box).
        // Needed because post-seek appends are media-only and the reset demuxer
        // cannot decode them without re-seeing the stream headers.
        if self.init_cache.borrow().is_empty() {
            let init_len = webm_init_len(&bytes).or_else(|| fmp4_init_len(&bytes));
            if let Some(len) = init_len && len > 0 {
                *self.init_cache.borrow_mut() = bytes[..len].to_vec();
            }
        }
        if self.needs_init_refeed.get() {
            self.needs_init_refeed.set(false);
            let cache = self.init_cache.borrow();
            let already_has_init = webm_init_len(&bytes).is_some() || fmp4_init_len(&bytes).is_some();
            if !cache.is_empty() && !already_has_init {
                let mut joined = cache.clone();
                joined.extend_from_slice(&bytes);
                bytes = joined;
            }
        }
        // timestampOffset: reposition fmp4 media on the timeline by rewriting
        // tfdt decode times in place. WebM offset application is unimplemented
        // (cluster timecode vint resizing) — the sites that need offsets are
        // hls.js-class fmp4 transmuxing. No-op when the offset is zero.
        let offset = self.timestamp_offset.get();
        // Rewrite whenever moof boxes are present — tfdt only exists inside
        // media segments, so concatenated init+media appends are safe too.
        if offset != 0.0 && bytes.len() >= 8 &&
            bytes.windows(4).any(|w| w == b"moof")
        {
            let timescale = crate::dom::media::ebml::fmp4_timescale(&self.init_cache.borrow()) as f64;
            let delta = (offset * timescale).round() as i64;
            crate::dom::media::ebml::rewrite_tfdt_offsets(&mut bytes, delta);
        }
        // <https://w3c.github.io/media-source/#dom-sourcebuffer-appendbuffer>
        // updating flips synchronously, the append completes in a queued task
        // with updatestart/update/updateend — real player JS loops on these.
        // Quota enforcement: push into the engine FIRST and bail with
        // QuotaExceededError when the track buffer is full (the engine rejects
        // the push) — spec-compliant backpressure; real players respond by
        // removing buffered ranges. Rejected bytes are never indexed, keeping
        // the buffered index consistent with what the engine actually holds.
        if let Some(ref ms) = *self.media_source.borrow() {
            if !ms.push_appended_data(self.track_id.get(), bytes.clone()) {
                return Err(Error::QuotaExceeded {
                    quota: None,
                    requested: None,
                });
            }
        }
        self.updating.set(true);
        let prev_indexed_tc = self.webm_indexer.borrow().last_timecode();
        let timecodes = self.webm_indexer.borrow_mut().feed(&bytes);
        // fmp4 path: webm scan finds nothing in mp4 — fall back to tfdt scanning
        // so buffered indexing works for twitch/vimeo-class mp4 appends.
        let mut fmp4_range = None;
        if timecodes.is_empty() {
            let ranges = crate::dom::media::ebml::scan_fmp4_timecodes(&bytes, &self.init_cache.borrow());
            fmp4_range = ranges.first().copied();
        }
        let mut retained_range = None;
        if let Some((fs, fe)) = fmp4_range {
            let mut ranges = self.buffered.borrow_mut();
            let _ = ranges.add(fs, fe);
            retained_range = Some((fs, fe));
        }
        if let (Some(first), Some(last)) = (timecodes.first(), timecodes.last()) {
            // Segments are appended in presentation order by real players;
            // extend the trailing range or start a new one. Estimating the
            // tail duration from cluster spacing is refined in later stages.
            let mut ranges = self.buffered.borrow_mut();
            // span estimate: prefer in-batch spacing; otherwise use the gap
            // from the previous append's last cluster — real players (youtube
            // verified) often append one cluster per call, so in-batch spacing
            // is unavailable and a fixed 1s guess fragments the ranges.
            let span = if timecodes.len() > 1 {
                (last - first) / (timecodes.len() - 1) as f64
            } else if let Some(prev) = prev_indexed_tc {
                let gap = *first - prev;
                if gap > 0.05 && gap < 30.0 { gap } else { 1.0 }
            } else {
                1.0
            };
            let _ = ranges.add(*first, last + span);
            retained_range = Some((*first, last + span));
        }
        // Fragment retention (spec-persistent buffering): keep the appended
        // bytes keyed by their time range so seeks can re-feed without the
        // page re-appending. Bytes are the post-timestampOffset-rewrite
        // canonical stream. Eviction is oldest-end-first once over the cap.
        if let Some((fs, fe)) = retained_range {
            let mut retained = self.retained.borrow_mut();
            retained.push((fs, fe, bytes.clone()));
            let total: usize = retained.iter().map(|(_, _, b)| b.len()).sum();
            if total > MAX_RETAINED_BYTES {
                let mut excess = total - MAX_RETAINED_BYTES;
                retained.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
                while excess > 0 && retained.len() > 1 {
                    let (_, _, b) = retained.first().unwrap();
                    let len = b.len();
                    if len > excess + MAX_RETAINED_BYTES / 2 {
                        break;
                    }
                    excess = excess.saturating_sub(len);
                    retained.remove(0);
                }
            }
        }
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_append_complete: move |cx| {
                let this = this.root();
                this.fire_simple_event_cx(cx, Atom::from("updatestart"));
                this.fire_simple_event_cx(cx, Atom::from("update"));
                this.updating.set(false);
                this.fire_simple_event_cx(cx, Atom::from("updateend"));
            }));
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-abort>
    fn Abort(&self) -> Fallible<()> {
        Ok(())
    }

    /// <https://w3c.github.io/media-source/#dom-sourcebuffer-remove>
    fn Remove(&self, start: Finite<f64>, end: Finite<f64>) -> Fallible<()> {
        if self.updating.get() {
            return Err(Error::InvalidState(None));
        }
        if *end <= *start || start.is_nan() || end.is_nan() {
            return Err(Error::Type(c"Invalid removal range".into()));
        }
        self.updating.set(true);
        // The bytes were already handed to the pipeline; removal is honest
        // index bookkeeping so eviction loops see the ranges they expect.
        let (rs, re) = (*start, *end);
        let mut kept = crate::dom::timeranges::TimeRangesContainer::default();
        {
            let ranges = self.buffered.borrow();
            for i in 0..ranges.len() {
                if let (Ok(a), Ok(b)) = (ranges.start(i), ranges.end(i)) {
                    if b <= rs || a >= re {
                        let _ = kept.add(a, b);
                    } else {
                        if a < rs {
                            let _ = kept.add(a, rs);
                        }
                        if b > re {
                            let _ = kept.add(re, b);
                        }
                    }
                }
            }
        }
        *self.buffered.borrow_mut() = kept;
        // Retention must follow the same eviction: fragments overlapping the
        // removed range are dropped (spec removes the coded frames in range;
        // re-feeding them afterward would resurrect media the page discarded).
        self.retained
            .borrow_mut()
            .retain(|(fs, fe, _)| *fe <= rs || *fs >= re);
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_remove_complete: move |cx| {
                let this = this.root();
                this.fire_simple_event_cx(cx, Atom::from("updatestart"));
                this.fire_simple_event_cx(cx, Atom::from("update"));
                this.updating.set(false);
                this.fire_simple_event_cx(cx, Atom::from("updateend"));
            }));
        Ok(())
    }

    event_handler!(updatestart, GetOnupdatestart, SetOnupdatestart);
    event_handler!(update, GetOnupdate, SetOnupdate);
    event_handler!(updateend, GetOnupdateend, SetOnupdateend);
    event_handler!(error, GetOnerror, SetOnerror);
    event_handler!(abort, GetOnabort, SetOnabort);
}

/// Length of the webm init portion (everything before the first Cluster), or
/// None if the buffer doesn't start a fresh webm stream.
fn webm_init_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 || bytes[..4] != [0x1A, 0x45, 0xDF, 0xA3] {
        return None;
    }
    let mut pos = 4;
    while pos + 4 <= bytes.len() {
        if bytes[pos] == 0x1F && bytes[pos + 1] == 0x43 && bytes[pos + 2] == 0xB6 &&
            bytes[pos + 3] == 0x75
        {
            return Some(pos);
        }
        pos += 1;
    }
    // No cluster yet — the whole buffer is init.
    Some(bytes.len())
}

/// Length of the fmp4 init portion (everything before the first moof box), or
/// None if the buffer doesn't carry fmp4 init (no ftyp/moov in head).
fn fmp4_init_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 8 {
        return None;
    }
    let is_mp4ish = &bytes[4..8] == b"ftyp" || &bytes[4..8] == b"moov" || &bytes[4..8] == b"styp";
    if !is_mp4ish {
        return None;
    }
    let mut pos = 0;
    while pos + 8 <= bytes.len() {
        if &bytes[pos + 4..pos + 8] == b"moof" {
            return Some(pos);
        }
        pos += 1;
    }
    Some(bytes.len())
}
