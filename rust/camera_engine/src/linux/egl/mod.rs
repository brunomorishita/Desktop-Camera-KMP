// Camera preview with EGL + OpenGL ES on the X11 window of the UI (also under XWayland)
// Binding tables: not every entry point and constant is used
#[allow(dead_code)]
mod api;
pub(crate) mod preview;

pub(crate) use preview::EglPreview;
