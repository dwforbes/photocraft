//! JPEG XL export and import: the built-in lossless encoder always, and libjxl's `cjxl` when it is
//! installed. The `cjxl` tests pass with a note when it isn't (CI installs libjxl so they run).

mod common;

use common::*;
use photocraft_color::{ColorMode, SampleType};
use photocraft_io::*;

fn single(mode: ColorMode, depth: SampleType, alpha: bool) -> photocraft_doc::Document {
    let mut d = photocraft_doc::Document::new("s", photocraft_geom::Size::new(9, 6), mode, depth);
    let fmt = d.pixel_format();
    d.layers.push(raster("Background", fmt, d.bounds(), 3, alpha));
    d
}

fn max_diff(a: &photocraft_doc::Document, b: &photocraft_doc::Document) -> f32 {
    let r = a.bounds();
    let (va, vb) = (a.layers[0].surface().unwrap().read_region(r), b.layers[0].surface().unwrap().read_region(r));
    assert_eq!(va.len(), vb.len());
    va.iter().zip(&vb).map(|(x, y)| (x - y).abs()).fold(0.0f32, f32::max)
}

fn built_in() -> ExportOptions {
    ExportOptions { external_tools: false, ..ExportOptions::default() }
}

/// `cjxl`, or `None` after printing why the test is skipped.
fn cjxl() -> Option<&'static jxl_tool::Cjxl> {
    let found = jxl_tool::cjxl();
    if found.is_none() {
        eprintln!("cjxl isn't installed: skipping the libjxl part of this test");
    }
    found
}

/// Exported and opened again, with the export's warnings; JPEG XL has no resolution field, so
/// the "DPI dropped" warning every such format gives is left out.
fn round_trip(d: &photocraft_doc::Document, opts: &ExportOptions) -> (photocraft_doc::Document, Vec<String>) {
    let r = export(d, "x.jxl", opts).expect("export");
    assert_eq!(photocraft_codecs::detect(&r.bytes), Some(photocraft_codecs::Format::Jxl));
    assert!(r.warnings.iter().any(|w| w.contains("DPI")), "{:?}", r.warnings);
    // An export by cjxl says so, and whether it ran sandboxed; the built-in encoder adds no note.
    let note = r.warnings.iter().find(|w| w.starts_with("JPEG XL written with cjxl"));
    if let Some(note) = note {
        assert_eq!(note.ends_with("(sandboxed)"), photocraft_io::tool_sandbox().is_some(), "{note}");
    }
    let warnings = r.warnings.into_iter().filter(|w| !w.contains("DPI") && !w.starts_with("JPEG XL written with")).collect();
    (import("x.jxl", &r.bytes).expect("import").document, warnings)
}

