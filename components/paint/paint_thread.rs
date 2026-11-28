/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A dedicated paint thread that owns the WebRender [`Renderer`] and the GL
//! context it renders with, so that the expensive rasterization in
//! `Renderer::render` does not run on the embedder's UI thread.
//!
//! The thread renders into an offscreen surfman surface backed by a
//! [`SwapChain`]. After each frame the front surface is consumed on the UI
//! thread, imported as a texture into the embedder's [`RenderingContext`], and
//! blitted into the embedder framebuffer — preserving the exact externally
//! visible behavior of direct rendering. This is the same cross-context
//! surface-sharing mechanism that WebGL uses in production (surfaces are
//! backed by platform-shareable buffers, so no shared GL object namespace is
//! needed).

use std::rc::Rc;
use std::sync::Arc;

use crossbeam_channel::{Sender, unbounded};
use dpi::PhysicalSize;
use log::error;
use paint_api::rendering_context::{RenderingContext, SoftwareRenderingContext};
use paint_api::{
    WebRenderExternalImageHandlers, WebRenderExternalImageIdManager, WebRenderImageHandlerType,
};
use rustc_hash::FxHashMap;
#[cfg(feature = "webgl")]
use servo_canvas_traits::webgl::WebGLThreads;
use servo_base::id::PainterId;
use surfman::chains::SwapChain;
use surfman::{Adapter, Connection, Device};
use webrender::RenderApiSender;
use webrender_api::units::DevicePixel;
use webrender_api::{DocumentId, Epoch as WebRenderEpoch, PipelineId as WebRenderPipelineId};
use euclid::Size2D;

use crate::paint::WebRenderDebugOption;

/// A reply to a [`PaintThreadMessage::Render`] request: the current WebRender
/// epoch for each requested pipeline, sampled right after the frame was
/// rendered.
pub(crate) struct RenderReply {
    pub(crate) epochs: FxHashMap<WebRenderPipelineId, WebRenderEpoch>,
}

pub(crate) enum PaintThreadMessage {
    Render {
        document: DocumentId,
        size: Size2D<i32, DevicePixel>,
        epoch_pipelines: Vec<WebRenderPipelineId>,
        reply: Sender<RenderReply>,
    },
    Resize {
        size: PhysicalSize<u32>,
        reply: Sender<()>,
    },
    ToggleDebugFlags {
        option: WebRenderDebugOption,
        reply: Sender<()>,
    },
    Shutdown {
        reply: Sender<()>,
    },
}

/// Inputs needed to construct WebRender's external image handlers. These are
/// all thread-safe shared handles (channels and `Arc`s) cloned from `Paint`.
pub(crate) struct ExternalImageHandlerInputs {
    pub(crate) id_manager: WebRenderExternalImageIdManager,
    #[cfg(feature = "webgl")]
    pub(crate) webgl_threads: WebGLThreads,
    #[cfg(feature = "webgl")]
    pub(crate) swap_chains: surfman::chains::SwapChains<
        servo_canvas_traits::webgl::WebGLContextId,
        Device,
    >,
    #[cfg(feature = "webgl")]
    pub(crate) busy_webgl_contexts_map: webgl::webgl_thread::WebGLContextBusyMap,
    #[cfg(feature = "webgl")]
    pub(crate) readback_map: webgl::webgl_thread::WebGLContextReadbackMap,
    #[cfg(feature = "webgpu")]
    pub(crate) webgpu_image_map: webgpu::canvas_context::WebGpuExternalImageMap,
}

struct StartupSuccess {
    render_api_sender: RenderApiSender,
    swap_chain: SwapChain<Device>,
}

/// A handle to a running paint thread. All methods are synchronous
/// rendezvous round-trips for now; making frame consumption asynchronous is a
/// later step that preserves this exact structure.
pub(crate) struct PaintThreadHandle {
    sender: Sender<PaintThreadMessage>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    swap_chain: SwapChain<Device>,
}

