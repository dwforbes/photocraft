//! Linux confinement of helper tools: Landlock (file access) and seccomp (network), or `bwrap`
//! (bubblewrap) on kernels without Landlock.
//!
//! Landlock and seccomp restrict the calling *thread*, and a process started from it inherits the
//! restrictions through `exec`. So a tool is started from a short-lived thread that restricts
//! itself first and hands the child back: the rest of PhotoCraft is untouched, nothing is
//! relaunched, and no `unsafe` code is needed (the `landlock` and `seccompiler` crates are safe
//! APIs over the system calls).
//!
//! What a confined tool may do: read and run the system's programs and libraries (`/usr`, `/lib*`,
//! `/bin`, `/sbin`, the dynamic loader's configuration), its own folder and the `lib` next to it,
//! Homebrew on Linux; read and write its private job folder. Not readable: the home folder, other
//! users' files, `/proc` (other processes' command lines and environments) and the rest of `/etc`.
//! Network: seccomp refuses `socket()` for anything but local (Unix) sockets and refuses io_uring
//! (which can open sockets without `socket()`); Landlock also denies TCP where the kernel supports
//! it (ABI 4, Linux 6.7). `bwrap` gives the same file view through mount namespaces and a separate,
//! empty network namespace.

use std::path::{Path, PathBuf};

use landlock::{ABI, Access, AccessFs, AccessNet, Ruleset, RulesetAttr, RulesetCreatedAttr, RulesetStatus, path_beneath_rules};

/// The newest Landlock ABI asked for; older kernels get what they support (best effort), and a
/// kernel with none is detected by [`landlock_available`].
const ABI_WANTED: ABI = ABI::V6;

/// What a confined tool may read, and the one folder it may write.
#[derive(Debug, Clone)]
pub(crate) struct Rules {
    read: Vec<PathBuf>,
    read_write: Vec<PathBuf>,
}

/// The system folders every tool needs, read-only.
const SYSTEM_READ: &[&str] = &[
    "/usr",
    "/lib",
    "/lib64",
    "/lib32",
    "/libx32",
    "/bin",
    "/sbin",
    "/etc/ld.so.cache",
    "/etc/ld.so.conf",
    "/etc/ld.so.conf.d",
    "/etc/localtime",
    "/sys/devices/system/cpu",
    "/dev/urandom",
    "/dev/zero",
    "/home/linuxbrew/.linuxbrew",
];

impl Rules {
    /// The rules for `program` working in `job`.
    pub(crate) fn new(program: &Path, job: &Path) -> Self {
        let mut read: Vec<PathBuf> = SYSTEM_READ.iter().map(PathBuf::from).collect();
        let (dir, lib) = tool_dirs(program);
        read.extend([program.to_path_buf(), dir, lib]);
        Rules { read, read_write: vec![job.to_path_buf(), PathBuf::from("/dev/null")] }
    }

    /// The folders that exist, for `bwrap` (which fails on a missing source).
    fn existing(paths: &[PathBuf]) -> impl Iterator<Item = &PathBuf> {
        paths.iter().filter(|p| p.exists())
    }
}

/// A tool's own folder, and for one in a `bin` folder the `lib` next to it; never the folder above
/// `bin` as a whole (for `/bin/cat` that is `/`, for `~/bin/cjxl` the home folder).
pub(crate) fn tool_dirs(program: &Path) -> (PathBuf, PathBuf) {
    let dir = program.parent().unwrap_or(program).to_path_buf();
    let lib = match (dir.file_name(), dir.parent()) {
        (Some(name), Some(up)) if name == "bin" => up.join("lib"),
        _ => dir.clone(),
    };
    (dir, lib)
}

/// How tools are confined on this system.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mechanism {
    Landlock,
    Bubblewrap,
}

/// The mechanism available here, checked once: Landlock when the kernel enforces it, else a
/// working `bwrap` (user namespaces may be disabled), else none.
pub(crate) fn mechanism() -> Option<Mechanism> {
    static FOUND: std::sync::OnceLock<Option<Mechanism>> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        if landlock_available() {
            Some(Mechanism::Landlock)
        } else if bwrap_works() {
            Some(Mechanism::Bubblewrap)
        } else {
            None
        }
    })
}

/// Whether the kernel enforces Landlock: a throwaway thread restricts itself (only itself, and it
/// ends straight away) and reports the outcome.
pub(crate) fn landlock_available() -> bool {
    std::thread::spawn(|| {
        Ruleset::default()
            .handle_access(AccessFs::from_all(ABI::V1))
            .and_then(|r| r.create())
            .and_then(|r| r.restrict_self())
            .is_ok_and(|status| status.ruleset != RulesetStatus::NotEnforced)
    })
    .join()
    .unwrap_or(false)
}

