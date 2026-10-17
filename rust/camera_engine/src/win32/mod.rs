pub(crate) mod capture;
pub(crate) mod device;
pub(crate) mod encoder;
pub(crate) mod frame;
pub(crate) mod renderer;

use crate::{media::Backend, CameraError};

// WinRT MediaCapture -> D3D11 NV12 frames -> D3D11 video processor / Media Foundation encoder
pub(crate) struct Win32Backend;

impl Backend for Win32Backend {
    type Gpu = device::GpuDevice;
    type Frame = frame::SharedFrame;
    type Capture = capture::InnerNativeCapture;
    type Renderer = renderer::InnerNativeRenderer;
    type Encoder = encoder::H264Encoder;

    const SUPPORTS_PREVIEW: bool = true;

    fn create_gpu() -> Result<device::GpuDevice, CameraError> {
        device::GpuDevice::new()
    }
}
