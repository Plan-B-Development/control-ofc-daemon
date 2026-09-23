//! Guards on `packaging/control-ofc-superio-guard` (DEC-332).
//!
//! The guard stops `nct6775`/`w83627ehf` probing 0x2E/0x4E on a board whose
//! Super-I/O is ITE. Both drivers call one `superio_enter()` that writes
//! `outb(0x87); outb(0x87)` UNCONDITIONALLY before reading the device ID, and on
//! a Gigabyte board with an ITE eSPI-to-LPC bridge that write latches the bridge
//! into configuration mode — measured 2026-09-05 on an X870E AORUS MASTER as the
//! loss of 3 of 8 fan headers and 3 of 9 temperatures until a full power cut.
//!
//! Two properties are pinned here, and they fail for different reasons:
//!   * the **decision table** — suppress on an ITE board, load everywhere else;
//!   * **parity** with `GIGABYTE_DUAL_CHIP_BOARDS`, so the shell list cannot
//!     drift away from the Rust table it was copied from.

use std::path::PathBuf;
use std::process::Command;

fn repo_root() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.."))
}

fn guard_path() -> PathBuf {
    repo_root().join("packaging/control-ofc-superio-guard")
}

/// Run the guard against a synthetic DMI directory, in dry-run so it prints its
/// decision instead of exec'ing the real `modprobe`.
fn decide(vendor: &str, board: Option<&str>, module: &str) -> String {
    // Distinct per CASE, not per board: two cases share the board name
    // "X870E AORUS MASTER" and differ only by vendor, so keying on the board
    // alone gave them one directory. Under cargo's parallel test threads each
    // overwrote the other's `board_vendor` and each called `remove_dir_all` on
    // it — an intermittent red on the two tests that pin the decision table.
    // A static counter makes every call site unique regardless of its inputs.
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "cofc-guard-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create fake DMI dir");
    std::fs::write(dir.join("board_vendor"), vendor).expect("write vendor");
    match board {
        Some(b) => std::fs::write(dir.join("board_name"), b).expect("write board"),
        // Absent board_name models unreadable DMI (container, exotic firmware).
        None => {
            let _ = std::fs::remove_file(dir.join("board_name"));
        }
    }

    let out = Command::new("sh")
        .arg(guard_path())
        .arg(module)
        .env("CONTROL_OFC_DMI_DIR", &dir)
        .env("CONTROL_OFC_GUARD_DRY_RUN", "1")
        .output()
        .expect("run guard");

    std::fs::remove_dir_all(&dir).ok();
    assert!(
        out.status.success(),
        "the guard must always exit 0 — it runs from a modprobe install rule at \
         early boot, where a non-zero exit is a booting user's problem. \
         stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const GIGABYTE: &str = "Gigabyte Technology Co., Ltd.";

#[test]
fn suppresses_nuvoton_probes_on_the_measured_board() {
    // The exact board and both modules the defect was measured on.
    for module in ["nct6775", "w83627ehf"] {
        let d = decide(GIGABYTE, Some("X870E AORUS MASTER"), module);
        assert!(
            d.starts_with("SUPPRESS"),
            "{module} must be suppressed on the board where the latch was \
             measured, got: {d:?}"
        );
    }
}

#[test]
fn board_matching_is_case_insensitive_and_substring() {
    // chip_db.rs matches case-insensitively as a substring; the guard must agree
    // or the two lists would be "identical" and still behave differently.
    let lower = decide(
        "gigabyte technology co., ltd.",
        Some("x870e aorus master"),
        "nct6775",
    );
    assert!(lower.starts_with("SUPPRESS"), "case-insensitive: {lower:?}");

    // Real DMI carries board revision suffixes (`-CF`, ` -CF`, etc.).
    let suffixed = decide(GIGABYTE, Some("X870E AORUS MASTER-CF"), "nct6775");
    assert!(suffixed.starts_with("SUPPRESS"), "substring: {suffixed:?}");
}

#[test]
fn loads_normally_everywhere_the_module_is_actually_needed() {
    // This is the half that matters most: nct6775 is REQUIRED on most Nuvoton
    // boards. A guard that over-matches silently removes fan control from a much
    // larger population than the one it protects.
    let cases: [(&str, Option<&str>, &str); 4] = [
        // A Nuvoton board — the module's whole purpose.
        (
            "ASUSTeK COMPUTER INC.",
            Some("ROG STRIX X670E-E GAMING WIFI"),
            "a Nuvoton board",
        ),
        // Gigabyte, but not a dual-chip ITE board.
        (GIGABYTE, Some("B650M DS3H"), "a single-chip Gigabyte board"),
        // Another vendor that happens to collide on board_name: the vendor gate
        // must stop it.
        (
            "ASRock",
            Some("X870E AORUS MASTER"),
            "a name collision on another vendor",
        ),
        // Unreadable DMI: we cannot judge, so we must not suppress.
        (GIGABYTE, None, "unreadable DMI"),
    ];
    for (vendor, board, why) in cases {
        let d = decide(vendor, board, "nct6775");
        assert!(
            d.starts_with("LOAD"),
            "nct6775 must still load for {why}, got: {d:?}"
        );
    }
}

#[test]
fn superio_guard_board_list_matches_chip_db() {
    // The drift guard. The shell list is a copy of the Rust table; without this
    // test, adding a board to `GIGABYTE_DUAL_CHIP_BOARDS` would silently leave
    // its owners unprotected.
    let guard = std::fs::read_to_string(guard_path()).expect("read guard");
    let begin = guard
        .find("# ── BEGIN GENERATED BOARD LIST ──")
        .expect("BEGIN marker");
    let end = guard
        .find("# ── END GENERATED BOARD LIST ──")
        .expect("END marker");
    let block = &guard[begin..end];
    let list_start = block.find('\'').expect("opening quote") + 1;
    let list_end = block.rfind('\'').expect("closing quote");
    let mut shell_boards: Vec<&str> = block[list_start..list_end]
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();

    let chip_db = std::fs::read_to_string(repo_root().join("daemon/src/hwmon/chip_db.rs"))
        .expect("read chip_db.rs");
    let table_start = chip_db
        .find("const GIGABYTE_DUAL_CHIP_BOARDS")
        .expect("table");
    // The table ends at the first `];` after it starts.
    let table_end = table_start + chip_db[table_start..].find("\n];").expect("table end");
    let mut rust_boards: Vec<&str> = chip_db[table_start..table_end]
        .lines()
        .filter_map(|l| l.trim().strip_prefix("board_name: \""))
        .filter_map(|l| l.strip_suffix("\","))
        .collect();

    assert!(
        !rust_boards.is_empty(),
        "parser found no boards in chip_db.rs — the extraction broke, which \
         would make this test pass vacuously"
    );
    shell_boards.sort_unstable();
    rust_boards.sort_unstable();
    assert_eq!(
        shell_boards, rust_boards,
        "packaging/control-ofc-superio-guard's board list has drifted from \
         GIGABYTE_DUAL_CHIP_BOARDS in daemon/src/hwmon/chip_db.rs. Update the \
         list between the GENERATED markers in the guard."
    );
}

// ── The modprobe.d `install` lines themselves (DEC-414, `PKG-g`) ─────────────

const INSTALLED_GUARD: &str = "/usr/lib/control-ofc/control-ofc-superio-guard";

/// Every `install <module> <command>` line in the shipped drop-in, as kmod
/// reads it: the module, then the rest of the line verbatim.
fn install_lines() -> Vec<(String, String)> {
    let conf =
        std::fs::read_to_string(repo_root().join("packaging/modprobe.d-control-ofc-superio.conf"))
            .expect("read the modprobe.d drop-in");
    conf.lines()
        .filter_map(|l| l.strip_prefix("install "))
        .map(|rest| {
            let (module, cmd) = rest.split_once(' ').expect("install <module> <command>");
            (module.to_string(), cmd.to_string())
        })
        .collect()
}

/// A unique scratch directory per call, for the same parallel-test reason as
/// [`decide`].
fn scratch(tag: &str) -> PathBuf {
    static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "cofc-install-{tag}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

/// Run one install command the way kmod does (`sh -c`, `$CMDLINE_OPTS`
/// substituted), with the guard's installed path pointed at `guard`, and a
/// `modprobe` on PATH that records any call instead of loading anything.
fn run_install(
    cmd: &str,
    guard: &std::path::Path,
    dmi: Option<&std::path::Path>,
) -> (std::process::Output, bool) {
    let bin = scratch("bin");
    let marker = bin.join("modprobe-was-called");
    let fake = bin.join("modprobe");
    std::fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" > '{}'\n", marker.display()),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let cmd = cmd
        .replace(INSTALLED_GUARD, &guard.display().to_string())
        .replace("$CMDLINE_OPTS", "");
    let mut c = Command::new("sh");
    c.arg("-c")
        .arg(&cmd)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
    if let Some(dmi) = dmi {
        c.env("CONTROL_OFC_DMI_DIR", dmi)
            .env("CONTROL_OFC_GUARD_DRY_RUN", "1");
    }
    let out = c.output().expect("run the install command");
    let called = marker.exists();
    std::fs::remove_dir_all(&bin).ok();
    (out, called)
}

#[test]
fn both_modules_have_a_guarded_install_line() {
    // Presence first: the tests below iterate over these lines, and an empty
    // list would pass them all.
    let lines = install_lines();
    let modules: Vec<&str> = lines.iter().map(|(m, _)| m.as_str()).collect();
    assert_eq!(modules, ["nct6775", "w83627ehf"]);
    for (module, cmd) in &lines {
        assert_eq!(
            cmd.matches(INSTALLED_GUARD).count(),
            2,
            "{module}: the line must both check for the guard and exec it, at its \
             installed path: {cmd}"
        );
    }
}

/// [SAFETY] DEC-414 (`PKG-g`): mkinitcpio's `modconf` copies the drop-in into
/// every initramfs but not the guard it names. There, the line must load
/// NOTHING — on an affected ITE board an unguarded load is the very probe the
/// guard exists to stop — and must say why, and must not fail the boot.
#[test]
fn without_the_guard_nothing_is_loaded_and_the_line_says_why() {
    let missing = scratch("noguard").join("control-ofc-superio-guard");
    for (module, cmd) in install_lines() {
        let (out, modprobe_called) = run_install(&cmd, &missing, None);
        assert!(
            out.status.success(),
            "{module}: a missing guard must not fail the initramfs load: {out:?}"
        );
        assert!(
            !modprobe_called,
            "{module}: the line loaded the module without its guard — that is the \
             destructive probe on an ITE board"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains(&format!("{module} is not loaded in the initramfs")),
            "{module}: the line must say why nothing was loaded, got: {stderr:?}"
        );
    }
}

/// With the guard present — every real root — the line hands the decision to
/// the guard, for the module the line names.
#[test]
fn with_the_guard_the_line_hands_the_decision_to_it() {
    for (module, cmd) in install_lines() {
        for (vendor, board, want) in [
            (GIGABYTE, "X870E AORUS MASTER", "SUPPRESS"),
            (
                "ASUSTeK COMPUTER INC.",
                "ROG STRIX X670E-E GAMING WIFI",
                "LOAD",
            ),
        ] {
            let dmi = scratch("dmi");
            std::fs::write(dmi.join("board_vendor"), vendor).unwrap();
            std::fs::write(dmi.join("board_name"), board).unwrap();
            let (out, _) = run_install(&cmd, &guard_path(), Some(&dmi));
            std::fs::remove_dir_all(&dmi).ok();
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(
                stdout.trim().starts_with(&format!("{want} {module}")),
                "{module} on {board}: expected {want} from the guard, got {stdout:?} \
                 (stderr {:?})",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