const BWRAP: &[&str] = &["/usr/bin/bwrap", "/bin/bwrap"];

fn bwrap_path() -> Option<&'static str> {
    BWRAP.iter().copied().find(|p| Path::new(p).is_file())
}

/// Whether `bwrap` can create its namespaces here (it can't where unprivileged user namespaces are
/// turned off and it isn't installed setuid).
fn bwrap_works() -> bool {
    bwrap_path().is_some_and(|bwrap| {
        std::process::Command::new(bwrap)
            .args(["--ro-bind", "/", "/", "--unshare-all", "--die-with-parent", "/bin/true"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    })
}

/// `program` wrapped in `bwrap`: only the system folders, the tool's folders (read-only) and the
/// job folder (read-write) are visible, in new namespaces with no network.
pub(crate) fn bwrap_command(program: &Path, rules: &Rules) -> Option<std::process::Command> {
    let mut cmd = std::process::Command::new(bwrap_path()?);
    cmd.args(["--die-with-parent", "--new-session", "--unshare-all", "--cap-drop", "ALL", "--dev", "/dev", "--proc", "/proc"]);
    for p in Rules::existing(&rules.read) {
        cmd.arg("--ro-bind").arg(p).arg(p);
    }
    for p in Rules::existing(&rules.read_write) {
        if p != Path::new("/dev/null") {
            cmd.arg("--bind").arg(p).arg(p);
        }
    }
    cmd.arg("--").arg(program);
    Some(cmd)
}

/// Starts `cmd` from a thread that has restricted itself to `rules` (Landlock) and to local
/// sockets only (seccomp); the child inherits both. Fails rather than start the tool unconfined.
pub(crate) fn spawn_confined(cmd: &mut std::process::Command, rules: &Rules) -> std::io::Result<std::process::Child> {
    std::thread::scope(|s| {
        s.spawn(|| {
            restrict_this_thread(rules)?;
            cmd.spawn()
        })
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("the sandbox thread failed")))
    })
}

fn restrict_this_thread(rules: &Rules) -> std::io::Result<()> {
    let fail = |what: &str, e: &dyn std::fmt::Display| std::io::Error::other(format!("couldn't set up the sandbox ({what}): {e}"));
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(ABI_WANTED))
        .and_then(|r| r.handle_access(AccessNet::from_all(ABI_WANTED)))
        .and_then(|r| r.create())
        .and_then(|r| r.add_rules(path_beneath_rules(&rules.read, AccessFs::from_read(ABI_WANTED))))
        .and_then(|r| r.add_rules(path_beneath_rules(&rules.read_write, AccessFs::from_all(ABI_WANTED))))
        .and_then(|r| r.restrict_self())
        .map_err(|e| fail("Landlock", &e))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(std::io::Error::other("couldn't set up the sandbox: the kernel doesn't enforce Landlock"));
    }
    block_network().map_err(|e| fail("seccomp", &e))
}

