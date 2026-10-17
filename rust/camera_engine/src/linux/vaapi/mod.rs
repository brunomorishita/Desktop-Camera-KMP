// VA-API: hardware video decode on Intel (iHD, i965) and AMD (Mesa)
mod api;
pub(crate) mod decoder;
#[rustfmt::skip]
pub(crate) mod sys;

pub(crate) use api::{Api, Display};
pub(crate) use decoder::{DecodedFrame, H264Decoder};
