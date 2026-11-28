//! Detached desktop launcher behind `zeron-pet --launch`.
//!
//! The plugin's `commands` permission may only spawn `zeron-pet`; this flag
//! is the entry point that turns that spawn into "make sure the desktop pet
//! is up". The contract is: return quickly with the running status line,
//! cold-start a detached GTK instance when absent, never restart a running
//! one, and never leave the caller's stdout/stderr pipes held open.
//!
//! Readiness is probed by executing `SELF --status` with a hard per-probe
//! deadline (the real status line is under 1KB). Probe stdio is captured to
//! transcripts inside a private 0700 scratch directory — created, never
//! reused — rather than pipes: a pipe whose write end is inherited by a
//! grandchild can deadlock the reader, `wait_with_output` has no deadline
//! at all, and a predictable world-writable path invites symlink games.
//! Transcript growth is capped while the probe runs and after it exits.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use std::os::unix::process::CommandExt;

/// Per-probe ceiling: a wedged `--status` must never stall the launch.
const PROBE_MAX: Duration = Duration::from_secs(1);
/// Readiness poll cadence between `--status` probes.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
/// Cap on probe transcripts kept for diagnostics; the real status line is
/// ~200 bytes, anything beyond this is noise from a misbehaving binary.
const PROBE_OUTPUT_CAP: u64 = 16 * 1024;

/// The only cold-start signature: exit 1 with exactly this line on stdout.
/// Any other probe result means "do not launch".
const NOT_RUNNING: &str = "zeron-pet: not running";

/// argv elements that consume the next element as a value. A literal
/// `--launch` in that position is data, not a flag, and is preserved.
const VALUE_FLAGS: [&str; 6] = [
    "--assets",
    "--scale",
    "--monitor",
    "--demo",
    "--pose",
    "--click-through",
];

static TAG_SEQ: AtomicU64 = AtomicU64::new(0);

/// Ensure a zeron-pet desktop is running and return its status line.
///
/// `executable` is the binary to probe and to spawn (normally the current
/// exe). `forwarded_args` are passed to a cold-started instance verbatim,
/// minus flag-position `--launch` elements. `timeout` bounds the whole
/// operation including every probe.
///
/// Succeeds without spawning when a pet already answers `--status` — a
/// repeated `--launch` against the user's existing instance is a no-op.
/// Errors describe probe failures explicitly; a launch only ever follows
/// the exact `not running` signature.
pub fn launch(
    executable: &Path,
    forwarded_args: &[String],
    timeout: Duration,
) -> Result<String, String> {
    launch_in(executable, forwarded_args, timeout, &std::env::temp_dir())
}

/// `launch` with an explicit scratch root for probe transcripts. Unit
/// tests inject a root they fully control; production always passes the
/// system temp dir.
fn launch_in(
    executable: &Path,
    forwarded_args: &[String],
    timeout: Duration,
    temp_root: &Path,
) -> Result<String, String> {
    let deadline = Instant::now() + timeout;

    match probe_status(executable, deadline, temp_root)? {
        Probe::Running(status) => return Ok(status),
        Probe::NotRunning => {}
        Probe::Failed(reason) => return Err(reason),
    }

    let child = spawn_detached(executable, forwarded_args)?;
    wait_ready(executable, child, deadline, temp_root)
}

enum Probe {
    /// exit 0: the line is the pet's one-line status.
    Running(String),
    /// exit 1 with exactly `zeron-pet: not running` on stdout.
    NotRunning,
    /// Any other outcome — including exit 3 (no display/GTK) — which must
    /// never trigger a launch.
    Failed(String),
}

/// Run `executable --status` within `min(PROBE_MAX, deadline)` and classify
/// the result. Spawn failures and probe timeouts are hard `Err`s.
fn probe_status(executable: &Path, deadline: Instant, temp_root: &Path) -> Result<Probe, String> {
    let out = run_bounded(executable, &["--status".to_string()], deadline, temp_root)?;
    let stdout = out.stdout.trim_end();
    match out.status.code() {
        Some(0) => Ok(Probe::Running(stdout.to_string())),
        Some(1) if stdout == NOT_RUNNING => Ok(Probe::NotRunning),
        Some(3) => Ok(Probe::Failed(format!(
            "{}: status probe exited 3 (no display? GTK init failed): {}",
            executable.display(),
            summarize(&out),
        ))),
        code => Ok(Probe::Failed(format!(
            "{}: unexpected status probe result (exit {code:?}): {}",
            executable.display(),
            summarize(&out),
        ))),
    }
}

