// Kernel ABI of V4L2 (linux/videodev2.h), only the parts the capture uses
use std::{
    ffi::{c_ulong, c_void},
    io,
    mem::{self, size_of},
    os::fd::RawFd,
};

pub const fn fourcc(code: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*code)
}

pub const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
pub const MEMORY_MMAP: u32 = 1;
pub const FIELD_NONE: u32 = 1;

pub const CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
pub const CAP_STREAMING: u32 = 0x0400_0000;
pub const CAP_DEVICE_CAPS: u32 = 0x8000_0000;
pub const CAP_TIMEPERFRAME: u32 = 0x1000;

pub const FRMSIZE_TYPE_DISCRETE: u32 = 1;
pub const FRMIVAL_TYPE_DISCRETE: u32 = 1;

pub const BUF_FLAG_KEYFRAME: u32 = 0x0000_0008;
pub const BUF_FLAG_ERROR: u32 = 0x0000_0040;

#[repr(C)]
pub struct Capability {
    pub driver: [u8; 16],
    pub card: [u8; 32],
    pub bus_info: [u8; 32],
    pub version: u32,
    pub capabilities: u32,
    pub device_caps: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
pub struct FmtDesc {
    pub index: u32,
    pub buf_type: u32,
    pub flags: u32,
    pub description: [u8; 32],
    pub pixelformat: u32,
    pub mbus_code: u32,
    pub reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PixFormat {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub private: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

// 200 bytes, 8 byte aligned: other members (v4l2_window) hold pointers
#[repr(C)]
#[derive(Clone, Copy)]
pub union FormatUnion {
    pub pix: PixFormat,
    raw: [u64; 25],
}

#[repr(C)]
pub struct Format {
    pub buf_type: u32,
    pub fmt: FormatUnion,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fract {
    pub numerator: u32,
    pub denominator: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct CaptureParm {
    pub capability: u32,
    pub capturemode: u32,
    // Seconds per frame
    pub timeperframe: Fract,
    pub extendedmode: u32,
    pub readbuffers: u32,
    pub reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union ParmUnion {
    pub capture: CaptureParm,
    raw: [u8; 200],
}

#[repr(C)]
pub struct StreamParm {
    pub buf_type: u32,
    pub parm: ParmUnion,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FrmSizeDiscrete {
    pub width: u32,
    pub height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FrmSizeStepwise {
    pub min_width: u32,
    pub max_width: u32,
    pub step_width: u32,
    pub min_height: u32,
    pub max_height: u32,
    pub step_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FrmSizeUnion {
    pub discrete: FrmSizeDiscrete,
    pub stepwise: FrmSizeStepwise,
}

#[repr(C)]
pub struct FrmSizeEnum {
    pub index: u32,
    pub pixel_format: u32,
    pub size_type: u32,
    pub size: FrmSizeUnion,
    pub reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct FrmIvalStepwise {
    pub min: Fract,
    pub max: Fract,
    pub step: Fract,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union FrmIvalUnion {
    pub discrete: Fract,
    pub stepwise: FrmIvalStepwise,
}

#[repr(C)]
pub struct FrmIvalEnum {
    pub index: u32,
    pub pixel_format: u32,
    pub width: u32,
    pub height: u32,
    pub ival_type: u32,
    pub interval: FrmIvalUnion,
    pub reserved: [u32; 2],
}

#[repr(C)]
pub struct RequestBuffers {
    pub count: u32,
    pub buf_type: u32,
    pub memory: u32,
    pub capabilities: u32,
    pub flags: u8,
    pub reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Timecode {
    pub timecode_type: u32,
    pub flags: u32,
    pub frames: u8,
    pub seconds: u8,
    pub minutes: u8,
    pub hours: u8,
    pub userbits: [u8; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union BufferMemory {
    pub offset: u32,
    pub userptr: c_ulong,
    pub planes: *mut c_void,
    pub fd: i32,
}

#[repr(C)]
pub struct Buffer {
    pub index: u32,
    pub buf_type: u32,
    pub bytesused: u32,
    pub flags: u32,
    pub field: u32,
    pub timestamp: libc::timeval,
    pub timecode: Timecode,
    pub sequence: u32,
    pub memory: u32,
    pub m: BufferMemory,
    pub length: u32,
    pub reserved2: u32,
    pub request_fd: i32,
}

#[repr(C)]
pub struct ExportBuffer {
    pub buf_type: u32,
    pub index: u32,
    pub plane: u32,
    pub flags: u32,
    pub fd: i32,
    pub reserved: [u32; 11],
}

// Sizes from the kernel headers on 64-bit targets: a layout mistake fails the build
#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Capability>() == 104);
    assert!(size_of::<FmtDesc>() == 64);
    assert!(size_of::<Format>() == 208);
    assert!(size_of::<StreamParm>() == 204);
    assert!(size_of::<FrmSizeEnum>() == 44);
    assert!(size_of::<FrmIvalEnum>() == 52);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<Buffer>() == 88);
    assert!(size_of::<ExportBuffer>() == 64);
};

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr) as c_ulong
}

pub const VIDIOC_QUERYCAP: c_ulong = ioc(IOC_READ, 0, size_of::<Capability>());
pub const VIDIOC_ENUM_FMT: c_ulong = ioc(IOC_READ | IOC_WRITE, 2, size_of::<FmtDesc>());
pub const VIDIOC_S_FMT: c_ulong = ioc(IOC_READ | IOC_WRITE, 5, size_of::<Format>());
pub const VIDIOC_REQBUFS: c_ulong = ioc(IOC_READ | IOC_WRITE, 8, size_of::<RequestBuffers>());
pub const VIDIOC_QUERYBUF: c_ulong = ioc(IOC_READ | IOC_WRITE, 9, size_of::<Buffer>());
pub const VIDIOC_QBUF: c_ulong = ioc(IOC_READ | IOC_WRITE, 15, size_of::<Buffer>());
pub const VIDIOC_EXPBUF: c_ulong = ioc(IOC_READ | IOC_WRITE, 16, size_of::<ExportBuffer>());
pub const VIDIOC_DQBUF: c_ulong = ioc(IOC_READ | IOC_WRITE, 17, size_of::<Buffer>());
pub const VIDIOC_STREAMON: c_ulong = ioc(IOC_WRITE, 18, size_of::<i32>());
pub const VIDIOC_STREAMOFF: c_ulong = ioc(IOC_WRITE, 19, size_of::<i32>());
pub const VIDIOC_G_PARM: c_ulong = ioc(IOC_READ | IOC_WRITE, 21, size_of::<StreamParm>());
pub const VIDIOC_S_PARM: c_ulong = ioc(IOC_READ | IOC_WRITE, 22, size_of::<StreamParm>());
pub const VIDIOC_ENUM_FRAMESIZES: c_ulong = ioc(IOC_READ | IOC_WRITE, 74, size_of::<FrmSizeEnum>());
pub const VIDIOC_ENUM_FRAMEINTERVALS: c_ulong =
    ioc(IOC_READ | IOC_WRITE, 75, size_of::<FrmIvalEnum>());

// Every struct here is plain data where all zero bytes is a valid value
pub fn zeroed<T>() -> T {
    unsafe { mem::zeroed() }
}

/// # Safety
/// `arg` must point to the struct the request expects
pub unsafe fn ioctl<T>(fd: RawFd, request: c_ulong, arg: *mut T) -> io::Result<()> {
    loop {
        if libc::ioctl(fd, request as _, arg) != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

// Fixed size, NUL padded strings of the kernel structs
pub fn text(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_codes_match_the_kernel_headers() {
        // Values of the macros as compiled on x86_64
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(fourcc(b"MJPG"), 0x4750_4a4d);
    }
}
