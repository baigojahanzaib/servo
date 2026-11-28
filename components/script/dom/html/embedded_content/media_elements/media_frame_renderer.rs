/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::{Arc, Mutex, RwLock, Weak};

use euclid::default::Size2D;
use layout_api::MediaFrame;
use media::{GLPlayerMsg, GLPlayerMsgForward, WindowGLContext};
use paint_api::{CrossProcessPaintApi, ImageUpdate, SerializableImageData};
use pixels::RasterImage;
use servo_base::generic_channel::{GenericCallback, GenericSharedMemory};
use servo_base::id::WebViewId;
use servo_media::player::video::{VideoFrame, VideoFrameRenderer};
use webrender_api::{
    ExternalImageData, ExternalImageId, ExternalImageType, ImageBufferKind, ImageDescriptor,
    ImageDescriptorFlags, ImageFormat, ImageKey,
};

use crate::tasks::task_source::SendableTaskSource;

#[derive(MallocSizeOf, PartialEq)]
enum FrameStatus {
    Locked,
    Unlocked,
}

#[derive(MallocSizeOf)]
pub(crate) struct FrameHolder(FrameStatus, VideoFrame);

impl FrameHolder {
    fn new(frame: VideoFrame) -> FrameHolder {
        FrameHolder(FrameStatus::Unlocked, frame)
    }

    fn lock(&mut self) {
        if self.0 == FrameStatus::Unlocked {
            self.0 = FrameStatus::Locked;
        };
    }

    fn unlock(&mut self) {
        if self.0 == FrameStatus::Locked {
            self.0 = FrameStatus::Unlocked;
        };
    }

    fn set(&mut self, new_frame: VideoFrame) {
        if self.0 == FrameStatus::Unlocked {
            self.1 = new_frame
        };
    }

    fn get(&self) -> (u32, Size2D<i32>, usize) {
        if self.0 == FrameStatus::Locked {
            (
                self.1.get_texture_id(),
                Size2D::new(self.1.get_width(), self.1.get_height()),
                0,
            )
        } else {
            unreachable!();
        }
    }

    pub(crate) fn get_frame(&self) -> VideoFrame {
        self.1.clone()
    }
}

#[derive(MallocSizeOf)]
pub(crate) struct MediaFrameRenderer {
    webview_id: WebViewId,
    player_id: Option<usize>,
    #[conditional_malloc_size_of]
    glplayer_id: Arc<RwLock<Option<u64>>>,
    paint_api: CrossProcessPaintApi,
    player_context: WindowGLContext,
    current_frame: Option<MediaFrame>,
    old_frame: Option<MediaFrame>,
    very_old_frame: Option<MediaFrame>,
    current_frame_holder: Option<FrameHolder>,
    /// <https://html.spec.whatwg.org/multipage/#poster-frame>
    poster_frame: Option<MediaFrame>,
}

impl MediaFrameRenderer {
    pub(crate) fn new(
        webview_id: WebViewId,
        paint_api: CrossProcessPaintApi,
        player_context: WindowGLContext,
    ) -> Self {
        Self {
            webview_id,
            player_id: None,
            glplayer_id: Arc::new(RwLock::new(None)),
            paint_api,
            player_context,
            current_frame: None,
            old_frame: None,
            very_old_frame: None,
            current_frame_holder: None,
            poster_frame: None,
        }
    }

    pub(crate) fn current_frame(&self) -> Option<MediaFrame> {
        self.current_frame
    }

    pub(crate) fn poster_frame(&self) -> Option<MediaFrame> {
        self.poster_frame
    }

    pub(crate) fn current_frame_holder(&self) -> Option<&FrameHolder> {
        self.current_frame_holder.as_ref()
    }

