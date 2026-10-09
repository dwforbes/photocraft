//! HEIC and AVIF through the user's own tools: libheif's `heif-enc` and `heif-dec` (once called
//! `heif-convert`), or macOS's built-in `sips`.
//!
//! Every mature HEVC encoder is C or C++ and HEVC is patent-encumbered, so PhotoCraft ships none:
//! a HEIC export hands a PNG of the image (profile, EXIF and XMP included) to `heif-enc`, or to
//! `sips`, which every Mac has. AVIF goes the same way (`heif-enc -A`), ahead of the optional
//! pure-Rust ravif encoder, because these keep the colour profile. Neither tool reads standard
//! input, so the image goes through a private temporary folder.
//!
//! Opening: heic-rs (the `heif` feature) stays the first choice for HEIC. A HEIC it can't open
//! (image sequences, overlays, some 4:4:4 files), every HEIC in a build without the feature, and
//! every AVIF (PhotoCraft has no AV1 decoder) are decoded by `heif-dec` or `sips` to a PNG. Both
//! turn the picture upright: `heif-dec` applies the container's rotation and writes EXIF
//! Orientation 1, `sips` keeps the stored pixels and writes the rotation to EXIF, which the PNG
//! decoder then applies. `heif-dec` leaves out a colour signalled only by an `nclx` box (common
//! outside Apple's photos), so that signal is read from the file and the matching profile
//! attached.
//!
//! Found from `PHOTOCRAFT_HEIF_ENC` / `PHOTOCRAFT_HEIF_DEC` (a path, or `off`), then
//! `heif-enc` / `heif-dec` / `heif-convert` on the `PATH` and in the usual install folders, then
//! `/usr/bin/sips` on macOS. Looked for once per session.

use photocraft_codecs::{EncodeOptions, Format, Image};

use crate::IoError;
#[cfg(not(target_arch = "wasm32"))]
use crate::external::{self, Profile, TempDir};

/// The program that writes HEIC and AVIF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encoder {
    /// libheif's `heif-enc` and its version.
    HeifEnc { path: std::path::PathBuf, version: (u32, u32, u32) },
    /// macOS `sips` (Apple's encoders): no lossless mode, and XMP isn't kept.
    Sips { path: std::path::PathBuf },
}

/// The program that reads HEIC and AVIF.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decoder {
    /// libheif's `heif-dec` (or `heif-convert`, its name before libheif 1.18).
    HeifDec { path: std::path::PathBuf },
    /// macOS `sips`.
    Sips { path: std::path::PathBuf },
}

impl Encoder {
    /// The tool's name, for messages.
    pub fn name(&self) -> &'static str {
        match self {
            Encoder::HeifEnc { .. } => "heif-enc",
            Encoder::Sips { .. } => "sips",
        }
    }
}

impl Decoder {
    /// The tool's name, for messages.
    pub fn name(&self) -> &'static str {
        match self {
            Decoder::HeifDec { .. } => "heif-dec",
            Decoder::Sips { .. } => "sips",
        }
    }
}

/// The oldest `heif-enc` with the options used here (`-q`, `-L`, `-b`, `-A`, PNG input with
/// profile and metadata).
pub const MIN_HEIF_ENC: (u32, u32, u32) = (1, 13, 0);

/// The encoder found on this system, if any.
#[cfg(not(target_arch = "wasm32"))]
pub fn encoder() -> Option<&'static Encoder> {
    static FOUND: std::sync::OnceLock<Option<Encoder>> = std::sync::OnceLock::new();
    FOUND.get_or_init(find_encoder).as_ref()
}

/// The decoder found on this system, if any.
#[cfg(not(target_arch = "wasm32"))]
pub fn decoder() -> Option<&'static Decoder> {
    static FOUND: std::sync::OnceLock<Option<Decoder>> = std::sync::OnceLock::new();
    FOUND.get_or_init(find_decoder).as_ref()
}

/// No program can run in the browser.
#[cfg(target_arch = "wasm32")]
pub fn encoder() -> Option<&'static Encoder> {
    None
}

/// No program can run in the browser.
#[cfg(target_arch = "wasm32")]
pub fn decoder() -> Option<&'static Decoder> {
    None
}

/// What exporting `format` needs when no encoder is installed.
pub fn missing_encoder_message(format: Format) -> String {
    let name = if format == Format::Avif { "AVIF" } else { "HEIC" };
    if cfg!(target_arch = "wasm32") {
        format!("{name} can't be written in the browser")
    } else {
        format!("{name} export needs libheif's heif-enc (or macOS), which wasn't found; install libheif or set PHOTOCRAFT_HEIF_ENC")
    }
}

