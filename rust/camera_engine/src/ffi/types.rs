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

#[derive(uniffi::Record)]
pub struct VideoDimensions {
    pub width: u32,
    pub height: u32,
}
