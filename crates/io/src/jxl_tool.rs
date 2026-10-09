//! JPEG XL export through libjxl's `cjxl`, when it is installed.
//!
//! PhotoCraft reads JPEG XL itself (jxl-oxide) and has a pure-Rust lossless encoder
//! (zune-jpegxl), but lossy JPEG XL, and keeping an ICC profile in the file, need libjxl. Rather
//! than linking C++, a JPEG XL export hands the image to the user's own `cjxl` as a PNG (which
//! carries the ICC profile, EXIF and XMP) and takes back the file it writes. libjxl keeps
//! improving without a PhotoCraft release, and nothing changes when it is missing: the built-in
//! encoder writes a lossless file instead, and the export says so.
//!
//! The PNG goes to `cjxl`'s standard input and the JPEG XL file comes back on its standard output
//! (`cjxl - -`, supported since libjxl 0.6), so nothing is written to disk. Before 0.12, `cjxl`
//! read a file named `-` in its working folder instead of standard input when one existed; it
//! runs in an empty private folder (its sandbox, see `external`), so none can. If the piped run fails, it is retried once with
//! temporary files. Memory is the same either way: `cjxl` loads the whole image in both, and the
//! PNG is freed as soon as `cjxl` has read it (or it has been written to the temporary file).
//!
//! Where it is looked for: `PHOTOCRAFT_CJXL` (a path to the program, or `off` to never use it),
//! then the `PATH`, then the usual install folders. A macOS app opened from the Finder gets a
//! `PATH` without Homebrew's folders, so those are searched explicitly. The program found is
//! remembered for the session (restart PhotoCraft after installing libjxl).
//!
//! The program runs directly, never through a shell. It is stopped if it runs past a deadline
//! that grows with the image, and its own error message is passed on when it fails.

use photocraft_codecs::{EncodeOptions, Image};

use crate::IoError;
#[cfg(not(target_arch = "wasm32"))]
use crate::external::{self, Profile, RunError, TempDir};

/// libjxl's `cjxl` as found on this system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cjxl {
    pub path: std::path::PathBuf,
    /// `(major, minor, patch)` from `cjxl --version`.
    pub version: (u32, u32, u32),
}

/// The oldest libjxl whose `cjxl` takes the flags used here (`-d`, `-q`, `-e`, PNG input, `-`
/// for standard input and output).
pub const MIN_VERSION: (u32, u32, u32) = (0, 7, 0);

/// `cjxl`, if it is installed and recent enough (looked for once per session).
#[cfg(not(target_arch = "wasm32"))]
pub fn cjxl() -> Option<&'static Cjxl> {
    static FOUND: std::sync::OnceLock<Option<Cjxl>> = std::sync::OnceLock::new();
    FOUND.get_or_init(find).as_ref()
}

/// No program can run in the browser.
#[cfg(target_arch = "wasm32")]
pub fn cjxl() -> Option<&'static Cjxl> {
    None
}

/// The `cjxl` arguments (after the input and output paths) for these options.
pub fn arguments(opts: &EncodeOptions) -> Vec<String> {
    let effort = opts.jxl_effort.clamp(1, 10).to_string();
    match opts.jxl_quality {
        Some(q) => vec!["-q".into(), q.clamp(1, 100).to_string(), "-e".into(), effort],
        None => vec!["-d".into(), "0".into(), "-e".into(), effort],
    }
}

/// A written file's bytes and the export's warnings.
type Encoded = (Vec<u8>, Vec<String>);

/// Encodes `img` (already in the colour model, alpha and profile it is saved with) with `cjxl`:
/// the file's bytes and what the export changed. `Ok(None)` when `cjxl` isn't available, so the
/// caller uses the built-in encoder.
pub(crate) fn encode(img: &Image, opts: &EncodeOptions) -> Result<Option<Encoded>, IoError> {
    match cjxl() {
        Some(tool) => encode_with(tool, img, opts).map(Some),
        None => Ok(None),
    }
}

