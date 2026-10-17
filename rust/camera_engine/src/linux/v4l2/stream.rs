use std::{
    fs::File,
    io,
    os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    ptr, slice,
    sync::Arc,
    thread::JoinHandle,
};

use super::{open_node, sys, CaptureMode};
use crate::{media::FrameRate, CameraError};

// Enough for the frame being handed out, one being filled and slack for a slow consumer
const BUFFER_COUNT: u32 = 6;
// Wakes up now and then even if the camera stalls, so a hung device is noticed
const POLL_TIMEOUT_MS: i32 = 2000;

// One dequeued buffer, valid only during the callback: it goes back to the camera after it
pub(crate) struct CapturedFrame<'a> {
    pub data: &'a [u8],
    // Same memory as `data`, exported for uncompressed formats so the GPU imports it directly
    pub dmabuf: Option<BorrowedFd<'a>>,
    // 100ns units since the first frame, from the driver's monotonic timestamps
    pub timestamp_hns: i64,
    pub sequence: u32,
    // Only set by drivers that know it; UVC cameras leave it off, so H.264 is parsed instead
    pub keyframe: bool,
}

// Capture thread handle. Dropping it stops the camera and joins the thread
pub(crate) struct CaptureStream {
    mode: CaptureMode,
    // Second descriptor of the camera, for controls sent while the thread streams
    control: Arc<File>,
    stop: Arc<OwnedFd>,
    thread: Option<JoinHandle<()>>,
}

