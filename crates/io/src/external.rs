//! Running the user's own image tools (libjxl's `cjxl`, libheif's `heif-enc`/`heif-dec`, macOS
//! `sips`) for formats PhotoCraft has no pure-Rust encoder or decoder for. Shared by
//! [`crate::jxl_tool`] and [`crate::heif_tool`].
//!
//! A tool is found from an environment variable (a path, or `off`), then the `PATH`, then the
//! usual install folders: a macOS app opened from the Finder gets a `PATH` without Homebrew's.
//! Tools run directly, never through a shell, with a deadline that grows with the image; both
//! output pipes are drained while they run, standard output is capped, and a failure passes on the
//! tool's last line of standard error.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use photocraft_codecs::{EncodeOptions, Image};

use crate::IoError;

/// Why running a tool failed.
#[derive(Debug)]
pub(crate) enum RunError {
    /// It couldn't be started.
    Spawn(String),
    /// It ran past its deadline and was stopped.
    Timeout(String),
    /// It failed, or its output was wrong.
    Failed(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RunError::Spawn(m) | RunError::Timeout(m) | RunError::Failed(m) => f.write_str(m),
        }
    }
}

impl From<RunError> for IoError {
    fn from(e: RunError) -> Self {
        IoError::Unsupported(e.to_string())
    }
}

/// What the environment variable `var` says: `None` when unset, `Some(None)` when it turns the
/// tool off (empty, `off` or `0`), `Some(Some(path))` when it names the program.
pub(crate) fn env_override(var: &str) -> Option<Option<PathBuf>> {
    let v = std::env::var_os(var)?;
    let s = v.to_string_lossy();
    Some((!(s.is_empty() || s.eq_ignore_ascii_case("off") || s == "0")).then(|| PathBuf::from(&v)))
}

/// Every existing file called `name` (`name.exe` on Windows) in the `PATH` and the usual install
/// folders, in search order.
pub(crate) fn candidates(name: &str) -> Vec<PathBuf> {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].map(PathBuf::from));
    } else if cfg!(windows) {
        for (var, sub) in [
            ("ProgramFiles", "libjxl\\bin"),
            ("ProgramFiles", "libheif\\bin"),
            ("LOCALAPPDATA", "Microsoft\\WinGet\\Links"),
            ("USERPROFILE", "scoop\\shims"),
            ("SystemDrive", "msys64\\ucrt64\\bin"),
        ] {
            if let Some(base) = std::env::var_os(var) {
                let base = if var == "SystemDrive" { PathBuf::from(format!("{}\\", base.to_string_lossy())) } else { PathBuf::from(base) };
                dirs.push(base.join(sub));
            }
        }
    } else {
        dirs.extend(["/usr/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin"].map(PathBuf::from));
    }
    let mut seen = std::collections::HashSet::new();
    dirs.into_iter().map(|d| d.join(&exe)).filter(|p| p.is_file() && seen.insert(p.clone())).collect()
}

/// The first `x.y[.z]` version number in `text` (after an optional `v`), as `(major, minor, patch)`.
pub(crate) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    text.split(|c: char| c.is_whitespace() || c == ':').find_map(|w| {
        let w = w.strip_prefix('v').unwrap_or(w);
        if !w.starts_with(|c: char| c.is_ascii_digit()) || !w.contains('.') {
            return None;
        }
        let mut parts = w.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(str::parse::<u32>);
        Some((parts.next()?.ok()?, parts.next().and_then(Result::ok).unwrap_or(0), parts.next().and_then(Result::ok).unwrap_or(0)))
    })
}

/// `program --version` (or `args`), parsed; `None` when it can't run or prints no version.
pub(crate) fn version_of(name: &str, program: &Path, args: &[&str]) -> Option<(u32, u32, u32)> {
    let out = run(name, std::process::Command::new(program).args(args), None, Duration::from_secs(10), 1 << 20).ok()?;
    parse_version(&String::from_utf8_lossy(&out))
}

/// About two minutes, plus time for the slowest settings on large images.
pub(crate) fn timeout_for(img: &Image) -> Duration {
    let megapixels = u64::try_from(img.pixel_count() / 1_000_000).unwrap_or(u64::MAX);
    Duration::from_secs(120u64.saturating_add(megapixels.saturating_mul(20)).min(3600))
}

/// The PNG handed to a tool: fast to write, with the profile and metadata the export keeps.
pub(crate) fn png(img: &Image, opts: &EncodeOptions) -> Result<Vec<u8>, IoError> {
    let png_opts = EncodeOptions {
        png_compression: photocraft_codecs::PngCompression::Fast,
        png_interlaced: false,
        embed_icc: opts.embed_icc,
        embed_metadata: opts.embed_metadata,
        ..EncodeOptions::default()
    };
    Ok(photocraft_codecs::encode(img, photocraft_codecs::Format::Png, &png_opts)?)
}

