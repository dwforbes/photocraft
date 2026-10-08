//! JPEG XL export through libjxl's `cjxl`, when it is installed.
//!
//! PhotoCraft reads JPEG XL itself (jxl-oxide) and has a pure-Rust lossless encoder
//! (zune-jpegxl), but lossy JPEG XL, and keeping an ICC profile in the file, need libjxl. Rather
//! than linking C++, a JPEG XL export hands the image to the user's own `cjxl` as a PNG (which
//! carries the ICC profile, EXIF and XMP) and reads back the file it writes. libjxl keeps
//! improving without a PhotoCraft release, and nothing changes when it is missing: the built-in
//! encoder writes a lossless file instead, and the export says so.
//!
//! Where it is looked for: `PHOTOCRAFT_CJXL` (a path to the program, or `off` to never use it),
//! then the `PATH`, then the usual install folders. A macOS app opened from the Finder gets a
//! `PATH` without Homebrew's folders, so those are searched explicitly. The program found is
//! remembered for the session (restart PhotoCraft after installing libjxl).
//!
//! The program runs directly, never through a shell, with absolute temporary paths as arguments.
//! It is stopped if it runs past a deadline that grows with the image, and its own error message
//! is passed on when it fails.

use photocraft_codecs::{EncodeOptions, Image};

use crate::IoError;

/// libjxl's `cjxl` as found on this system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cjxl {
    pub path: std::path::PathBuf,
    /// `(major, minor, patch)` from `cjxl --version`.
    pub version: (u32, u32, u32),
}

/// The oldest libjxl whose `cjxl` takes the flags used here (`-d`, `-q`, `-e`, PNG input).
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

/// The version in `cjxl --version`'s first line ("cjxl v0.12.0 …").
pub fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let line = text.lines().next()?;
    let v = line.split_whitespace().find_map(|w| w.strip_prefix('v').filter(|v| v.starts_with(|c: char| c.is_ascii_digit())))?;
    let mut parts = v.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(str::parse::<u32>);
    Some((parts.next()?.ok()?, parts.next().and_then(Result::ok).unwrap_or(0), parts.next().and_then(Result::ok).unwrap_or(0)))
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
        Some(tool) => encode_with(&tool.path, img, opts).map(Some),
        None => Ok(None),
    }
}

/// [`encode`] with the given program.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn encode_with(program: &std::path::Path, img: &Image, opts: &EncodeOptions) -> Result<Encoded, IoError> {
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
    let png_opts = EncodeOptions {
        png_compression: photocraft_codecs::PngCompression::Fast,
        png_interlaced: false,
        embed_icc: opts.embed_icc,
        embed_metadata: opts.embed_metadata,
        ..EncodeOptions::default()
    };
    let png = photocraft_codecs::encode(img, photocraft_codecs::Format::Png, &png_opts)?;
    let dir = TempDir::new()?;
    let (input, output) = (dir.path.join("in.png"), dir.path.join("out.jxl"));
    std::fs::write(&input, &png).map_err(|e| IoError::Unsupported(format!("JPEG XL: couldn't write a temporary file: {e}")))?;
    let mut cmd = std::process::Command::new(program);
    cmd.arg(&input).arg(&output).args(arguments(opts));
    // About two minutes, plus time for the slowest efforts on large images.
    let megapixels = u64::try_from(img.pixel_count() / 1_000_000).unwrap_or(u64::MAX);
    let timeout = std::time::Duration::from_secs(120u64.saturating_add(megapixels.saturating_mul(20)).min(3600));
    run(&mut cmd, timeout)?;
    let bytes = std::fs::read(&output).map_err(|e| IoError::Unsupported(format!("cjxl didn't write the JPEG XL file: {e}")))?;
    if photocraft_codecs::detect(&bytes) != Some(photocraft_codecs::Format::Jxl) {
        return Err(IoError::Unsupported("cjxl wrote something that isn't a JPEG XL file".into()));
    }
    Ok((bytes, warnings))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn encode_with(_: &std::path::Path, _: &Image, _: &EncodeOptions) -> Result<Encoded, IoError> {
    Err(IoError::Unsupported("cjxl can't run in the browser".into()))
}

#[cfg(not(target_arch = "wasm32"))]
fn find() -> Option<Cjxl> {
    use std::path::PathBuf;
    let exe = if cfg!(windows) { "cjxl.exe" } else { "cjxl" };
    if let Some(v) = std::env::var_os("PHOTOCRAFT_CJXL") {
        let s = v.to_string_lossy();
        if s.is_empty() || s.eq_ignore_ascii_case("off") || s == "0" {
            return None;
        }
        return probe(&PathBuf::from(v));
    }
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].map(PathBuf::from));
    } else if cfg!(windows) {
        for (var, sub) in [("ProgramFiles", "libjxl\\bin"), ("LOCALAPPDATA", "Microsoft\\WinGet\\Links"), ("USERPROFILE", "scoop\\shims")] {
            if let Some(base) = std::env::var_os(var) {
                dirs.push(PathBuf::from(base).join(sub));
            }
        }
    } else {
        dirs.extend(["/usr/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin"].map(PathBuf::from));
    }
    dirs.into_iter().map(|d| d.join(exe)).filter(|p| p.is_file()).find_map(|p| probe(&p))
}