impl CaptureStream {
    // Configures the camera on the calling thread, so errors (busy device, refused format)
    // reach the caller, then streams on a dedicated thread
    pub fn start(
        mode: &CaptureMode,
        mut on_frame: impl FnMut(CapturedFrame<'_>) + Send + 'static,
    ) -> Result<Self, CameraError> {
        let mut device = Device::open(mode)?;
        let mode = device.mode.clone();
        let control = Arc::new(
            device
                .file
                .try_clone()
                .map_err(|e| v4l2_error("duplicate the camera descriptor", e))?,
        );
        device.start_streaming()?;

        let stop = Arc::new(event_fd()?);
        let thread_stop = stop.clone();
        let thread = std::thread::Builder::new()
            .name("v4l2-capture".to_string())
            .spawn(move || {
                if let Err(e) = device.run(thread_stop.as_fd(), &mut on_frame) {
                    println!("Camera capture stopped: {e}");
                }
            })
            .map_err(|e| common(format!("Failed to start the capture thread: {e}")))?;

        Ok(CaptureStream {
            mode,
            control,
            stop,
            thread: Some(thread),
        })
    }

    // The mode the driver accepted, which may differ slightly from the one requested
    pub fn mode(&self) -> &CaptureMode {
        &self.mode
    }

    pub fn keyframe_requester(&self) -> KeyframeRequester {
        KeyframeRequester(self.control.clone())
    }
}

// Asks an H.264 camera for an IDR. Best effort: cameras without the codec control (most
// UVC ones) ignore it and the client waits for the next IDR of the camera's own GOP
#[derive(Clone)]
pub(crate) struct KeyframeRequester(Arc<File>);

impl KeyframeRequester {
    pub fn request(&self) {
        let mut control = sys::Control {
            id: sys::CID_MPEG_VIDEO_FORCE_KEY_FRAME,
            value: 1,
        };
        let _ = unsafe { sys::ioctl(self.0.as_raw_fd(), sys::VIDIOC_S_CTRL, &mut control) };
    }
}

impl Drop for CaptureStream {
    fn drop(&mut self) {
        let one = 1u64;
        unsafe {
            libc::write(
                self.stop.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                size_of::<u64>(),
            );
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct MappedBuffer {
    ptr: *mut u8,
    length: usize,
    dmabuf: Option<OwnedFd>,
}

// Drop unmaps the buffers before `file` closes the device
struct Device {
    file: File,
    mode: CaptureMode,
    buffers: Vec<MappedBuffer>,
    streaming: bool,
}

// The mappings are only touched by the thread that owns the Device
unsafe impl Send for Device {}

impl Device {
    fn open(requested: &CaptureMode) -> Result<Self, CameraError> {
        let file = open_node(&requested.node)
            .map_err(|e| common(format!("Cannot open {}: {e}", requested.node.display())))?;
        let mut device = Device {
            file,
            mode: requested.clone(),
            buffers: Vec::new(),
            streaming: false,
        };
        device.set_format()?;
        device.set_frame_rate()?;
        device.map_buffers()?;
        Ok(device)
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn set_format(&mut self) -> Result<(), CameraError> {
        let mut format: sys::Format = sys::zeroed();
        format.buf_type = sys::BUF_TYPE_VIDEO_CAPTURE;
        format.fmt.pix = sys::PixFormat {
            width: self.mode.width,
            height: self.mode.height,
            pixelformat: self.mode.format.fourcc(),
            field: sys::FIELD_NONE,
            ..sys::zeroed()
        };
        unsafe { sys::ioctl(self.fd(), sys::VIDIOC_S_FMT, &mut format) }
            .map_err(|e| v4l2_error("set the format", e))?;

        // The driver adjusts what it cannot do instead of failing
        let accepted = unsafe { format.fmt.pix };
        if accepted.pixelformat != self.mode.format.fourcc() {
            return Err(common(format!(
                "Camera refused the {:?} format",
                self.mode.format
            )));
        }
        self.mode.width = accepted.width;
        self.mode.height = accepted.height;
        Ok(())
    }

    fn set_frame_rate(&mut self) -> Result<(), CameraError> {
        let mut parm: sys::StreamParm = sys::zeroed();
        parm.buf_type = sys::BUF_TYPE_VIDEO_CAPTURE;
        unsafe { sys::ioctl(self.fd(), sys::VIDIOC_G_PARM, &mut parm) }
            .map_err(|e| v4l2_error("read the frame rate", e))?;
        if unsafe { parm.parm.capture.capability } & sys::CAP_TIMEPERFRAME == 0 {
            // Fixed rate camera: keep whatever it runs at
            return Ok(());
        }

        parm.parm.capture.timeperframe = sys::Fract {
            numerator: self.mode.frame_rate.denominator,
            denominator: self.mode.frame_rate.numerator,
        };
        unsafe { sys::ioctl(self.fd(), sys::VIDIOC_S_PARM, &mut parm) }
            .map_err(|e| v4l2_error("set the frame rate", e))?;

        let accepted = unsafe { parm.parm.capture.timeperframe };
        if accepted.numerator > 0 && accepted.denominator > 0 {
            self.mode.frame_rate = FrameRate {
                numerator: accepted.denominator,
                denominator: accepted.numerator,
            };
        }
        Ok(())
    }

    fn map_buffers(&mut self) -> Result<(), CameraError> {
        let mut request: sys::RequestBuffers = sys::zeroed();
        request.count = BUFFER_COUNT;
        request.buf_type = sys::BUF_TYPE_VIDEO_CAPTURE;
        request.memory = sys::MEMORY_MMAP;
        unsafe { sys::ioctl(self.fd(), sys::VIDIOC_REQBUFS, &mut request) }
            .map_err(|e| v4l2_error("allocate buffers", e))?;
        if request.count < 2 {
            return Err(common("Camera allocated too few buffers".to_string()));
        }

        // Compressed frames are parsed and uploaded by the CPU, only raw ones go to the GPU
        // as they are, so only those are exported
        let export = !self.mode.format.is_compressed();
        for index in 0..request.count {
            let mut buffer = self.buffer(index);
            unsafe { sys::ioctl(self.fd(), sys::VIDIOC_QUERYBUF, &mut buffer) }
                .map_err(|e| v4l2_error("query a buffer", e))?;

            let length = buffer.length as usize;
            let mapped = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    length,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    self.fd(),
                    buffer.m.offset as libc::off_t,
                )
            };
            if mapped == libc::MAP_FAILED {
                return Err(v4l2_error("map a buffer", io::Error::last_os_error()));
            }
            // Pushed right away so Drop unmaps it even if exporting fails below
            self.buffers.push(MappedBuffer {
                ptr: mapped as *mut u8,
                length,
                dmabuf: None,
            });

            if export {
                let mut export_buffer: sys::ExportBuffer = sys::zeroed();
                export_buffer.buf_type = sys::BUF_TYPE_VIDEO_CAPTURE;
                export_buffer.index = index;
                export_buffer.flags = (libc::O_RDONLY | libc::O_CLOEXEC) as u32;
                unsafe { sys::ioctl(self.fd(), sys::VIDIOC_EXPBUF, &mut export_buffer) }
                    .map_err(|e| v4l2_error("export a buffer as DMA-BUF", e))?;
                self.buffers[index as usize].dmabuf =
                    Some(unsafe { OwnedFd::from_raw_fd(export_buffer.fd) });
            }
        }
        Ok(())
    }

    fn buffer(&self, index: u32) -> sys::Buffer {
        let mut buffer: sys::Buffer = sys::zeroed();
        buffer.index = index;
        buffer.buf_type = sys::BUF_TYPE_VIDEO_CAPTURE;
        buffer.memory = sys::MEMORY_MMAP;
        buffer
    }

    fn queue(&self, index: u32) -> Result<(), CameraError> {
        let mut buffer = self.buffer(index);
        unsafe { sys::ioctl(self.fd(), sys::VIDIOC_QBUF, &mut buffer) }
            .map_err(|e| v4l2_error("queue a buffer", e))
    }

    fn start_streaming(&mut self) -> Result<(), CameraError> {
        for index in 0..self.buffers.len() as u32 {
            self.queue(index)?;
        }
        let mut buf_type = sys::BUF_TYPE_VIDEO_CAPTURE as i32;
        unsafe { sys::ioctl(self.fd(), sys::VIDIOC_STREAMON, &mut buf_type) }
            .map_err(|e| v4l2_error("start streaming", e))?;
        self.streaming = true;
        Ok(())
    }

    fn run(
        &mut self,
        stop: BorrowedFd<'_>,
        on_frame: &mut impl FnMut(CapturedFrame<'_>),
    ) -> Result<(), CameraError> {
        let mut first_timestamp: Option<i64> = None;
        let mut fds = [
            libc::pollfd {
                fd: self.fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: stop.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];

        loop {
            let ready =
                unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_TIMEOUT_MS) };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(v4l2_error("wait for frames", error));
            }
            if fds[1].revents != 0 {
                return Ok(());
            }
            if fds[0].revents & (libc::POLLERR | libc::POLLHUP) != 0 {
                return Err(common("Camera disconnected".to_string()));
            }
            if fds[0].revents & libc::POLLIN == 0 {
                continue;
            }

            let mut buffer = self.buffer(0);
            match unsafe { sys::ioctl(self.fd(), sys::VIDIOC_DQBUF, &mut buffer) } {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(v4l2_error("dequeue a frame", e)),
            }

            // Corrupted frames (USB errors) are skipped, not shown
            if buffer.flags & sys::BUF_FLAG_ERROR == 0 && buffer.bytesused > 0 {
                let mapped = &self.buffers[buffer.index as usize];
                let length = (buffer.bytesused as usize).min(mapped.length);
                let timestamp =
                    buffer.timestamp.tv_sec * 10_000_000 + buffer.timestamp.tv_usec * 10;
                let first = *first_timestamp.get_or_insert(timestamp);

                on_frame(CapturedFrame {
                    data: unsafe { slice::from_raw_parts(mapped.ptr, length) },
                    dmabuf: mapped.dmabuf.as_ref().map(|fd| fd.as_fd()),
                    timestamp_hns: timestamp - first,
                    sequence: buffer.sequence,
                    keyframe: buffer.flags & sys::BUF_FLAG_KEYFRAME != 0,
                });
            }

            self.queue(buffer.index)?;
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        if self.streaming {
            let mut buf_type = sys::BUF_TYPE_VIDEO_CAPTURE as i32;
            let _ = unsafe { sys::ioctl(self.fd(), sys::VIDIOC_STREAMOFF, &mut buf_type) };
        }
        for buffer in self.buffers.drain(..) {
            unsafe { libc::munmap(buffer.ptr as *mut libc::c_void, buffer.length) };
            drop(buffer.dmabuf);
        }
        // Frees the driver's buffers now instead of when the file closes
        let mut request: sys::RequestBuffers = sys::zeroed();
        request.buf_type = sys::BUF_TYPE_VIDEO_CAPTURE;
        request.memory = sys::MEMORY_MMAP;
        let _ = unsafe { sys::ioctl(self.fd(), sys::VIDIOC_REQBUFS, &mut request) };
    }
}

fn event_fd() -> Result<OwnedFd, CameraError> {
    let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
    if fd < 0 {
        return Err(common(format!(
            "Failed to create an eventfd: {}",
            io::Error::last_os_error()
        )));
    }
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn v4l2_error(action: &str, error: io::Error) -> CameraError {
    let hint = if error.raw_os_error() == Some(libc::EBUSY) {
        " (camera in use by another application?)"
    } else {
        ""
    };
    common(format!("Failed to {action}: {error}{hint}"))
}

fn common(msg: String) -> CameraError {
    CameraError::CommonError { msg }
}