/// The arguments for `tool` after the program: input, output and settings.
pub fn arguments(
    tool: &Encoder,
    format: Format,
    input: &std::path::Path,
    output: &std::path::Path,
    sixteen_bit: bool,
    opts: &EncodeOptions,
) -> Vec<std::ffi::OsString> {
    let mut a: Vec<std::ffi::OsString> = Vec::new();
    match tool {
        Encoder::HeifEnc { .. } => {
            if format == Format::Avif {
                a.push("-A".into());
            }
            a.push(input.into());
            a.push("-o".into());
            a.push(output.into());
            match opts.heif_quality {
                Some(q) => a.extend(["-q".into(), q.clamp(1, 100).to_string().into()]),
                None => a.push("-L".into()),
            }
            if sixteen_bit {
                // 16-bit input keeps the 10 bits HEIC and AVIF decoders commonly read.
                a.extend(["-b".into(), "10".into()]);
            }
        }
        Encoder::Sips { .. } => {
            let fmt = if format == Format::Avif { "avif" } else { "heic" };
            a.extend(["-s".into(), "format".into(), fmt.into()]);
            a.extend(["-s".into(), "formatOptions".into(), opts.heif_quality.unwrap_or(100).clamp(1, 100).to_string().into()]);
            a.push(input.into());
            a.push("--out".into());
            a.push(output.into());
        }
    }
    a
}

/// A written file's bytes and the export's warnings.
pub type Encoded = (Vec<u8>, Vec<String>);

/// Encodes `img` (already in the colour model, alpha and profile it is saved with) as HEIC or AVIF
/// with the installed tool. `Ok(None)` when there is none.
pub(crate) fn encode(img: &Image, format: Format, opts: &EncodeOptions) -> Result<Option<Encoded>, IoError> {
    match encoder() {
        Some(tool) => encode_with(tool, img, format, opts).map(Some),
        None => Ok(None),
    }
}

/// [`encode`] with the given tool.
#[cfg(not(target_arch = "wasm32"))]
pub fn encode_with(tool: &Encoder, img: &Image, format: Format, opts: &EncodeOptions) -> Result<Encoded, IoError> {
    use photocraft_codecs::SampleType;
    let mut warnings = Vec::new();
    let name = if format == Format::Avif { "AVIF" } else { "HEIC" };
    match img.sample_type() {
        SampleType::U8 => {}
        SampleType::U16 => warnings.push(format!("16-bit will be reduced to 10-bit for {name}")),
        SampleType::F16 | SampleType::F32 => {
            warnings.push(format!("32-bit float will be reduced to 10-bit for {name}"));
            if img.has_out_of_range() {
                warnings.push("HDR values outside 0..1 will be clipped".to_string());
            }
        }
    }
    match (opts.heif_quality, tool) {
        (None, Encoder::Sips { .. }) => warnings.push("sips can't write lossless files; written at quality 100".to_string()),
        (None, Encoder::HeifEnc { .. }) => {}
        (Some(_), _) => warnings.push("lossy compression".to_string()),
    }
    if opts.embed_metadata {
        if img.meta.xmp.is_some() && matches!(tool, Encoder::Sips { .. }) {
            warnings.push("XMP not supported by sips; it will be dropped".to_string());
        }
        if img.meta.dpi.is_some() {
            warnings.push("resolution (DPI) not supported; it will be dropped".to_string());
        }
        if !img.meta.text.is_empty() {
            warnings.push("text metadata not supported; it will be dropped".to_string());
        }
    }
    let sixteen_bit = img.sample_type() != SampleType::U8;
    let dir = TempDir::new()?;
    let input = dir.write("in.png", &external::png(img, opts)?)?;
    let output = dir.path.join(if format == Format::Avif { "out.avif" } else { "out.heic" });
    let program = match tool {
        Encoder::HeifEnc { path, .. } | Encoder::Sips { path } => path,
    };
    let profile = if matches!(tool, Encoder::Sips { .. }) { Profile::SystemImaging } else { Profile::Tool };
    let mut cmd = external::command(tool.name(), program, &dir.path, profile, external::sandbox_policy())?;
    cmd.cmd.args(arguments(tool, format, &input, &output, sixteen_bit, opts));
    cmd.run(tool.name(), None, external::timeout_for(img), 1 << 20)?;
    let bytes = std::fs::read(&output).map_err(|e| IoError::Unsupported(format!("{} didn't write the {name} file: {e}", tool.name())))?;
    if photocraft_codecs::detect(&bytes) != Some(format) {
        return Err(IoError::Unsupported(format!("{} wrote something that isn't a {name} file", tool.name())));
    }
    Ok((bytes, warnings))
}

