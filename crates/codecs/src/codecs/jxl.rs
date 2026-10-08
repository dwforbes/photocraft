//! JPEG XL. Decoding uses jxl-oxide (pure Rust, the whole format: VarDCT and modular, 8/16-bit
//! and float, alpha, ICC, EXIF/XMP boxes, animation). Encoding here is lossless only, with
//! zune-jpegxl's modular encoder (8/16-bit gray or RGB, ± alpha) wrapped in the ISO BMFF
//! container written below when EXIF or XMP is kept. Lossy files, and files that keep an ICC
//! profile, are written by libjxl's `cjxl` from `photocraft-io` when it is installed.
//!
//! The codestream records the orientation and the decoder applies it, so EXIF Orientation is
//! rewritten to 1 (the specification says the EXIF tag must be ignored). CMYK files are refused
//! rather than shown with the wrong colours. Both libraries run under `catch_unwind`: a panic on a
//! hostile file is an error, never a crash.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::Format;
use crate::error::CodecError;
use crate::fidelity::Plan;
use crate::image::{ChannelLayout, DecodeWarning, Image, Metadata, SampleType};
use crate::options::{EncodeOptions, Limits};

const F: Format = Format::Jxl;

/// The container's signature box (the whole of it).
const SIGNATURE: &[u8; 12] = b"\0\0\0\x0cJXL \r\n\x87\n";

fn err(e: impl std::fmt::Display) -> CodecError {
    CodecError::malformed(F, e)
}

/// Runs `f`, turning a panic in jxl-oxide or zune-jpegxl into `on_panic`.
fn guarded<T>(on_panic: impl FnOnce() -> CodecError, f: impl FnOnce() -> Result<T, CodecError>) -> Result<T, CodecError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(on_panic()))
}

pub(crate) fn decode(bytes: &[u8], limits: &Limits, keep_orientation: bool) -> Result<Image, CodecError> {
    guarded(|| err("the JPEG XL decoder failed on this file"), || decode_inner(bytes, limits, keep_orientation))
}