impl PaintThreadHandle {
    /// Spawn the paint thread, creating its GL context, surface, swap chain,
    /// and WebRender renderer there. Blocks until startup completes. The
    /// returned [`RenderApiSender`] is used to create the [`RenderApi`] on the
    /// calling thread.
    pub(crate) fn spawn(
        connection: Connection,
        adapter: Adapter,
        size: PhysicalSize<u32>,
        painter_id: PainterId,
        paint_proxy: paint_api::PaintProxy,
        external_image_inputs: ExternalImageHandlerInputs,
    ) -> Result<(Self, RenderApiSender), String> {
        let (sender, receiver) = unbounded();
        let (startup_sender, startup_receiver) = unbounded();
        let join_handle = std::thread::Builder::new()
            .name(format!("PaintThread{painter_id:?}"))
            .spawn(move || {
                paint_thread_main(
                    connection,
                    adapter,
                    size,
                    painter_id,
                    paint_proxy,
                    external_image_inputs,
                    receiver,
                    startup_sender,
                );
            })
            .map_err(|error| format!("Failed to spawn paint thread: {error:?}"))?;

        let startup_result = startup_receiver
            .recv()
            .map_err(|_| "Paint thread died during startup".to_owned())?;
        let StartupSuccess {
            render_api_sender,
            swap_chain,
        } = startup_result?;

        Ok((
            PaintThreadHandle {
                sender,
                join_handle: Some(join_handle),
                swap_chain,
            },
            render_api_sender,
        ))
    }

    pub(crate) fn swap_chain(&self) -> SwapChain<Device> {
        self.swap_chain.clone()
    }

    /// Render a frame on the paint thread, blocking until it is complete and
    /// the frame has been swapped into the front buffer of the swap chain.
    pub(crate) fn render(
        &self,
        document: DocumentId,
        size: Size2D<i32, DevicePixel>,
        epoch_pipelines: Vec<WebRenderPipelineId>,
    ) -> Option<RenderReply> {
        let (reply_sender, reply_receiver) = unbounded();
        self.sender
            .send(PaintThreadMessage::Render {
                document,
                size,
                epoch_pipelines,
                reply: reply_sender,
            })
            .ok()?;
        reply_receiver.recv().ok()
    }

    pub(crate) fn resize(&self, size: PhysicalSize<u32>) {
        let (reply_sender, reply_receiver) = unbounded();
        if self
            .sender
            .send(PaintThreadMessage::Resize {
                size,
                reply: reply_sender,
            })
            .is_ok()
        {
            let _ = reply_receiver.recv();
        }
    }

    pub(crate) fn toggle_debug_flags(&self, option: WebRenderDebugOption) {
        let (reply_sender, reply_receiver) = unbounded();
        if self
            .sender
            .send(PaintThreadMessage::ToggleDebugFlags {
                option,
                reply: reply_sender,
            })
            .is_ok()
        {
            let _ = reply_receiver.recv();
        }
    }

    /// Shut down the paint thread, deinitializing the renderer with its GL
    /// context current, and join the thread.
    pub(crate) fn shutdown(&mut self) {
        let (reply_sender, reply_receiver) = unbounded();
        if self
            .sender
            .send(PaintThreadMessage::Shutdown {
                reply: reply_sender,
            })
            .is_ok()
        {
            let _ = reply_receiver.recv();
        }
        if let Some(join_handle) = self.join_handle.take() {
            let _ = join_handle.join();
        }
    }
}

