// libva entry points, loaded with dlopen so the engine still loads on machines without
// libva (e.g. NVIDIA only). Types and constants come from the generated sys.rs
use std::{
    ffi::{c_char, c_int, c_uint, c_void, CStr},
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    path::Path,
    ptr,
    sync::{Arc, OnceLock},
};

use libloading::Library;

use super::sys::*;

type MessageCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

macro_rules! va_api {
    ($($field:ident: $symbol:literal => fn($($arg:ty),*) -> $ret:ty;)*) => {
        pub(crate) struct Api {
            // Keep the libraries loaded while the function pointers are in use
            _va: Library,
            _va_drm: Library,
            pub get_display_drm: unsafe extern "C" fn(c_int) -> VADisplay,
            $(pub $field: unsafe extern "C" fn($($arg),*) -> $ret,)*
        }

        impl Api {
            fn load() -> Result<Self, String> {
                unsafe {
                    let va = Library::new("libva.so.2")
                        .map_err(|e| format!("libva not found: {e}"))?;
                    let va_drm = Library::new("libva-drm.so.2")
                        .map_err(|e| format!("libva-drm not found: {e}"))?;
                    let missing = |e: libloading::Error| format!("libva is missing a function: {e}");
                    Ok(Api {
                        get_display_drm: *va_drm.get(b"vaGetDisplayDRM\0").map_err(missing)?,
                        $($field: *va.get(concat!($symbol, "\0").as_bytes()).map_err(missing)?,)*
                        _va: va,
                        _va_drm: va_drm,
                    })
                }
            }
        }
    };
}

va_api! {
    initialize: "vaInitialize" => fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
    terminate: "vaTerminate" => fn(VADisplay) -> VAStatus;
    set_info_callback: "vaSetInfoCallback" => fn(VADisplay, MessageCallback, *mut c_void) -> MessageCallback;
    error_str: "vaErrorStr" => fn(VAStatus) -> *const c_char;
    query_vendor_string: "vaQueryVendorString" => fn(VADisplay) -> *const c_char;
    max_num_profiles: "vaMaxNumProfiles" => fn(VADisplay) -> c_int;
    max_num_entrypoints: "vaMaxNumEntrypoints" => fn(VADisplay) -> c_int;
    query_config_profiles: "vaQueryConfigProfiles" => fn(VADisplay, *mut VAProfile, *mut c_int) -> VAStatus;
    query_config_entrypoints: "vaQueryConfigEntrypoints" => fn(VADisplay, VAProfile, *mut VAEntrypoint, *mut c_int) -> VAStatus;
    get_config_attributes: "vaGetConfigAttributes" => fn(VADisplay, VAProfile, VAEntrypoint, *mut VAConfigAttrib, c_int) -> VAStatus;
    create_config: "vaCreateConfig" => fn(VADisplay, VAProfile, VAEntrypoint, *mut VAConfigAttrib, c_int, *mut VAConfigID) -> VAStatus;
    destroy_config: "vaDestroyConfig" => fn(VADisplay, VAConfigID) -> VAStatus;
    create_surfaces: "vaCreateSurfaces" => fn(VADisplay, c_uint, c_uint, c_uint, *mut VASurfaceID, c_uint, *mut VASurfaceAttrib, c_uint) -> VAStatus;
    destroy_surfaces: "vaDestroySurfaces" => fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus;
    create_context: "vaCreateContext" => fn(VADisplay, VAConfigID, c_int, c_int, c_int, *mut VASurfaceID, c_int, *mut VAContextID) -> VAStatus;
    destroy_context: "vaDestroyContext" => fn(VADisplay, VAContextID) -> VAStatus;
    create_buffer: "vaCreateBuffer" => fn(VADisplay, VAContextID, VABufferType, c_uint, c_uint, *mut c_void, *mut VABufferID) -> VAStatus;
    destroy_buffer: "vaDestroyBuffer" => fn(VADisplay, VABufferID) -> VAStatus;
    map_buffer: "vaMapBuffer" => fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus;
    unmap_buffer: "vaUnmapBuffer" => fn(VADisplay, VABufferID) -> VAStatus;
    begin_picture: "vaBeginPicture" => fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
    render_picture: "vaRenderPicture" => fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
    end_picture: "vaEndPicture" => fn(VADisplay, VAContextID) -> VAStatus;
    sync_surface: "vaSyncSurface" => fn(VADisplay, VASurfaceID) -> VAStatus;
    create_image: "vaCreateImage" => fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus;
    get_image: "vaGetImage" => fn(VADisplay, VASurfaceID, c_int, c_int, c_uint, c_uint, VAImageID) -> VAStatus;
    destroy_image: "vaDestroyImage" => fn(VADisplay, VAImageID) -> VAStatus;
    export_surface_handle: "vaExportSurfaceHandle" => fn(VADisplay, VASurfaceID, u32, u32, *mut c_void) -> VAStatus;
}