fn decode_inner(bytes: &[u8], limits: &Limits, keep_orientation: bool) -> Result<Image, CodecError> {
    use jxl_oxide::image::BitDepth;
    use jxl_oxide::{AllocTracker, AuxBoxData, JxlImage, PixelFormat};

    let image = JxlImage::builder().alloc_tracker(AllocTracker::with_limit(limits.alloc_usize())).read(Cursor::new(bytes)).map_err(err)?;
    let layout = match image.pixel_format() {
        PixelFormat::Gray => ChannelLayout::Gray,
        PixelFormat::Graya => ChannelLayout::GrayA,
        PixelFormat::Rgb => ChannelLayout::Rgb,
        PixelFormat::Rgba => ChannelLayout::Rgba,
        PixelFormat::Cmyk | PixelFormat::Cmyka => return Err(CodecError::unsupported(F, "CMYK JPEG XL files are not supported yet")),
    };
    let sample = match image.image_header().metadata.bit_depth {
        BitDepth::IntegerSample { bits_per_sample } if bits_per_sample <= 8 => SampleType::U8,
        BitDepth::IntegerSample { bits_per_sample } if bits_per_sample <= 16 => SampleType::U16,
        // Wider integers and every float encoding keep their precision as f32.
        _ => SampleType::F32,
    };
    let (w, h) = (image.width(), image.height());
    limits.check(w, h, layout, sample)?;
    let keyframes = image.num_loaded_keyframes();
    if keyframes == 0 {
        return Err(err("the file ends before the first frame is complete"));
    }
    let render = image.render_frame(0).map_err(err)?;
    let mut stream = render.stream();
    if (stream.width(), stream.height()) != (w, h) || stream.channels() as usize != layout.channels() {
        return Err(err("the decoded frame does not match the image header"));
    }
    let n = (w as usize)
        .checked_mul(h as usize)
        .and_then(|p| p.checked_mul(layout.channels()))
        .ok_or_else(|| CodecError::LimitExceeded("image too large".into()))?;
    let mut img = match sample {
        SampleType::U8 => {
            let mut buf = vec![0u8; n];
            stream.write_to_buffer(&mut buf);
            Image::from_u8(w, h, layout, buf)?
        }
        SampleType::U16 => {
            let mut buf = vec![0u16; n];
            stream.write_to_buffer(&mut buf);
            Image::from_u16(w, h, layout, &buf)?
        }
        SampleType::F16 | SampleType::F32 => {
            let mut buf = vec![0f32; n];
            stream.write_to_buffer(&mut buf);
            Image::from_f32(w, h, layout, &buf)?
        }
    };
    // Untagged RGB means sRGB everywhere else in PhotoCraft, so an sRGB colour encoding is left
    // untagged; any other encoding (and every gray one) is described by the profile of the
    // pixels as rendered, which jxl-oxide synthesizes when the file has none.
    let srgb = !layout.is_gray() && matches!(image.rendered_cicp(), Some([1, 13, 0, _]));
    img.icc = (!srgb).then(|| image.rendered_icc());
    let exif = match image.aux_boxes().first_exif() {
        Ok(AuxBoxData::Data(raw)) => raw.payload().get(raw.tiff_header_offset() as usize..).map(<[u8]>::to_vec),
        _ => None,
    };
    let xmp = match image.aux_boxes().first_xml() {
        AuxBoxData::Data(x) => String::from_utf8(x.to_vec()).ok(),
        _ => None,
    };
    img.meta = Metadata {
        exif: exif.map(|e| if keep_orientation { e } else { crate::orientation::upright_exif(&e).into_owned() }),
        xmp: xmp.map(|x| if keep_orientation { x } else { crate::orientation::upright_xmp(&x).into_owned() }),
        ..Default::default()
    };
    if keyframes > 1 {
        img.warnings.push(DecodeWarning::MoreFrames { total: u32::try_from(keyframes).ok() });
    }
    if !image.is_loading_done() {
        img.warnings.push(DecodeWarning::Truncated { format: F });
    }
    let orientation = u16::try_from(image.image_header().metadata.orientation).unwrap_or(1);
    if keep_orientation && orientation != 1 {
        // The decoder always turns the pixels upright; undo it to give the stored ones.
        img = img.oriented(inverse_orientation(orientation))?;
    }
    Ok(img)
}

/// The orientation that undoes `o` (only the two quarter turns differ from their inverse).
fn inverse_orientation(o: u16) -> u16 {
    match o {
        6 => 8,
        8 => 6,
        o => o,
    }
}