/// Runs `cmd` (the tool `name`, for messages), writing `input` (if any) to its standard input, and
/// returns its standard output (at most `out_cap` bytes: more is an error). Failing, or running
/// past `timeout`, is an error; the program is then killed and reaped.
pub(crate) fn run(name: &str, cmd: &mut std::process::Command, input: Option<Vec<u8>>, timeout: Duration, out_cap: u64) -> Result<Vec<u8>, RunError> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
    let mut child = cmd.stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| RunError::Spawn(format!("couldn't run {name}: {e}")))?;
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
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Timeout(format!("{name} took longer than {} s and was stopped", timeout.as_secs())));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Failed(format!("waiting for {name} failed: {e}")));
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
        return Err(RunError::Failed(format!("{name} wrote more than {out_cap} bytes")));
    }
    if !status.success() {
        let err = String::from_utf8_lossy(&err);
        let last = err.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("");
        return Err(RunError::Failed(format!("{name} failed ({status}){}", if last.is_empty() { String::new() } else { format!(": {last}") })));
    }
    Ok(out)
}

/// A private temporary folder, removed with everything in it when dropped.
pub(crate) struct TempDir {
    pub path: PathBuf,
}

impl TempDir {
    pub(crate) fn new() -> Result<Self, IoError> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
        let mut last = None;
        for _ in 0..8 {
            let name = format!("photocraft-tool-{}-{}-{nanos}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed));
            let path = std::env::temp_dir().join(name);
            // `create_dir` fails if it exists, so the folder is ours alone.
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir { path }),
                Err(e) => last = Some(e),
            }
        }
        Err(IoError::Unsupported(format!("couldn't create a temporary folder: {}", last.map_or_else(String::new, |e| e.to_string()))))
    }

    /// Writes `bytes` to `name` in the folder, returning its path.
    pub(crate) fn write(&self, name: &str, bytes: &[u8]) -> Result<PathBuf, IoError> {
        let path = self.path.join(name);
        std::fs::write(&path, bytes).map_err(|e| IoError::Unsupported(format!("couldn't write a temporary file: {e}")))?;
        Ok(path)
    }
}

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
        assert_eq!(parse_version("1.23.6\nlibheif: 1.23.6"), Some((1, 23, 6)));
        assert_eq!(parse_version("libheif: 1.17.6"), Some((1, 17, 6)));
        for bad in ["", "cjxl", "cjxl version", "vx.y", "\n", "x265 4"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_unset_override_is_none() {
        // Only reads: the variable names are unique to this test and never set.
        assert_eq!(env_override("PHOTOCRAFT_TEST_UNSET_TOOL_VARIABLE"), None);
    }

    #[cfg(unix)]
    #[test]
    fn input_is_piped_and_output_capped() {
        let t = Duration::from_secs(10);
        let echoed = run("cat", &mut std::process::Command::new("/bin/cat"), Some(b"hello".to_vec()), t, 100).unwrap();
        assert_eq!(echoed, b"hello");
        // 1 MB through both pipes: neither side blocks on a full pipe.
        let big: Vec<u8> = (0..1 << 20).map(|i| (i % 251) as u8).collect();
        assert_eq!(run("cat", &mut std::process::Command::new("/bin/cat"), Some(big.clone()), t, 2 << 20).unwrap(), big);
        let r = run("cat", &mut std::process::Command::new("/bin/cat"), Some(big), t, 1000);
        assert!(matches!(&r, Err(RunError::Failed(m)) if m.contains("more than 1000 bytes")), "{r:?}");
        // A program that exits without reading its input is no error in itself.
        assert!(run("true", &mut std::process::Command::new("/usr/bin/true"), Some(vec![0; 1 << 20]), t, 10).unwrap().is_empty());
        let r = run("false", &mut std::process::Command::new("/usr/bin/false"), None, t, 10);
        assert!(matches!(&r, Err(RunError::Failed(m)) if m.starts_with("false failed")), "{r:?}");
        let r = run("nothing", &mut std::process::Command::new("/nonexistent/tool"), None, t, 10);
        assert!(matches!(&r, Err(RunError::Spawn(m)) if m.contains("couldn't run nothing")), "{r:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_hung_program_is_stopped() {
        let t = std::time::Instant::now();
        let r = run("sleep", std::process::Command::new("/bin/sleep").arg("30"), None, Duration::from_millis(200), 10);
        assert!(matches!(&r, Err(RunError::Timeout(m)) if m.contains("was stopped")), "{r:?}");
        assert!(t.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn the_temporary_folder_is_removed() {
        let dir = TempDir::new().unwrap();
        let path = dir.path.clone();
        dir.write("x", b"1").unwrap();
        drop(dir);
        assert!(!path.exists());
    }
}
