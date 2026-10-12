use windows::{
    core::Interface,
    Win32::{
        Foundation::HMODULE,
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext,
                ID3D11DeviceContext1, ID3D11Multithread, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
            },
            Dxgi::{IDXGIAdapter, IDXGIDevice},
        },
    },
};

use crate::CameraError;

// Single D3D11 device shared by capture copy, preview renderers and H.264 encoder,
// so frames never leave the GPU nor cross devices after the capture copy
#[derive(Clone)]
pub struct GpuDevice {
    pub device: ID3D11Device1,
    pub context: ID3D11DeviceContext1,
}

impl GpuDevice {
    pub fn new() -> Result<Self, CameraError> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        // VIDEO_SUPPORT is required to hand the device to Media Foundation encoders
        #[allow(unused_mut)]
        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;

        // Enabling Debug Layer for development build (debug)
        #[cfg(debug_assertions)]
        {
            use windows::Win32::Graphics::Direct3D11::D3D11_CREATE_DEVICE_DEBUG;

            flags |= D3D11_CREATE_DEVICE_DEBUG;
        }

        unsafe {
            D3D11CreateDevice(
                None,                     // Default adapter (primary GPU)
                D3D_DRIVER_TYPE_HARDWARE, // Hardware Aceleration
                HMODULE::default(),
                flags,
                Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]), // Resource levels
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }?;

        let device = device.ok_or_else(|| CameraError::CommonError {
            msg: "Failed to get device".to_string(),
        })?;
        let context = context.ok_or_else(|| CameraError::CommonError {
            msg: "Failed to get device context".to_string(),
        })?;

        // Immediate context is used from capture, render and encoder threads
        let multithread: ID3D11Multithread = context.cast()?;
        let _ = unsafe { multithread.SetMultithreadProtected(true) };

        Ok(GpuDevice {
            device: device.cast()?,
            context: context.cast()?,
        })
    }

    pub fn adapter(&self) -> Result<IDXGIAdapter, CameraError> {
        let dxgi_device: IDXGIDevice = self.device.cast()?;
        Ok(unsafe { dxgi_device.GetAdapter() }?)
    }
}