pub(crate) fn encode(src: &Image, plan: Plan, opts: &EncodeOptions) -> Result<Vec<u8>, CodecError> {
    use zune_core::bit_depth::BitDepth;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::EncoderOptions;

    let img = src.converted(plan.layout, plan.sample);
    let (w, h) = img.dimensions();
    // The encoder rejects images narrower or shorter than two pixels.
    if w < 2 || h < 2 {
        return Err(CodecError::encode(F, "the built-in JPEG XL encoder needs an image of at least 2×2 pixels"));
    }
    let colorspace = match img.layout() {
        ChannelLayout::Gray => ColorSpace::Luma,
        ChannelLayout::GrayA => ColorSpace::LumaA,
        ChannelLayout::Rgb => ColorSpace::RGB,
        ChannelLayout::Rgba => ColorSpace::RGBA,
        l => return Err(CodecError::encode(F, format!("unsupported layout {l:?}"))),
    };
    let depth = match img.sample_type() {
        SampleType::U8 => BitDepth::Eight,
        SampleType::U16 => BitDepth::Sixteen,
        s => return Err(CodecError::encode(F, format!("unsupported sample type {s:?}"))),
    };
    let options = EncoderOptions::new(w as usize, h as usize, colorspace, depth).set_effort(opts.jxl_effort.clamp(1, 10) * 2);
    // One thread per core on native; wasm is single-threaded.
    #[cfg(not(target_arch = "wasm32"))]
    let options = options.set_num_threads(u8::try_from(std::thread::available_parallelism().map_or(1, |n| n.get())).unwrap_or(u8::MAX));
    let data = img.data();
    let codestream = guarded(
        || CodecError::encode(F, "the built-in JPEG XL encoder failed on this image"),
        || {
            let mut out = Vec::new();
            zune_jpegxl::JxlSimpleEncoder::new(data, options).encode(&mut out).map_err(|e| CodecError::encode(F, format!("{e:?}").trim()))?;
            Ok(out)
        },
    )?;
    let codestream =
        if depth == BitDepth::Sixteen && img.layout().has_alpha() { with_16_bit_alpha(codestream, w, h, img.layout().is_gray())? } else { codestream };
    let exif = if opts.embed_metadata { img.meta.exif.as_deref().map(crate::orientation::upright_exif) } else { None };
    let xmp = if opts.embed_metadata { img.meta.xmp.as_deref().map(crate::orientation::upright_xmp) } else { None };
    if exif.is_none() && xmp.is_none() {
        return Ok(codestream);
    }
    Ok(container(&codestream, exif.as_deref(), xmp.as_deref().map(str::as_bytes)))
}

/// zune-jpegxl 0.5.2 always declares its alpha channel 8-bit, so a 16-bit image's alpha would
/// decode wrong. Its image header ends on a byte boundary and nothing after it depends on its
/// length, so the header is rebuilt with a 16-bit alpha channel. The header it wrote is
/// reproduced bit for bit first: if the encoder ever writes something else, this is an error,
/// never a corrupt file.
fn with_16_bit_alpha(codestream: Vec<u8>, w: u32, h: u32, gray: bool) -> Result<Vec<u8>, CodecError> {
    let written = image_header(w, h, gray, false);
    let Some(rest) = codestream.strip_prefix(written.as_slice()) else {
        return Err(CodecError::encode(F, "the built-in JPEG XL encoder wrote an unexpected header"));
    };
    let mut out = image_header(w, h, gray, true);
    out.extend_from_slice(rest);
    Ok(out)
}

/// The image header zune-jpegxl writes for a 16-bit image with alpha (`alpha_16`: with the
/// alpha channel declared 16-bit, as it should be).
fn image_header(w: u32, h: u32, gray: bool, alpha_16: bool) -> Vec<u8> {
    let mut b = Bits::default();
    b.put(16, 0x0AFF); // signature
    b.put(1, 0); // not a small size header
    for (i, size) in [h, w].into_iter().enumerate() {
        let v = u64::from(size.saturating_sub(1));
        let (sel, n) = match v {
            v if v < 1 << 9 => (0, 9),
            v if v < 1 << 13 => (1, 13),
            v if v < 1 << 18 => (2, 18),
            _ => (3, 30),
        };
        b.put(2, sel);
        b.put(n, v);
        if i == 0 {
            b.put(3, 0); // no aspect ratio: the width follows
        }
    }
    b.put(1, 0); // metadata not all default
    b.put(1, 0); // no extra fields
    b.put(1, 0); // integer samples
    b.put(2, 3); // bits per sample: 1 + u(6)
    b.put(6, 15);
    b.put(1, 0); // a 16-bit buffer is not sufficient
    b.put(2, 1); // one extra channel
    if alpha_16 {
        b.put(1, 0); // not the default (8-bit) alpha channel
        b.put(2, 0); // type: alpha
        b.put(1, 0); // integer samples
        b.put(2, 3); // bits per sample: 1 + u(6)
        b.put(6, 15);
        b.put(2, 0); // dim_shift 0
        b.put(2, 0); // no name
        b.put(1, 0); // straight (unassociated) alpha
    } else {
        b.put(1, 1); // the default alpha channel
    }
    b.put(1, 0); // not XYB
    if gray {
        b.put(1, 0); // colour encoding not all default
        b.put(1, 0); // no ICC profile
        b.put(2, 1); // grayscale
        b.put(2, 1); // D65
        b.put(1, 0); // no gamma
        b.put(2, 2); // transfer function: 2 + u(4)
        b.put(4, 11); // sRGB
        b.put(2, 1); // relative rendering intent
    } else {
        b.put(1, 1); // sRGB
    }
    b.put(2, 0); // no extensions
    b.put(1, 1); // default transform data
    b.finish()
}

