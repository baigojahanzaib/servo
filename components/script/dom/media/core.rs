/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Cross-thread shared state for MSE-in-workers.
//!
//! A MediaSource constructed in a dedicated worker is attached to a media
//! element on the main thread through a serialized [MediaSourceHandle]. The
//! DOM objects are affine to their owning script threads, so the attach
//! plumbing cannot use direct DOM references the way the same-thread blob-URL
//! path does. Instead both sides share this core object: the worker pushes
//! appended bytes straight into the player (player handles are Send), and the
//! media element mirrors control events (attach/detach/seek) back into the
//! worker through the owner's task source.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use servo_media::player::Player;
use uuid::Uuid;

use crate::tasks::task_source::SendableTaskSource;

pub(crate) struct MediaSourceCore {
    pub(crate) id: Uuid,
    /// The media element's push player, installed by the attach path on the
    /// element's thread. Worker-side appends lock and push directly.
    pub(crate) player: Mutex<Option<Arc<Mutex<dyn Player>>>>,
    /// Task source of the script thread that owns the MediaSource object —
    /// used by the element thread to post attach/detach/seek notifications
    /// back into the owner's event loop, where the DOM state machine lives.
    pub(crate) owner_task_source: Mutex<Option<SendableTaskSource>>,
    /// MS duration mirrored for the element thread (f64 bits; NaN = unset).
    pub(crate) duration_bits: AtomicU64,
    /// Merged buffered ranges across all source buffers, mirrored worker-side
    /// after each append for the element thread's readyState logic.
    pub(crate) buffered: Mutex<Vec<(f64, f64)>>,
    pub(crate) attached: AtomicBool,
}

impl MediaSourceCore {
    pub(crate) fn new(id: Uuid) -> MediaSourceCore {
        MediaSourceCore {
            id,
            player: Mutex::new(None),
            owner_task_source: Mutex::new(None),
            duration_bits: AtomicU64::new(f64::NAN.to_bits()),
            buffered: Mutex::new(Vec::new()),
            attached: AtomicBool::new(false),
        }
    }

    pub(crate) fn duration(&self) -> f64 {
        f64::from_bits(self.duration_bits.load(Ordering::Relaxed))
    }

    pub(crate) fn set_duration(&self, value: f64) {
        self.duration_bits.store(value.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.attached.load(Ordering::Relaxed)
    }

    /// Push appended bytes into the attached player. Returns false when no
    /// player is attached (bytes dropped, same as the same-thread path).
    pub(crate) fn push_data(&self, track: u64, total_tracks: u64, bytes: Vec<u8>) -> bool {
        let player = self.player.lock().unwrap();
        if let Some(ref player) = *player {
            return player
                .lock()
                .unwrap()
                .push_data_track(track, total_tracks, bytes)
                .is_ok();
        }
        false
    }

    pub(crate) fn end_of_stream(&self) {
        let player = self.player.lock().unwrap();
        if let Some(ref player) = *player {
            let _ = player.lock().unwrap().end_of_stream();
        }
    }

    pub(crate) fn buffered_ranges(&self) -> Vec<(f64, f64)> {
        self.buffered.lock().unwrap().clone()
    }

    pub(crate) fn set_buffered_ranges(&self, ranges: Vec<(f64, f64)>) {
        *self.buffered.lock().unwrap() = ranges;
    }
}

fn registry() -> &'static Mutex<HashMap<Uuid, Weak<MediaSourceCore>>> {
    static REGISTRY: OnceLock<Mutex<HashMap<Uuid, Weak<MediaSourceCore>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(crate) fn register_core(core: &Arc<MediaSourceCore>) {
    registry()
        .lock()
        .unwrap()
        .insert(core.id, Arc::downgrade(core));
}

/// Look up the shared core for a serialized handle id. Returns None when the
/// owning MediaSource and all its handles have been garbage-collected.
pub(crate) fn lookup_core(id: &Uuid) -> Option<Arc<MediaSourceCore>> {
    registry().lock().unwrap().get(id).and_then(Weak::upgrade)
}
