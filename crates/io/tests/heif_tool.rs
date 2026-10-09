//! HEIC and AVIF through the installed helpers (libheif's `heif-enc`/`heif-dec`, macOS `sips`).
//! Each test passes with a note when its tool isn't installed; CI installs libheif so they run.

#![cfg(not(target_arch = "wasm32"))]

use photocraft_codecs::{ChannelLayout, EncodeOptions, Format, Image, SampleType};
use photocraft_color::{ColorMode, SampleType as DocSample};
use photocraft_io::heif_tool::{self, Decoder, Encoder};
use photocraft_io::*;

/// A smooth 96×64 picture (what lossy coding is for), 8 or 16-bit, with or without alpha.
fn picture(sample: SampleType, alpha: bool) -> Image {
    let layout = if alpha { ChannelLayout::Rgba } else { ChannelLayout::Rgb };
    let (w, h) = (96u32, 64u32);
    let vals: Vec<f32> = (0..h)
        .flat_map(|y| {
            (0..w).flat_map(move |x| {
                let px = [x as f32 / (w - 1) as f32, y as f32 / (h - 1) as f32, (x + y) as f32 / (w + h - 2) as f32, 0.25 + 0.75 * x as f32 / (w - 1) as f32];
                px.into_iter().take(if alpha { 4 } else { 3 })
            })
        })
        .collect();
    Image::from_normalized(w, h, layout, sample, &vals).unwrap()
}

