#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CameraError {
    #[error("Win Error: {msg}")]
    Windows { code: i32, msg: String },

    #[error("Stream Error: {msg}")]
    StreamError { msg: String },

    #[error("Stream Error: {msg}")]
    CommonError { msg: String },
}

impl From<windows::core::Error> for CameraError {
    fn from(value: windows::core::Error) -> Self {
        CameraError::Windows {
            code: value.code().0,
            msg: value.message().to_string(),
        }
    }
}

// Estrutura enviada para o Kotlin via FFI
#[derive(uniffi::Record)]
pub struct FrameGpuShared {
    pub share_handle: u64,
    pub width: u32,
    pub height: u32,
}

#[derive(uniffi::Record, Debug, Clone, Copy)]
pub struct VideoDimensions {
    pub width: u32,
    pub height: u32,
}

// Which RTP transports clients may pick in SETUP
#[derive(uniffi::Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTransport {
    Any,
    TcpOnly,
    UdpOnly,
}

impl StreamTransport {
    pub fn allows(self, tcp: bool) -> bool {
        match self {
            StreamTransport::Any => true,
            StreamTransport::TcpOnly => tcp,
            StreamTransport::UdpOnly => !tcp,
        }
    }
}

#[derive(uniffi::Record, Debug, Clone)]
pub struct StreamConfig {
    /// IPv4 address to listen on, "0.0.0.0" for every interface
    pub bind_address: String,
    pub port: u16,
    /// URL path of the stream, e.g. "camera" -> rtsp://<ip>:<port>/camera
    pub path: String,
    pub transport: StreamTransport,
    pub bitrate: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_policy() {
        assert!(StreamTransport::Any.allows(true) && StreamTransport::Any.allows(false));
        assert!(StreamTransport::TcpOnly.allows(true) && !StreamTransport::TcpOnly.allows(false));
        assert!(!StreamTransport::UdpOnly.allows(true) && StreamTransport::UdpOnly.allows(false));
    }
}
