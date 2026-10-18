// EGL, OpenGL ES 2 and the bit of Xlib the preview needs, loaded with dlopen like libva
use std::{
    ffi::{c_char, c_int, c_uint, c_void, CStr},
    sync::OnceLock,
};

use libloading::Library;

pub type EglDisplay = *mut c_void;
pub type EglConfig = *mut c_void;
pub type EglContext = *mut c_void;
pub type EglSurface = *mut c_void;
pub type EglImage = *mut c_void;
pub type EglDevice = *mut c_void;
pub type EglBoolean = c_uint;
pub type EglInt = i32;

pub const EGL_SUCCESS: EglInt = 0x3000;
pub const EGL_NONE: EglInt = 0x3038;
pub const EGL_EXTENSIONS: EglInt = 0x3055;
pub const EGL_WIDTH: EglInt = 0x3057;
pub const EGL_HEIGHT: EglInt = 0x3056;
pub const EGL_SURFACE_TYPE: EglInt = 0x3033;
pub const EGL_WINDOW_BIT: EglInt = 0x0004;
pub const EGL_PBUFFER_BIT: EglInt = 0x0001;
pub const EGL_RENDERABLE_TYPE: EglInt = 0x3040;
pub const EGL_OPENGL_ES2_BIT: EglInt = 0x0004;
pub const EGL_RED_SIZE: EglInt = 0x3024;
pub const EGL_GREEN_SIZE: EglInt = 0x3023;
pub const EGL_BLUE_SIZE: EglInt = 0x3022;
pub const EGL_CONTEXT_CLIENT_VERSION: EglInt = 0x3098;
pub const EGL_OPENGL_ES_API: c_uint = 0x30A0;
pub const EGL_PLATFORM_X11_KHR: c_uint = 0x31D5;
pub const EGL_PLATFORM_DEVICE_EXT: c_uint = 0x313F;
pub const EGL_DRM_RENDER_NODE_FILE_EXT: EglInt = 0x3377;
// EGL_EXT_image_dma_buf_import(_modifiers)
pub const EGL_LINUX_DMA_BUF_EXT: c_uint = 0x3270;
pub const EGL_LINUX_DRM_FOURCC_EXT: EglInt = 0x3271;
pub const EGL_DMA_BUF_PLANE_FD_EXT: [EglInt; 2] = [0x3272, 0x3275];
pub const EGL_DMA_BUF_PLANE_OFFSET_EXT: [EglInt; 2] = [0x3273, 0x3276];
pub const EGL_DMA_BUF_PLANE_PITCH_EXT: [EglInt; 2] = [0x3274, 0x3277];
pub const EGL_DMA_BUF_PLANE_MODIFIER_LO_EXT: [EglInt; 2] = [0x3443, 0x3445];
pub const EGL_DMA_BUF_PLANE_MODIFIER_HI_EXT: [EglInt; 2] = [0x3444, 0x3446];
pub const EGL_YUV_COLOR_SPACE_HINT_EXT: EglInt = 0x327B;
pub const EGL_SAMPLE_RANGE_HINT_EXT: EglInt = 0x327C;
pub const EGL_ITU_REC601_EXT: EglInt = 0x327F;
pub const EGL_ITU_REC709_EXT: EglInt = 0x3280;
pub const EGL_YUV_FULL_RANGE_EXT: EglInt = 0x3282;
pub const EGL_YUV_NARROW_RANGE_EXT: EglInt = 0x3283;

pub const GL_TEXTURE_EXTERNAL_OES: c_uint = 0x8D65;
pub const GL_TEXTURE_2D: c_uint = 0x0DE1;
pub const GL_TEXTURE0: c_uint = 0x84C0;
pub const GL_TEXTURE_MIN_FILTER: c_uint = 0x2801;
pub const GL_TEXTURE_MAG_FILTER: c_uint = 0x2800;
pub const GL_TEXTURE_WRAP_S: c_uint = 0x2802;
pub const GL_TEXTURE_WRAP_T: c_uint = 0x2803;
pub const GL_LINEAR: c_int = 0x2601;
pub const GL_CLAMP_TO_EDGE: c_int = 0x812F;
pub const GL_VERTEX_SHADER: c_uint = 0x8B31;
pub const GL_FRAGMENT_SHADER: c_uint = 0x8B30;
pub const GL_COMPILE_STATUS: c_uint = 0x8B81;
pub const GL_LINK_STATUS: c_uint = 0x8B82;
pub const GL_FLOAT: c_uint = 0x1406;
pub const GL_TRIANGLE_STRIP: c_uint = 0x0005;
pub const GL_COLOR_BUFFER_BIT: c_uint = 0x4000;
pub const GL_RGBA: c_uint = 0x1908;
pub const GL_UNSIGNED_BYTE: c_uint = 0x1401;
pub const GL_FRAMEBUFFER: c_uint = 0x8D40;
pub const GL_COLOR_ATTACHMENT0: c_uint = 0x8CE0;
pub const GL_FRAMEBUFFER_COMPLETE: c_uint = 0x8CD5;

