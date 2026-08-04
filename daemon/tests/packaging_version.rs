//! Guards against version drift between the crate and the AUR package.
//!
//! The release workflow (`.github/workflows/release.yml`) asserts both
//! `daemon/Cargo.toml`'s version and `packaging/PKGBUILD`'s `pkgver` against the
//! pushed git tag. This test pins the two source files to each other so a drift
//! fails `cargo test` at commit time — before a tag is ever pushed — mirroring
//! the GUI's pkgver-vs-source check.

#[test]
fn cargo_version_matches_pkgbuild_pkgver() {
    let pkgbuild = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../packaging/PKGBUILD"
    ))
    .expect("read PKGBUILD");
    let pkgver = pkgbuild
        .lines()
        .find_map(|l| l.strip_prefix("pkgver="))
        .expect("pkgver= line")
        .trim();
    assert_eq!(
        pkgver,
        env!("CARGO_PKG_VERSION"),
        "packaging/PKGBUILD pkgver ({pkgver}) != daemon/Cargo.toml version ({})",
        env!("CARGO_PKG_VERSION")
    );
}

/// DEC-199 regression: the systemd sandbox's writable sysfs carve-out must
/// target the device tree (`/sys/devices`), not the `/sys/class/{hwmon,drm}`
/// symlink directories.
///
/// `ProtectKernelTunables=true` remounts all of `/sys` read-only. sysfs decides
/// writability by the symlink *target* inode's mount, so a carve-out of the
/// class dirs re-exposes the symlinks but leaves the real `pwm*`/`pwm*_enable`
/// and GPU `fan_curve` inodes (all under `/sys/devices`) read-only — every fan
/// write then fails with EROFS ("Read-only file system", os error 30). This
/// test fails if someone reverts to the ineffective class-level carve-out.
#[test]
fn service_readwritepaths_covers_device_tree() {
    let service = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../packaging/control-ofc-daemon.service"
    ))
    .expect("read control-ofc-daemon.service");

    let has_devices = service
        .lines()
        .any(|l| l.trim_start().starts_with("ReadWritePaths=") && l.contains("/sys/devices"));
    assert!(
        has_devices,
        "systemd unit ReadWritePaths= must cover /sys/devices: the pwm*/pwm*_enable \
         and GPU fan_curve nodes the daemon writes are symlinks from /sys/class/* into \
         /sys/devices, and ProtectKernelTunables=true remounts /sys read-only — carving \
         out only the /sys/class/* symlink dirs leaves the real inodes read-only and every \
         fan write fails with EROFS (DEC-199)."
    );
}

// ---------------------------------------------------------------------------
// DEC-239 — the GitHub Release carries the clean-room package as an asset so
// `pacman -U` is a complete install path while the AUR is read-only (the
// 2026-08-02 freeze stranded the GUI's v2.34.0 for over a day). Each failure
// mode guarded below yields a *green* release that silently ships no usable
// asset — not noticed until the AUR is down and the fallback is needed.
//
// Parsed by hand rather than with a YAML crate: serde_yaml is archived, and a
// dependency added purely for one guard test is a worse trade than the small
// amount of string handling here. The helpers are indentation-independent.
// ---------------------------------------------------------------------------

fn release_workflow() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.github/workflows/release.yml"
    ))
    .expect("read .github/workflows/release.yml")
}

/// Extract one job's body. Jobs are the only keys at exactly two-space indent
/// inside `jobs:`; the body is every deeper-indented line until the next one.
fn job_block(wf: &str, job: &str) -> String {
    let mut out = String::new();
    let mut in_job = false;
    for line in wf.lines() {
        let is_key_at_job_depth =
            line.starts_with("  ") && !line.starts_with("   ") && line.trim_end().ends_with(':');
        if is_key_at_job_depth {
            in_job = line.trim() == format!("{job}:");
            continue;
        }
        if in_job {
            out.push_str(line);
            out.push('\n');
        }
    }
    assert!(!out.is_empty(), "no `{job}:` job found in release.yml");
    out
}

/// The `name:` value of the `with:` block belonging to `action`.
fn artifact_name_for(block: &str, action: &str) -> String {
    let mut lines = block.lines();
    while let Some(line) = lines.next() {
        if line.contains(action) {
            for follow in lines.by_ref().take(4) {
                if let Some(value) = follow.trim().strip_prefix("name:") {
                    return value.trim().to_string();
                }
            }
            panic!("no `name:` within 4 lines of `{action}` in release.yml");
        }
    }
    panic!("`{action}` not found in release.yml");
}

/// The artifact name must agree across jobs or the Release ships no package.
/// A typo on either side still builds green; the breakage only shows up at
/// release time, and the `files:` glob then resolves to nothing.
#[test]
fn release_artifact_name_matches_between_upload_and_download() {
    let wf = release_workflow();
    let upload = artifact_name_for(&job_block(&wf, "build-test"), "actions/upload-artifact@");
    let download = artifact_name_for(
        &job_block(&wf, "github-release"),
        "actions/download-artifact@",
    );
    assert_eq!(
        upload, download,
        "artifact name drift between build-test upload ({upload}) and github-release \
         download ({download}) — the Release would carry no package (DEC-239)"
    );
}

