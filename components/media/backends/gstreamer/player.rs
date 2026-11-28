/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, Once};
use std::time;

use byte_slice_cast::AsSliceOf;
use glib;
use glib::prelude::*;
use gstreamer;
use gstreamer_app;
use gstreamer_play;
use gstreamer_play::prelude::*;
use malloc_size_of::MallocSizeOf;
use malloc_size_of_derive::MallocSizeOf;
use servo_base::generic_channel::{self, GenericCallback, GenericReceiver};
use servo_media::MediaInstanceError;
use servo_media_player::audio::AudioRenderer;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::metadata::Metadata;
use servo_media_player::video::VideoFrameRenderer;
use servo_media_player::{
    PlaybackState, Player, PlayerError, PlayerEvent, SeekLock, SeekLockMsg, StreamType,
};
use servo_media_streams::registry::{MediaStreamId, get_stream};
use servo_media_traits::{BackendMsg, ClientContextId, MediaInstance};

use super::BACKEND_BASE_TIME;
use crate::media_stream::GStreamerMediaStream;
use crate::media_stream_source::{ServoMediaStreamSrc, register_servo_media_stream_src};
use crate::mse_source::{ServoMse, register_servo_mse};
use crate::render::GStreamerRender;
use crate::source::{ServoSrc, register_servo_src};

const DEFAULT_MUTED: bool = false;
const DEFAULT_PAUSED: bool = true;
const DEFAULT_CAN_RESUME: bool = false;
const DEFAULT_PLAYBACK_RATE: f64 = 1.0;
const DEFAULT_VOLUME: f64 = 1.0;
const DEFAULT_TIME_RANGES: Vec<Range<f64>> = vec![];

const MAX_BUFFER_SIZE: i32 = 500 * 1024 * 1024;

/// Run a gstreamer signal-callback body catching any panic before it can
/// unwind into the calling C thread. Gstreamer invokes these callbacks from
/// its own internal (C) threads; an unwind crossing those foreign frames is
/// undefined behaviour, and an uncaught panic kills the thread silently,
/// leaving the media element in a permanently degraded state. A caught panic
/// is logged by the panic hook (like every other thread) and the callback
/// simply does not complete its update — the player degrades gracefully
/// instead of dying.
fn run_callback_guarded<F, T>(body: F) -> Option<T>
where
    F: FnOnce() -> T,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(body)).ok()
}

fn metadata_from_media_info(media_info: &gstreamer_play::PlayMediaInfo) -> Result<Metadata, ()> {
    let dur = media_info.duration();
    let duration = if let Some(dur) = dur {
        let mut nanos = dur.nseconds();
        nanos %= 1_000_000_000;
        let seconds = dur.seconds();
        Some(time::Duration::new(seconds, nanos as u32))
    } else {
        None
    };

    let mut audio_tracks = Vec::new();
    let mut video_tracks = Vec::new();

    let format = media_info
        .container_format()
        .unwrap_or_else(|| glib::GString::from(""))
        .to_string();

    for stream_info in media_info.stream_list() {
        let stream_type = stream_info.stream_type();
        match stream_type.as_str() {
            "audio" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                audio_tracks.push(codec);
            },
            "video" => {
                let codec = stream_info
                    .codec()
                    .unwrap_or_else(|| glib::GString::from(""))
                    .to_string();
                video_tracks.push(codec);
            },
            _ => {},
        }
    }

    let mut width: u32 = 0;
    let height: u32 = if media_info.number_of_video_streams() > 0 {
        let first_video_stream = &media_info.video_streams()[0];
        width = first_video_stream.width() as u32;
        first_video_stream.height() as u32
    } else {
        0
    };

    let is_seekable = media_info.is_seekable();
    let is_live = media_info.is_live();
    let title = media_info.title().map(|s| s.as_str().to_string());

    Ok(Metadata {
        duration,
        width,
        height,
        format,
        is_seekable,
        audio_tracks,
        video_tracks,
        is_live,
        title,
    })
}

pub struct GStreamerAudioChunk(gstreamer::buffer::MappedBuffer<gstreamer::buffer::Readable>);
impl AsRef<[f32]> for GStreamerAudioChunk {
    fn as_ref(&self) -> &[f32] {
        self.0.as_ref().as_slice_of::<f32>().unwrap_or_default()
    }
}

#[derive(PartialEq)]
enum PlayerSource {
    Seekable(ServoSrc),
    Stream(ServoMediaStreamSrc),
    Mse(ServoMse),
}

impl MallocSizeOf for PlayerSource {
    fn size_of(&self, _ops: &mut malloc_size_of::MallocSizeOfOps) -> usize {
        // Currently both are external types
        0
    }
}

#[derive(MallocSizeOf)]
struct PlayerInner {
    #[ignore_malloc_size_of = "External Type"]
    player: gstreamer_play::Play,
    #[ignore_malloc_size_of = "External Type"]
    signal_adapter: gstreamer_play::PlaySignalAdapter,
    source: Option<PlayerSource>,
    #[ignore_malloc_size_of = "External Type"]
    video_sink: gstreamer_app::AppSink,
    input_size: u64,
    seekable: bool,
    #[ignore_malloc_size_of = "External Type"]
    play_state: gstreamer_play::PlayState,
    paused: Cell<bool>,
    can_resume: Cell<bool>,
    playback_rate: Cell<f64>,
    muted: Cell<bool>,
    volume: Cell<f64>,
    stream_type: StreamType,
    last_metadata: Option<Metadata>,
    #[ignore_malloc_size_of = "External Type"]
    cat: gstreamer::DebugCategory,
    #[conditional_malloc_size_of]
    enough_data: Arc<AtomicBool>,
    /// To allow teardown to free seek_data that is waiting for the script thread.
    seek_lock: Option<SeekLock>,
    /// MSE position reporting correction: after a seek the flush+recreate
    /// re-bases the pipeline at the first post-seek buffer, so playbin position
    /// reports time-since-seek instead of presentation time. The seek target is
    /// the offset that must be added back. Zero before the first seek.
    mse_position_offset: Cell<f64>,
    /// Serializes MSE seek reset+state-cycle workers: overlapping seeks must
    /// not interleave pipeline state changes and appsrc recreation, or the
    /// pipeline wedges mid-transition (clock never restarts — sec1zt220).
    #[ignore_malloc_size_of = "External Type"]
    mse_seek_serial: Arc<Mutex<()>>,
    /// Generation counter for MSE seeks: a worker that finds itself superseded
    /// while waiting on the serial lock abandons its cycle — the newer seek's
    /// worker performs the only reset+cycle that matters.
    #[ignore_malloc_size_of = "External Type"]
    mse_seek_gen: Arc<AtomicU64>,
}