impl Api {
    // Loaded once per process, the libraries stay loaded afterwards
    pub fn get() -> Result<&'static Api, String> {
        static API: OnceLock<Result<Api, String>> = OnceLock::new();
        API.get_or_init(Api::load).as_ref().map_err(Clone::clone)
    }

    pub fn error(&self, call: &str, status: VAStatus) -> String {
        let text = unsafe { (self.error_str)(status) };
        let text = if text.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(text) }
                .to_string_lossy()
                .into_owned()
        };
        format!("{call} failed: {text} ({status})")
    }

    pub fn check(&self, call: &str, status: VAStatus) -> Result<(), String> {
        if status == VA_STATUS_SUCCESS as VAStatus {
            Ok(())
        } else {
            Err(self.error(call, status))
        }
    }
}

// An initialized VA display on one GPU render node, terminated on drop
pub(crate) struct Display {
    pub api: &'static Api,
    pub raw: VADisplay,
    pub vendor: String,
    // The display uses this descriptor until vaTerminate
    _node: File,
}

// libva serializes calls on a display internally, it is used from several threads
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

impl Display {
    pub fn open(render_node: &Path) -> Result<Arc<Self>, String> {
        let api = Api::get()?;
        let node = OpenOptions::new()
            .read(true)
            .write(true)
            .open(render_node)
            .map_err(|e| format!("cannot open {}: {e}", render_node.display()))?;

        unsafe {
            let raw = (api.get_display_drm)(node.as_raw_fd());
            if raw.is_null() {
                return Err("vaGetDisplayDRM failed".to_string());
            }
            // libva prints its version and the driver it loaded to stderr otherwise
            (api.set_info_callback)(raw, None, ptr::null_mut());

            let (mut major, mut minor) = (0, 0);
            let status = (api.initialize)(raw, &mut major, &mut minor);
            if status != VA_STATUS_SUCCESS as VAStatus {
                (api.terminate)(raw);
                return Err(format!("no VA-API driver for this GPU (status {status})"));
            }

            let vendor = (api.query_vendor_string)(raw);
            Ok(Arc::new(Display {
                api,
                raw,
                vendor: if vendor.is_null() {
                    String::new()
                } else {
                    CStr::from_ptr(vendor).to_string_lossy().into_owned()
                },
                _node: node,
            }))
        }
    }

    pub fn profiles(&self) -> Vec<VAProfile> {
        let api = self.api;
        list(
            unsafe { (api.max_num_profiles)(self.raw) },
            |items, count| unsafe { (api.query_config_profiles)(self.raw, items, count) },
        )
    }

    pub fn entrypoints(&self, profile: VAProfile) -> Vec<VAEntrypoint> {
        let api = self.api;
        list(
            unsafe { (api.max_num_entrypoints)(self.raw) },
            |items, count| unsafe {
                (api.query_config_entrypoints)(self.raw, profile, items, count)
            },
        )
    }

    // None when the attribute is not supported for this profile and entrypoint
    pub fn config_attribute(
        &self,
        profile: VAProfile,
        entrypoint: VAEntrypoint,
        attribute: VAConfigAttribType,
    ) -> Option<u32> {
        let mut attrib = VAConfigAttrib {
            type_: attribute,
            value: 0,
        };
        let status = unsafe {
            (self.api.get_config_attributes)(self.raw, profile, entrypoint, &mut attrib, 1)
        };
        (status == VA_STATUS_SUCCESS as VAStatus && attrib.value != VA_ATTRIB_NOT_SUPPORTED)
            .then_some(attrib.value)
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        unsafe { (self.api.terminate)(self.raw) };
    }
}

// Not in the generated constants: va.h defines it as a plain macro of 0x80000000
pub(crate) const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

// VA-API list queries fill a caller allocated array of at most `max` items
fn list<T: Copy + Default>(
    max: c_int,
    query: impl FnOnce(*mut T, *mut c_int) -> VAStatus,
) -> Vec<T> {
    let mut items = vec![T::default(); max.max(0) as usize];
    let mut count = 0;
    if query(items.as_mut_ptr(), &mut count) != VA_STATUS_SUCCESS as VAStatus {
        return Vec::new();
    }
    items.truncate(count.clamp(0, max) as usize);
    items
}
