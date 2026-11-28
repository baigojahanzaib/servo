/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! servomse: a multi-track push source for Media Source Extensions playback.
//! Unlike ServoSrc (single linear byte stream), each MSE SourceBuffer is an
//! independent fragmented stream — YouTube feeds audio and video through
//! separate buffers — so this element hosts one appsrc per track and exposes
//! each as a sometimes src pad. Downstream decodebin3 demuxes and decodes
//! each track independently.

use std::collections::BTreeMap;
use std::sync::Mutex;

use glib::subclass::prelude::*;
use gstreamer::prelude::*;
use gstreamer::subclass::prelude::*;

const MAX_SRC_QUEUE_SIZE: u64 = 50 * 1024 * 1024; // 50 MB per track.

mod imp {
    use std::sync::LazyLock;

    use super::*;

    pub struct MseTrack {
        pub appsrc: gstreamer_app::AppSrc,
        pub eos: bool,
        /// The stream-id this track's appsrc started with — captured from the
        /// first stream-start event. Reused verbatim in post-seek resequencing
        /// so decodebin3 treats the re-fed stream as the SAME stream rather
        /// than an unselected new one (sec1zt74 stream-id theory).
        pub stream_id: std::sync::Arc<Mutex<Option<(String, Option<u32>)>>>,
        /// Set by seek_reset: the next push must re-sequence the stream in-band
        /// (stream-start/caps/segment) before the first post-seek buffer.
        pub needs_resequence: bool,
    }

    pub struct ServoMse {
        pub cat: gstreamer::DebugCategory,
        pub tracks: Mutex<BTreeMap<u64, MseTrack>>,
        pub signalled_no_more_pads: std::sync::atomic::AtomicBool,
        /// Pending resequence parameters: (seek target seconds, pipeline
        /// running time at the seek moment in nanoseconds). Consumed by the
        /// first post-seek push to build the injected segment.
        pub pending_resequence: Mutex<Option<(f64, u64)>>,
        /// Worker-channel for buffer pushes. Pushing events/buffers into a
        /// live pipeline synchronously from the script thread deadlocks
        /// against downstream streaming locks (proven on youtube: the script
        /// thread wedges inside the StreamStart push_event, taking the whole
        /// page down — eval starvation, zero further appends). All injection
        /// + buffer pushes happen on this dedicated worker instead; the
        /// script thread only enqueues.
        pub push_tx: Mutex<Option<std::sync::mpsc::Sender<PushJob>>>,
        /// Bytes enqueued but not yet pushed by the worker — counted into the
        /// quota check so backpressure still reaches appendBuffer.
        pub inflight_bytes: std::sync::atomic::AtomicU64,
        /// Bumped by seek_reset: jobs enqueued before the seek carry stale
        /// pre-seek bytes and must be dropped by the worker — otherwise the
        /// resequence injection fires and is immediately followed by stale
        /// pre-seek media, corrupting the re-fed stream.
        pub push_generation: std::sync::atomic::AtomicU64,
    }

    pub struct PushJob {
        pub track: u64,
        pub total: u64,
        pub data: Vec<u8>,
        pub generation: u64,
        /// Ordered end-of-stream marker: endOfStream must not overtake pushes
        /// already enqueued — the final chunks of the initial append sequence
        /// would otherwise look like post-EOS re-appends and trigger appsrc
        /// recreation mid-startup, killing initial playback entirely.
        pub eos_marker: bool,
    }