#[cfg(target_arch = "wasm32")]
pub fn encode_with(_: &Encoder, _: &Image, _: Format, _: &EncodeOptions) -> Result<Encoded, IoError> {
    Err(IoError::Unsupported("HEIC and AVIF can't be written in the browser".into()))
}

/// Decodes a HEIC or AVIF file with the installed tool: the upright image, tagged with its
/// colour, and a note saying which tool opened it. `Ok(None)` when there is none.
pub(crate) fn decode(bytes: &[u8], format: Format) -> Result<Option<(Image, Vec<String>)>, IoError> {
    match decoder() {
        Some(tool) => decode_with(tool, bytes, format).map(Some),
        None => Ok(None),
    }
}

/// [`decode`] with the given tool.
#[cfg(not(target_arch = "wasm32"))]
pub fn decode_with(tool: &Decoder, bytes: &[u8], format: Format) -> Result<(Image, Vec<String>), IoError> {
    let name = if format == Format::Avif { "AVIF" } else { "HEIC" };
    let dir = TempDir::new()?;
    let input = dir.write(if format == Format::Avif { "in.avif" } else { "in.heic" }, bytes)?;
    let output = dir.path.join("out.png");
    // The file is untrusted: the tool is confined to its job folder (see `external`).
    let mut cmd = match tool {
        Decoder::HeifDec { path } => {
            let mut c = external::command(tool.name(), path, &dir.path, Profile::Tool, external::sandbox_policy())?;
            c.cmd.arg(&input).arg(&output);
            c
        }
        Decoder::Sips { path } => {
            let mut c = external::command(tool.name(), path, &dir.path, Profile::SystemImaging, external::sandbox_policy())?;
            c.cmd.args(["-s", "format", "png"]).arg(&input).arg("--out").arg(&output);
            c
        }
    };
    let confined = cmd.confined;
    // Decoding is quicker than encoding; the size isn't known yet, so allow for a large photo.
    cmd.run(tool.name(), None, std::time::Duration::from_secs(300), 1 << 20)?;
    let png = std::fs::read(&output).map_err(|e| IoError::Unsupported(format!("{} didn't decode the {name} file: {e}", tool.name())))?;
    // The default decode turns the pixels upright from the PNG's EXIF Orientation (see the module docs).
    let mut img = photocraft_codecs::decode_as(Format::Png, &png)?;
    let mut warnings = vec![format!("{name} opened with {}{}", tool.name(), if confined { " (sandboxed)" } else { "" })];
    if img.icc.is_none() && !img.layout().is_gray() {
        match nclx(bytes).map(profile_for) {
            Some(Ok(Some(profile))) => img.icc = Some(profile),
            Some(Err(w)) => warnings.push(w),
            Some(Ok(None)) | None => {}
        }
    }
    Ok((img, warnings))
}

#[cfg(target_arch = "wasm32")]
pub fn decode_with(_: &Decoder, _: &[u8], _: Format) -> Result<(Image, Vec<String>), IoError> {
    Err(IoError::Unsupported("HEIC and AVIF can't be opened with a helper in the browser".into()))
}

/// An `nclx` colour signal: H.273 colour primaries, transfer characteristics, matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nclx {
    pub primaries: u16,
    pub transfer: u16,
    pub matrix: u16,
}

