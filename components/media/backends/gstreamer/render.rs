/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use glib::prelude::*;
use gstreamer_video::prelude::*;
use malloc_size_of_derive::MallocSizeOf;
use servo_media_gstreamer_render::Render;
use servo_media_player::PlayerError;
use servo_media_player::context::PlayerGLContext;
use servo_media_player::video::{Buffer, VideoFrame, VideoFrameData};

#[cfg(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd"
))]
mod platform {
    extern crate servo_media_gstreamer_render_unix;
    pub use self::servo_media_gstreamer_render_unix::RenderUnix as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(target_os = "android")]
mod platform {
    extern crate servo_media_gstreamer_render_android;
    pub use self::servo_media_gstreamer_render_android::RenderAndroid as Render;
    use super::*;

    pub fn create_render(gl_context: Box<dyn PlayerGLContext>) -> Option<Render> {
        Render::new(gl_context)
    }
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "android",
)))]
mod platform {
    use servo_media_gstreamer_render::Render as RenderTrait;
    use servo_media_player::PlayerError;
    use servo_media_player::context::PlayerGLContext;
    use servo_media_player::video::VideoFrame;

    pub struct RenderDummy();
    pub type Render = RenderDummy;

    pub fn create_render(_: Box<dyn PlayerGLContext>) -> Option<RenderDummy> {
        None
    }

    impl RenderTrait for RenderDummy {
        fn is_gl(&self) -> bool {
            false
        }

        fn build_frame(&self, _: gstreamer::Sample) -> Option<VideoFrame> {
            None
        }

        fn build_video_sink(
            &self,
            _: &gstreamer::Element,
            _: &gstreamer::Element,
        ) -> Result<(), PlayerError> {
            Err(PlayerError::Backend(
                "Not available videosink decorator".to_owned(),
            ))
        }
    }
}

#[derive(MallocSizeOf)]
struct GStreamerBuffer {
    #[ignore_malloc_size_of = "External Type"]
    frame: gstreamer_video::VideoFrame<gstreamer_video::video_frame::Readable>,
    stride: usize,
    row_bytes: usize,
    is_nv12: bool,
}

fn pack_plane(data: &[u8], stride: usize, row_bytes: usize, rows: usize) -> Vec<u8> {
    if stride == row_bytes && data.len() == row_bytes * rows {
        return data.to_vec();
    }
    // Whatever consumes VideoFrame expects packed lines, so we discard the
    // padding at the end of each line.
    let mut packed = Vec::with_capacity(row_bytes * rows);
    for row in data.chunks_exact(stride).take(rows) {
        packed.extend_from_slice(&row[..row_bytes]);
    }
    packed
}

impl Buffer for GStreamerBuffer {
    fn to_vec(&self) -> Option<VideoFrameData> {
        let data = self.frame.plane_data(0).ok()?;
        if self.is_nv12 {
            // NV12: plane 0 is full-resolution Y (row_bytes == width), plane 1
            // is half-height interleaved CbCr with the same byte width per row.
            let height = self.frame.height() as usize;
            let uv_stride = usize::try_from(*self.frame.plane_stride().get(1)?).ok()?;
            let uv_data = self.frame.plane_data(1).ok()?;
            let y = pack_plane(data, self.stride, self.row_bytes, height);
            let uv = pack_plane(uv_data, uv_stride, self.row_bytes, height / 2);
            return Some(VideoFrameData::NV12 {
                y: Arc::new(y),
                uv: Arc::new(uv),
            });
        }
        if self.stride == self.row_bytes {
            return Some(VideoFrameData::Raw(Arc::new(data.to_vec())));
        }
        Some(VideoFrameData::Raw(Arc::new(pack_plane(
            data,
            self.stride,
            self.row_bytes,
            self.frame.height() as usize,
        ))))
    }
}

#[derive(MallocSizeOf)]
pub struct GStreamerRender {
    #[ignore_malloc_size_of = "External Type"]
    render: Option<platform::Render>,
}

impl GStreamerRender {
    pub fn new(gl_context: Box<dyn PlayerGLContext>) -> Self {
        GStreamerRender {
            render: platform::create_render(gl_context),
        }
    }

    pub fn is_gl(&self) -> bool {
        if let Some(render) = self.render.as_ref() {
            render.is_gl()
        } else {
            false
        }
    }

