pub(crate) mod deep_exr;
pub(crate) mod exr;
pub(crate) mod exr_cryptomatte;
pub(crate) mod heif;
pub(crate) mod jpeg;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod jxl;

/// The browser build leaves JPEG XL out (its decoder would push the wasm past its size cap): the
/// format is still recognised, and reading or writing it says it isn't included.
#[cfg(target_arch = "wasm32")]
pub(crate) mod jxl {
    use crate::Format;
    use crate::error::CodecError;
    use crate::fidelity::Plan;
    use crate::image::Image;
    use crate::options::{EncodeOptions, Limits};

    const NOT_IN_BUILD: &str = "JPEG XL isn't included in the browser build of PhotoCraft";

    pub(crate) fn decode(_: &[u8], _: &Limits, _: bool) -> Result<Image, CodecError> {
        Err(CodecError::unsupported(Format::Jxl, NOT_IN_BUILD))
    }

    pub(crate) fn encode(_: &Image, _: Plan, _: &EncodeOptions) -> Result<Vec<u8>, CodecError> {
        Err(CodecError::unsupported(Format::Jxl, NOT_IN_BUILD))
    }
}
pub(crate) mod png;
pub(crate) mod pnm;
pub(crate) mod tiff;
pub(crate) mod tiff_ifd;
pub(crate) mod via_image;
pub(crate) mod vp8;
pub(crate) mod webp;
