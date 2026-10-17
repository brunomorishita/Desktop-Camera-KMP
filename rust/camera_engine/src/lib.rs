mod ffi;
mod media;
mod rtsp;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod win32;

// Re-exporta a camada FFI para que o UniFFI consiga encontrar tudo
pub use ffi::*;

// Configura o scaffolding do UniFFI
uniffi::setup_scaffolding!();