macro_rules! library {
    ($name:ident, $file:literal, { $($field:ident: $symbol:literal => fn($($arg:ty),*) -> $ret:ty;)* }) => {
        pub(crate) struct $name {
            _lib: Library,
            $(pub $field: unsafe extern "C" fn($($arg),*) -> $ret,)*
        }

        impl $name {
            fn load() -> Result<Self, String> {
                unsafe {
                    let lib = Library::new($file).map_err(|e| format!("{} not found: {e}", $file))?;
                    let missing = |e: libloading::Error| format!("{} is missing a function: {e}", $file);
                    Ok($name {
                        $($field: *lib.get(concat!($symbol, "\0").as_bytes()).map_err(missing)?,)*
                        _lib: lib,
                    })
                }
            }
        }
    };
}

library!(Egl, "libEGL.so.1", {
    get_platform_display: "eglGetPlatformDisplay" => fn(c_uint, *mut c_void, *const isize) -> EglDisplay;
    initialize: "eglInitialize" => fn(EglDisplay, *mut EglInt, *mut EglInt) -> EglBoolean;
    terminate: "eglTerminate" => fn(EglDisplay) -> EglBoolean;
    bind_api: "eglBindAPI" => fn(c_uint) -> EglBoolean;
    choose_config: "eglChooseConfig" => fn(EglDisplay, *const EglInt, *mut EglConfig, EglInt, *mut EglInt) -> EglBoolean;
    create_context: "eglCreateContext" => fn(EglDisplay, EglConfig, EglContext, *const EglInt) -> EglContext;
    destroy_context: "eglDestroyContext" => fn(EglDisplay, EglContext) -> EglBoolean;
    create_platform_window_surface: "eglCreatePlatformWindowSurface" => fn(EglDisplay, EglConfig, *mut c_void, *const isize) -> EglSurface;
    destroy_surface: "eglDestroySurface" => fn(EglDisplay, EglSurface) -> EglBoolean;
    make_current: "eglMakeCurrent" => fn(EglDisplay, EglSurface, EglSurface, EglContext) -> EglBoolean;
    swap_buffers: "eglSwapBuffers" => fn(EglDisplay, EglSurface) -> EglBoolean;
    swap_interval: "eglSwapInterval" => fn(EglDisplay, EglInt) -> EglBoolean;
    query_surface: "eglQuerySurface" => fn(EglDisplay, EglSurface, EglInt, *mut EglInt) -> EglBoolean;
    query_string: "eglQueryString" => fn(EglDisplay, EglInt) -> *const c_char;
    get_error: "eglGetError" => fn() -> EglInt;
    get_proc_address: "eglGetProcAddress" => fn(*const c_char) -> *mut c_void;
});

library!(Gles, "libGLESv2.so.2", {
    create_shader: "glCreateShader" => fn(c_uint) -> c_uint;
    shader_source: "glShaderSource" => fn(c_uint, c_int, *const *const c_char, *const c_int) -> ();
    compile_shader: "glCompileShader" => fn(c_uint) -> ();
    get_shader_iv: "glGetShaderiv" => fn(c_uint, c_uint, *mut c_int) -> ();
    get_shader_info_log: "glGetShaderInfoLog" => fn(c_uint, c_int, *mut c_int, *mut c_char) -> ();
    delete_shader: "glDeleteShader" => fn(c_uint) -> ();
    create_program: "glCreateProgram" => fn() -> c_uint;
    attach_shader: "glAttachShader" => fn(c_uint, c_uint) -> ();
    bind_attrib_location: "glBindAttribLocation" => fn(c_uint, c_uint, *const c_char) -> ();
    link_program: "glLinkProgram" => fn(c_uint) -> ();
    get_program_iv: "glGetProgramiv" => fn(c_uint, c_uint, *mut c_int) -> ();
    get_program_info_log: "glGetProgramInfoLog" => fn(c_uint, c_int, *mut c_int, *mut c_char) -> ();
    use_program: "glUseProgram" => fn(c_uint) -> ();
    delete_program: "glDeleteProgram" => fn(c_uint) -> ();
    get_uniform_location: "glGetUniformLocation" => fn(c_uint, *const c_char) -> c_int;
    uniform_1i: "glUniform1i" => fn(c_int, c_int) -> ();
    gen_textures: "glGenTextures" => fn(c_int, *mut c_uint) -> ();
    delete_textures: "glDeleteTextures" => fn(c_int, *const c_uint) -> ();
    bind_texture: "glBindTexture" => fn(c_uint, c_uint) -> ();
    active_texture: "glActiveTexture" => fn(c_uint) -> ();
    tex_parameter_i: "glTexParameteri" => fn(c_uint, c_uint, c_int) -> ();
    tex_image_2d: "glTexImage2D" => fn(c_uint, c_int, c_int, c_int, c_int, c_int, c_uint, c_uint, *const c_void) -> ();
    vertex_attrib_pointer: "glVertexAttribPointer" => fn(c_uint, c_int, c_uint, u8, c_int, *const c_void) -> ();
    enable_vertex_attrib_array: "glEnableVertexAttribArray" => fn(c_uint) -> ();
    viewport: "glViewport" => fn(c_int, c_int, c_int, c_int) -> ();
    clear_color: "glClearColor" => fn(f32, f32, f32, f32) -> ();
    clear: "glClear" => fn(c_uint) -> ();
    draw_arrays: "glDrawArrays" => fn(c_uint, c_int, c_int) -> ();
    get_error: "glGetError" => fn() -> c_uint;
    finish: "glFinish" => fn() -> ();
    read_pixels: "glReadPixels" => fn(c_int, c_int, c_int, c_int, c_uint, c_uint, *mut c_void) -> ();
    gen_framebuffers: "glGenFramebuffers" => fn(c_int, *mut c_uint) -> ();
    delete_framebuffers: "glDeleteFramebuffers" => fn(c_int, *const c_uint) -> ();
    bind_framebuffer: "glBindFramebuffer" => fn(c_uint, c_uint) -> ();
    framebuffer_texture_2d: "glFramebufferTexture2D" => fn(c_uint, c_uint, c_uint, c_uint, c_int) -> ();
    check_framebuffer_status: "glCheckFramebufferStatus" => fn(c_uint) -> c_uint;
});

