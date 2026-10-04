//! Which firmware the daemon writes itself, and the file staged for it
//! (DEC-483).
//!
//! The daemon writes only a published OpenFAN release it knows by its SHA-256;
//! any other image is the user's to copy onto the board's drive, as in Phase 1.
//! The 2023 FW_01 binary is refused outright, whoever would write it: it is a
//! pre-production debug build that floods the serial link and drives no fan.
//!
//! A client uploads the file (`PUT /fans/openfan/firmware`); the daemon
//! fingerprints and parses it, says what it would do with it, and keeps it
//! when it would write it. A start that asks the daemon to write names that
//! file's SHA-256 and runs from a copy of it, so a later upload cannot change
//! what a running update writes.

use sha2::{Digest, Sha256};

use crate::serial::uf2::Image;

/// A published OpenFAN firmware release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Release {
    pub sha256: &'static str,
    pub size: u64,
    pub name: &'static str,
    /// Refused for any update.
    pub broken: bool,
}

/// The releases the daemon knows — the GUI's `KNOWN_RELEASES`, by the same
/// fingerprints.
pub const RELEASES: &[Release] = &[
    Release {
        sha256: "86187e7833cfccc6bb3f1d4bf4e7c03af2a154c17fa35123d73d7708f524ba7b",
        size: 85_504,
        name: "2023-09-29 release (FW_01)",
        broken: true,
    },
    Release {
        sha256: "6614f66db6754cb598665a5da2b263acef749db4952e3925eab43bf8329d1cc4",
        size: 79_360,
        name: "2026-09-13 release",
        broken: false,
    },
    Release {
        sha256: "79a3c951beb69ed5b30161ac34eb3e3d8ba70499b0761460028bf609da6feee0",
        size: 79_360,
        name: "2026-09-27 release",
        broken: false,
    },
];

/// The release with this SHA-256 (lower-case hex).
pub fn release(sha256: &str) -> Option<&'static Release> {
    release_in(RELEASES, sha256)
}

