//! Running the user's own image tools (libjxl's `cjxl`, libheif's `heif-enc`/`heif-dec`, macOS
//! `sips`) for formats PhotoCraft has no pure-Rust encoder or decoder for. Shared by
//! [`crate::jxl_tool`] and [`crate::heif_tool`].
//!
//! A tool is found from an environment variable (a path, or `off`), then the `PATH`, then the
//! usual install folders: a macOS app opened from the Finder gets a `PATH` without Homebrew's.
//! Tools run directly, never through a shell, with a deadline that grows with the image; both
//! output pipes are drained while they run, standard output is capped, and a failure passes on the
//! tool's last line of standard error.
//!
//! **Sandbox.** Every tool runs with a minimal environment, in its own private job folder (which
//! holds its input and output), and on macOS confined by the system sandbox (`sandbox-exec`): it
//! can read its own install folder and the system libraries, read and write the job folder, and
//! nothing else: not the user's files, not the network. A malicious file that exploits a C/C++
//! decoder (libde265, dav1d, ImageIO) can then reach neither. `sips` needs macOS's image services
//! and writes through the user's temporary folder, so its profile also allows those. The policy
//! comes from `PHOTOCRAFT_TOOL_SANDBOX`: unset confines tools where the system can (Windows runs
//! them unconfined for now), `require` refuses to run a tool unconfined, `off` never confines.
//! On Linux the same rules come from Landlock and seccomp, or `bwrap` (see [`linux`]).

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::time::Duration;

use photocraft_codecs::{EncodeOptions, Image};

use crate::IoError;

#[cfg(target_os = "linux")]
mod linux;

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

/// The note an import or export carries about the helper that did the work: "HEIC opened with
/// heif-dec (sandboxed)", "JPEG XL written with cjxl" (no suffix: it ran unconfined).
pub(crate) fn tool_note(format: &str, done: &str, tool: &str, confined: bool) -> String {
    format!("{format} {done} with {tool}{}", if confined { " (sandboxed)" } else { "" })
}

/// Whether helper tools are confined (`PHOTOCRAFT_TOOL_SANDBOX`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxPolicy {
    /// Never confine.
    Off,
    /// Confine where the system can; elsewhere run unconfined (the default).
    Auto,
    /// Refuse to run a tool that can't be confined.
    Require,
}

/// The session's policy, from `PHOTOCRAFT_TOOL_SANDBOX` (`off`, `require`, anything else automatic).
pub fn sandbox_policy() -> SandboxPolicy {
    static POLICY: std::sync::OnceLock<SandboxPolicy> = std::sync::OnceLock::new();
    *POLICY.get_or_init(|| match std::env::var("PHOTOCRAFT_TOOL_SANDBOX").map(|v| v.to_ascii_lowercase()) {
        Ok(v) if v == "off" || v == "0" => SandboxPolicy::Off,
        Ok(v) if v == "require" => SandboxPolicy::Require,
        _ => SandboxPolicy::Auto,
    })
}

/// What a confined tool may read beyond its job folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    /// Its own install folder, the package managers' folders and the system libraries: libheif's
    /// and libjxl's tools.
    Tool,
    /// Also macOS's image services and the user's temporary folder, which ImageIO writes
    /// through: `sips`.
    SystemImaging,
}

/// The macOS sandbox profile for `profile`. Paths come in as parameters (`-D`), never spliced
/// into the text, so no file name can change the rules.
#[cfg(target_os = "macos")]
fn sandbox_profile(profile: Profile) -> String {
    let mut p = String::from(
        r#"(version 1)
(deny default)
(import "system.sb")
(allow file-read* (literal (param "TOOL")) (subpath (param "TOOL_DIR")) (subpath (param "TOOL_LIB")) (subpath "/opt/homebrew") (subpath "/usr/local") (subpath "/opt/local")
  (subpath "/bin") (subpath "/usr/bin") (subpath "/usr/lib") (subpath "/System/Library") (subpath "/private/var/db/dyld")
  (literal "/dev/urandom") (literal "/dev/null"))
; A wrapper script may start its shell and the real tool: whatever starts here inherits these rules.
(allow process-exec (literal (param "TOOL")) (subpath (param "TOOL_DIR")) (subpath "/opt/homebrew") (subpath "/usr/local")
  (subpath "/opt/local") (subpath "/bin") (subpath "/usr/bin"))
(allow process-fork)
(allow file-read* file-write* (subpath (param "JOB")))
(allow sysctl-read)
"#,
    );
    if profile == Profile::SystemImaging {
        p.push_str(
            r#"(allow file-read* (subpath "/System") (subpath "/usr/share") (subpath "/Library/Apple") (subpath "/private/var/db"))
(allow file-read-metadata)
(allow file-read* file-write* (subpath (param "USER_TEMP")))
(allow mach-lookup)
(allow ipc-posix-shm-read* ipc-posix-shm-write-data ipc-posix-shm-write-create)
(allow iokit-open)
(allow user-preference-read)
"#,
        );
    }
    p
}