struct BoundedOut {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

fn summarize(out: &BoundedOut) -> String {
    let clip = |s: &str| -> String {
        let t = s.trim();
        if t.chars().count() > 160 {
            format!("{:.160}…", t)
        } else {
            t.to_string()
        }
    };
    format!(
        "stdout={:?} stderr={:?}",
        clip(&out.stdout),
        clip(&out.stderr)
    )
}

/// Private scratch dir holding one probe's transcripts. `DirBuilder`
/// with mode 0700 makes mkdir itself private — no permissive window, and
/// a failed create leaves nothing behind — while still refusing
/// preexisting paths (including symlinks) so nothing already in place
/// can be followed, truncated, or deleted. Names carry pid +
/// timestamp + an atomic sequence; retries dodge collisions without ever
/// taking over an existing path. Drop removes only the directory this
/// guard created (and whatever the probe wrote inside it), on every path.
struct ProbeDir(PathBuf);

impl ProbeDir {
    fn create(root: &Path) -> Result<ProbeDir, String> {
        let pid = std::process::id();
        for _ in 0..32 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let seq = TAG_SEQ.fetch_add(1, AtomicOrdering::Relaxed);
            let path = root.join(format!("zeron-pet-probe-{pid}-{nanos}-{seq}"));
            // `mut` only pays for the unix-only mode(0o700) below.
            #[allow(unused_mut)]
            let mut builder = fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            match builder.create(&path) {
                Ok(()) => return Ok(ProbeDir(path)),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("{}: probe dir: {e}", path.display())),
            }
        }
        Err(format!(
            "{}: could not allocate a probe dir",
            root.display()
        ))
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for ProbeDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Open a fresh transcript: `create_new` can never touch an existing
/// inode; mode 0600 keeps probe output private.
fn probe_file(path: &Path) -> Result<File, String> {
    let mut opt = fs::OpenOptions::new();
    opt.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opt.mode(0o600);
    }
    opt.open(path)
        .map_err(|e| format!("{}: probe transcript: {e}", path.display()))
}

fn file_len(f: &File) -> u64 {
    f.metadata().map(|m| m.len()).unwrap_or(0)
}

/// Execute `executable args` with stdin/stdout/stderr redirected (stdout and
/// stderr to fresh private transcripts inside a ProbeDir), wait with a
/// deadline, then reap and read back capped transcripts. On timeout the
/// child is SIGKILLed and reaped; a probe that floods a transcript past
/// PROBE_OUTPUT_CAP is killed the same way — the cap is enforced during
/// the wait and once more after reap so a fast writer can't slip through.
fn run_bounded(
    executable: &Path,
    args: &[String],
    deadline: Instant,
    temp_root: &Path,
) -> Result<BoundedOut, String> {
    let now = Instant::now();
    if now >= deadline {
        return Err(format!(
            "{}: probe deadline already exhausted",
            executable.display()
        ));
    }
    let budget = std::cmp::min(deadline, now + PROBE_MAX);

    let dir = ProbeDir::create(temp_root)?;
    let out_path = dir.path("stdout");
    let err_path = dir.path("stderr");
    let out_file = probe_file(&out_path)?;
    let err_file = probe_file(&err_path)?;

    let mut child = spawn_bounded(executable, args, &out_file, &err_file, budget)?;

    let cap_msg = || {
        format!(
            "{} --status exceeded the {}KB output cap (killed)",
            executable.display(),
            PROBE_OUTPUT_CAP / 1024
        )
    };
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {
                if file_len(&out_file) > PROBE_OUTPUT_CAP || file_len(&err_file) > PROBE_OUTPUT_CAP
                {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(cap_msg());
                }
                if Instant::now() >= budget {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(format!(
                        "{} --status exceeded its {}ms deadline (killed)",
                        executable.display(),
                        PROBE_MAX.as_millis()
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "{} --status: wait failed: {e}",
                    executable.display()
                ));
            }
        }
    };

    // The ProbeDir guard removes the whole scratch dir here on every path.
    let status = result?;
    let stdout = read_capped(&out_path);
    let stderr = read_capped(&err_path);
    if file_len(&out_file) > PROBE_OUTPUT_CAP || file_len(&err_file) > PROBE_OUTPUT_CAP {
        return Err(cap_msg());
    }
    Ok(BoundedOut {
        status,
        stdout,
        stderr,
    })
}

