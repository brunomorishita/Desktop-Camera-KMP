// Linux backend. Capture runs on V4L2 and takes cameras that send H.264 or MJPEG. H.264 goes
// out as is; the preview decodes either format with VA-API and draws it with EGL on the X11
// window. Not implemented yet: the H.264 encoder (streaming MJPEG cameras), raw cameras
// and NVDEC
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
use gpu::{Codec, DecodeApi, GpuInfo, GpuPreference};
use v4l2::{CaptureMode, CaptureStream, CapturedFrame, PixelFormat};
use vaapi::{DecodedFrame, H264Decoder, JpegDecoder};

pub(crate) struct LinuxBackend;

// GPUs found when the controller first needs one; each stage picks its own with gpu::plan
#[derive(Clone)]
pub(crate) struct LinuxGpu {
    pub gpus: Arc<Vec<GpuInfo>>,
    pub preference: GpuPreference,
}

impl LinuxGpu {
    // Render node that decodes `codec` for the preview. Only VA-API on the GPU that drives
    // the display: the frame is then drawn where it was decoded, without copies
    fn preview_decoder(&self, codec: Codec) -> Option<PathBuf> {
        match gpu::preview_decoder(&self.gpus, codec, &self.preference)? {
            (index, DecodeApi::Vaapi) => Some(self.gpus[index].render_node.clone()),
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
        std::env::var_os("DISPLAY").is_some()
            && [Codec::H264, Codec::Jpeg]
                .into_iter()
                .any(|codec| gpu.preview_decoder(codec).is_some())
    }
}

impl CaptureSource<LinuxBackend> for LinuxCapture {
    async fn open(gpu: LinuxGpu) -> Result<Self, CameraError> {
        let devices = v4l2::list_devices();
        let device = devices
            .first()
            .ok_or_else(|| common("No camera found".to_string()))?;
        // H.264 streams even when nothing decodes it; MJPEG is of use only once decoded.
        // Raw cameras need the encoder
        let decodes_jpeg = gpu.preview_decoder(Codec::Jpeg).is_some();
        let mode = v4l2::choose_mode(device, |format| match format {
            PixelFormat::H264 => true,
            PixelFormat::Mjpeg => decodes_jpeg,
            PixelFormat::Nv12 | PixelFormat::Yuyv => false,
        })
        .ok_or_else(|| {
            let sends_mjpeg = device
                .modes
                .iter()
                .any(|mode| mode.format == PixelFormat::Mjpeg);
            common(if sends_mjpeg {
                format!(
                    "{} sends MJPEG, and the GPU showing the screen cannot decode it with VA-API",
                    device.name
                )
            } else {
                format!(
                    "{} sends neither H.264 nor MJPEG. Raw cameras are not supported on Linux yet",
                    device.name
                )
            })
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

        let mode = self.mode.lock().clone();
        let on_frame = match mode.format {
            PixelFormat::H264 => h264_handler(&gpu, self.h264.clone(), hub),
            PixelFormat::Mjpeg => mjpeg_handler(&gpu, hub)?,
            other => return Err(common(format!("{other:?} cameras are not supported yet"))),
        };
        let started = CaptureStream::start(&mode, on_frame)?;

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
        let stream = self.stream.lock();
        let stream = stream.as_ref()?;
        if stream.mode().format != PixelFormat::H264 {
            return None;
        }
        let requester = stream.keyframe_requester();
        Some(CameraH264 {
            frames: self.h264.clone(),
            request_keyframe: Arc::new(move || requester.request()),
        })
    }
}

type FrameHandler = Box<dyn FnMut(CapturedFrame<'_>) + Send>;

// H.264 camera: every access unit goes to the stream as is, and is decoded only while some
// preview shows it
fn h264_handler(
    gpu: &LinuxGpu,
    h264: Arc<FrameHub<SharedEncodedFrame>>,
    hub: Arc<FrameHub<LinuxFrame>>,
) -> FrameHandler {
    // Without a decoder the stream still works, previews just stay empty
    let mut decoder = gpu.preview_decoder(Codec::H264).and_then(|node| {
        H264Decoder::new(&node)
            .map_err(|e| println!("Camera preview unavailable: {e}"))
            .ok()
    });
    let mut decoding = false;

    Box::new(move |frame| {
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
    })
}

// MJPEG camera: each picture stands alone, decoded only while someone (preview, later the
// encoder) takes the frames
fn mjpeg_handler(
    gpu: &LinuxGpu,
    hub: Arc<FrameHub<LinuxFrame>>,
) -> Result<FrameHandler, CameraError> {
    let node = gpu
        .preview_decoder(Codec::Jpeg)
        .ok_or_else(|| common("No GPU decodes MJPEG with VA-API".to_string()))?;
    let mut decoder = JpegDecoder::new(&node).map_err(common)?;
    // A broken picture now and then is normal (USB hiccups); a decoder that refuses every
    // picture would flood the log, so only the first error of a run is printed
    let mut failing = false;

    Ok(Box::new(move |frame| {
        if !hub.has_subscribers() {
            return;
        }
        match decoder.decode(frame.data, frame.timestamp_hns) {
            Ok(picture) => {
                failing = false;
                hub.publish(picture);
            }
            Err(e) if !failing => {
                failing = true;
                println!("MJPEG decode error: {e}");
            }
            Err(_) => {}
        }
    }))
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
                "Camera preview needs an X11 display and a GPU that decodes H.264 or JPEG \
                 with VA-API"
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // What a preview window gets: decoded frames of the camera's size, from either format
    #[test]
    #[ignore = "opens the real camera, run with: cargo test -- --ignored --nocapture"]
    fn previews_the_camera() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let gpu = LinuxBackend::create_gpu().unwrap();
        let capture = runtime.block_on(LinuxCapture::open(gpu.clone())).unwrap();
        let hub = Arc::new(FrameHub::default());
        runtime.block_on(capture.start(gpu, hub.clone())).unwrap();
        let mode = capture.mode.lock().clone();
        println!(
            "{} {:?} {}x{}",
            mode.node.display(),
            mode.format,
            mode.width,
            mode.height
        );
        // Only an H.264 camera feeds the stream without the encoder
        assert_eq!(
            capture.camera_h264().is_some(),
            mode.format == PixelFormat::H264
        );

        let (subscription, frames) = hub.subscribe(2);
        for _ in 0..30 {
            // Dropped right away like the preview does, the surfaces go back to the decoder
            let frame = frames
                .recv_timeout(Duration::from_secs(3))
                .expect("no decoded frame");
            frame.sync().unwrap();
            assert_eq!(frame.visible, (0, 0, mode.width, mode.height));
        }
        hub.unsubscribe(subscription);
        runtime.block_on(capture.stop()).unwrap();
    }
}
