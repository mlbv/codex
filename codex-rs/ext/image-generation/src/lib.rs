mod artifact;
mod backend;
mod extension;
mod standalone;
mod tool;

pub use backend::ImageBackendError;
pub use extension::install;
pub use standalone::StandaloneImages;

pub(crate) const IMAGE_GEN_NAMESPACE: &str = "image_gen";
pub(crate) const IMAGEGEN_TOOL_NAME: &str = "imagegen";
pub const IMAGE_MODEL: &str = "gpt-image-2";