    pub fn get_frame_from_sample(&self, sample: gstreamer::Sample) -> Option<VideoFrame> {
        if let Some(render) = self.render.as_ref() {
            render.build_frame(sample)
        } else {
            let buffer = sample.buffer_owned()?;
            let caps = sample.caps()?;
            let info = gstreamer_video::VideoInfo::from_caps(caps).ok()?;
            let frame = gstreamer_video::VideoFrame::from_buffer_readable(buffer, &info).ok()?;
            let is_nv12 = info.format() == gstreamer_video::VideoFormat::Nv12;
            let bytes_per_pixel = if is_nv12 { 1 } else { 4 }; // NV12 Y plane or BGRA
            let row_bytes = info.width() as usize * bytes_per_pixel;
            // This comes from the mapped frame, that respects GstVideoMeta.
            // Caps understate it when the decoder aligns rows, as VideoToolbox
            // does on macOS.
            let stride = usize::try_from(*frame.plane_stride().first()?)
                .ok()
                .filter(|stride| *stride >= row_bytes)?;

            VideoFrame::new(
                info.width() as i32,
                info.height() as i32,
                Arc::new(GStreamerBuffer {
                    frame,
                    stride,
                    row_bytes,
                    is_nv12,
                }),
            )
        }
    }

    pub fn setup_video_sink(
        &self,
        pipeline: &gstreamer::Element,
    ) -> Result<gstreamer_app::AppSink, PlayerError> {
        let appsink = gstreamer::ElementFactory::make("appsink")
            .build()
            .map_err(|error| PlayerError::Backend(format!("appsink creation failed: {error:?}")))?
            .downcast::<gstreamer_app::AppSink>()
            .unwrap();

        if let Some(render) = self.render.as_ref() {
            render.build_video_sink(appsink.upcast_ref::<gstreamer::Element>(), pipeline)?
        } else {
            // Prefer NV12: hardware decoders (VideoToolbox) output it natively,
            // so accepting it avoids a videoconvert colorspace pass per frame,
            // and planar frames travel to webrender's YUV rasterization path.
            // BGRA remains as the fallback for sources that cannot do NV12.
            let caps = gstreamer::Caps::builder("video/x-raw")
                .field(
                    "format",
                    gstreamer::List::new([
                        gstreamer_video::VideoFormat::Nv12.to_str(),
                        gstreamer_video::VideoFormat::Bgra.to_str(),
                    ]),
                )
                .field("pixel-aspect-ratio", gstreamer::Fraction::from((1, 1)))
                .build();

            appsink.set_caps(Some(&caps));
            pipeline.set_property("video-sink", &appsink);
        };

        Ok(appsink)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(width: u32, height: u32, stride: usize) -> gstreamer::Sample {
        let row_bytes = width as usize * 4; // BGRA => 4
        let mut data = vec![0xEE_u8; stride * height as usize];
        for (y, row) in data.chunks_exact_mut(stride).enumerate() {
            row[..row_bytes].fill(y as u8 + 1);
        }
        let mut buffer = gstreamer::Buffer::from_mut_slice(data);
        gstreamer_video::VideoMeta::add_full(
            buffer.get_mut().unwrap(),
            gstreamer_video::VideoFrameFlags::empty(),
            gstreamer_video::VideoFormat::Bgra,
            width,
            height,
            &[0],
            &[stride as i32],
        )
        .unwrap();
        let caps =
            gstreamer_video::VideoInfo::builder(gstreamer_video::VideoFormat::Bgra, width, height)
                .build()
                .unwrap()
                .to_caps()
                .unwrap();
        gstreamer::Sample::builder()
            .buffer(&buffer)
            .caps(&caps)
            .build()
    }

    fn frame_data(width: u32, height: u32, stride: usize) -> Arc<Vec<u8>> {
        gstreamer::init().unwrap();
        let render = GStreamerRender { render: None };
        render
            .get_frame_from_sample(sample(width, height, stride))
            .unwrap()
            .get_data()
    }

    // VideoToolbox aligns rows to 32 bytes, so 854 BGRA pixels end up as 3424 bytes instead of 3416.
    #[test]
    fn padded_rows_are_packed() {
        let data = frame_data(854, 4, 3424);
        assert_eq!(data.len(), 3416 * 4);
        for (y, row) in data.chunks_exact(3416).enumerate() {
            assert!(row.iter().all(|byte| *byte == y as u8 + 1), "row {y}");
        }
    }

    // Normal case with no padding, ensure it does not break.
    #[test]
    fn packed_rows_are_unchanged() {
        let data = frame_data(854, 4, 3416);
        assert_eq!(data.len(), 3416 * 4);
        for (y, row) in data.chunks_exact(3416).enumerate() {
            assert!(row.iter().all(|byte| *byte == y as u8 + 1), "row {y}");
        }
    }
}
