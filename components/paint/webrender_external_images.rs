/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::rc::Rc;

use euclid::default::Size2D;
use log::debug;
use paint_api::rendering_context::RenderingContext;
use paint_api::{ExternalImageSource, WebRenderExternalImageApi};
use rustc_hash::FxHashMap;
use servo_canvas_traits::webgl::{WebGLContextId, WebGLThreads};
use surfman::chains::{SwapChainAPI, SwapChains, SwapChainsAPI};
use surfman::{Device, SurfaceTexture};
use webgl::webgl_thread::{WebGLContextBusyMap, WebGLContextReadbackMap};

/// Bridge between the webrender::ExternalImage callbacks and the WebGLThreads.
pub struct WebGLExternalImages {
    webgl_threads: WebGLThreads,
    rendering_context: Rc<dyn RenderingContext>,
    swap_chains: SwapChains<WebGLContextId, Device>,
    busy_webgl_context_map: WebGLContextBusyMap,
    readback_map: WebGLContextReadbackMap,
    locked_front_buffers: FxHashMap<WebGLContextId, SurfaceTexture>,
    /// Frames copied out of the shared readback map at lock time, so the
    /// raw-data borrow remains valid and stable until unlock.
    locked_readbacks: FxHashMap<WebGLContextId, (Vec<u8>, Size2D<i32>)>,
}

impl WebGLExternalImages {
    pub fn new(
        webgl_threads: WebGLThreads,
        rendering_context: Rc<dyn RenderingContext>,
        swap_chains: SwapChains<WebGLContextId, Device>,
        busy_webgl_context_map: WebGLContextBusyMap,
        readback_map: WebGLContextReadbackMap,
    ) -> Self {
        Self {
            webgl_threads,
            rendering_context,
            swap_chains,
            busy_webgl_context_map,
            readback_map,
            locked_front_buffers: FxHashMap::default(),
            locked_readbacks: FxHashMap::default(),
        }
    }

    fn lock_swap_chain(&mut self, id: WebGLContextId) -> Option<(u32, Size2D<i32>)> {
        debug!("... locking chain {:?}", id);

        {
            let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
            *busy_webgl_context_map.entry(id).or_default() += 1;
        }

        let front_buffer = self.swap_chains.get(id)?.take_surface()?;
        let (surface_texture, gl_texture, size) =
            self.rendering_context.create_texture(front_buffer)?;
        self.locked_front_buffers.insert(id, surface_texture);

        Some((gl_texture, size))
    }

    fn unlock_swap_chain(&mut self, id: WebGLContextId) -> Option<()> {
        debug!("... unlocked chain {:?}", id);

        if let Some(locked_front_buffer) = self.locked_front_buffers.remove(&id) &&
            let Some(locked_front_buffer) =
                self.rendering_context.destroy_texture(locked_front_buffer)
        {
            self.swap_chains
                .get(id)
                .expect("Should always have a SwapChain for a busy WebGLContext")
                .recycle_surface(locked_front_buffer);
        }

        {
            let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
            *busy_webgl_context_map.entry(id).or_insert(1) -= 1;
        }

        let _ = self.webgl_threads.finished_rendering_to_context(id);

        Some(())
    }
}

impl WebRenderExternalImageApi for WebGLExternalImages {
    fn lock(&mut self, id: u64) -> (ExternalImageSource<'_>, Size2D<i32>) {
        // A software-compositing painter has no surfman connection: GL
        // textures cannot cross into the CPU rasterizer, so frames arrive as
        // memory read back by the WebGL thread at swap time.
        if self.rendering_context.connection().is_none() {
            let context_id = WebGLContextId(id);
            {
                let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
                *busy_webgl_context_map.entry(context_id).or_default() += 1;
            }
            let frame = self.readback_map.read().get(&context_id).map(|frame| {
                (frame.pixels.clone(), Size2D::new(frame.width, frame.height))
            });
            if let Some((pixels, size)) = frame {
                self.locked_readbacks.insert(context_id, (pixels, size));
                let (pixels, size) = self.locked_readbacks.get(&context_id).unwrap();
                return (ExternalImageSource::RawData(pixels), *size);
            }
            return (ExternalImageSource::Invalid, Size2D::zero());
        }
        let (texture_id, size) = self.lock_swap_chain(WebGLContextId(id)).unwrap_or_default();
        (ExternalImageSource::NativeTexture(texture_id), size)
    }

    fn unlock(&mut self, id: u64) {
        if self.rendering_context.connection().is_none() {
            let context_id = WebGLContextId(id);
            self.locked_readbacks.remove(&context_id);
            {
                let mut busy_webgl_context_map = self.busy_webgl_context_map.write();
                *busy_webgl_context_map.entry(context_id).or_insert(1) -= 1;
            }
            let _ = self.webgl_threads.finished_rendering_to_context(context_id);
            return;
        }
        self.unlock_swap_chain(WebGLContextId(id));
    }
}
