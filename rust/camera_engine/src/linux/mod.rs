// Placeholder backend so the crate builds on Linux: every entry point reports that the
// platform is not supported yet. The types are uninhabited, no value of them ever exists.
// To be replaced by V4L2 capture, VA-API/NVENC encoding and a Vulkan/EGL preview
// Used by the backend once capture and encoding land
#[allow(dead_code, unused_imports)]
pub(crate) mod gpu;
#[allow(dead_code, unused_imports)]
pub(crate) mod v4l2;

use std::sync::{atomic::AtomicBool, Arc};

use crossbeam_channel::Receiver;

use crate::{
    media::{
        Backend, CaptureSource, EncodedSink, EncoderConfig, FrameHub, FrameRate, PendingSize,
        PreviewRenderer, VideoEncoder,
    },
    CameraError, VideoDimensions,
};

pub(crate) struct LinuxBackend;

#[derive(Clone)]
pub(crate) enum LinuxGpu {}

#[derive(Clone)]
pub(crate) enum LinuxFrame {}

pub(crate) enum LinuxCapture {}

pub(crate) enum LinuxRenderer {}

pub(crate) enum LinuxEncoder {}

impl Backend for LinuxBackend {
    type Gpu = LinuxGpu;
    type Frame = LinuxFrame;
    type Capture = LinuxCapture;
    type Renderer = LinuxRenderer;
    type Encoder = LinuxEncoder;

    fn create_gpu() -> Result<LinuxGpu, CameraError> {
        Err(not_supported())
    }
}

impl CaptureSource<LinuxBackend> for LinuxCapture {
    async fn open() -> Result<Self, CameraError> {
        Err(not_supported())
    }

    fn dimensions(&self) -> VideoDimensions {
        match *self {}
    }

    fn frame_rate(&self) -> FrameRate {
        match *self {}
    }

    async fn start(
        &self,
        _gpu: LinuxGpu,
        _hub: Arc<FrameHub<LinuxFrame>>,
    ) -> Result<(), CameraError> {
        match *self {}
    }

    async fn stop(&self) -> Result<(), CameraError> {
        match *self {}
    }
}

impl PreviewRenderer<LinuxBackend> for LinuxRenderer {
    fn new(
        gpu: LinuxGpu,
        _window_handle: u64,
        _width: u32,
        _height: u32,
    ) -> Result<Self, CameraError> {
        match gpu {}
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
        gpu: LinuxGpu,
        _config: EncoderConfig,
        _frames: Receiver<LinuxFrame>,
        _force_keyframe: Arc<AtomicBool>,
        _sink: EncodedSink,
    ) -> Result<Self, CameraError> {
        match gpu {}
    }
}

fn not_supported() -> CameraError {
    CameraError::CommonError {
        msg: "Camera is not supported on Linux yet".to_string(),
    }
}