fn paint_thread_main(
    connection: Connection,
    adapter: Adapter,
    size: PhysicalSize<u32>,
    painter_id: PainterId,
    paint_proxy: paint_api::PaintProxy,
    external_image_inputs: ExternalImageHandlerInputs,
    receiver: crossbeam_channel::Receiver<PaintThreadMessage>,
    startup_sender: Sender<Result<StartupSuccess, String>>,
) {
    let rendering_context =
        match SoftwareRenderingContext::new_with_connection_and_adapter(&connection, &adapter, size)
        {
            Ok(rendering_context) => Rc::new(rendering_context),
            Err(error) => {
                let _ = startup_sender
                    .send(Err(format!("Failed to create paint thread GL context: {error:?}")));
                return;
            },
        };

    if let Err(error) = rendering_context.make_current() {
        let _ = startup_sender.send(Err(format!(
            "Failed to make paint thread GL context current: {error:?}"
        )));
        return;
    }

    let webrender_gl = rendering_context.gleam_gl_api();

    let mut external_image_handlers = Box::new(WebRenderExternalImageHandlers::new(
        external_image_inputs.id_manager,
    ));

    #[cfg(feature = "webgl")]
    {
        let image_handler = Box::new(crate::webrender_external_images::WebGLExternalImages::new(
            external_image_inputs.webgl_threads,
            rendering_context.clone(),
            external_image_inputs.swap_chains,
            external_image_inputs.busy_webgl_contexts_map,
            external_image_inputs.readback_map,
        ));
        external_image_handlers.set_handler(image_handler, WebRenderImageHandlerType::WebGl);
    }

    #[cfg(feature = "webgpu")]
    external_image_handlers.set_handler(
        Box::new(webgpu::WebGpuExternalImages::new(
            external_image_inputs.webgpu_image_map,
        )),
        WebRenderImageHandlerType::WebGpu,
    );

    media::WindowGLContext::initialize_image_handler(&mut external_image_handlers);

    rendering_context.prepare_for_rendering();
    let clear_color = servo_config::pref!(shell_background_color_rgba);
    let clear_color = webrender_api::ColorF::new(
        clear_color[0] as f32,
        clear_color[1] as f32,
        clear_color[2] as f32,
        clear_color[3] as f32,
    );

    use gleam::gl::RENDERER;
    let is_software_webrender = webrender_gl
        .get_string(RENDERER)
        .starts_with("Software WebRender");
    let upload_method = if webrender_gl.get_string(RENDERER).starts_with("ANGLE") ||
        is_software_webrender
    {
        webrender::UploadMethod::Immediate
    } else {
        webrender::UploadMethod::PixelBuffer(webrender::ONE_TIME_USAGE_HINT)
    };
    let worker_threads = std::thread::available_parallelism()
        .map(|i| i.get())
        .unwrap_or(servo_config::pref!(thread_pool_fallback_workers) as usize)
        .min(servo_config::pref!(thread_pool_webrender_workers_max).max(1) as usize);
    let workers = Some(Arc::new(
        rayon::ThreadPoolBuilder::new()
            .num_threads(worker_threads)
            .thread_name(|idx| format!("WRWorker#{}", idx))
            .start_handler(|_| {
                servo_base::threadboost::boost_thread(
                    servo_base::threadboost::ThreadPriority::Elevated,
                    servo_base::threadboost::BoostAffinity::Boost,
                )
            })
            .build()
            .expect("Unable to initialize WebRender worker pool."),
    ));

    let (mut renderer, render_api_sender) = webrender::create_webrender_instance(
        webrender_gl.clone(),
        Box::new(crate::render_notifier::RenderNotifier::new(painter_id, paint_proxy)),
        webrender::WebRenderOptions {
            use_optimized_shaders: true,
            resource_override_path: servo_config::opts::get().shaders_path.clone(),
            debug_flags: webrender::DebugFlags::empty(),
            precache_flags: if servo_config::pref!(gfx_precache_shaders) {
                webrender::ShaderPrecacheFlags::FULL_COMPILE
            } else {
                webrender::ShaderPrecacheFlags::empty()
            },
            enable_aa: servo_config::pref!(gfx_text_antialiasing_enabled),
            enable_subpixel_aa: servo_config::pref!(gfx_subpixel_text_antialiasing_enabled),
            allow_texture_swizzling: servo_config::pref!(gfx_texture_swizzling_enabled),
            enable_dithering: !is_software_webrender,
            clear_color,
            clear_caches_with_quads: !is_software_webrender,
            upload_method,
            workers,
            size_of_op: Some(servo_allocator::usable_size),
            namespace_alloc_by_client: true,
            render_backend_hooks: Some(Box::new(BoostWebRenderThread)),
            scene_builder_hooks: Some(Box::new(BoostWebRenderThread)),
            shared_font_namespace: Some(painter_id.into()),
            ..Default::default()
        },
        None,
    )
    .expect("Unable to initialize WebRender.");

    renderer.set_external_image_handler(external_image_handlers);

    let swap_chain = rendering_context.swap_chain();
    if startup_sender
        .send(Ok(StartupSuccess {
            render_api_sender,
            swap_chain,
        }))
        .is_err()
    {
        return;
    }

    // Clear the initial surface, as `Painter::new` does for direct rendering.
    clear_background(&*rendering_context, &webrender_gl);

    while let Ok(message) = receiver.recv() {
        match message {
            PaintThreadMessage::Render {
                document,
                size,
                epoch_pipelines,
                reply,
            } => {
                if let Err(error) = rendering_context.make_current() {
                    error!("Failed to make the rendering context current: {error:?}");
                }
                rendering_context.prepare_for_rendering();

                renderer.update();
                clear_background(&*rendering_context, &webrender_gl);
                renderer.render(size, 0 /* buffer_age */).ok();

                // Ensure all GL work is complete before the surface is
                // consumed on another thread. Correctness first: pipelining
                // is a later optimization.
                webrender_gl.finish();
                rendering_context.present();

                let epochs = epoch_pipelines
                    .into_iter()
                    .filter_map(|pipeline_id| {
                        renderer
                            .current_epoch(document, pipeline_id)
                            .map(|epoch| (pipeline_id, epoch))
                    })
                    .collect();

                let _ = reply.send(RenderReply { epochs });
            },
            PaintThreadMessage::Resize { size, reply } => {
                if let Err(error) = rendering_context.make_current() {
                    error!("Failed to make the rendering context current: {error:?}");
                }
                rendering_context.resize(size);
                let _ = reply.send(());
            },
            PaintThreadMessage::ToggleDebugFlags { option, reply } => {
                let mut flags = renderer.get_debug_flags();
                let flag = match option {
                    WebRenderDebugOption::Profiler => {
                        webrender::DebugFlags::PROFILER_DBG |
                            webrender::DebugFlags::GPU_TIME_QUERIES |
                            webrender::DebugFlags::GPU_SAMPLE_QUERIES
                    },
                    WebRenderDebugOption::TextureCacheDebug => {
                        webrender::DebugFlags::TEXTURE_CACHE_DBG
                    },
                    WebRenderDebugOption::RenderTargetDebug => {
                        webrender::DebugFlags::RENDER_TARGET_DBG
                    },
                };
                flags.toggle(flag);
                renderer.set_debug_flags(flags);
                let _ = reply.send(());
            },
            PaintThreadMessage::Shutdown { reply } => {
                if let Err(error) = rendering_context.make_current() {
                    error!("Failed to make the rendering context current: {error:?}");
                }
                renderer.deinit();
                let _ = reply.send(());
                return;
            },
        }
    }
}

