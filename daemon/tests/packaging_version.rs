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
