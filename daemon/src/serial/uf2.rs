//! A UF2 firmware image, parsed for the daemon to write (DEC-483).
//!
//! The GUI checks a file before an update and shows what it found; this parse
//! is the daemon's own, of the bytes it is about to write. It refuses anything
//! that could write outside the board's flash or say two things about one page.
//! Which release a file is — and so whether the daemon writes it at all — is
//! the fingerprint's job ([`crate::openfan_maintenance::firmware`]), not this
//! one's.
//!
//! An RP2040 image (<https://github.com/microsoft/uf2>): 512-byte blocks, each
//! one 256-byte page of main flash, numbered in order with one total, carrying
//! the RP2040 family id; 256-byte-aligned addresses inside the board's 4 MiB
//! flash, none repeated, the lowest at the start of flash.

use std::collections::BTreeMap;
use std::fmt;

pub const BLOCK_SIZE: usize = 512;
/// One block's payload: one flash page.
pub const PAGE_SIZE: usize = 256;
/// The flash erase unit.
pub const SECTOR_SIZE: u32 = 4096;
/// Where the RP2040 maps its flash.
pub const FLASH_BASE: u32 = 0x1000_0000;
/// The OpenFAN board's W25Q32. The bootloader wraps an address past the chip's
/// size onto its start, so nothing past this is ever written.
pub const FLASH_SIZE: u32 = 4 * 1024 * 1024;
/// The largest file taken: the GUI's bound, and the start request's.
pub const MAX_FILE_BYTES: usize = 1024 * 1024;
pub const RP2040_FAMILY_ID: u32 = 0xE48B_FF56;

const MAGIC_START0: u32 = 0x0A32_4655;
const MAGIC_START1: u32 = 0x9E5D_5157;
const MAGIC_END: u32 = 0x0AB1_6F30;
/// The only flag an RP2040 image carries. Any other — "not main flash",
/// "file container", "MD5 present" — is refused.
const FLAG_FAMILY_ID_PRESENT: u32 = 0x0000_2000;
const HEADER_LEN: usize = 32;

/// Why a file is not an image the daemon writes. The message is for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uf2Error(pub String);

impl fmt::Display for Uf2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Uf2Error {}

/// A contiguous run of the image's pages inside one sector: one WRITE, and one
/// READ to check it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub addr: u32,
    pub data: Vec<u8>,
}

/// A flash sector the image touches: erased whole, then its runs programmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sector {
    pub base: u32,
    pub runs: Vec<Run>,
}

impl Sector {
    /// The image's bytes in this sector.
    pub fn bytes(&self) -> u64 {
        self.runs.iter().map(|r| r.data.len() as u64).sum()
    }
}

/// A firmware image: its pages by address, every one inside the flash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pages: BTreeMap<u32, Vec<u8>>,
}

fn word(block: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
}

impl Image {
    /// Parse `data`, refusing it at the first problem.
    pub fn parse(data: &[u8]) -> Result<Self, Uf2Error> {
        let fail = |m: String| Err(Uf2Error(m));
        if data.is_empty() {
            return fail("the file is empty".into());
        }
        if data.len() > MAX_FILE_BYTES {
            return fail(format!("the file is larger than {MAX_FILE_BYTES} bytes"));
        }
        if !data.len().is_multiple_of(BLOCK_SIZE) {
            return fail("the file is not a whole number of 512-byte UF2 blocks".into());
        }
        let count = data.len() / BLOCK_SIZE;
        let last_page = u64::from(FLASH_BASE) + u64::from(FLASH_SIZE) - PAGE_SIZE as u64;
        let mut pages = BTreeMap::new();
        // The length is a whole number of blocks, so nothing is left over.
        for (index, block) in data.as_chunks::<BLOCK_SIZE>().0.iter().enumerate() {
            let n = index + 1;
            let (flags, addr, size) = (word(block, 8), word(block, 12), word(block, 16));
            let (number, total, family) = (word(block, 20), word(block, 24), word(block, 28));
            if (word(block, 0), word(block, 4), word(block, BLOCK_SIZE - 4))
                != (MAGIC_START0, MAGIC_START1, MAGIC_END)
            {
                return fail(format!("block {n} is not a UF2 block"));
            }
            if flags != FLAG_FAMILY_ID_PRESENT {
                return fail(format!(
                    "block {n} has flags {flags:#010x}; an RP2040 flash image has only the \
                     family flag"
                ));
            }
            if family != RP2040_FAMILY_ID {
                return fail(format!(
                    "block {n} is for chip family {family:#010x}, not the RP2040"
                ));
            }
            if size as usize != PAGE_SIZE {
                return fail(format!(
                    "block {n} carries {size} bytes, not one {PAGE_SIZE}-byte page"
                ));
            }
            if number as usize != index || total as usize != count {
                return fail(format!(
                    "block {n} is numbered {} of {total}; the file holds {count} blocks in order",
                    u64::from(number) + 1
                ));
            }
            if !(addr as usize).is_multiple_of(PAGE_SIZE) {
                return fail(format!(
                    "block {n}'s address {addr:#010x} is not page-aligned"
                ));
            }
            if u64::from(addr) < u64::from(FLASH_BASE) || u64::from(addr) > last_page {
                return fail(format!(
                    "block {n}'s address {addr:#010x} is outside the board's flash"
                ));
            }
            let payload = block[HEADER_LEN..HEADER_LEN + PAGE_SIZE].to_vec();
            if pages.insert(addr, payload).is_some() {
                return fail(format!("block {n} writes {addr:#010x} again"));
            }
        }
        if pages.keys().next() != Some(&FLASH_BASE) {
            return fail("the image does not start at the start of flash".into());
        }
        Ok(Self { pages })
    }

    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// The bytes the image writes.
    pub fn byte_count(&self) -> u64 {
        (self.pages.len() * PAGE_SIZE) as u64
    }

