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

The full dependency licence set can be regenerated with `cargo tree` /
`cargo about`; this notice records only the non-permissive case (audit P2-H,
DEC-155).
