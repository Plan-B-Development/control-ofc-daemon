//! Guards on the release chain: clean-room build -> GitHub Release -> pacman repo.
//!
//! Originally "version drift between the crate and the AUR package"; the AUR was
//! retired as a publishing channel at DEC-240 and every guard here is now about
//! the chain that actually publishes (DEC-239/241/263). Renamed because the stale
//! title invited the conclusion that the file was dead weight.
//!
//! The release workflow (`.github/workflows/release.yml`) asserts both
//! `daemon/Cargo.toml`'s version and `packaging/PKGBUILD`'s `pkgver` against the
//! pushed git tag. These tests pin the same facts at commit time — before a tag is
//! ever pushed — mirroring the GUI's checks.

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

/// The dependency names in a job's `needs:`, accepting either the scalar form
/// (`needs: build-test`) or the flow-sequence form (`needs: [a, b]`).
///
/// DEC-263: this used to be a literal `l.trim() == "needs: build-test"` match,
/// which is a different assertion from the one the doc comment claimed — adding a
/// second, *stricter* gate to the same job broke it. A guard that fails when the
/// thing it guards gets stronger teaches you to weaken it.
fn job_needs(wf: &str, job: &str) -> Vec<String> {
    let block = job_block(wf, job);
    let raw = block
        .lines()
        .find_map(|l| l.trim().strip_prefix("needs:").map(str::trim))
        .unwrap_or("")
        .to_string();
    raw.trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches(['\'', '"']).to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// github-release must gate on build-test. Ordering: otherwise the jobs race
/// and the download can run before the package exists. Integrity: the attached
/// asset must be the artifact the clean-room build (a full `cargo build
/// --release` + `cargo test`) actually verified, and a PKGBUILD that does not
/// build must not produce a Release at all.
#[test]
fn github_release_gates_on_clean_room_build() {
    let needs = job_needs(&release_workflow(), "github-release");
    assert!(
        needs.iter().any(|n| n == "build-test"),
        "github-release must declare `needs: build-test` (DEC-239) — without it the job \
         races the build and can publish an unverified or missing asset; got needs={needs:?}"
    );
}

/// github-release must also gate on the tagged commit's CI result.
///
/// DEC-263: `build-test` proves the package *assembles*; it runs no test suite.
/// Until this gate existed the two were unrelated, and the GUI's v2.41.0 published
/// with every test leg red. Pinned here because the failure mode is silent — the
/// Release still succeeds, so nothing surfaces the loss if this job is dropped.
#[test]
fn github_release_gates_on_a_green_test_suite() {
    let wf = release_workflow();
    let needs = job_needs(&wf, "github-release");
    assert!(
        needs.iter().any(|n| n == "ci-green"),
        "github-release must declare `needs: ci-green` (DEC-263) — build-test only proves \
         the package builds, so without this a red test suite can still publish; \
         got needs={needs:?}"
    );
    let block = job_block(&wf, "ci-green");
    assert!(
        block.contains("actions/workflows/ci.yml/runs"),
        "ci-green must resolve the tagged commit's ci.yml run via the Checks API (DEC-263)"
    );
    assert!(
        block.contains("head_sha=$SHA"),
        "ci-green must query CI for THIS commit — a query that is not pinned to the tagged \
         SHA would pass on some other commit's green run (DEC-263)"
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

/// The settle wait must exist AND run before the dispatch (DEC-248).
///
/// pacman-repo's publish is declarative: it assembles from whatever is the
/// latest Release of each source project at the moment it runs. In a coordinated
/// GUI + daemon release the two `release.yml` runs finish at different times, so
/// dispatching the instant this one finishes can publish THIS new package paired
/// with the OTHER project's previous one, and serve that pair as current.
///
/// A timing heuristic, not a guarantee — which is exactly why it needs a guard:
/// nothing else in the workflow breaks if the wait is deleted or moved after the
/// dispatch, and a wait that runs *afterwards* does nothing at all while still
/// looking present in a diff. The GUI has carried this test since DEC-248
/// (`test_notify_repo_settles_before_dispatching`); the daemon's half of the
/// same change shipped without one.
#[test]
fn notify_repo_settles_before_dispatching() {
    let block = job_block(&release_workflow(), "notify-repo");
    let lines: Vec<&str> = block.lines().collect();

    let sleep_at = lines
        .iter()
        .position(|l| l.contains("sleep "))
        .expect("notify-repo must wait before dispatching so a paired cross-stack release can land first (DEC-248); no sleep step found");
    let dispatch_at = lines
        .iter()
        .position(|l| l.contains("dispatches"))
        .expect("notify-repo must dispatch to pacman-repo");

    assert!(
        sleep_at < dispatch_at,
        "the settle wait must come BEFORE the dispatch — waiting afterwards does \
         nothing at all. sleep at line {sleep_at}, dispatch at line {dispatch_at}"
    );
}

/// The settle window must stay strong enough to be worth having.
///
/// History: this was a fixed `sleep 180`, and this test pinned that number's order
/// of magnitude. The fixed wait was then measured to be **inadequate** — on
/// 2026-08-17 the two `notify-repo` jobs fired 9 m 29 s apart, so publish #1
/// assembled daemon 2.18.0 with GUI 2.41.0 and `verify.yml` reported that pair
/// green, because the GUI Release object did not exist yet. 180 s was never going
/// to cover a nine-minute skew.
///
/// So the mechanism is now a **poll for the peer's release run**, and this test
/// pins that instead. It deliberately asserts BOTH halves, because either one
/// alone can be deleted while the step still looks correct in a diff:
///
/// 1. the step actually polls the peer's `release.yml` runs, and
/// 2. the fail-open fallback is still a substantial wait, not a token one.
///
/// The plausible future edits this guards against are "drop the poll, it is
/// slow" (which restores the mismatched-pair bug) and "trim the fallback to a few
/// seconds" (which removes the protection whenever the API call fails).
#[test]
fn the_settle_window_is_not_trimmed_to_nothing() {
    let block = job_block(&release_workflow(), "notify-repo");

    assert!(
        block.contains("actions/workflows/release.yml/runs"),
        "notify-repo must POLL for a paired release run before dispatching — a fixed \
         sleep was measured insufficient (9m29s observed skew vs a 180s wait, which \
         published a mismatched pair behind a green verify.yml)"
    );
    assert!(
        block.contains("PEER"),
        "notify-repo's poll must name the peer repository it waits on"
    );

    // The fail-open fallback: any API problem must degrade to the old fixed wait,
    // never to no wait at all.
    let fallback_secs: u32 = block
        .lines()
        .find_map(|l| l.trim().strip_prefix("sleep "))
        .and_then(|v| v.trim().parse().ok())
        .expect(
            "notify-repo must retain a plain `sleep <seconds>` fallback for when the \
             peer poll cannot run — failing open with no wait at all reintroduces the \
             mismatched-pair window",
        );
    assert!(
        fallback_secs >= 120,
        "the fail-open fallback is {fallback_secs}s; it stands in for the old fixed \
         window, so anything much shorter is not a fallback (DEC-248)"
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

// ---------------------------------------------------------------------------
// DEC-241 — the [control-ofc] pacman repository is what makes `pacman -Syu`
// upgrade this package. `notify-repo` is the only thing that tells it a new
// release exists. Every failure below is SILENT: the release goes green, the
// Release object is correct, and users simply never receive the update.
// ---------------------------------------------------------------------------

/// The pacman repository must be told about a release, on the tag path.
#[test]
fn notify_repo_runs_on_a_tag_push() {
    let block = job_block(&release_workflow(), "notify-repo");
    assert!(
        block
            .lines()
            .any(|l| l.trim() == "if: github.event_name == 'push'"),
        "notify-repo must be gated to tag pushes (DEC-241) so the manual AUR path \
         does not also trigger a rebuild"
    );
}

/// `needs: github-release` is correctness, not ordering aesthetics.
///
/// The assembler downloads `*.pkg.tar.zst` from this repo's *latest* Release.
/// If notify-repo fires before github-release has created it, the rebuild picks
/// up the PREVIOUS version and republishes it as current — a stale package
/// served to every user, with a fully green release run.
#[test]
fn notify_repo_waits_for_the_release_to_exist() {
    let block = job_block(&release_workflow(), "notify-repo");
    assert!(
        block.lines().any(|l| l.trim() == "needs: github-release"),
        "notify-repo must declare `needs: github-release` (DEC-241) — firing early \
         rebuilds the pacman repo around the previous version"
    );
}

/// Endpoint and credential must both be right, and neither fails loudly. The
/// ambient GITHUB_TOKEN cannot dispatch to another repository, so a swap to
/// `github.token` yields a 404 that looks like a typo.
#[test]
fn notify_repo_targets_the_pacman_repo_with_a_cross_repo_token() {
    let block = job_block(&release_workflow(), "notify-repo");
    assert!(
        block.contains("repos/Plan-B-Development/pacman-repo/dispatches"),
        "notify-repo must POST to the pacman-repo dispatches endpoint (DEC-241)"
    );
    assert!(
        block.contains("package-released"),
        "the dispatch event_type must be `package-released` — publish.yml listens \
         for exactly that type and ignores anything else"
    );
    assert!(
        block.contains("PACMAN_REPO_TOKEN"),
        "notify-repo must authenticate with the cross-repo PACMAN_REPO_TOKEN; the \
         ambient GITHUB_TOKEN cannot dispatch across repositories"
    );
}

/// [RELEASE] A nightly CI run must never be able to veto a release (DEC-270).
///
/// `ci-green` gates publication on the newest `ci.yml` run for the tagged SHA.
/// `ci.yml` also runs on a nightly `schedule`, and that leg deliberately runs a
/// WIDER matrix than the per-push path — it adds py3.14 and restores the canary's
/// full loop count, neither `continue-on-error`. So without a filter the newest
/// run for a tagged commit can be last night's cron, and a failure on a leg the
/// push path never runs would block a release whose own CI was green.
///
/// Conditional on purpose, and currently INERT in this repo: the nightly lives in
/// the GUI's `ci.yml`, not this one, so `has_nightly` is false here and the test
/// returns early. It is kept because the two `release.yml` files are deliberately
/// near-identical and the filter is carried for symmetry — this test is what makes
/// the requirement fire automatically if a nightly is ever added here, instead of
/// the gate silently becoming vetoable again.
#[test]
fn a_nightly_ci_run_cannot_veto_a_release() {
    let ci = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.github/workflows/ci.yml"
    ))
    .expect("read .github/workflows/ci.yml");

    let has_nightly = ci
        .lines()
        .any(|l| l.trim_end() == "  schedule:" || l.trim() == "schedule:");
    if !has_nightly {
        return;
    }

    let block = job_block(&release_workflow(), "ci-green");
    // Strip comments: the step explains *why* `event=push` is the wrong filter, so
    // the negative check below would otherwise match that explanation.
    let code: String = block
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        code.contains(r#"select(.event != "schedule")"#),
        "ci.yml has a nightly `schedule` whose matrix is wider than the push path, so \
         ci-green must exclude scheduled runs when picking the newest run for the \
         tagged SHA — otherwise a py3.14-only nightly failure vetoes a green release"
    );
    assert!(
        !code.contains("event=push"),
        "filter on `.event != \"schedule\"`, not `event=push`: an operator \
         re-dispatching ci.yml produces a `workflow_dispatch` run, and that is the \
         documented escape hatch for tagging a docs-only commit"
    );
}

// ---------------------------------------------------------------------------
// Release back-stops. Both failure modes below are invisible locally and only
// bite AFTER the tag is public, forcing a delete-and-retag: `ci.yml` runs plain
// `cargo clippy`/`cargo test` (no `--locked`), and the CHANGELOG is only read by
// `release.yml`'s note extraction. The GUI has carried the CHANGELOG guard since
// DEC-239; the daemon shipped without either.
// ---------------------------------------------------------------------------

/// CI must run cargo with `--locked`, which is the ONLY thing that catches a
/// stale `Cargo.lock` before the tag is public.
///
/// This replaces a test that compared `Cargo.lock`'s recorded version against
/// `CARGO_PKG_VERSION` directly. That test could not fail: absent
/// `--locked`/`--frozen`/`--offline`, cargo re-resolves and REWRITES the lockfile
/// on disk before compiling, so by the time the test read the file, the very
/// invocation running it had already repaired the thing it was checking.
/// Measured: bump `daemon/Cargo.toml` to 2.21.0, leave the lock at 2.20.0, run
/// `cargo test` — the guard reported ok and the lock on disk had become 2.21.0.
///
/// The real failure it was written for: `packaging/PKGBUILD` fetches `--locked`
/// and builds/tests `--frozen`, so a lock whose own entry lags the manifest dies
/// inside `makepkg` — in `build-test`, which runs only on the tag-push path,
/// after the tag exists. `prepare()`'s `cargo fetch --locked` is therefore the
/// existing net, and it is a LATE one; `--locked` in CI moves it earlier.
#[test]
fn ci_runs_cargo_locked_so_a_stale_lockfile_cannot_reach_a_tag() {
    let ci = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../.github/workflows/ci.yml"
    ))
    .expect("read .github/workflows/ci.yml");

    let build_steps: Vec<&str> = ci
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("run: cargo "))
        // fmt reads no manifest, so a lockfile cannot be stale for it.
        .filter(|l| !l.contains("cargo fmt"))
        .collect();

    assert!(
        !build_steps.is_empty(),
        "expected ci.yml to run cargo; found none"
    );
    let unlocked: Vec<&&str> = build_steps
        .iter()
        .filter(|l| !l.contains("--locked") && !l.contains("--frozen"))
        .collect();
    assert!(
        unlocked.is_empty(),
        "every cargo build/test step in ci.yml must pass --locked, or cargo silently \
         rewrites Cargo.lock and a forgotten `cargo update -w` reaches the tag and \
         fails the clean-room build afterwards; unlocked steps: {unlocked:?}"
    );
}