/// Whether this system can confine tools: macOS with a working `sandbox-exec` (it fails, for one,
/// when PhotoCraft itself runs in a sandbox); Linux with Landlock or a working `bwrap`. Checked once.
pub fn sandbox_available() -> bool {
    sandbox_mechanism().is_some()
}

/// How tools are confined here: `"sandbox-exec"`, `"Landlock"`, `"bwrap"`, or `None`.
pub fn sandbox_mechanism() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        linux::mechanism().map(|m| match m {
            linux::Mechanism::Landlock => "Landlock",
            linux::Mechanism::Bubblewrap => "bwrap",
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        mac_sandbox_works().then_some("sandbox-exec")
    }
}

#[cfg(not(target_os = "linux"))]
fn mac_sandbox_works() -> bool {
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        cfg!(target_os = "macos")
            && run(
                "sandbox-exec",
                std::process::Command::new(SANDBOX_EXEC).args(["-p", "(version 1)(allow default)", "/usr/bin/true"]),
                None,
                Duration::from_secs(10),
                1 << 10,
            )
            .is_ok()
    })
}

#[cfg(not(target_os = "linux"))]
const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The folders a confined tool may read besides the package managers' and the system's: its own
/// folder, and for one in a `bin` folder the `lib` next to it (`…/libjxl/bin/cjxl` →
/// `…/libjxl/lib`). Never the folder above `bin` as a whole: for `/bin/cat` that is the whole
/// disk, for `~/bin/cjxl` the home folder.
#[cfg(target_os = "macos")]
fn tool_dirs(program: &Path) -> (PathBuf, PathBuf) {
    let dir = program.parent().unwrap_or(program).to_path_buf();
    let lib = match (dir.file_name(), dir.parent()) {
        (Some(name), Some(up)) if name == "bin" => up.join("lib"),
        _ => dir.clone(),
    };
    (dir, lib)
}

/// A command running `program` in the private folder `job` with a minimal environment, confined
/// to `job` by `profile` when `policy` and the system allow; and whether it is confined. The
/// caller adds the tool's arguments.
pub(crate) fn command(name: &str, program: &Path, job: &Path, profile: Profile, policy: SandboxPolicy) -> Result<ToolCommand, IoError> {
    // Resolved, so the rules name the real file (Homebrew's `bin` holds symbolic links). Checked
    // here: inside the sandbox a missing program would only be `sandbox-exec`'s failure.
    let program = std::fs::canonicalize(program).map_err(|e| IoError::Unsupported(format!("couldn't run {name}: {e}")))?;
    let job = std::fs::canonicalize(job).map_err(|e| IoError::Unsupported(format!("the tool's folder is missing: {e}")))?;
    let confine = match policy {
        SandboxPolicy::Off => false,
        SandboxPolicy::Auto => sandbox_available(),
        SandboxPolicy::Require if sandbox_available() => true,
        SandboxPolicy::Require => {
            return Err(IoError::Unsupported("helper tools must run in a sandbox (PHOTOCRAFT_TOOL_SANDBOX=require), which this system doesn't provide".into()));
        }
    };
    let mut tool = if confine { sandboxed(&program, &job, profile)? } else { ToolCommand::plain(&program) };
    let cmd = &mut tool.cmd;
    cmd.env_clear().env("PATH", "/usr/bin:/bin").env("TMPDIR", &job).current_dir(&job);
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        // Windows programs need these to load their libraries.
        cmd.env("SystemRoot", root).env("PATH", std::env::var_os("PATH").unwrap_or_default());
    }
    Ok(tool)
}

/// A tool ready to run: its command (the caller adds the tool's arguments), whether it is
/// confined, and on Linux the Landlock rules it is started under.
pub(crate) struct ToolCommand {
    pub cmd: std::process::Command,
    pub confined: bool,
    #[cfg(target_os = "linux")]
    landlock: Option<linux::Rules>,
}

impl ToolCommand {
    fn plain(program: &Path) -> Self {
        ToolCommand {
            cmd: std::process::Command::new(program),
            confined: false,
            #[cfg(target_os = "linux")]
            landlock: None,
        }
    }

    /// [`run`] with this tool's confinement.
    pub(crate) fn run(&mut self, name: &str, input: Option<Vec<u8>>, timeout: Duration, out_cap: u64) -> Result<Vec<u8>, RunError> {
        #[cfg(target_os = "linux")]
        if let Some(rules) = self.landlock.clone() {
            return run_with(name, &mut self.cmd, input, timeout, out_cap, |c| linux::spawn_confined(c, &rules));
        }
        run(name, &mut self.cmd, input, timeout, out_cap)
    }
}

