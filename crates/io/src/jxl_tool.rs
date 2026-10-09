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
//! read a file named `-` in its working folder instead of standard input when one existed, so
//! older versions run in an empty private folder. If the piped run fails, it is retried once with
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

/// From this version `cjxl` reads standard input for `-` even when a file named `-` exists.
#[cfg(not(target_arch = "wasm32"))]
const STDIN_FIXED: (u32, u32, u32) = (0, 12, 0);

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
    // About two minutes, plus time for the slowest efforts on large images.
    let megapixels = u64::try_from(img.pixel_count() / 1_000_000).unwrap_or(u64::MAX);
    let timeout = std::time::Duration::from_secs(120u64.saturating_add(megapixels.saturating_mul(20)).min(3600));
    // A JPEG XL file is never much larger than the raw pixels; more output is a misbehaving program.
    let out_cap = u64::try_from(img.data().len()).unwrap_or(u64::MAX).saturating_mul(2).saturating_add(1 << 20);
    let bytes = match piped(tool, png(img, opts)?, opts, timeout, out_cap) {
        Ok(bytes) => bytes,
        // A program that can't start, or that ran out of time, would do the same again.
        Err(RunError::Spawn(e) | RunError::Timeout(e)) => return Err(IoError::Unsupported(e)),
        Err(RunError::Failed(_)) => through_files(tool, png(img, opts)?, opts, timeout)?,
    };
    Ok((bytes, warnings))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn encode_with(_: &Cjxl, _: &Image, _: &EncodeOptions) -> Result<Encoded, IoError> {
    Err(IoError::Unsupported("cjxl can't run in the browser".into()))
}

/// The PNG handed to `cjxl`: fast to write, with the profile and metadata the export keeps.
#[cfg(not(target_arch = "wasm32"))]
fn png(img: &Image, opts: &EncodeOptions) -> Result<Vec<u8>, IoError> {
    let png_opts = EncodeOptions {
        png_compression: photocraft_codecs::PngCompression::Fast,
        png_interlaced: false,
        embed_icc: opts.embed_icc,
        embed_metadata: opts.embed_metadata,
        ..EncodeOptions::default()
    };
    Ok(photocraft_codecs::encode(img, photocraft_codecs::Format::Png, &png_opts)?)
}

/// `cjxl - -`: the PNG on standard input, the JPEG XL file on standard output.
#[cfg(not(target_arch = "wasm32"))]
fn piped(tool: &Cjxl, png: Vec<u8>, opts: &EncodeOptions, timeout: std::time::Duration, out_cap: u64) -> Result<Vec<u8>, RunError> {
    let mut cmd = std::process::Command::new(&tool.path);
    cmd.args(["-", "-"]).args(arguments(opts));
    // Older versions would read a file named `-` from their working folder: give them an empty one.
    let empty = if tool.version < STDIN_FIXED { Some(TempDir::new().map_err(|e| RunError::Failed(e.to_string()))?) } else { None };
    if let Some(dir) = &empty {
        cmd.current_dir(&dir.path);
    }
    let bytes = run(&mut cmd, Some(png), timeout, out_cap)?;
    if photocraft_codecs::detect(&bytes) != Some(photocraft_codecs::Format::Jxl) {
        return Err(RunError::Failed("cjxl wrote something that isn't a JPEG XL file".into()));
    }
    Ok(bytes)
}

/// `cjxl in.png out.jxl` in a private temporary folder, for when piping fails.
#[cfg(not(target_arch = "wasm32"))]
fn through_files(tool: &Cjxl, png: Vec<u8>, opts: &EncodeOptions, timeout: std::time::Duration) -> Result<Vec<u8>, IoError> {
    let dir = TempDir::new()?;
    let (input, output) = (dir.path.join("in.png"), dir.path.join("out.jxl"));
    std::fs::write(&input, &png).map_err(|e| IoError::Unsupported(format!("JPEG XL: couldn't write a temporary file: {e}")))?;
    drop(png);
    let mut cmd = std::process::Command::new(&tool.path);
    cmd.arg(&input).arg(&output).args(arguments(opts));
    run(&mut cmd, None, timeout, 1 << 20).map_err(|e| IoError::Unsupported(e.to_string()))?;
    let bytes = std::fs::read(&output).map_err(|e| IoError::Unsupported(format!("cjxl didn't write the JPEG XL file: {e}")))?;
    if photocraft_codecs::detect(&bytes) != Some(photocraft_codecs::Format::Jxl) {
        return Err(IoError::Unsupported("cjxl wrote something that isn't a JPEG XL file".into()));
    }
    Ok(bytes)
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
    let out = run(std::process::Command::new(program).arg("--version"), None, std::time::Duration::from_secs(10), 1 << 20).ok()?;
    let version = parse_version(&String::from_utf8_lossy(&out))?;
    (version >= MIN_VERSION).then(|| Cjxl { path: program.to_path_buf(), version })
}