/// Spawn a probe child with stdio bound to files. The OS can report the
/// target as transiently busy (ETXTBSY on unix, sharing/lock violations
/// on Windows while AV scans a fresh binary); retry within the caller's
/// budget rather than failing on a transient — a persistent busy still
/// surfaces as an error.
fn spawn_bounded(
    executable: &Path,
    args: &[String],
    out_file: &File,
    err_file: &File,
    budget: Instant,
) -> Result<Child, String> {
    let mut attempt = 0u32;
    loop {
        let stdout = out_file
            .try_clone()
            .map_err(|e| format!("probe stdout clone: {e}"))?;
        let stderr = err_file
            .try_clone()
            .map_err(|e| format!("probe stderr clone: {e}"))?;
        match Command::new(executable)
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout)
            .stderr(stderr)
            .spawn()
        {
            Ok(child) => return Ok(child),
            Err(e) => {
                let text_busy = transient_spawn_busy(&e);
                attempt += 1;
                if !text_busy || attempt > 10 || Instant::now() >= budget {
                    return Err(format!("spawn {} {:?}: {e}", executable.display(), args));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[cfg(unix)]
fn transient_spawn_busy(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(libc::ETXTBSY)
}

/// ERROR_SHARING_VIOLATION / ERROR_LOCK_VIOLATION: a just-written binary
/// can stay locked by AV or indexing for a few scheduler ticks.
#[cfg(windows)]
fn transient_spawn_busy(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(32) | Some(33))
}

fn read_capped(path: &Path) -> String {
    let mut s = String::new();
    if let Ok(f) = File::open(path) {
        let _ = f.take(PROBE_OUTPUT_CAP).read_to_string(&mut s);
    }
    s
}

/// Spawn `executable` detached from this process: null stdio (nothing can
/// hold the caller's pipes open), a fresh session via setsid on unix /
/// DETACHED_PROCESS + own process group on Windows, argv passed without a
/// shell so values are preserved byte-for-byte.
pub fn spawn_detached(executable: &Path, forwarded_args: &[String]) -> Result<Child, String> {
    spawn_detached_env(executable, forwarded_args, &[])
}

/// `spawn_detached` with extra environment variables for the child; the
/// rest of the inherited environment is untouched.
pub fn spawn_detached_env(
    executable: &Path,
    forwarded_args: &[String],
    envs: &[(&str, &Path)],
) -> Result<Child, String> {
    #[cfg(windows)]
    use std::os::windows::process::CommandExt;
    #[cfg(windows)]
    use windows_sys::Win32::System::Threading::{
        CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS,
    };
    let args = strip_launch_flag(forwarded_args);
    let mut cmd = Command::new(executable);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, value) in envs {
        cmd.env(key, value);
    }
    #[cfg(unix)]
    unsafe {
        // AUDIT: pre_exec runs between fork and exec in the child, where
        // only async-signal-safe calls are permitted. setsid() qualifies:
        // it makes the pet a session leader in a new process group, so the
        // desktop instance outlives this launcher and never shares our
        // controlling terminal or process group. No allocation, no locks.
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(std::io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    #[cfg(windows)]
    {
        // DETACHED_PROCESS + own group mirrors the setsid split: no shared
        // console, no console-ctrl propagation. BREAKAWAY additionally
        // escapes an enclosing Job Object so a job-kill spares the pet.
        cmd.creation_flags(CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    match cmd.spawn() {
        #[cfg(windows)]
        Err(e) if e.raw_os_error() == Some(5) => {
            // ERROR_ACCESS_DENIED: the enclosing job lacks the breakaway
            // limit. Retry inside the job — the pet still detaches.
            cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
            cmd.spawn()
        }
        r => r,
    }
    .map_err(|e| format!("spawn {} {:?}: {e}", executable.display(), args))
}

/// Remove flag-position `--launch` elements only. A `--launch` appearing as
/// the value of an option that takes one is data and is kept.
fn strip_launch_flag(args: &[String]) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len());
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--launch" {
            i += 1;
            continue;
        }
        out.push(a.clone());
        if VALUE_FLAGS.contains(&a.as_str()) {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
                i += 1;
            }
        }
        i += 1;
    }
    out
}

/// Reap a spawned child on a background thread. Without this, a `Child`
/// dropped while still running leaves a zombie if it exits before a
/// long-lived caller does; the `--launch` CLI exits right away and init
/// adopts the pet, but library embeddings get a clean reap either way.
pub(crate) fn reap_async(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Poll `--status` until the pet answers or the deadline hits. A readiness
/// answer wins even if our spawn already exited (single-instance forward:
/// the second invocation can exit 0 after handing off to a running pet).
/// On timeout or early child exit, only the child we spawned is killed and
/// reaped — never a process group, never a killall.
fn wait_ready(
    executable: &Path,
    mut child: Child,
    deadline: Instant,
    temp_root: &Path,
) -> Result<String, String> {
    let mut last_probe: Option<String> = None;
    loop {
        if Instant::now() < deadline {
            match probe_status(executable, deadline, temp_root) {
                Ok(Probe::Running(status)) => {
                    reap_async(child);
                    return Ok(status);
                }
                Ok(Probe::NotRunning) => last_probe = None,
                Ok(Probe::Failed(reason)) => last_probe = Some(reason),
                Err(reason) => last_probe = Some(reason),
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // Grace probe: a single-instance hand-off makes readiness
                // true while our own spawn exits 0. One last probe decides.
                if let Ok(Probe::Running(s)) = probe_status(executable, deadline, temp_root) {
                    return Ok(s);
                }
                let detail = last_probe
                    .map(|p| format!("; last probe: {p}"))
                    .unwrap_or_default();
                return Err(format!(
                    "{} exited during launch ({status}){detail}",
                    executable.display()
                ));
            }
            Ok(None) => {}
            Err(e) => {
                // try_wait may fail before reaping — still terminate and
                // reap exactly the child we spawned.
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "{}: wait on spawned instance failed: {e}",
                    executable.display()
                ));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let detail = last_probe
                .map(|p| format!("; last probe: {p}"))
                .unwrap_or_default();
            return Err(format!(
                "timed out waiting for the pet to become ready{detail}"
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    /// Unique injected scratch root per test; tests never touch
    /// std::env::temp_dir() shared space.
    fn test_root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "zeron-pet-launcher-ut-{}-{}-{name}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[cfg(unix)]
    fn write_exe(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake-pet");
        fs::write(&p, body).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn probe_artifacts(root: &Path) -> Vec<PathBuf> {
        fs::read_dir(root)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("zeron-pet-probe-"))
            })
            .collect()
    }

    #[test]
    fn probe_dir_is_private_unique_and_self_cleaning() {
        let root = test_root("dir-private");
        let dir = ProbeDir::create(&root).unwrap();
        #[cfg(unix)]
        assert_eq!(
            dir.0.metadata().unwrap().permissions().mode() & 0o777,
            0o700,
            "scratch dir must be owner-only"
        );
        let _f = probe_file(&dir.path("stdout")).unwrap();
        #[cfg(unix)]
        assert_eq!(
            _f.metadata().unwrap().permissions().mode() & 0o777,
            0o600,
            "transcript must be owner-only"
        );
        // create_new never overwrites what exists.
        assert!(probe_file(&dir.path("stdout")).is_err());
        drop(_f); // Windows cannot remove a dir holding an open file.
        let created = dir.0.clone();
        drop(dir);
        assert!(!created.exists(), "guard must remove its own dir");
        let a = ProbeDir::create(&root).unwrap();
        let b = ProbeDir::create(&root).unwrap();
        assert_ne!(a.0, b.0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)]
    fn probe_never_touches_preexisting_paths() {
        let root = test_root("preexisting");
        // A symlink at the *old* predictable naming scheme, pointing at a
        // sentinel: hostile fixture that must survive untouched.
        let sentinel = root.join("sentinel.txt");
        fs::write(&sentinel, "keep-me").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                &sentinel,
                root.join(format!("zeron-pet-probe-{}-0.out", std::process::id())),
            )
            .unwrap();
            std::os::unix::fs::symlink(
                &sentinel,
                root.join(format!("zeron-pet-probe-{}-0", std::process::id())),
            )
            .unwrap();
        }
        // A lookalike preexisting directory with content must survive too.
        let decoy = root.join(format!("zeron-pet-probe-{}-decoy", std::process::id()));
        fs::create_dir(&decoy).unwrap();
        fs::write(decoy.join("data"), "keep-me").unwrap();

        let fake = write_exe(&root, "#!/bin/sh\necho ok-status\nexit 0\n");
        let out = run_bounded(
            &fake,
            &["--status".to_string()],
            Instant::now() + Duration::from_secs(2),
            &root,
        )
        .unwrap();
        assert!(out.status.success());
        assert_eq!(fs::read_to_string(&sentinel).unwrap(), "keep-me");
        assert_eq!(fs::read_to_string(decoy.join("data")).unwrap(), "keep-me");
        // All scratch dirs the probe created are gone; every preexisting
        // fixture (decoy dir + both symlinks) remains.
        let mut remaining = probe_artifacts(&root);
        remaining.sort();
        let mut fixtures = vec![
            decoy.clone(),
            root.join(format!("zeron-pet-probe-{}-0", std::process::id())),
            root.join(format!("zeron-pet-probe-{}-0.out", std::process::id())),
        ];
        fixtures.sort();
        assert_eq!(remaining, fixtures);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn spawn_failure_leaves_no_probe_artifacts() {
        let root = test_root("spawn-fail");
        // Missing executable.
        let missing = root.join("no-such-exe");
        assert!(run_bounded(
            &missing,
            &["--status".to_string()],
            Instant::now() + Duration::from_secs(2),
            &root,
        )
        .is_err());
        // Existing but not executable.
        let not_exec = root.join("not-exec");
        fs::write(&not_exec, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        fs::set_permissions(&not_exec, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(run_bounded(
            &not_exec,
            &["--status".to_string()],
            Instant::now() + Duration::from_secs(2),
            &root,
        )
        .is_err());
        assert!(
            probe_artifacts(&root).is_empty(),
            "spawn failure must leave zero probe artifacts: {:?}",
            probe_artifacts(&root)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)] // fixture is a #!/bin/sh script
    fn oversized_probe_output_fails_bounded() {
        let root = test_root("output-cap");
        // Writes 64KB to stdout then exits 0: exceeds the 16KB cap.
        let fake = write_exe(&root, "#!/bin/sh\nhead -c 65536 /dev/zero\nexit 0\n");
        let err = run_bounded(
            &fake,
            &["--status".to_string()],
            Instant::now() + Duration::from_secs(3),
            &root,
        )
        .err()
        .unwrap();
        assert!(err.contains("cap"), "{err}");
        assert!(probe_artifacts(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    #[cfg(unix)] // fixture is a #!/bin/sh script
    fn runaway_probe_output_is_killed_bounded() {
        let root = test_root("output-runaway");
        // Floods stderr forever: must be killed near the cap, not at 1s.
        let fake = write_exe(&root, "#!/bin/sh\nexec cat /dev/zero >&2\n");
        let start = Instant::now();
        let err = run_bounded(
            &fake,
            &["--status".to_string()],
            Instant::now() + Duration::from_secs(10),
            &root,
        )
        .err()
        .unwrap();
        assert!(err.contains("cap") || err.contains("deadline"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "runaway writer must be stopped, took {:?}",
            start.elapsed()
        );
        assert!(probe_artifacts(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn strips_only_flag_position_launch() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            strip_launch_flag(&args(&["--launch"])),
            Vec::<String>::new()
        );
        assert_eq!(
            strip_launch_flag(&args(&["--launch", "--monitor", "--launch", "--launch"])),
            args(&["--monitor", "--launch"])
        );
        assert_eq!(
            strip_launch_flag(&args(&["--assets", "/a b", "--scale", "3"])),
            args(&["--assets", "/a b", "--scale", "3"])
        );
        // A bare value never in flag position is preserved.
        assert_eq!(
            strip_launch_flag(&args(&["--click-through", "--launch"])),
            args(&["--click-through", "--launch"])
        );
    }
}
