// NVIDIA proprietary driver: NVENC (encode) and NVDEC (decode) both run on a CUDA context.
// Layouts and constants follow nvEncodeAPI.h (Video Codec SDK 11.0, supported by drivers
// 455 and newer) and cuviddec.h
use std::{
    ffi::{c_char, c_int, c_uint, c_void, CStr},
    mem, ptr,
};

use libloading::Library;

use super::NvidiaCaps;

type CuResult = c_int;
type CuDevice = c_int;
type CuContext = *mut c_void;

const CUDA_SUCCESS: CuResult = 0;

struct Cuda {
    _lib: Library,
    init: unsafe extern "C" fn(c_uint) -> CuResult,
    device_get_count: unsafe extern "C" fn(*mut c_int) -> CuResult,
    device_get: unsafe extern "C" fn(*mut CuDevice, c_int) -> CuResult,
    device_get_name: unsafe extern "C" fn(*mut c_char, c_int, CuDevice) -> CuResult,
    device_get_pci_bus_id: unsafe extern "C" fn(*mut c_char, c_int, CuDevice) -> CuResult,
    ctx_create: unsafe extern "C" fn(*mut CuContext, c_uint, CuDevice) -> CuResult,
    ctx_destroy: unsafe extern "C" fn(CuContext) -> CuResult,
}

impl Cuda {
    fn load() -> Result<Self, String> {
        unsafe {
            let lib =
                Library::new("libcuda.so.1").map_err(|e| format!("libcuda not found: {e}"))?;
            let missing = |e: libloading::Error| format!("libcuda is missing a function: {e}");
            Ok(Cuda {
                init: *lib.get(b"cuInit\0").map_err(missing)?,
                device_get_count: *lib.get(b"cuDeviceGetCount\0").map_err(missing)?,
                device_get: *lib.get(b"cuDeviceGet\0").map_err(missing)?,
                device_get_name: *lib.get(b"cuDeviceGetName\0").map_err(missing)?,
                device_get_pci_bus_id: *lib.get(b"cuDeviceGetPCIBusId\0").map_err(missing)?,
                ctx_create: *lib.get(b"cuCtxCreate_v2\0").map_err(missing)?,
                ctx_destroy: *lib.get(b"cuCtxDestroy_v2\0").map_err(missing)?,
                _lib: lib,
            })
        }
    }
}

pub(super) fn probe(pci_address: &str) -> Result<NvidiaCaps, String> {
    let cuda = Cuda::load()?;
    unsafe {
        check((cuda.init)(0), "cuInit")?;
        let device = find_device(&cuda, pci_address)?;

        let mut name = [0 as c_char; 256];
        check(
            (cuda.device_get_name)(name.as_mut_ptr(), name.len() as c_int, device),
            "cuDeviceGetName",
        )?;

        let mut context: CuContext = ptr::null_mut();
        check((cuda.ctx_create)(&mut context, 0, device), "cuCtxCreate")?;
        // The new context is current on this thread, NVDEC queries run against it
        let caps = NvidiaCaps {
            name: CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned(),
            nvenc_h264: nvenc_supports_h264(context),
            nvdec_h264: nvdec_supports(CODEC_H264),
            nvdec_jpeg: nvdec_supports(CODEC_JPEG),
        };
        (cuda.ctx_destroy)(context);

        Ok(caps)
    }
}

// CUDA numbers devices on its own, they are matched to the DRM node by PCI address
unsafe fn find_device(cuda: &Cuda, pci_address: &str) -> Result<CuDevice, String> {
    let wanted = parse_pci(pci_address);
    let mut count = 0;
    check((cuda.device_get_count)(&mut count), "cuDeviceGetCount")?;

    for ordinal in 0..count {
        let mut device = 0;
        if (cuda.device_get)(&mut device, ordinal) != CUDA_SUCCESS {
            continue;
        }
        let mut bus_id = [0 as c_char; 32];
        if (cuda.device_get_pci_bus_id)(bus_id.as_mut_ptr(), bus_id.len() as c_int, device)
            != CUDA_SUCCESS
        {
            continue;
        }
        let bus_id = CStr::from_ptr(bus_id.as_ptr()).to_string_lossy();
        if wanted.is_some() && parse_pci(&bus_id) == wanted {
            return Ok(device);
        }
    }

    Err(format!("CUDA does not list the GPU at {pci_address}"))
}

