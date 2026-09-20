use std::ffi::c_void;

use crossbeam_channel::Sender;
use windows::{
    core::{AgileReference, Interface, PCWSTR},
    Media::Capture::{
        Frames::{
            MediaFrameArrivedEventArgs, MediaFrameReader, MediaFrameSource, MediaFrameSourceKind,
        },
        MediaCapture,
    },
    Win32::{
        Foundation::{CloseHandle, HANDLE},
        Graphics::{
            Direct3D11::{ID3D11Texture2D, D3D11_TEXTURE2D_DESC},
            Dxgi::{IDXGIResource1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE},
        },
        System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess,
    },
};

use crate::{CameraError, VideoDimensions};

// Internal state of native capture
#[derive(Debug)]
pub struct InnerNativeCapture {
    media_capture: AgileReference<MediaCapture>,
    media_source: AgileReference<MediaFrameSource>,
    frame_reader: AgileReference<MediaFrameReader>,
}

impl InnerNativeCapture {
    pub async fn new() -> Result<Self, CameraError> {
        // 2. Initialize MediaCapture
        let media_capture_agile = {
            let media_capture = MediaCapture::new()?;
            AgileReference::new(&media_capture)?
        };

        let async_op = media_capture_agile.resolve()?.InitializeAsync()?;

        async_op.await?;

        // 3. Search video source
        let frame_sources = media_capture_agile.resolve()?.FrameSources()?;
        let mut selected_source = None;

        for pair in frame_sources {
            let source = pair.Value()?;
            let kind = source.Info()?.SourceKind()?;

            if kind == MediaFrameSourceKind::Color {
                struct SendSource(windows::Media::Capture::Frames::MediaFrameSource);
                unsafe impl Send for SendSource {}
                selected_source = Some(SendSource(source));
                break;
            }
        }

        let source = selected_source.ok_or_else(|| CameraError::CommonError {
            msg: "No camera found".to_string(),
        })?;

        let media_source_agile = AgileReference::new(&source.0)?;

        let frame_reader_agile = {
            let agile_op = media_capture_agile
                .resolve()?
                .CreateFrameReaderAsync(&source.0)?;

            let frame_reader = agile_op.await?;

            AgileReference::new(&frame_reader)?
        };

        Ok(InnerNativeCapture {
            media_capture: media_capture_agile,
            media_source: media_source_agile,
            frame_reader: frame_reader_agile,
        })
    }

    pub fn get_dimensions_from_media_capture(&self) -> Result<VideoDimensions, CameraError> {
        let media_source = self.media_source.resolve()?;
        let current_format = media_source.CurrentFormat()?;

        // 2. Acessa as propriedades específicas de vídeo (VideoFormat)
        let video_format = current_format.VideoFormat()?;

        let width = video_format.Width()?;
        let height = video_format.Height()?;

        Ok(VideoDimensions { width, height })
    }

    pub async fn start(&self, sender_channel: Sender<u64>) -> Result<(), CameraError> {
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
                    sender_channel.try_send(handle.0 as u64)
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

            self.frame_reader.resolve()?.FrameArrived(&event_handler)?;
        }

        let async_op = self.frame_reader.resolve()?.StartAsync()?;

        // 6. Start streaming
        async_op.await?;

        Ok(())
    }

    pub async fn stop(&self) -> Result<(), CameraError> {
        let async_op = self.frame_reader.resolve()?.StopAsync()?;

        async_op.await?;
        self.frame_reader.resolve()?.Close()?;
        self.media_capture.resolve()?.Close()?;

        Ok(())
    }
}