/// Refuses `socket()` for every address family but `AF_UNIX`, and io_uring, on this thread (and
/// what it starts) with `EPERM`. Architectures seccompiler doesn't know keep Landlock's TCP rules.
fn block_network() -> Result<(), seccompiler::Error> {
    use seccompiler::{SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule, TargetArch};
    let arch = match std::env::consts::ARCH {
        "x86_64" => TargetArch::x86_64,
        "aarch64" => TargetArch::aarch64,
        "riscv64" => TargetArch::riscv64,
        _ => return Ok(()),
    };
    let not_unix = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Ne, libc::AF_UNIX as u64)?;
    let rules = std::collections::BTreeMap::from([
        (libc::SYS_socket, vec![SeccompRule::new(vec![not_unix])?]),
        // An empty rule list matches every call.
        (libc::SYS_io_uring_setup, Vec::new()),
    ]);
    let filter = SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32), arch)?;
    let program: seccompiler::BpfProgram = filter.try_into()?;
    seccompiler::apply_filter(&program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external::{TempDir, run, run_with};
    use std::time::Duration;

    /// Runs `program args` confined by `how` ("landlock", "bwrap" or "none") for a job in `job`.
    fn try_run(how: &str, program: &str, args: &[&std::ffi::OsStr], job: &TempDir) -> Result<Vec<u8>, String> {
        let rules = Rules::new(Path::new(program), &job.path);
        let t = Duration::from_secs(20);
        let r = match how {
            "landlock" => {
                let mut cmd = std::process::Command::new(program);
                cmd.args(args).env_clear().current_dir(&job.path);
                run_with("test", &mut cmd, None, t, 1 << 20, |c| spawn_confined(c, &rules))
            }
            "bwrap" => {
                let mut cmd = bwrap_command(Path::new(program), &rules).unwrap();
                cmd.args(args).env_clear().current_dir(&job.path);
                run("test", &mut cmd, None, t, 1 << 20)
            }
            _ => {
                let mut cmd = std::process::Command::new(program);
                cmd.args(args).current_dir(&job.path);
                run("test", &mut cmd, None, t, 1 << 20)
            }
        };
        r.map_err(|e| e.to_string())
    }

    /// Every check that the sandbox must block, run under `how`; with `"none"` they all succeed,
    /// which shows the checks test the sandbox and not the setup.
    fn confinement(how: &str) {
        let (job, elsewhere) = (TempDir::new().unwrap(), TempDir::new().unwrap());
        let inside = job.write("in.txt", b"job data").unwrap();
        let outside = elsewhere.write("private.txt", b"someone's file").unwrap();
        let blocked = how != "none";
        assert_eq!(try_run(how, "/bin/cat", &[inside.as_os_str()], &job).unwrap(), b"job data", "{how}: its input");
        assert_eq!(try_run(how, "/bin/cat", &[outside.as_os_str()], &job).is_err(), blocked, "{how}: another folder");
        let made = job.path.join("made");
        assert!(try_run(how, "/usr/bin/touch", &[made.as_os_str()], &job).is_ok() && made.exists(), "{how}: its output");
        let planted = elsewhere.path.join("planted");
        assert_eq!(try_run(how, "/usr/bin/touch", &[planted.as_os_str()], &job).is_err(), blocked, "{how}: writing elsewhere");
        if let Some(home) = std::env::var_os("HOME").filter(|h| Path::new(h).is_dir()) {
            assert_eq!(try_run(how, "/bin/ls", &[home.as_os_str()], &job).is_err(), blocked, "{how}: the home folder");
        }
        // A TCP connection attempt (bash's /dev/tcp needs no network tools): refused by seccomp
        // and Landlock, or by bwrap's empty network namespace, before any packet leaves.
        if Path::new("/bin/bash").exists() {
            let probe = ["-c", "exec 3<>/dev/tcp/127.0.0.1/9"].map(std::ffi::OsStr::new);
            let r = try_run(how, "/bin/bash", &probe, &job);
            if blocked {
                assert!(r.is_err(), "{how}: the network");
            }
        }
    }

    #[test]
    fn landlock_confines_a_tool_to_its_job_folder() {
        if !landlock_available() {
            eprintln!("this kernel doesn't enforce Landlock: skipping");
            return;
        }
        confinement("landlock");
        // Other processes' command lines and environments stay private.
        let job = TempDir::new().unwrap();
        assert!(try_run("landlock", "/bin/cat", &[std::ffi::OsStr::new("/proc/1/cmdline")], &job).is_err());
        // The restricted thread is gone: PhotoCraft itself still reads anywhere.
        assert!(std::fs::read_dir("/").is_ok());
    }

    #[test]
    fn bwrap_confines_a_tool_to_its_job_folder() {
        if !bwrap_works() {
            eprintln!("bwrap isn't installed or can't create namespaces here: skipping");
            return;
        }
        confinement("bwrap");
    }

    #[test]
    fn unconfined_the_same_checks_succeed() {
        confinement("none");
    }

    #[test]
    fn a_tool_may_read_its_folder_and_lib_never_what_is_above() {
        let dirs = |p: &str| {
            let (d, l) = tool_dirs(Path::new(p));
            (d.to_string_lossy().into_owned(), l.to_string_lossy().into_owned())
        };
        assert_eq!(dirs("/opt/libjxl/bin/cjxl"), ("/opt/libjxl/bin".into(), "/opt/libjxl/lib".into()));
        assert_eq!(dirs("/bin/cat"), ("/bin".into(), "/lib".into()), "not the whole disk");
        assert_eq!(dirs("/home/x/bin/cjxl"), ("/home/x/bin".into(), "/home/x/lib".into()), "not the home folder");
    }

    #[test]
    fn the_rules_never_open_the_home_folder_or_proc() {
        let r = Rules::new(Path::new("/usr/bin/heif-dec"), Path::new("/tmp/photocraft-tool-1"));
        for p in r.read.iter().chain(&r.read_write) {
            assert!(!p.starts_with("/proc") && p != Path::new("/") && p != Path::new("/home") && p != Path::new("/etc"), "{}", p.display());
        }
        assert_eq!(r.read_write[0], Path::new("/tmp/photocraft-tool-1"));
    }
}