    /// The sectors the image touches, in address order, each with its runs of
    /// consecutive pages. A page the image leaves out stays erased (`0xff`),
    /// as a copy onto the drive would leave it.
    pub fn sectors(&self) -> Vec<Sector> {
        let mut sectors: Vec<Sector> = Vec::new();
        for (&addr, page) in &self.pages {
            let base = addr - addr % SECTOR_SIZE;
            if sectors.last().is_none_or(|s| s.base != base) {
                sectors.push(Sector {
                    base,
                    runs: Vec::new(),
                });
            }
            let sector = sectors.last_mut().expect("pushed above");
            match sector.runs.last_mut() {
                Some(run) if run.addr + run.data.len() as u32 == addr => {
                    run.data.extend_from_slice(page)
                }
                _ => sector.runs.push(Run {
                    addr,
                    data: page.clone(),
                }),
            }
        }
        sectors
    }
}

#[cfg(test)]
pub(crate) mod fixture {
    //! UF2 files built in the test, block by block.
    use super::*;

    /// One block: `payload` (padded to a page) at `addr`, numbered `n` of `total`.
    pub fn block(addr: u32, n: u32, total: u32, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; BLOCK_SIZE];
        for (at, v) in [
            (0, MAGIC_START0),
            (4, MAGIC_START1),
            (8, FLAG_FAMILY_ID_PRESENT),
            (12, addr),
            (16, PAGE_SIZE as u32),
            (20, n),
            (24, total),
            (28, RP2040_FAMILY_ID),
            (BLOCK_SIZE - 4, MAGIC_END),
        ] {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[HEADER_LEN..HEADER_LEN + payload.len().min(PAGE_SIZE)]
            .copy_from_slice(&payload[..payload.len().min(PAGE_SIZE)]);
        b
    }

    /// A file writing one page at each address, in order; page `i` is filled
    /// with `seed + i`.
    pub fn file(addrs: &[u32], seed: u8) -> Vec<u8> {
        let total = addrs.len() as u32;
        addrs
            .iter()
            .enumerate()
            .flat_map(|(i, &a)| block(a, i as u32, total, &[seed.wrapping_add(i as u8); PAGE_SIZE]))
            .collect()
    }

