// Linux backend. Capture runs on V4L2 and takes cameras that send H.264: the stream goes out
// as is, and the preview decodes it with VA-API and draws it with EGL on the X11 window.
// Not implemented yet: MJPEG and raw cameras (decode + VA-API/NVENC encode) and NVDEC
pub(crate) mod egl;
// Parts of these modules are used only once the remaining GPU stages land
#[allow(dead_code, unused_imports)]
pub(crate) mod gpu;
#[allow(dead_code, unused_imports)]
pub(crate) mod v4l2;
#[allow(dead_code, unused_imports)]
pub(crate) mod vaapi;

use std::{
    cell::RefCell,
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};

use crossbeam_channel::{select, Receiver};
use parking_lot::Mutex;

use crate::{
    media::{
        Backend, CameraH264, CaptureSource, EncodedFrame, EncodedSink, EncoderConfig, FrameHub,
        FrameRate, PendingSize, PreviewRenderer, SharedEncodedFrame, VideoEncoder,
    },
    CameraError, VideoDimensions,
};
use egl::EglPreview;
use gpu::{DecodeApi, GpuInfo, GpuPreference, Workload};
use v4l2::{CaptureMode, CaptureStream, PixelFormat};
use vaapi::{DecodedFrame, H264Decoder};

pub(crate) struct LinuxBackend;

// GPUs found when the controller first needs one; each stage picks its own with gpu::plan
#[derive(Clone)]
pub(crate) struct LinuxGpu {
    pub gpus: Arc<Vec<GpuInfo>>,
    pub preference: GpuPreference,
}

impl LinuxGpu {
    // Render node that decodes H.264 for the preview. Only VA-API on the GPU that drives
    // the display: the frame is then drawn where it was decoded, without copies
    fn preview_decoder(&self) -> Option<PathBuf> {
        let plan = gpu::plan(&self.gpus, Workload::H264Passthrough, &self.preference).ok()?;
        match plan.decode? {
            (index, DecodeApi::Vaapi) if index == plan.display_gpu => {
                Some(self.gpus[index].render_node.clone())
            }
            _ => None,
        }
    }
}

pub(crate) type LinuxFrame = DecodedFrame;

pub(crate) struct LinuxCapture {
    // Updated with what the driver accepted once streaming starts
    mode: Mutex<CaptureMode>,
    stream: Mutex<Option<CaptureStream>>,
    h264: Arc<FrameHub<SharedEncodedFrame>>,
}

// Only touched by its render thread
pub(crate) struct LinuxRenderer {
    preview: RefCell<EglPreview>,
}

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

    // The window comes from AWT through JAWT, which is X11 only (also under XWayland)
    fn supports_preview(gpu: &LinuxGpu) -> bool {
        std::env::var_os("DISPLAY").is_some() && gpu.preview_decoder().is_some()
    }
}

impl CaptureSource<LinuxBackend> for LinuxCapture {
    async fn open() -> Result<Self, CameraError> {
        let devices = v4l2::list_devices();
        let device = devices
            .first()
            .ok_or_else(|| common("No camera found".to_string()))?;
        // Only H.264 for now: MJPEG and raw cameras need the transcoder
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
        gpu: LinuxGpu,
        hub: Arc<FrameHub<LinuxFrame>>,
    ) -> Result<(), CameraError> {
        let mut stream = self.stream.lock();
        if stream.is_some() {
            return Ok(());
        }

        // Without a decoder the stream still works, previews just stay empty
        let mut decoder = gpu.preview_decoder().and_then(|node| {
            H264Decoder::new(&node)
                .map_err(|e| println!("Camera preview unavailable: {e}"))
                .ok()
        });
        let mut decoding = false;

        let h264 = self.h264.clone();
        let mode = self.mode.lock().clone();
        let started = CaptureStream::start(&mode, move |frame| {
            // The buffer goes back to the camera after this call, so it is copied: it is
            // compressed, tens of KB per frame
            h264.publish(Arc::new(EncodedFrame {
                data: frame.data.to_vec(),
                timestamp_hns: frame.timestamp_hns,
            }));

            let Some(decoder) = decoder.as_mut() else {
                return;
            };
            // Decoded only while some preview shows it; once it skipped frames the
            // decoder must restart at the next IDR, the references are gone
            if !hub.has_subscribers() {
                if decoding {
                    decoder.reset();
                    decoding = false;
                }
                return;
            }
            decoding = true;
            match decoder.decode(frame.data, frame.timestamp_hns) {
                Ok(pictures) => pictures
                    .into_iter()
                    .for_each(|picture| hub.publish(picture)),
                Err(e) => println!("H.264 decode error, waiting for the next IDR: {e}"),
            }
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
        gpu: LinuxGpu,
        window_handle: u64,
        _width: u32,
        _height: u32,
    ) -> Result<Self, CameraError> {
        if !LinuxBackend::supports_preview(&gpu) {
            return Err(common(
                "Camera preview needs an X11 display and a GPU that decodes H.264 with VA-API"
                    .to_string(),
            ));
        }
        let preview = EglPreview::for_x11_window(window_handle).map_err(common)?;
        Ok(LinuxRenderer {
            preview: RefCell::new(preview),
        })
    }

    fn run(
        &self,
        frames: Receiver<LinuxFrame>,
        shutdown: Receiver<()>,
        pending_size: PendingSize,
    ) -> Result<(), CameraError> {
        let mut preview = self.preview.borrow_mut();
        preview.make_current().map_err(common)?;

        loop {
            let frame = select! {
                recv(frames) -> msg => match msg {
                    Ok(frame) => frame,
                    Err(_) => break,
                },
                recv(shutdown) -> _ => break,
            };
            // The EGL surface follows the window size by itself
            pending_size.lock().take();
            preview.draw(&frame).map_err(common)?;
            // Surface back to the decoder's pool as soon as it is on screen
            drop(frame);
        }

        Ok(())
    }

    fn stop(&self) {}
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
