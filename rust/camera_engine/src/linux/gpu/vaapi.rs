// VA-API: the video acceleration API of Mesa (Intel, AMD) and of Intel's iHD driver.
// Only the handful of functions needed to list profiles and entrypoints are loaded
use std::{
    ffi::{c_char, c_int, c_void, CStr},
    fs::OpenOptions,
    os::fd::AsRawFd,
    path::Path,
    ptr,
};

use libloading::Library;

use super::VaapiCaps;

type VaDisplay = *mut c_void;
type VaStatus = c_int;
type VaProfile = c_int;
type VaEntrypoint = c_int;
type VaMessageCallback = Option<unsafe extern "C" fn(*mut c_void, *const c_char)>;

const VA_STATUS_SUCCESS: VaStatus = 0;

const VA_PROFILE_H264_MAIN: VaProfile = 6;
const VA_PROFILE_H264_HIGH: VaProfile = 7;
const VA_PROFILE_JPEG_BASELINE: VaProfile = 12;
const VA_PROFILE_H264_CONSTRAINED_BASELINE: VaProfile = 13;

const VA_ENTRYPOINT_VLD: VaEntrypoint = 1;
const VA_ENTRYPOINT_ENC_SLICE: VaEntrypoint = 6;
const VA_ENTRYPOINT_ENC_SLICE_LP: VaEntrypoint = 8;

const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 5;
const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;
const VA_RC_CBR: u32 = 0x2;

#[repr(C)]
struct VaConfigAttrib {
    attrib_type: c_int,
    value: u32,
}

struct Api {
    // Keep the libraries loaded while the function pointers below are in use
    _va: Library,
    _va_drm: Library,
    get_display_drm: unsafe extern "C" fn(c_int) -> VaDisplay,
    initialize: unsafe extern "C" fn(VaDisplay, *mut c_int, *mut c_int) -> VaStatus,
    terminate: unsafe extern "C" fn(VaDisplay) -> VaStatus,
    set_info_callback:
        unsafe extern "C" fn(VaDisplay, VaMessageCallback, *mut c_void) -> VaMessageCallback,
    query_vendor_string: unsafe extern "C" fn(VaDisplay) -> *const c_char,
    max_num_profiles: unsafe extern "C" fn(VaDisplay) -> c_int,
    max_num_entrypoints: unsafe extern "C" fn(VaDisplay) -> c_int,
    query_config_profiles: unsafe extern "C" fn(VaDisplay, *mut VaProfile, *mut c_int) -> VaStatus,
    query_config_entrypoints:
        unsafe extern "C" fn(VaDisplay, VaProfile, *mut VaEntrypoint, *mut c_int) -> VaStatus,
    get_config_attributes: unsafe extern "C" fn(
        VaDisplay,
        VaProfile,
        VaEntrypoint,
        *mut VaConfigAttrib,
        c_int,
    ) -> VaStatus,
}

impl Api {
    fn load() -> Result<Self, String> {
        unsafe {
            let va = Library::new("libva.so.2").map_err(|e| format!("libva not found: {e}"))?;
            let va_drm =
                Library::new("libva-drm.so.2").map_err(|e| format!("libva-drm not found: {e}"))?;

            Ok(Api {
                get_display_drm: *va_drm.get(b"vaGetDisplayDRM\0").map_err(missing)?,
                initialize: *va.get(b"vaInitialize\0").map_err(missing)?,
                terminate: *va.get(b"vaTerminate\0").map_err(missing)?,
                set_info_callback: *va.get(b"vaSetInfoCallback\0").map_err(missing)?,
                query_vendor_string: *va.get(b"vaQueryVendorString\0").map_err(missing)?,
                max_num_profiles: *va.get(b"vaMaxNumProfiles\0").map_err(missing)?,
                max_num_entrypoints: *va.get(b"vaMaxNumEntrypoints\0").map_err(missing)?,
                query_config_profiles: *va.get(b"vaQueryConfigProfiles\0").map_err(missing)?,
                query_config_entrypoints: *va
                    .get(b"vaQueryConfigEntrypoints\0")
                    .map_err(missing)?,
                get_config_attributes: *va.get(b"vaGetConfigAttributes\0").map_err(missing)?,
                _va: va,
                _va_drm: va_drm,
            })
        }
    }
}

