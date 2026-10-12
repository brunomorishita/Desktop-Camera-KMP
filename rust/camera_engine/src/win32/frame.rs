use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crossbeam_channel::{bounded, Receiver, Sender};
use parking_lot::Mutex;
use windows::Win32::Graphics::{
    Direct3D11::{
        ID3D11Device1, ID3D11Texture2D, D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
    },
    Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC},
};

use crate::CameraError;

// Fixed set of NV12 textures owned by our device. Camera frames are copied into them,
// so consumers (preview, encoder) can hold a frame without blocking MediaCapture's own pool
pub struct FramePool {
    free: Mutex<Vec<ID3D11Texture2D>>,
    pub width: u32,
    pub height: u32,
}

impl FramePool {
    pub fn new(
        device: &ID3D11Device1,
        width: u32,
        height: u32,
        count: usize,
    ) -> Result<Arc<Self>, CameraError> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_NV12,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };

        let mut textures = Vec::with_capacity(count);
        for _ in 0..count {
            let mut texture: Option<ID3D11Texture2D> = None;
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut texture)) }?;
            textures.push(texture.ok_or_else(|| CameraError::CommonError {
                msg: "Failed to create frame pool texture".to_string(),
            })?);
        }

        Ok(Arc::new(FramePool {
            free: Mutex::new(textures),
            width,
            height,
        }))
    }

    // None when every texture is still in use by some consumer: caller drops the frame
    pub fn acquire(self: &Arc<Self>, timestamp_hns: i64) -> Option<VideoFrame> {
        let texture = self.free.lock().pop()?;
        Some(VideoFrame {
            texture: Some(texture),
            pool: self.clone(),
            timestamp_hns,
        })
    }
}

pub struct VideoFrame {
    texture: Option<ID3D11Texture2D>,
    pool: Arc<FramePool>,
    // 100ns units, relative to capture start
    pub timestamp_hns: i64,
}

impl VideoFrame {
    pub fn texture(&self) -> &ID3D11Texture2D {
        // Only taken on drop
        self.texture.as_ref().expect("texture taken before drop")
    }

    pub fn width(&self) -> u32 {
        self.pool.width
    }

    pub fn height(&self) -> u32 {
        self.pool.height
    }
}

impl Drop for VideoFrame {
    fn drop(&mut self) {
        // Texture goes back to the pool when the last consumer releases the frame
        if let Some(texture) = self.texture.take() {
            self.pool.free.lock().push(texture);
        }
    }
}

pub type SharedFrame = Arc<VideoFrame>;

// Fan-out of captured frames to every consumer (preview windows and encoder)
#[derive(Default)]
pub struct FrameHub {
    subscribers: Mutex<Vec<(u64, Sender<SharedFrame>)>>,
    next_id: AtomicU64,
}

impl FrameHub {
    pub fn subscribe(&self, capacity: usize) -> (u64, Receiver<SharedFrame>) {
        let (sender, receiver) = bounded(capacity);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.subscribers.lock().push((id, sender));
        (id, receiver)
    }

    pub fn unsubscribe(&self, id: u64) {
        self.subscribers.lock().retain(|(sub_id, _)| *sub_id != id);
    }

    pub fn publish(&self, frame: SharedFrame) {
        // Never blocks the capture thread: a consumer that is behind just misses this frame
        for (_, sender) in self.subscribers.lock().iter() {
            let _ = sender.try_send(frame.clone());
        }
    }
}