/// [`encode`] with the given program.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn encode_with(tool: &Cjxl, img: &Image, opts: &EncodeOptions) -> Result<Encoded, IoError> {
    let mut warnings = Vec::new();
    // PNG holds up to 16 bits per sample: float (HDR) data is clipped to 0..1 on the way.
    if img.sample_type().is_float() {
        warnings.push("32-bit float will be reduced to 16-bit for JPEG XL".to_string());
        if img.has_out_of_range() {
            warnings.push("HDR values outside 0..1 will be clipped".to_string());
        }
    }
    if opts.jxl_quality.is_some() {
        warnings.push("lossy compression".to_string());
    }
    if opts.embed_metadata && img.meta.dpi.is_some() {
        warnings.push("resolution (DPI) not supported; it will be dropped".to_string());
    }
    if opts.embed_metadata && !img.meta.text.is_empty() {
        warnings.push("text metadata not supported; it will be dropped".to_string());
    }
    let timeout = external::timeout_for(img);
    // A JPEG XL file is never much larger than the raw pixels; more output is a misbehaving program.
    let out_cap = u64::try_from(img.data().len()).unwrap_or(u64::MAX).saturating_mul(2).saturating_add(1 << 20);
    let (bytes, confined) = match piped(tool, external::png(img, opts)?, opts, timeout, out_cap) {
        Ok(done) => done,
        // A program that can't start, or that ran out of time, would do the same again.
        Err(RunError::Spawn(e) | RunError::Timeout(e)) => return Err(IoError::Unsupported(e)),
        Err(RunError::Failed(_)) => through_files(tool, external::png(img, opts)?, opts, timeout)?,
    };
    warnings.insert(0, external::tool_note("JPEG XL", "written", "cjxl", confined));
    Ok((bytes, warnings))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn encode_with(_: &Cjxl, _: &Image, _: &EncodeOptions) -> Result<Encoded, IoError> {
    Err(IoError::Unsupported("cjxl can't run in the browser".into()))
}

/// `cjxl - -`: the PNG on standard input, the JPEG XL file on standard output; and whether `cjxl`
/// ran sandboxed.
#[cfg(not(target_arch = "wasm32"))]
fn piped(tool: &Cjxl, png: Vec<u8>, opts: &EncodeOptions, timeout: std::time::Duration, out_cap: u64) -> Result<(Vec<u8>, bool), RunError> {
    // The job folder stays empty: it is the sandbox's only writable place and the working folder,
    // where versions before 0.12 would have read a file named `-` instead of standard input.
    let job = TempDir::new().map_err(|e| RunError::Failed(e.to_string()))?;
    let mut cmd = external::command("cjxl", &tool.path, &job.path, Profile::Tool, external::sandbox_policy()).map_err(|e| RunError::Spawn(e.to_string()))?;
    cmd.cmd.args(["-", "-"]).args(arguments(opts));
    let bytes = cmd.run("cjxl", Some(png), timeout, out_cap)?;
    if photocraft_codecs::detect(&bytes) != Some(photocraft_codecs::Format::Jxl) {
        return Err(RunError::Failed("cjxl wrote something that isn't a JPEG XL file".into()));
    }
    Ok((bytes, cmd.confined))
}

/// `cjxl in.png out.jxl` in a private temporary folder, for when piping fails.
#[cfg(not(target_arch = "wasm32"))]
fn through_files(tool: &Cjxl, png: Vec<u8>, opts: &EncodeOptions, timeout: std::time::Duration) -> Result<(Vec<u8>, bool), IoError> {
    let dir = TempDir::new()?;
    let input = dir.write("in.png", &png)?;
    drop(png);
    let output = dir.path.join("out.jxl");
    let mut cmd = external::command("cjxl", &tool.path, &dir.path, Profile::Tool, external::sandbox_policy())?;
    cmd.cmd.arg(&input).arg(&output).args(arguments(opts));
    cmd.run("cjxl", None, timeout, 1 << 20)?;
    let bytes = std::fs::read(&output).map_err(|e| IoError::Unsupported(format!("cjxl didn't write the JPEG XL file: {e}")))?;
    if photocraft_codecs::detect(&bytes) != Some(photocraft_codecs::Format::Jxl) {
        return Err(IoError::Unsupported("cjxl wrote something that isn't a JPEG XL file".into()));
    }
    Ok((bytes, cmd.confined))
}