pub(super) fn probe(render_node: &Path) -> Result<VaapiCaps, String> {
    let api = Api::load()?;
    // Must stay open until vaTerminate, the display uses this file descriptor
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(render_node)
        .map_err(|e| format!("cannot open {}: {e}", render_node.display()))?;

    unsafe {
        let display = (api.get_display_drm)(file.as_raw_fd());
        if display.is_null() {
            return Err("vaGetDisplayDRM failed".to_string());
        }
        // libva prints its version and the driver it loaded to stderr otherwise
        (api.set_info_callback)(display, None, ptr::null_mut());

        let (mut major, mut minor) = (0, 0);
        let status = (api.initialize)(display, &mut major, &mut minor);
        let caps = if status == VA_STATUS_SUCCESS {
            Ok(query_caps(&api, display))
        } else {
            Err(format!("no VA-API driver for this GPU (status {status})"))
        };
        (api.terminate)(display);
        caps
    }
}

unsafe fn query_caps(api: &Api, display: VaDisplay) -> VaapiCaps {
    let vendor = (api.query_vendor_string)(display);
    let mut caps = VaapiCaps {
        driver: if vendor.is_null() {
            String::new()
        } else {
            CStr::from_ptr(vendor).to_string_lossy().into_owned()
        },
        ..Default::default()
    };

    let profiles = list(
        (api.max_num_profiles)(display),
        |items, count| (api.query_config_profiles)(display, items, count),
    );
    let entrypoints = |profile: VaProfile| {
        list((api.max_num_entrypoints)(display), |items, count| {
            (api.query_config_entrypoints)(display, profile, items, count)
        })
    };

    for profile in [
        VA_PROFILE_H264_CONSTRAINED_BASELINE,
        VA_PROFILE_H264_MAIN,
        VA_PROFILE_H264_HIGH,
    ] {
        if !profiles.contains(&profile) {
            continue;
        }
        let supported = entrypoints(profile);
        caps.h264_decode |= supported.contains(&VA_ENTRYPOINT_VLD);
        caps.h264_encode |= supported.contains(&VA_ENTRYPOINT_ENC_SLICE)
            && supports_cbr(api, display, profile, VA_ENTRYPOINT_ENC_SLICE);
        caps.h264_encode_low_power |= supported.contains(&VA_ENTRYPOINT_ENC_SLICE_LP)
            && supports_cbr(api, display, profile, VA_ENTRYPOINT_ENC_SLICE_LP);
    }

    if profiles.contains(&VA_PROFILE_JPEG_BASELINE) {
        caps.jpeg_decode = entrypoints(VA_PROFILE_JPEG_BASELINE).contains(&VA_ENTRYPOINT_VLD);
    }

    caps
}

// Low power encoders may lack bitrate control when their firmware (HuC) is not loaded
unsafe fn supports_cbr(
    api: &Api,
    display: VaDisplay,
    profile: VaProfile,
    entrypoint: VaEntrypoint,
) -> bool {
    let mut attrib = VaConfigAttrib {
        attrib_type: VA_CONFIG_ATTRIB_RATE_CONTROL,
        value: 0,
    };
    (api.get_config_attributes)(display, profile, entrypoint, &mut attrib, 1) == VA_STATUS_SUCCESS
        && attrib.value != VA_ATTRIB_NOT_SUPPORTED
        && attrib.value & VA_RC_CBR != 0
}

// VA-API list queries fill a caller allocated array of at most `max` items
fn list(max: c_int, query: impl FnOnce(*mut c_int, *mut c_int) -> VaStatus) -> Vec<c_int> {
    let mut items = vec![0; max.max(0) as usize];
    let mut count = 0;
    if query(items.as_mut_ptr(), &mut count) != VA_STATUS_SUCCESS {
        return Vec::new();
    }
    items.truncate(count.clamp(0, max) as usize);
    items
}

fn missing(e: libloading::Error) -> String {
    format!("libva is missing a function: {e}")
}
