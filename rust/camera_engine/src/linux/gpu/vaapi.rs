// VA-API capabilities of one GPU: which H.264 and JPEG profiles it decodes and encodes
use std::path::Path;

use super::VaapiCaps;
use crate::linux::vaapi::{
    sys::{
        VAConfigAttribRateControl, VAEntrypoint, VAEntrypointEncSlice, VAEntrypointEncSliceLP,
        VAEntrypointVLD, VAProfile, VAProfileH264ConstrainedBaseline, VAProfileH264High,
        VAProfileH264Main, VAProfileJPEGBaseline,
    },
    Display,
};

// VA_RC_CBR in va.h
const VA_RC_CBR: u32 = 0x2;

pub(super) fn probe(render_node: &Path) -> Result<VaapiCaps, String> {
    let display = Display::open(render_node)?;
    let profiles = display.profiles();
    let mut caps = VaapiCaps {
        driver: display.vendor.clone(),
        ..Default::default()
    };

    for profile in [
        VAProfileH264ConstrainedBaseline,
        VAProfileH264Main,
        VAProfileH264High,
    ] {
        if !profiles.contains(&profile) {
            continue;
        }
        let supported = display.entrypoints(profile);
        caps.h264_decode |= supported.contains(&VAEntrypointVLD);
        caps.h264_encode |= supported.contains(&VAEntrypointEncSlice)
            && supports_cbr(&display, profile, VAEntrypointEncSlice);
        caps.h264_encode_low_power |= supported.contains(&VAEntrypointEncSliceLP)
            && supports_cbr(&display, profile, VAEntrypointEncSliceLP);
    }

    if profiles.contains(&VAProfileJPEGBaseline) {
        caps.jpeg_decode = display
            .entrypoints(VAProfileJPEGBaseline)
            .contains(&VAEntrypointVLD);
    }

    Ok(caps)
}

// Low power encoders may lack bitrate control when their firmware (HuC) is not loaded
fn supports_cbr(display: &Display, profile: VAProfile, entrypoint: VAEntrypoint) -> bool {
    display
        .config_attribute(profile, entrypoint, VAConfigAttribRateControl)
        .is_some_and(|modes| modes & VA_RC_CBR != 0)
}
