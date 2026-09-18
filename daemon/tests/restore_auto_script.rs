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
    t.record(&[
        line(&en1, &pwm1, "mode", "5"),
        line(&en2, &pwm2, "manual", "77"),
        line(&en4, &pwm4, "full", "-"),
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
        read(&en3),
        "1",
        "a header no record line names is not touched"
    );
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
