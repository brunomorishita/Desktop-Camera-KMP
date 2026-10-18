use std::{ffi::c_void, mem::ManuallyDrop};

use crossbeam_channel::{select, Receiver};
use windows::{
    core::Interface,
    Win32::{
        Foundation::{HWND, RECT},
        Graphics::{
            Direct3D11::{
                ID3D11Texture2D, ID3D11VideoContext, ID3D11VideoDevice, ID3D11VideoProcessor,
                ID3D11VideoProcessorEnumerator, ID3D11VideoProcessorInputView,
                ID3D11VideoProcessorOutputView, D3D11_TEX2D_VPIV, D3D11_TEX2D_VPOV,
                D3D11_VIDEO_COLOR, D3D11_VIDEO_COLOR_0, D3D11_VIDEO_COLOR_RGBA,
                D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE, D3D11_VIDEO_PROCESSOR_CONTENT_DESC,
                D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC, D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0,
                D3D11_VIDEO_PROCESSOR_STREAM, D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
                D3D11_VPIV_DIMENSION_TEXTURE2D, D3D11_VPOV_DIMENSION_TEXTURE2D,
            },
            Dxgi::{
                Common::{
                    DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_UNKNOWN,
                    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
                },
                IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
                DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
                DXGI_USAGE_RENDER_TARGET_OUTPUT,
            },
        },
    },
};

use crate::{
    media::{PendingSize, PreviewRenderer},
    win32::{device::GpuDevice, frame::SharedFrame, Win32Backend},
    CameraError,
};

// Frame pool has a fixed set of textures, so input views can be cached per texture
const MAX_CACHED_INPUT_VIEWS: usize = 16;

pub struct InnerNativeRenderer {
    gpu: GpuDevice,
    swap_chain: IDXGISwapChain1,
}

// Video processor objects, bound to the input size and the swap chain size
struct Processor {
    input_width: u32,
    input_height: u32,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_view: ID3D11VideoProcessorOutputView,
    input_views: Vec<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>,
}

impl PreviewRenderer<Win32Backend> for InnerNativeRenderer {
    fn new(gpu: GpuDevice, window_handle: u64, width: u32, height: u32) -> Result<Self, CameraError> {
        let hwnd = HWND(window_handle as *mut c_void);

        // Navigate COM graph to get modern Factory (IDXGIFactory2)
        let factory: IDXGIFactory2 = unsafe { gpu.adapter()?.GetParent() }?;

        // Configure modern SwapChain (DESC1)
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

        let swap_chain = unsafe {
            factory.CreateSwapChainForHwnd(
                &gpu.device,
                hwnd,
                &swapchain_desc,
                None, // Opcional: Fullscreen desc
                None, // Opcional: Restrict to one output
            )
        }?;

        Ok(InnerNativeRenderer { gpu, swap_chain })
    }

    fn run(
        &self,
        frames: Receiver<SharedFrame>,
        shutdown: Receiver<()>,
        pending_size: PendingSize,
    ) -> Result<(), CameraError> {
        let video_device: ID3D11VideoDevice = self.gpu.device.cast()?;
        let video_context: ID3D11VideoContext = self.gpu.context.cast()?;
        let mut processor: Option<Processor> = None;

        // Render every frame received until shutdown is signaled (message sent or sender dropped)
        loop {
            let frame = select! {
                recv(frames) -> msg => match msg {
                    Ok(frame) => frame,
                    Err(_) => break,
                },
                recv(shutdown) -> _ => break,
            };

            // Apply pending resize before rendering, releasing the back buffer view first
            if let Some((width, height)) = pending_size.lock().take() {
                processor = None;
                self.resize(width, height)?;
            }

            let current = match processor.take() {
                Some(p) if p.input_width == frame.width() && p.input_height == frame.height() => p,
                _ => self.create_processor(
                    &video_device,
                    &video_context,
                    frame.width(),
                    frame.height(),
                )?,
            };
            let current = processor.insert(current);

            let input_view = current.input_view(&video_device, frame.texture())?;

            unsafe {
                // Prepare Stream's args for a GPU
                let streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                    Enable: true.into(),
                    pInputSurface: ManuallyDrop::new(Some(input_view)),
                    ..Default::default()
                }];

                // GPU runs a NV12 -> BGRA8 conversion and scaling by Hardware
                let blt_result = video_context.VideoProcessorBlt(
                    &current.processor,
                    &current.output_view,
                    0,
                    &streams,
                );

                // Release the reference added to the input view above, even if Blt failed
                let [stream_data] = streams;
                drop(ManuallyDrop::into_inner(stream_data.pInputSurface));
                blt_result?;

                // Show frame at Swing's HWND
                self.swap_chain.Present(1, DXGI_PRESENT(0)).ok()?;
            }

