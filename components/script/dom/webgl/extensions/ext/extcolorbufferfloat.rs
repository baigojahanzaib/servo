/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::reflector::{Reflector, reflect_dom_object};
use servo_canvas_traits::webgl::WebGLVersion;

use super::{WebGLExtension, WebGLExtensionSpec, WebGLExtensions};
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::webgl::webglrenderingcontext::WebGLRenderingContext;

#[dom_struct]
pub(crate) struct EXTColorBufferFloat {
    reflector_: Reflector,
}

impl EXTColorBufferFloat {
    fn new_inherited() -> EXTColorBufferFloat {
        Self {
            reflector_: Reflector::new(),
        }
    }
}

impl WebGLExtension for EXTColorBufferFloat {
    type Extension = EXTColorBufferFloat;
    fn new(cx: &mut JSContext, ctx: &WebGLRenderingContext) -> DomRoot<EXTColorBufferFloat> {
        reflect_dom_object(cx, Box::new(Self::new_inherited()), &*ctx.global())
    }

    fn spec() -> WebGLExtensionSpec {
        WebGLExtensionSpec::Specific(WebGLVersion::WebGL2)
    }

    fn is_supported(ext: &WebGLExtensions) -> bool {
        // On desktop GL, 32-bit float color buffers have been core since GL 3.0 —
        // and every desktop GL context capable of backing a WebGL2 context is newer
        // than that. Core-profile contexts do not advertise the fact through any
        // extension string, so requiring GL_EXT_color_buffer_float here would
        // wrongly disable the feature on exactly the contexts that support it best.
        // (GLES3 needs the extension proper.)
        !ext.is_gles() || ext.supports_gl_extension("GL_EXT_color_buffer_float")
    }

    fn enable(_ext: &WebGLExtensions) {}

    fn name() -> &'static str {
        "EXT_color_buffer_float"
    }
}
