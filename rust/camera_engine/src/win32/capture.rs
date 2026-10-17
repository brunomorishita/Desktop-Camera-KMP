use std::{sync::Arc, time::Instant};

use parking_lot::Mutex;
use windows::{
    core::{AgileReference, Interface, BOOL, PCWSTR},
    Media::{
        Capture::{
            Frames::{
                MediaFrameArrivedEventArgs, MediaFrameReader, MediaFrameSource,
                MediaFrameSourceKind,
            },
            MediaCapture, MediaCaptureInitializationSettings, MediaCaptureMemoryPreference,
            MediaCaptureSharingMode, StreamingCaptureMode,
        },
        MediaProperties::MediaEncodingSubtypes,
    },
    Win32::{
        Foundation::CloseHandle,
        Graphics::{
            Direct3D11::{
                ID3D11Query, ID3D11Texture2D, D3D11_BOX, D3D11_QUERY_DESC, D3D11_QUERY_EVENT,
                D3D11_TEXTURE2D_DESC,
            },
            Dxgi::{IDXGIResource1, DXGI_SHARED_RESOURCE_READ},
        },
        System::WinRT::Direct3D11::IDirect3DDxgiInterfaceAccess,
    },
};

use crate::{
    media::{CaptureSource, FrameHub, FrameRate},
    win32::{
        device::GpuDevice,
        frame::{FramePool, SharedFrame, VideoFrame},
        Win32Backend,
    },
    CameraError, VideoDimensions,
};

// Enough for preview (1-2) + encoder in flight (2-3) + the frame being copied
const FRAME_POOL_SIZE: usize = 8;
// Camera pool textures opened on our device, a camera usually recycles 4-10
const MAX_OPENED_TEXTURES: usize = 16;

// Internal state of native capture
#[derive(Debug)]
pub struct InnerNativeCapture {
    media_capture: AgileReference<MediaCapture>,
    frame_reader: AgileReference<MediaFrameReader>,
    frame_arrived_token: Mutex<Option<i64>>,
    dimensions: VideoDimensions,
    frame_rate: FrameRate,
}

impl CaptureSource<Win32Backend> for InnerNativeCapture {
    async fn open() -> Result<Self, CameraError> {
        // 1. Initialize MediaCapture
        let media_capture_agile = {
            let media_capture = MediaCapture::new()?;
            AgileReference::new(&media_capture)?
        };

        let async_op = {
            let settings = MediaCaptureInitializationSettings::new()?;
            // Default is AudioAndVideo, which would also open the microphone
            settings.SetStreamingCaptureMode(StreamingCaptureMode::Video)?;
            // Auto delivers GPU frames whenever the camera pipeline allows (Cpu would force copies)
            settings.SetMemoryPreference(MediaCaptureMemoryPreference::Auto)?;
            settings.SetSharingMode(MediaCaptureSharingMode::ExclusiveControl)?;
            media_capture_agile
                .resolve()?
                .InitializeWithSettingsAsync(&settings)?
        };

        async_op.await?;

        // 2. Search video source
        let media_source_agile = {
            let frame_sources = media_capture_agile.resolve()?.FrameSources()?;
            let mut selected_source: Option<MediaFrameSource> = None;

            for pair in frame_sources {
                let source = pair.Value()?;
                if source.Info()?.SourceKind()? == MediaFrameSourceKind::Color {
                    selected_source = Some(source);
                    break;
                }
            }

            let source = selected_source.ok_or_else(|| CameraError::CommonError {
                msg: "No camera found".to_string(),
            })?;
            AgileReference::new(&source)?
        };

        let (dimensions, frame_rate) = {
            let current_format = media_source_agile.resolve()?.CurrentFormat()?;
            let video_format = current_format.VideoFormat()?;
            let ratio = current_format.FrameRate()?;
            let (numerator, denominator) = match (ratio.Numerator()?, ratio.Denominator()?) {
                (n, d) if n > 0 && d > 0 => (n, d),
                _ => (30, 1),
            };

            (
                VideoDimensions {
                    width: video_format.Width()?,
                    height: video_format.Height()?,
                },
                FrameRate {
                    numerator,
                    denominator,
                },
            )
        };

        // 3. NV12 is what both the video processor and the H.264 encoder consume,
        // MediaCapture converts on the GPU if the camera delivers something else
        let async_op = media_capture_agile.resolve()?.CreateFrameReaderWithSubtypeAsync(
            &media_source_agile.resolve()?,
            &MediaEncodingSubtypes::Nv12()?,
        )?;
        let frame_reader = async_op.await?;
        let frame_reader_agile = AgileReference::new(&frame_reader)?;

        Ok(InnerNativeCapture {
            media_capture: media_capture_agile,
            frame_reader: frame_reader_agile,
            frame_arrived_token: Mutex::new(None),
            dimensions,
            frame_rate,
        })
    }

    fn dimensions(&self) -> VideoDimensions {
        self.dimensions
    }

    fn frame_rate(&self) -> FrameRate {
        self.frame_rate
    }

