use std::sync::Arc;

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;

use crate::{
    win32::{InnerNativeCapture, InnerNativeRenderer},
    CameraError,
};

#[derive(uniffi::Object)]
pub struct CameraController {
    renderer: Mutex<Option<InnerNativeRenderer>>,
    capture: Mutex<Option<InnerNativeCapture>>,
    sender: Sender<u64>,
    receiver: Receiver<u64>,
}

#[uniffi::export]
impl CameraController {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        // Cria o canal interno de comunicação
        let (sender, receiver) = bounded::<u64>(3);

        Arc::new(Self {
            renderer: Mutex::new(None),
            capture: Mutex::new(None),
            sender,
            receiver,
        })
    }

    pub async fn start(&self, hwnd_raw: u64, width: u32, height: u32) -> Result<(), CameraError> {
        // Inicialize capture
        let capture = InnerNativeCapture::new().await?;
        let input_dim = capture.get_dimensions_from_media_capture()?;

        // Initialize Renderer D3D
        let renderer = InnerNativeRenderer::new(hwnd_raw, width, height)?;
        capture.start(self.sender.clone()).await?;
        renderer.start(input_dim, self.receiver.clone()).await?;

        *self.capture.lock() = Some(capture);
        *self.renderer.lock() = Some(renderer);

        Ok(())
    }

    pub async fn stop(&self) -> Result<(), CameraError> {
        let capture_opt = self.capture.lock().take();
        if let Some(capture) = capture_opt {
            capture.stop().await?;
        }
        *self.capture.lock() = None;

        if let Some(renderer) = self.renderer.lock().take() {
            renderer.stop();
        }
        *self.renderer.lock() = None;

        Ok(())
    }
}
