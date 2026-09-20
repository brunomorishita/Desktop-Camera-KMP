use std::{ffi::c_void, mem::ManuallyDrop};

use crossbeam_channel::Receiver;
use windows::{
    core::Interface,
    Win32::{
        Foundation::{HMODULE, HWND},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1},
            Direct3D11::{
                D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext,
                ID3D11DeviceContext1, ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice,
                ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV,
                D3D11_TEX2D_VPOV, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
                D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL,
                    DXGI_SAMPLE_DESC,
                },
                IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT,
                DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
                DXGI_USAGE_RENDER_TARGET_OUTPUT,
            },
        },
    },
};

use crate::{CameraError, VideoDimensions};

pub struct InnerNativeRenderer {
    device1: ID3D11Device1,
    context1: ID3D11DeviceContext1,
    swap_chain: IDXGISwapChain1,
}

impl InnerNativeRenderer {
    pub fn new(hwnd_raw: u64, width: u32, height: u32) -> Result<Self, CameraError> {
        let hwnd_ptr = hwnd_raw as *mut c_void;
        let hwnd = HWND(hwnd_ptr);

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        let mut flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT;

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
                flags, // Essential for GDI / Direct2D interoperability
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

        // 2. Navigate COM graph to get modern Factory (IDXGIFactory2)
        let dxgi_device: IDXGIDevice = device.cast()?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }?;
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }?;

        // 3. Configure modern SwapChain (DESC1)
        let swapchain_desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM, // Formato compatível com AWT/Windows
            Stereo: false.into(),
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD, // Alta performance do Win10/11
            AlphaMode: DXGI_ALPHA_MODE_IGNORE,
            Flags: 0,
        };

        // 4. Create IDXGISwapChain1
        let swapchain1: IDXGISwapChain1 = unsafe {
            factory.CreateSwapChainForHwnd(
                &device,
                hwnd,
                &swapchain_desc,
                None, // Opcional: Fullscreen desc
                None, // Opcional: Restrict to one output
            )
        }?;

        let device1: ID3D11Device1 = device.cast()?;
        let context1: ID3D11DeviceContext1 = context.cast()?;

        Ok(InnerNativeRenderer {
            device1,
            context1,
            swap_chain: swapchain1,
        })
    }

    pub fn start(
        &self,
        input_dim: VideoDimensions,
        receiver: Receiver<u64>,
    ) -> Result<(), CameraError> {
        let input_desc: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC =
            D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0, // 0 tell DirectX to use the same DXGI_FORMAT of texture_desc (ex: NV12)
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };

        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };

        let swap_chain_desc1 = unsafe { self.swap_chain.GetDesc1() }?;

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: input_dim.width,
            InputHeight: input_dim.height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: swap_chain_desc1.Width,
            OutputHeight: swap_chain_desc1.Height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let video_device: ID3D11VideoDevice = self.device1.cast()?;
        let video_context: ID3D11VideoContext = self.context1.cast()?;

        let video_enum: ID3D11VideoProcessorEnumerator =
            unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }?;

        // Create Processor from enumerator
        let video_processor: ID3D11VideoProcessor =
            unsafe { video_device.CreateVideoProcessor(&video_enum, 0) }?;

        // 3. Empties handle's queue (u64) pending to be sent by camera
        while let Ok(handle_u64) = receiver.recv() {
            unsafe {
                let handle = windows::Win32::Foundation::HANDLE(handle_u64 as *mut _);

                // Access private member from InnerNativeRenderer
                let frame_texture: ID3D11Texture2D =
                    self.device1.OpenSharedResource1(handle).inspect_err(|_| {
                        let _ = windows::Win32::Foundation::CloseHandle(handle);
                    })?;

                // Free native NT Handle immediately after Direct3D's opening
                let _ = windows::Win32::Foundation::CloseHandle(handle);

                let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
                video_device.CreateVideoProcessorInputView(
                    &frame_texture,
                    &video_enum,
                    &input_desc,
                    Some(&mut input_view),
                )?;

                // Get SwapChain's Back Buffer
                let back_buffer_texture: ID3D11Texture2D = self.swap_chain.GetBuffer(0)?;

                let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;

                video_device.CreateVideoProcessorOutputView(
                    &back_buffer_texture, // 1. Recurso de saída (BackBuffer da SwapChain)
                    &video_enum,
                    &output_desc, // 2. Ponteiro para D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC
                    Some(&mut output_view), // 3. Ponteiro de saída
                )?;

                let input_view: ManuallyDrop<Option<ID3D11VideoProcessorInputView>> =
                    ManuallyDrop::new(input_view);

                // 6. Prepare Stream's args for a GPU
                let stream_data = D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: true.into(),
                    pInputSurface: input_view, // <--- A View da sua textura entra na GPU aqui!
                    ..Default::default()
                };

                let output_view = output_view.ok_or_else(|| CameraError::CommonError {
                    msg: "Failed to load output view".to_string(),
                })?;

                // 7. GPU runs a NV12 -> BGRA8 conversion by Hardware
                video_context.VideoProcessorBlt(
                    &video_processor,
                    &output_view,
                    0,
                    &[stream_data],
                )?;

                // Show frame at Swing's HWND
                self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            }
        }

        Ok(())
    }

    pub fn stop(&self) {
        unsafe {
            self.context1.ClearState();
            self.context1.Flush();
        }
    }
}