    impl ServoMse {
        /// Create (once) the appsrc + ghost src pad for a track. Pads are
        /// sometimes pads: they appear as SourceBuffers are added by the page,
        /// which decodebin3 handles as dynamic streams.
        pub fn ensure_track(&self, id: u64) {
            let mut tracks = self.tracks.lock().unwrap();
            if tracks.contains_key(&id) {
                return;
            }
            eprintln!("[mse] CREATE track={} inst={:x} mapkeys={:?}", id, self as *const _ as usize, tracks.keys().collect::<Vec<_>>());
            let appsrc = gstreamer::ElementFactory::make("appsrc")
                .build()
                .map(|elem| elem.downcast::<gstreamer_app::AppSrc>().unwrap())
                .expect("Could not create appsrc element");

            let obj = self.obj();
            obj.add(&appsrc).expect("Could not add appsrc to servomse bin");

            let pad_templ = obj
                .pad_template("src")
                .expect("servomse src pad template missing");
            let ghost = gstreamer::GhostPad::builder_from_template(&pad_templ)
                .name(format!("src_{id}"))
                .build();
            ghost.set_target(appsrc.static_pad("src").as_ref()).unwrap();
            ghost.set_active(true).unwrap();
            obj.add_pad(&ghost).expect("Could not add track pad");
            let stream_id_slot = std::sync::Arc::new(Mutex::new(None));
            {
                let idc = id;
                let slot = stream_id_slot.clone();
                ghost.add_probe(gstreamer::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                    if let Some(gstreamer::PadProbeData::Event(ref ev)) = info.data {
                        if ev.type_() == gstreamer::EventType::StreamStart {
                            if let Some(s) = ev.structure() {
                                eprintln!("[mse] STREAMSTART track={} struct={}", idc, s.to_string());
                                if let Ok(idstr) = s.get::<String>("stream-id") {
                                    let group = s.get::<u32>("group-id").ok();
                                    if slot.lock().unwrap().is_none() {
                                        *slot.lock().unwrap() = Some((idstr, group));
                                    }
                                }
                            }
                        }
                    }
                    gstreamer::PadProbeReturn::Ok
                });
                let n = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                ghost.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
                    if let Some(gstreamer::PadProbeData::Buffer(ref b)) = info.data {
                        let c = n.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if c % 20 == 0 || c < 3 {
                            eprintln!("[mse] FLOW track={} n={} bytes={} pts={:?}", idc, c, b.size(), b.pts());
                        }
                    }
                    gstreamer::PadProbeReturn::Ok
                });
            }

            appsrc.set_caps(None::<&gstreamer::Caps>);
            appsrc.set_max_bytes(MAX_SRC_QUEUE_SIZE);
            appsrc.set_block(false);
            appsrc.set_format(gstreamer::Format::Bytes);
            appsrc.set_stream_type(gstreamer_app::AppStreamType::Stream);

            // Sync state only after the pad chain exists end-to-end, so the
            // src pad activation can actually start the streaming task.
            let sr = appsrc.sync_state_with_parent();
            let (_, cur, _) = appsrc.state(gstreamer::ClockTime::ZERO);
            eprintln!("[mse] recreated appsrc sync={:?} state={:?}", sr, cur);
            {
                let a2 = appsrc.clone();
                std::thread::spawn(move || {
                    for w in [1u64, 2, 4] {
                        std::thread::sleep(std::time::Duration::from_secs(w));
                        let (_, st, _) = a2.state(gstreamer::ClockTime::ZERO);
                        let pad = a2.static_pad("src");
                        let (linked, peer) = match pad {
                            Some(ref p) => (p.is_linked(), p.peer().map(|x| format!("{}:{}", x.parent_element().map(|e| e.name().to_string()).unwrap_or_default(), x.name()))),
                            None => (false, None),
                        };
                        eprintln!("[mse] recreated appsrc +{}s level={} state={:?} linked={} peer={:?}", w, a2.current_level_bytes(), st, linked, peer);
                    }
                });
            }
            tracks.insert(id, MseTrack { appsrc, eos: false, needs_resequence: false, stream_id: stream_id_slot });
        }

        pub fn push_buffer(
            &self,
            track: u64,
            total_tracks: u64,
            data: Vec<u8>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            // Quota check up front (approximate: in-flight bytes are counted
            // globally rather than per-track — conservative backpressure, so
            // appendBuffer still throws QuotaExceeded instead of unbounded
            // queue growth when downstream stalls).
            {
                let tracks = self.tracks.lock().unwrap();
                if let Some(t) = tracks.get(&track) {
                    let inflight = self.inflight_bytes.load(std::sync::atomic::Ordering::SeqCst);
                    if t.appsrc.current_level_bytes() + inflight + data.len() as u64 >
                        MAX_SRC_QUEUE_SIZE
                    {
                        return Err(gstreamer::FlowError::Error);
                    }
                }
            }
            if std::env::var("SERVO_MSE_SYNC").is_ok() {
                return self.push_buffer_inner(track, total_tracks, data);
            }
            // Spawn the push worker lazily. It owns all event injection and
            // buffer pushing so the script thread never blocks inside a live
            // pipeline — the youtube deadlock (script thread wedged in the
            // StreamStart push_event, page fully unresponsive) is structurally
            // impossible once pushes are async.
            {
                let mut guard = self.push_tx.lock().unwrap();
                if guard.is_none() {
                    let (tx, rx) = std::sync::mpsc::channel::<PushJob>();
                    let weak = self.obj().downgrade();
                    std::thread::spawn(move || {
                        while let Ok(job) = rx.recv() {
                            let Some(obj) = weak.upgrade() else {
                                break;
                            };
                            let imp = obj.imp();
                            let len = job.data.len() as u64;
                            let cur_gen = imp.push_generation.load(std::sync::atomic::Ordering::SeqCst);
                            if job.eos_marker {
                                imp.end_of_stream_ordered();
                            } else if job.generation == cur_gen {
                                let _ = imp.push_buffer_inner(job.track, job.total, job.data);
                            } else {
                                eprintln!("[mse] drop stale pre-seek job track={} gen={} cur={}", job.track, job.generation, cur_gen);
                            }
                            imp.inflight_bytes
                                .fetch_sub(len, std::sync::atomic::Ordering::SeqCst);
                        }
                    });
                    *guard = Some(tx);
                }
                self.inflight_bytes
                    .fetch_add(data.len() as u64, std::sync::atomic::Ordering::SeqCst);
                let job = PushJob {
                    track,
                    total: total_tracks,
                    data,
                    generation: self.push_generation.load(std::sync::atomic::Ordering::SeqCst),
                    eos_marker: false,
                };
                if guard.as_ref().unwrap().send(job).is_err() {
                    return Err(gstreamer::FlowError::Error);
                }
            }
            return Ok(gstreamer::FlowSuccess::Ok);
        }

        fn push_buffer_inner(
            &self,
            track: u64,
            total_tracks: u64,
            data: Vec<u8>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            self.ensure_track(track);
            {
                let tracks = self.tracks.lock().unwrap();
                // uridecodebin waits for the source's no-more-pads before it
                // finalizes the stream collection and unblocks stream selection;
                // without it the pipeline deadlocks pre-preroll with zero errors.
                // Pads appearing after no-more-pads error out, so it must only
                // fire once every declared SourceBuffer has its pad — the page
                // tells us the count on every push.
                if tracks.len() as u64 >= total_tracks &&
                    !self.signalled_no_more_pads.swap(true, std::sync::atomic::Ordering::SeqCst)
                {
                    drop(tracks);
                    self.obj().no_more_pads();
                }
            }
            // Lock discipline: the tracks mutex must never be held across a
            // push_event/push_buffer — those can block on downstream streaming
            // locks, and any thread touching the tracks map meanwhile (the
            // script thread's quota pre-check, seek_reset) wedges with it.
            // The original deadlock survived the worker-thread redesign in
            // exactly this form: the worker blocked mid-injection holding the
            // lock, and appendBuffer's quota pre-check died on the same mutex.
            // gst objects are refcounted — clone the appsrc and snapshot the
            // resequence state under the lock, then do all pushing unlocked.
            let (appsrc, was_resequence, stream_id_pair) = {
                let mut tracks = self.tracks.lock().unwrap();
                // Post-endOfStream appends are legal per the MSE spec - players
                // rely on them for seeks/rebuffering - but gst appsrc EOS is
                // terminal: every push after end_of_stream fails Err(Eos)
                // forever. Measured as the youtube freeze signature: playback
                // stalls at the first post-EOS rebuffer point while the player
                // retries appends in an endless Err(Eos) storm. Recreate the
                // track's appsrc on the first post-EOS append; a fresh appsrc
                // re-sequences the stream naturally.
                if tracks.get(&track).is_some_and(|t| t.eos) {
                    eprintln!("[mse] post-eos append track={} - recreating appsrc", track);
                    self.reset_track_locked(&mut tracks, track);
                    if self.pending_resequence.lock().unwrap().is_some() {
                        if let Some(t) = tracks.get_mut(&track) {
                            t.needs_resequence = true;
                        }
                    }
                }
                let Some(t) = tracks.get_mut(&track) else {
                    return Err(gstreamer::FlowError::Error);
                };
                let was = t.needs_resequence;
                if was {
                    t.needs_resequence = false;
                }
                let pair = t.stream_id.lock().unwrap().clone();
                (t.appsrc.clone(), was, pair)
            };
            if was_resequence {
                eprintln!("[mse] inject-fire inst={:x} track={}", self as *const _ as usize, track);
                if let Some(srcpad) = appsrc.static_pad("src") {
                    let (stream_id, group_id) = stream_id_pair
                        .map(|(i, g)| (i, g))
                        .unwrap_or_else(|| (format!("servomse-{track}-reseq"), None));
                    // The resequenced stream-start must reproduce the original
                    // stream-id AND group-id: decodebin3 keys stream selection
                    // and collection grouping on both — a missing group-id or
                    // fresh id makes the re-fed stream an unselected stranger
                    // and zero frames reach the sinks (sec1zt82 remaining half).
                    let mut ev = gstreamer::event::StreamStart::builder(&stream_id).build();
                    if let Some(g) = group_id {
                        // The safe builder only generates fresh group ids, so
                        // patch the original back into the structure directly.
                        if let Some(evref) = ev.get_mut() {
                            evref.structure_mut().set("group-id", g);
                        }
                    }
                    eprintln!("[mse] inject streamstart push begin track={}", track);
                    let r1 = srcpad.push_event(ev);
                    eprintln!("[mse] inject streamstart push done track={} r={}", track, r1);
                    // No caps event: track caps are ANY (typefind re-detects
                    // on the new stream-start), and gst_event_new_caps rejects
                    // non-fixed caps with a null-event crash (event.rs:149).
                    let r2 = true;
                    // TIME-format segment: demuxers in push mode adopt a TIME
                    // input segment directly (a BYTES one is dropped as
                    // untranslatable — verified: base never propagates, frames
                    // dropped as too-late). start = seek target, base = pipeline
                    // running time at the seek moment, so media timestamps map
                    // into the live clock domain: pts(target) -> base + 0.
                    let mut segment =
                        gstreamer::FormattedSegment::<gstreamer::ClockTime>::new();
                    if let Some((target, _base_ns)) = *self.pending_resequence.lock().unwrap() {
                        segment.set_start(gstreamer::ClockTime::from_nseconds(
                            (target * 1_000_000_000.0) as u64,
                        ));
                        // Base MUST be the live running time at this injection
                        // moment — not the seek-moment value passed in, and
                        // never 0. Pipeline running time does NOT restart
                        // across the Ready cycle (gst re-bases for continuity
                        // on READY->PLAYING), so a 0/stale base maps refed
                        // frames into the past: preroll frame renders (sinks
                        // present it regardless of timestamps) then every
                        // subsequent buffer is dropped as too-late — the
                        // exact round-5 symptom. Computing base here, at the
                        // first post-reset push, aligns pts(target) with NOW:
                        // immediate display, subsequent frames in real time.
                        let live_base = (|| {
                            let clock = self.obj().clock()?;
                            let now = clock.time().nseconds();
                            let bt = self.obj().base_time().map(|b| b.nseconds()).unwrap_or(0);
                            Some(now.saturating_sub(bt))
                        })()
                        .unwrap_or(0);
                        eprintln!("[mse] resequence base live={} passed={}", live_base, _base_ns);
                        segment.set_base(gstreamer::ClockTime::from_nseconds(live_base));
                    }
                    eprintln!("[mse] inject segment push begin track={}", track);
                    let r3 = srcpad.push_event(gstreamer::event::Segment::new(&segment));
                    eprintln!("[mse] inject segment push done track={} r={}", track, r3);
                    eprintln!(
                        "[mse] resequence inject track={} streamstart={} caps={} segment={}",
                        track, r1, r2, r3
                    );
                }
            }
            let size = data.len() as u64;
            // Quota enforcement: appsrc accepts pushed buffers unconditionally
            // (max-bytes only drives the need/enough-data signals), so without
            // this check every byte a page ever appends piles in the queue —
            // measured 144MB -> 336MB of SourceBuffer-attributed memory in
            // under a minute of MSE churn (sec1zq). Rejecting the push when the
            // track is full back-pressures through the chain to appendBuffer,
            // which throws QuotaExceededError per the MSE spec; real players
            // respond by evicting buffered ranges. Playing videos drain their
            // queues constantly and never hit this; paused/never-played videos
            // (infinite-feed preloads) hit it exactly as intended.
            if appsrc.current_level_bytes() + size > MAX_SRC_QUEUE_SIZE {
                return Err(gstreamer::FlowError::Error);
            }
            let buffer = gstreamer::Buffer::from_slice(data);
            let r = appsrc.push_buffer(buffer);
            eprintln!("[mse] push track={} size={} result={:?} level={}", track, size, r, appsrc.current_level_bytes());
            r
        }

        /// EOS is per-stream in gstreamer but MSE endOfStream ends the whole
        /// presentation: flush every track.
        pub fn end_of_stream(&self) {
            self.ensure_track(0);
            // Serialized through the worker queue so EOS cannot overtake
            // in-flight appends (see PushJob::eos_marker).
            let mut guard = self.push_tx.lock().unwrap();
            if guard.is_none() {
                drop(guard);
                self.end_of_stream_ordered();
                return;
            }
            let job = PushJob {
                track: 0,
                total: 0,
                data: Vec::new(),
                generation: self.push_generation.load(std::sync::atomic::Ordering::SeqCst),
                eos_marker: true,
            };
            let _ = guard.as_ref().unwrap().send(job);
        }

        /// The ordered end-of-stream body, run on the worker thread after
        /// every previously-enqueued push has landed.
        fn end_of_stream_ordered(&self) {
            // Same lock discipline as push_buffer_inner: EOS pushes can block
            // downstream — clone the appsrcs under the lock, push unlocked.
            let appsrcs: Vec<gstreamer_app::AppSrc> = {
                let mut tracks = self.tracks.lock().unwrap();
                for t in tracks.values_mut() {
                    t.eos = true;
                }
                tracks.values().map(|t| t.appsrc.clone()).collect()
            };
            for a in appsrcs {
                let _ = a.end_of_stream();
            }
        }

        /// MSE seek support: flush downstream demuxers/decoders, then recreate
        /// every track's appsrc. A fresh appsrc re-emits stream-start/caps/segment
        /// naturally in-stream on the first post-seek push — pushing those events
        /// manually from the application thread wedges the pipeline (FEATHER_MEMORY
        /// §1q), this design avoids injection entirely. Validated in the isolated
        /// gst harness: playback resumes cleanly with continuous position.
        pub fn seek_reset(&self, target_secs: f64, base_ns: u64) {
            eprintln!("[mse] seek_reset enter target={} base_ns={}", target_secs, base_ns);
            self.push_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *self.pending_resequence.lock().unwrap() = Some((target_secs, base_ns));
            // Hold the tracks lock for the whole reset: pushes arriving
            // mid-reset would otherwise land in a dying appsrc (or a removed
            // track) and be silently lost, permanently starving the demuxer —
            // the post-seek resume-stall race. Blocking pushes until the new
            // appsrcs are in place makes the handover lossless.
            let mut tracks = self.tracks.lock().unwrap();
            // No-flush default (sec-media): bare flush events injected mid-stream
            // leave downstream in unrecoverable flushing state — appsrc streaming
            // tasks stall permanently and post-seek data piles up unconsumed
            // (validated: with flush, position freezes exactly at the seek target;
            // without, playback advances through repeated seeks end-to-end).
            // The re-fed data is cluster/keyframe-aligned by the script-side
            // indexer so the demuxer resyncs without a flush. Residual artifact:
            // already-decoded pre-seek frames in downstream queues play out
            // briefly before the jump — strictly better than stream death.
            // SERVO_MSE_FLUSH re-enables flushing for A/B verification.
            if std::env::var("SERVO_MSE_FLUSH").is_ok() {
                for pad in self.obj().src_pads() {
                    let _ = pad.push_event(gstreamer::event::FlushStart::new());
                    let _ = pad.push_event(gstreamer::event::FlushStop::new(true));
                }
            }
            // In-band re-sequencing design (sec1zt75): do NOT recreate the
            // appsrcs — recreation reliably dies in a live playbin3 pipeline
            // (first post-seek push returns not-linked within milliseconds).
            // Instead mark every track for re-sequencing: the next push_buffer
            // injects stream-start/caps/segment on the appsrc src pad at a
            // moment when the streaming task is guaranteed idle (no post-seek
            // data has been pushed yet), which is the one injection timing
            // that cannot race the stream — the §1q wedge does not apply.
            if std::env::var("SERVO_MSE_RECREATE").is_ok() {
                let ids: Vec<u64> = tracks.keys().copied().collect();
                for id in ids {
                    self.reset_track_locked(&mut tracks, id);
                }
            } else {
                let nmarked = tracks.len();
                for t in tracks.values_mut() {
                    t.needs_resequence = true;
                }
                eprintln!("[mse] seek_reset: tracks marked for in-band resequence inst={:x} nmarked={}", self as *const _ as usize, nmarked);
            }
        }

        /// Recreate one track's appsrc, re-targeting the existing ghost pad so
        /// downstream pad identity (and decodebin3's stream collection) is stable.
        fn reset_track_locked(&self, tracks: &mut BTreeMap<u64, MseTrack>, id: u64) {
            let Some(old) = tracks.remove(&id) else {
                return;
            };
            let obj = self.obj();
            let ghost_name = format!("src_{id}");
            let Some(pad) = obj.static_pad(&ghost_name) else {
                return;
            };
            let Ok(ghost) = pad.downcast::<gstreamer::GhostPad>() else {
                return;
            };
            let _ = ghost.set_target(None::<&gstreamer::Pad>);
            let _ = old.appsrc.set_state(gstreamer::State::Null);
            let _ = obj.remove(&old.appsrc);
            drop(old);

            let appsrc = gstreamer::ElementFactory::make("appsrc")
                .build()
                .map(|elem| elem.downcast::<gstreamer_app::AppSrc>().unwrap())
                .expect("Could not create appsrc element");
            // Link BEFORE adding to the bin: gst_bin_add immediately starts the
            // state transition in a running pipeline, and the appsrc streaming
            // task dies with not-linked if it starts before the pad chain exists.
            ghost.set_target(appsrc.static_pad("src").as_ref()).unwrap();
            // The messy teardown of the old appsrc (removed while PLAYING) can
            // leave the ghost's proxy chain deactivated — downstream pushes then
            // return not-linked even though the peer exists. Force reactivation.
            let _ = ghost.set_active(false);
            let _ = ghost.set_active(true);
            obj.add(&appsrc).expect("Could not add appsrc to servomse bin");
            eprintln!("[mse] reset_track {} recreated, ghost peer: {:?}", id, ghost.peer().map(|p| format!("{}:{}", p.parent_element().map(|e| e.name().to_string()).unwrap_or_default(), p.name())));
            let idc = id;
            ghost.add_probe(gstreamer::PadProbeType::BUFFER, move |_, info| {
                if let Some(gstreamer::PadProbeData::Buffer(ref b)) = info.data {
                    eprintln!("[mse] OUT track={} bytes={} pts={:?}", idc, b.size(), b.pts());
                }
                gstreamer::PadProbeReturn::Ok
            });
            appsrc.set_caps(None::<&gstreamer::Caps>);
            appsrc.set_max_bytes(MAX_SRC_QUEUE_SIZE);
            appsrc.set_block(false);
            appsrc.set_format(gstreamer::Format::Bytes);
            appsrc.set_stream_type(gstreamer_app::AppStreamType::Stream);
            let _ = appsrc.sync_state_with_parent();
            tracks.insert(id, MseTrack { appsrc, eos: false, needs_resequence: false, stream_id: std::sync::Arc::new(Mutex::new(None)) });
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ServoMse {
        const NAME: &'static str = "ServoMse";
        type Type = super::ServoMse;
        type ParentType = gstreamer::Bin;
        type Interfaces = (gstreamer::URIHandler,);

        fn with_class(_klass: &Self::Class) -> Self {
            Self {
                cat: gstreamer::DebugCategory::new(
                    "servomse",
                    gstreamer::DebugColorFlags::empty(),
                    Some("Servo MSE source"),
                ),
                tracks: Mutex::new(BTreeMap::new()),
                signalled_no_more_pads: std::sync::atomic::AtomicBool::new(false),
                pending_resequence: Mutex::new(None),
                push_tx: Mutex::new(None),
                inflight_bytes: std::sync::atomic::AtomicU64::new(0),
                push_generation: std::sync::atomic::AtomicU64::new(0),
            }
        }
    }

    impl ObjectImpl for ServoMse {
        fn constructed(&self) {
            self.parent_constructed();
            self.obj().set_element_flags(gstreamer::ElementFlags::SOURCE);
        }
    }

    impl GstObjectImpl for ServoMse {}

    impl ElementImpl for ServoMse {
        fn metadata() -> Option<&'static gstreamer::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gstreamer::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gstreamer::subclass::ElementMetadata::new(
                        "Servo MSE Source",
                        "Source/Audio/Video",
                        "Feed player with MSE-tracked media data",
                        "Servo developers",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gstreamer::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gstreamer::PadTemplate>> = LazyLock::new(|| {
                let caps = gstreamer::Caps::new_any();
                let src_pad_template = gstreamer::PadTemplate::new(
                    "src",
                    gstreamer::PadDirection::Src,
                    gstreamer::PadPresence::Sometimes,
                    &caps,
                )
                .unwrap();

                vec![src_pad_template]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    impl BinImpl for ServoMse {}

    impl URIHandlerImpl for ServoMse {
        const URI_TYPE: gstreamer::URIType = gstreamer::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["servomse"]
        }

        fn uri(&self) -> Option<String> {
            Some("servomse://".to_string())
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            if uri.starts_with("servomse:") {
                return Ok(());
            }
            Err(glib::Error::new(
                gstreamer::URIError::BadUri,
                format!("Invalid URI '{uri:?}'").as_str(),
            ))
        }
    }
}

glib::wrapper! {
    pub struct ServoMse(ObjectSubclass<imp::ServoMse>)
        @extends gstreamer::Bin, gstreamer::Element, gstreamer::Object, @implements gstreamer::URIHandler;
}

unsafe impl Send for ServoMse {}
unsafe impl Sync for ServoMse {}

impl ServoMse {
    pub fn ensure_track(&self, track: u64) {
        self.imp().ensure_track(track)
    }

    pub fn push_buffer(
        &self,
        track: u64,
        total_tracks: u64,
        data: Vec<u8>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().push_buffer(track, total_tracks, data)
    }

    /// Seek support: flush + recreate all track appsrcs so the next pushes
    /// re-sequence the stream naturally. See imp::ServoMse::seek_reset.
    pub fn seek_reset(&self, target_secs: f64, base_ns: u64) {
        self.imp().seek_reset(target_secs, base_ns)
    }

    pub fn end_of_stream_all(&self) {
        self.imp().end_of_stream()
    }
}

pub fn register_servo_mse() -> Result<(), glib::BoolError> {
    gstreamer::Element::register(
        None,
        "servomse",
        gstreamer::Rank::NONE,
        ServoMse::static_type(),
    )
}
