use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::Arc;
use windows::Media::Capture::Frames::MediaFrameSource;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, HWND};
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11DeviceContext1,
    ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
    D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
    D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
    D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIFactory2, IDXGIResource1, IDXGISwapChain1, DXGI_PRESENT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};

use windows::core::{AgileReference, Interface, PCWSTR};
use windows::{
    Media::Capture::{
        Frames::{MediaFrameArrivedEventArgs, MediaFrameReader, MediaFrameSourceKind},
        MediaCapture,
    },
    Win32::Graphics::Direct3D11::{ID3D11Texture2D, D3D11_TEXTURE2D_DESC},
    Win32::Graphics::Dxgi::{DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE},
    Win32::System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess,
};

// Configura o scaffolding do UniFFI
uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CameraError {
    #[error("Init Error: {msg}")]
    InitError { msg: String },

    #[error("Stream Error: {msg}")]
    StreamError { msg: String },
}

// Estrutura enviada para o Kotlin via FFI
#[derive(uniffi::Record)]
pub struct FrameGpuShared {
    pub share_handle: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(uniffi::Record)]
pub struct VideoDimensions {
    pub width: u32,
    pub height: u32,
}

// Internal state of native capture
struct InnerCameraState {
    media_capture: AgileReference<MediaCapture>,
    media_source: AgileReference<MediaFrameSource>,
    frame_reader: AgileReference<MediaFrameReader>,
}

pub struct InnerNativeRenderer {
    device1: ID3D11Device1,
    context1: ID3D11DeviceContext1,
    swap_chain: IDXGISwapChain1,
}

// Main controller used by kotlin
#[derive(uniffi::Object)]
pub struct CameraController {
    renderer: Mutex<Option<InnerNativeRenderer>>,
    state: Mutex<Option<InnerCameraState>>,
    sender: Sender<u64>,
    receiver: Receiver<u64>,
}