// "0000:01:00.0" -> (domain, bus, device, function). CUDA may pad the domain to 8 digits
fn parse_pci(address: &str) -> Option<(u32, u32, u32, u32)> {
    let (domain, rest) = address.split_once(':')?;
    let (bus, rest) = rest.split_once(':')?;
    let (device, function) = rest.split_once('.')?;
    let hex = |text: &str| u32::from_str_radix(text, 16).ok();
    Some((hex(domain)?, hex(bus)?, hex(device)?, hex(function)?))
}

// ---- NVENC ----

const NVENC_API_MAJOR: u32 = 11;
const NVENC_API_MINOR: u32 = 0;
const NVENC_API_VERSION: u32 = NVENC_API_MAJOR | (NVENC_API_MINOR << 24);
const NV_ENC_SUCCESS: c_int = 0;
const NV_ENC_DEVICE_TYPE_CUDA: u32 = 1;

const fn nvenc_struct_version(version: u32) -> u32 {
    NVENC_API_VERSION | (version << 16) | (0x7 << 28)
}

// Positions in NV_ENCODE_API_FUNCTION_LIST, counted after its two leading u32 fields
const FN_GET_ENCODE_GUID_COUNT: usize = 1;
const FN_GET_ENCODE_GUIDS: usize = 4;
const FN_DESTROY_ENCODER: usize = 27;
const FN_OPEN_ENCODE_SESSION_EX: usize = 29;

#[repr(C)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct Guid {
    data1: u32,
    data2: u16,
    data3: u16,
    data4: [u8; 8],
}

const NV_ENC_CODEC_H264_GUID: Guid = Guid {
    data1: 0x6bc8_2762,
    data2: 0x4e63,
    data3: 0x4ca4,
    data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
};

// Larger than the real struct (41 functions + reserved space): the driver fills only
// what it knows for the requested version
#[repr(C)]
struct FunctionList {
    version: u32,
    reserved: u32,
    functions: [*mut c_void; 400],
}

#[repr(C)]
struct OpenSessionParams {
    version: u32,
    device_type: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}

// Opening a session is the only reliable test: some GPUs (e.g. GeForce 940MX, GM108) have
// no NVENC at all, and the driver reports it only here
unsafe fn nvenc_supports_h264(context: CuContext) -> bool {
    let Ok(lib) = Library::new("libnvidia-encode.so.1") else {
        return false;
    };
    let Ok(max_version) =
        lib.get::<unsafe extern "C" fn(*mut u32) -> c_int>(b"NvEncodeAPIGetMaxSupportedVersion\0")
    else {
        return false;
    };
    let Ok(create_instance) =
        lib.get::<unsafe extern "C" fn(*mut FunctionList) -> c_int>(b"NvEncodeAPICreateInstance\0")
    else {
        return false;
    };

    // Driver reports (major << 4) | minor
    let mut supported = 0u32;
    if max_version(&mut supported) != NV_ENC_SUCCESS
        || supported < ((NVENC_API_MAJOR << 4) | NVENC_API_MINOR)
    {
        return false;
    }

    let mut functions: Box<FunctionList> = Box::new(mem::zeroed());
    functions.version = nvenc_struct_version(2);
    if create_instance(&mut *functions) != NV_ENC_SUCCESS {
        return false;
    }

    type OpenSessionEx = unsafe extern "C" fn(*mut OpenSessionParams, *mut *mut c_void) -> c_int;
    type GetGuidCount = unsafe extern "C" fn(*mut c_void, *mut u32) -> c_int;
    type GetGuids = unsafe extern "C" fn(*mut c_void, *mut Guid, u32, *mut u32) -> c_int;
    type DestroyEncoder = unsafe extern "C" fn(*mut c_void) -> c_int;

    let entry = |index: usize| functions.functions[index];
    if [
        FN_GET_ENCODE_GUID_COUNT,
        FN_GET_ENCODE_GUIDS,
        FN_DESTROY_ENCODER,
        FN_OPEN_ENCODE_SESSION_EX,
    ]
    .iter()
    .any(|&index| entry(index).is_null())
    {
        return false;
    }
    let open_session: OpenSessionEx = mem::transmute(entry(FN_OPEN_ENCODE_SESSION_EX));
    let get_guid_count: GetGuidCount = mem::transmute(entry(FN_GET_ENCODE_GUID_COUNT));
    let get_guids: GetGuids = mem::transmute(entry(FN_GET_ENCODE_GUIDS));
    let destroy_encoder: DestroyEncoder = mem::transmute(entry(FN_DESTROY_ENCODER));

    let mut params: Box<OpenSessionParams> = Box::new(mem::zeroed());
    params.version = nvenc_struct_version(1);
    params.device_type = NV_ENC_DEVICE_TYPE_CUDA;
    params.device = context;
    params.api_version = NVENC_API_VERSION;

    let mut encoder: *mut c_void = ptr::null_mut();
    if open_session(&mut *params, &mut encoder) != NV_ENC_SUCCESS || encoder.is_null() {
        return false;
    }

    let mut count = 0u32;
    let mut has_h264 = false;
    if get_guid_count(encoder, &mut count) == NV_ENC_SUCCESS && count > 0 {
        let mut guids = vec![
            Guid {
                data1: 0,
                data2: 0,
                data3: 0,
                data4: [0; 8],
            };
            count as usize
        ];
        let mut filled = 0u32;
        if get_guids(encoder, guids.as_mut_ptr(), count, &mut filled) == NV_ENC_SUCCESS {
            guids.truncate(filled.min(count) as usize);
            has_h264 = guids.contains(&NV_ENC_CODEC_H264_GUID);
        }
    }
    destroy_encoder(encoder);

    has_h264
}