            // Texture can go back to the pool: Blt was already queued on the shared context
            drop(frame);
        }

        Ok(())
    }

    fn stop(&self) {
        unsafe {
            self.gpu.context.Flush();
        }
    }
}

impl InnerNativeRenderer {
    fn create_processor(
        &self,
        video_device: &ID3D11VideoDevice,
        video_context: &ID3D11VideoContext,
        input_width: u32,
        input_height: u32,
    ) -> Result<Processor, CameraError> {
        let swap_chain_desc = unsafe { self.swap_chain.GetDesc1() }?;
        let (output_width, output_height) = (swap_chain_desc.Width, swap_chain_desc.Height);

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: input_width,
            InputHeight: input_height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: output_width,
            OutputHeight: output_height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator: ID3D11VideoProcessorEnumerator =
            unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc) }?;

        // Create Processor from enumerator
        let processor: ID3D11VideoProcessor =
            unsafe { video_device.CreateVideoProcessor(&enumerator, 0) }?;

        // With flip model, buffer 0 always refers to the current back buffer,
        // so a single output view stays valid until the next resize
        let back_buffer: ID3D11Texture2D = unsafe { self.swap_chain.GetBuffer(0) }?;
        let output_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };
        let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &back_buffer,
                &enumerator,
                &output_desc,
                Some(&mut output_view),
            )
        }?;
        let output_view = output_view.ok_or_else(|| CameraError::CommonError {
            msg: "Failed to load output view".to_string(),
        })?;

        // Keep the camera aspect ratio: image centered, black bars on the remaining area
        let target_rect = RECT {
            left: 0,
            top: 0,
            right: output_width as i32,
            bottom: output_height as i32,
        };
        let dest_rect = letterbox(input_width, input_height, output_width, output_height);
        let black = D3D11_VIDEO_COLOR {
            Anonymous: D3D11_VIDEO_COLOR_0 {
                RGBA: D3D11_VIDEO_COLOR_RGBA {
                    R: 0.0,
                    G: 0.0,
                    B: 0.0,
                    A: 1.0,
                },
            },
        };
        unsafe {
            video_context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&target_rect));
            video_context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&dest_rect));
            video_context.VideoProcessorSetOutputBackgroundColor(&processor, false, &black);
        }

        Ok(Processor {
            input_width,
            input_height,
            enumerator,
            processor,
            output_view,
            input_views: Vec::new(),
        })
    }

    fn resize(&self, width: u32, height: u32) -> Result<(), CameraError> {
        unsafe {
            // Destroy the dropped back buffer view now, ResizeBuffers fails while it is alive
            self.gpu.context.Flush();

            // 0 / UNKNOWN keep the current buffer count and format
            self.swap_chain.ResizeBuffers(
                0,
                width,
                height,
                DXGI_FORMAT_UNKNOWN,
                DXGI_SWAP_CHAIN_FLAG(0),
            )?;
        }

        Ok(())
    }
}

impl Processor {
    fn input_view(
        &mut self,
        video_device: &ID3D11VideoDevice,
        texture: &ID3D11Texture2D,
    ) -> Result<ID3D11VideoProcessorInputView, CameraError> {
        if let Some((_, view)) = self.input_views.iter().find(|(t, _)| t == texture) {
            return Ok(view.clone());
        }

        let input_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0, // 0 tell DirectX to use the same DXGI_FORMAT of texture_desc (ex: NV12)
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: 0,
                },
            },
        };

        let mut view: Option<ID3D11VideoProcessorInputView> = None;
        unsafe {
            video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &input_desc,
                Some(&mut view),
            )
        }?;
        let view = view.ok_or_else(|| CameraError::CommonError {
            msg: "Failed to create input view".to_string(),
        })?;

        if self.input_views.len() >= MAX_CACHED_INPUT_VIEWS {
            self.input_views.clear();
        }
        self.input_views.push((texture.clone(), view.clone()));

        Ok(view)
    }
}

fn letterbox(input_width: u32, input_height: u32, output_width: u32, output_height: u32) -> RECT {
    let scale = f64::min(
        output_width as f64 / input_width as f64,
        output_height as f64 / input_height as f64,
    );
    let width = (input_width as f64 * scale).round() as i32;
    let height = (input_height as f64 * scale).round() as i32;
    let left = (output_width as i32 - width) / 2;
    let top = (output_height as i32 - height) / 2;

    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}
