// Camera capture straight from the kernel (V4L2): lists the cameras and their modes, picks
// the best mode the GPUs can handle and streams buffers from a memory mapped queue
mod stream;
mod sys;

use std::{
    cmp::Reverse,
    fs::{self, File, OpenOptions},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
};

pub(crate) use stream::{CaptureStream, CapturedFrame};

use crate::{linux::gpu::Workload, media::FrameRate};

// Below this the preview looks choppy; such modes are taken only when nothing else exists
const MIN_SMOOTH_FPS: f64 = 24.0;
// The stream is sized for 1080p: bigger modes win nothing and cost USB and GPU bandwidth
const MAX_USEFUL_PIXELS: u32 = 1920 * 1080;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PixelFormat {
    H264,
    Mjpeg,
    Nv12,
    Yuyv,
}

impl PixelFormat {
    const ALL: [PixelFormat; 4] = [
        PixelFormat::H264,
        PixelFormat::Mjpeg,
        PixelFormat::Nv12,
        PixelFormat::Yuyv,
    ];

    pub fn fourcc(self) -> u32 {
        sys::fourcc(match self {
            PixelFormat::H264 => b"H264",
            PixelFormat::Mjpeg => b"MJPG",
            PixelFormat::Nv12 => b"NV12",
            PixelFormat::Yuyv => b"YUYV",
        })
    }