    async fn start(
        &self,
        gpu: GpuDevice,
        hub: Arc<FrameHub<SharedFrame>>,
    ) -> Result<(), CameraError> {
        {
            let pool = FramePool::new(
                &gpu.device,
                self.dimensions.width,
                self.dimensions.height,
                FRAME_POOL_SIZE,
            )?;
            let copier = Mutex::new(FrameCopier::new(gpu, pool)?);
            let capture_start = Instant::now();

            let event_handler = windows::Foundation::TypedEventHandler::<
                MediaFrameReader,
                MediaFrameArrivedEventArgs,
            >::new(move |sender, _args| {
                let reader = sender.as_ref().ok_or(windows::core::Error::empty())?;
                // Null when no new frame is available since the last one
                let Ok(frame) = reader.TryAcquireLatestFrame() else {
                    return Ok(());
                };
                let video_frame = frame.VideoMediaFrame()?;
                let d3d_surface = video_frame.Direct3DSurface()?;
                let dxgi_access = d3d_surface.cast::<IDirect3DDxgiInterfaceAccess>()?;
                let camera_texture = unsafe { dxgi_access.GetInterface::<ID3D11Texture2D>()? };

                let timestamp_hns = (capture_start.elapsed().as_nanos() / 100) as i64;
                let copied = copier
                    .lock()
                    .copy(&camera_texture, timestamp_hns)
                    .map_err(|e| windows::core::Error::new(windows::core::HRESULT(-1), e.to_string()))?;

                // `frame` is released after this point, but the copy already finished on the GPU
                if let Some(copied) = copied {
                    hub.publish(Arc::new(copied));
                }

                Ok(())
            });

            let token = self.frame_reader.resolve()?.FrameArrived(&event_handler)?;
            *self.frame_arrived_token.lock() = Some(token);
        }

        let async_op = self.frame_reader.resolve()?.StartAsync()?;

        // 4. Start streaming
        async_op.await?;

        Ok(())
    }

    async fn stop(&self) -> Result<(), CameraError> {
        let async_op = self.frame_reader.resolve()?.StopAsync()?;

        async_op.await?;
        if let Some(token) = self.frame_arrived_token.lock().take() {
            self.frame_reader.resolve()?.RemoveFrameArrived(token)?;
        }
        self.frame_reader.resolve()?.Close()?;
        self.media_capture.resolve()?.Close()?;

        Ok(())
    }
}

// Copies camera textures (owned by MediaCapture's device) into our frame pool
struct FrameCopier {
    gpu: GpuDevice,
    pool: Arc<FramePool>,
    copy_done: ID3D11Query,
    // (camera texture, same texture opened on our device, its size)
    opened: Vec<(ID3D11Texture2D, ID3D11Texture2D, u32, u32)>,
}

impl FrameCopier {
    fn new(gpu: GpuDevice, pool: Arc<FramePool>) -> Result<Self, CameraError> {
        let mut query: Option<ID3D11Query> = None;
        unsafe {
            gpu.device.CreateQuery(
                &D3D11_QUERY_DESC {
                    Query: D3D11_QUERY_EVENT,
                    MiscFlags: 0,
                },
                Some(&mut query),
            )
        }?;

        Ok(FrameCopier {
            gpu,
            pool,
            copy_done: query.ok_or_else(|| CameraError::CommonError {
                msg: "Failed to create GPU query".to_string(),
            })?,
            opened: Vec::new(),
        })
    }

    fn copy(
        &mut self,
        camera_texture: &ID3D11Texture2D,
        timestamp_hns: i64,
    ) -> Result<Option<VideoFrame>, CameraError> {
        // Every pool texture still in use: drop this frame
        let Some(frame) = self.pool.acquire(timestamp_hns) else {
            return Ok(None);
        };

        let (source, source_width, source_height) = self.open_on_our_device(camera_texture)?;
        let copy_box = D3D11_BOX {
            left: 0,
            top: 0,
            front: 0,
            right: source_width.min(self.pool.width),
            bottom: source_height.min(self.pool.height),
            back: 1,
        };

        unsafe {
            let context = &self.gpu.context;
            context.CopySubresourceRegion(frame.texture(), 0, 0, 0, 0, &source, 0, Some(&copy_box));

            // Camera texture returns to MediaCapture's pool once the handler returns,
            // so wait until the GPU actually finished reading it
            context.End(&self.copy_done);
            let mut done = BOOL(0);
            while context
                .GetData(
                    &self.copy_done,
                    Some(&mut done as *mut BOOL as *mut _),
                    std::mem::size_of::<BOOL>() as u32,
                    0,
                )
                .is_ok()
                && !done.as_bool()
            {
                std::thread::yield_now();
            }
        }

        Ok(Some(frame))
    }

    fn open_on_our_device(
        &mut self,
        camera_texture: &ID3D11Texture2D,
    ) -> Result<(ID3D11Texture2D, u32, u32), CameraError> {
        // Camera recycles its pool, so each texture is shared and opened only once
        if let Some((_, opened, width, height)) =
            self.opened.iter().find(|(camera, ..)| camera == camera_texture)
        {
            return Ok((opened.clone(), *width, *height));
        }

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { camera_texture.GetDesc(&mut desc) };

        let opened: ID3D11Texture2D = unsafe {
            let handle = camera_texture.cast::<IDXGIResource1>()?.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0,
                PCWSTR::null(),
            )?;
            let opened = self.gpu.device.OpenSharedResource1(handle);
            let _ = CloseHandle(handle);
            opened?
        };

        // Camera reallocated its pool (format change): forget the old textures
        if self.opened.len() >= MAX_OPENED_TEXTURES {
            self.opened.clear();
        }
        self.opened
            .push((camera_texture.clone(), opened.clone(), desc.Width, desc.Height));

        Ok((opened, desc.Width, desc.Height))
    }
}
