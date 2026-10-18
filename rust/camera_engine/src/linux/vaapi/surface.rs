// Decoded pictures on the GPU: fixed pools of VA surfaces shared by the decoders, and the
// frames handed to consumers (preview, encoder), which return their surface when dropped
use std::{
    ffi::c_void,
    os::fd::{FromRawFd, OwnedFd},
    ptr,
    sync::Arc,
};

use parking_lot::Mutex;

use super::{api::Display, sys::*};

// A decoded picture, NV12 on the GPU. Its surface returns to the pool when the last
// clone drops, so consumers should not hold frames for long
#[derive(Clone)]
pub(crate) struct DecodedFrame {
    pub(super) surface: Arc<Surface>,
    pub timestamp_hns: i64,
    // Visible area inside the coded surface: (x, y, width, height)
    pub visible: (u32, u32, u32, u32),
    pub full_range: bool,
    // YUV to RGB matrix: BT.709 (HD) or BT.601 (SD)
    pub bt709: bool,
}

impl DecodedFrame {
    // Same value for the same GPU memory: lets consumers cache what they build per surface
    // (e.g. an imported texture). Surfaces are recycled, the pool changes on a new size
    pub fn surface_key(&self) -> (usize, VASurfaceID) {
        (Arc::as_ptr(&self.surface.pool) as usize, self.surface.id)
    }

    // Waits until the GPU finished decoding into the surface
    pub fn sync(&self) -> Result<(), String> {
        let display = &self.surface.pool.display;
        unsafe {
            display.api.check(
                "vaSyncSurface",
                (display.api.sync_surface)(display.raw, self.surface.id),
            )
        }
    }

    // For the encoder, which reads the surface where the decoder wrote it
    pub(super) fn surface_id(&self) -> VASurfaceID {
        self.surface.id
    }

    pub(super) fn display(&self) -> &Arc<Display> {
        &self.surface.pool.display
    }

    pub fn coded_size(&self) -> (u32, u32) {
        (self.surface.pool.width, self.surface.pool.height)
    }

    // Shares the surface as DMA-BUF objects (one or more planes) for import elsewhere,
    // e.g. by the preview renderer. The descriptors are owned by the caller
    pub fn export(&self) -> Result<ExportedFrame, String> {
        let display = &self.surface.pool.display;
        let mut descriptor = VADRMPRIMESurfaceDescriptor::default();
        unsafe {
            display.api.check(
                "vaSyncSurface",
                (display.api.sync_surface)(display.raw, self.surface.id),
            )?;
            display.api.check(
                "vaExportSurfaceHandle",
                (display.api.export_surface_handle)(
                    display.raw,
                    self.surface.id,
                    VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                    VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_COMPOSED_LAYERS,
                    &mut descriptor as *mut _ as *mut c_void,
                ),
            )?;
        }
        let objects = descriptor.objects[..descriptor.num_objects as usize]
            .iter()
            .map(|object| ExportedObject {
                fd: unsafe { OwnedFd::from_raw_fd(object.fd) },
                size: object.size,
                modifier: object.drm_format_modifier,
            })
            .collect();
        let layer = descriptor.layers[0];
        Ok(ExportedFrame {
            drm_format: layer.drm_format,
            objects,
            planes: (0..layer.num_planes as usize)
                .map(|plane| {
                    (
                        layer.object_index[plane],
                        layer.offset[plane],
                        layer.pitch[plane],
                    )
                })
                .collect(),
        })
    }

