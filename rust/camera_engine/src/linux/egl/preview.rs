// Draws decoded NV12 surfaces with OpenGL ES 2. Each VA-API surface is imported once as an
// EGLImage over its DMA-BUF and sampled as an external texture: the GPU converts YUV to RGB
// while sampling, the frame is never copied
use std::{
    collections::HashMap,
    ffi::{c_char, c_void, CStr, CString},
    path::Path,
    ptr,
};

use super::api::*;
use crate::linux::vaapi::{sys::VASurfaceID, DecodedFrame};

const VERTEX_SHADER: &str = "
attribute vec2 position;
attribute vec2 uv;
varying vec2 v_uv;
void main() {
    gl_Position = vec4(position, 0.0, 1.0);
    v_uv = uv;
}
";

const FRAGMENT_SHADER: &str = "
#extension GL_OES_EGL_image_external : require
precision mediump float;
uniform samplerExternalOES frame;
varying vec2 v_uv;
void main() {
    gl_FragColor = texture2D(frame, v_uv);
}
";

const ATTRIBUTE_POSITION: u32 = 0;
const ATTRIBUTE_UV: u32 = 1;
// Pools are small (refs + reorder + consumers); more entries mean a leak somewhere
const MAX_CACHED_IMAGES: usize = 64;

enum Target {
    // Output to an X11 window owned by the UI (AWT Canvas), through our own X connection
    X11 {
        xdisplay: *mut c_void,
        surface: EglSurface,
    },
    // Output to a texture, read back by tests
    #[cfg_attr(not(test), allow(dead_code))]
    Offscreen {
        framebuffer: u32,
        texture: u32,
        width: i32,
        height: i32,
    },
}

struct ImportedSurface {
    image: EglImage,
    texture: u32,
}

pub(crate) struct EglPreview {
    api: &'static Api,
    display: EglDisplay,
    context: EglContext,
    target: Target,
    program: u32,
    images: HashMap<(usize, VASurfaceID), ImportedSurface>,
}

// EGL objects are moved to the render thread once and only used there
unsafe impl Send for EglPreview {}

impl EglPreview {
    pub fn for_x11_window(window: u64) -> Result<Self, String> {
        let api = Api::get()?;
        let xlib = xlib()?;
        unsafe {
            let xdisplay = (xlib.open_display)(ptr::null());
            if xdisplay.is_null() {
                return Err(
                    "cannot connect to the X server (no X11 or XWayland session?)".to_string(),
                );
            }
            let result = (|| {
                let display =
                    (api.egl.get_platform_display)(EGL_PLATFORM_X11_KHR, xdisplay, ptr::null());
                let (config, context) = setup(api, display, EGL_WINDOW_BIT)?;
                // The X11 platform takes a pointer to the Window XID
                let mut xid = window as std::ffi::c_ulong;
                let surface = (api.egl.create_platform_window_surface)(
                    display,
                    config,
                    &mut xid as *mut _ as *mut c_void,
                    ptr::null(),
                );
                if surface.is_null() {
                    let error = api.egl_error("eglCreatePlatformWindowSurface");
                    (api.egl.destroy_context)(display, context);
                    (api.egl.terminate)(display);
                    return Err(error);
                }
                Ok((display, context, surface))
            })();
            let (display, context, surface) = match result {
                Ok(objects) => objects,
                Err(e) => {
                    (xlib.close_display)(xdisplay);
                    return Err(e);
                }
            };
            Self::finish(api, display, context, Target::X11 { xdisplay, surface })
        }
    }