#[uniffi::export]
impl CameraController {
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = bounded::<u64>(3);
        Arc::new(Self {
            renderer: Mutex::new(None),
            state: Mutex::new(None),
            sender,
            receiver,
        })
    }

    fn get_dimensions_from_media_capture(&self) -> Result<VideoDimensions, CameraError> {
        let state_guard = self
            .state
            .try_lock()
            .ok_or_else(|| CameraError::InitError {
                msg: "Capture not initialized".to_string(),
            })?;

        let camera_state = state_guard.as_ref().ok_or_else(|| CameraError::InitError {
            msg: "Capture not initialized".to_string(),
        })?;

        let media_source = camera_state
            .media_source
            .resolve()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        let current_format = media_source
            .CurrentFormat()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        // 2. Acessa as propriedades específicas de vídeo (VideoFormat)
        let video_format = current_format
            .VideoFormat()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        let width = video_format
            .Width()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
        let height = video_format
            .Height()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        Ok(VideoDimensions { width, height })
    }

    pub fn create_renderer(
        &self,
        hwnd_raw: u64,
        width: u32,
        height: u32,
    ) -> Result<(), CameraError> {
        {
            if self.renderer.lock().is_some() {
                return Ok(()); // Already running this instance
            }
        }

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
        }
        .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        let device = device.ok_or_else(|| CameraError::InitError {
            msg: "Failed to get device".to_string(),
        })?;
        let context = context.ok_or_else(|| CameraError::InitError {
            msg: "Failed to get device context".to_string(),
        })?;

        // 2. Navigate COM graph to get modern Factory (IDXGIFactory2)
        let dxgi_device: IDXGIDevice = device
            .cast()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
        let adapter: IDXGIAdapter = unsafe { dxgi_device.GetAdapter() }
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
        let factory: IDXGIFactory2 = unsafe { adapter.GetParent() }
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

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
        }
        .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        let device1: ID3D11Device1 = device
            .cast()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        let context1: ID3D11DeviceContext1 = context
            .cast()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        {
            let mut renderer_guard = self.renderer.lock();
            *renderer_guard = Some(InnerNativeRenderer {
                device1,
                context1,
                swap_chain: swapchain1,
            });
        }

        Ok(())
    }

    pub async fn render_all_pending_frames(&self) -> Result<(), CameraError> {
        let input_dim = self
            .get_dimensions_from_media_capture()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        let mut renderer_guard =
            self.renderer
                .try_lock()
                .ok_or_else(|| CameraError::InitError {
                    msg: "Nenhuma câmera encontrada".to_string(),
                })?;

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

        if let Some(renderer) = renderer_guard.as_mut() {
            let swap_chain_desc1 = unsafe { renderer.swap_chain.GetDesc1() }
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

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

            let video_device: ID3D11VideoDevice = renderer
                .device1
                .cast()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            let video_context: ID3D11VideoContext = renderer
                .context1
                .cast()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            let video_enum: ID3D11VideoProcessorEnumerator =
                unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }
                    .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            // Create Processor from enumerator
            let video_processor: ID3D11VideoProcessor =
                unsafe { video_device.CreateVideoProcessor(&video_enum, 0) }
                    .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            // 3. Empties handle's queue (u64) pending to be sent by camera
            while let Ok(handle_u64) = self.receiver.recv() {
                unsafe {
                    let handle = windows::Win32::Foundation::HANDLE(handle_u64 as *mut _);

                    // Access private member from InnerNativeRenderer
                    let frame_texture: ID3D11Texture2D =
                        match renderer.device1.OpenSharedResource1(handle) {
                            Ok(tex) => tex,
                            Err(e) => {
                                let _ = windows::Win32::Foundation::CloseHandle(handle);
                                return Err(CameraError::InitError { msg: e.to_string() });
                            }
                        };

                    // Free native NT Handle immediately after Direct3D's opening
                    let _ = windows::Win32::Foundation::CloseHandle(handle);

                    let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
                    video_device
                        .CreateVideoProcessorInputView(
                            &frame_texture,
                            &video_enum,
                            &input_desc,
                            Some(&mut input_view),
                        )
                        .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

                    // Get SwapChain's Back Buffer
                    let back_buffer_texture: ID3D11Texture2D = renderer
                        .swap_chain
                        .GetBuffer(0)
                        .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

                    let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;

                    video_device
                        .CreateVideoProcessorOutputView(
                            &back_buffer_texture, // 1. Recurso de saída (BackBuffer da SwapChain)
                            &video_enum,
                            &output_desc, // 2. Ponteiro para D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC
                            Some(&mut output_view), // 3. Ponteiro de saída
                        )
                        .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

                    let input_view: ManuallyDrop<Option<ID3D11VideoProcessorInputView>> =
                        ManuallyDrop::new(input_view);

                    // 6. Prepare Stream's args for a GPU
                    let stream_data = D3D11_VIDEO_PROCESSOR_STREAM {
                        Enable: true.into(),
                        pInputSurface: input_view, // <--- A View da sua textura entra na GPU aqui!
                        ..Default::default()
                    };

                    let output_view = output_view.ok_or_else(|| CameraError::InitError {
                        msg: "Failed to load output view".to_string(),
                    })?;

                    // 7. GPU runs a NV12 -> BGRA8 conversion by Hardware
                    video_context
                        .VideoProcessorBlt(&video_processor, &output_view, 0, &[stream_data])
                        .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

                    // Show frame at Swing's HWND
                    renderer
                        .swap_chain
                        .Present(1, DXGI_PRESENT(0))
                        .ok()
                        .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
                }
            }
        }

        Ok(())
    }

    pub async fn start_capture(&self) -> Result<(), CameraError> {
        // 1. Initial validation
        {
            if self.state.lock().is_some() {
                return Ok(()); // Already running this instance
            }
        }

        // 2. Initialize MediaCapture
        let media_capture_agile = {
            let media_capture =
                MediaCapture::new().map_err(|e| CameraError::InitError { msg: e.to_string() })?;
            AgileReference::new(&media_capture)
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
        };

        let async_op = media_capture_agile
            .resolve()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?
            .InitializeAsync()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        async_op
            .await
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        // 3. Search video source
        let frame_sources = media_capture_agile
            .resolve()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?
            .FrameSources()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
        let mut selected_source = None;

        for pair in frame_sources {
            let source = pair
                .Value()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
            let kind = source
                .Info()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
                .SourceKind()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            if kind == MediaFrameSourceKind::Color {
                struct SendSource(windows::Media::Capture::Frames::MediaFrameSource);
                unsafe impl Send for SendSource {}
                selected_source = Some(SendSource(source));
                break;
            }
        }

        let source = selected_source.ok_or_else(|| CameraError::InitError {
            msg: "No camera found".to_string(),
        })?;

        let media_source_agile = AgileReference::new(&source.0)
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        let frame_reader_agile = {
            let agile_op = media_capture_agile
                .resolve()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
                .CreateFrameReaderAsync(&source.0)
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            let frame_reader = agile_op
                .await
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            AgileReference::new(&frame_reader)
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
        };

        let handler_sender = self.sender.clone();

        {
            let event_handler = windows::Foundation::TypedEventHandler::<
                MediaFrameReader,
                MediaFrameArrivedEventArgs,
            >::new(move |sender, _args| {
                let reader = sender.as_ref().ok_or(windows::core::Error::empty())?;
                let frame = reader.TryAcquireLatestFrame()?;
                let video_frame = frame.VideoMediaFrame()?;
                let d3d_surface = video_frame.Direct3DSurface()?;
                let dxgi_access = d3d_surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
                let handle = unsafe {
                    let texture = dxgi_access.GetInterface::<ID3D11Texture2D>()?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    texture.GetDesc(&mut desc);

                    let dxgi_resource = texture.cast::<IDXGIResource1>()?;
                    dxgi_resource.CreateSharedHandle(
                        None,
                        DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                        PCWSTR::null(),
                    )?
                };

                if let Err(crossbeam_channel::TrySendError::Full(dropped_handle)) =
                    handler_sender.try_send(handle.0 as u64)
                {
                    // If queue is full, close handle immediately without blocking thread
                    unsafe {
                        let hwnd_ptr = dropped_handle as *mut c_void;
                        let handle = HANDLE(hwnd_ptr);
                        let _ = CloseHandle(handle);
                    }
                }

                Ok(())
            });

            frame_reader_agile
                .resolve()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
                .FrameArrived(&event_handler)
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
        }

        let async_op = frame_reader_agile
            .resolve()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?
            .StartAsync()
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        // 6. Start streaming
        async_op
            .await
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        // 7. Holds native state within a instance's synchronous Mutex
        {
            let mut state_guard = self.state.lock();
            *state_guard = Some(InnerCameraState {
                media_capture: media_capture_agile,
                media_source: media_source_agile,
                frame_reader: frame_reader_agile,
            });
        }

        Ok(())
    }

    pub async fn stop_capture(&self) -> Result<(), CameraError> {
        {
            if self.state.lock().is_none() {
                return Ok(()); // Already stopped
            }
        }

        let async_op = {
            let state_guard = self.state.lock();
            let camera_state = state_guard.as_ref().ok_or_else(|| CameraError::InitError {
                msg: "Failure to acquire lock".to_string(),
            })?;

            camera_state
                .frame_reader
                .resolve()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
                .StopAsync()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
        };

        async_op
            .await
            .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

        {
            let state_guard = self.state.lock();
            let camera_state = state_guard.as_ref().ok_or_else(|| CameraError::InitError {
                msg: "Failure to acquire lock".to_string(),
            })?;

            camera_state
                .frame_reader
                .resolve()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
                .Close()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;

            camera_state
                .media_capture
                .resolve()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?
                .Close()
                .map_err(|e| CameraError::InitError { msg: e.to_string() })?;
        };

        {
            let mut state_guard = self.state.lock();
            *state_guard = None;
        }

        Ok(())
    }
}