/// The first `nclx` colour box in the file's item properties (`meta` › `iprp` › `ipco` ›
/// `colr`). Bounded: malformed sizes end the search, and boxes nest at most four deep.
pub fn nclx(bytes: &[u8]) -> Option<Nclx> {
    fn search(b: &[u8], depth: u32) -> Option<Nclx> {
        if depth > 4 {
            return None;
        }
        let mut at = 0usize;
        while at.checked_add(8)? <= b.len() {
            let size32 = u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?) as usize;
            let ty: [u8; 4] = b.get(at + 4..at + 8)?.try_into().ok()?;
            let (header, size) = match size32 {
                0 => (8, b.len() - at),
                1 => (16, usize::try_from(u64::from_be_bytes(b.get(at + 8..at + 16)?.try_into().ok()?)).ok()?),
                n => (8, n),
            };
            let end = at.checked_add(size)?;
            if size < header || end > b.len() {
                return None;
            }
            let body = b.get(at + header..end)?;
            match &ty {
                // `meta` is a full box: a version and flags come before its children.
                b"meta" => {
                    if let Some(n) = search(body.get(4..)?, depth + 1) {
                        return Some(n);
                    }
                }
                b"iprp" | b"ipco" => {
                    if let Some(n) = search(body, depth + 1) {
                        return Some(n);
                    }
                }
                b"colr" if body.get(0..4) == Some(b"nclx") => {
                    let v = |i: usize| body.get(i..i + 2).map(|s| u16::from_be_bytes([s[0], s[1]]));
                    return Some(Nclx { primaries: v(4)?, transfer: v(6)?, matrix: v(8)? });
                }
                _ => {}
            }
            at = end;
        }
        None
    }
    search(bytes, 0)
}

/// The profile for an `nclx` signal: `Ok(None)` for sRGB (or unspecified), the matching built-in
/// profile, or a warning when PhotoCraft has no profile for it.
pub fn profile_for(n: Nclx) -> Result<Option<Vec<u8>>, String> {
    use photocraft_cms::Builtin;
    // 2 is "unspecified": treated as sRGB, like an untagged file.
    let srgb_tf = matches!(n.transfer, 2 | 13);
    let rec709_tf = matches!(n.transfer, 1 | 6 | 14 | 15);
    let builtin = match n.primaries {
        1 | 2 if srgb_tf => return Ok(None),
        1 | 2 if n.transfer == 8 => Builtin::LinearSrgb,
        12 if srgb_tf => Builtin::DisplayP3,
        9 if rec709_tf => Builtin::Rec2020,
        _ if matches!(n.transfer, 16 | 18) => {
            return Err(format!(
                "HDR colour ({}) isn't supported yet: the image is shown without tone mapping, as sRGB",
                if n.transfer == 16 { "PQ" } else { "HLG" }
            ));
        }
        _ => return Err(format!("unrecognised colour signal (primaries {}, transfer {}): shown as sRGB", n.primaries, n.transfer)),
    };
    Ok(Some(builtin.profile().to_bytes().as_ref().clone()))
}