library!(Xlib, "libX11.so.6", {
    open_display: "XOpenDisplay" => fn(*const c_char) -> *mut c_void;
    close_display: "XCloseDisplay" => fn(*mut c_void) -> c_int;
});

// Extension entry points, resolved through eglGetProcAddress
type CreateImage =
    unsafe extern "C" fn(EglDisplay, EglContext, c_uint, *mut c_void, *const EglInt) -> EglImage;
type DestroyImage = unsafe extern "C" fn(EglDisplay, EglImage) -> EglBoolean;
type ImageTargetTexture = unsafe extern "C" fn(c_uint, EglImage);
type QueryDevices = unsafe extern "C" fn(EglInt, *mut EglDevice, *mut EglInt) -> EglBoolean;
type QueryDeviceString = unsafe extern "C" fn(EglDevice, EglInt) -> *const c_char;

pub(crate) struct EglExtensions {
    pub create_image: CreateImage,
    pub destroy_image: DestroyImage,
    pub image_target_texture: ImageTargetTexture,
    pub query_devices: Option<QueryDevices>,
    pub query_device_string: Option<QueryDeviceString>,
}

pub(crate) struct Api {
    pub egl: Egl,
    pub gl: Gles,
    pub ext: EglExtensions,
}

impl Api {
    pub fn get() -> Result<&'static Api, String> {
        static API: OnceLock<Result<Api, String>> = OnceLock::new();
        API.get_or_init(|| {
            let egl = Egl::load()?;
            let gl = Gles::load()?;
            let ext = unsafe {
                let proc = |name: &CStr| (egl.get_proc_address)(name.as_ptr());
                let required = |name: &CStr| {
                    let address = proc(name);
                    if address.is_null() {
                        Err(format!("EGL does not provide {}", name.to_string_lossy()))
                    } else {
                        Ok(address)
                    }
                };
                let optional = |name: &CStr| Some(proc(name)).filter(|address| !address.is_null());
                EglExtensions {
                    create_image: std::mem::transmute::<*mut c_void, CreateImage>(required(
                        c"eglCreateImageKHR",
                    )?),
                    destroy_image: std::mem::transmute::<*mut c_void, DestroyImage>(required(
                        c"eglDestroyImageKHR",
                    )?),
                    image_target_texture: std::mem::transmute::<*mut c_void, ImageTargetTexture>(
                        required(c"glEGLImageTargetTexture2DOES")?,
                    ),
                    query_devices: optional(c"eglQueryDevicesEXT")
                        .map(|address| std::mem::transmute::<*mut c_void, QueryDevices>(address)),
                    query_device_string: optional(c"eglQueryDeviceStringEXT").map(|address| {
                        std::mem::transmute::<*mut c_void, QueryDeviceString>(address)
                    }),
                }
            };
            Ok(Api { egl, gl, ext })
        })
        .as_ref()
        .map_err(Clone::clone)
    }

    pub fn egl_error(&self, call: &str) -> String {
        format!("{call} failed (EGL error {:#x})", unsafe {
            (self.egl.get_error)()
        })
    }
}

pub(crate) fn xlib() -> Result<&'static Xlib, String> {
    static XLIB: OnceLock<Result<Xlib, String>> = OnceLock::new();
    XLIB.get_or_init(Xlib::load).as_ref().map_err(Clone::clone)
}