#[cfg(target_os = "macos")]
fn sandboxed(program: &Path, job: &Path, profile: Profile) -> Result<ToolCommand, IoError> {
    let param = |k: &str, v: &Path| format!("{k}={}", v.display());
    let mut cmd = std::process::Command::new(SANDBOX_EXEC);
    cmd.arg("-p").arg(sandbox_profile(profile));
    let (tool_dir, tool_lib) = tool_dirs(program);
    cmd.arg("-D").arg(param("TOOL", program)).arg("-D").arg(param("TOOL_DIR", &tool_dir)).arg("-D").arg(param("TOOL_LIB", &tool_lib));
    cmd.arg("-D").arg(param("JOB", job));
    if profile == Profile::SystemImaging {
        // The per-user temporary folder (`/private/var/folders/…/T`), where ImageIO saves through.
        let temp = std::fs::canonicalize(std::env::temp_dir()).map_err(|e| IoError::Unsupported(format!("no temporary folder: {e}")))?;
        cmd.arg("-D").arg(param("USER_TEMP", &temp));
    }
    cmd.arg(program);
    Ok(ToolCommand { cmd, confined: true })
}

/// Linux: `bwrap` around the program, or the program as is with Landlock rules applied when it
/// starts (`ToolCommand::run`). `sips` doesn't exist here, so both profiles get the same rules.
#[cfg(target_os = "linux")]
fn sandboxed(program: &Path, job: &Path, _: Profile) -> Result<ToolCommand, IoError> {
    let rules = linux::Rules::new(program, job);
    match linux::mechanism() {
        Some(linux::Mechanism::Bubblewrap) => {
            let cmd = linux::bwrap_command(program, &rules).ok_or_else(|| IoError::Unsupported("bwrap disappeared".into()))?;
            Ok(ToolCommand { cmd, confined: true, landlock: None })
        }
        _ => Ok(ToolCommand { cmd: std::process::Command::new(program), confined: true, landlock: Some(rules) }),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sandboxed(program: &Path, _: &Path, _: Profile) -> Result<ToolCommand, IoError> {
    Ok(ToolCommand::plain(program))
}

/// Runs `cmd` (the tool `name`, for messages), writing `input` (if any) to its standard input, and
/// returns its standard output (at most `out_cap` bytes: more is an error). Failing, or running
/// past `timeout`, is an error; the program is then killed and reaped.
pub(crate) fn run(name: &str, cmd: &mut std::process::Command, input: Option<Vec<u8>>, timeout: Duration, out_cap: u64) -> Result<Vec<u8>, RunError> {
    run_with(name, cmd, input, timeout, out_cap, std::process::Command::spawn)
}

/// [`run`], starting the process with `spawn`.
pub(crate) fn run_with(
    name: &str,
    cmd: &mut std::process::Command,
    input: Option<Vec<u8>>,
    timeout: Duration,
    out_cap: u64,
    spawn: impl FnOnce(&mut std::process::Command) -> std::io::Result<std::process::Child>,
) -> Result<Vec<u8>, RunError> {
    use std::io::{Read, Write};
    use std::process::Stdio;
    let stdin = if input.is_some() { Stdio::piped() } else { Stdio::null() };
    let mut child = spawn(cmd.stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped())).map_err(|e| RunError::Spawn(format!("couldn't run {name}: {e}")))?;
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
            // `create_dir` fails if it exists, so the folder is ours alone. The path is resolved
            // (macOS reaches it through `/var` → `/private/var`), so it matches sandbox rules.
            match std::fs::create_dir(&path) {
                Ok(()) => return Ok(TempDir { path: std::fs::canonicalize(&path).unwrap_or(path) }),
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

    /// Runs `program args` for a job in `job` under `policy`; `Ok(stdout)` when it succeeds.
    #[cfg(unix)]
    fn confined(program: &str, args: &[&std::ffi::OsStr], job: &TempDir, policy: SandboxPolicy) -> Result<Vec<u8>, RunError> {
        let mut tool = command("test", Path::new(program), &job.path, Profile::Tool, policy).map_err(|e| RunError::Spawn(e.to_string()))?;
        tool.cmd.args(args);
        tool.run("test", None, Duration::from_secs(20), 1 << 20)
    }

    /// The sandbox is what stops each of these: with it off, the same command succeeds.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_sandboxed_tool_reaches_only_its_job_folder() {
        assert!(sandbox_available(), "macOS provides sandbox-exec");
        let (job, elsewhere) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let inside = job.write("in.txt", b"job data").unwrap();
        let outside = elsewhere.write("private.txt", b"someone's file").unwrap();
        for policy in [SandboxPolicy::Auto, SandboxPolicy::Require] {
            assert_eq!(confined("/bin/cat", &[inside.as_os_str()], &job, policy).unwrap(), b"job data");
            assert!(confined("/bin/cat", &[outside.as_os_str()], &job, policy).is_err(), "reading outside the job folder");
            let made = job.path.join("made");
            assert!(confined("/usr/bin/touch", &[made.as_os_str()], &job, policy).is_ok() && made.exists(), "writing inside it");
            let planted = elsewhere.path.join("planted");
            assert!(confined("/usr/bin/touch", &[planted.as_os_str()], &job, policy).is_err() && !planted.exists(), "writing outside");
            let net = ["-sS", "-m", "5", "-o", "/dev/null", "https://example.com"].map(std::ffi::OsStr::new);
            assert!(confined("/usr/bin/curl", &net, &job, policy).is_err(), "the network");
        }
        // Without the sandbox the same reads and writes succeed: the checks above test the sandbox.
        assert_eq!(confined("/bin/cat", &[outside.as_os_str()], &job, SandboxPolicy::Off).unwrap(), b"someone's file");
        let planted = elsewhere.path.join("planted");
        assert!(confined("/usr/bin/touch", &[planted.as_os_str()], &job, SandboxPolicy::Off).is_ok() && planted.exists());
    }

    /// `sips`'s wider profile adds the user's temporary folder and system services, not the rest of
    /// the disk: a file in the build folder stays out of reach.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_system_imaging_profile_still_keeps_other_files_out() {
        let job = TempDir::new().unwrap();
        let probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target").join(format!("sandbox-probe-{}", std::process::id()));
        std::fs::write(&probe, b"outside").unwrap();
        let mut tool = command("test", Path::new("/bin/cat"), &job.path, Profile::SystemImaging, SandboxPolicy::Auto).unwrap();
        tool.cmd.arg(&probe);
        let r = tool.run("test", None, Duration::from_secs(20), 1 << 20);
        let _ = std::fs::remove_file(&probe);
        assert!(tool.confined && r.is_err(), "{r:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_tool_gets_a_minimal_environment_in_its_job_folder() {
        let job = TempDir::new().unwrap();
        for policy in [SandboxPolicy::Off, SandboxPolicy::Auto] {
            let env = String::from_utf8(confined("/usr/bin/env", &[], &job, policy).unwrap()).unwrap();
            let mut names: Vec<&str> = env.lines().filter_map(|l| l.split('=').next()).collect();
            names.sort_unstable();
            // `sandbox-exec` may add nothing; the shell's own variables are gone either way.
            assert!(names.iter().all(|n| ["PATH", "TMPDIR", "PWD", "SHLVL", "_"].contains(n)), "{env}");
            assert!(env.contains(&format!("TMPDIR={}", job.path.display())), "{env}");
            let pwd = String::from_utf8(confined("/bin/pwd", &[std::ffi::OsStr::new("-P")], &job, policy).unwrap()).unwrap();
            assert_eq!(pwd.trim(), job.path.to_str().unwrap());
        }
        let missing = confined("/nonexistent/tool", &[], &job, SandboxPolicy::Auto);
        assert!(matches!(&missing, Err(RunError::Spawn(m)) if m.contains("couldn't run test")), "{missing:?}");
    }

    /// `require` runs the tool confined where the system has a sandbox, and refuses where it has
    /// none: never unconfined.
    #[cfg(unix)]
    #[test]
    fn requiring_a_sandbox_confines_or_refuses() {
        let job = TempDir::new().unwrap();
        let r = command("test", Path::new("/bin/cat"), &job.path, Profile::Tool, SandboxPolicy::Require);
        if sandbox_available() {
            assert!(r.is_ok_and(|t| t.confined), "{:?}", sandbox_mechanism());
        } else {
            assert!(matches!(&r, Err(IoError::Unsupported(m)) if m.contains("sandbox")), "{:?}", r.err());
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_tool_may_read_its_folder_and_lib_never_what_is_above() {
        let dirs = |p: &str| {
            let (d, l) = tool_dirs(Path::new(p));
            (d.to_string_lossy().into_owned(), l.to_string_lossy().into_owned())
        };
        assert_eq!(dirs("/opt/libjxl/bin/cjxl"), ("/opt/libjxl/bin".into(), "/opt/libjxl/lib".into()));
        assert_eq!(dirs("/bin/cat"), ("/bin".into(), "/lib".into()), "not the whole disk");
        assert_eq!(dirs("/Users/x/bin/cjxl"), ("/Users/x/bin".into(), "/Users/x/lib".into()), "not the home folder");
        assert_eq!(dirs("/tools/cjxl"), ("/tools".into(), "/tools".into()));
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
