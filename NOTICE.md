# Third-Party License Notices

`control-ofc-daemon` is distributed under the **MIT License** (see the package
metadata in `daemon/Cargo.toml`). It depends on third-party crates under their
own licenses, including one whose license differs from MIT and is called out
here for transparency:

- **serialport** (4.9.0) — **Mozilla Public License 2.0 (MPL-2.0)**.
  <https://github.com/serialport/serialport-rs>

MPL-2.0 is weak, file-level copyleft: it permits use within an MIT-licensed
binary and does not change this project's own license. The obligation to make
the MPL-covered source available on request applies to the `serialport` crate's
own files and is satisfied by that project's public upstream repository and its
crates.io publication.

The full dependency licence set can be regenerated with `cargo tree` /
`cargo about`; this notice records only the non-permissive case (audit P2-H,
DEC-155).