    fn from_fourcc(code: u32) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.fourcc() == code)
    }

    pub fn is_compressed(self) -> bool {
        matches!(self, PixelFormat::H264 | PixelFormat::Mjpeg)
    }

    pub fn workload(self) -> Workload {
        match self {
            PixelFormat::H264 => Workload::H264Passthrough,
            PixelFormat::Mjpeg => Workload::MjpegTranscode,
            PixelFormat::Nv12 | PixelFormat::Yuyv => Workload::RawTranscode,
        }
    }

    // Lower is better: H.264 skips the encoder entirely, MJPEG is decoded by fixed function
    // hardware, raw frames need a shader pass and far more USB bandwidth
    fn rank(self) -> u8 {
        match self {
            PixelFormat::H264 => 0,
            PixelFormat::Mjpeg => 1,
            PixelFormat::Nv12 => 2,
            PixelFormat::Yuyv => 3,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CaptureMode {
    // A camera may offer each format on a different node, e.g. H.264 on its own /dev/videoN
    pub node: PathBuf,
    pub format: PixelFormat,
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
}

impl CaptureMode {
    fn fps(&self) -> f64 {
        self.frame_rate.numerator as f64 / self.frame_rate.denominator.max(1) as f64
    }
}

// One physical camera: every capture node with the same bus
#[derive(Debug, Clone)]
pub(crate) struct CaptureDevice {
    pub name: String,
    pub bus_info: String,
    pub modes: Vec<CaptureMode>,
}

pub(crate) fn list_devices() -> Vec<CaptureDevice> {
    let mut nodes: Vec<(u32, PathBuf)> = fs::read_dir("/dev")
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().into_string().ok()?;
                    let number = name.strip_prefix("video")?.parse().ok()?;
                    Some((number, entry.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    nodes.sort();

    let mut devices: Vec<CaptureDevice> = Vec::new();
    for (_, node) in nodes {
        let Some((name, bus_info, modes)) = probe_node(&node) else {
            continue;
        };
        match devices
            .iter_mut()
            .find(|device| device.bus_info == bus_info)
        {
            Some(device) => device.modes.extend(modes),
            None => devices.push(CaptureDevice {
                name,
                bus_info,
                modes,
            }),
        }
    }
    devices.retain(|device| !device.modes.is_empty());
    devices
}

// Best mode among the formats `usable` accepts (those the GPUs can decode or encode)
pub(crate) fn choose_mode(
    device: &CaptureDevice,
    usable: impl Fn(PixelFormat) -> bool,
) -> Option<CaptureMode> {
    device
        .modes
        .iter()
        .filter(|mode| usable(mode.format))
        .max_by_key(|mode| {
            let pixels = mode.width * mode.height;
            (
                mode.fps() >= MIN_SMOOTH_FPS,
                pixels.min(MAX_USEFUL_PIXELS),
                Reverse(mode.format.rank()),
                (mode.fps() * 1000.0) as u32,
                Reverse(pixels),
            )
        })
        .cloned()
}

fn open_node(node: &Path) -> std::io::Result<File> {
    // Non blocking: the stream waits with poll, and a busy camera must not hang the caller
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(node)
}

fn probe_node(node: &Path) -> Option<(String, String, Vec<CaptureMode>)> {
    let file = open_node(node).ok()?;
    let fd = file.as_raw_fd();

    let mut capability: sys::Capability = sys::zeroed();
    unsafe { sys::ioctl(fd, sys::VIDIOC_QUERYCAP, &mut capability) }.ok()?;
    // Metadata nodes of UVC cameras share the bus but cannot capture video
    let caps = if capability.capabilities & sys::CAP_DEVICE_CAPS != 0 {
        capability.device_caps
    } else {
        capability.capabilities
    };
    if caps & sys::CAP_VIDEO_CAPTURE == 0 || caps & sys::CAP_STREAMING == 0 {
        return None;
    }

    let mut modes = Vec::new();
    for format in enum_formats(fd) {
        for (width, height) in enum_sizes(fd, format) {
            if let Some(frame_rate) = max_frame_rate(fd, format, width, height) {
                modes.push(CaptureMode {
                    node: node.to_path_buf(),
                    format,
                    width,
                    height,
                    frame_rate,
                });
            }
        }
    }

    Some((
        sys::text(&capability.card),
        sys::text(&capability.bus_info),
        modes,
    ))
}

fn enum_formats(fd: i32) -> Vec<PixelFormat> {
    let mut formats = Vec::new();
    for index in 0.. {
        let mut desc: sys::FmtDesc = sys::zeroed();
        desc.index = index;
        desc.buf_type = sys::BUF_TYPE_VIDEO_CAPTURE;
        if unsafe { sys::ioctl(fd, sys::VIDIOC_ENUM_FMT, &mut desc) }.is_err() {
            break;
        }
        formats.extend(PixelFormat::from_fourcc(desc.pixelformat));
    }
    formats
}

fn enum_sizes(fd: i32, format: PixelFormat) -> Vec<(u32, u32)> {
    let mut sizes = Vec::new();
    for index in 0.. {
        let mut size: sys::FrmSizeEnum = sys::zeroed();
        size.index = index;
        size.pixel_format = format.fourcc();
        if unsafe { sys::ioctl(fd, sys::VIDIOC_ENUM_FRAMESIZES, &mut size) }.is_err() {
            break;
        }

        if size.size_type == sys::FRMSIZE_TYPE_DISCRETE {
            let discrete = unsafe { size.size.discrete };
            sizes.push((discrete.width, discrete.height));
        } else {
            // Continuous or stepwise range, reported once: offer its largest size and 1080p
            let range = unsafe { size.size.stepwise };
            sizes.push((range.max_width, range.max_height));
            let fits_1080p = (range.min_width..=range.max_width).contains(&1920)
                && (range.min_height..=range.max_height).contains(&1080);
            if fits_1080p && (range.max_width, range.max_height) != (1920, 1080) {
                sizes.push((1920, 1080));
            }
            break;
        }
    }
    sizes
}

// Intervals are seconds per frame, so the shortest one is the highest frame rate
fn max_frame_rate(fd: i32, format: PixelFormat, width: u32, height: u32) -> Option<FrameRate> {
    let mut shortest: Option<sys::Fract> = None;
    for index in 0.. {
        let mut interval: sys::FrmIvalEnum = sys::zeroed();
        interval.index = index;
        interval.pixel_format = format.fourcc();
        interval.width = width;
        interval.height = height;
        if unsafe { sys::ioctl(fd, sys::VIDIOC_ENUM_FRAMEINTERVALS, &mut interval) }.is_err() {
            break;
        }

        let candidate = if interval.ival_type == sys::FRMIVAL_TYPE_DISCRETE {
            unsafe { interval.interval.discrete }
        } else {
            unsafe { interval.interval.stepwise.min }
        };
        if candidate.numerator == 0 || candidate.denominator == 0 {
            continue;
        }
        let shorter = shortest.is_none_or(|current| {
            // a/b < c/d without floating point
            (candidate.numerator as u64 * current.denominator as u64)
                < (current.numerator as u64 * candidate.denominator as u64)
        });
        if shorter {
            shortest = Some(candidate);
        }
        if interval.ival_type != sys::FRMIVAL_TYPE_DISCRETE {
            break;
        }
    }

    shortest.map(|interval| FrameRate {
        numerator: interval.denominator,
        denominator: interval.numerator,
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    fn mode(format: PixelFormat, width: u32, height: u32, fps: u32) -> CaptureMode {
        CaptureMode {
            node: PathBuf::from("/dev/video0"),
            format,
            width,
            height,
            frame_rate: FrameRate {
                numerator: fps,
                denominator: 1,
            },
        }
    }

    fn device(modes: Vec<CaptureMode>) -> CaptureDevice {
        CaptureDevice {
            name: "Camera".to_string(),
            bus_info: "usb-1".to_string(),
            modes,
        }
    }

    #[test]
    fn prefers_smooth_frame_rate_over_resolution() {
        // The integrated webcam of the development laptop
        let camera = device(vec![
            mode(PixelFormat::Mjpeg, 1280, 720, 30),
            mode(PixelFormat::Mjpeg, 640, 480, 30),
            mode(PixelFormat::Yuyv, 1280, 720, 10),
            mode(PixelFormat::Yuyv, 640, 480, 30),
        ]);
        assert_eq!(
            choose_mode(&camera, |_| true),
            Some(mode(PixelFormat::Mjpeg, 1280, 720, 30))
        );
        // Without an MJPEG decoder, 720p at 10 fps loses to 480p at 30 fps
        assert_eq!(
            choose_mode(&camera, |format| format != PixelFormat::Mjpeg),
            Some(mode(PixelFormat::Yuyv, 640, 480, 30))
        );
    }

    #[test]
    fn prefers_h264_at_the_same_size_and_ignores_sizes_above_1080p() {
        let camera = device(vec![
            mode(PixelFormat::Mjpeg, 3840, 2160, 30),
            mode(PixelFormat::Mjpeg, 1920, 1080, 30),
            mode(PixelFormat::H264, 1920, 1080, 30),
        ]);
        assert_eq!(
            choose_mode(&camera, |_| true),
            Some(mode(PixelFormat::H264, 1920, 1080, 30))
        );
        // Same useful size: the smaller frame wins, 4K costs bandwidth for nothing
        assert_eq!(
            choose_mode(&camera, |format| format == PixelFormat::Mjpeg),
            Some(mode(PixelFormat::Mjpeg, 1920, 1080, 30))
        );
    }

    #[test]
    fn takes_choppy_modes_when_nothing_else_exists() {
        let camera = device(vec![mode(PixelFormat::Yuyv, 1280, 720, 10)]);
        assert_eq!(
            choose_mode(&camera, |_| true),
            Some(mode(PixelFormat::Yuyv, 1280, 720, 10))
        );
        assert_eq!(choose_mode(&camera, |_| false), None);
    }

    #[test]
    #[ignore = "opens the real camera, run with: cargo test -- --ignored --nocapture"]
    fn captures_from_the_camera() {
        let devices = list_devices();
        for device in &devices {
            println!("{} ({})", device.name, device.bus_info);
            for mode in &device.modes {
                println!(
                    "  {} {:?} {}x{} @ {:.2} fps",
                    mode.node.display(),
                    mode.format,
                    mode.width,
                    mode.height,
                    mode.fps()
                );
            }
        }

        // One run per format, so every path (H.264, MJPEG, raw + DMA-BUF) is exercised
        let device = devices.first().expect("no camera found");
        for format in PixelFormat::ALL {
            if let Some(mode) = choose_mode(device, |candidate| candidate == format) {
                capture_one_second(&mode);
            }
        }
    }

    fn capture_one_second(mode: &CaptureMode) {
        let (frames, receiver) = crossbeam_channel::unbounded();
        let stream = CaptureStream::start(mode, move |frame: CapturedFrame<'_>| {
            let _ = frames.send((
                frame.data.len(),
                frame.data.get(..4).map(<[u8]>::to_vec).unwrap_or_default(),
                frame.timestamp_hns,
                frame.dmabuf.is_some(),
            ));
        })
        .expect("failed to start the capture");
        println!("Streaming {:?}", stream.mode());

        // Plus slack for the camera to start
        let wanted = (mode.fps().ceil() as usize).max(2);
        let started = Instant::now();
        let mut received = Vec::new();
        while received.len() < wanted && started.elapsed() < Duration::from_secs(10) {
            if let Ok(frame) = receiver.recv_timeout(Duration::from_secs(1)) {
                received.push(frame);
            }
        }
        drop(stream);

        assert!(received.len() > 1, "no frames received");
        let (_, first_bytes, first_timestamp, has_dmabuf) = &received[0];
        let last_timestamp = received.last().unwrap().2;
        let average = received.iter().map(|frame| frame.0).sum::<usize>() / received.len();
        let fps = (received.len() - 1) as f64 / ((last_timestamp - first_timestamp) as f64 / 1e7);
        println!(
            "  {} frames, {fps:.2} fps by timestamps, {average} bytes on average, \
             first bytes {first_bytes:02x?}, DMA-BUF exported: {has_dmabuf}",
            received.len()
        );

        match mode.format {
            PixelFormat::Mjpeg => assert_eq!(&first_bytes[..2], &[0xff, 0xd8], "not a JPEG"),
            PixelFormat::H264 => assert!(
                first_bytes.starts_with(&[0, 0, 1]) || first_bytes.starts_with(&[0, 0, 0, 1]),
                "not Annex B H.264"
            ),
            PixelFormat::Nv12 | PixelFormat::Yuyv => assert!(has_dmabuf),
        }
    }
}
