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