#[test]
fn the_built_in_encoder_is_lossless() {
    for (mode, depth, alpha) in [
        (ColorMode::Rgb, SampleType::U8, true),
        (ColorMode::Rgb, SampleType::U16, true),
        (ColorMode::Rgb, SampleType::U16, false),
        (ColorMode::Grayscale, SampleType::U8, false),
        (ColorMode::Grayscale, SampleType::U16, false),
    ] {
        let d = single(mode, depth, alpha);
        let (back, warnings) = round_trip(&d, &built_in());
        assert_eq!((back.mode, back.depth), (mode, depth), "{mode:?} {depth:?}");
        assert_eq!(max_diff(&d, &back), 0.0, "{mode:?} {depth:?} {alpha}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}

#[test]
fn the_built_in_encoder_says_what_it_cannot_do() {
    let mut d = single(ColorMode::Rgb, SampleType::U8, false);
    let lossy = ExportOptions { encode: photocraft_codecs::EncodeOptions { jxl_quality: Some(80), ..Default::default() }, ..built_in() };
    let (back, warnings) = round_trip(&d, &lossy);
    assert!(warnings.iter().any(|w| w.contains("written lossless")), "{warnings:?}");
    assert_eq!(max_diff(&d, &back), 0.0);

    // A wide-gamut document is converted to sRGB (the built-in encoder writes no profile).
    d.icc_profile = Some(photocraft_cms::Builtin::DisplayP3.profile().to_bytes());
    let (back, warnings) = round_trip(&d, &built_in());
    assert!(warnings.iter().any(|w| w.contains("converted to sRGB") && w.contains("cjxl")), "{warnings:?}");
    assert!(back.icc_profile.is_none());
}

#[test]
fn cjxl_keeps_depth_profile_and_metadata() {
    let Some(tool) = cjxl() else { return };
    assert!(tool.version >= jxl_tool::MIN_VERSION);
    let mut d = single(ColorMode::Rgb, SampleType::U16, true);
    d.icc_profile = Some(photocraft_cms::Builtin::DisplayP3.profile().to_bytes());
    d.metadata.xmp = Some("<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"/></x:xmpmeta>".into());
    let (back, warnings) = round_trip(&d, &ExportOptions::default());
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!((back.mode, back.depth), (ColorMode::Rgb, SampleType::U16));
    assert_eq!(max_diff(&d, &back), 0.0, "lossless by default");
    assert!(back.icc_profile.is_some(), "the Display P3 profile is kept");
    // Verified with libjxl 0.12; older versions (Ubuntu 24.04 ships 0.7) aren't checked here.
    if tool.version >= (0, 9, 0) {
        assert!(back.metadata.xmp.as_deref().is_some_and(|x| x.contains("xmpmeta")), "XMP is kept");
    }
}

#[test]
fn cjxl_writes_lossy_files() {
    let Some(_) = cjxl() else { return };
    // A smooth picture: what lossy coding is for.
    let mut d = photocraft_doc::Document::new("s", photocraft_geom::Size::new(160, 120), ColorMode::Rgb, SampleType::U8);
    let mut s = photocraft_raster::Surface::new(d.pixel_format());
    let vals: Vec<f32> = (0..120).flat_map(|y| (0..160).flat_map(move |x| [x as f32 / 159.0, y as f32 / 119.0, (x + y) as f32 / 278.0, 1.0])).collect();
    s.write_region(d.bounds(), &vals);
    d.layers.push(photocraft_doc::Layer::new("Background", photocraft_doc::LayerContent::Raster(s)));
    let lossless = export(&d, "x.jxl", &ExportOptions::default()).unwrap();
    let opts = ExportOptions { encode: photocraft_codecs::EncodeOptions { jxl_quality: Some(60), ..Default::default() }, ..ExportOptions::default() };
    let lossy = export(&d, "x.jxl", &opts).unwrap();
    assert!(lossy.warnings.iter().any(|w| w == "lossy compression"), "{:?}", lossy.warnings);
    assert!(lossy.warnings.first().is_some_and(|w| w.starts_with("JPEG XL written with cjxl")), "{:?}", lossy.warnings);
    assert_ne!(lossy.bytes, lossless.bytes);
    let back = import("x.jxl", &lossy.bytes).unwrap().document;
    assert_eq!(back.size, d.size);
    let r = d.bounds();
    let (va, vb) = (d.layers[0].surface().unwrap().read_region(r), back.layers[0].surface().unwrap().read_region(r));
    let mean = va.iter().zip(&vb).map(|(x, y)| (x - y).abs()).sum::<f32>() / va.len() as f32;
    assert!(mean > 0.0 && mean < 0.01, "lossy, but the same picture: mean diff {mean}");
}

#[test]
fn cjxl_writes_what_the_built_in_encoder_cannot() {
    let Some(_) = cjxl() else { return };
    // One pixel (the built-in encoder needs 2×2) and 32-bit float (reduced to 16-bit, with a warning).
    let d = photocraft_doc::Document::new("s", photocraft_geom::Size::new(1, 1), ColorMode::Rgb, SampleType::U8);
    let mut d = d;
    let fmt = d.pixel_format();
    d.layers.push(raster("Background", fmt, d.bounds(), 1, false));
    assert!(export(&d, "x.jxl", &built_in()).is_err());
    let (back, _) = round_trip(&d, &ExportOptions::default());
    assert_eq!(back.size, d.size);

    let d = single(ColorMode::Rgb, SampleType::F32, false);
    let (back, warnings) = round_trip(&d, &ExportOptions::default());
    assert!(warnings.iter().any(|w| w.contains("reduced to 16-bit")), "{warnings:?}");
    assert_eq!(back.depth, SampleType::U16);
}