    // Draws into a texture of the given size on the GPU behind `render_node`
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn offscreen(render_node: &Path, width: i32, height: i32) -> Result<Self, String> {
        let api = Api::get()?;
        unsafe {
            let display = device_display(api, render_node)?;
            let (_, context) = setup(api, display, EGL_PBUFFER_BIT)?;
            if (api.egl.make_current)(display, ptr::null_mut(), ptr::null_mut(), context) == 0 {
                return Err(api.egl_error("eglMakeCurrent"));
            }
            let gl = &api.gl;
            let (mut framebuffer, mut texture) = (0, 0);
            (gl.gen_textures)(1, &mut texture);
            (gl.bind_texture)(GL_TEXTURE_2D, texture);
            (gl.tex_image_2d)(
                GL_TEXTURE_2D,
                0,
                GL_RGBA as i32,
                width,
                height,
                0,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                ptr::null(),
            );
            (gl.gen_framebuffers)(1, &mut framebuffer);
            (gl.bind_framebuffer)(GL_FRAMEBUFFER, framebuffer);
            (gl.framebuffer_texture_2d)(
                GL_FRAMEBUFFER,
                GL_COLOR_ATTACHMENT0,
                GL_TEXTURE_2D,
                texture,
                0,
            );
            if (gl.check_framebuffer_status)(GL_FRAMEBUFFER) != GL_FRAMEBUFFER_COMPLETE {
                return Err("offscreen framebuffer is incomplete".to_string());
            }
            (api.egl.make_current)(display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            Self::finish(
                api,
                display,
                context,
                Target::Offscreen {
                    framebuffer,
                    texture,
                    width,
                    height,
                },
            )
        }
    }

    // The context is never made current here: the calling thread may be the UI thread, which
    // has Skiko's GLX context current, and libglvnd refuses an EGL context on a thread that
    // holds one of another API (EGL_BAD_ACCESS). The render thread builds the GL objects
    unsafe fn finish(
        api: &'static Api,
        display: EglDisplay,
        context: EglContext,
        target: Target,
    ) -> Result<Self, String> {
        Ok(EglPreview {
            api,
            display,
            context,
            target,
            program: 0,
            images: HashMap::new(),
        })
    }

    // Must be called on the thread that draws, before the first draw
    pub fn make_current(&mut self) -> Result<(), String> {
        let surface = match self.target {
            Target::X11 { surface, .. } => surface,
            Target::Offscreen { .. } => ptr::null_mut(),
        };
        unsafe {
            if (self.api.egl.make_current)(self.display, surface, surface, self.context) == 0 {
                return Err(self.api.egl_error("eglMakeCurrent"));
            }
            if let Target::X11 { .. } = self.target {
                // Paced by the display like the Windows swap chain (Present with sync 1)
                (self.api.egl.swap_interval)(self.display, 1);
            }
            if self.program == 0 {
                self.program = build_program(self.api)?;
            }
        }
        Ok(())
    }

    fn release_current(&self) {
        unsafe {
            (self.api.egl.make_current)(
                self.display,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
    }

    // Current output size in pixels: the window follows the AWT Canvas on its own
    fn target_size(&self) -> (i32, i32) {
        match self.target {
            Target::X11 { surface, .. } => unsafe {
                let (mut width, mut height) = (0, 0);
                (self.api.egl.query_surface)(self.display, surface, EGL_WIDTH, &mut width);
                (self.api.egl.query_surface)(self.display, surface, EGL_HEIGHT, &mut height);
                (width, height)
            },
            Target::Offscreen { width, height, .. } => (width, height),
        }
    }

    // Draws the frame letterboxed (black bars) into the whole output and shows it
    pub fn draw(&mut self, frame: &DecodedFrame) -> Result<(), String> {
        let texture = self.texture_for(frame)?;
        frame.sync()?;

        let gl = &self.api.gl;
        let (width, height) = self.target_size();
        let (coded_width, coded_height) = frame.coded_size();
        let (x, y, visible_width, visible_height) = frame.visible;
        let (u0, v0) = (
            x as f32 / coded_width as f32,
            y as f32 / coded_height as f32,
        );
        let (u1, v1) = (
            (x + visible_width) as f32 / coded_width as f32,
            (y + visible_height) as f32 / coded_height as f32,
        );
        // Triangle strip: top-left, bottom-left, top-right, bottom-right. The first image
        // row is the top of the picture, at v0
        let positions: [f32; 8] = [-1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0, -1.0];
        let uvs: [f32; 8] = [u0, v0, u0, v1, u1, v0, u1, v1];

        unsafe {
            (gl.viewport)(0, 0, width, height);
            (gl.clear_color)(0.0, 0.0, 0.0, 1.0);
            (gl.clear)(GL_COLOR_BUFFER_BIT);

            let (vx, vy, vw, vh) = letterbox(visible_width, visible_height, width, height);
            (gl.viewport)(vx, vy, vw, vh);
            (gl.use_program)(self.program);
            (gl.active_texture)(GL_TEXTURE0);
            (gl.bind_texture)(GL_TEXTURE_EXTERNAL_OES, texture);
            (gl.vertex_attrib_pointer)(
                ATTRIBUTE_POSITION,
                2,
                GL_FLOAT,
                0,
                0,
                positions.as_ptr() as _,
            );
            (gl.vertex_attrib_pointer)(ATTRIBUTE_UV, 2, GL_FLOAT, 0, 0, uvs.as_ptr() as _);
            (gl.enable_vertex_attrib_array)(ATTRIBUTE_POSITION);
            (gl.enable_vertex_attrib_array)(ATTRIBUTE_UV);
            (gl.draw_arrays)(GL_TRIANGLE_STRIP, 0, 4);

            match self.target {
                Target::X11 { surface, .. } => {
                    if (self.api.egl.swap_buffers)(self.display, surface) == 0 {
                        return Err(self.api.egl_error("eglSwapBuffers"));
                    }
                }
                Target::Offscreen { .. } => (gl.finish)(),
            }
            match (gl.get_error)() {
                0 => Ok(()),
                error => Err(format!("OpenGL error {error:#x} while drawing")),
            }
        }
    }

    // RGBA rows, top row first (offscreen target only)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn read_pixels(&self) -> Vec<u8> {
        let Target::Offscreen { width, height, .. } = self.target else {
            return Vec::new();
        };
        let mut pixels = vec![0u8; (width * height * 4) as usize];
        unsafe {
            (self.api.gl.read_pixels)(
                0,
                0,
                width,
                height,
                GL_RGBA,
                GL_UNSIGNED_BYTE,
                pixels.as_mut_ptr() as _,
            );
        }
        // OpenGL reads bottom-up
        let row = (width * 4) as usize;
        pixels.chunks(row).rev().flatten().copied().collect()
    }

    fn texture_for(&mut self, frame: &DecodedFrame) -> Result<u32, String> {
        let key = frame.surface_key();
        if let Some(imported) = self.images.get(&key) {
            return Ok(imported.texture);
        }
        // Surfaces of an older pool (previous resolution) are gone
        if self.images.keys().any(|(pool, _)| *pool != key.0)
            || self.images.len() >= MAX_CACHED_IMAGES
        {
            self.clear_images();
        }

        let exported = frame.export()?;
        let (coded_width, coded_height) = frame.coded_size();
        let mut attributes = vec![
            EGL_WIDTH,
            coded_width as EglInt,
            EGL_HEIGHT,
            coded_height as EglInt,
            EGL_LINUX_DRM_FOURCC_EXT,
            exported.drm_format as EglInt,
            EGL_YUV_COLOR_SPACE_HINT_EXT,
            if frame.bt709 {
                EGL_ITU_REC709_EXT
            } else {
                EGL_ITU_REC601_EXT
            },
            EGL_SAMPLE_RANGE_HINT_EXT,
            if frame.full_range {
                EGL_YUV_FULL_RANGE_EXT
            } else {
                EGL_YUV_NARROW_RANGE_EXT
            },
        ];
        for (plane, &(object, offset, pitch)) in exported.planes.iter().enumerate().take(2) {
            let object = &exported.objects[object as usize];
            attributes.extend([
                EGL_DMA_BUF_PLANE_FD_EXT[plane],
                std::os::fd::AsRawFd::as_raw_fd(&object.fd),
                EGL_DMA_BUF_PLANE_OFFSET_EXT[plane],
                offset as EglInt,
                EGL_DMA_BUF_PLANE_PITCH_EXT[plane],
                pitch as EglInt,
                EGL_DMA_BUF_PLANE_MODIFIER_LO_EXT[plane],
                object.modifier as u32 as EglInt,
                EGL_DMA_BUF_PLANE_MODIFIER_HI_EXT[plane],
                (object.modifier >> 32) as u32 as EglInt,
            ]);
        }
        attributes.push(EGL_NONE);

        let api = self.api;
        unsafe {
            // EGL keeps its own reference to the buffers: the exported descriptors close
            // when `exported` drops at the end of this function
            let image = (api.ext.create_image)(
                self.display,
                ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                ptr::null_mut(),
                attributes.as_ptr(),
            );
            if image.is_null() {
                return Err(api.egl_error("eglCreateImageKHR (DMA-BUF import)"));
            }
            let gl = &api.gl;
            let mut texture = 0;
            (gl.gen_textures)(1, &mut texture);
            (gl.bind_texture)(GL_TEXTURE_EXTERNAL_OES, texture);
            (gl.tex_parameter_i)(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            (gl.tex_parameter_i)(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            (gl.tex_parameter_i)(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            (gl.tex_parameter_i)(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
            (api.ext.image_target_texture)(GL_TEXTURE_EXTERNAL_OES, image);
            self.images.insert(key, ImportedSurface { image, texture });
            Ok(texture)
        }
    }

    fn clear_images(&mut self) {
        for (_, imported) in self.images.drain() {
            unsafe {
                (self.api.gl.delete_textures)(1, &imported.texture);
                (self.api.ext.destroy_image)(self.display, imported.image);
            }
        }
    }
}

impl Drop for EglPreview {
    fn drop(&mut self) {
        let api = self.api;
        // GL objects can only be deleted with the context current, and exist only once the
        // render thread made it current
        if self.program != 0 && self.make_current().is_ok() {
            self.clear_images();
            unsafe {
                (api.gl.delete_program)(self.program);
                if let Target::Offscreen {
                    framebuffer,
                    texture,
                    ..
                } = self.target
                {
                    (api.gl.delete_framebuffers)(1, &framebuffer);
                    (api.gl.delete_textures)(1, &texture);
                }
            }
        }
        self.release_current();
        unsafe {
            if let Target::X11 { surface, .. } = self.target {
                (api.egl.destroy_surface)(self.display, surface);
            }
            (api.egl.destroy_context)(self.display, self.context);
            (api.egl.terminate)(self.display);
            if let Target::X11 { xdisplay, .. } = self.target {
                if let Ok(xlib) = xlib() {
                    (xlib.close_display)(xdisplay);
                }
            }
        }
    }
}

// Initializes the display and creates an ES 2 context able to draw on `surface_type`
unsafe fn setup(
    api: &Api,
    display: EglDisplay,
    surface_type: EglInt,
) -> Result<(EglConfig, EglContext), String> {
    if display.is_null() {
        return Err("eglGetPlatformDisplay failed".to_string());
    }
    let (mut major, mut minor) = (0, 0);
    if (api.egl.initialize)(display, &mut major, &mut minor) == 0 {
        return Err(api.egl_error("eglInitialize"));
    }
    let result = (|| {
        let extensions =
            CStr::from_ptr((api.egl.query_string)(display, EGL_EXTENSIONS)).to_string_lossy();
        for needed in [
            "EGL_EXT_image_dma_buf_import",
            "EGL_EXT_image_dma_buf_import_modifiers",
        ] {
            if !extensions
                .split_whitespace()
                .any(|extension| extension == needed)
            {
                return Err(format!("EGL lacks {needed}"));
            }
        }
        if (api.egl.bind_api)(EGL_OPENGL_ES_API) == 0 {
            return Err(api.egl_error("eglBindAPI"));
        }

        let attributes = [
            EGL_SURFACE_TYPE,
            surface_type,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_NONE,
        ];
        let mut config = ptr::null_mut();
        let mut count = 0;
        if (api.egl.choose_config)(display, attributes.as_ptr(), &mut config, 1, &mut count) == 0
            || count == 0
        {
            return Err("no EGL config for an OpenGL ES 2 RGB8 surface".to_string());
        }
        let context_attributes = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let context = (api.egl.create_context)(
            display,
            config,
            ptr::null_mut(),
            context_attributes.as_ptr(),
        );
        if context.is_null() {
            return Err(api.egl_error("eglCreateContext"));
        }
        Ok((config, context))
    })();
    if result.is_err() {
        (api.egl.terminate)(display);
    }
    result
}

// EGL display on a specific GPU (EGL_EXT_platform_device), to draw where VA-API decoded
#[cfg_attr(not(test), allow(dead_code))]
unsafe fn device_display(api: &Api, render_node: &Path) -> Result<EglDisplay, String> {
    let (Some(query_devices), Some(query_string)) =
        (api.ext.query_devices, api.ext.query_device_string)
    else {
        return Err("EGL cannot enumerate devices (EGL_EXT_device_enumeration)".to_string());
    };
    let mut devices = [ptr::null_mut(); 16];
    let mut count = 0;
    if query_devices(devices.len() as EglInt, devices.as_mut_ptr(), &mut count) == 0 {
        return Err(api.egl_error("eglQueryDevicesEXT"));
    }
    let wanted = render_node.to_string_lossy();
    let device = devices[..count as usize]
        .iter()
        .copied()
        .find(|&device| {
            let node = query_string(device, EGL_DRM_RENDER_NODE_FILE_EXT);
            !node.is_null() && CStr::from_ptr(node).to_string_lossy() == wanted
        })
        .ok_or_else(|| format!("no EGL device for {wanted}"))?;
    Ok((api.egl.get_platform_display)(
        EGL_PLATFORM_DEVICE_EXT,
        device,
        ptr::null(),
    ))
}

unsafe fn build_program(api: &Api) -> Result<u32, String> {
    let gl = &api.gl;
    let vertex = compile(api, GL_VERTEX_SHADER, VERTEX_SHADER)?;
    let fragment = match compile(api, GL_FRAGMENT_SHADER, FRAGMENT_SHADER) {
        Ok(shader) => shader,
        Err(e) => {
            (gl.delete_shader)(vertex);
            return Err(e);
        }
    };
    let program = (gl.create_program)();
    (gl.attach_shader)(program, vertex);
    (gl.attach_shader)(program, fragment);
    (gl.bind_attrib_location)(program, ATTRIBUTE_POSITION, c"position".as_ptr());
    (gl.bind_attrib_location)(program, ATTRIBUTE_UV, c"uv".as_ptr());
    (gl.link_program)(program);
    (gl.delete_shader)(vertex);
    (gl.delete_shader)(fragment);

    let mut linked = 0;
    (gl.get_program_iv)(program, GL_LINK_STATUS, &mut linked);
    if linked == 0 {
        let mut log = [0 as c_char; 1024];
        (gl.get_program_info_log)(program, log.len() as i32, ptr::null_mut(), log.as_mut_ptr());
        (gl.delete_program)(program);
        return Err(format!(
            "shader link failed: {}",
            CStr::from_ptr(log.as_ptr()).to_string_lossy()
        ));
    }
    (gl.use_program)(program);
    (gl.uniform_1i)((gl.get_uniform_location)(program, c"frame".as_ptr()), 0);
    Ok(program)
}

unsafe fn compile(api: &Api, kind: u32, source: &str) -> Result<u32, String> {
    let gl = &api.gl;
    let shader = (gl.create_shader)(kind);
    let source = CString::new(source).expect("shader source has no NUL");
    (gl.shader_source)(shader, 1, &source.as_ptr(), ptr::null());
    (gl.compile_shader)(shader);
    let mut compiled = 0;
    (gl.get_shader_iv)(shader, GL_COMPILE_STATUS, &mut compiled);
    if compiled == 0 {
        let mut log = [0 as c_char; 1024];
        (gl.get_shader_info_log)(shader, log.len() as i32, ptr::null_mut(), log.as_mut_ptr());
        (gl.delete_shader)(shader);
        return Err(format!(
            "shader compile failed: {}",
            CStr::from_ptr(log.as_ptr()).to_string_lossy()
        ));
    }
    Ok(shader)
}

// Largest rectangle with the picture's aspect ratio, centered: (x, y, width, height)
fn letterbox(
    content_width: u32,
    content_height: u32,
    width: i32,
    height: i32,
) -> (i32, i32, i32, i32) {
    if content_width == 0 || content_height == 0 || width <= 0 || height <= 0 {
        return (0, 0, width.max(0), height.max(0));
    }
    let scaled_width = (height as i64 * content_width as i64 / content_height as i64) as i32;
    if scaled_width <= width {
        ((width - scaled_width) / 2, 0, scaled_width, height)
    } else {
        let scaled_height = (width as i64 * content_height as i64 / content_width as i64) as i32;
        (0, (height - scaled_height) / 2, width, scaled_height)
    }
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;
    use crate::{
        linux::{
            gpu,
            vaapi::{H264Decoder, JpegDecoder},
        },
        rtsp::h264::{nal_type, split_annex_b, NAL_AUD},
    };

    fn ffmpeg(args: String) {
        let status = Command::new("ffmpeg")
            .args("-hide_banner -loglevel error -y".split_whitespace())
            .args(args.split_whitespace())
            .status()
            .expect("ffmpeg not found");
        assert!(status.success(), "ffmpeg {args}");
    }

    // First decoded frame of a test pattern, and ffmpeg's own RGBA conversion of it. The
    // sws flags make ffmpeg interpolate chroma like the GPU sampler does (its default
    // repeats chroma samples, which differs at sharp color edges)
    fn decoded_test_frame(node: &Path, size: &str) -> (DecodedFrame, H264Decoder, Vec<u8>) {
        let dir = std::env::temp_dir().join(format!("egl-{}-{size}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (stream, reference) = (dir.join("stream.h264"), dir.join("reference.rgba"));
        ffmpeg(format!(
            "-f lavfi -i testsrc2=size={size}:rate=30 -t 0.2 -pix_fmt yuv420p -c:v libx264 \
             -x264-params aud=1 -f h264 {}",
            stream.display()
        ));
        ffmpeg(format!(
            "-i {} -frames:v 1 -sws_flags bilinear+full_chroma_int+accurate_rnd -f rawvideo -pix_fmt rgba {}",
            stream.display(),
            reference.display()
        ));
        let data = std::fs::read(&stream).unwrap();
        let reference = std::fs::read(&reference).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        let mut units: Vec<Vec<u8>> = Vec::new();
        for nal in split_annex_b(&data) {
            if nal_type(nal) == NAL_AUD || units.is_empty() {
                units.push(Vec::new());
            }
            units
                .last_mut()
                .unwrap()
                .extend([0, 0, 0, 1].iter().chain(nal));
        }
        let mut decoder = H264Decoder::new(node).unwrap();
        let mut frames = units
            .iter()
            .flat_map(|unit| decoder.decode(unit, 0).unwrap());
        let frame = frames.next().expect("no frame decoded");
        drop(frames);
        (frame, decoder, reference)
    }

    // (mean absolute difference, 99th percentile) over the RGB channels
    fn difference(actual: &[u8], expected: &[u8]) -> (f64, u8) {
        assert_eq!(actual.len(), expected.len());
        let mut errors: Vec<u8> = actual
            .chunks(4)
            .zip(expected.chunks(4))
            .flat_map(|(a, e)| (0..3).map(move |c| a[c].abs_diff(e[c])))
            .collect();
        let mean = errors.iter().map(|&e| e as f64).sum::<f64>() / errors.len() as f64;
        errors.sort_unstable();
        (mean, errors[errors.len() * 99 / 100])
    }

    fn render_node() -> std::path::PathBuf {
        gpu::probe()
            .into_iter()
            .find(|gpu| gpu.vaapi.as_ref().is_ok_and(|caps| caps.h264_decode))
            .expect("no GPU decodes H.264 with VA-API")
            .render_node
    }

    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU with EGL, run with: cargo test -- --ignored --nocapture"]
    fn draws_decoded_frames_like_ffmpeg_converts_them() {
        let node = render_node();
        for (size, width, height) in [("640x480", 640, 480), ("360x202", 360, 202)] {
            let (frame, _decoder, reference) = decoded_test_frame(&node, size);
            let mut preview = EglPreview::offscreen(&node, width, height).unwrap();
            preview.make_current().unwrap();
            preview.draw(&frame).unwrap();
            // Drawn twice: the second time comes from the image cache
            preview.draw(&frame).unwrap();
            let (mean, p99) = difference(&preview.read_pixels(), &reference);
            println!("{size}: mean difference {mean:.2}, 99th percentile {p99}");
            // Bit exact on Intel; a little slack for other drivers' rounding
            assert!(
                mean < 0.5 && p99 <= 2,
                "{size}: picture differs from ffmpeg's conversion"
            );
        }
    }

    // JPEG is full range BT.601, the H.264 test streams limited range: both must come out
    // as ffmpeg converts them
    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU with EGL, run with: cargo test -- --ignored --nocapture"]
    fn draws_full_range_jpeg_frames_like_ffmpeg_converts_them() {
        let node = render_node();
        let dir = std::env::temp_dir().join(format!("egl-jpeg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (picture, reference) = (dir.join("picture.jpg"), dir.join("reference.rgba"));
        ffmpeg(format!(
            "-f lavfi -i testsrc2=size=640x480 -frames:v 1 -c:v mjpeg -q:v 2 -pix_fmt yuvj420p {}",
            picture.display()
        ));
        ffmpeg(format!(
            "-i {} -sws_flags bilinear+full_chroma_int+accurate_rnd -f rawvideo -pix_fmt rgba {}",
            picture.display(),
            reference.display()
        ));
        let (picture, reference) = (
            std::fs::read(&picture).unwrap(),
            std::fs::read(&reference).unwrap(),
        );
        std::fs::remove_dir_all(&dir).ok();

        let mut decoder = JpegDecoder::new(&node).unwrap();
        let frame = decoder.decode(&picture, 0).unwrap();
        assert!(frame.full_range && !frame.bt709);
        let mut preview = EglPreview::offscreen(&node, 640, 480).unwrap();
        preview.make_current().unwrap();
        preview.draw(&frame).unwrap();
        let (mean, p99) = difference(&preview.read_pixels(), &reference);
        println!("JPEG: mean difference {mean:.2}, 99th percentile {p99}");
        assert!(
            mean < 0.5 && p99 <= 2,
            "picture differs from ffmpeg's conversion"
        );
    }

    #[test]
    #[ignore = "needs ffmpeg and a VA-API GPU with EGL, run with: cargo test -- --ignored --nocapture"]
    fn letterboxes_into_wider_outputs() {
        let node = render_node();
        let (frame, _decoder, reference) = decoded_test_frame(&node, "640x480");
        let mut preview = EglPreview::offscreen(&node, 800, 480).unwrap();
        preview.make_current().unwrap();
        preview.draw(&frame).unwrap();
        let pixels = preview.read_pixels();

        let row_bytes = 800 * 4;
        let bars_black = pixels.chunks(row_bytes).all(|row| {
            row[..80 * 4]
                .iter()
                .chain(&row[720 * 4..])
                .enumerate()
                .all(|(i, &v)| i % 4 == 3 || v == 0)
        });
        assert!(bars_black, "the bars beside the picture are not black");
        let center: Vec<u8> = pixels
            .chunks(row_bytes)
            .flat_map(|row| row[80 * 4..720 * 4].to_vec())
            .collect();
        let (mean, _) = difference(&center, &reference);
        assert!(mean < 0.5, "picture inside the bars differs: {mean:.2}");
    }

    #[test]
    fn letterbox_keeps_the_aspect_ratio_centered() {
        assert_eq!(letterbox(640, 480, 640, 480), (0, 0, 640, 480));
        // Wider window: bars left and right
        assert_eq!(letterbox(640, 480, 800, 480), (80, 0, 640, 480));
        // Taller window: bars top and bottom
        assert_eq!(letterbox(1280, 720, 640, 640), (0, 140, 640, 360));
        assert_eq!(letterbox(0, 0, 100, 100), (0, 0, 100, 100));
    }
}