/// Why running `cjxl` failed.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug)]
enum RunError {
    /// It couldn't be started.
    Spawn(String),
    /// It ran past its deadline and was stopped.
    Timeout(String),
    /// It failed, or its output was wrong.
    Failed(String),
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(m) | RunError::Timeout(m) | RunError::Failed(m) => f.write_str(m),
        }
    }
}

/// Runs `cmd`, writing `input` (if any) to its standard input, and returns its standard output
/// (at most `out_cap` bytes: more is an error). Failing, or running past `timeout`, is an error;
/// the program is then killed and reaped.
#[cfg(not(target_arch = "wasm32"))]
fn run(cmd: &mut std::process::Command, input: Option<Vec<u8>>, timeout: std::time::Duration, out_cap: u64) -> Result<Vec<u8>, RunError> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
    let mut child = cmd.stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| RunError::Spawn(format!("couldn't run cjxl: {e}")))?;
    // The input is written, and both outputs read, on their own threads, so neither side ever
    // blocks on a full pipe. The writer drops the input (freeing it) and closes the pipe (the end
    // of input for the program) as soon as it is written; a program that exits early just ends the
    // write.
    let writer = match (input, child.stdin.take()) {
        (Some(data), Some(mut pipe)) => Some(std::thread::spawn(move || {
            let _ = pipe.write_all(&data);
        })),
        _ => None,
    };
    let drain = |p: Option<Box<dyn Read + Send>>, cap: u64| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(p) = p {
                let _ = p.take(cap.saturating_add(1)).read_to_end(&mut buf);
            }
            buf
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>), out_cap);
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>), 1 << 20);
    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout(format!("cjxl took longer than {} s and was stopped", timeout.as_secs())));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Failed(format!("waiting for cjxl failed: {e}")));
            }
        }
    };
    if let Some(w) = writer {
        let _ = w.join();
    }
    let out = out.join().unwrap_or_default();
    let err = err.join().unwrap_or_default();
    // Checked first: reading stops at the cap, so a program writing more then fails on its pipe.
    if out.len() as u64 > out_cap {
        return Err(RunError::Failed(format!("cjxl wrote more than {out_cap} bytes")));
    }
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let last = err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
        return Err(RunError::Failed(format!("cjxl failed ({status}){}", if last.is_empty() { String::new() } else { format!(": {last}") })));
    }
    Ok(out)
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

    #[cfg(unix)]
    #[test]
    fn input_is_piped_and_output_capped() {
        let t = std::time::Duration::from_secs(10);
        let echoed = run(&mut std::process::Command::new("/bin/cat"), Some(b"hello".to_vec()), t, 100).unwrap();
        assert_eq!(echoed, b"hello");
        // 1 MB through both pipes: neither side blocks on a full pipe.
        let big: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();
        assert_eq!(run(&mut std::process::Command::new("/bin/cat"), Some(big.clone()), t, 2 << 20).unwrap(), big);
        let r = run(&mut std::process::Command::new("/bin/cat"), Some(big), t, 1000);
        assert!(matches!(&r, Err(RunError::Failed(m)) if m.contains("more than 1000 bytes")), "{r:?}");
        // A program that exits without reading its input is no error in itself.
        assert!(run(&mut std::process::Command::new("/usr/bin/true"), Some(vec![0; 1 << 20]), t, 10).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_program_is_stopped() {
        let t = std::time::Instant::now();
        let r = run(std::process::Command::new("/bin/sleep").arg("30"), None, std::time::Duration::from_millis(200), 10);
        assert!(matches!(&r, Err(RunError::Timeout(m)) if m.contains("was stopped")), "{r:?}");
        assert!(t.elapsed() < std::time::Duration::from_secs(10));
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
            let bytes = piped(&tool(found.path.to_str().unwrap(), version), png(&img, &opts).unwrap(), &opts, t, 1 << 20).unwrap();
            assert_eq!(decoded(&bytes).data(), img.data(), "lossless through pipes ({version:?})");
        }
        let bytes = through_files(found, png(&img, &opts).unwrap(), &opts, t).unwrap();
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