    pub(crate) fn setup(
        &mut self,
        player_id: usize,
        task_source: SendableTaskSource,
        weak_video_renderer: Weak<Mutex<MediaFrameRenderer>>,
    ) {
        self.player_id = Some(player_id);
        let glplayer_id = self.glplayer_id.clone();
        let callback = GenericCallback::new(move |message| {
            let message = message.unwrap();
            let weak_video_renderer = weak_video_renderer.clone();

            let glplayer_id = glplayer_id.clone();
            task_source.queue(task!(handle_glplayer_message: move || {
                trace!("GLPlayer message {:?}", message);

                let Some(video_renderer) = weak_video_renderer.upgrade() else {
                    return;
                };

                match message {
                    GLPlayerMsgForward::Lock(sender) => {
                        if let Some(holder) = video_renderer
                            .lock()
                            .unwrap()
                            .current_frame_holder
                            .as_mut() {
                                holder.lock();
                                sender.send(holder.get()).unwrap();
                            };
                    },
                    GLPlayerMsgForward::Unlock() => {
                        if let Some(holder) = video_renderer
                            .lock()
                            .unwrap()
                            .current_frame_holder
                            .as_mut() { holder.unlock() }
                    },
                    GLPlayerMsgForward::PlayerId(id) => {
                        let mut glplayer_id = glplayer_id.write().unwrap();
                        if let Some(already_set_id) = *glplayer_id {
                            error!("Player id already set to {already_set_id} will be replaced with {id}");
                        }
                        *glplayer_id = Some(id);
                    },
                }
            }));
        })
        .expect("Could not create callback");

        if let Some(glplayer_thread_sender) = &self.player_context.glplayer_thread_sender {
            glplayer_thread_sender
                .send(GLPlayerMsg::RegisterPlayer(callback))
                .unwrap();
        }
    }

    pub(crate) fn reset(&mut self) {
        self.player_id = None;

        if let Some(glplayer_id) = self.glplayer_id.write().unwrap().take() {
            self.player_context
                .send(GLPlayerMsg::UnregisterPlayer(glplayer_id));
        }

        self.current_frame_holder = None;

        let mut updates = smallvec::smallvec![];

        if let Some(current_frame) = self.current_frame.take() {
            delete_frame_keys(&mut updates, current_frame);
        }

        if let Some(old_frame) = self.old_frame.take() {
            delete_frame_keys(&mut updates, old_frame);
        }

        if let Some(very_old_frame) = self.very_old_frame.take() {
            delete_frame_keys(&mut updates, very_old_frame);
        }

        if !updates.is_empty() {
            self.paint_api
                .update_images(self.webview_id.into(), updates);
        }
    }

    pub(crate) fn set_poster_frame(&mut self, image: Option<Arc<RasterImage>>) {
        self.poster_frame = image.and_then(|image| {
            image.id.map(|image_key| MediaFrame {
                image_key,
                width: image.metadata.width as i32,
                height: image.metadata.height as i32,
                uv_key: None,
                v_key: None,
            })
        });
    }
}


fn delete_frame_keys<A: smallvec::Array<Item = ImageUpdate>>(
    updates: &mut smallvec::SmallVec<A>,
    frame: MediaFrame,
) {
    updates.push(ImageUpdate::DeleteImage(frame.image_key));
    if let Some(uv_key) = frame.uv_key {
        updates.push(ImageUpdate::DeleteImage(uv_key));
    }
    if let Some(v_key) = frame.v_key {
        updates.push(ImageUpdate::DeleteImage(v_key));
    }
}

fn plane_descriptor(width: i32, height: i32, format: ImageFormat) -> ImageDescriptor {
    ImageDescriptor::new(width, height, format, ImageDescriptorFlags::empty())
}

impl Drop for MediaFrameRenderer {
    fn drop(&mut self) {
        self.reset();
    }
}