/// `program` as `cjxl`, if `--version` says it is one recent enough.
#[cfg(not(target_arch = "wasm32"))]
fn probe(program: &std::path::Path) -> Option<Cjxl> {
    let out = run(std::process::Command::new(program).arg("--version"), std::time::Duration::from_secs(10)).ok()?;
    let version = parse_version(&out)?;
    (version >= MIN_VERSION).then(|| Cjxl { path: program.to_path_buf(), version })
}

/// Runs `cmd` and returns its standard output; failing, or running past `timeout`, is an error
/// (the program is then killed and reaped).
#[cfg(not(target_arch = "wasm32"))]
fn run(cmd: &mut std::process::Command, timeout: std::time::Duration) -> Result<String, IoError> {
    use std::io::Read;
    use std::process::Stdio;
    let fail = |m: String| IoError::Unsupported(m);
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| fail(format!("couldn't run cjxl: {e}")))?;
    // Both pipes are drained while waiting, so a chatty program never blocks on a full pipe; at
    // most 1 MB of each is kept.
    let drain = |p: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = p {
                let _ = p.take(1 << 20).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!("cjxl took longer than {} s and was stopped", timeout.as_secs())));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(format!("waiting for cjxl failed: {e}")));
            }
        }
    };
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let last = err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
        return Err(fail(format!("cjxl failed ({status}){}", if last.is_empty() { String::new() } else { format!(": {last}") })));
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// A private temporary folder, removed with everything in it when dropped.
#[cfg(not(target_arch = "wasm32"))]
struct TempDir {
    path: std::path::PathBuf,
}

#[cfg(not(target_arch = "wasm32"))]
impl TempDir {
    fn new() -> Result<Self, IoError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let mut last = None;
        for _ in 0..8 {
            let name = format!("photocraft-jxl-{}-{}-{nanos}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
            let path = std::env::temp_dir().join(name);
            // `create_dir` fails if it exists, so the folder is ours alone.
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir { path }),
                Err(e) => last = Some(e),
            }
        }
        Err(IoError::Unsupported(format!("JPEG XL: couldn't create a temporary folder: {}", last.map_or_else(String::new, |e| e.to_string()))))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse() {
        assert_eq!(parse_version("cjxl v0.12.0 0.12.0 [_NEON_BF16_,NEON] {AppleClang 21}\nCopyright"), Some((0, 12, 0)));
        assert_eq!(parse_version("cjxl v0.8.2 [AVX2]"), Some((0, 8, 2)));
        assert_eq!(parse_version("cjxl v0.11"), Some((0, 11, 0)));
        assert_eq!(parse_version("JPEG XL encoder v0.7.0 1234abc"), Some((0, 7, 0)));
        for bad in ["", "cjxl", "cjxl version", "vx.y", "\n"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert!((0, 6, 1) < MIN_VERSION && (0, 7, 0) >= MIN_VERSION);
    }

    #[test]
    fn arguments_follow_the_options() {
        assert_eq!(arguments(&EncodeOptions::default()), ["-d", "0", "-e", "7"]);
        let o = EncodeOptions { jxl_quality: Some(85), jxl_effort: 3, ..Default::default() };
        assert_eq!(arguments(&o), ["-q", "85", "-e", "3"]);
        let o = EncodeOptions { jxl_quality: Some(0), jxl_effort: 99, ..Default::default() };
        assert_eq!(arguments(&o), ["-q", "1", "-e", "10"], "out-of-range values are clamped");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_missing_or_failing_program_is_an_error_not_a_panic() {
        let img = Image::from_u8(4, 4, photocraft_codecs::ChannelLayout::Rgb, vec![9; 48]).unwrap();
        let r = encode_with(std::path::Path::new("/nonexistent/cjxl"), &img, &EncodeOptions::default());
        assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("couldn't run cjxl")), "{r:?}");
        assert!(probe(std::path::Path::new("/nonexistent/cjxl")).is_none());
        #[cfg(unix)]
        {
            // `false` exits with an error and writes nothing.
            let r = encode_with(std::path::Path::new("/usr/bin/false"), &img, &EncodeOptions::default());
            assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("cjxl failed")), "{r:?}");
            // `true` succeeds without writing the output file.
            let r = encode_with(std::path::Path::new("/usr/bin/true"), &img, &EncodeOptions::default());
            assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("didn't write")), "{r:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_program_is_stopped() {
        let t = std::time::Instant::now();
        let r = run(std::process::Command::new("/bin/sleep").arg("30"), std::time::Duration::from_millis(200));
        assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("was stopped")), "{r:?}");
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_temporary_folder_is_removed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path.clone();
        std::fs::write(path.join("x"), b"1").unwrap();
        drop(dir);
        assert!(!path.exists());
    }
}