    // Copies the picture to memory as tightly packed NV12 (Y plane then UV plane), visible
    // area only. Slow: for tests and snapshots, not for every frame
    pub fn read_nv12(&self) -> Result<Vec<u8>, String> {
        let pool = &self.surface.pool;
        let display = &pool.display;
        let api = display.api;
        let (x, y, width, height) = self.visible;
        let mut format = VAImageFormat {
            fourcc: VA_FOURCC_NV12,
            byte_order: 1,
            bits_per_pixel: 12,
            ..Default::default()
        };
        let mut image = VAImage::default();
        unsafe {
            api.check(
                "vaCreateImage",
                (api.create_image)(
                    display.raw,
                    &mut format,
                    pool.width as i32,
                    pool.height as i32,
                    &mut image,
                ),
            )?;
        }

        let result = (|| unsafe {
            api.check(
                "vaGetImage",
                (api.get_image)(
                    display.raw,
                    self.surface.id,
                    0,
                    0,
                    pool.width,
                    pool.height,
                    image.image_id,
                ),
            )?;
            let mut data: *mut c_void = ptr::null_mut();
            api.check(
                "vaMapBuffer",
                (api.map_buffer)(display.raw, image.buf, &mut data),
            )?;
            let base = data as *const u8;

            let mut nv12 = Vec::with_capacity((width * height * 3 / 2) as usize);
            for row in 0..height {
                let start = image.offsets[0] as usize + ((y + row) * image.pitches[0] + x) as usize;
                nv12.extend_from_slice(std::slice::from_raw_parts(base.add(start), width as usize));
            }
            for row in 0..height / 2 {
                let start =
                    image.offsets[1] as usize + ((y / 2 + row) * image.pitches[1] + x) as usize;
                nv12.extend_from_slice(std::slice::from_raw_parts(base.add(start), width as usize));
            }
            (api.unmap_buffer)(display.raw, image.buf);
            Ok(nv12)
        })();

        unsafe { (api.destroy_image)(display.raw, image.image_id) };
        result
    }
}

pub(crate) struct ExportedObject {
    pub fd: OwnedFd,
    pub size: u32,
    pub modifier: u64,
}

pub(crate) struct ExportedFrame {
    // DRM fourcc of the whole picture (NV12)
    pub drm_format: u32,
    pub objects: Vec<ExportedObject>,
    // Per plane: (object index, offset, pitch)
    pub planes: Vec<(u32, u32, u32)>,
}

pub(super) struct Surface {
    pub id: VASurfaceID,
    pool: Arc<SurfacePool>,
}

impl Drop for Surface {
    fn drop(&mut self) {
        self.pool.free.lock().push(self.id);
    }
}

// Fixed set of surfaces of one size, destroyed when the last frame using them is gone
pub(super) struct SurfacePool {
    display: Arc<Display>,
    pub ids: Vec<VASurfaceID>,
    free: Mutex<Vec<VASurfaceID>>,
    pub width: u32,
    pub height: u32,
}

impl SurfacePool {
    // `rt_format` is the chroma layout (VA_RT_FORMAT_*), `fourcc` the memory layout
    pub fn new(
        display: Arc<Display>,
        rt_format: u32,
        fourcc: u32,
        width: u32,
        height: u32,
        count: u32,
    ) -> Result<Arc<Self>, String> {
        let api = display.api;
        let mut format = VASurfaceAttrib {
            type_: VASurfaceAttribPixelFormat,
            flags: VA_SURFACE_ATTRIB_SETTABLE,
            value: VAGenericValue {
                type_: VAGenericValueTypeInteger,
                value: _VAGenericValue__bindgen_ty_1 { i: fourcc as i32 },
            },
        };
        let mut ids = vec![VA_INVALID_SURFACE; count as usize];
        unsafe {
            api.check(
                "vaCreateSurfaces",
                (api.create_surfaces)(
                    display.raw,
                    rt_format,
                    width,
                    height,
                    ids.as_mut_ptr(),
                    count,
                    &mut format,
                    1,
                ),
            )?;
        }
        Ok(Arc::new(SurfacePool {
            display,
            free: Mutex::new(ids.clone()),
            ids,
            width,
            height,
        }))
    }

    pub fn acquire(self: &Arc<Self>) -> Option<Surface> {
        let id = self.free.lock().pop()?;
        Some(Surface {
            id,
            pool: self.clone(),
        })
    }
}

impl Drop for SurfacePool {
    fn drop(&mut self) {
        unsafe {
            (self.display.api.destroy_surfaces)(
                self.display.raw,
                self.ids.as_mut_ptr(),
                self.ids.len() as i32,
            )
        };
    }
}