// ---- NVDEC ----

const CODEC_H264: u32 = 4;
const CODEC_JPEG: u32 = 5;
const CHROMA_FORMAT_420: u32 = 1;

// CUVIDDECODECAPS: the first fields are inputs, the rest is filled by the driver
#[repr(C)]
#[allow(dead_code)]
struct DecodeCaps {
    codec_type: u32,
    chroma_format: u32,
    bit_depth_minus8: u32,
    reserved1: [u32; 3],
    is_supported: u8,
    num_nvdecs: u8,
    output_format_mask: u16,
    max_width: u32,
    max_height: u32,
    max_mb_count: u32,
    min_width: u16,
    min_height: u16,
    is_histogram_supported: u8,
    counter_bit_depth: u8,
    max_histogram_bins: u16,
    reserved3: [u32; 10],
}

// Needs a current CUDA context
unsafe fn nvdec_supports(codec: u32) -> bool {
    let Ok(lib) = Library::new("libnvcuvid.so.1") else {
        return false;
    };
    let Ok(get_decoder_caps) =
        lib.get::<unsafe extern "C" fn(*mut DecodeCaps) -> CuResult>(b"cuvidGetDecoderCaps\0")
    else {
        return false;
    };

    let mut caps: DecodeCaps = mem::zeroed();
    caps.codec_type = codec;
    caps.chroma_format = CHROMA_FORMAT_420;
    get_decoder_caps(&mut caps) == CUDA_SUCCESS && caps.is_supported != 0
}

fn check(result: CuResult, call: &str) -> Result<(), String> {
    if result == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(format!("{call} failed with error {result}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_pci_addresses_with_any_domain_width() {
        assert_eq!(parse_pci("0000:01:00.0"), Some((0, 1, 0, 0)));
        assert_eq!(parse_pci("00000000:01:00.0"), parse_pci("0000:01:00.0"));
        assert_eq!(parse_pci("0000:0A:1f.3"), Some((0, 0x0a, 0x1f, 3)));
        assert_eq!(parse_pci("garbage"), None);
    }

    #[test]
    fn struct_versions_match_the_sdk_macros() {
        // NV_ENCODE_API_FUNCTION_LIST_VER and NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER for SDK 11.0
        assert_eq!(nvenc_struct_version(2), 0x7002_000b);
        assert_eq!(nvenc_struct_version(1), 0x7001_000b);
    }
}
