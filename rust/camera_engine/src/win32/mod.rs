pub(crate) mod capture;
pub(crate) mod renderer;

// Re-exporta as structs principais para simplificar as chamadas internas
pub(crate) use capture::InnerNativeCapture;
pub(crate) use renderer::InnerNativeRenderer;