/// github-release must gate on build-test. Ordering: otherwise the jobs race
/// and the download can run before the package exists. Integrity: the attached
/// asset must be the artifact the clean-room build (a full `cargo build
/// --release` + `cargo test`) actually verified, and a PKGBUILD that does not
/// build must not produce a Release at all.
#[test]
fn github_release_gates_on_clean_room_build() {
    let block = job_block(&release_workflow(), "github-release");
    assert!(
        block.lines().any(|l| l.trim() == "needs: build-test"),
        "github-release must declare `needs: build-test` (DEC-239) — without it the job \
         races the build and can publish an unverified or missing asset"
    );
}

/// The Release step must attach the package, not create an empty Release —
/// otherwise the AUR-free install path documented in README.md is a dead link.
#[test]
fn release_attaches_the_built_package() {
    let block = job_block(&release_workflow(), "github-release");
    assert!(
        block
            .lines()
            .any(|l| l.trim().starts_with("files:") && l.contains("pkg.tar.zst")),
        "the Release step must attach the built *.pkg.tar.zst (DEC-239)"
    );
}

/// `actions/attest-build-provenance` fails at runtime without `id-token: write`
/// (keyless Sigstore) and `attestations: write`. Dropping either turns the
/// `gh attestation verify` command documented in README.md into a lie.
#[test]
fn release_attests_build_provenance_with_required_permissions() {
    let block = job_block(&release_workflow(), "github-release");
    assert!(
        block.contains("actions/attest-build-provenance@"),
        "github-release must attest the package's build provenance (DEC-239)"
    );
    for permission in ["id-token: write", "attestations: write", "contents: write"] {
        assert!(
            block.lines().any(|l| l.trim().starts_with(permission)),
            "github-release needs `{permission}` for provenance attestation (DEC-239)"
        );
    }
}

/// A mutable tag on a step holding `contents: write` and `id-token: write` is a
/// supply-chain hole: an upstream tag move would run unreviewed code able to
/// sign artifacts and publish Releases under this repo's name.
#[test]
fn release_actions_are_pinned_to_a_sha() {
    let wf = release_workflow();
    let unpinned: Vec<&str> = wf
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("uses:") || l.starts_with("- uses:"))
        .filter(|l| {
            let reference = l.rsplit('@').next().unwrap_or("");
            // strip the trailing `# vX.Y.Z` comment before judging the ref
            let reference = reference.split('#').next().unwrap_or("").trim();
            !(reference.len() == 40 && reference.chars().all(|c| c.is_ascii_hexdigit()))
        })
        .collect();
    assert!(
        unpinned.is_empty(),
        "release.yml actions must be pinned to a 40-char commit SHA; found {unpinned:?}"
    );
}

// ---------------------------------------------------------------------------
// DEC-240 — the AUR is retired as a publishing channel. `aur-publish` is kept
// in the workflow but gated to a manual `workflow_dispatch`, so a release is
// never reddened by a third party being down. Both guards below protect the
// *silent* failure modes of that change.
// ---------------------------------------------------------------------------

/// `aur-publish` must stay gated to a manual dispatch. Dropping the `if:`
/// restores the pre-DEC-240 behaviour where an upstream AUR outage turns an
/// otherwise-complete release red — the failure that burned four release
/// attempts on the GUI's v2.34.0. Nothing else in the workflow breaks if this
/// condition is removed, so only this test catches it.
#[test]
fn aur_publish_never_runs_on_a_tag_push() {
    let block = job_block(&release_workflow(), "aur-publish");
    assert!(
        block
            .lines()
            .any(|l| l.trim() == "if: github.event_name == 'workflow_dispatch'"),
        "aur-publish must be gated to `if: github.event_name == 'workflow_dispatch'` \
         (DEC-240) so it never runs on a tag push"
    );
}

/// The pkgver / Cargo.toml version guards must live in `build-test`.
///
/// They originally sat in `aur-publish`. Because that job no longer runs on a
/// tag push (DEC-240), leaving them there would let a forgotten version bump
/// produce a GitHub Release whose attached package disagrees with its own tag —
/// green, published, and wrong. `build-test` runs on both paths and gates
/// `github-release`, so the guards belong there.
#[test]
fn version_guards_run_on_the_tag_push_path() {
    let block = job_block(&release_workflow(), "build-test");
    assert!(
        block.contains("packaging/PKGBUILD") && block.contains("pkgver="),
        "build-test must verify packaging/PKGBUILD pkgver against the tag (DEC-240) — \
         it is the only job on the tag-push path that can catch a missed bump"
    );
    assert!(
        block.contains("daemon/Cargo.toml"),
        "build-test must verify daemon/Cargo.toml's version against the tag (DEC-240)"
    );
    assert!(
        block.contains("RELEASE_TAG"),
        "build-test must expose RELEASE_TAG for the version guards to compare against"
    );
}
