//! `control-ofc-daemon`'s `system-sleep` hook, run for real (`TS-ao`, DEC-396).
//!
//! The hook sends a signal whose DEFAULT action terminates the process, so what
//! matters most is whom it refuses to signal: anything that is not both the PID
//! the daemon wrote and the unit's MainPID. A fake `systemctl` stands in for
//! PID 1, and child processes stand in for the daemon — one that handles the
//! signals and acknowledges them, and one that would die of them.

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use control_ofc_daemon::sd_notify::{SLEEP_HOOK_ACK_FILE, SLEEP_HOOK_PID_FILE};

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../packaging/control-ofc-sleep-hook.sh")
}

struct Rig {
    _tmp: tempfile::TempDir,
    run: PathBuf,
    systemctl: PathBuf,
    children: Vec<Child>,
}

impl Rig {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let run = tmp.path().join("run");
        std::fs::create_dir_all(&run).unwrap();
        let systemctl = tmp.path().join("systemctl");
        Self {
            _tmp: tmp,
            run,
            systemctl,
            children: Vec::new(),
        }
    }

    /// The fake PID 1 reports `pid` as the unit's MainPID.
    fn main_pid(&self, pid: u32) {
        std::fs::write(&self.systemctl, format!("#!/bin/sh\necho {pid}\n")).unwrap();
        let mut perm = std::fs::metadata(&self.systemctl).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perm, 0o755);
        std::fs::set_permissions(&self.systemctl, perm).unwrap();
    }

    fn pid_file(&self, pid: u32) {
        std::fs::write(self.run.join(SLEEP_HOOK_PID_FILE), format!("{pid}\n")).unwrap();
    }

    fn ack(&self) -> PathBuf {
        self.run.join(SLEEP_HOOK_ACK_FILE)
    }

    /// A stand-in daemon that acknowledges each signal the way the real one does
    /// (after "sending"), and records which signals it saw.
    fn answering_daemon(&mut self) -> u32 {
        self.daemon_acking_pre_with("sent")
    }

    /// The same stand-in, acknowledging `pre` with `outcome` ("sent", "failed", ...).
    fn daemon_acking_pre_with(&mut self, outcome: &str) -> u32 {
        let ready = self.run.join("ready");
        let seen = self.run.join("seen");
        let ack = self.ack();
        let body = format!(
            "trap 'echo usr1 >> {s}; echo \"pre {outcome}\" > {a}' USR1\n\
             trap 'echo usr2 >> {s}; echo \"post sent\" > {a}' USR2\n\
             touch {r}\n\
             while :; do sleep 0.02; done\n",
            s = seen.display(),
            a = ack.display(),
            r = ready.display()
        );
        let child = Command::new("bash").arg("-c").arg(body).spawn().unwrap();
        let pid = child.id();
        self.children.push(child);
        wait_for(|| ready.exists(), "the stand-in daemon installs its traps");
        pid
    }

    /// A process with SIGUSR1's default action: the signal would kill it.
    fn fragile_process(&mut self) -> usize {
        self.children
            .push(Command::new("sleep").arg("30").spawn().unwrap());
        self.children.len() - 1
    }

    fn seen(&self) -> String {
        std::fs::read_to_string(self.run.join("seen")).unwrap_or_default()
    }

    fn run_hook(&self, phase: &str) -> (std::process::Output, Duration) {
        let started = Instant::now();
        let out = Command::new("bash")
            .arg(script())
            .args([phase, "suspend"])
            .env("CONTROL_OFC_RUN_DIR", &self.run)
            .env("CONTROL_OFC_SYSTEMCTL", &self.systemctl)
            .output()
            .expect("bash must be available to run the sleep hook");
        (out, started.elapsed())
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn wait_for(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// `pre` signals the daemon and returns once it has acknowledged — the wait is
/// what keeps the sleep from starting before the wide watchdog is queued.
#[test]
fn pre_signals_the_daemon_and_waits_for_its_acknowledgement() {
    let mut rig = Rig::new();
    let pid = rig.answering_daemon();
    rig.main_pid(pid);
    rig.pid_file(pid);
    std::fs::write(rig.ack(), "post sent\n").unwrap(); // a stale ack from last time

    let (out, took) = rig.run_hook("pre");
    assert!(out.status.success());
    assert_eq!(rig.seen(), "usr1\n");
    assert_eq!(
        std::fs::read_to_string(rig.ack()).unwrap(),
        "pre sent\n",
        "the hook must have waited for THIS acknowledgement, not the stale one"
    );
    assert!(
        took < Duration::from_secs(2),
        "returned on the ack, not the timeout"
    );
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // `post` does not wait for the acknowledgement, so poll for its effect.
    let (out, _) = rig.run_hook("post");
    assert!(out.status.success());
    wait_for(|| rig.seen() == "usr1\nusr2\n", "post sends SIGUSR2");
}

/// A widen the daemon could not deliver is acknowledged as `pre failed`: the
/// hook still lets the sleep proceed, and says so in the journal rather than
/// passing it off as success.
#[test]
fn a_failed_widen_is_reported_not_passed_off_as_success() {
    let mut rig = Rig::new();
    let pid = rig.daemon_acking_pre_with("failed");
    rig.main_pid(pid);
    rig.pid_file(pid);

    let (out, took) = rig.run_hook("pre");
    assert!(
        out.status.success(),
        "a failed widen must never block the sleep"
    );
    assert!(
        took < Duration::from_secs(2),
        "returned on the ack, not the timeout"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not widen its watchdog"),
        "no warning for a failed widen: {stderr:?}"
    );
}

/// [SAFETY] A PID the daemon's file names but systemd does not report as the
/// unit's MainPID is never signalled — a stale file must not kill whatever now
/// holds that PID.
#[test]
fn a_pid_that_is_not_the_units_main_pid_is_never_signalled() {
    let mut rig = Rig::new();
    let idx = rig.fragile_process();
    let pid = rig.children[idx].id();
    rig.pid_file(pid);
    rig.main_pid(pid + 1);

    let (out, _) = rig.run_hook("pre");
    assert!(out.status.success());
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        rig.children[idx].try_wait().unwrap().is_none(),
        "the process was signalled (SIGUSR1 killed it)"
    );

    // Presence before absence: the same process, once it IS the MainPID, does
    // receive the signal — so the survival above was the guard, not a dud rig.
    rig.main_pid(pid);
    let (_, took) = rig.run_hook("pre");
    wait_for(
        || rig.children[idx].try_wait().unwrap().is_some(),
        "the matching PID is signalled",
    );
    assert!(
        took >= Duration::from_secs(2),
        "no ack came, so the hook waited out its bound"
    );
}

/// [SAFETY] With no PID file — an older daemon that predates the hook, or one
/// still starting — nothing is signalled, even when systemd names a MainPID.
#[test]
fn no_pid_file_signals_nothing() {
    let mut rig = Rig::new();
    let idx = rig.fragile_process();
    rig.main_pid(rig.children[idx].id());

    let (out, took) = rig.run_hook("pre");
    assert!(out.status.success());
    assert!(took < Duration::from_secs(1), "nothing to wait for");
    std::thread::sleep(Duration::from_millis(100));
    assert!(rig.children[idx].try_wait().unwrap().is_none());
}

/// Any phase other than `pre`/`post` is ignored.
#[test]
fn an_unknown_phase_is_ignored() {
    let mut rig = Rig::new();
    let pid = rig.answering_daemon();
    rig.main_pid(pid);
    rig.pid_file(pid);
    let (out, _) = rig.run_hook("resume");
    assert!(out.status.success());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.seen(), "");
}

/// The hook and the daemon agree on the file names — two copies of one contract.
#[test]
fn the_hook_uses_the_daemons_file_names() {
    let body = std::fs::read_to_string(script()).unwrap();
    for name in [SLEEP_HOOK_PID_FILE, SLEEP_HOOK_ACK_FILE] {
        assert!(body.contains(&format!("$run_dir/{name}\"")), "{name}");
    }
}