impl PlayerInner {
    pub fn set_input_size(&mut self, size: u64) -> Result<(), PlayerError> {
        // Set input_size to proxy its value, since it
        // could be set by the user before calling .setup().
        self.input_size = size;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_size(if size > 0 {
                size as i64
            } else {
                -1 // live source
            });
        }
        Ok(())
    }

    pub fn set_seekable(&mut self, seekable: bool) -> Result<(), PlayerError> {
        self.seekable = seekable;
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            source.set_seekable(seekable);
        }
        Ok(())
    }

    pub fn set_mute(&mut self, muted: bool) -> Result<(), PlayerError> {
        if self.muted.get() == muted {
            return Ok(());
        }

        self.muted.set(muted);
        self.player.set_mute(muted);
        Ok(())
    }

    pub fn muted(&self) -> bool {
        self.muted.get()
    }

    pub fn set_playback_rate(&mut self, playback_rate: f64) -> Result<(), PlayerError> {
        if self.stream_type != StreamType::Seekable {
            return Err(PlayerError::NonSeekableStream);
        }

        if self.playback_rate.get() == playback_rate {
            return Ok(());
        }

        self.playback_rate.set(playback_rate);

        // The new playback rate will not be passed to the pipeline if the
        // current GstPlay state is less than GST_STATE_PAUSED, which will be
        // set immediately before the initial gstreamer_play_MESSAGE_MEDIA_INFO_UPDATED
        // message is posted to bus.
        if self.last_metadata.is_some() {
            self.player.set_rate(playback_rate);
        }
        Ok(())
    }

    pub fn playback_rate(&self) -> f64 {
        self.playback_rate.get()
    }

    pub fn play(&mut self) -> Result<(), PlayerError> {
        // Always forward to the pipeline: after natural end-of-stream the
        // pipeline stops consuming even though this object's paused flag was
        // never set, so the early-return here used to make replay impossible.
        self.paused.set(false);
        self.can_resume.set(false);
        self.player.play();
        Ok(())
    }

    pub fn stop(&mut self) -> Result<(), PlayerError> {
        self.player.stop();
        self.paused.set(true);
        self.can_resume.set(false);
        self.last_metadata = None;
        self.source = None;
        // Teardown nudges for the navigate-away retention class (sec1zt86):
        // an external glib ref on the Play object never drains otherwise,
        // pinning ~140MB per video permanently. NULL the pipeline and flush
        // the bus so in-flight/queued bus messages and their object refs are
        // released. Safety is unaffected: disposal only happens when the
        // reaper verifies the refcount actually drained.
        let pipeline = self.player.pipeline();
        let _ = pipeline.set_state(gstreamer::State::Null);
        if let Some(bus) = pipeline.bus() {
            bus.set_flushing(true);
        }
        Ok(())
    }

    pub fn pause(&mut self) -> Result<(), PlayerError> {
        if self.paused.get() {
            return Ok(());
        }

        self.paused.set(true);
        self.can_resume.set(true);
        self.player.pause();
        Ok(())
    }

    pub fn paused(&self) -> bool {
        self.paused.get()
    }

    pub fn can_resume(&self) -> bool {
        self.can_resume.get()
    }

    pub fn end_of_stream(&mut self) -> Result<(), PlayerError> {
        match self.source {
            Some(ref mut source) => match source {
                PlayerSource::Seekable(source) => source
                    .push_end_of_stream()
                    .map(|_| ())
                    .map_err(|_| PlayerError::EOSFailed),
                PlayerSource::Mse(source) => {
                    source.end_of_stream_all();
                    Ok(())
                },
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }

    pub fn seek(&mut self, time: f64) -> Result<(), PlayerError> {
        if self.stream_type == StreamType::Stream {
            return Err(PlayerError::NonSeekableStream);
        }
        if self.stream_type == StreamType::MediaSource {
            // Push-mode MSE pipelines cannot execute gst seeks (no byte-serving
            // upstream). Instead: flush downstream and recreate the track appsrcs
            // so the page's post-seek appends re-sequence the stream in-band —
            // the harness-validated design in FEATHER_MEMORY §1q/§1t.
            if let Some(PlayerSource::Mse(ref source)) = self.source {
                // Pipeline-reset design: decodebin3's stream reselection kills
                // re-fed streams in a live pipeline (sec1zt84 — zero frames at
                // sink, injection iteration exhausted). Cycling the playbin to
                // Ready and back makes ALL downstream state fresh — decodebin3
                // re-plumbs the resequenced stream exactly like initial
                // playback, the one path proven to work.
                //
                // The reset AND the state cycle run as one serialized unit on a
                // worker thread (sec1zt220): overlapping seeks — which real
                // sites fire routinely (resume-at-position, quality switches,
                // ad transitions) — must never interleave appsrc recreation
                // with another seek's in-flight state changes, or the pipeline
                // wedges mid-transition and the clock never restarts. The
                // worker must not run on the caller thread either:
                // set_state(Ready->Playing) can block indefinitely joining
                // streaming threads, and the caller is the script thread.
                // A worker that finds a newer seek queued while it waited for
                // the serial lock abandons its cycle: only the newest target
                // matters, and its worker performs the reset+cycle.
                let source = source.clone();
                let pipeline = self.player.pipeline();
                let restore = if self.paused.get() {
                    gstreamer::State::Paused
                } else {
                    gstreamer::State::Playing
                };
                let serial = self.mse_seek_serial.clone();
                let gen_counter = self.mse_seek_gen.clone();
                let my_gen = gen_counter.fetch_add(1, Ordering::SeqCst) + 1;
                eprintln!("[mse] seek target={} gen={}", time, my_gen);
                if std::env::var("SERVO_MSE_NORESET").is_err() {
                    std::thread::spawn(move || {
                        let _guard = serial.lock().unwrap();
                        if gen_counter.load(Ordering::SeqCst) != my_gen {
                            eprintln!("[mse] seek gen={} superseded, skipping cycle", my_gen);
                            return;
                        }
                        // Wait for any in-flight state transition — critically
                        // the initial startup preroll — to settle before
                        // resetting: recreating appsrcs mid-transition breaks
                        // decodebin3's plumbing and the restore never completes
                        // (boot freeze: sites that resume-at-position seek
                        // within milliseconds of pipeline creation, wedging
                        // playback at the seek target — sec1zt222). Bounded:
                        // a slow preroll must not deadlock the seek path.
                        let deadline = std::time::Instant::now() +
                            std::time::Duration::from_secs(5);
                        loop {
                            let (_, _current, pending) =
                                pipeline.state(gstreamer::ClockTime::ZERO);
                            if pending == gstreamer::State::VoidPending {
                                break;
                            }
                            if std::time::Instant::now() > deadline {
                                eprintln!("[mse] seek gen={} state settle timeout", my_gen);
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        source.seek_reset(time, 0);
                        // No-cycle experiment (sec-media): the Ready->restore cycle makes
                        // uridecodebin RE-INSTANTIATE the ServoMse source element — the
                        // resequence flags marked pre-cycle die with the detached instance,
                        // and the virgin post-cycle instance never triggers downstream
                        // stream reselection (proven: per-seek instance addresses differ in
                        // production logs, injection never fires). The hardened in-band
                        // resequence (original stream-id/group-id, TIME segment, live base)
                        // was never tested WITHOUT the cycle — the two landed together.
                        // Skipping the cycle keeps the source instance alive so injection
                        // can actually fire in the live pipeline. SERVO_MSE_CYCLE re-enables
                        // the old behavior for A/B verification.
                        if std::env::var("SERVO_MSE_CYCLE").is_ok() {
                            let r = pipeline.set_state(gstreamer::State::Ready);
                            eprintln!("[mse] seek pipeline->ready {:?}", r);
                            let r = pipeline.set_state(restore);
                            eprintln!("[mse] seek pipeline->restore {:?}", r);
                        } else {
                            eprintln!("[mse] seek no-cycle: resequencing in live pipeline");
                        }
                    });
                }
            }
            self.mse_position_offset.set(time);
            return Ok(());
        }
        if let Some(ref metadata) = self.last_metadata
            && let Some(ref duration) = metadata.duration
            && duration < &time::Duration::new(time as u64, 0)
        {
            gstreamer::warning!(self.cat, obj = &self.player, "Trying to seek out of range");
            return Err(PlayerError::SeekOutOfRange);
        }

        let time = time * 1_000_000_000.;
        self.player
            .seek(gstreamer::ClockTime::from_nseconds(time as u64));
        Ok(())
    }

    pub fn set_volume(&mut self, volume: f64) -> Result<(), PlayerError> {
        if self.volume.get() == volume {
            return Ok(());
        }

        self.volume.set(volume);
        self.player.set_volume(volume);
        Ok(())
    }

    pub fn volume(&self) -> f64 {
        self.volume.get()
    }

    pub fn push_data(&mut self, data: Vec<u8>) -> Result<(), PlayerError> {
        if let Some(PlayerSource::Seekable(ref mut source)) = self.source {
            if self.enough_data.load(Ordering::Relaxed) {
                return Err(PlayerError::EnoughData);
            }
            return source
                .push_buffer(data)
                .map(|_| ())
                .map_err(|_| PlayerError::BufferPushFailed);
        }
        Err(PlayerError::BufferPushFailed)
    }

    pub fn push_data_track(
        &mut self,
        track: u64,
        total_tracks: u64,
        data: Vec<u8>,
    ) -> Result<(), PlayerError> {
        if let Some(PlayerSource::Mse(ref mut source)) = self.source {
            return source
                .push_buffer(track, total_tracks, data)
                .map(|_| ())
                .map_err(|_| PlayerError::BufferPushFailed);
        }
        self.push_data(data)
    }

    pub fn set_src(&mut self, source: PlayerSource) {
        self.source = Some(source);
    }

    pub fn buffered(&self) -> Vec<Range<f64>> {
        let mut buffered_ranges = vec![];

        let Some(duration) = self
            .last_metadata
            .as_ref()
            .and_then(|metadata| metadata.duration)
        else {
            return buffered_ranges;
        };

        let pipeline = self.player.pipeline();
        let mut buffering = gstreamer::query::Buffering::new(gstreamer::Format::Percent);
        if pipeline.query(&mut buffering) {
            let ranges = buffering.ranges();
            for (start, end) in ranges {
                let start = (if let gstreamer::GenericFormattedValue::Percent(start) = start {
                    start.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64
                    * duration.as_secs_f64();
                let end = (if let gstreamer::GenericFormattedValue::Percent(end) = end {
                    end.unwrap()
                } else {
                    gstreamer::format::Percent::from_percent(0)
                } / gstreamer::format::Percent::MAX) as f64
                    * duration.as_secs_f64();
                buffered_ranges.push(Range { start, end });
            }
        }

        buffered_ranges
    }

    pub fn seekable(&self) -> Vec<Range<f64>> {
        // if the servosrc is seekable, we should return the duration of the media
        if let Some(metadata) = self.last_metadata.as_ref()
            && metadata.is_seekable
            && let Some(duration) = metadata.duration
        {
            return vec![Range {
                start: 0.0,
                end: duration.as_secs_f64(),
            }];
        }

        // if the servosrc is not seekable, we should return the buffered range
        self.buffered()
    }

    fn set_stream(&mut self, stream: &MediaStreamId, only_stream: bool) -> Result<(), PlayerError> {
        debug_assert!(self.stream_type == StreamType::Stream);
        let Some(PlayerSource::Stream(ref source)) = self.source else {
            return Err(PlayerError::SetStreamFailed);
        };

        let stream = get_stream(stream).expect("Media streams registry does not contain such ID");
        let mut stream = stream.lock().unwrap();
        let Some(stream) = stream.as_mut_any().downcast_mut::<GStreamerMediaStream>() else {
            return Err(PlayerError::SetStreamFailed);
        };

        let playbin = self
            .player
            .pipeline()
            .dynamic_cast::<gstreamer::Pipeline>()
            .unwrap();
        let clock = gstreamer::SystemClock::obtain();
        playbin.set_base_time(*BACKEND_BASE_TIME);
        playbin.set_start_time(gstreamer::ClockTime::NONE);
        playbin.use_clock(Some(&clock));
        source
            .set_stream(stream, only_stream)
            .map_err(|_| PlayerError::SetStreamFailed)
    }

    fn set_audio_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_audio_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_audio_track_enabled(enabled);
        Ok(())
    }

    fn set_video_track(&mut self, stream_index: i32, enabled: bool) -> Result<(), PlayerError> {
        self.player
            .set_video_track(stream_index)
            .map_err(|_| PlayerError::SetTrackFailed)?;
        self.player.set_video_track_enabled(enabled);
        Ok(())
    }
}

macro_rules! notify(
    ($observer:expr_2021, $event:expr_2021) => {
        $observer.lock().unwrap().send($event)
    };
);

struct SeekChannel {
    sender: SeekLock,
    recv: GenericReceiver<SeekLockMsg>,
}

impl SeekChannel {
    fn new() -> Self {
        let (sender, recv) =
            generic_channel::channel::<SeekLockMsg>().expect("Couldn't create IPC channel");
        Self {
            sender: SeekLock {
                lock_channel: sender,
            },
            recv,
        }
    }

    fn sender(&self) -> SeekLock {
        self.sender.clone()
    }

    fn _await(&self) -> Option<SeekLockMsg> {
        // Bounded wait: the streaming thread must never wedge forever waiting
        // for the script side to service the seek (unbounded-IPC wedge class).
        // On timeout the seek is reported as failed and playback continues
        // with whatever data the pipeline already holds.
        self.recv
            .try_recv_timeout(std::time::Duration::from_secs(5))
            .ok()
    }
}

#[derive(MallocSizeOf)]
pub struct GStreamerPlayer {
    /// The player unique ID.
    id: usize,
    /// The ID of the client context this player belongs to.
    context_id: ClientContextId,
    /// Channel to communicate with the owner GStreamerBackend instance.
    #[conditional_malloc_size_of]
    backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
    #[conditional_malloc_size_of]
    inner: RefCell<Option<Arc<Mutex<PlayerInner>>>>,
    #[conditional_malloc_size_of]
    observer: Arc<Mutex<GenericCallback<PlayerEvent>>>,
    #[ignore_malloc_size_of = "External Type"]
    audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
    #[ignore_malloc_size_of = "External Type"]
    video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
    /// Indicates whether the setup was succesfully performed and
    /// we are ready to consume a/v data.
    #[conditional_malloc_size_of]
    is_ready: Arc<Once>,
    /// Indicates whether the type of media stream to be played is a live stream.
    stream_type: StreamType,
    /// Decorator used to setup the video sink and process the produced frames.
    #[conditional_malloc_size_of]
    render: Arc<Mutex<GStreamerRender>>,
    /// Whether to enable download buffering. This is only enabled heuristically for some media,
    /// because it causes issues in certain situations. See <https://github.com/servo/servo/issues/23241>.
    download_buffering_enabled: AtomicBool,
}

/// downloadbuffer keeps the download in the user cache directory and it will
/// make the whole pipeline fail if it can't be accessed.
fn ensure_download_directory() -> bool {
    let cache_dir = glib::user_cache_dir();
    match std::fs::create_dir_all(&cache_dir) {
        Ok(()) => true,
        Err(error) => {
            gstreamer::warning!(
                gstreamer::DebugCategory::get("servoplayer").unwrap(),
                "Can't use cache dir {:?} ({}), won't be able to use progressive download",
                cache_dir,
                error
            );
            false
        },
    }
}

static LEAKED_PLAYERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Dragons-hit players awaiting disposal: the refs that blocked safe dispose
/// at drop time are transient (in-flight bus messages / signal emissions) and
/// drain within milliseconds-to-seconds. Instead of leaking permanently, the
/// reaper re-runs the EXACT same refcount check later; once only the queue's
/// own references remain (inner strong count 1, glib ref count 1) disposal is
/// exactly as safe as the original drop-time dispose. Fixes the permanent
/// per-video retention measured at ~140MB per navigate-away (sec1zt86).
struct PendingDispose {
    player: gstreamer_play::Play,
    queued_at: std::time::Instant,
}

static PENDING_DISPOSE: std::sync::LazyLock<Mutex<Vec<PendingDispose>>> =
    std::sync::LazyLock::new(|| Mutex::new(Vec::new()));
static REAPER_SPAWNED: std::sync::Once = std::sync::Once::new();

fn queue_deferred_dispose(player: gstreamer_play::Play) {
    PENDING_DISPOSE.lock().unwrap().push(PendingDispose {
        player,
        queued_at: std::time::Instant::now(),
    });
    REAPER_SPAWNED.call_once(|| {
        std::thread::Builder::new()
            .name("gst-dispose-reaper".to_string())
            .spawn(|| loop {
                std::thread::sleep(std::time::Duration::from_secs(2));
                let mut pending = PENDING_DISPOSE.lock().unwrap();
                let mut still_pending = Vec::new();
                for entry in pending.drain(..) {
                    let play_refs = entry.player.ref_count();
                    // count 1 means only the queue's own clone references the
                    // object — the exact drop-time invariant, shifted by the
                    // queue's reference.
                    if play_refs == 1 {
                        eprintln!("[playerleak] deferred dispose firing after {:?}",
                            entry.queued_at.elapsed());
                        // SAFETY: identical to the drop-time dispose — nobody
                        // uses the player any more; handlers keep weak refs.
                        unsafe {
                            entry.player.run_dispose();
                        }
                    } else {
                        still_pending.push(entry);
                    }
                }
                *pending = still_pending;
            })
            .expect("could not spawn gst dispose reaper");
    });
}

impl GStreamerPlayer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: usize,
        context_id: &ClientContextId,
        backend_chan: Arc<Mutex<Sender<BackendMsg>>>,
        stream_type: StreamType,
        observer: GenericCallback<PlayerEvent>,
        video_renderer: Option<Arc<Mutex<dyn VideoFrameRenderer>>>,
        audio_renderer: Option<Arc<Mutex<dyn AudioRenderer>>>,
        gl_context: Box<dyn PlayerGLContext>,
    ) -> GStreamerPlayer {
        let _ = gstreamer::DebugCategory::new(
            "servoplayer",
            gstreamer::DebugColorFlags::empty(),
            Some("Servo player"),
        );

        Self {
            id,
            context_id: *context_id,
            backend_chan,
            inner: RefCell::new(None),
            observer: Arc::new(Mutex::new(observer)),
            audio_renderer,
            video_renderer,
            is_ready: Arc::new(Once::new()),
            stream_type,
            render: Arc::new(Mutex::new(GStreamerRender::new(gl_context))),
            download_buffering_enabled: AtomicBool::new(false),
        }
    }

    fn setup(&self) -> Result<(), PlayerError> {
        if self.inner.borrow().is_some() {
            return Ok(());
        }

        // Check that we actually have and can load the elements that we need to
        // make this work.
        for element in ["playbin3", "decodebin3", "queue"] {
            let is_loadable = gstreamer::ElementFactory::find(element)
                .is_some_and(|factory| factory.load().is_ok());
            if !is_loadable {
                return Err(PlayerError::Backend(format!(
                    "Missing dependency: {}",
                    element
                )));
            }
        }

        let player = gstreamer_play::Play::default();
        let signal_adapter = gstreamer_play::PlaySignalAdapter::new_sync_emit(&player);
        let pipeline = player.pipeline();

        // FIXME(#43421): The progressive downloading breaks playback on Android.
        if !cfg!(target_os = "android")
            && self.download_buffering_enabled.load(Ordering::Relaxed)
            && ensure_download_directory()
        {
            // Set player to perform progressive downloading. This will make the
            // player store the downloaded media in a local temporary file for
            // faster playback of already-downloaded chunks.
            let flags = pipeline.property_value("flags");
            let flags_class = match glib::FlagsClass::with_type(flags.type_()) {
                Some(flags) => flags,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let flags_class = match flags_class.builder_with_value(flags) {
                Some(class) => class,
                None => {
                    return Err(PlayerError::Backend(
                        "FlagsClass creation failed".to_owned(),
                    ));
                },
            };
            let Some(flags) = flags_class.set_by_nick("download").build() else {
                return Err(PlayerError::Backend(
                    "FlagsClass creation failed".to_owned(),
                ));
            };
            pipeline.set_property_from_value("flags", &flags);
        }

        // Set max size for the player buffer.
        pipeline.set_property("buffer-size", MAX_BUFFER_SIZE);

        // Set player position interval update to 0.5 seconds.
        let mut config = player.config();
        config.set_position_update_interval(500u32);
        player
            .set_config(config)
            .map_err(|e| PlayerError::Backend(e.to_string()))?;

        // The audio sink must exist even when there is no audio renderer
        // (e.g. no audio hardware): without any consumer the audio stream
        // backpressures the whole pipeline and video playback freezes.
        // With no renderer we install the same appsink but drain-and-drop.
        {
            let audio_sink =
                gstreamer::ElementFactory::make("appsink")
                    .build()
                    .map_err(|error| {
                        PlayerError::Backend(format!("appsink creation failed: {error:?}"))
                    })?;

            pipeline.set_property("audio-sink", &audio_sink);

            let audio_sink = audio_sink.dynamic_cast::<gstreamer_app::AppSink>().unwrap();

            let weak_audio_renderer = self
                .audio_renderer
                .as_ref()
                .map(|renderer| Arc::downgrade(renderer));

            audio_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll(|_| Ok(gstreamer::FlowSuccess::Ok))
                    .new_sample(move |audio_sink| {
                        let sample = audio_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;

                        let Some(ref weak_audio_renderer) = weak_audio_renderer else {
                            // No audio renderer — drain and drop so the
                            // stream never backpressures the pipeline.
                            return Ok(gstreamer::FlowSuccess::Ok);
                        };

                        let buffer = sample.buffer_owned().ok_or(gstreamer::FlowError::Error)?;
                        let audio_info = sample
                            .caps()
                            .and_then(|caps| gstreamer_audio::AudioInfo::from_caps(caps).ok())
                            .ok_or(gstreamer::FlowError::Error)?;
                        let positions =
                            audio_info.positions().ok_or(gstreamer::FlowError::Error)?;

                        let Some(audio_renderer) = weak_audio_renderer.upgrade() else {
                            return Ok(gstreamer::FlowSuccess::Ok);
                        };

                        for position in positions.iter() {
                            let buffer = buffer.clone();
                            let map = match buffer.into_mapped_buffer_readable() {
                                Ok(map) => map,
                                _ => {
                                    return Err(gstreamer::FlowError::Error);
                                },
                            };
                            let chunk = Box::new(GStreamerAudioChunk(map));
                            let channel = position.to_mask() as u32;

                            {
                                use std::sync::atomic::{AtomicUsize, Ordering};
                                static AUDIO_SAMPLES: AtomicUsize = AtomicUsize::new(0);
                                let n = AUDIO_SAMPLES.fetch_add(1, Ordering::Relaxed);
                                if n % 100 == 0 {
                                    eprintln!("[mse] audio sample n={}", n);
                                }
                            }

                            audio_renderer.lock().unwrap().render(chunk, channel);
                        }
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        let video_sink = self.render.lock().unwrap().setup_video_sink(&pipeline)?;

        // There's a known bug in gstreamer that may cause a wrong transition
        // to the ready state while setting the uri property:
        // https://cgit.freedesktop.org/gstreamer/gst-plugins-bad/commit/?id=afbbc3a97ec391c6a582f3c746965fdc3eb3e1f3
        // This may affect things like setting the config, so until the bug is
        // fixed, make sure that state dependent code happens before this line.
        // The estimated version for the fix is 1.14.5 / 1.15.1.
        // https://github.com/servo/servo/issues/22010#issuecomment-432599657
        let uri = match self.stream_type {
            StreamType::Stream => {
                register_servo_media_stream_src().map_err(|error| {
                    PlayerError::Backend(format!(
                        "servomediastreamsrc registration error: {error:?}"
                    ))
                })?;
                "mediastream://".to_value()
            },
            StreamType::Seekable => {
                register_servo_src().map_err(|error| {
                    PlayerError::Backend(format!("servosrc registration error: {error:?}"))
                })?;
                "servosrc://".to_value()
            },
            StreamType::MediaSource => {
                register_servo_mse().map_err(|error| {
                    PlayerError::Backend(format!("servomse registration error: {error:?}"))
                })?;
                "servomse://".to_value()
            },
        };
        player.set_property("uri", &uri);

        // No video_renderers no video
        if self.video_renderer.is_none() {
            player.set_video_track_enabled(false);
        }

        *self.inner.borrow_mut() = Some(Arc::new(Mutex::new(PlayerInner {
            player,
            signal_adapter: signal_adapter.clone(),
            source: None,
            video_sink,
            input_size: 0,
            seekable: true,
            play_state: gstreamer_play::PlayState::Stopped,
            paused: Cell::new(DEFAULT_PAUSED),
            can_resume: Cell::new(DEFAULT_CAN_RESUME),
            playback_rate: Cell::new(DEFAULT_PLAYBACK_RATE),
            muted: Cell::new(DEFAULT_MUTED),
            volume: Cell::new(DEFAULT_VOLUME),
            stream_type: self.stream_type,
            last_metadata: None,
            cat: gstreamer::DebugCategory::get("servoplayer").unwrap(),
            enough_data: Arc::new(AtomicBool::new(false)),
            seek_lock: None,
            mse_position_offset: Cell::new(0.0),
            mse_seek_serial: Arc::new(Mutex::new(())),
            mse_seek_gen: Arc::new(AtomicU64::new(0)),
        })));

        let inner = self.inner.borrow();
        let inner = inner.as_ref().unwrap();
        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        // Handle `end-of-stream` signal.
        signal_adapter.connect_end_of_stream(move |_| {
            run_callback_guarded(|| {
                // At EOS the pipeline stops consuming regardless of any prior
                // play() call, so mirror that in our bookkeeping. Without this the
                // paused flag stays false forever and a later replay never forwards
                // a fresh play() into the pipeline, freezing replay after EOS.
                if let Some(inner_clone) = weak_inner.upgrade() {
                    // Never block on the PlayerInner mutex from a gstreamer
                    // callback: the control path holds it across blocking gst
                    // calls, so blocking here completes an ABBA deadlock cycle
                    // (script thread wedged in gst_pad_set_active, callbacks
                    // wedged on this mutex — verified via gdb on a live wedge).
                    // State writes skipped under contention converge on the
                    // next emission; a wedged browser does not recover at all.
                    if let Some(inner) = inner_clone.try_lock().ok() {
                        inner.paused.set(true);
                    }
                }
                let _ = notify!(observer, PlayerEvent::EndOfStream);
            });
        });

        let observer = self.observer.clone();
        // Handle `error` signal
        signal_adapter.connect_error(move |_self, error, _details| {
            run_callback_guarded(|| {
                let _ = notify!(observer, PlayerEvent::Error(error.to_string()));
            });
        });

        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        // Handle `state-changed` signal.
        signal_adapter.connect_state_changed(move |_, play_state| {
            run_callback_guarded(|| {
                let Some(inner_clone) = weak_inner.upgrade() else {
                    return;
                };
                if let Ok(mut inner) = inner_clone.try_lock() {
                    inner.play_state = play_state;
                }

                let state = match play_state {
                    gstreamer_play::PlayState::Buffering => Some(PlaybackState::Buffering),
                    gstreamer_play::PlayState::Stopped => Some(PlaybackState::Stopped),
                    gstreamer_play::PlayState::Paused => Some(PlaybackState::Paused),
                    gstreamer_play::PlayState::Playing => Some(PlaybackState::Playing),
                    _ => None,
                };
                if let Some(v) = state {
                    let _ = notify!(observer, PlayerEvent::StateChanged(v));
                }
            });
        });

        let observer = self.observer.clone();
        let weak_inner_pos = Arc::downgrade(inner);
        // Handle `position-update` signal.
        signal_adapter.connect_position_updated(move |_, position| {
            run_callback_guarded(|| {
                if let Some(seconds) = position.map(|p| p.seconds_f64()) {
                    // Post-seek the pipeline stream-time restarts at zero, so
                    // the raw position reports time-since-seek. The seek target
                    // captured in mse_position_offset re-anchors it to
                    // presentation time (zero before the first seek — free).
                    let offset = weak_inner_pos
                        .upgrade()
                        .and_then(|inner| inner.try_lock().ok().map(|i| i.mse_position_offset.get()))
                        .unwrap_or(0.0);
                    let _ = notify!(observer, PlayerEvent::PositionChanged(seconds + offset));
                }
            });
        });

        let observer = self.observer.clone();
        // Handle `seek-done` signal.
        signal_adapter.connect_seek_done(move |_, position| {
            run_callback_guarded(|| {
                let _ = notify!(observer, PlayerEvent::SeekDone(position.seconds_f64()));
            });
        });

        // Handle `media-info-updated` signal.
        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        signal_adapter.connect_media_info_updated(move |_, info| {
            run_callback_guarded(|| {
                let Ok(metadata) = metadata_from_media_info(info) else {
                    return;
                };

                let Some(inner_clone) = weak_inner.upgrade() else {
                    return;
                };
                let Ok(mut inner) = inner_clone.try_lock() else {
                    return;
                };

                if inner.last_metadata.as_ref() == Some(&metadata) {
                    return;
                }

                // TODO: Workaround to generate expected `paused` state change event.
                // <https://github.com/servo/servo/issues/40740>
                let mut send_pause_event = false;

                if inner.last_metadata.is_none() {
                    if metadata.is_seekable && inner.playback_rate.get() != DEFAULT_PLAYBACK_RATE {
                        // The `paused` state change event will be fired after the
                        // seek initiated by the playback rate change has
                        // completed.
                        inner.player.set_rate(inner.playback_rate.get());
                    } else if inner.play_state == gstreamer_play::PlayState::Paused {
                        // We need to send the paused event for non seekable push streams.
                        send_pause_event = true;
                    }
                }

                let new_duration = metadata.duration;
                let duration_changed = new_duration.is_some()
                    && inner
                        .last_metadata
                        .as_ref()
                        .is_some_and(|last_metadata| last_metadata.duration != new_duration);

                inner.last_metadata = Some(metadata.clone());
                gstreamer::info!(
                    inner.cat,
                    obj = &inner.player,
                    "Metadata updated: {:?}",
                    metadata
                );
                let _ = notify!(observer, PlayerEvent::MetadataUpdated(metadata));

                // We need to check this here because there is no guarantee that
                // duration-changed and media-info-updated come in the same order.
                // If the latter arrives before the former, it will look like the
                // new duration was the old one.
                if duration_changed {
                    gstreamer::info!(
                        inner.cat,
                        obj = &inner.player,
                        "Duration changed with metadata: {:?}",
                        new_duration
                    );
                    let _ = notify!(observer, PlayerEvent::DurationChanged(new_duration));
                }

                if send_pause_event {
                    let _ = notify!(observer, PlayerEvent::StateChanged(PlaybackState::Paused));
                }
            });
        });

        // Handle `duration-changed` signal.
        let weak_inner = Arc::downgrade(inner);
        let observer = self.observer.clone();
        signal_adapter.connect_duration_changed(move |_, duration| {
            run_callback_guarded(|| {
                let duration = duration.map(|duration| {
                    time::Duration::new(
                        duration.seconds(),
                        (duration.nseconds() % 1_000_000_000) as u32,
                    )
                });

                let Some(inner_clone) = weak_inner.upgrade() else {
                    return;
                };
                let Ok(mut inner) = inner_clone.try_lock() else {
                    return;
                };
                if let Some(ref mut metadata) = inner.last_metadata
                    && metadata.duration != duration
                {
                    metadata.duration = duration;
                    gstreamer::info!(
                        inner.cat,
                        obj = &inner.player,
                        "Duration changed: {:?}",
                        duration
                    );
                    let _ = notify!(observer, PlayerEvent::DurationChanged(duration));
                }
            });
        });

        if let Some(video_renderer) = self.video_renderer.clone() {
            // Creates a closure that renders a frame using the video_renderer
            // Used in the preroll and sample callbacks
            let render_sample = {
                let render = self.render.clone();
                let observer = self.observer.clone();
                let weak_video_renderer = Arc::downgrade(&video_renderer);

                move |sample: gstreamer::Sample| {
                    run_callback_guarded(|| {
                        let Some(frame) = render.lock().unwrap().get_frame_from_sample(sample)
                        else {
                            return Err(gstreamer::FlowError::Error);
                        };

                    match weak_video_renderer.upgrade() {
                        Some(video_renderer) => {
                            video_renderer.lock().unwrap().render(frame);
                        },
                        _ => {
                            return Err(gstreamer::FlowError::Flushing);
                        },
                    };

                        let _ = notify!(observer, PlayerEvent::VideoFrameUpdated);
                        Ok(gstreamer::FlowSuccess::Ok)
                    })
                    .unwrap_or(Err(gstreamer::FlowError::Error))
                }
            };

            // Set video_sink callbacks.
            inner.lock().unwrap().video_sink.set_callbacks(
                gstreamer_app::AppSinkCallbacks::builder()
                    .new_preroll({
                        let render_sample = render_sample.clone();
                        move |video_sink| {
                            render_sample(
                                video_sink
                                    .pull_preroll()
                                    .map_err(|_| gstreamer::FlowError::Eos)?,
                            )
                        }
                    })
                    .new_sample(move |video_sink| {
                        let sample = video_sink
                            .pull_sample()
                            .map_err(|_| gstreamer::FlowError::Eos)?;
                        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let c = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if c % 30 == 0 || c < 3 {
                            eprintln!("[mse] video sample n={} pts={:?}", c, sample.buffer().map(|b| b.pts()));
                        }
                        render_sample(sample)
                    })
                    .build(),
            );
        };

        let (receiver, error_handler_id) = {
            let weak_inner = Arc::downgrade(inner);
            let inner = inner.lock().unwrap();
            let pipeline = inner.player.pipeline();

            let (sender, receiver) = mpsc::channel();

            let sender = Arc::new(Mutex::new(sender));
            let sender_clone = sender.clone();
            let is_ready_clone = self.is_ready.clone();
            let observer = self.observer.clone();
            pipeline.connect("source-setup", false, move |args| {
                return run_callback_guarded(|| {
                    let source = args[1].get::<gstreamer::Element>().unwrap();

                let inner_clone = weak_inner.upgrade()?;
                let mut inner = inner_clone.lock().unwrap();
                let source = match inner.stream_type {
                    StreamType::Seekable => {
                        let servosrc = source
                            .dynamic_cast::<ServoSrc>()
                            .expect("Source element is expected to be a ServoSrc!");

                        if inner.input_size > 0 {
                            servosrc.set_size(inner.input_size as i64);
                        }
                        servosrc.set_seekable(inner.seekable);

                        let sender_clone = sender.clone();
                        let is_ready = is_ready_clone.clone();
                        let observer_ = observer.clone();
                        let observer__ = observer.clone();
                        let observer___ = observer.clone();
                        let weak_servosrc = servosrc.downgrade();
                        let weak_servosrc_need = servosrc.downgrade();
                        let enough_data_ = inner.enough_data.clone();
                        let enough_data__ = inner.enough_data.clone();
                        let seek_channel = Arc::new(Mutex::new(SeekChannel::new()));
                        inner.seek_lock = Some(seek_channel.lock().unwrap().sender());
                        servosrc.set_callbacks(
                            gstreamer_app::AppSrcCallbacks::builder()
                                .need_data(move |_, _| {
                                    // We block the caller of the setup method until we get
                                    // the first need-data signal, so we ensure that we
                                    // don't miss any data between the moment the client
                                    // calls setup and the player is actually ready to
                                    // get any data.
                                    is_ready.call_once(|| {
                                        let _ = sender_clone.lock().unwrap().send(Ok(()));
                                    });

                                    if let Some(servosrc_) = weak_servosrc_need.upgrade() {
                                        servosrc_.fire_pending_eos();
                                    }
                                    enough_data_.store(false, Ordering::Relaxed);
                                    let _ = notify!(observer_, PlayerEvent::NeedData);
                                })
                                .enough_data(move |_| {
                                    enough_data__.store(true, Ordering::Relaxed);
                                    let _ = notify!(observer__, PlayerEvent::EnoughData);
                                })
                                .seek_data(move |_, offset| {
                                    let Some(servosrc_) = weak_servosrc.upgrade() else {
                                        return false;
                                    };
                                    let (ret, ack_channel) = if servosrc_.set_seek_offset(offset) {
                                        let _ = notify!(
                                            observer___,
                                            PlayerEvent::SeekData(
                                                offset,
                                                seek_channel.lock().unwrap().sender()
                                            )
                                        );
                                        match seek_channel.lock().unwrap()._await() {
                                            Some((ret, ack_channel)) => (ret, Some(ack_channel)),
                                            None => (false, None),
                                        }
                                    } else {
                                        (true, None)
                                    };

                                    servosrc_.set_seek_done();
                                    if let Some(ack_channel) = ack_channel {
                                        // The receiver is dropped when the player
                                        // is torn down while the seek is still in
                                        // flight — a missing ack is benign (nobody
                                        // is waiting), panicking is not.
                                        let _ = ack_channel.send(());
                                    }
                                    ret
                                })
                                .build(),
                        );

                        PlayerSource::Seekable(servosrc)
                    },
                    StreamType::Stream => {
                        let media_stream_src = source
                            .dynamic_cast::<ServoMediaStreamSrc>()
                            .expect("Source element is expected to be a ServoMediaStreamSrc!");
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::Stream(media_stream_src)
                    },
                    StreamType::MediaSource => {
                        let mse = source
                            .dynamic_cast::<ServoMse>()
                            .expect("Source element is expected to be a ServoMse!");
                        // MSE data arrives only when the page appends, so the
                        // pipeline readiness handshake must not wait for data.
                        let sender_clone = sender.clone();
                        is_ready_clone.call_once(|| {
                            let _ = notify!(sender_clone, Ok(()));
                        });
                        PlayerSource::Mse(mse)
                    },
                };

                    inner.set_src(source);

                    None
                })
                .unwrap_or(None)
            });

            let error_handler_id =
                signal_adapter.connect_error(move |signal_adapter, error, _details| {
                    run_callback_guarded(|| {
                        let _ = notify!(sender_clone, Err(PlayerError::Backend(error.to_string())));
                        signal_adapter.play().stop();
                    });
                });

            inner.player.pause();

            (receiver, error_handler_id)
        };

        let result = receiver.recv().unwrap();
        glib::signal::signal_handler_disconnect(
            &inner.lock().unwrap().signal_adapter,
            error_handler_id,
        );
        result
    }
}

macro_rules! inner_player_proxy_getter {
    ($fn_name:ident, $return_type:ty, $default_value:expr_2021) => {
        fn $fn_name(&self) -> $return_type {
            if self.setup().is_err() {
                return $default_value;
            }

            let inner = self.inner.borrow();
            let inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };
}

macro_rules! inner_player_proxy {
    ($fn_name:ident, $return_type:ty) => {
        fn $fn_name(&self) -> Result<$return_type, PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name()
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1)
        }
    };

    ($fn_name:ident, $arg1:ident, $arg1_type:ty, $arg2:ident, $arg2_type:ty) => {
        fn $fn_name(&self, $arg1: $arg1_type, $arg2: $arg2_type) -> Result<(), PlayerError> {
            self.setup()?;
            let inner = self.inner.borrow();
            let mut inner = inner.as_ref().unwrap().lock().unwrap();
            inner.$fn_name($arg1, $arg2)
        }
    };
}

impl Player for GStreamerPlayer {
    inner_player_proxy!(play, ());
    inner_player_proxy!(pause, ());
    inner_player_proxy_getter!(paused, bool, DEFAULT_PAUSED);
    inner_player_proxy_getter!(can_resume, bool, DEFAULT_CAN_RESUME);
    inner_player_proxy!(stop, ());
    inner_player_proxy!(end_of_stream, ());
    inner_player_proxy!(set_input_size, size, u64);
    inner_player_proxy!(set_seekable, seekable, bool);
    inner_player_proxy!(set_mute, muted, bool);
    inner_player_proxy_getter!(muted, bool, DEFAULT_MUTED);
    inner_player_proxy!(set_playback_rate, playback_rate, f64);
    inner_player_proxy_getter!(playback_rate, f64, DEFAULT_PLAYBACK_RATE);
    inner_player_proxy!(push_data, data, Vec<u8>);
    fn push_data_track(
        &self,
        track: u64,
        total_tracks: u64,
        data: Vec<u8>,
    ) -> Result<(), PlayerError> {
        self.setup()?;
        let inner = self.inner.borrow();
        let mut inner = inner.as_ref().unwrap().lock().unwrap();
        inner.push_data_track(track, total_tracks, data)
    }
    inner_player_proxy!(seek, time, f64);
    inner_player_proxy!(set_volume, volume, f64);
    inner_player_proxy_getter!(volume, f64, DEFAULT_VOLUME);
    inner_player_proxy_getter!(buffered, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy_getter!(seekable, Vec<Range<f64>>, DEFAULT_TIME_RANGES);
    inner_player_proxy!(set_stream, stream, &MediaStreamId, only_stream, bool);
    inner_player_proxy!(set_audio_track, stream_index, i32, enabled, bool);
    inner_player_proxy!(set_video_track, stream_index, i32, enabled, bool);

    fn set_download_buffering_enabled(&self, enable: bool) -> Result<(), PlayerError> {
        // We can't use the default implementation because we need to set the
        // download_buffering_enabled flag before calling setup, which is where
        // the "download" flag is set on the pipeline. That has to happen before
        // the URI is set.
        self.download_buffering_enabled
            .store(enable, Ordering::Relaxed);
        self.setup()?;
        Ok(())
    }

    fn render_use_gl(&self) -> bool {
        self.render.lock().unwrap().is_gl()
    }
}

impl MediaInstance for GStreamerPlayer {
    fn get_id(&self) -> usize {
        self.id
    }

    fn mute(&self, val: bool) -> Result<(), MediaInstanceError> {
        self.set_mute(val).map_err(|_| MediaInstanceError)
    }

    fn suspend(&self) -> Result<(), MediaInstanceError> {
        self.pause().map_err(|_| MediaInstanceError)
    }

    fn resume(&self) -> Result<(), MediaInstanceError> {
        if !self.can_resume() {
            return Ok(());
        }

        self.play().map_err(|_| MediaInstanceError)
    }
}

impl Drop for GStreamerPlayer {
    fn drop(&mut self) {
        let _ = self.stop();
        let (tx_ack, rx_ack) = mpsc::channel();
        let _ = self
            .backend_chan
            .lock()
            .unwrap()
            .send(BackendMsg::Shutdown {
                context: self.context_id,
                id: self.id,
                tx_ack,
            });
        let _ = rx_ack.recv();
        // We dispose the GstPlay from this thread while we still have a
        // reference, so that the GstPlay thread join happens here. Otherwise,
        // if the last reference was held by a GstPlay message being posted, the
        // GstPlay dispose would be done from its own thread and it would be
        // freed while still in use. Besides, the seek_data can be blocked
        // waiting for the script thread, which is this one, busy disposing the
        // player. We release it first so that the join does not wait forever.
        if let Some(inner) = self.inner.borrow_mut().take() {
            let seek_lock = inner.lock().unwrap().seek_lock.take();
            let player = inner.lock().unwrap().player.clone();
            let (ack_sender, _ack_receiver) =
                generic_channel::channel::<()>().expect("Couldn't create IPC channel");
            if let Some(seek_lock) = seek_lock {
                let _ = seek_lock.lock_channel.send((false, ack_sender));
            }
            // This code below can be reverted once we ensure we are using the
            // code at
            // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/13022
            // SAFETY: nobody uses the player after this moment. Handlers keep weak refs.
            //
            // That safety claim is sometimes FALSE in practice: bus messages or
            // signal emissions still in flight can hold references during
            // teardown (video-heavy feeds churning players on scroll hit this —
            // crash signature: gst_bus_set_flushing / g_object_ref
            // '!object_already_finalized' assertions then SIGSEGV in the script
            // thread). When the dragons condition is detected we therefore
            // INTENTIONALLY LEAK the player instead of disposing it: a bounded
            // memory cost per torn-down video in exchange for eliminating the
            // use-after-finalize crash class. The leak is a handful of pipeline
            // objects per video element, not frame data. Revert to unconditional
            // dispose once the upstream MR13022 lifecycle fix is in.
            let inner_refs = Arc::strong_count(&inner);
            let play_refs = player.ref_count();
            if inner_refs > 1 || play_refs > 1 {
                LEAKED_PLAYERS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                eprintln!(
                    "[playerleak] intentionally leaking player (inner refs {}, play refs {}); total leaked {}",
                    inner_refs,
                    play_refs,
                    LEAKED_PLAYERS.load(std::sync::atomic::Ordering::Relaxed)
                );
                // Do NOT dispose now — but do not leak permanently either:
                // the blocking refs are transient in-flight bus messages, so
                // queue for deferred dispose once they drain (sec1zt86).
                // Only the player is queued: inner contains the signal adapter
                // which itself holds a strong glib ref on the play object —
                // queuing inner would pin the object through our own queue and
                // the refcount would never drain (sec1zt87 mechanism).
                drop(inner);
                queue_deferred_dispose(player);
                return;
            }
            unsafe {
                player.run_dispose();
            }
            drop(inner);
        }
    }
}