impl VideoFrameRenderer for MediaFrameRenderer {
    fn render(&mut self, frame: VideoFrame) {
        if self.player_id.is_none() ||
            (frame.is_gl_texture() && self.glplayer_id.read().unwrap().is_none())
        {
            return;
        }

        let mut updates = smallvec::smallvec![];

        if let Some(very_old_frame) =
            std::mem::replace(&mut self.very_old_frame, self.old_frame.take())
        {
            delete_frame_keys(&mut updates, very_old_frame);
        }

        let width = frame.get_width();
        let height = frame.get_height();
        let is_nv12 = frame.is_nv12();

        let bgra_descriptor = plane_descriptor(width, height, ImageFormat::BGRA8);

        // NV12 frames are deinterleaved into THREE all-R8 planar payloads:
        // full-resolution luma plus half-resolution Cb and Cr planes. RG8
        // interleaved chroma is never uploaded because WebRender's shared
        // texture cache cannot allocate RG8 and panics (crash class of
        // sec1zt). The deinterleave is a quarter-resolution byte shuffle —
        // far cheaper than full color conversion — and PlanarYCbCr is a
        // natively supported WebRender YUV format.
        let nv12_planes = if is_nv12 {
            let (y, uv) = frame.get_nv12_planes();
            // Strided-copy deinterleave: per-byte push loops prevent
            // vectorization and capacity-check every byte — at 1080p the UV
            // plane is ~1MB/frame, so this cost backpressures the gst
            // streaming thread it runs on. Iterator collects let the compiler
            // batch the copies.
            let u: Vec<u8> = uv.iter().step_by(2).copied().collect();
            let v: Vec<u8> = uv.iter().skip(1).step_by(2).copied().collect();
            Some((
                plane_descriptor(width, height, ImageFormat::R8),
                SerializableImageData::Raw(GenericSharedMemory::from_arc_vec(y)),
                plane_descriptor(width / 2, height / 2, ImageFormat::R8),
                SerializableImageData::Raw(GenericSharedMemory::from_vec(u)),
                SerializableImageData::Raw(GenericSharedMemory::from_vec(v)),
            ))
        } else {
            None
        };

        let external_image_data = |frame: &VideoFrame| {
            self.glplayer_id
                .read()
                .unwrap()
                .filter(|_| frame.is_gl_texture())
                .map(|glplayer_id| {
                    let texture_target = if frame.is_external_oes() {
                        ImageBufferKind::TextureExternal
                    } else {
                        ImageBufferKind::Texture2D
                    };

                    SerializableImageData::External(ExternalImageData {
                        id: ExternalImageId(glplayer_id),
                        channel_index: 0,
                        image_type: ExternalImageType::TextureHandle(texture_target),
                        normalized_uvs: false,
                    })
                })
        };

        let format_matches = self
            .current_frame
            .as_ref()
            .is_some_and(|current| (current.uv_key.is_some()) == is_nv12);
        let size_matches = self
            .current_frame
            .as_ref()
            .is_some_and(|current| current.width == width && current.height == height);

        if format_matches && size_matches {
            let current_frame = self.current_frame.as_ref().unwrap();
            if let Some((y_desc, y_data, uv_desc, u_data, v_data)) = nv12_planes {
                updates.push(ImageUpdate::UpdateImage(
                    current_frame.image_key,
                    y_desc,
                    y_data,
                    None,
                ));
                if let Some(uv_key) = current_frame.uv_key {
                    updates.push(ImageUpdate::UpdateImage(uv_key, uv_desc, u_data, None));
                }
                if let Some(v_key) = current_frame.v_key {
                    updates.push(ImageUpdate::UpdateImage(v_key, uv_desc, v_data, None));
                }
            } else if !frame.is_gl_texture() {
                updates.push(ImageUpdate::UpdateImage(
                    current_frame.image_key,
                    bgra_descriptor,
                    SerializableImageData::Raw(GenericSharedMemory::from_arc_vec(
                        frame.get_data(),
                    )),
                    None,
                ));
            }

            self.current_frame_holder
                .get_or_insert_with(|| FrameHolder::new(frame.clone()))
                .set(frame);

            if let Some(old_frame) = self.old_frame.take() {
                delete_frame_keys(&mut updates, old_frame);
            }
        } else {
            if let Some(current_frame) = self.current_frame.take() {
                self.old_frame = Some(current_frame);
            }

            let Some(image_key) = self.paint_api.generate_image_key_blocking(self.webview_id)
            else {
                return;
            };

            let (descriptor, image_data, uv_key, v_key) = if let Some((y_desc, y_data, uv_desc, u_data, v_data)) =
                nv12_planes
            {
                let Some(uv_image_key) =
                    self.paint_api.generate_image_key_blocking(self.webview_id)
                else {
                    return;
                };
                let Some(v_image_key) =
                    self.paint_api.generate_image_key_blocking(self.webview_id)
                else {
                    return;
                };
                updates.push(ImageUpdate::AddImage(uv_image_key, uv_desc, u_data, false));
                updates.push(ImageUpdate::AddImage(v_image_key, uv_desc, v_data, false));
                (y_desc, y_data, Some(uv_image_key), Some(v_image_key))
            } else {
                let image_data = external_image_data(&frame).unwrap_or_else(|| {
                    SerializableImageData::Raw(GenericSharedMemory::from_arc_vec(
                        frame.get_data(),
                    ))
                });
                (bgra_descriptor, image_data, None, None)
            };

            self.current_frame = Some(MediaFrame {
                image_key,
                width,
                height,
                uv_key,
                v_key,
            });

            self.current_frame_holder
                .get_or_insert_with(|| FrameHolder::new(frame.clone()))
                .set(frame);

            updates.push(ImageUpdate::AddImage(image_key, descriptor, image_data, false));
        }
        self.paint_api
            .update_images(self.webview_id.into(), updates);
    }
}