fn mean_diff(a: &Image, b: &Image) -> f32 {
    let (a, b) = (a.to_normalized(), b.convert(a.layout(), a.sample_type()).to_normalized());
    assert_eq!(a.len(), b.len());
    a.iter().zip(&b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
}

/// The tools available here: what PhotoCraft found, plus `sips` on a Mac.
fn encoders() -> Vec<Encoder> {
    let mut v: Vec<Encoder> = heif_tool::encoder().into_iter().cloned().collect();
    if cfg!(target_os = "macos") && !v.iter().any(|e| matches!(e, Encoder::Sips { .. })) {
        v.push(Encoder::Sips { path: "/usr/bin/sips".into() });
    }
    if v.is_empty() {
        eprintln!("no HEIC encoder installed: skipping");
    }
    v
}

fn decoders() -> Vec<Decoder> {
    let mut v: Vec<Decoder> = heif_tool::decoder().into_iter().cloned().collect();
    if cfg!(target_os = "macos") && !v.iter().any(|d| matches!(d, Decoder::Sips { .. })) {
        v.push(Decoder::Sips { path: "/usr/bin/sips".into() });
    }
    if v.is_empty() {
        eprintln!("no HEIC decoder installed: skipping");
    }
    v
}

fn heif_enc() -> Option<Encoder> {
    encoders().into_iter().find(|e| matches!(e, Encoder::HeifEnc { .. }))
}

#[test]
fn every_encoder_and_decoder_round_trip_heic_and_avif() {
    for enc in encoders() {
        for format in [Format::Heif, Format::Avif] {
            for (sample, alpha) in [(SampleType::U8, false), (SampleType::U8, true), (SampleType::U16, false)] {
                let img = picture(sample, alpha);
                let (bytes, warnings) = heif_tool::encode_with(&enc, &img, format, &EncodeOptions::default()).unwrap();
                assert_eq!(photocraft_codecs::detect(&bytes), Some(format));
                assert!(warnings.iter().any(|w| w == "lossy compression"), "{warnings:?}");
                assert_eq!(warnings.iter().any(|w| w.contains("reduced to 10-bit")), sample == SampleType::U16, "{warnings:?}");
                for dec in decoders() {
                    let what = format!("{} → {} {format:?} {sample:?} alpha {alpha}", enc.name(), dec.name());
                    let (back, notes) = heif_tool::decode_with(&dec, &bytes, format).unwrap_or_else(|e| panic!("{what}: {e}"));
                    assert_eq!(back.dimensions(), img.dimensions(), "{what}");
                    assert_eq!(back.layout().has_alpha(), alpha, "{what}");
                    assert!(notes.iter().any(|n| n.contains(dec.name())), "{notes:?}");
                    let d = mean_diff(&img, &back);
                    assert!(d < 0.02, "{what}: mean diff {d}");
                }
            }
        }
    }
}

#[test]
fn heif_enc_writes_lossless_files() {
    let Some(enc) = heif_enc() else { return };
    let img = picture(SampleType::U8, false);
    let opts = EncodeOptions { heif_quality: None, ..Default::default() };
    let (bytes, warnings) = heif_tool::encode_with(&enc, &img, Format::Heif, &opts).unwrap();
    assert!(!warnings.iter().any(|w| w.contains("lossy")), "{warnings:?}");
    for dec in decoders().into_iter().filter(|d| matches!(d, Decoder::HeifDec { .. })) {
        let (back, _) = heif_tool::decode_with(&dec, &bytes, Format::Heif).unwrap();
        assert_eq!(back.convert(ChannelLayout::Rgb, SampleType::U8).data(), img.data(), "lossless");
    }
}

#[test]
fn rotation_is_applied_once_by_every_decoder() {
    let Some(Encoder::HeifEnc { path, .. }) = heif_enc() else { return };
    // `--rotate-cw` writes the rotation into the container, as a phone does.
    let dir = std::env::temp_dir().join(format!("photocraft-heif-rot-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("in.png");
    std::fs::write(&png, photocraft_codecs::encode(&picture(SampleType::U8, false), Format::Png, &EncodeOptions::default()).unwrap()).unwrap();
    let out = dir.join("rot.heic");
    let ok = std::process::Command::new(&path).arg(&png).arg("-o").arg(&out).args(["-q", "90", "--rotate-cw", "90"]).status().unwrap().success();
    let bytes = std::fs::read(&out);
    let _ = std::fs::remove_dir_all(&dir);
    if !ok {
        eprintln!("this heif-enc has no --rotate-cw: skipping");
        return;
    }
    let bytes = bytes.unwrap();
    let want = picture(SampleType::U8, false).oriented(6).unwrap();
    for dec in decoders() {
        let (back, _) = heif_tool::decode_with(&dec, &bytes, Format::Heif).unwrap();
        assert_eq!(back.dimensions(), (64, 96), "{} turns it upright", dec.name());
        assert!(mean_diff(&want, &back) < 0.02, "{}: {}", dec.name(), mean_diff(&want, &back));
        let exif = back.meta.exif.as_deref().map_or(1, photocraft_codecs::exif_orientation);
        assert_eq!(exif, 1, "{}: not turned again later", dec.name());
    }
}

#[test]
fn a_colour_signalled_only_by_nclx_gets_its_profile() {
    let Some(Encoder::HeifEnc { path, .. }) = heif_enc() else { return };
    let dir = std::env::temp_dir().join(format!("photocraft-heif-nclx-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("in.png");
    std::fs::write(&png, photocraft_codecs::encode(&picture(SampleType::U8, false), Format::Png, &EncodeOptions::default()).unwrap()).unwrap();
    let out = dir.join("p3.heic");
    let ok = std::process::Command::new(&path)
        .arg(&png)
        .arg("-o")
        .arg(&out)
        .args(["--colour_primaries=12", "--transfer_characteristic=13", "--matrix_coefficients=6"])
        .status()
        .unwrap()
        .success();
    let bytes = std::fs::read(&out);
    let _ = std::fs::remove_dir_all(&dir);
    if !ok {
        eprintln!("this heif-enc has no nclx options: skipping");
        return;
    }
    let bytes = bytes.unwrap();
    assert_eq!(heif_tool::nclx(&bytes).map(|n| (n.primaries, n.transfer)), Some((12, 13)));
    for dec in decoders() {
        let (back, notes) = heif_tool::decode_with(&dec, &bytes, Format::Heif).unwrap();
        assert!(back.icc.is_some(), "{}: Display P3 must not open as sRGB ({notes:?})", dec.name());
    }
}

fn document(alpha: bool) -> photocraft_doc::Document {
    let img = picture(SampleType::U8, alpha);
    let mut d = photocraft_doc::Document::new("s", photocraft_geom::Size::new(96, 64), ColorMode::Rgb, DocSample::U8);
    let mut s = photocraft_raster::Surface::new(d.pixel_format());
    let vals: Vec<f32> = img.convert(ChannelLayout::Rgba, SampleType::U8).to_normalized();
    s.write_region(d.bounds(), &vals);
    d.layers.push(photocraft_doc::Layer::new("Background", photocraft_doc::LayerContent::Raster(s)));
    d
}

#[test]
fn export_and_open_through_photocraft_io() {
    if heif_tool::encoder().is_none() || heif_tool::decoder().is_none() {
        eprintln!("libheif (or macOS) isn't installed: skipping");
        return;
    }
    let mut d = document(true);
    d.icc_profile = Some(photocraft_cms::Builtin::DisplayP3.profile().to_bytes());
    for (ext, format) in [("heic", Format::Heif), ("avif", Format::Avif)] {
        let r = export(&d, &format!("x.{ext}"), &ExportOptions::default()).unwrap();
        assert_eq!(photocraft_codecs::detect(&r.bytes), Some(format));
        let back = import(&format!("x.{ext}"), &r.bytes).unwrap();
        assert_eq!(back.document.size, d.size);
        assert!(back.document.icc_profile.is_some(), "{ext}: the Display P3 profile is kept");
        if format == Format::Avif || !cfg!(feature = "heif") {
            assert!(back.warnings.iter().any(|w| w.contains("opened with")), "{:?}", back.warnings);
        }
    }
}

#[test]
fn without_a_helper_heic_export_says_what_to_install() {
    let d = document(false);
    let opts = ExportOptions { external_tools: false, ..ExportOptions::default() };
    let r = export(&d, "x.heic", &opts);
    assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("heif-enc")), "{:?}", r.as_ref().err());
}

#[cfg(target_os = "macos")]
#[test]
fn sips_says_what_it_drops() {
    let sips = Encoder::Sips { path: "/usr/bin/sips".into() };
    let mut img = picture(SampleType::U8, false);
    img.meta.xmp = Some("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"/>".into());
    let (_, warnings) = heif_tool::encode_with(&sips, &img, Format::Heif, &EncodeOptions { heif_quality: None, ..Default::default() }).unwrap();
    assert!(warnings.iter().any(|w| w.contains("XMP")), "{warnings:?}");
    assert!(warnings.iter().any(|w| w.contains("lossless")), "{warnings:?}");
}
