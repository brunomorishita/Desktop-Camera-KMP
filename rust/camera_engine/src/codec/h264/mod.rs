// H.264 bitstream parsing and decoding process, shared by the hardware decoders: they only
// receive ready parameters (POCs, reference lists, scaling lists) for each slice
pub(crate) mod bits;
pub(crate) mod dpb;
pub(crate) mod params;
pub(crate) mod slice;
#[cfg(test)]
pub(crate) mod testing;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    Invalid(&'static str),
    MissingParameterSet(&'static str),
    Unsupported(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Truncated => write!(f, "H.264 data ends too early"),
            ParseError::Invalid(what) => write!(f, "invalid H.264 {what}"),
            ParseError::MissingParameterSet(which) => write!(f, "H.264 {which} not received yet"),
            ParseError::Unsupported(what) => write!(f, "H.264 {what} not supported"),
        }
    }
}