fn clear_background(rendering_context: &SoftwareRenderingContext, gl: &Rc<dyn gleam::gl::Gl>) {
    rendering_context.prepare_for_rendering();
    let color = servo_config::pref!(shell_background_color_rgba);
    gl.clear_color(
        color[0] as f32,
        color[1] as f32,
        color[2] as f32,
        color[3] as f32,
    );
    gl.clear(gleam::gl::COLOR_BUFFER_BIT);
}

/// Hook implementation to boost webrender thread priority. Mirrors the one in
/// `painter.rs` used for direct rendering.
struct BoostWebRenderThread;

impl webrender::RenderBackendHooks for BoostWebRenderThread {
    fn init_thread(&self) {
        servo_base::threadboost::boost_thread(
            servo_base::threadboost::ThreadPriority::Elevated,
            servo_base::threadboost::BoostAffinity::Boost,
        );
    }
}

impl webrender::SceneBuilderHooks for BoostWebRenderThread {
    fn register(&self) {
        servo_base::threadboost::boost_thread(
            servo_base::threadboost::ThreadPriority::Elevated,
            servo_base::threadboost::BoostAffinity::Boost,
        );
    }

    fn pre_scene_build(&self) {}

    fn pre_scene_swap(&self) {}

    fn post_scene_swap(
        &self,
        _document_id: &Vec<DocumentId>,
        _info: webrender::PipelineInfo,
        _schedule_frame: bool,
    ) {
    }

    fn post_resource_update(&self, _document_ids: &Vec<DocumentId>) {}

    fn post_empty_scene_build(&self) {}

    fn poke(&self) {}

    fn deregister(&self) {}
}
