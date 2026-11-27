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
    /// Cluster/moof-aligned re-feed chunks computed at seek time. Appends are
    /// not cluster-aligned, so re-feeding per-append bytes hands the demuxer
    /// a mid-cluster guillotined stream it parses as garbage (sec1zt126 —
    /// "reading large block of size 98144024"). Chunks are cut exactly on
    /// cluster/moof boundaries instead.
    refeed_queue: script_bindings::cell::DomRefCell<std::collections::VecDeque<Vec<u8>>>,
    /// Wall-clock anchor for the post-seek refeed settle window — set in
    /// handle_seek. Turn-counting cannot measure time on an idle event loop.
    seek_instant: script_bindings::cell::DomRefCell<Option<std::time::Instant>>,
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
            refeed_queue: script_bindings::cell::DomRefCell::new(std::collections::VecDeque::new()),
            seek_instant: script_bindings::cell::DomRefCell::new(None),
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
        self.prepare_cluster_aligned_refeed(seek_target);
        *self.seek_instant.borrow_mut() = Some(std::time::Instant::now());
        self.refeed_retained(seek_target, 0, 0);
    }

    /// Concatenate retained fragments in time order, scan for cluster/moof
    /// boundaries, and cut the re-feed exactly on them. Concatenation order
    /// matters: players append in stream order, so time-sorted fragments
    /// reconstruct the original byte stream (gaps from forward seeks and
    /// eviction aside — the boundary scan degrades gracefully there because
    /// the first boundary after the seek target still demuxes).
    fn prepare_cluster_aligned_refeed(&self, seek_target: f64) {
        // NO time-sort: insertion order is append order is byte order.
        // Sorting by scanned timekeys scrambled contiguous fragments whenever
        // keys were missing or duplicated — the round-8 byte-stream scramble.
        let frags: Vec<(f64, f64, Vec<u8>)> = self.retained.borrow().clone();
        let mut stream = Vec::new();
        for (_, _, bytes) in &frags {
            stream.extend_from_slice(bytes);
        }
        if stream.is_empty() {
            return;
        }
        // Boundary scan: webm clusters first, fmp4 moofs as fallback — the two
        // container families youtube actually uses.
        let full = crate::dom::media::ebml::scan_cluster_full(&stream);
        let has_timecodes = full.iter().any(|(_, tc, _, _)| *tc > 0.0);
        let boundaries: Vec<(usize, f64)> = if !full.is_empty() {
            full.iter().map(|(off, tc, _, _)| (*off, *tc)).collect()
        } else {
            crate::dom::media::ebml::scan_moof_offsets(&stream)
                .into_iter()
                .map(|off| (off, 0.0))
                .collect()
        };
        if boundaries.is_empty() {
            return;
        }
        // The re-feed starts at the last boundary at-or-before the seek target:
        // the cluster containing the target began before it, and a demuxer
        // must see the whole cluster. For moofs (timecode 0) start at the first
        // boundary — moof ordering is chronological in practice.
        let start_idx = if boundaries.iter().any(|(_, tc)| *tc > 0.0) {
            let idx = boundaries
                .iter()
                .rposition(|(_, tc)| *tc <= seek_target && *tc > 0.0)
                .unwrap_or(0);
            idx
        } else {
            0
        };
        let start_off = boundaries[start_idx].0;
        // Timestamp re-basing (sec1zt128/129): the refed clusters carry their
        // ORIGINAL timecodes, but the post-reset pipeline restarts at zero —
        // the collision drops/clips frames in the overlap region (the Swiss-
        // cheese holes) and restarts pts at 0. Rewrite each refed cluster's
        // timecode in place: new = seek_target + (orig - first_refed_orig),
        // producing a contiguous stream that starts exactly at the target.
        // Rewrites are same-length big-endian into the existing timecode
        // element, so stream length and all offsets are preserved. A cluster
        // whose value doesn't fit its element size is left as-is (rare —
        // youtube uses 4+ byte timecodes).
        if has_timecodes {
            let mut rewritten = 0u32;
            let base_tc = full[start_idx].1;
            let target_ms = (seek_target * 1000.0).round() as u64;
            let first_ms = (base_tc * 1000.0).round() as u64;
            for (idx, (_, tc, coff, csize)) in full.iter().enumerate() {
                if idx < start_idx || *csize == 0 {
                    continue;
                }
                let orig_ms = (*tc * 1000.0).round() as u64;
                let delta = orig_ms.saturating_sub(first_ms);
                let new_val = target_ms.saturating_add(delta);
                let max_val = if *csize >= 8 {
                    u64::MAX
                } else {
                    (1u64 << (*csize as u32 * 8)) - 1
                };
                if new_val <= max_val && *coff + *csize <= stream.len() {
                    let bytes = new_val.to_be_bytes();
                    stream[*coff..*coff + *csize]
                        .copy_from_slice(&bytes[8 - *csize..]);
                    rewritten += 1;
                }
            }
        }
        if has_timecodes {
            let scanned: Vec<String> = full
                .iter()
                .map(|(off, tc, coff, csize)| format!("off={} tc={:.3} coff={} csize={}", off, tc, coff, csize))
                .collect();
            eprintln!("[mse-refeed] scan detail: {}", scanned.join(" | "));
        }
        // Diagnostic dump: the exact bytes about to be refed, post-rewrite.
        // The offline sim validates the ALGORITHM but corruption fires live —
        // the divergence must be in the live inputs. /tmp is fine: diagnostic
        // only, not user evidence. FFprobe this file and diff against the sim.
        if std::env::var("SERVO_MSE_REFEED_DUMP").is_ok() {
            let dump_path = format!("/tmp/mse-refeed-dump-{}.webm", seek_target);
            if !std::path::Path::new(&dump_path).exists() {
                let slice_start = boundaries[start_idx].0;
                let _ = std::fs::write(&dump_path, &stream[slice_start..]);
                eprintln!("[mse-refeed] dumped {} bytes to {}", stream.len() - slice_start, dump_path);
            }
        }
        // Cut chunk boundaries: each boundary offset at-or-after start, plus
        // end-of-stream. Group clusters into pushes of at most ~1.5MB so the
        // engine per-track quota isn't blown.
        let mut cuts: Vec<usize> = boundaries[start_idx..]
            .iter()
            .map(|(off, _)| *off)
            .collect();
        cuts.push(stream.len());
        const MAX_CHUNK: usize = 1536 * 1024;
        let mut chunks: std::collections::VecDeque<Vec<u8>> = std::collections::VecDeque::new();
        let mut cur: Vec<u8> = Vec::new();
        for w in cuts.windows(2) {
            let (a, b) = (w[0], w[1]);
            if b <= a || a < start_off {
                // the first window starts exactly at start_off by construction;
                // later ones are contiguous
            }
            if b > a {
                if !cur.is_empty() && cur.len() + (b - a) > MAX_CHUNK {
                    chunks.push_back(std::mem::take(&mut cur));
                }
                cur.extend_from_slice(&stream[a..b]);
            }
        }
        if !cur.is_empty() {
            chunks.push_back(cur);
        }
        // Init segment rides ahead of the first media chunk so the fresh
        // demuxer sees stream headers (post-reset demuxer state is blank) —
        // but ONLY if it isn't already there: retained fragment 0 holds the
        // original first append, which itself begins with the init bytes, and
        // prepending a second copy produces init+init+media — the exact
        // garbage the demuxer chokes on (sec1zt128: "large block of size
        // 1794238142" — a duplicated-ebml-header length misread).
        let init = self.init_cache.borrow().clone();
        if !init.is_empty() {
            let already = chunks
                .front()
                .map(|first| first.len() >= init.len() && first[..init.len()] == init[..])
                .unwrap_or(false);
            if !already {
                if let Some(first) = chunks.front_mut() {
                    let mut joined = init.clone();
                    joined.extend_from_slice(first);
                    *first = joined;
                }
            }
        }
        // Instrumentation (sec1zt129): the offline byte-level refeed validates
        // clean (ffprobe/ffprobe-decode on a simulated stream) yet the live
        // demuxer reports corruption — the divergence is in WHAT actually gets
        // delivered, so log exactly that.
        let first_hex: String = chunks
            .front()
            .map(|c| c[..c.len().min(48)].iter().map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(""))
            .unwrap_or_default();
        eprintln!(
            "[mse-refeed] prep target={} frags={} streamlen={} boundaries={} start_idx={} chunks={} first48={}",
            seek_target,
            frags.len(),
            stream.len(),
            boundaries.len(),
            start_idx,
            chunks.len(),
            first_hex
        );
        *self.refeed_queue.borrow_mut() = chunks;
    }

    /// Push init + retained fragments one at a time, in time order, each as a
    /// chained task. Per-fragment pushes matter: the engine enforces a per-
    /// track byte quota and a single mega-push of the whole retention would
    /// exceed it and permanently fail. Failures retry the same fragment —
    /// early attempts commonly land while the pipeline is mid-reset (not-yet-
    /// playing appsrcs reject) or momentarily full (backpressure drains as
    /// playback resumes). Each chain link yields the event loop.
    fn refeed_retained(&self, seek_target: f64, _fragment_index: usize, retries: u32) {
        if retries >= 400 {
            return;
        }
        // Reset-settle delay: the gstreamer pipeline reset cycles on a detached
        // thread, and pushes landing mid-reset are silently dropped by the
        // flushing appsrc with NO feedback to script — the push call reports
        // success because it only means "handed to the media thread" (verified
        // sec1zt124). Burn the first ~90 event-loop turns (~1.5s at 60fps)
        // before the first chunk so the state cycle has completed.
        // Wall-clock settle, NOT turn counting: media_element_task_source
        // tasks run back-to-back on an idle loop, so 90 turns can elapse in
        // milliseconds — the turn-count version of this never actually
        // waited, and the first refeed chunk landed in the mid-teardown
        // pipeline (round-5 ordering anomaly: push before ready/restore).
        if _fragment_index == 0 && !self.refeed_queue.borrow().is_empty() {
            let elapsed = self
                .seek_instant
                .borrow()
                .map(|t| t.elapsed().as_millis())
                .unwrap_or(u128::MAX);
            if elapsed < 1500 {
                let this = Trusted::new(self);
                self.global()
                    .task_manager()
                    .media_element_task_source()
                    .queue(task!(mse_refeed_settle: move |_cx| {
                        let this = this.root();
                        // retries stays 0 during settling: the re-poll chain
                        // runs back-to-back, so counting re-polls toward the
                        // 400-cap exhausts it in milliseconds — long before
                        // the wall-clock window — and the chain silently died
                        // with zero bytes delivered (round-6 regression).
                        this.refeed_retained(seek_target, 0, 0);
                    }));
                return;
            }
        }
        let chunk = self.refeed_queue.borrow_mut().pop_front();
        let Some(bytes) = chunk else {
            return;
        };
        let pushed = if let Some(ref ms) = *self.media_source.borrow() {
            ms.push_appended_data(self.track_id.get(), bytes.clone())
        } else {
            false
        };
        if !pushed {
            // push failure — put the chunk back and retry after a yield
            self.refeed_queue.borrow_mut().push_front(bytes);
        }
        if self.refeed_queue.borrow().is_empty() && pushed {
            return;
        }
        let next_retries = if pushed { 90 } else { retries + 1 };
        let this = Trusted::new(self);
        self.global()
            .task_manager()
            .media_element_task_source()
            .queue(task!(mse_refeed_retry: move |_cx| {
                let this = this.root();
                this.refeed_retained(seek_target, 1, next_retries);
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
        // CONTIGUITY INVARIANT: retain EVERY append in insertion order —
        // MSE appends for a track are a contiguous byte stream (the demuxer
        // originally consumed exactly this concatenation), so insertion order
        // IS the correct byte order. The old time-keyed capture silently
        // dropped every append whose slice contained no cluster head (most
        // of them — slices are smaller than clusters), and the time-sort
        // scrambled the rest: holes + duplicated regions glued mid-cluster,
        // the deterministic demux corruption (sec1zt135). Keys still serve
        // eviction; keyless fragments inherit the previous fragment's end.
        {
            let prev_end = self.retained.borrow().last().map(|(_, fe, _)| *fe).unwrap_or(0.0);
            let (fs, fe) = retained_range.unwrap_or((prev_end, prev_end));
            let mut retained = self.retained.borrow_mut();
            retained.push((fs, fe, bytes.clone()));
            let total: usize = retained.iter().map(|(_, _, b)| b.len()).sum();
            if total > MAX_RETAINED_BYTES {
                let mut excess = total - MAX_RETAINED_BYTES;
                // Eviction must NOT re-sort: front IS oldest in a contiguous
                // append-order stream. Re-sorting scrambles byte order.
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
