//! `control-ofc-restore-auto` — the `ExecStopPost` hand-back — run for real
//! against a fake sysfs tree (DEC-382).
//!
//! The script restates `hwmon::handback`'s rules in shell, because systemd runs
//! it after the daemon has gone. That is two copies of a safety rule, which is
//! why this test drives the script itself rather than trusting that the copies
//! agree: every hand-back kind the daemon can record, a header it never took,
//! and the lines the script must refuse to act on.

use std::path::{Path, PathBuf};
use std::process::Command;

fn script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../packaging/control-ofc-restore-auto.sh")
}

struct Tree {
    _tmp: tempfile::TempDir,
    sys: PathBuf,
    run: PathBuf,
    hwmon: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let sys = tmp.path().join("sys");
        let run = tmp.path().join("run");
        let hwmon = sys.join("class/hwmon/hwmon0");
        std::fs::create_dir_all(&hwmon).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        Self {
            _tmp: tmp,
            sys,
            run,
            hwmon,
        }
    }

    /// `pwmN` at 150 and `pwmN_enable` at 1 — a header the daemon has in manual.
    fn header(&self, n: u8) -> (PathBuf, PathBuf) {
        let pwm = self.hwmon.join(format!("pwm{n}"));
        let enable = self.hwmon.join(format!("pwm{n}_enable"));
        std::fs::write(&pwm, "150\n").unwrap();
        std::fs::write(&enable, "1\n").unwrap();
        (enable, pwm)
    }

    fn record(&self, lines: &[String]) {
        let mut body = String::from("# control-ofc hwmon hand-back record v1\n");
        for l in lines {
            body.push_str(l);
            body.push('\n');
        }
        std::fs::write(self.run.join("hwmon-handback"), body).unwrap();
    }

    fn run_script(&self) -> std::process::Output {
        Command::new("bash")
            .arg(script())
            .env("RUNTIME_DIRECTORY", &self.run)
            .env("CONTROL_OFC_SYSFS_ROOT", &self.sys)
            .output()
            .expect("bash must be available to run the ExecStopPost script")
    }

    /// Run the script with every read of `unreadable` refused, as sysfs refuses
    /// a read-open of a `0200` attribute (`EACCES`) even to root. The script
    /// reads through `cat`, so a `cat` earlier on `PATH` that fails for that one
    /// path stands in for the kernel. A regular file with mode `0200` would not:
    /// root can read it, and CI may run as root.
    fn run_script_unable_to_read(&self, unreadable: &Path) -> std::process::Output {
        use std::os::unix::fs::PermissionsExt;
        let bin = self.run.with_file_name("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = std::env::var("PATH").unwrap_or_default();
        let shim = bin.join("cat");
        std::fs::write(
            &shim,
            format!(
                "#!/bin/bash\n[ \"$1\" = '{}' ] && exit 1\nPATH='{path}' exec cat \"$@\"\n",
                unreadable.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
        Command::new("bash")
            .arg(script())
            .env("RUNTIME_DIRECTORY", &self.run)
            .env("CONTROL_OFC_SYSFS_ROOT", &self.sys)
            .env("PATH", format!("{}:{path}", bin.display()))
            .output()
            .expect("bash must be available to run the ExecStopPost script")
    }
}

fn line(enable: &Path, pwm: &Path, kind: &str, value: &str) -> String {
    format!("{}\t{}\t{kind}\t{value}", enable.display(), pwm.display())
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap().trim().to_string()
}

/// [SAFETY] DEC-382 (`TS-a`): each recorded header gets back exactly what it
/// had, and nothing else is written. The script this replaced wrote `2` to every
/// `pwm*_enable` on the machine — `h1` would read `2` here, and `h3`, which no
/// record line names, would have been rewritten too.
#[test]
fn the_record_is_replayed_and_nothing_else_is_touched() {
    let t = Tree::new();
    let (en1, pwm1) = t.header(1);
    let (en2, pwm2) = t.header(2);
    let (en3, _) = t.header(3);
    let (en4, pwm4) = t.header(4);
    let (en5, pwm5) = t.header(5);
    t.record(&[
        line(&en1, &pwm1, "mode", "5"),
        line(&en2, &pwm2, "manual", "77"),
        line(&en4, &pwm4, "full", "-"),
        line(&en5, &pwm5, "write-only", "2"),
    ]);

    let out = t.run_script();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(read(&en1), "5", "a recorded mode is written back");
    assert_eq!(
        (read(&en2), read(&pwm2)),
        ("1".to_string(), "77".to_string()),
        "a header found in manual gets its duty back"
    );
    assert_eq!(
        read(&en4),
        "0",
        "an unrecorded mode gets fancontrol's full speed"
    );
    assert_eq!(
        (read(&en5), read(&pwm5)),
        ("2".to_string(), "150".to_string()),
        "a write-only switch gets its recorded value, and its duty is not touched"
    );
    assert_eq!(
        read(&en3),
        "1",
        "a header no record line names is not touched"
    );
}

/// [SAFETY] DEC-398 (`TS-ab`): a `write-only` line is confirmed by the write
/// alone. The switch here refuses every read, as `dell_smm`'s `0200`
/// `pwm1_enable` does. Confirmed by reading it back instead, the refused read
/// fails the confirmation, the fallback runs, and it leaves the switch at `1`
/// and the duty at 255 — on `dell_smm` that `1` takes the fans back from the
/// BIOS. The switch reading `2` afterwards is the presence half: a line the
/// script skipped would also leave the duty alone.
#[test]
fn a_write_only_switch_is_confirmed_by_the_write_alone() {
    let t = Tree::new();
    let (en1, pwm1) = t.header(1);
    t.record(&[line(&en1, &pwm1, "write-only", "2")]);

    let out = t.run_script_unable_to_read(&en1);

    assert!(out.status.success(), "{out:?}");
    assert_eq!(read(&en1), "2", "the switch was written");
    assert_eq!(read(&pwm1), "150", "and no fallback took it back");
    assert!(out.stderr.is_empty(), "{out:?}");
}

/// A write-only switch that refuses the write is not reported given back: the
/// fallback runs, and where nothing at all can be written the script says so.
#[test]
fn a_write_only_switch_that_takes_no_write_is_reported() {
    let t = Tree::new();
    let (en1, pwm1) = t.header(1);
    std::fs::remove_file(&en1).unwrap();
    // A directory: it exists, and every write to it fails.
    std::fs::create_dir(&en1).unwrap();
    t.record(&[line(&en1, &pwm1, "write-only", "2")]);

    let out = t.run_script();

    assert!(out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains(&format!("could not give {} back", en1.display())),
        "{stderr}"
    );
    assert_eq!(read(&pwm1), "150");
}

/// A line is checked before anything is written to the path it names — the
/// record is root's, but the script is the last writer after a crash and must
/// not be the thing that writes somewhere unexpected.
#[test]
fn a_line_naming_a_path_outside_sysfs_or_a_mismatched_pair_is_skipped() {
    let t = Tree::new();
    let (en1, pwm1) = t.header(1);
    let (en2, _pwm2) = t.header(2);
    let outside = t.run.join("pwm9_enable");
    std::fs::write(&outside, "1\n").unwrap();
    t.record(&[
        // Outside the sysfs root.
        line(&outside, &t.run.join("pwm9"), "mode", "5"),
        // An enable file that does not belong to the pwm file named with it.
        line(&en2, &pwm1, "mode", "5"),
        // A kind the script does not know.
        line(&en1, &pwm1, "rewrite", "5"),
    ]);

    let out = t.run_script();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(read(&outside), "1");
    assert_eq!(read(&en2), "1");
    assert_eq!(read(&en1), "1");
}

/// A record that asks for a value the driver will not keep falls back to full
/// speed. A regular file keeps whatever it is given, so the "driver" here is a
/// value outside the byte range, which the script treats as unrecorded.
#[test]
fn an_unusable_recorded_value_falls_back_to_full_speed() {
    let t = Tree::new();
    let (en1, pwm1) = t.header(1);
    t.record(&[line(&en1, &pwm1, "mode", "300")]);

    let out = t.run_script();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(read(&en1), "0");
}

/// No record — a daemon that never took a header, or one that gave every header
/// back itself before exiting — means nothing is written and the unit does not
/// fail.
#[test]
fn no_record_writes_nothing() {
    let t = Tree::new();
    let (en1, _) = t.header(1);

    let out = t.run_script();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(read(&en1), "1");
}