#[cfg(not(target_arch = "wasm32"))]
fn find() -> Option<Cjxl> {
    match external::env_override("PHOTOCRAFT_CJXL") {
        Some(Some(path)) => probe(&path),
        Some(None) => None,
        None => external::candidates("cjxl").into_iter().find_map(|p| probe(&p)),
    }
}

/// `program` as `cjxl`, if `--version` says it is one recent enough.
#[cfg(not(target_arch = "wasm32"))]
fn probe(program: &std::path::Path) -> Option<Cjxl> {
    let version = external::version_of("cjxl", program, &["--version"])?;
    (version >= MIN_VERSION).then(|| Cjxl { path: program.to_path_buf(), version })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_follow_the_options() {
        assert_eq!(arguments(&EncodeOptions::default()), ["-d", "0", "-e", "7"]);
        let o = EncodeOptions { jxl_quality: Some(85), jxl_effort: 3, ..Default::default() };
        assert_eq!(arguments(&o), ["-q", "85", "-e", "3"]);
        let o = EncodeOptions { jxl_quality: Some(0), jxl_effort: 99, ..Default::default() };
        assert_eq!(arguments(&o), ["-q", "1", "-e", "10"], "out-of-range values are clamped");
    }

    fn tool(path: &str, version: (u32, u32, u32)) -> Cjxl {
        Cjxl { path: path.into(), version }
    }

    fn small() -> Image {
        Image::from_u8(4, 4, photocraft_codecs::ChannelLayout::Rgb, (0..48).collect()).unwrap()
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_missing_or_failing_program_is_an_error_not_a_panic() {
        let img = small();
        let r = encode_with(&tool("/nonexistent/cjxl", (0, 12, 0)), &img, &EncodeOptions::default());
        assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("couldn't run cjxl")), "{r:?}");
        assert!(probe(std::path::Path::new("/nonexistent/cjxl")).is_none());
        #[cfg(unix)]
        for version in [(0, 7, 0), (0, 12, 0)] {
            // `false` fails both ways (piped, then with files).
            let r = encode_with(&tool("/usr/bin/false", version), &img, &EncodeOptions::default());
            assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("cjxl failed")), "{r:?}");
            // `true` writes nothing: not a JPEG XL file on standard output, then no output file.
            let r = encode_with(&tool("/usr/bin/true", version), &img, &EncodeOptions::default());
            assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("didn't write")), "{r:?}");
        }
    }

    /// The real `cjxl`: piped, as an older version would run (in an empty folder), and the
    /// retry with files when piping fails. Passes with a note when it isn't installed.
    #[cfg(unix)]
    #[test]
    fn real_cjxl_piped_and_through_files() {
        let Some(found) = cjxl() else {
            eprintln!("cjxl isn't installed: skipping");
            return;
        };
        let img = small();
        let opts = EncodeOptions::default();
        let t = std::time::Duration::from_secs(60);
        let decoded = |bytes: &[u8]| photocraft_codecs::decode(bytes).unwrap();
        for version in [found.version, (0, 7, 0)] {
            let (bytes, confined) = piped(&tool(found.path.to_str().unwrap(), version), external::png(&img, &opts).unwrap(), &opts, t, 1 << 20).unwrap();
            assert_eq!(confined, crate::tool_sandbox().is_some());
            assert_eq!(decoded(&bytes).data(), img.data(), "lossless through pipes ({version:?})");
        }
        let (bytes, confined) = through_files(found, external::png(&img, &opts).unwrap(), &opts, t).unwrap();
        assert_eq!(confined, crate::tool_sandbox().is_some());
        assert_eq!(decoded(&bytes).data(), img.data());

        // A cjxl that can't read standard input: the export still works, through files.
        let dir = TempDir::new().unwrap();
        let script = dir.path.join("cjxl-no-stdin");
        std::fs::write(&script, format!("#!/bin/sh\nif [ \"$1\" = \"-\" ]; then echo 'no stdin' >&2; exit 1; fi\nexec '{}' \"$@\"\n", found.path.display()))
            .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let (bytes, _) = encode_with(&tool(script.to_str().unwrap(), found.version), &img, &opts).unwrap();
        assert_eq!(decoded(&bytes).data(), img.data());
    }
}