    /// `pages` consecutive pages from the start of flash.
    pub fn image_file(pages: u32, seed: u8) -> Vec<u8> {
        let addrs: Vec<u32> = (0..pages)
            .map(|p| FLASH_BASE + p * PAGE_SIZE as u32)
            .collect();
        file(&addrs, seed)
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::{block, file, image_file};
    use super::*;

    fn refused(data: &[u8]) -> String {
        Image::parse(data).expect_err("refused").0
    }

    #[test]
    fn a_contiguous_image_reads_back_as_its_pages() {
        let image = Image::parse(&image_file(20, 7)).expect("valid");
        assert_eq!(image.page_count(), 20);
        assert_eq!(image.byte_count(), 20 * PAGE_SIZE as u64);
        let sectors = image.sectors();
        assert_eq!(
            sectors.iter().map(|s| s.base).collect::<Vec<_>>(),
            [FLASH_BASE, FLASH_BASE + SECTOR_SIZE],
            "16 pages fill the first sector, 4 start the second"
        );
        assert_eq!(sectors[0].runs.len(), 1);
        assert_eq!(sectors[0].runs[0].data.len(), SECTOR_SIZE as usize);
        assert_eq!(sectors[1].bytes(), 4 * PAGE_SIZE as u64);
        assert_eq!(
            sectors[1].runs[0].data[0],
            7 + 16,
            "page 16's fill, in place"
        );
    }

    #[test]
    fn a_gap_inside_a_sector_makes_two_runs_and_a_far_page_its_own_sector() {
        let page = PAGE_SIZE as u32;
        let image = Image::parse(&file(
            &[
                FLASH_BASE,
                FLASH_BASE + page,
                FLASH_BASE + 3 * page,
                0x1020_0000,
            ],
            1,
        ))
        .expect("valid");
        let sectors = image.sectors();
        assert_eq!(sectors.len(), 2);
        assert_eq!(
            sectors[0]
                .runs
                .iter()
                .map(|r| (r.addr, r.data.len()))
                .collect::<Vec<_>>(),
            [
                (FLASH_BASE, 2 * PAGE_SIZE),
                (FLASH_BASE + 3 * page, PAGE_SIZE)
            ]
        );
        assert_eq!(sectors[1].base, 0x1020_0000);
    }

    #[test]
    fn the_last_page_of_flash_is_inside_and_the_next_is_not() {
        let last = FLASH_BASE + FLASH_SIZE - PAGE_SIZE as u32;
        assert!(Image::parse(&file(&[FLASH_BASE, last], 0)).is_ok());
        let msg = refused(&file(&[FLASH_BASE, last + PAGE_SIZE as u32], 0));
        assert!(msg.contains("outside the board's flash"), "{msg}");
        // Where the bootloader would wrap it onto the start of flash.
        let msg = refused(&file(&[FLASH_BASE, FLASH_BASE + FLASH_SIZE], 0));
        assert!(msg.contains("outside"), "{msg}");
    }

    #[test]
    fn every_malformed_shape_is_refused() {
        let good = image_file(4, 0);
        let mut cases: Vec<(Vec<u8>, &str)> = vec![
            (Vec::new(), "empty"),
            (good[..BLOCK_SIZE + 1].to_vec(), "whole number"),
            (vec![0u8; MAX_FILE_BYTES + BLOCK_SIZE], "larger"),
        ];
        let mut patch = |at: usize, v: u32, why: &'static str| {
            let mut d = good.clone();
            d[BLOCK_SIZE + at..BLOCK_SIZE + at + 4].copy_from_slice(&v.to_le_bytes());
            cases.push((d, why));
        };
        patch(0, 0, "not a UF2 block");
        patch(BLOCK_SIZE - 4, 0, "not a UF2 block");
        patch(8, FLAG_FAMILY_ID_PRESENT | 1, "flags");
        patch(28, 0xe48b_ff59, "family");
        patch(16, 128, "one 256-byte page");
        patch(20, 3, "in order");
        patch(24, 9, "in order");
        patch(12, FLASH_BASE + 0x80, "page-aligned");
        patch(12, FLASH_BASE, "again");
        patch(12, 0x2000_0000, "outside");
        for (data, why) in cases {
            let msg = refused(&data);
            assert!(msg.contains(why), "{why}: {msg}");
        }
        let late = file(&[FLASH_BASE + SECTOR_SIZE], 0);
        assert!(refused(&late).contains("start of flash"));
    }

    /// Malformed input never panics: every single-byte change to every
    /// header field, and every truncation, parses or refuses cleanly.
    #[test]
    fn no_mutation_of_a_header_panics() {
        let good = image_file(3, 9);
        for at in 0..HEADER_LEN {
            for v in [0x00u8, 0x01, 0x7f, 0x80, 0xff] {
                let mut d = good.clone();
                d[BLOCK_SIZE + at] = v;
                let _ = Image::parse(&d);
            }
        }
        for len in 0..good.len() {
            let _ = Image::parse(&good[..len]);
        }
        let mut rogue = block(u32::MAX - 255, 0, 1, &[]);
        rogue.extend(block(u32::MAX - 255, 1, 1, &[]));
        let _ = Image::parse(&rogue);
    }
}
