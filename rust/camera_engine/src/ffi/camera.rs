use std::{collections::HashMap, sync::Arc, thread::JoinHandle};

use crossbeam_channel::{bounded, Sender};
use parking_lot::Mutex;

use crate::{
    ffi::rtsp_transmitter::Streaming,
    media::{
        Backend, CaptureSource, FrameHub, FrameRate, NativeBackend, NativeCapture, NativeFrame,
        NativeGpu, NativeRenderer, PendingSize, PreviewRenderer,
    },
    CameraError, VideoDimensions,
};

// One camera capture shared by every consumer: preview windows (by native window handle)
// and RTSP streaming.
// Capture starts on start_capture()/start_streaming() and stops on release_if_idle() after
// the last consumer is gone
#[derive(uniffi::Object)]
pub struct CameraController {
    gpu: Mutex<Option<NativeGpu>>,
    pub(crate) hub: Arc<FrameHub<NativeFrame>>,
    // Async lock: start/stop await native operations, and it serializes consumers being
    // added against the capture being released
    pub(crate) capture: tokio::sync::Mutex<Option<NativeCapture>>,
    previews: Mutex<HashMap<u64, Preview>>,
    pub(crate) streaming: Mutex<Option<Streaming>>,
}

struct Preview {
    subscription: u64,
    pending_size: PendingSize,
    // Dropping it wakes the render loop and makes it exit
    shutdown: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Preview {
    fn drop(&mut self) {
        drop(self.shutdown.take());
        // Loop exits right away, at most after finishing the current frame
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[uniffi::export]
impl CameraController {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            gpu: Mutex::new(None),
            hub: Arc::new(FrameHub::default()),
            capture: tokio::sync::Mutex::new(None),
            previews: Mutex::new(HashMap::new()),
            streaming: Mutex::new(None),
        })
    }

    /// Starts rendering camera frames into the window (HWND on Windows, X11 Window on Linux).
    /// Synchronous on purpose: it must run while the window is alive, ordered with
    /// detach_preview. Call start_capture() after it
    pub fn attach_preview(
        &self,
        window_handle: u64,
        width: u32,
        height: u32,
    ) -> Result<(), CameraError> {
        if self.previews.lock().contains_key(&window_handle) {
            return Ok(());
        }

        // Initialize the native renderer, it waits for frames until the capture is running
        let renderer = NativeRenderer::new(self.gpu()?, window_handle, width, height)?;
        // Capacity 1: the preview always shows the most recent frame
        let (subscription, frames) = self.hub.subscribe(1);
        let pending_size: PendingSize = Arc::new(Mutex::new(None));
        let (shutdown, shutdown_receiver) = bounded::<()>(1);

        let thread_pending_size = pending_size.clone();
        let thread = std::thread::spawn(move || {
            if let Err(e) = renderer.run(frames, shutdown_receiver, thread_pending_size) {
                println!("Preview stopped: {e}");
            }
            renderer.stop();
        });

        self.previews.lock().insert(
            window_handle,
            Preview {
                subscription,
                pending_size,
                shutdown: Some(shutdown),
                thread: Some(thread),
            },
        );

        Ok(())
    }

    /// Stops rendering into the window. Must be called before the window is destroyed
    pub fn detach_preview(&self, window_handle: u64) {
        let preview = self.previews.lock().remove(&window_handle);
        if let Some(preview) = preview {
            self.hub.unsubscribe(preview.subscription);
            // Joins the render thread
            drop(preview);
        }
    }

    /// Requests a new output size; applied by the render thread before the next frame
    pub fn resize(&self, window_handle: u64, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        if let Some(preview) = self.previews.lock().get(&window_handle) {
            *preview.pending_size.lock() = Some((width, height));
        }
    }

    /// Starts the camera if some preview is attached. Together with release_if_idle it
    /// converges to the right state whatever order the calls arrive in
    pub async fn start_capture(&self) -> Result<(), CameraError> {
        let mut capture = self.capture.lock().await;
        if !self.is_idle() {
            self.ensure_capture(&mut capture).await?;
        }

        Ok(())
    }

    /// Stops the camera when no preview nor streaming is using it anymore
    pub async fn release_if_idle(&self) -> Result<(), CameraError> {
        let mut capture = self.capture.lock().await;

        if self.is_idle() {
            if let Some(active) = capture.take() {
                active.stop().await?;
            }
        }

        Ok(())
    }

    /// Stops every consumer and the camera
    pub async fn stop(&self) -> Result<(), CameraError> {
        let previews: Vec<u64> = self.previews.lock().keys().copied().collect();
        for window_handle in previews {
            self.detach_preview(window_handle);
        }
        self.stop_streaming();
        self.release_if_idle().await
    }
}

impl CameraController {
    fn is_idle(&self) -> bool {
        self.previews.lock().is_empty() && self.streaming.lock().is_none()
    }

    pub(crate) fn gpu(&self) -> Result<NativeGpu, CameraError> {
        let mut gpu = self.gpu.lock();
        if gpu.is_none() {
            *gpu = Some(NativeBackend::create_gpu()?);
        }
        Ok(gpu.as_ref().expect("device created above").clone())
    }

    // Caller holds the capture lock
    pub(crate) async fn ensure_capture(
        &self,
        capture: &mut Option<NativeCapture>,
    ) -> Result<(VideoDimensions, FrameRate), CameraError> {
        if capture.is_none() {
            let new_capture = NativeCapture::open().await?;
            new_capture.start(self.gpu()?, self.hub.clone()).await?;
            *capture = Some(new_capture);
        }

        let active = capture.as_ref().expect("capture started above");
        Ok((active.dimensions(), active.frame_rate()))
    }
}
