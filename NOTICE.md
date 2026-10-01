# Third-Party License Notices

`control-ofc-daemon` is distributed under the **MIT License** (see the package
metadata in `daemon/Cargo.toml`). The `control-ofc-tray` binary shipped in the
same package is likewise MIT (`tray/Cargo.toml`). Both depend on third-party
crates under their own licenses, including two whose license differs from MIT
and which are called out here for transparency:

- **serialport** (4.9.0) — **Mozilla Public License 2.0 (MPL-2.0)**.
  <https://github.com/serialport/serialport-rs>
- **ksni** (0.3.6) — **The Unlicense**, used by `control-ofc-tray` for the
  StatusNotifierItem / dbusmenu implementation (DEC-352).
  <https://github.com/iovxw/ksni>

MPL-2.0 is weak, file-level copyleft: it permits use within an MIT-licensed
binary and does not change this project's own license. The obligation to make
the MPL-covered source available on request applies to the `serialport` crate's
own files and is satisfied by that project's public upstream repository and its
crates.io publication.

The Unlicense is a public-domain dedication, not a copyleft licence. It imposes
no condition of any kind — no re-linking obligation, no source-availability
obligation, not even attribution — so it places no requirement on this project
or on anyone redistributing it. It is recorded here only because it is not the
MIT licence the rest of the tree carries. It is OSI-approved and listed by the
FSF as Free/Libre.

## Board voltage-rail facts from the it87 project (DEC-464)

`daemon/src/hwmon/voltage_catalogue_data.rs` is generated from the Gigabyte sensor
configurations in **frankcrawford/it87** (`Sensors configs/Gigabyte/configs/`:
`gigabyte-it87-amd.conf`, `gigabyte-it87-intel.conf`, and the inputs on which
`gigabyte-it87-intel-kabylakex.conf` and `gigabyte-it87-intel-skylakex.conf` agree),
which are distributed under the **GNU General Public License v2.0**.
<https://github.com/frankcrawford/it87>

What the daemon carries is the factual content of their `label inN`, `compute inN`
and `ignore inN` lines — which board rail each Super-I/O voltage input is wired to,
the divider ratio on it, and which inputs a board's configuration does not map — re-expressed
as a Rust table keyed the way those configurations key it (chip and Gigabyte SIV).
No configuration file is copied or shipped. The generated file's header records the
upstream commit it was produced from. The same approach was already taken for the
catalogue-derived fan-header labels in `control-ofc-gui` (DEC-421). We thank the it87
maintainers and contributors for publishing this per-board data.

The full dependency licence set can be regenerated with `cargo tree` /
`cargo about`; this notice records only the non-permissive case (audit P2-H,
DEC-155).