fn release_in(releases: &'static [Release], sha256: &str) -> Option<&'static Release> {
    releases.iter().find(|r| r.sha256 == sha256)
}

/// Whether no update may use this file.
pub fn is_known_broken(sha256: &str) -> bool {
    release(sha256).is_some_and(|r| r.broken)
}

/// `verdict` tokens on `PUT /fans/openfan/firmware`.
pub mod verdict {
    /// Staged: the daemon writes it itself when a start asks.
    pub const DAEMON_WRITE: &str = "daemon_write";
    /// Not staged: the user copies it onto the drive, as in Phase 1.
    pub const MANUAL_COPY: &str = "manual_copy";
    /// Not staged, and no update may use it.
    pub const REFUSED: &str = "refused";
}

/// `reason` tokens beside a verdict other than [`verdict::DAEMON_WRITE`].
pub mod reason {
    /// A valid image, but not a release the daemon knows.
    pub const UNKNOWN_BUILD: &str = "unknown_build";
    /// The daemon's own parse refused the image.
    pub const INVALID_IMAGE: &str = "invalid_image";
    /// FW_01.
    pub const KNOWN_BROKEN: &str = "firmware_known_broken";
}

/// The message for a refused FW_01, wherever it is refused.
pub const KNOWN_BROKEN_MESSAGE: &str = "this is the 2023 FW_01 binary, a pre-production debug \
     build that floods the serial link and drives no fan — use a 2026 release";

/// A file the daemon would write: a known release, parsed.
#[derive(Debug, Clone)]
pub struct Staged {
    pub sha256: String,
    pub release: &'static Release,
    pub image: Image,
}

/// What the daemon makes of an uploaded file.
#[derive(Debug, Clone)]
pub struct Assessment {
    pub sha256: String,
    pub size: u64,
    pub release: Option<&'static Release>,
    /// A [`verdict`] token.
    pub verdict: &'static str,
    /// A [`reason`] token, unless the verdict is [`verdict::DAEMON_WRITE`].
    pub reason: Option<&'static str>,
    pub message: String,
    /// The file, when the verdict is [`verdict::DAEMON_WRITE`].
    pub staged: Option<Staged>,
}

/// Lower-case hex SHA-256 of `data`.
pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Fingerprint, parse and judge `data`.
pub fn assess(data: &[u8]) -> Assessment {
    assess_against(data, RELEASES)
}

fn assess_against(data: &[u8], releases: &'static [Release]) -> Assessment {
    let sha256 = sha256_hex(data);
    let release = release_in(releases, &sha256);
    let not_staged = |verdict, reason, message: String| Assessment {
        sha256: sha256.clone(),
        size: data.len() as u64,
        release,
        verdict,
        reason: Some(reason),
        message,
        staged: None,
    };
    if release.is_some_and(|r| r.broken) {
        return not_staged(
            verdict::REFUSED,
            reason::KNOWN_BROKEN,
            KNOWN_BROKEN_MESSAGE.into(),
        );
    }
    let image = match Image::parse(data) {
        Ok(image) => image,
        Err(e) => {
            return not_staged(
                verdict::MANUAL_COPY,
                reason::INVALID_IMAGE,
                format!("Control-OFC will not write this file itself: {e}"),
            )
        }
    };
    let Some(known) = release else {
        return not_staged(
            verdict::MANUAL_COPY,
            reason::UNKNOWN_BUILD,
            "not a published OpenFAN release Control-OFC knows — it writes only those itself, so \
             this one is copied onto the board's drive by hand"
                .into(),
        );
    };
    Assessment {
        sha256: sha256.clone(),
        size: data.len() as u64,
        release,
        verdict: verdict::DAEMON_WRITE,
        reason: None,
        message: format!(
            "the {} — Control-OFC writes it itself and reads every byte back",
            known.name
        ),
        staged: Some(Staged {
            sha256,
            release: known,
            image,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::serial::uf2::fixture::image_file;

    #[test]
    fn the_table_is_well_formed_and_only_fw_01_is_broken() {
        for r in RELEASES {
            assert_eq!(r.sha256.len(), 64, "{}", r.name);
            assert!(r
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
            assert_eq!(r.size % 512, 0, "{}: whole UF2 blocks", r.name);
        }
        let broken: Vec<&str> = RELEASES
            .iter()
            .filter(|r| r.broken)
            .map(|r| r.name)
            .collect();
        assert_eq!(broken, ["2023-09-29 release (FW_01)"]);
        assert!(is_known_broken(RELEASES[0].sha256));
        assert!(!is_known_broken(RELEASES[2].sha256));
        assert!(!is_known_broken(&"00".repeat(32)));
    }

    #[test]
    fn a_valid_image_that_is_no_known_release_is_the_users_to_copy() {
        let a = assess(&image_file(4, 1));
        assert_eq!(a.verdict, verdict::MANUAL_COPY);
        assert_eq!(a.reason, Some(reason::UNKNOWN_BUILD));
        assert!(a.staged.is_none() && a.release.is_none());
        assert_eq!(a.size, 4 * 512);
        assert_eq!(a.sha256, sha256_hex(&image_file(4, 1)));
    }

    #[test]
    fn an_image_the_parser_refuses_is_never_staged() {
        let a = assess(b"not a uf2 file");
        assert_eq!(
            (a.verdict, a.reason),
            (verdict::MANUAL_COPY, Some(reason::INVALID_IMAGE))
        );
        assert!(a.message.contains("512-byte"), "{}", a.message);
        assert!(a.staged.is_none());
    }

    /// A table holding `data`'s fingerprint, as the real one holds a release's.
    fn table_with(data: &[u8], broken: bool) -> &'static [Release] {
        let sha: &'static str = Box::leak(sha256_hex(data).into_boxed_str());
        Box::leak(Box::new([Release {
            sha256: sha,
            size: data.len() as u64,
            name: "test release",
            broken,
        }]))
    }

    #[test]
    fn a_known_release_is_staged_with_its_image() {
        let data = image_file(20, 5);
        let a = assess_against(&data, table_with(&data, false));
        assert_eq!((a.verdict, a.reason), (verdict::DAEMON_WRITE, None));
        assert_eq!(a.release.map(|r| r.name), Some("test release"));
        let staged = a.staged.expect("staged");
        assert_eq!(staged.sha256, a.sha256);
        assert_eq!(staged.image, Image::parse(&data).unwrap());
        assert!(a.message.contains("test release"), "{}", a.message);
    }

    #[test]
    fn a_broken_release_is_refused_whatever_it_parses_as() {
        let data = image_file(20, 5);
        let a = assess_against(&data, table_with(&data, true));
        assert_eq!(
            (a.verdict, a.reason),
            (verdict::REFUSED, Some(reason::KNOWN_BROKEN))
        );
        assert!(a.staged.is_none());
        assert_eq!(a.message, KNOWN_BROKEN_MESSAGE);
    }

    /// Opt-in, as the GUI's own real-file test: with `OFC_FIRMWARE_DIR` naming
    /// a folder of `.uf2` files, FW_01 is refused, every other published
    /// release is staged with a page for each of its blocks, and no other file
    /// is ever staged. Without it there is nothing to read.
    #[test]
    fn real_firmware_files_are_judged_by_their_fingerprint() {
        let Some(dir) = std::env::var_os("OFC_FIRMWARE_DIR") else {
            return;
        };
        let mut seen = 0;
        for entry in std::fs::read_dir(dir).expect("the folder") {
            let path = entry.expect("an entry").path();
            if path.extension().is_none_or(|e| e != "uf2") {
                continue;
            }
            let data = std::fs::read(&path).expect("the file");
            let a = assess(&data);
            seen += 1;
            match a.release {
                Some(r) if r.broken => assert_eq!(a.verdict, verdict::REFUSED, "{path:?}"),
                Some(r) => {
                    assert_eq!(a.verdict, verdict::DAEMON_WRITE, "{path:?}: {}", a.message);
                    assert_eq!(a.size, r.size, "{path:?}");
                    let staged = a.staged.expect("a release is staged");
                    assert_eq!(staged.image.page_count(), data.len() / 512, "{path:?}");
                }
                None => assert_ne!(a.verdict, verdict::DAEMON_WRITE, "{path:?}"),
            }
        }
        assert!(seen > 0, "precondition: the folder holds firmware files");
    }

    #[test]
    fn the_digest_is_sha_256() {
        // FIPS 180-2's "abc" vector.
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
