// Finds the GPUs of the machine, what video engines each one exposes and which of them
// should decode, encode and show the preview. Vendor libraries are loaded with dlopen at
// run time, so a missing driver only disables that path instead of failing to load
mod nvidia;
mod select;
mod sysfs;
mod vaapi;

use std::{fmt, path::PathBuf};

pub(crate) use select::{
    plan, preview_decoder, Codec, DecodeApi, EncodeApi, GpuPreference, PipelinePlan, Workload,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Vendor {
    Intel,
    Nvidia,
    Amd,
    Other(u16),
}

impl Vendor {
    fn from_pci_id(id: u16) -> Self {
        match id {
            0x8086 => Vendor::Intel,
            0x10de => Vendor::Nvidia,
            0x1002 => Vendor::Amd,
            other => Vendor::Other(other),
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct GpuInfo {
    pub render_node: PathBuf,
    pub pci_address: String,
    pub vendor: Vendor,
    pub device_id: u16,
    pub kernel_driver: String,
    // A monitor is connected to it, so the compositor shows our windows from this GPU
    pub drives_display: bool,
    pub boot_vga: bool,
    // Err holds why the API is not usable on this GPU
    pub vaapi: Result<VaapiCaps, String>,
    // None when the GPU is not on the proprietary NVIDIA driver
    pub nvidia: Option<Result<NvidiaCaps, String>>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct VaapiCaps {
    pub driver: String,
    pub h264_decode: bool,
    pub jpeg_decode: bool,
    // Only encoders with constant bitrate count, it is what the RTSP stream uses
    pub h264_encode: bool,
    // Fixed function encoder (VDENC on Intel): leaves the shader cores free, uses less power
    pub h264_encode_low_power: bool,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct NvidiaCaps {
    pub name: String,
    pub nvenc_h264: bool,
    pub nvdec_h264: bool,
    pub nvdec_jpeg: bool,
}

pub(crate) fn probe() -> Vec<GpuInfo> {
    sysfs::render_nodes()
        .into_iter()
        .map(|node| {
            let vaapi = vaapi::probe(&node.render_node);
            let nvidia = (node.kernel_driver == "nvidia").then(|| nvidia::probe(&node.pci_address));
            GpuInfo {
                render_node: node.render_node,
                pci_address: node.pci_address,
                vendor: Vendor::from_pci_id(node.vendor_id),
                device_id: node.device_id,
                kernel_driver: node.kernel_driver,
                drives_display: node.drives_display,
                boot_vga: node.boot_vga,
                vaapi,
                nvidia,
            }
        })
        .collect()
}

impl fmt::Display for GpuInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let vendor_id = match self.vendor {
            Vendor::Intel => 0x8086,
            Vendor::Nvidia => 0x10de,
            Vendor::Amd => 0x1002,
            Vendor::Other(id) => id,
        };
        write!(
            f,
            "{} {:?} [{vendor_id:04x}:{:04x}] driver={} pci={}",
            self.render_node.display(),
            self.vendor,
            self.device_id,
            self.kernel_driver,
            self.pci_address,
        )?;
        if self.drives_display {
            write!(f, " (display)")?;
        }

        match &self.vaapi {
            Ok(caps) => write!(
                f,
                "\n  VA-API: {} | H.264 decode={} encode={} low-power encode={} | JPEG decode={}",
                caps.driver,
                yes_no(caps.h264_decode),
                yes_no(caps.h264_encode),
                yes_no(caps.h264_encode_low_power),
                yes_no(caps.jpeg_decode),
            )?,
            Err(reason) => write!(f, "\n  VA-API: unavailable ({reason})")?,
        }

        match &self.nvidia {
            Some(Ok(caps)) => write!(
                f,
                "\n  NVIDIA: {} | NVENC H.264={} | NVDEC H.264={} JPEG={}",
                caps.name,
                yes_no(caps.nvenc_h264),
                yes_no(caps.nvdec_h264),
                yes_no(caps.nvdec_jpeg),
            ),
            Some(Err(reason)) => write!(f, "\n  NVIDIA: unavailable ({reason})"),
            None => Ok(()),
        }
    }
}

fn yes_no(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "probes the real hardware, run with: cargo test -- --ignored --nocapture"]
    fn prints_gpu_report() {
        let gpus = probe();
        for gpu in &gpus {
            println!("{gpu}");
        }

        let preference = GpuPreference::from_env();
        for workload in [
            Workload::H264Passthrough,
            Workload::MjpegTranscode,
            Workload::RawTranscode,
        ] {
            match plan(&gpus, workload, &preference) {
                Ok(plan) => println!("{workload:?}: {}", plan.describe(&gpus)),
                Err(e) => println!("{workload:?}: {e}"),
            }
        }
    }
}
