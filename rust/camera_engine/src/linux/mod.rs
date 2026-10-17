// Linux backend. Capture runs on V4L2, and cameras that send H.264 are streamed as is.
// The GPU stages are not implemented yet and report so: H.264 decode for the preview
// (VA-API/NVDEC), MJPEG and raw cameras (decode + VA-API/NVENC encode) and the preview
// itself (Vulkan/EGL on the window)
// Parts of these modules are used only once the GPU stages land
#[allow(dead_code, unused_imports)]
pub(crate) mod gpu;
#[allow(dead_code, unused_imports)]
pub(crate) mod v4l2;

use std::sync::{atomic::AtomicBool, Arc};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;

use crate::{
    media::{
        Backend, CameraH264, CaptureSource, EncodedFrame, EncodedSink, EncoderConfig, FrameHub,
        FrameRate, PendingSize, PreviewRenderer, SharedEncodedFrame, VideoEncoder,
    },
    CameraError, VideoDimensions,
};
use gpu::{GpuInfo, GpuPreference};
use v4l2::{CaptureMode, CaptureStream, PixelFormat};

pub(crate) struct LinuxBackend;

// GPUs found when the controller first needs one; each stage picks its own with gpu::plan
#[derive(Clone)]
#[allow(dead_code)] // Read by the GPU stages once they land
pub(crate) struct LinuxGpu {
    pub gpus: Arc<Vec<GpuInfo>>,
    pub preference: GpuPreference,
}

// No value exists yet: GPU frames come from the decode stage, not implemented
#[derive(Clone)]
pub(crate) enum LinuxFrame {}

pub(crate) struct LinuxCapture {
    // Updated with what the driver accepted once streaming starts
    mode: Mutex<CaptureMode>,
    stream: Mutex<Option<CaptureStream>>,
    h264: Arc<FrameHub<SharedEncodedFrame>>,
}

pub(crate) enum LinuxRenderer {}

pub(crate) enum LinuxEncoder {}

impl Backend for LinuxBackend {
    type Gpu = LinuxGpu;
    type Frame = LinuxFrame;
    type Capture = LinuxCapture;
    type Renderer = LinuxRenderer;
    type Encoder = LinuxEncoder;

    // Never fails: H.264 passthrough streams without any GPU
    fn create_gpu() -> Result<LinuxGpu, CameraError> {
        Ok(LinuxGpu {
            gpus: Arc::new(gpu::probe()),
            preference: GpuPreference::from_env(),
        })
    }
}

impl CaptureSource<LinuxBackend> for LinuxCapture {
    async fn open() -> Result<Self, CameraError> {
        let devices = v4l2::list_devices();
        let device = devices
            .first()
            .ok_or_else(|| common("No camera found".to_string()))?;
        // Only H.264 needs no GPU stage to stream, the other formats wait for the transcoder
        let mode =
            v4l2::choose_mode(device, |format| format == PixelFormat::H264).ok_or_else(|| {
                common(format!(
                    "{} does not send H.264. MJPEG and raw cameras are not supported \
                     on Linux yet",
                    device.name
                ))
            })?;

        Ok(LinuxCapture {
            mode: Mutex::new(mode),
            stream: Mutex::new(None),
            h264: Arc::new(FrameHub::default()),
        })
    }

    fn dimensions(&self) -> VideoDimensions {
        let mode = self.mode.lock();
        VideoDimensions {
            width: mode.width,
            height: mode.height,
        }
    }

    fn frame_rate(&self) -> FrameRate {
        self.mode.lock().frame_rate
    }

    async fn start(
        &self,
        _gpu: LinuxGpu,
        _hub: Arc<FrameHub<LinuxFrame>>,
    ) -> Result<(), CameraError> {
        let mut stream = self.stream.lock();
        if stream.is_some() {
            return Ok(());
        }

        let h264 = self.h264.clone();
        let mode = self.mode.lock().clone();
        let started = CaptureStream::start(&mode, move |frame| {
            // The buffer goes back to the camera after this call, so it is copied: it is
            // compressed, tens of KB per frame
            h264.publish(Arc::new(EncodedFrame {
                data: frame.data.to_vec(),
                timestamp_hns: frame.timestamp_hns,
            }));
        })?;

        *self.mode.lock() = started.mode().clone();
        *stream = Some(started);
        Ok(())
    }

    async fn stop(&self) -> Result<(), CameraError> {
        // Joins the capture thread outside the lock
        let stream = self.stream.lock().take();
        drop(stream);
        Ok(())
    }

    fn camera_h264(&self) -> Option<CameraH264> {
        let requester = self.stream.lock().as_ref()?.keyframe_requester();
        Some(CameraH264 {
            frames: self.h264.clone(),
            request_keyframe: Arc::new(move || requester.request()),
        })
    }
}

impl PreviewRenderer<LinuxBackend> for LinuxRenderer {
    fn new(
        _gpu: LinuxGpu,
        _window_handle: u64,
        _width: u32,
        _height: u32,
    ) -> Result<Self, CameraError> {
        Err(common(
            "Camera preview is not supported on Linux yet".to_string(),
        ))
    }

    fn run(
        &self,
        _frames: Receiver<LinuxFrame>,
        _shutdown: Receiver<()>,
        _pending_size: PendingSize,
    ) -> Result<(), CameraError> {
        match *self {}
    }

    fn stop(&self) {
        match *self {}
    }
}

impl VideoEncoder<LinuxBackend> for LinuxEncoder {
    fn spawn(
        _gpu: LinuxGpu,
        _config: EncoderConfig,
        _frames: Receiver<LinuxFrame>,
        _force_keyframe: Arc<AtomicBool>,
        _sink: EncodedSink,
    ) -> Result<Self, CameraError> {
        Err(common(
            "H.264 encoding is not supported on Linux yet".to_string(),
        ))
    }
}

fn common(msg: String) -> CameraError {
    CameraError::CommonError { msg }
}