#[cfg(not(target_arch = "wasm32"))]
fn find_encoder() -> Option<Encoder> {
    match external::env_override("PHOTOCRAFT_HEIF_ENC") {
        Some(Some(path)) => probe_encoder(&path),
        Some(None) => None,
        None => external::candidates("heif-enc").into_iter().find_map(|p| probe_encoder(&p)).or_else(|| sips_path().map(|path| Encoder::Sips { path })),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn find_decoder() -> Option<Decoder> {
    match external::env_override("PHOTOCRAFT_HEIF_DEC") {
        Some(Some(path)) if is_sips(&path) => Some(Decoder::Sips { path }),
        Some(Some(path)) => path.is_file().then_some(Decoder::HeifDec { path }),
        Some(None) => None,
        None => external::candidates("heif-dec")
            .into_iter()
            .chain(external::candidates("heif-convert"))
            .next()
            .map(|path| Decoder::HeifDec { path })
            .or_else(|| sips_path().map(|path| Decoder::Sips { path })),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn is_sips(path: &std::path::Path) -> bool {
    path.file_name().is_some_and(|n| n == "sips")
}

/// macOS `sips`, which ships with the system.
#[cfg(not(target_arch = "wasm32"))]
fn sips_path() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from("/usr/bin/sips");
    (cfg!(target_os = "macos") && path.is_file()).then_some(path)
}

/// `program` as an encoder: `sips` by name, else a `heif-enc` recent enough.
#[cfg(not(target_arch = "wasm32"))]
fn probe_encoder(program: &std::path::Path) -> Option<Encoder> {
    if is_sips(program) {
        return program.is_file().then(|| Encoder::Sips { path: program.to_path_buf() });
    }
    let version = external::version_of("heif-enc", program, &["--version"])?;
    (version >= MIN_HEIF_ENC).then(|| Encoder::HeifEnc { path: program.to_path_buf(), version })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A box: big-endian size, type, payload.
    fn bx(ty: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(ty);
        b.extend_from_slice(payload);
        b
    }

    fn nclx_box(primaries: u16, transfer: u16) -> Vec<u8> {
        let mut p = b"nclx".to_vec();
        for v in [primaries, transfer, 6] {
            p.extend_from_slice(&v.to_be_bytes());
        }
        p.push(0x80);
        bx(b"colr", &p)
    }

    fn file(colr: &[u8]) -> Vec<u8> {
        let mut meta = vec![0, 0, 0, 0];
        meta.extend(bx(b"hdlr", &[0; 24]));
        meta.extend(bx(b"iprp", &bx(b"ipco", &[bx(b"ispe", &[0; 12]), colr.to_vec()].concat())));
        [bx(b"ftyp", b"heic\0\0\0\0mif1heic"), bx(b"meta", &meta)].concat()
    }

    #[test]
    fn the_nclx_signal_is_found_and_mapped() {
        let p3 = file(&nclx_box(12, 13));
        assert_eq!(nclx(&p3), Some(Nclx { primaries: 12, transfer: 13, matrix: 6 }));
        assert!(profile_for(nclx(&p3).unwrap()).unwrap().is_some(), "Display P3 gets a profile");
        assert_eq!(profile_for(Nclx { primaries: 1, transfer: 13, matrix: 6 }), Ok(None), "sRGB stays untagged");
        assert!(profile_for(Nclx { primaries: 9, transfer: 16, matrix: 9 }).unwrap_err().contains("PQ"));
        assert!(profile_for(Nclx { primaries: 22, transfer: 13, matrix: 6 }).unwrap_err().contains("unrecognised"));
        // A profile box (`prof`) is no nclx signal.
        assert_eq!(nclx(&file(&bx(b"colr", b"prof\0\0\0\0"))), None);
    }

    #[test]
    fn hostile_boxes_end_the_search_without_panicking() {
        let good = file(&nclx_box(12, 13));
        for n in 0..good.len() {
            let _ = nclx(&good[..n]);
        }
        let mut flipped = good.clone();
        for i in 0..flipped.len() {
            flipped[i] ^= 0xFF;
            let _ = nclx(&flipped);
            flipped[i] ^= 0xFF;
        }
        // A 64-bit size past the end, a zero size, and nesting deeper than the limit.
        assert_eq!(nclx(&[0, 0, 0, 1, b'm', b'e', b't', b'a', 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]), None);
        let mut deep = nclx_box(12, 13);
        for _ in 0..10 {
            deep = bx(b"ipco", &deep);
        }
        assert_eq!(nclx(&deep), None);
    }

    #[test]
    fn heif_enc_and_sips_arguments() {
        let enc = Encoder::HeifEnc { path: "heif-enc".into(), version: (1, 23, 6) };
        let (i, o) = (std::path::Path::new("/t/in.png"), std::path::Path::new("/t/out.heic"));
        let args = |t: &Encoder, f, sixteen, q| -> Vec<String> {
            let opts = EncodeOptions { heif_quality: q, ..Default::default() };
            arguments(t, f, i, o, sixteen, &opts).into_iter().map(|a| a.to_string_lossy().into_owned()).collect()
        };
        assert_eq!(args(&enc, Format::Heif, false, Some(80)), ["/t/in.png", "-o", "/t/out.heic", "-q", "80"]);
        assert_eq!(args(&enc, Format::Avif, true, None), ["-A", "/t/in.png", "-o", "/t/out.heic", "-L", "-b", "10"]);
        let sips = Encoder::Sips { path: "/usr/bin/sips".into() };
        assert_eq!(args(&sips, Format::Heif, false, Some(0)), ["-s", "format", "heic", "-s", "formatOptions", "1", "/t/in.png", "--out", "/t/out.heic"]);
        assert_eq!(args(&sips, Format::Avif, false, None)[5], "100", "no lossless mode: the best quality");
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_tool_is_an_error_not_a_panic() {
        let img = Image::from_u8(4, 4, photocraft_codecs::ChannelLayout::Rgb, vec![7; 48]).unwrap();
        let opts = EncodeOptions::default();
        for program in ["/usr/bin/false", "/usr/bin/true", "/nonexistent/heif-enc"] {
            let enc = Encoder::HeifEnc { path: program.into(), version: (1, 23, 6) };
            assert!(encode_with(&enc, &img, Format::Heif, &opts).is_err(), "{program}");
            let dec = Decoder::HeifDec { path: program.into() };
            assert!(decode_with(&dec, b"not a heic", Format::Heif).is_err(), "{program}");
        }
        assert!(probe_encoder(std::path::Path::new("/nonexistent/heif-enc")).is_none());
    }
}