/// A least-significant-bit-first bit writer (JPEG XL's bit order).
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u64,
    n: u32,
}

impl Bits {
    fn put(&mut self, bits: u32, v: u64) {
        for i in 0..bits {
            self.acc |= ((v >> i) & 1) << self.n;
            self.n += 1;
            if self.n == 8 {
                self.out.push(self.acc as u8);
                self.acc = 0;
                self.n = 0;
            }
        }
    }

    /// The bytes, zero-padded to a byte boundary.
    fn finish(mut self) -> Vec<u8> {
        if self.n > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// An ISO BMFF box: big-endian size, type, payload.
fn bmff_box(out: &mut Vec<u8>, ty: &[u8; 4], payload: &[u8]) {
    // A payload too large for a 32-bit size uses the 64-bit form (size field 1).
    match u32::try_from(payload.len() + 8) {
        Ok(size) => out.extend_from_slice(&size.to_be_bytes()),
        Err(_) => {
            out.extend_from_slice(&1u32.to_be_bytes());
            out.extend_from_slice(ty);
            out.extend_from_slice(&(payload.len() as u64 + 16).to_be_bytes());
            out.extend_from_slice(payload);
            return;
        }
    }
    out.extend_from_slice(ty);
    out.extend_from_slice(payload);
}

/// The container: signature, `ftyp`, the metadata boxes, then the whole codestream in `jxlc`.
fn container(codestream: &[u8], exif: Option<&[u8]>, xmp: Option<&[u8]>) -> Vec<u8> {
    let mut out = Vec::with_capacity(codestream.len() + 64 + exif.map_or(0, <[u8]>::len) + xmp.map_or(0, <[u8]>::len));
    out.extend_from_slice(SIGNATURE);
    bmff_box(&mut out, b"ftyp", b"jxl \0\0\0\0jxl ");
    if let Some(exif) = exif {
        // The box holds the offset of the TIFF header within the payload, then the payload; ours
        // is the bare TIFF structure.
        let tiff = exif.strip_prefix(b"Exif\0\0").unwrap_or(exif);
        let mut payload = Vec::with_capacity(tiff.len() + 4);
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(tiff);
        bmff_box(&mut out, b"Exif", &payload);
    }
    if let Some(xmp) = xmp {
        bmff_box(&mut out, b"xml ", xmp);
    }
    bmff_box(&mut out, b"jxlc", codestream);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inverse_orientation_undoes_every_orientation() {
        let img = Image::from_u8(3, 2, ChannelLayout::Gray, vec![1, 2, 3, 4, 5, 6]).unwrap();
        for o in 1..=8 {
            let back = img.clone().oriented(o).unwrap().oriented(inverse_orientation(o)).unwrap();
            assert_eq!(back.data(), img.data(), "orientation {o}");
        }
    }

    #[test]
    fn the_container_starts_with_the_signature_and_is_detected() {
        let c = container(&[0xFF, 0x0A], Some(b"Exif\0\0MM\0*"), Some(b"<x/>"));
        assert!(c.starts_with(SIGNATURE));
        assert_eq!(crate::detect(&c), Some(F));
        assert_eq!(crate::detect(&[0xFF, 0x0A, 0]), Some(F));
    }
}
