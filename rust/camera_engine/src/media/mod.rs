// Platform independent side of the engine: the contracts every native backend implements
// (capture, preview, encoder) and the types shared between them and the RTSP server.
// CameraController only talks to these traits, through the NativeBackend picked at compile time
pub(crate) mod hub;

use std::sync::{atomic::AtomicBool, Arc};

use crossbeam_channel::Receiver;
use parking_lot::Mutex;

pub(crate) use hub::FrameHub;

use crate::{CameraError, VideoDimensions};

#[cfg(windows)]
pub(crate) type NativeBackend = crate::win32::Win32Backend;
#[cfg(target_os = "linux")]
pub(crate) type NativeBackend = crate::linux::LinuxBackend;
#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("camera_engine supports only Windows and Linux");

pub(crate) type NativeGpu = <NativeBackend as Backend>::Gpu;
pub(crate) type NativeFrame = <NativeBackend as Backend>::Frame;
pub(crate) type NativeCapture = <NativeBackend as Backend>::Capture;
pub(crate) type NativeRenderer = <NativeBackend as Backend>::Renderer;
pub(crate) type NativeEncoder = <NativeBackend as Backend>::Encoder;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRate {
    pub numerator: u32,
    pub denominator: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub frame_rate: FrameRate,
    pub bitrate: u32,
}

// One H.264 access unit in Annex B format (start codes + NAL units)
pub struct EncodedFrame {
    pub data: Vec<u8>,
    // 100ns units, same clock as the captured frames
    pub timestamp_hns: i64,
}

pub type EncodedSink = Box<dyn FnMut(EncodedFrame) + Send>;
pub type SharedEncodedFrame = Arc<EncodedFrame>;
pub type KeyframeRequest = Arc<dyn Fn() + Send + Sync>;

// H.264 produced by the camera itself, streamed as is instead of being encoded again
#[derive(Clone)]
pub(crate) struct CameraH264 {
    pub frames: Arc<FrameHub<SharedEncodedFrame>>,
    // Asks the camera for an IDR, best effort: many cameras only follow their own GOP
    pub request_keyframe: KeyframeRequest,
}

// Latest output size requested by the UI, consumed by the render thread
pub type PendingSize = Arc<Mutex<Option<(u32, u32)>>>;

// Ties together the types of one platform. Frames are opaque GPU handles: only the
// renderer and encoder of the same backend know how to read them
pub(crate) trait Backend: Sized + 'static {
    // Device shared by capture, previews and encoder, so frames never cross devices
    type Gpu: Clone + Send + 'static;
    // Cheap to clone (reference counted), returns to the capture pool when the last clone drops
    type Frame: Clone + Send + 'static;
    type Capture: CaptureSource<Self>;
    type Renderer: PreviewRenderer<Self>;
    type Encoder: VideoEncoder<Self>;

    fn create_gpu() -> Result<Self::Gpu, CameraError>;
}

// Camera producing frames into the hub
pub(crate) trait CaptureSource<B: Backend>: Sized + Send + Sync + 'static {
    async fn open() -> Result<Self, CameraError>;
    fn dimensions(&self) -> VideoDimensions;
    fn frame_rate(&self) -> FrameRate;
    async fn start(&self, gpu: B::Gpu, hub: Arc<FrameHub<B::Frame>>) -> Result<(), CameraError>;
    async fn stop(&self) -> Result<(), CameraError>;
    // Some while capturing from a camera that sends H.264: streaming then skips the encoder
    fn camera_h264(&self) -> Option<CameraH264>;
}

// Draws frames into a native window owned by the UI (HWND on Windows, X11 Window on Linux)
pub(crate) trait PreviewRenderer<B: Backend>: Sized + Send + 'static {
    fn new(gpu: B::Gpu, window_handle: u64, width: u32, height: u32) -> Result<Self, CameraError>;
    // Blocks the calling thread, rendering every frame until shutdown is signaled
    // (message sent or sender dropped)
    fn run(
        &self,
        frames: Receiver<B::Frame>,
        shutdown: Receiver<()>,
        pending_size: PendingSize,
    ) -> Result<(), CameraError>;
    fn stop(&self);
}

// Hardware H.264 encoder running on its own thread until dropped
pub(crate) trait VideoEncoder<B: Backend>: Sized + Send + 'static {
    fn spawn(
        gpu: B::Gpu,
        config: EncoderConfig,
        frames: Receiver<B::Frame>,
        // Set to true to make the next encoded frame an IDR
        force_keyframe: Arc<AtomicBool>,
        sink: EncodedSink,
    ) -> Result<Self, CameraError>;
}
