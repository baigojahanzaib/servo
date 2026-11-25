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
    }

    pub struct ServoMse {
        pub cat: gstreamer::DebugCategory,
        pub tracks: Mutex<BTreeMap<u64, MseTrack>>,
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

            appsrc.set_caps(None::<&gstreamer::Caps>);
            appsrc.set_max_bytes(MAX_SRC_QUEUE_SIZE);
            appsrc.set_block(false);
            appsrc.set_format(gstreamer::Format::Bytes);
            appsrc.set_stream_type(gstreamer_app::AppStreamType::Stream);

            // Sync state only after the pad chain exists end-to-end, so the
            // src pad activation can actually start the streaming task.
            let _ = appsrc.sync_state_with_parent();
            tracks.insert(id, MseTrack { appsrc, eos: false });
        }

        pub fn push_buffer(
            &self,
            track: u64,
            data: Vec<u8>,
        ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
            self.ensure_track(track);
            let tracks = self.tracks.lock().unwrap();
            let Some(ref t) = tracks.get(&track) else {
                return Err(gstreamer::FlowError::Error);
            };
            let len = data.len();
            eprintln!("MSETRACK push track={} len={}", track, len);
            let buffer = gstreamer::Buffer::from_slice(data);
            t.appsrc.push_buffer(buffer)
        }

        /// EOS is per-stream in gstreamer but MSE endOfStream ends the whole
        /// presentation: flush every track.
        pub fn end_of_stream(&self) {
            self.ensure_track(0);
            let tracks = self.tracks.lock().unwrap();
            for t in tracks.values() {
                let _ = t.appsrc.end_of_stream();
            }
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
        data: Vec<u8>,
    ) -> Result<gstreamer::FlowSuccess, gstreamer::FlowError> {
        self.imp().push_buffer(track, data)
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