/// `CHANGELOG.md` must carry a `## [X.Y.Z]` section for the current version.
///
/// `release.yml`'s `github-release` job extracts the Release notes from the
/// section matching the pushed tag and FAILS when there is none — by which point
/// the tag is public, no Release object exists, `notify-repo` never fires, and
/// the pacman repository goes on serving the previous version from a run that
/// looked fine until its last job.
#[test]
fn changelog_has_a_section_for_the_current_version() {
    let changelog =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../CHANGELOG.md"))
            .expect("read CHANGELOG.md");
    let version = env!("CARGO_PKG_VERSION");
    let heading = format!("## [{version}]");

    let start = changelog.lines().position(|l| l.starts_with(&heading));
    assert!(
        start.is_some(),
        "CHANGELOG.md has no '{heading}' section but daemon/Cargo.toml is at {version}. \
         CI extracts the GitHub Release notes from that section and github-release fails \
         without it — after the tag is already pushed. Add the section before tagging."
    );

    // A heading alone is not enough: `release.yml` also fails on an EMPTY section
    // (`if [ ! -s release-notes.md ]`), which is just as late and just as fatal.
    // Reachable by writing the new heading directly above the previous one.
    let body_lines = changelog
        .lines()
        .skip(start.unwrap() + 1)
        .take_while(|l| !l.starts_with("## ["))
        .filter(|l| !l.trim().is_empty())
        .count();
    assert!(
        body_lines > 0,
        "CHANGELOG.md's '{heading}' section is empty. release.yml extracts the Release \
         notes from it and fails when the result has no content — after the tag is \
         public, so no Release is created and notify-repo never fires."
    );
}
