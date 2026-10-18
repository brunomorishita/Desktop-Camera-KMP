// Picks which GPU and API run each stage of the pipeline. The preview is always shown by the
// GPU driving the monitor, so every stage that also runs there shares frames as DMA-BUFs
// without copies. That is why Auto keeps everything on the display GPU when it can, even
// with a stronger dedicated GPU around: on hybrid laptops (Intel + NVIDIA) frames would
// otherwise cross PCIe twice per frame and keep the dedicated GPU awake
use std::path::{Path, PathBuf};

use super::{GpuInfo, Vendor};

const PREFERENCE_ENV: &str = "CAMERA_ENGINE_GPU";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Workload {
    // Camera already sends H.264: streamed as is, decoded only for the preview
    H264Passthrough,
    // MJPEG camera: JPEG decoded on the GPU, then encoded to H.264
    MjpegTranscode,
    // Uncompressed camera (YUYV/NV12): converted by a shader, then encoded to H.264
    RawTranscode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum GpuPreference {
    Auto,
    Vendor(Vendor),
    RenderNode(PathBuf),
}

impl GpuPreference {
    // CAMERA_ENGINE_GPU=intel | nvidia | amd | /dev/dri/renderD129
    pub fn from_env() -> Self {
        std::env::var(PREFERENCE_ENV)
            .map(|value| Self::parse(&value))
            .unwrap_or(GpuPreference::Auto)
    }

    fn parse(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "intel" => GpuPreference::Vendor(Vendor::Intel),
            "nvidia" => GpuPreference::Vendor(Vendor::Nvidia),
            "amd" => GpuPreference::Vendor(Vendor::Amd),
            "" | "auto" => GpuPreference::Auto,
            _ => GpuPreference::RenderNode(PathBuf::from(value.trim())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DecodeApi {
    Vaapi,
    Nvdec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EncodeApi {
    Vaapi { low_power: bool },
    Nvenc,
}

// Stages are indices into the probed GPU list
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PipelinePlan {
    pub display_gpu: usize,
    pub decode: Option<(usize, DecodeApi)>,
    pub encode: Option<(usize, EncodeApi)>,
}

impl PipelinePlan {
    // Every stage on the display GPU: no frame is copied between GPUs
    pub fn is_zero_copy(&self) -> bool {
        let on_display = |gpu: usize| gpu == self.display_gpu;
        self.decode.is_none_or(|(gpu, _)| on_display(gpu))
            && self.encode.is_none_or(|(gpu, _)| on_display(gpu))
    }

    pub fn describe(&self, gpus: &[GpuInfo]) -> String {
        let node = |gpu: usize| gpus[gpu].render_node.display().to_string();
        let mut text = format!("preview on {}", node(self.display_gpu));
        if let Some((gpu, api)) = self.decode {
            text += &format!(", decode {api:?} on {}", node(gpu));
        }
        if let Some((gpu, api)) = self.encode {
            text += &format!(", encode {api:?} on {}", node(gpu));
        }
        text += if self.is_zero_copy() {
            " (zero copy)"
        } else {
            " (copies frames between GPUs)"
        };
        text
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Codec {
    H264,
    Jpeg,
}

pub(crate) fn plan(
    gpus: &[GpuInfo],
    workload: Workload,
    preference: &GpuPreference,
) -> Result<PipelinePlan, String> {
    let ranking = Ranking::new(gpus, preference)?;
    let decode = match workload {
        Workload::H264Passthrough => Some(
            ranking
                .best_decoder(gpus, Codec::H264)
                .ok_or("No GPU can decode H.264 (VA-API or NVDEC) for the preview")?,
        ),
        Workload::MjpegTranscode => Some(
            ranking
                .best_decoder(gpus, Codec::Jpeg)
                .ok_or("No GPU can decode MJPEG (VA-API or NVDEC)")?,
        ),
        Workload::RawTranscode => None,
    };
    let encode = match workload {
        Workload::H264Passthrough => None,
        Workload::MjpegTranscode | Workload::RawTranscode => Some(
            ranking
                .best_encoder(gpus)
                .ok_or("No GPU can encode H.264 with constant bitrate (VA-API or NVENC)")?,
        ),
    };

    Ok(PipelinePlan {
        display_gpu: ranking.display_gpu,
        decode,
        encode,
    })
}

// Decoder feeding the preview, chosen as plan() does but without needing an encoder (the
// preview works before streaming does). None when it would not run on the display GPU
pub(crate) fn preview_decoder(
    gpus: &[GpuInfo],
    codec: Codec,
    preference: &GpuPreference,
) -> Option<(usize, DecodeApi)> {
    let ranking = Ranking::new(gpus, preference).ok()?;
    ranking
        .best_decoder(gpus, codec)
        .filter(|(gpu, _)| *gpu == ranking.display_gpu)
}

struct Ranking {
    // Preferred GPU: the chosen one, or the display GPU on Auto
    home: usize,
    display_gpu: usize,
}

impl Ranking {
    fn new(gpus: &[GpuInfo], preference: &GpuPreference) -> Result<Self, String> {
        if gpus.is_empty() {
            return Err("No GPU found under /dev/dri".to_string());
        }

        let display_gpu = gpus
            .iter()
            .position(|gpu| gpu.drives_display)
            .or_else(|| gpus.iter().position(|gpu| gpu.boot_vga))
            .unwrap_or(0);
        let home = match preference {
            GpuPreference::Auto => display_gpu,
            GpuPreference::Vendor(vendor) => gpus
                .iter()
                .position(|gpu| gpu.vendor == *vendor)
                .ok_or_else(|| format!("No {vendor:?} GPU found"))?,
            GpuPreference::RenderNode(path) => gpus
                .iter()
                .position(|gpu| gpu.render_node == Path::new(path))
                .ok_or_else(|| format!("GPU {} not found", path.display()))?,
        };
        Ok(Ranking { home, display_gpu })
    }

    // Home first; when it lacks the feature, the display GPU at least avoids a copy back
    fn gpu_score(&self, gpu: usize) -> u32 {
        if gpu == self.home {
            100
        } else if gpu == self.display_gpu {
            50
        } else {
            0
        }
    }

    fn best_decoder(&self, gpus: &[GpuInfo], codec: Codec) -> Option<(usize, DecodeApi)> {
        let mut candidates = Vec::new();
        for (index, gpu) in gpus.iter().enumerate() {
            let score = self.gpu_score(index);
            if let Some(Ok(nvidia)) = &gpu.nvidia {
                let supported = match codec {
                    Codec::H264 => nvidia.nvdec_h264,
                    Codec::Jpeg => nvidia.nvdec_jpeg,
                };
                // Native API of the driver; VA-API on NVIDIA is a translation layer over it
                if supported {
                    candidates.push((score + 10, index, DecodeApi::Nvdec));
                }
            }
            if let Ok(vaapi) = &gpu.vaapi {
                let supported = match codec {
                    Codec::H264 => vaapi.h264_decode,
                    Codec::Jpeg => vaapi.jpeg_decode,
                };
                if supported {
                    candidates.push((score, index, DecodeApi::Vaapi));
                }
            }
        }
        best(candidates)
    }

    fn best_encoder(&self, gpus: &[GpuInfo]) -> Option<(usize, EncodeApi)> {
        let mut candidates = Vec::new();
        for (index, gpu) in gpus.iter().enumerate() {
            let score = self.gpu_score(index);
            if let Some(Ok(nvidia)) = &gpu.nvidia {
                if nvidia.nvenc_h264 {
                    candidates.push((score + 10, index, EncodeApi::Nvenc));
                }
            }
            if let Ok(vaapi) = &gpu.vaapi {
                if vaapi.h264_encode_low_power {
                    candidates.push((score + 5, index, EncodeApi::Vaapi { low_power: true }));
                }
                if vaapi.h264_encode {
                    candidates.push((score, index, EncodeApi::Vaapi { low_power: false }));
                }
            }
        }
        best(candidates)
    }
}

// Highest score wins, ties go to the first GPU listed
fn best<T>(candidates: Vec<(u32, usize, T)>) -> Option<(usize, T)> {
    candidates
        .into_iter()
        .fold(
            None,
            |best: Option<(u32, usize, T)>, candidate| match best {
                Some(current) if current.0 >= candidate.0 => Some(current),
                _ => Some(candidate),
            },
        )
        .map(|(_, index, api)| (index, api))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::gpu::{NvidiaCaps, VaapiCaps};

    fn intel(drives_display: bool) -> GpuInfo {
        GpuInfo {
            render_node: PathBuf::from("/dev/dri/renderD128"),
            pci_address: "0000:00:02.0".to_string(),
            vendor: Vendor::Intel,
            device_id: 0x5916,
            kernel_driver: "i915".to_string(),
            drives_display,
            boot_vga: true,
            vaapi: Ok(VaapiCaps {
                driver: "iHD".to_string(),
                h264_decode: true,
                jpeg_decode: true,
                h264_encode: true,
                h264_encode_low_power: true,
            }),
            nvidia: None,
        }
    }

    fn nvidia(drives_display: bool, caps: Option<NvidiaCaps>) -> GpuInfo {
        GpuInfo {
            render_node: PathBuf::from("/dev/dri/renderD129"),
            pci_address: "0000:01:00.0".to_string(),
            vendor: Vendor::Nvidia,
            device_id: 0x134d,
            kernel_driver: if caps.is_some() { "nvidia" } else { "nouveau" }.to_string(),
            drives_display,
            boot_vga: false,
            vaapi: Err("no VA-API driver for this GPU".to_string()),
            nvidia: caps.map(Ok),
        }
    }

    fn full_nvidia() -> NvidiaCaps {
        NvidiaCaps {
            name: "GeForce RTX".to_string(),
            nvenc_h264: true,
            nvdec_h264: true,
            nvdec_jpeg: true,
        }
    }

    #[test]
    fn hybrid_laptop_keeps_everything_on_the_display_gpu() {
        let gpus = [intel(true), nvidia(false, Some(full_nvidia()))];

        let passthrough = plan(&gpus, Workload::H264Passthrough, &GpuPreference::Auto).unwrap();
        assert_eq!(passthrough.decode, Some((0, DecodeApi::Vaapi)));
        assert_eq!(passthrough.encode, None);
        assert!(passthrough.is_zero_copy());

        let mjpeg = plan(&gpus, Workload::MjpegTranscode, &GpuPreference::Auto).unwrap();
        assert_eq!(mjpeg.decode, Some((0, DecodeApi::Vaapi)));
        assert_eq!(
            mjpeg.encode,
            Some((0, EncodeApi::Vaapi { low_power: true }))
        );
        assert!(mjpeg.is_zero_copy());
    }

    #[test]
    fn forced_gpu_without_nvenc_falls_back_to_the_display_gpu_for_encoding() {
        // GeForce 940MX on the proprietary driver: NVDEC yes, NVENC no
        let gm108 = NvidiaCaps {
            name: "GeForce 940MX".to_string(),
            nvenc_h264: false,
            nvdec_h264: true,
            nvdec_jpeg: true,
        };
        let gpus = [intel(true), nvidia(false, Some(gm108))];
        let preference = GpuPreference::Vendor(Vendor::Nvidia);

        let mjpeg = plan(&gpus, Workload::MjpegTranscode, &preference).unwrap();
        assert_eq!(mjpeg.decode, Some((1, DecodeApi::Nvdec)));
        assert_eq!(
            mjpeg.encode,
            Some((0, EncodeApi::Vaapi { low_power: true }))
        );
        assert!(!mjpeg.is_zero_copy());
    }

    #[test]
    fn desktop_nvidia_uses_its_native_apis() {
        let gpus = [nvidia(true, Some(full_nvidia()))];

        let raw = plan(&gpus, Workload::RawTranscode, &GpuPreference::Auto).unwrap();
        assert_eq!(raw.decode, None);
        assert_eq!(raw.encode, Some((0, EncodeApi::Nvenc)));

        let passthrough = plan(&gpus, Workload::H264Passthrough, &GpuPreference::Auto).unwrap();
        assert_eq!(passthrough.decode, Some((0, DecodeApi::Nvdec)));
    }

    #[test]
    fn display_gpu_without_encoder_uses_another_gpu() {
        let mut no_encoder = intel(true);
        if let Ok(vaapi) = &mut no_encoder.vaapi {
            vaapi.h264_encode = false;
            vaapi.h264_encode_low_power = false;
        }
        let gpus = [no_encoder, nvidia(false, Some(full_nvidia()))];

        let raw = plan(&gpus, Workload::RawTranscode, &GpuPreference::Auto).unwrap();
        assert_eq!(raw.encode, Some((1, EncodeApi::Nvenc)));
        assert!(!raw.is_zero_copy());
    }

    #[test]
    fn preview_decoder_needs_no_encoder_and_stays_on_the_display_gpu() {
        let mut no_encoder = intel(true);
        if let Ok(vaapi) = &mut no_encoder.vaapi {
            vaapi.h264_encode = false;
            vaapi.h264_encode_low_power = false;
        }
        let alone = [no_encoder];
        assert!(plan(&alone, Workload::MjpegTranscode, &GpuPreference::Auto).is_err());
        assert_eq!(
            preview_decoder(&alone, Codec::Jpeg, &GpuPreference::Auto),
            Some((0, DecodeApi::Vaapi))
        );

        // Decoding on the NVIDIA GPU would leave the frames away from the screen
        let gpus = [intel(true), nvidia(false, Some(full_nvidia()))];
        assert_eq!(
            preview_decoder(&gpus, Codec::Jpeg, &GpuPreference::Vendor(Vendor::Nvidia)),
            None
        );
    }

    #[test]
    fn reports_missing_capabilities_and_gpus() {
        let gpus = [nvidia(true, None)];
        assert!(plan(&gpus, Workload::RawTranscode, &GpuPreference::Auto).is_err());
        assert!(plan(&[], Workload::H264Passthrough, &GpuPreference::Auto).is_err());
        assert!(plan(
            &[intel(true)],
            Workload::H264Passthrough,
            &GpuPreference::Vendor(Vendor::Nvidia)
        )
        .is_err());
    }

    #[test]
    fn without_connected_monitor_the_boot_gpu_shows_the_preview() {
        let gpus = [nvidia(false, Some(full_nvidia())), intel(false)];
        let passthrough = plan(&gpus, Workload::H264Passthrough, &GpuPreference::Auto).unwrap();
        assert_eq!(passthrough.display_gpu, 1);
        assert_eq!(passthrough.decode, Some((1, DecodeApi::Vaapi)));
    }

    #[test]
    fn parses_preference() {
        assert_eq!(
            GpuPreference::parse("NVIDIA"),
            GpuPreference::Vendor(Vendor::Nvidia)
        );
        assert_eq!(GpuPreference::parse(" auto "), GpuPreference::Auto);
        assert_eq!(
            GpuPreference::parse("/dev/dri/renderD129"),
            GpuPreference::RenderNode(PathBuf::from("/dev/dri/renderD129"))
        );
    }
}
