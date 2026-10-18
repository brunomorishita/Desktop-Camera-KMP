// VA-API: hardware video decode on Intel (iHD, i965) and AMD (Mesa)
mod api;
pub(crate) mod h264;
pub(crate) mod jpeg;
pub(crate) mod surface;
#[rustfmt::skip]
pub(crate) mod sys;

pub(crate) use api::{Api, Display};
pub(crate) use h264::H264Decoder;
pub(crate) use jpeg::JpegDecoder;
pub(crate) use surface::DecodedFrame;
