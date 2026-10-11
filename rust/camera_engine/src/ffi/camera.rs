use std::{ffi::c_void, sync::Arc, thread::JoinHandle};

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, HANDLE};

use crate::{
    win32::{renderer::PendingSize, InnerNativeCapture, InnerNativeRenderer},
    CameraError,
};

#[derive(uniffi::Object)]
pub struct CameraController {
    capture: Mutex<Option<InnerNativeCapture>>,
    sender: Sender<u64>,
    receiver: Receiver<u64>,
    pending_size: PendingSize,
    render_thread: Mutex<Option<RenderThread>>,
}

struct RenderThread {
    // Dropping it wakes the render loop and makes it exit
    shutdown: Sender<()>,
    handle: JoinHandle<()>,
}

#[uniffi::export]
impl CameraController {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        // Cria o canal interno de comunicação
        let (sender, receiver) = bounded::<u64>(3);

        Arc::new(Self {
            capture: Mutex::new(None),
            sender,
            receiver,
            pending_size: Arc::new(Mutex::new(None)),
            render_thread: Mutex::new(None),
        })
    }

    pub async fn start(&self, hwnd_raw: u64, width: u32, height: u32) -> Result<(), CameraError> {
        // Inicialize capture
        let capture = InnerNativeCapture::new().await?;
        let input_dim = capture.get_dimensions_from_media_capture()?;

        capture.start(self.sender.clone()).await?;
        *self.capture.lock() = Some(capture);

        // Initialize Renderer D3D
        let renderer = InnerNativeRenderer::new(hwnd_raw, width, height)?;
        let receiver = self.receiver.clone();
        let pending_size = self.pending_size.clone();
        let (shutdown, shutdown_receiver) = bounded::<()>(1);
        let handle = std::thread::spawn(move || {
            let _ = renderer.start(input_dim, receiver, shutdown_receiver, pending_size);
            renderer.stop();
        });
        *self.render_thread.lock() = Some(RenderThread { shutdown, handle });

        Ok(())
    }

    /// Requests a new output size; applied by the render thread before the next frame
    pub fn resize(&self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        *self.pending_size.lock() = Some((width, height));
    }

    pub async fn stop(&self) -> Result<(), CameraError> {
        let capture_opt = self.capture.lock().take();
        if let Some(capture) = capture_opt {
            capture.stop().await?;
        }

        let render_thread = self.render_thread.lock().take();
        if let Some(RenderThread { shutdown, handle }) = render_thread {
            drop(shutdown);
            // Loop exits right away, at most after finishing the current frame
            let _ = handle.join();
        }

        // Close handles left in the queue so they don't leak or show up on the next start
        while let Ok(handle_u64) = self.receiver.try_recv() {
            unsafe {
                let _ = CloseHandle(HANDLE(handle_u64 as *mut c_void));
            }
        }

        Ok(())
    }
}
