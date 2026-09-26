# Changelog

## [Unreleased]

## [2.56.3] — 2026-09-27

### Changed

- **The `nct6687` / `nct6775` collision remediation now says what to do** (DEC-433). It used to
  say "Do NOT write PWM", but users never write PWM, and the daemon keeps writing through a
  reported collision. It now says, first, to deactivate the active profile (the tray's **Stop
  profile control**, or `POST /profile/deactivate`) and run no fan tests until you have rebooted
  and the collision is gone. It also says what that does not do: the daemon still restores each
  header's original mode once, a thermal emergency still drives writable headers to 100 %, and
  only removing the wrong driver stops writes to the chip.

### Documentation

- **The port-probe consent texts now say what the probe writes and when it refuses** (DEC-433).
  `superio-port-probe.conf.example` and `daemon.toml.example` called the probe read-only and said
  it skipped only ports a driver owns. Where nothing answers, it writes a vendor unlock and exit,
  including the Nuvoton `0x87,0x87` unlock except on boards listed as ITE-only. It refuses the whole
  probe while any recognised Super-I/O driver is bound, as it does whenever `it87` is loaded on a
  dual-chip Gigabyte board. `daemon.md` is corrected to match.

- The user guide now says the example profile `/etc/control-ofc/profiles/quiet.json` controls no
  fan as shipped. Its one control has no members, and the GUI's starter profiles are the same
  (DEC-432).

## [2.56.2] — 2026-09-26

### Fixed

**An RX 7000 GPU without its PMFW fan curve is no longer reported as controllable** (DEC-430).
RDNA3 cards expose `pwm1` and `pwm1_enable`, but the driver can accept a write there and do
nothing, so their only working fan control is the PMFW `fan_curve`. When that file was missing,
`/capabilities` reported `fan_control_method: "hwmon_pwm"` and `fan_write_supported: true`, and
`POST /gpu/{id}/fan/reset` and `/fan/verify` took the old `pwm1` path. An RDNA3 or RDNA4 card now
never uses that path, whatever files exist (every such card libdrm 2.4.134 lists; a future card
missing from that list would still be judged by its files): without `fan_curve` it reports
`read_only`, and reset
and verify answer `400 feature_unavailable`. The error message and the startup log now say why. They
suggest `amdgpu.ppfeaturemask=0xffffffff` only while overdrive is off. With it on, they say the
kernel did not expose the curve. Kernel 7.0 and later hide it when the card's firmware reports an
invalid fan range. RX 6000 and older are unchanged.

**GPU model names are correct** (DEC-430). The name table matched most cards on the PCI device id
alone, but one id often covers several cards, told apart by revision. Several were named wrong: an
RX 7600 appeared as "7900GRE", a Radeon Pro W7900 as "7900XT", a Pro W6600 as "6800". Names now
come from libdrm's `amdgpu.ids` by device and revision, for every RDNA2, RDNA3 and RDNA4 card it
lists. A card it does not list is shown as "AMD D-GPU" rather than a guess. Workstation names are
kept as written ("Pro W7900", "AI PRO R9700"). Only "RX" names are shortened, as before ("9070XT").

**Memory temperatures on nct6683-family chips are no longer treated as the CPU** (DEC-429). The
`nct6683` driver (also used for `nct6686` and `nct6687`) reports four memory temperatures,
`PECI DIMM 0` to `PECI DIMM 3`. They are read over the same PECI bus as the CPU, and the daemon
reported them as `cpu_temp`. So the thermal safety check counted them as CPU readings. On a machine
with no other CPU sensor (no `k10temp` or `coretemp`), a memory reading stood in for the missing CPU
and kept the 40% no-CPU-sensor minimum from switching on. They are now reported as `mb_temp`.
The CPU's own PECI channels (`PECI 0.0` to `PECI 3.1`) still count as CPU. Curves bound to these
sensors still find them, because sensor ids do not change. Like any board sensor, though, a
`PECI DIMM` that stops updating now holds its curve's fans at their last speed (the control is
listed as not controlled) instead of running on the frozen value. And on a machine where these
were the only CPU readings, the PWM Test Report's stall probe now refuses to run, because there is
no CPU temperature to watch.

**A validation session that is full now says so** (DEC-426). A session keeps up to 4096 events
and 512 external measurements. Past those caps, `POST /validation/session/event` answered
`200 {"recorded": true}` for a marker it had dropped, and `POST /validation/session/measurement`
answered `404` "no validation session is recording" while one was. Both now answer
`409 session_full` (not retryable), with the cap in `details.limit`, and nothing is appended. The
`404` is kept for a session that is not recording. This is a new error code: a client that does not
know it can show the message.

**A missing resource no longer reports a missing endpoint** (DEC-426). The handlers that answer
`404 not_found` when a session, run or cooling device does not exist all prefixed their message
with "endpoint not found:". Only the unknown-route fallback says that now. The others send
their own message, such as "no stall probe has run". The code is still `not_found` in both cases.

**The `controls` health reason no longer says every skipped control holds its fans' speed**
(DEC-426). A `backend_unavailable` control's fans were never driven by the daemon, so for them the
reason now says their speed is up to the hardware. When the reasons are mixed, it gives a count
for each.

### Documentation

**An OpenFan stop is not limited to 8 seconds.** The user guide and developer handover said a 0%
OpenFan command is refused after 8 seconds. It is not: a repeated 0% is never re-sent, so a stop
lasts as long as it is commanded. `limits.openfan_stop_timeout_s` bounds only a check that no
normal sequence of commands reaches. The GUI contract (`docs/08` in the GUI repository) is
corrected in the same change, including which success responses carry `api_version`.

## [2.56.1] — 2026-09-25

### Fixed

**Controlling some channels of an ARCTIC Fan Controller no longer stops the rest** (DEC-425). The
`arctic_fan` driver (kernel 7.2 and 7.3) sends all ten channels in every write, taking the ones not being
written from a cache that starts at 0 at probe and after resume. So the first write to one channel
commanded 0% on every other channel. A profile that controlled some channels stopped every fan on the
rest, and so did a verify, an identify or the thermal emergency's first write. Before any write to such
a device, the daemon now sets every other channel whose cached duty reads 0 to 100%, unless the daemon
itself last set that channel to 0. The exit floor's own writes do the same. It re-applies by itself after
a resume. If the device stops answering, the daemon stops at the first write that fails and logs one
warning, then sets the remaining channels straight after the next write the device accepts. The
uncontrolled channels run at full speed; control all ten channels to keep them quiet.

**The Super-I/O guard covers every Gigabyte board** (DEC-424). `control-ofc-superio-guard` used to suppress
`nct6775`/`w83627ehf` only on the 46 boards in its list. Every other Gigabyte board was therefore still probed
at every boot, an AM5 board behind the same ITE eSPI bridge included, and that probe can hide a chip's fan
headers until a power cut. The guard now suppresses the two modules on every board whose DMI vendor is
Gigabyte. All 131 Gigabyte models sampled use ITE Super-I/O, and none is known with a Nuvoton or Winbond
chip. It also suppresses them on a listed board when the firmware reports no vendor at all. Other vendors,
and Gigabyte's server brand "Giga Computing", are unchanged. The journal line names the board and gives the
recovery steps. To turn the guard off, create an empty `/etc/modprobe.d/control-ofc-superio.conf`.

**The NZXT Kraken 2024 Elite is on the liquid-cooler lists** (DEC-423). `kraken2024elite` (mainline 7.3)
was in `NZXT_KRAKEN3_CHIPS` but not in `LIQUID_COOLER_CHIPS` or `COOLANT_TEMP_CHIPS`. Its pump was always
protected by its "Pump speed" label. What changes now: `is_aio` and `aio_hwmon.present` hold for it; its
channel 2 classifies as `radiator_fan` rather than `unknown`; that channel gets the cooler floor through
`member_is_pump_or_cpu`; and its temperature classifies as coolant by chip. The cross-stack role oracle
gains both channels. Because `validate()` rejects a pump/CPU control below 30% (`FLOOR_TOO_LOW`), a profile
that a GUI older than the paired release stamped at 20% on that channel is refused. Upgrade the GUI with
this daemon; the paired GUI raises the floor when it loads a profile.

**The AMD GPU kernel advisory is the one bisected, fixed hang, on the kernels and GPUs that carry it**
(DEC-422). `rdna_hang_kernel_6_18_6_19` flagged every 6.18/6.19 kernel on RDNA3/4 as critical and advised
pinning 6.15–6.17, none of which was ever a longterm kernel. `smu_mismatch_navi48_r9700` told every
R9700 owner their fan curve could not work, on the strength of a benign message. Both are retired. The
daemon now raises `rdna_mes_hang_drm_amd_4765` (drm/amd #4765, the MES eviction hang) on 6.17.9–6.17.13,
a backport that 6.17 never fixed, and on 6.18.0–6.18.6. The hang is fixed in 6.18.7 and 6.19.
`is_rdna3_or_rdna4` now matches every RDNA3, RDNA3.5 and RDNA4 product in libdrm's `amdgpu.ids`. It used
to miss the RX 9060 series, four RDNA3 cards and the integrated GPUs. The same list chooses the
actionable `amdgpu.ppfeaturemask` hint for a read-only GPU, so that hint now reaches those cards too.

**The Gigabyte dual-chip board table is corrected and extended, so working boards stop reporting a
missing chip** (DEC-421). `GET /diagnostics/hardware`'s `expected_chips` listed an IT87952E for the X670E
AORUS MASTER, whose second chip is an IT8792E. It also listed two chips for the X670 AORUS ELITE AX,
the Z790 AORUS ELITE AX and — through the bare "X870E AORUS ELITE" entry — the X870E AORUS ELITE WIFI7,
all of which have one. Each of those boards showed a missing-chip warning while working. The table can
now hold single-chip boards, which keeps them under the modprobe guard without the false warning. It
gains more boards with published evidence — X570 / B550 / TRX40 / X399 / Z390 / Z490 AORUS boards,
X570S boards, the Z790 AORUS PRO X and Z890 AORUS MASTER, among others — 46 in all. The packaged
`control-ofc-superio-guard` list matches it, so `nct6775` and `w83627ehf` are no longer probed on the
added boards either. A test now fails if an entry would shadow a later board with a different chip
complement.

**The advice for an unbound ITE secondary chip gives one recovery ladder** (DEC-421). It used to branch
on a device ID the driver prints only at debug level. It now says: stop `nct6775`, `w83627ehf` and
`sensors-detect` reaching the chip, reboot, then remove mains power. It also warns that `it87-dkms-git`
builds from 2026-09-09 rename Gigabyte chips.

**The `nct6687` / `nct6775` collision text covers `force=1`** (DEC-421). Since nct6687d PR #174,
`force=1` attaches to any Nuvoton chip ID from 0xD000 to 0xDFFF, so it can still claim an NCT679x chip
that a current build otherwise leaves alone. The remediation now identifies the chip from the kernel log
rather than the hwmon name, which both Nuvoton drivers share. It no longer tells owners of a genuine
NCT6687D to blacklist `nct6775`.

### Documentation

The README's prerequisites and the User Guide's driver notes are corrected (DEC-421). ASUS AM4
300/400-series boards need `it87-dkms-git` for their IT8665E. There is no `nct6686d-dkms-git` package,
so the package's optional dependencies no longer offer one. They also name the MSI chip as the NCT6687D,
and list the ASUS AM4 boards under `it87-dkms-git`. `it87` should use its own `ignore_resource_conflict=1` before the system-wide
`acpi_enforce_resources=lax`. A BIOS fan curve must never have a 0% point, and "Full Speed" is a
fail-safe, not a fix. The packaged `modules-load.d` file no longer says Gigabyte boards use Nuvoton chips
or that ASRock boards use ITE ones. Code comments that described the D5 Next's pump channel as
monitor-only, and `kernel_detected_chips` as populated on Arch, are corrected.

## [2.56.0] — 2026-09-24

### Fixed

**A PWM test stops when the fan it is testing becomes a pump** (`TS-aw`, DEC-418). Verify,
PWM characterisation and control-path discovery decide at the start whether a header drives a
pump, and choose their test duties from that. If you activated a profile that names the header a
pump, or assigned it the pump role, while one of them was running, the test carried on with duties
chosen for an ordinary fan — which can be below the 30 % pump floor — until it finished. The daemon
now re-checks before every write and every half second. When the header becomes pump-protected, the
test stops, and the restore the daemon then writes is never below 30 %. A characterisation or
discovery run ends `aborted`, and its `detail` says why. A verify returns a new result,
`pump_protected_mid_run`. Nothing was measured, so it is not a finding about your board: run the test
again, and it will use pump-safe duties. The restore checks once more just before it writes, so
protection that arrives after the last measurement — which leaves that measurement standing — still
keeps the pump at 30 % or above. If the test could not read the header's duty before it started, the
header is left where the test left it, as before, but raised to 30 % if it has become a pump.

**Assigning the pump role during a fan identify can no longer leave that pump stopped** (`TS-ax`,
DEC-419). Assigning `pump` to a header releases any identify hold on it, so the fan returns to its
curve. But identify read the header's role before it took the lock its hold is written under. An
assignment that landed in between was missed both ways: identify had already decided the header was
not a pump, and the release found no hold, because the hold did not exist yet. The pump was then held
at 0 % for up to 15 s. Identify now reads the role under that lock, so it either sees the assignment
and perturbs the pump instead of stopping it, or the assignment's release removes the stop. The same
race with a profile activation was closed in 2.51.1 (DEC-394).

**A write can no longer re-take a header the daemon has just given back at shutdown** (`PTR-s`,
DEC-420). A clean stop gives every header the daemon took back to what it was doing before. That
hand-back cannot wait for the controller's lock, so a write that had stalled in a slow driver could
land just after it — and since 2.53.0's duty check, even a repeat of the same duty did, pulling a
header just given back to full speed back down to the curve's duty, in manual mode with nothing left
to drive it until `ExecStopPost` replayed the record. The stop now marks the hand-back begun before
it writes anything — the crash-time hand-back too — and from then on the daemon refuses every write to
a header it gives back. A verify still running when the daemon stops gets `503 hardware_unavailable`
(retryable), the same answer as a request that arrives while stopping.

**After a failed duty write, the next identical command is written instead of skipped** (`TS-au`,
DEC-420). The daemon skips a write whose duty equals the last one it wrote successfully. A write the
driver reported as failed may still have landed, so after `60 %` → `30 %` (failed) → `60 %` the header
could stay at 30 % until the curve moved — and where the register could not be read back, 2.53.0's duty
check could not notice. A failed write now makes the next command write again, re-asserting manual
mode where the header has a mode switch. The same applies after the stop's exit floor fails to write.
One side effect is deliberate: a correction whose write fails is followed by a fresh write, which
restarts the count behind `duty_not_holding`, so a header whose writes fail now and then while
something else fights it takes longer to be flagged.

**A PWM characterisation can no longer hang on a stuck driver** (`PTR-v`, DEC-420). The sweep read the
header directly on the daemon's async runtime with no time limit, so a driver that stopped answering
left the fan at a test duty with every safety check — temperature, cancel, the pump re-check —
waiting behind that read, and could stall other daemon work. Every read now runs off the runtime and
gives up after 2 s: the run ends `aborted`, its `detail` says a read did not return, and the sweep
reads and writes that header no further. **The duty it had before the sweep is then not written back**
(new `restore_outcome`: `skipped_unresponsive`, with `restore_failed: true` once the sweep had written):
a write goes through a path whose own reads have no time limit and hold the lock the rest of fan
control needs, so writing to a driver that has stopped answering could stall every hwmon fan. The fan
stays at the last test duty, which is never below 20 % (30 % for a pump), until your profile drives it
again. The one exception: a header that became a pump during the run still gets its restore, raised
to 30 %. The sweep also checks for a shutdown
immediately before each write, takes its reference reading before its safety checks rather than
between them and the write, and reports a mode change it sees while the daemon is stopping as the
shutdown rather than as another controller taking the fan. **`original_pct` in the 202 response to
`POST /hwmon/{id}/characterize` is now `null`**: the daemon no longer reads the header while answering
the request. The finished run carries it, taken from the same reading the restore uses, so the
published value and the restored duty can no longer differ.

## [2.55.0] — 2026-09-23

### Added

**Long diagnostics say what they are doing between results** (`P8-bg`, DEC-411). A
characterisation sweep publishes a point only when its hold ends, and control-path discovery
publishes a cycle only after both of its windows, so a healthy run could be silent for up to 26 s.
`CharacterizationRun` and `ControlPathRun` now carry `current_step`: the phase being held (`settle`
or `dwell` for a sweep; `settle_wait`, `baseline` or `perturbed` for discovery), the step or cycle,
the duty, when the phase started and its upper bound. It changes at every phase boundary and is
`null` before the first write and once the run has finished.

**`GET /capabilities` publishes the diagnostic temperature limit** (`PTA-i`, DEC-411).
`limits.diagnostic_max_temp_c` is the temperature above which any sensor stops a verify, a
characterisation, control-path discovery, the stall probe or an OpenFan calibration. Clients that
tell the user where a test stops can now show the real figure.

### Changed

**A slow ramp is no longer called settled** (`PTR-q`, DEC-411). A point settled when four
consecutive tach updates sat within 5 % of their median. A slowly moving pump could pass that while
still ramping, so a report called a moving reading steady. The four updates must now also not all
move the same way by more than 2.5 % of their median. Slow devices on slow tach registers now settle
later, or report that they did not settle (`settled_ms: null`, stability `not_settled`, discovery
`baseline_settled: false`), where they used to report a settle that had not happened.

**A point's `rpm_verdict` is judged against its own measured noise** (`PTR-m`, DEC-411). A reading
counted as `changed` only if it moved by more than a tenth of itself, so a smooth, fast fan's
genuine ~265 rpm steps read `unchanged`. A point that settled is now `changed` when it moved by more
than three of its own standard deviations (never less than 50 rpm). A point that did not settle, or
had too few readings, keeps the old rule.

**`backend_unavailable` now covers fans on headers the daemon cannot write** (`OFN-al`, DEC-412).
A control none of whose fans the daemon can write is listed in `skipped_controls[]` as
`backend_unavailable`. That used to be decided per backend: on a board with at least one writable
header, a control bound only to read-only headers counted as deliverable. It published a duty, and
the engine then dropped every write to it without logging anything. Each fan is now checked on its
own, so such a control is listed and leaves `control_outputs`. The same applies to headers the
board doesn't have at all, as in a profile imported from another machine. A control where some
fans can be written and some can't is still only logged, not listed, as before.

### Fixed

**`/status` no longer reports an OpenFan duty the daemon has stopped knowing** (`TS-ad`, DEC-412).
After a write whose reply failed, and for every channel after a serial reconnect or a system resume,
the daemon treats the channel's duty as unknown. `last_commanded_pwm` kept showing the previous
value anyway, for as long as nothing wrote that channel again. It is now omitted until a write
lands. That channel's `stall_detected` is omitted over the same period, because it is worked out from
that duty.

**A cancelled OpenFan calibration no longer leaves a fan stopped** (DEC-412). A calibration puts
the channel back to its previous speed when it ends, even when cancelled. If that speed wasn't known
(the channel had never been set, or its speed was lost as above), nothing was restored, so a sweep
cancelled in its first steps left the fan at 0 %. The channel now goes to 100 % instead. A
calibration that is refused before it starts (too hot, the thermal emergency still active, or
temperature readings too old) no longer writes anything to the channel, not even a restore.

**A crash during a GPU verify no longer leaves an older AMD card's fan stuck in manual mode**
(`TS-aa`, DEC-414). On an RX 6000 or older card, the hardware verify puts the fan in manual mode
at a test speed and restores it when done. If the daemon died in between, nothing gave the card
back. The service's stop script used to reset every fan header, which covered this, until DEC-382
limited it to headers the daemon had recorded. The verify now records the card's original fan mode
before its test and removes that record when it restores the card. The stop script gives back any
card still recorded. A card the daemon did not test, including one another tool controls, is not
touched.

**A Super-I/O module in the initramfs no longer fails to load with a missing-file error**
(`PKG-g`, DEC-414). mkinitcpio copies the package's modprobe guard rule into the initramfs, but not
the guard program it runs. So `nct6775` or `w83627ehf` in `MODULES=` failed there with a "not
found" error on every board. The rule now checks for the guard first. Without it, nothing is loaded
or probed in the initramfs, and a message explains why. The package's `modules-load.d` file then
loads the module after boot, where the guard can check the board. The check needs a shell in the
initramfs, which mkinitcpio's `base` hook provides. An image built without `base` fails the load
as before, still without probing.

**A calibration that starts just as a thermal emergency ends keeps its test speeds** (`TS-z`,
DEC-413). After an emergency, the daemon returns each OpenFan fan that no profile controls to its
speed from before the emergency. It checked for a running calibration once, before setting up to ten
channels. A calibration that started partway through could then have a test speed overwritten,
which corrupted that step's reading. Each channel is now checked before it is written, and the
channels not yet returned wait until the calibration ends.

**A config reload can no longer undo an exit-minimum change made at the same moment** (`TS-aq`,
DEC-412). A `SIGHUP` / `systemctl reload` that read the config files just before
`POST /config/exit-floor` saved a new value could then apply the old one. The file and the running
daemon disagreed, and the next stop used the older minimum. A reload now waits for any config
change in progress, and a SIGTERM never waits behind the reload.

**A validation session records one profile's pump settings** (`TS-ag`, DEC-412). A session's
metadata read the active profile once per member and again for its id. A profile activated in
between could leave the recorded id beside pump fields worked out from a different profile. It is
now read once.

**A diagnostic always gets the duty it asks for** (`PTA-j`, DEC-412). A verify, characterisation or
probe write that repeated the engine's last duty was skipped. If a second program was holding the
header at another speed, the test recorded that program's RPM under the duty it asked for. It is
now always written.

**A failed hand-back no longer leaves "duty not holding" raised** (`PTA-k`, DEC-412). When a
profile stopped naming a header and giving it back to firmware failed, the header kept reporting
`duty_not_holding: true`. That blamed corrections for a failing hand-back. It could also stop a
later profile from correcting a second writer on that header.

**A validation session that tests several members reports each one** (`PTR-n`, DEC-411). Every
finding derived from a characterisation, verify or control-path run reported only the first run it
found, so a session sweeping several members showed one member's verdict and said nothing about
the others. Each finding is now reported once per member. The GUI's start form sends one sweep
member, so this was only reachable through the API.

## [2.54.0] — 2026-09-22

### Added

**A stall/restart probe: find where a fan stops, and where it starts again**
(`PTR-h`, DEC-407). `POST /hwmon/{header}/stall-probe` is the one diagnostic
that drives a fan below 20 %, down to 0 %. It finds the **stall duty** (where
the fan stops as the speed falls) and the **restart duty** (where it starts
again as the speed rises). A profile that asks for a speed between the two can
stop a fan that it can't then restart. On one machine a radiator fan stopped at
6 % and only restarted at 10 %.

It is opt-in and runs on one header at a time. It refuses anything that could
be a pump (the label, your role assignment, or a profile that names it a pump),
a `cpu_fan` header, and a header whose role is still `unknown`. Assign
`chassis_fan` or `radiator_fan` first. It keeps checking this while it runs, and
stops within half a second if you assign a pump to that header. It also needs a
fresh CPU temperature, and stops if that rises more than 5 °C. It stops if it
can't read the fan's speed, rather than guess. A fan that stops as soon as it
gets to 20 % is reported as stopping at 20 % or above, and is given the 100 %
kick before it gets its previous speed back. There is nothing to tune: the request is exactly
`{"acknowledge_below_floor": true}`, and the daemon works out the step length
and time budget from how often the fan's speed reading updates (at most three
minutes below 20 %). A chip that updates too slowly is refused, with the
reason. If the probe stops early or you cancel it, the fan runs at 100 % until
it is seen spinning, then gets its previous speed back. (If the daemon itself is
shutting down it doesn't; the shutdown puts every fan back on its own.)
`GET /diagnostics/stall-probe` returns the result;
`DELETE /diagnostics/stall-probe` cancels it. Capability `control.stall_probe`;
preflight `diagnostic=pwm_stall_probe`. Characterisation is unchanged and still
never goes below 20 %.

### Changed

Characterisation's safety checks before each write (shutdown, cancel, the
temperature checks, and the check that its claim on the fan is still held) now
come from one shared definition, which the stall probe also uses. There is no
behaviour change.

## [2.53.0] — 2026-09-22

This release also carries **2.52.0**, which was never published on its own —
its changes (DEC-405: more accurate fan-diagnostic settling and resolution
figures) are listed under
[2.52.0](https://github.com/Plan-B-Development/control-ofc-daemon/blob/v2.53.0/CHANGELOG.md#2520--2026-09-22)
below.

### Fixed

**A fan speed set by another program no longer sticks while your profile's
curve is steady** (`PTR-g`, DEC-406). The daemon skips rewriting a motherboard
fan header when the speed it wants hasn't changed, and before this release it
skipped without checking whether the header still held that speed. So if a
vendor tool, a script or the firmware set a different speed, it stayed until the
curve moved. On one machine a header stayed at 60 % for 55 seconds while the
daemon was asking for 46 % and then 40 %. That header can be a pump. The daemon
now reads the speed back each time it would skip. If the reading has moved more
than 2 points from what the header held right after the daemon's own write, it
writes the speed again. (It compares with what the header held, not with what
was asked for, so a fan controller with only a few speed steps, like Dell and
ThinkPad laptops, or a chip that won't go below some speed, doesn't look like
drift.) If three of those rewrites in a row don't stick because another program
keeps writing, it stops rewriting that header until the speed it wants changes
or the header holds again. It doesn't keep fighting. Each episode logs one warning when the
first rewrite happens, one when it gives up, and one line when the header holds
again.

This covers the daemon's own control (profile curves and manual overrides).
Diagnostics (verify, characterise, control-path discovery) still get exactly the
speed they ask for. The thermal emergency always writes, as it already did.

### Added

**`duty_corrections` and `duty_not_holding` on every motherboard-header entry
of `/fans` and `/poll`** (DEC-406): how many times the daemon has rewritten the
header's speed since it started, and whether it has currently stopped trying. A
new capability, `control.duty_reconciliation`, tells a client this daemon does
both.

## [2.52.0] — 2026-09-22

### Fixed

**Diagnostic settling times and stability figures describe the fan, not the
tach's refresh rate** (`PTR-a`, `PTR-b`, DEC-405). Many motherboard chips update
their fan-speed reading only every couple of seconds, while the characterisation
sweep reads it twice a second. The sweep saw four identical readings, a single
stale value, and called the fan settled before it had moved. It then worked out
the fan's steadiness from the moment of the write, so the fan speeding up or
slowing down counted as unsteadiness: 14 % variation was reported for a pump that
varies by under 1 %. Settling now counts real updates of the reading, and can't
happen before the first one. Steadiness is measured only after the fan has
settled, while tach dropouts are still counted across the whole step. A point
that never settles says "not settled" instead of showing a figure. The reported measurement resolution is now how often the chip refreshes
(the median gap between updates, or the chip's own figure where it publishes
one), not 500 ms. A sweep now holds each point for 12 s by default instead of
6 s, so a default sweep takes about twice as long.

**A sweep that walks down and back up no longer reports every fan as
"non-monotonic"** (`PTR-d`, DEC-405). The check read the readings in the order
they were taken, so the step from the lowest duty back up always looked like a
drop. Each direction is now judged on its own, by duty. The result says which
direction, if either, did not rise steadily.

**Control-path discovery no longer mistakes a slow pump's recovery for noise**
(`PTR-c`, DEC-405). Each later cycle started measuring right after the previous
change was undone, while the pump was still returning to speed. That recovery was
counted as noise, so a real response could fail to clear it: on 2026-09-08 a
+796 rpm response was graded "ambiguous". The run now waits, for up to 15 s, for
the fans to settle first. A channel that doesn't settle in time uses the noise
level from the first cycle, and the result says so. The reported measurement
resolution is now the header's own tach, not the fastest fan on the machine. Each
measurement window defaults to 12 s instead of 6 s. Cancelling a discovery now
takes effect when the current measurement window ends. It used to wait for the
end of the whole cycle, up to two windows. A pump whose speed reading disappears
while the run waits stops the run at the end of that wait.

**A validation session records what its PWM verify actually found** (`PTR-e`,
DEC-405). The session read the verify's readback and before/after speeds from the
wrong places, so they were always empty. Every verify that ran was recorded as
"observed", including one where the fan did not respond at all. The session now
keeps the real readings, the verify's result and whether the fan was put back
where it was. It records a working result as passed, and a result that says
something about the hardware as observed. A verify the daemon refused, for
example because of heat or another test running, is now recorded as unavailable.
It used to count as a failed "PWM header control" finding.

### Added

**The hardware report names the kernel, BIOS date and driver builds**
(`PTR-f`, DEC-405). `GET /diagnostics/hardware` now includes the running kernel
version, the BIOS date, and each loaded fan driver's version, build checksum, and
whether it was built outside the kernel (a DKMS driver, for example). With these,
two reports can show whether a driver, kernel or BIOS update is what changed.
Each is left empty when the system doesn't provide it.

## [2.51.3] — 2026-09-19

### Changed

**An OpenFan fan whose speed was lost to a reconnect or resume goes to full speed
when no CPU temperature can be read** (`TS-av`, DEC-401). When the daemon cannot
read any CPU temperature it holds the active profile's fans at 40 % or more, and a
fan whose curve has also lost its sensor stays at the speed it had. After the
OpenFan controller reconnects, or the computer wakes from sleep, the daemon no
longer knows that speed, because the controller may have come back at its
power-on default. Such a fan used to get the bare 40 %, which could be lower than
it was running, and it then stayed there. It now goes to full speed and stays
there until its curve can run again, which is what the daemon already does with a
lost speed when it stops. A fan the daemon has never set still gets 40 %, and so
does one whose last command went unanswered, unless the controller has reconnected
since. Motherboard fans are unaffected: the daemon keeps
their last speed across a resume.

### Fixed

**A missed stop notice is sent again sooner** (`TS-ay`, DEC-402). When systemd is
too busy to take the daemon's stop notice, 2.51.2 sends it again just before the
fan hand-back. In one rare case that was too late: several parts of the shutdown
each ran out their full time limit, and systemd's watchdog could interrupt the
hand-back first. The daemon now also sends the notice as soon as it has stopped
taking requests, before it waits for those parts, and still sends it before the
hand-back if that attempt fails as well. This makes the rare case rarer, not
impossible: if systemd stays busy for a moment longer, the notice can still arrive
too late.

**The sleep hook's two signals are handled in the safer order when both are
waiting** (`TS-az`, DEC-402). If the daemon receives the "going to sleep" and
"woke up" signals together, it cannot tell which came first. It used to pick
either order at random. One wrong order could leave the watchdog at its normal
15 seconds across the next sleep, so a machine that is slow to resume could see
the daemon restarted as it woke. The daemon now always handles "woke up" first.
When that guess is wrong, the watchdog stays at 120 seconds for up to two minutes,
which only delays the detection of a hung daemon.

## [2.51.2] — 2026-09-19

### Fixed

**GPU fans keep following their own curves during a thermal event** (`TS-t`,
DEC-399). While the daemon holds motherboard and OpenFan fans up — in a thermal
emergency, or when it cannot read any CPU temperature — it stopped writing GPU
fans at all, so each one stayed at the speed it had when the event began. On a
machine where no CPU temperature can be read at all, that state lasts the whole
session, so a GPU fan curve stopped following its temperature a few seconds after
the daemon started. GPU fans are still never forced to a safety speed, because the
card's own firmware protects it. Their curves now keep running through the event,
like the curves of the other fans in the same profile. During the event a curve
runs without its step limits and without its Start % kickstart, for GPU fans as
for the rest; the card's firmware still applies its own minimum fan speed. A
manual override or an identify on a GPU fan now takes effect during the event as
well, where before it did nothing until the event ended.

**Dell fans go back to BIOS control when the daemon lets go of them** (`TS-ab`,
DEC-398). On many Dell machines the `dell_smm` driver has one switch that turns
the BIOS's fan control on or off for every fan at once. The switch sits on the
first fan header, and it can be set but never read. Since 2.50.0 the daemon gives
each header back exactly as it found it, and a setting it could not read got full
speed instead. On those machines, whenever the daemon let go of the first header,
that fan went to full speed and the BIOS stayed off. That happened when a thermal
emergency ended, when a profile was deactivated or stopped naming the header, and
when the daemon stopped. The other fans were left where they were, with nothing
controlling them, until the next boot. The daemon now turns the BIOS's fan
control back on, which is what the switch's `2` means and how the machine starts.
The `ExecStopPost` cleanup does the same after a crash. If the switch refuses the
write, the first fan still goes to full speed and the BIOS stays off. Because
the switch covers every fan, handing it back also gives the BIOS any other fan a
profile still controls, until something takes the switch again. On these
machines a profile should therefore control all of the fans or none of them.
Other drivers are unchanged.

**CPU temperature via PECI/TSI is now read on every Nuvoton nct67xx chip**
(`DOC-w`, DEC-397). On `nct6779`, `nct6791`–`nct6793` and `nct6795`–`nct6799`, a
`PECI Agent` or `TSI` temperature channel is the CPU temperature the kernel says to
read. The daemon reported it as a motherboard sensor, because only five Nuvoton
chips were recognised. On an ASUS board with one of these chips, the unreliable
`CPUTIN` pin is already set aside. That left the chip with no CPU temperature at
all, so a newer CPU without its own kernel sensor driver could not be protected by
the thermal-emergency rule. These channels now report `kind: "cpu"`. They appear
as CPU sensors in the GUI's sensor table and curve sensor picker, matching how the
GUI already described them. They also count toward the thermal emergency, which
acts on the hottest CPU reading.

**A slow resume from suspend no longer restarts the daemon** (`TS-ao`, DEC-396).
The service watchdog keeps counting while the machine's devices suspend and
resume, a stretch in which the daemon is frozen and cannot check in. On hardware
where that took more than about ten seconds, systemd could restart the daemon
just as the machine woke. The package now installs a systemd sleep hook,
`/usr/lib/systemd/system-sleep/control-ofc-daemon`. Before each suspend or
hibernate it asks the daemon to widen its watchdog to 120 seconds, and after resume
it puts the normal 15 seconds back. If the resume call never comes, the daemon
narrows the watchdog again by itself after 120 seconds. The hook only ever signals
the running daemon, and never stops the machine from sleeping. A `WatchdogSec`
drop-in is no longer needed for this, and a wider one you already set is kept.

**A missed stop notice is sent again before the shutdown restore** (`TS-ap`,
DEC-396). When the daemon stops, it tells systemd to stand its watchdog down so the
fan hand-back can take as long as it needs. If systemd was too busy to take that
message, the watchdog stayed armed, and a slow hand-back could be interrupted
part-way through. The daemon now sends the message a second time just before the
hand-back, after the rest of the shutdown has given systemd time to catch up. It
can still be too late in one rare case: systemd was busy when the stop began, and
several parts of the shutdown each ran out their full time limit.

**A fan diagnostic now refuses on the same stale temperature the thermal
safety rule stops trusting** (`TS-aj`, DEC-395). The hwmon verify, PWM
characterisation, control-path discovery and OpenFan calibration refuse to start
— and a sweep in progress stops — when no temperature reading is fresh. Until now
"fresh" meant up to 10 s old, while the thermal safety rule stops acting on a CPU
reading after 5 poll intervals (5 s at the default 1 s poll). For those 5 seconds a
diagnostic could drive a fan on a temperature the safety rule had already set aside,
so it could not force full speed on it however hot it read. Readings refresh every
poll, so a reading that old means sensor reads are failing. The two limits are now
the same at every poll interval. At the default poll a diagnostic therefore refuses
a few seconds sooner when reads fail, and the preflight's `temperature_source` line
names the limit actually applied.

### Documentation

**A faulty CPU sensor stuck at or above the emergency limit keeps the emergency
on, by design** (`TS-s`, DEC-400). One reading at the limit starts it, and it
holds until that sensor reads a fresh value at or below 80°C. The daemon never
decides afterwards that the reading was wrong, following the industrial safety
standard IEC 61511 (a safety action that has fired stays in force until its
reset). The user guide now says so, and says what to do when fans stay at full
speed on a cool machine. Nothing about the behaviour changed.

## [2.51.1] — 2026-09-19

### Fixed

**A motherboard header with no mode switch can no longer be lowered after the
exit minimum was applied on stop** (`TS-ar`, DEC-392). On a clean stop the daemon
raises each header that has a `pwmN` but no `pwmN_enable` to at least the exit
minimum (DEC-388). A final control-loop write that was still in flight — possible
only when sysfs writes had wedged long enough to outlast the shutdown drains —
could land after that and leave the header below the minimum you set. The minimum
now latches for these headers exactly as it already did for OpenFan channels: once
it has run, any lower write to such a header is raised to it, and a higher one
still lands.

**An OpenFan channel's duty is treated as unknown straight after a reconnect or
resume** (`TS-ak`, DEC-393). After the controller reconnected or the system resumed,
the daemon went on treating each channel's last commanded duty as current until its
next write to any channel, although the device may have come back at its power-on
default. Two things read it in that window. A thermal emergency's record of what to
give back afterwards could return a channel to that old duty; such a channel now
stays at full speed instead. And under the missing-CPU-sensor 40 % floor, the
profile's lowest-numbered OpenFan channel, when its control was skipped, was held at
the old duty while every other skipped channel already dropped to the floor. All of
them now get the floor, which for that one channel can be lower than before, and it
stays there while the control is skipped.

**Activating a profile that treats a header as a pump now ends an identify stop
on it** (`TS-af`, DEC-394). If you had stopped a fan with identify while no
profile called it a pump, and then activated a profile that does — for example
one whose member for that header is labelled "Pump" — the header stayed stopped
until identify timed out (up to 15 s). It now resumes under the new profile at
once, the same as when you assign the header the pump role. An identify that is
nudging a pump rather than stopping it is left running. A narrow timing window
in which an identify arriving at the same moment as an activation could still
stop the newly-protected pump is closed too.

## [2.51.0] — 2026-09-19

### Fixed

**Saving a setting while `runtime.toml` is unreadable no longer removes your other
fan header roles** (`TS-r`, DEC-391). If the daemon's settings file had become
unreadable since startup — usually a hand-edit with a mistake in it — the next
setting you saved moved the file aside and started a new one from defaults.
Assigning a role to one header then also removed the role you had given every
other header, immediately: a pump known only by its assigned role lost its 30 %
minimum and could be stopped by fan identify, and nothing on `/status` said so.
The new file now starts from the header roles and cooling devices the daemon is
running with, and it is written before the save does anything else — so a save
that is then refused, or that fails to write, no longer leaves no settings file at
all, which made the next restart drop every role without a word. `/status` and
`/poll` report it as `runtime_config_degraded` with the new phase `update`. Any
other setting that was only in the old file (kept as
`runtime.toml.invalid-<timestamp>`) still needs copying back. A settings file that
has been deleted by hand is treated the same way: the next save keeps the roles
the daemon is running with.

## [2.50.0] — 2026-09-18

### Added

**The daemon is restarted if its control loop stops running** (`TS-d`, DEC-387).
If the loop that drives the fans and watches for a thermal emergency stopped
making progress while the daemon stayed up — a bug that hangs rather than one
that crashes — every fan held its last speed, no emergency could fire, and nothing
noticed: to systemd the service looked healthy. The daemon now tells systemd each
time the loop completes a pass, and systemd restarts it if 15 seconds go by
without one, handing the motherboard fans back to the BIOS in between. A fan whose
hardware is merely slow to respond does not trigger it. `systemctl start` now also
waits until the daemon is actually controlling the fans. One limit: on a machine
whose devices take more than about ten seconds to suspend and resume, the daemon
may be restarted as the machine wakes; a systemd drop-in raising `WatchdogSec`
avoids it there.

**OpenFan fans no longer stop at whatever speed the daemon last gave them**
(`TS-j`, `TS-y`, DEC-388). An OpenFan channel has no firmware behaviour to fall back
to, so after the daemon stopped it held its last speed indefinitely — 20 % if the
machine was idle at the time, even under a heavy load that followed. On a clean
stop each channel the daemon drove is now left at its last speed or the **exit
minimum**, whichever is higher (50 % by default), and at 100 % if the daemon had
lost track of its speed. The same applies to motherboard headers that have no
automatic mode to hand back to. Set the minimum in the GUI (Settings → Daemon
Configuration → Exit minimum), with `POST /config/exit-floor`, or with
`[shutdown] exit_floor_pct`; it applies at once, and 0 turns it off. A crash cannot
run it, since nothing outside the daemon can reach the OpenFan controller.

### Fixed

**The daemon now gives each motherboard fan back exactly the way it found it**
(`TS-a`, `TS-b`, `TS-c`, DEC-382). Every way the daemon let go of a fan header —
stopping, crashing, panicking — wrote `pwm_enable=2` to every header on the
machine, touched or not. `2` means "automatic" only on the it87 driver. On
nct6775 boards (common on ASUS and ASRock) it selects Thermal Cruise instead of
the BIOS's Smart Fan IV, and on an NZXT Kraken it applies a curve that is all
zeros — the pump on a 0 % curve. The daemon now reads each header's mode just
before it first takes it, keeps a record of every header it holds, and gives back
exactly that: the mode it found, or the duty if the header was already in manual
mode. A header whose mode cannot be restored is set to full speed instead, as
lm-sensors `fancontrol` does, and a header the daemon never took is never
written. After a crash, `ExecStopPost` replays the same record.

**Fans are given back as soon as nothing holds them.** A thermal emergency ending,
a Test PWM Control, Characterise or discovery run ending, deactivating a profile,
and switching to a profile that no longer controls a header each used to leave the
header in manual mode at its last duty, BIOS curve off, until the daemon stopped.
Each now hands it back on the next tick. OpenFan channels no profile controls get
their pre-emergency speed back when an emergency ends; a channel whose speed the
daemon does not know — never set, or its last command unconfirmed — stays at 100 %.

**An OpenFan fan can no longer miss a thermal emergency because one reply
failed** (`TS-o`, DEC-383). The daemon sends a speed command and then waits for the
controller's reply. If that reply timed out or arrived garbled, the command may
well have been applied — but the daemon kept its previous record of the channel's
speed. A channel it believed was at 100 % that took a 60 % whose reply failed
could then sit at 60 % for a whole emergency, because every forced 100 % matched
the stale record and was skipped as a duplicate, while `/status` reported 100 %.
A failed reply now leaves the channel's speed unknown, so the next command is
always sent.

**Identifying fans no longer stops a pump that your profile holds at 30 %**
(`TS-h`, DEC-384). Many motherboards publish no fan names, so on those the only
thing telling the daemon a header drives a pump can be the name your profile
gives that fan. The daemon already used that name to keep the fan at no less than
30 %, but identify ignored it and stopped the fan for the length of the test. A
fan the active profile calls a pump or an AIO is now shifted instead of stopped —
the same rule as a header labelled `PUMP` — and so is the diagnostic test duty.
`stop_permitted` and `effective_min_pwm_pct` on `/hwmon/headers`, `/hwmon/rescan`
and `/inventory/hwmon` now follow the active profile too. A CPU fan and a
liquid cooler's radiator fans are still stopped, as before. Assigning the pump
role is still what protects a pump whatever profile is active.

**A fan diagnostic no longer runs on temperatures nothing is measuring**
(`TS-q`, DEC-385). If sensor polling stalls, the last temperatures stay in the
daemon's cache. The thermal emergency correctly stops trusting a stale CPU
reading — but the checks that guard a Test PWM Control, a PWM characterisation
and an OpenFan calibration compared the numbers without asking how old they were.
A stalled poll that last read 84 °C therefore let a calibration sweep drive a fan
from 0 % for up to five minutes while the emergency could not fire. All three now
refuse to start (`409`, retryable, "Retry once sensor polling recovers") when no
temperature reading is fresh, and a sweep already running stops at its next step.
The safety preflight now reports this as blocking for verify and characterisation
too, where it used to be only a warning.

**A fan keeps its speed when the no-sensor floor applies, if its own curve can no
longer be read** (`TS-p`, DEC-386). When every CPU temperature reading fails, the
daemon stops trusting the sensor and, after a few more seconds, holds your
profile's fans at no less than 40 %. A control whose sensor was gone had already
stopped being updated — its fans keep their last speed — but that 40 % floor then
treated them as if nothing were driving them and set them to 40 %. A CPU curve that
had been running its fans at 90 % dropped to 40 %, with the CPU's temperature
unknown. They now keep their last speed under the floor.

### Changed

**A daemon that keeps failing is restarted more and more slowly, instead of being
given up on** (DEC-387). systemd used to stop restarting the daemon after five
failed starts in a minute, leaving the machine with no fan control until someone
intervened. Restarts now back off instead — 3 s, then about 5, 10, 18 and 33 s,
then once a minute — and never stop. After five healthy minutes the next restart
is fast again. The startup delay is also capped at 30 seconds when it is set by
hand in `runtime.toml`, as it already was everywhere else, because systemd now
waits for the daemon to finish starting.

**A watchdog restart is now a graceful stop** (DEC-388). When the watchdog finds
the control loop hung, systemd now asks the daemon to stop, so its normal shutdown
— the exit minimum included — runs before the restart; if it cannot within 10
seconds it is killed. The daemon's stop timeout rises from 30 to 40 seconds to
cover the two new shutdown steps.

**A thermal emergency stays at 100 % until the CPU is measured cool again**
(DEC-386). If the CPU temperature sensor disappeared entirely during an emergency,
the daemon used to drop the fans to 40 %. It now holds 100 % whether the sensor has
stopped updating or vanished, and the emergency ends only on a fresh reading at or
below 80 °C.

**The emergency no longer steps through 60 % on its way out** (DEC-386). On release
the daemon held your profile's fans at 60 % for two seconds before handing them
back; it now hands them back at once. `thermal_state` no longer reports
`"recovery"` — clients still recognise it from older daemons.

**The 40 % no-CPU-sensor floor now applies only to the fans your active profile
controls** (DEC-382). The 100 % thermal emergency still takes every fan. A 40 %
floor on a fan nothing controls replaced its BIOS curve, which could be running it
faster — and with no CPU temperature sensor at all
(a new AMD CPU on an older kernel, say) every such fan sat at a flat 40 % for as
long as the daemon ran. Those fans now stay under their BIOS curve, and with no
profile active the no-sensor floor forces nothing.

**An NZXT Kraken runs its pump and fans at 100 % after the daemon stops.** Its
kernel driver has no way to hand the device back to its own behaviour, and full
speed is what the pump makers recommend.

**Upgrading:** the first stop after upgrading still runs the previous version's
shutdown, so the next start records whatever that left — Thermal Cruise on an
nct6775 board — until the next reboot resets every header. And because the
daemon running during the upgrade predates the watchdog, systemd restarts it onto
the new version by itself when pacman reloads systemd at the end of the upgrade:
the journal shows one "Watchdog timeout" for the old process. That is expected, and
happens once.

### Documentation

**`ExecStopPost` does run when the daemon crashes and is restarted** (`TS-k`,
DEC-387). The user guide and `daemon.md` said it runs only for a requested stop,
and that after an internal failure only the daemon's own restore gave the fans
back. systemd runs it after every exit, before scheduling any restart — measured
on systemd 261. What it cannot do is rescue a restore that hangs inside the
daemon, which is why that restore is bounded.

## [2.49.1] — 2026-09-17

### Internal

**The two legs of a forced safety write are now distinct types, so they cannot
be passed the wrong way round** (`OFN-ae`, DEC-378). `force_present_backends`
was generified in DEC-371 to fold two inline `if let` arms into one helper, and
bounding both of its type parameters by `SafetyWriteBackend` made them
interchangeable: `force_present_backends(hwmon_be.as_mut(), openfan_be.as_mut(),
..)` compiled cleanly, which was measured before the fix rather than assumed.
Before DEC-371 those arms named `openfan_be` and `hwmon_be` explicitly and the
mistake was unrepresentable.

A swap would have inverted an await order that two other rules are derived from
— `update_serial_timeout_handler` caps the serial timeout at 1000 ms *because*
the OpenFan leg runs first, and `health/staleness.rs` builds its
worst-legitimate-tick budget from the same sequence — and would have inverted
the two `ForcedScope` labels, so the operator line printed after a forced write
would have named the backend that was not driven.

Two empty marker traits, `OpenFanSafetyWrite` and `HwmonSafetyWrite`, each with
exactly one production implementor, now bound the parameters separately. A
swapped call site fails to **compile** rather than being merely easier to spot
in review. Zero runtime change, no new dependency: today's call site was already
correct, so this is latent-defect hardening on the force-all path, not a fix to
observable behaviour. Verified by re-running the swap after the fix and reading
which error fired — `HwmonBackend: OpenFanSafetyWrite is not satisfied`,
`required by a bound in force_present_backends`.

The one-implementor-per-leg invariant that makes the compiler's answer correct is
itself unenforceable in Rust — a trait cannot be sealed against a second in-crate
impl and negative impls are nightly — so a second `impl OpenFanSafetyWrite for
HwmonBackend {}` would silently reopen `OFN-ae` with nothing failing, the
invariant having no runtime signature for CI or the parity oracle to see. It is
pinned by `each_safety_leg_has_exactly_one_production_implementor`, which asserts
the implementor **set** in impl position at line start (raised as a P3 by
`ofc:concurrency-reviewer` in this change's own review).

### Tests

**The tray's process-liveness budget is 30 s, held in one named const**
(`T1-r`). `tray/tests/launch.rs` gave two spawned `/bin/sh` processes 5 s to
prove they ran, which is not enough under a saturated `cargo test
--all-targets`: `launching_twice_starts_two_processes` failed once in two
consecutive full-gate runs during DEC-370, then passed on re-run, passed alone,
and passed 6/6 concurrently as a standalone binary — wall-clock contention, not
a defect in `ProcessLauncher`.

**This widens a failure budget; it does not remove the wall clock.** The
structural fix — waiting on the spawned `Child` — is not reachable, because
`GuiLauncher::launch` returns `Result<(), LaunchError>` and the `Child` is moved
into a detached reaper thread, so no handle ever reaches a test. It costs
nothing on the happy path: `wait_until` returns on the first true poll and the
whole binary finishes in 0.01 s idle, so only a run that was going to fail
anyway spends the difference. 30 s matches the existing precedent in
`daemon/src/main.rs`. One const rather than two literals because
`an_absolute_path_is_launched_and_reaped` shares the hazard and was simply the
one not yet observed failing.

### Documentation

**The OpenFan reconnect backoff was described in seconds; it is a count of poll
ticks** (`OFN-aj`). `reconnect_backoff = (reconnect_backoff * 2).min(30)` is
consumed by `attempts_reconnect_this_cycle` as a number of *loop cycles*, and
the loop's period is `polling.poll_interval_ms`. So the documented "1s..30s"
holds only at the default 1000 ms interval — at the API's 250 ms floor the real
range is 250 ms–7.5 s, and the admin file allows 100 ms (→ 100 ms–3 s). An
operator who has tuned the poll interval was reading a reconnect-timing bound
that does not hold for them. `polling.rs`'s doc comment now states the cap in
poll intervals and gives the seconds reading as a default-interval example.
That comment is the site worth fixing first because it is what the next doc
writer copies — which is exactly how the figure reached the GUI's
`daemon-end-to-end.md`, where DEC-375 corrected it in its own diff. The GUI
manual carries the matching correction in the same window.

No behaviour change: the 30-cycle cap, `attempts_reconnect_this_cycle` and every
config clamp are untouched. `polling.rs:860` already said "cycles" and is
unchanged.

## [2.49.0] — 2026-09-17

### Fixed

**A board whose every `pwmN` is read-only was told the daemon could control it,
twice over.** Two fields answered "can this daemon write hwmon?" from header
*presence* rather than from whether any header is actually writable, and on such
a board those are different answers.

- **`skipped_controls[]` could never report an `hwmon:` control as
  `backend_unavailable`** (`OFN-ah`). The profile engine built its hwmon backend
  from any non-empty discovery result, so the `hwmon_available` flag it hands the
  deliverability rule was `true` even where nothing could be driven. The control
  therefore resolved, computed an output, published it in `control_outputs[]` —
  and delivered nothing, silently. That is the silence `backend_unavailable`
  exists to remove (2.47.0, `OFN-j`), and it also broke the 277-k invariant that
  a control listed as skipped is absent from `control_outputs[]`: the daemon
  published a duty nothing applied. `docs/08` has promised the hwmon case in
  those exact words since 2.47.0; it is now true. The engine takes no hwmon
  backend on such a board, so hwmon controls there are listed as
  `backend_unavailable` and drop out of `control_outputs[]`.
- **`devices.hwmon.write_support` and `features.hwmon_write_supported` claimed a
  write path that does not exist** (`OFN-ak`). Both were the same expression as
  `devices.hwmon.present`, so no client could distinguish the two — any branch of
  the form `present and not write_support` was structurally unreachable, which
  includes the GUI's "motherboard fan headers detected but all are read-only"
  banner. Both now report "at least one writable header". `present` is unchanged:
  a read-only header is a real header and is still listed.

**Client-visible on an affected board, and only there.** A machine with at least
one writable `pwmN` — every machine the project has been tested on — sees no
change in any field. The emergency's reach is unchanged on every board: the
forced write already filtered to writable headers (DEC-295), so the write now
skipped is one that would have driven nothing.

All three answers are derived from one predicate,
`HwmonPwmController::forced_target_ids()`, which is also the set the thermal force
writes — so the claim and the write cannot drift (DEC-334, one flag one gating
shape). DEC-372's `SafetyWriteBackend::has_forced_targets` is retired with it:
gating construction makes it unconditionally true on both backends, and its
invariant moved to the gate's own test.

DEC-376. Register rows `OFN-ah` and `OFN-ak`, from the 2026-09-16/17
`/ofc:investigate-bug` sessions.

## [2.48.1] — 2026-09-16

### Documentation

**The man page described the thermal ladder in two ways that were wrong, plus one
that was missing.** `control-ofc-daemon(1)` said the post-emergency 60% recovery
floor holds "for one cycle" — it holds **two**, the release cycle and one more —
and said the emergency "drives all fans to 100% PWM", which GPU fans are
structurally excluded from (DEC-130: `GpuBackend` does not implement
`SafetyWriteBackend`). It also omitted DEC-307 entirely: all three forced duties
are **floors** over the active profile's output, `max(commanded, forced)`, never
replacements for it — which is what stops the 60% and 40% rungs ever *lowering*
cooling. Every other prose site in both repos already stated all three correctly,
so this was one stale page rather than a disagreement about the rule. **No daemon
behaviour changed**; only what the shipped man page claims about it.

`daemon.md`'s module map also gained the three current modules it omitted —
`serial/adoption.rs`, `hwmon/plausibility.rs` (the cross-sensor filter that stops a
bogus-LOW CPU reading defeating DEC-190's 40% fallback) and `hwmon/power.rs` — and
its "25C hysteresis" aside no longer bakes in `105 − 80`, an arithmetic DEC-308's
per-machine trip point falsifies on any part that derives a higher limit.

Register rows `DOC-a`, `DOC-b`, `DOC-c` and `DOC-i`, from the 2026-09-16
`/ofc:docs-correctness` audit.

**The thermal emergency's reach is now described the same way everywhere.** DEC-371
qualified it in the daemon's own source — the ladder drives every OpenFan channel and
writable hwmon header **the machine has**, which on a GPU-only box or a board whose
every `pwmN` is read-only is none at all — but roughly a dozen prose sites across both
repos still said "all OpenFan and hwmon fans" without the qualifier. They were
*unqualified* rather than false ("all OpenFan channels" over a machine with none is
vacuously true), and `docs/08` had reached the point of contradicting itself: one
paragraph carried the qualifier and two did not. **Nothing about what the daemon does
has changed, and the reach itself is unchanged** — DEC-307 still gives every output
`max(commanded, forced)`, and an output no control commands still receives the forced
duty, which is what preserves the emergency's reach. Register row `OFN-ag`.

### Internal

**Nothing you can see changed, and nothing the daemon does changed.** A PWM
verify, a characterisation sweep and a control-path discovery all behave exactly
as they did in v2.48.0.

The two diagnostics a validation session can orchestrate each watch their run to
completion, and those two watch loops were near-identical copies. That is not a
theoretical cost: daemon v2.43.6 had to apply one fix to both of them, and the
register carried it as two separate defects because one rule in two copies is two
rules to anyone counting. They are now one loop, so the next fix lands once.

The rule that loop enforces is the one worth naming: **a diagnostic must not
outlive the session that asked for it.** A sweep left running keeps driving the
header and keeps renewing the engine's write-pause, which suspends curve control
after the user has already ended the session. (It never suspended *cooling* —
the thermal emergency's forced-duty branch runs above that gate.) Both exits
where that can happen — the session ending, and the orchestrator giving up at its
deadline — cancel the sweep, as before; what is new is that both are now proved
by tests that actually run the loop, rather than by guards that scanned the
source for the call. DEC-374, register row `P8-bu`.

## [2.48.0] — 2026-09-16

**A PWM verify no longer says "PWM values held" when it could not read the header
back (DEC-373, register row `ACK-m`). No change to fan control, to any commanded
duty, or to the thermal emergency.** Pairs with GUI v2.76.0, but needs no
particular GUI — an older one renders the new result and says it needs upgrading.

**What a verify told you could be more than it had checked.** The PWM verify
writes a test duty, waits six seconds, reads the header back, then restores it.
It decides what happened by running two checks — did the firmware take the
header back, and did the value get clamped — and then looking at the tach.

Both checks are written so that *no value* means *no problem*. That is right when
the file simply is not there, which is ordinary on some headers. It is wrong when
the file is there and the read **failed** — a transient I/O error, or the sensor
chip being removed or its driver unbound while the test ran. In that case the
checks were skipped rather than passed, and on a header with no usable tach the
verify still answered:

> PWM values held but RPM sensor unavailable or too low to verify

Nothing had established that the values held. The verify reported a check it
never performed.

**The daemon now says so.** That case gets its own result,
`pwm_readback_unavailable`, which tells you plainly that the readback failed, so
whether the write held is unknown — and that this is **not** evidence the write
failed or that your firmware interfered. Re-running is the fix; if it repeats,
`dmesg` will usually show the chip's driver going away.

Two readbacks reach it: `pwmN` itself, and a `pwmN_enable` that was readable
*before* the write and not after. **A header with no `pwmN_enable` file at all is
ordinary hardware and is unaffected** — it reads absent both times and still
reports `rpm_unavailable`.

**A fan that visibly responded is still a pass.** The new result is used only
where `rpm_unavailable` would have been. If the tach moved after the write, that
is evidence in its own right, and the verify still reports `effective` even
though the readback failed. GPU fan verify already handled this case correctly
and is unchanged.

### Contract

- `POST /hwmon/{header_id}/verify` gains a sixth `result` token,
  `pwm_readback_unavailable`. **No capability flag**, and none is needed: a
  client that does not know it must render it rather than drop it (273-i), and
  the GUI's unknown-token path already treats it as neutral and inconclusive,
  which is the correct verdict.
- `rpm_unavailable`'s documented meaning **narrows** and is now exact — both
  guards passed, only the tach confirmation is missing.
- `docs/08_API_Integration_Contract.md` updated in the GUI repo in the same
  window.

## [2.47.6] — 2026-09-16

**The thermal-emergency log lines claimed a reach the daemon did not have
(DEC-371 and its two review follow-ups, DEC-372; register rows `OFN-n`,
`OFN-ad`, `OFN-af`). No change to fan control, to any commanded duty, or to the
reach of the thermal emergency itself.**

**What an operator was told during a thermal event overstated what the daemon
did.** All three safety log lines read *"forcing all OpenFan+hwmon fans to N%"*
regardless of which backends existed — so on a machine with no OpenFanController,
which is most machines, the highest-stakes message this daemon emits claimed a
reach it did not have. The line was already correct about the *threshold*, which
it interpolates per-machine rather than restating (DEC-292/DEC-308); this is the
same drift one noun over.

The engine's line now reports what it **actually wrote** — `all OpenFan channels
and writable hwmon headers`, `all writable hwmon headers`, or `all OpenFan
channels`, derived per tick from the backends actually asked to force. The two
lines emitted from pure decision functions, which have no view of the backends,
now state the duty and claim no reach: *"THERMAL EMERGENCY: CPU Tctl 106.2°C >=
105°C — forcing fans to 100%"*.

**A machine where the emergency reaches nothing now says so.** With neither
backend present — a GPU-only box, or a VM with no fan hardware — the ladder
latches, publishes `thermal_state: "emergency"` and writes to no fan at all (GPU
fans are excluded by design, DEC-130). That case previously produced the most
false version of the message; it now logs at ERROR: *"Thermal safety override
reached NO fans — this daemon has no writable fan backend…"*. No version of this
daemon has previously reported it.

**A board whose every `pwmN` is read-only reaches that alarm too (`OFN-ad`).**
The daemon builds its hwmon backend from any discovered header without consulting
writability — `main.rs` gates that on discovering *any* header, not a writable one
— so such a board has a real backend that writes nothing, and would otherwise have
been reported as driven. The reported scope comes from whether a backend has **at
least one output it can actually drive**, rather than merely from which backends
were asked to force. The forced write itself is unchanged: every present backend
is still asked to force, whatever it reports about its targets.

**The forced branch logged every second for the whole hold, with nothing
throttling it (`OFN-af`).** There is no in-process rate limiter and the packaged
unit sets no `LogRateLimit*`, so journald's default never engaged at 1 Hz — and
on a VM, where a missing CPU sensor forces the no-sensor floor indefinitely, the
line repeated forever. It is now first / periodic summary / recovery, keyed on
the forced duty **and** the driven backend set, so the ladder's 100 → 60 step and
a mid-hold OpenFan adoption are still announced the moment they happen rather
than waiting for a summary. Summaries fall due every
`THERMAL_FORCE_LOG_SUMMARY_TICKS` (60) ticks — deliberately more often than the
write-failure throttle, because a thermal event outranks one.

**Forcing now reports when it ends.** *"Thermal force ended after N tick(s)"*.
`safety.rs` already logged the emergency's release at 80 °C, but forcing
continues for two recovery ticks past that and a no-sensor force ending logged
nothing at all, so nothing ever said the fans were back under the profile.

**Unchanged and verified:** the two forced writes happen in the same order, with
the same floor and the same baseline. The OpenFan-before-hwmon ordering that
`update_serial_timeout_handler`'s 1000 ms ceiling depends on is now pinned by a
test rather than left implicit in two adjacent statements. Confirmed by the
parity oracle and by the v2.38.0 P1 regression test that pins the emergency's
reach to uncommanded OpenFan channels.

`SafetyWriteBackend::force_all_with_floor` is now spelled with an explicit
`+ Send` bound on its returned future (compile-time only; both implementations
are unchanged), without which the new generic helper could not live inside the
spawned engine loop.

`OFN-ah` — an unrelated contract divergence found while researching `OFN-ad`, in
which a control bound to read-only headers is not reported as
`backend_unavailable` — is **deliberately still open**. The gating fix that would
have closed it changes what `/status` publishes, and was judged a separate
decision.

## [2.47.4] — 2026-09-16

**Three truthfulness fixes on surfaces a client or a user reads (DEC-370;
register rows `OFN-k`, `OFN-u`, `T1-q`). No change to fan control, to any
commanded duty, or to the thermal emergency.**

**`GET /capabilities` advertised OpenFan hardware that was not there (`OFN-k`).**
`devices.openfan.channels` and `rpm_support` were the literals `10` and `true`
regardless of whether a controller was attached — only `write_support` was
derived — so a machine that has never had an OpenFanController reported ten
RPM-capable channels. Both fields are now derived from presence: `0` and `false`
with no controller, and the present-branch count comes from the protocol layer's
own `NUM_CHANNELS` rather than a second literal that could drift away from the
first. **No user-visible change in this GUI**, which reads `channels` only inside
its `present` branch; the point is that the next client to forget that check is
no longer lied to. `control.openfan_rescan` stays hardcoded `true` and is
correct: it advertises the *endpoint*, not the hardware, and a controller-less
machine is exactly where that action must still be offered.

**The rescan cooldown blamed the user for a probe the daemon had made
(`OFN-u`).** `POST /fans/openfan/rescan` refuses a repeat probe over an unchanged
port set with a `409`, and the message read *"an OpenFan rescan over the same
ports was attempted moments ago"* — but the post-boot adoption loop probes
through the same handler, so the probe being rationed is frequently the daemon's
own. It now reads *"a probe over the same ports was attempted moments ago"*. The
rationing itself is unchanged, and `retryable: true` and the named wait are
unchanged. `docs/08_API_Integration_Contract.md` (GUI repo) is updated in the
same change, including a correction it had been owed since 2026-09-12: that
document claimed a retry over newly attached hardware is "*not* refused", which
is false once the loop has probed first — `RescanGuard`'s drop re-stamps the
cooldown with the **new** candidate set, so a user clicking inside the 10-second
window meets `elapsed < COOLDOWN && same_port_set(new)` and is refused.

**The zsh completion offered a `--opt=value` form the daemon silently ignores
(`T1-q`).** `_control-ofc-daemon` declared `--config=`, `--profile=` and
`--profile-file=`, and in `_arguments` a trailing `=` means the value may be
joined to the option name with an equals sign. `resolve_config_path` and
`parse_profile_arg` match those flags as exact strings and never split on `=`,
so accepting the completion literally produced `--profile=name`, which fell
through the parser's catch-all arm — the daemon booted with no profile and said
nothing. The three specs now advertise only the separated form the parser
accepts. The bash and fish completions were already correct, and the tray's
instance was fixed in 2.44.2. A new guard fails on *any* joining suffix in
*any* zsh completion, so a flag added tomorrow cannot reintroduce it.

## [2.47.3] — 2026-09-16

**Three shutdown- and executor-correctness fixes in the OpenFan adoption path
(DEC-368; register rows `OFN-t`, `OFN-v`, `OFN-x`), plus one developer-facing fix
(DEC-369, `OFN-ab`). No change to fan control, to
any commanded duty, or to the thermal emergency.**

**An OpenFanController adopted in the instant the daemon was shutting down could
leave a poll loop nothing ever joined (`OFN-t`).** `main` collected the poll
handles of post-boot adoptions into its shutdown list and only *then* began the
shutdown sequence, whose first act is to signal every loop to stop — so the
collection provably ran before the signal, and an adoption completing in between
registered a handle into a list nothing would read again. Nothing on the install
path consulted a shutdown signal at all, so a controller could be installed, and
its poll loop started, after the daemon had already handed the motherboard fans
back to firmware. Harmless in practice — that loop only reads RPM — but the
guarantee that "the hardware restore is the last writer" was not actually
established for it. Closing the list and taking it are now one indivisible step,
and so are checking-then-registering, so an adoption racing shutdown is either
drained with everything else or refused outright. A rescan arriving once shutdown
has begun is now refused *before* anything touches the serial bus, and answers
`503` with a message saying so rather than the misleading "no OpenFanController
found" — each probe opens every candidate tty and asserts DTR, which resets
Arduino-class boards, and doing that on behalf of an exiting daemon buys nothing.

**A `systemctl stop` landing while the post-boot search was mid-probe added up to
three seconds to shutdown (`OFN-v`).** The background search waited for its probe
unconditionally, and that task is drained before the hardware restore runs — so
the restore sat behind it. It now stops waiting on shutdown, and on its own
adoption-window deadline, which a long probe could previously overrun — at both
points it waits, the probe and the port enumeration. The probe itself is still
never cancelled: it is detached precisely so a caller that gives up does not
discard a controller that was found. Relatedly, the "nothing appeared, try a
rescan" log line is no longer emitted when the window closes while a probe is
still running, because that probe may yet adopt something.

**Serial enumeration no longer runs on an async worker thread (`OFN-x`).** Listing
candidate ports opens no candidate — that is what lets the rescan cooldown ration
DTR resets, and it is unchanged — but the underlying `available_ports()` opens the
devnode of any legacy `serial8250` tty before the USB-serial filter applies. The
post-boot search runs that every five seconds for the whole adoption window, so it
now runs on the blocking pool instead of parking a runtime worker on somebody's
serial bus. The packaged unit already blocked those opens
(`DeviceAllow=char-ttyACM/ttyUSB`), so this was a development- and
container-only exposure. An enumeration that fails now **skips** the tick rather
than reading as "no ports": an empty list would have dropped a configured
`[serial] port` as well, and would have made each recovery look like the hardware
changing, which is what earns a fresh round of probes.

**Running the test suite no longer resets your serial hardware (`OFN-ab`).** Three
tests drove `POST /fans/openfan/rescan` against whatever was actually plugged into
the machine — six probes per `cargo test` run, each opening every `ttyACM`/`ttyUSB`
and asserting DTR, which resets Arduino-class boards. Anyone running the project's
own test suite on a machine with an OpenFanController attached was resetting it
every time. The rescan handler now takes its port enumeration and its serial open
as parameters, so those tests drive a fake bus and keep every assertion they had;
real serial opens in the suite are down from six per run to none. No change to the
endpoint's behaviour, its payloads or its status codes — the shipped path passes
the same two functions it always called directly.

## [2.47.2] — 2026-09-16

**No daemon behaviour change whatsoever — one test fixture, updated so it still
matches the GUI's copy byte for byte.**

`daemon/tests/fixtures/wire_fields.json` is a shared oracle: one byte-identical
copy in each repository, compared by `parity.yml` in both (`P8-cb`). The GUI's
DEC-367 reclassified every field in it — each is now marked as read, deliberately
unread, or unprovable, so adding a field to the wire forces that choice — and this
release carries the matching copy. `api/responses.rs::tests::wire_field_surface_is_pinned`
reads only the `daemon` and `fields` keys and is unaffected; it passes against the
new file unchanged. No source and no API surface changed. `tray/Cargo.toml` moves
to 2.47.2 with the daemon because the two ship together and
`packaging_version::tray_version_matches_daemon_version` requires it.

## [2.47.1] — 2026-09-12

**The post-boot OpenFan search now gets all the retries it was designed to have
(DEC-362, register row `OFN-w`).** v2.47.0 gave the background search a budget of
three handshake retries, for the board whose tty enumerates a moment before its
firmware answers. The budget was spent per *tick* rather than per probe that
actually ran — and because the loop ticks faster than the rescan cooldown that
refuses repeat probes, most of those ticks opened nothing. The four intended
probes were two in practice, or one where a slow probe overran a tick, so a
controller attached during the window could get a single attempt: the one most
likely to fail, since opening the tty resets the board.

The loop now reads the cooldown stamp either side of each call and spends a retry
only when a probe really ran. The bound is unchanged — still one fresh probe plus
three retries, and still only after the set of serial devices changes — so the
DTR-reset saving that motivated the whole design is untouched. The ticker also
takes `MissedTickBehavior::Skip`, so a slow probe is no longer followed by a
burst of catch-up ticks.

No API, capability or configuration change. Pairs with `control-ofc-gui` >= v2.23.0,
unchanged — this release advertises no new capability.

## [2.47.0] — 2026-09-12

**Additive `skipped_controls[].reason` token; `API_VERSION` unchanged.** No
capability, endpoint or error code moves, and no existing field changes shape. A
client that already renders unrecognised tokens — which the contract has always
required — needs no change.

**The OpenFanController is optional hardware, and a machine without one now says
so once, quietly, instead of behaving like something is broken.** Five changes,
four of them on the boot path (register `OFN`):

- **Boot no longer stalls for ~31 s on a machine with no controller.** The
  adoption ladder slept 1+2+4+8+16 s across six attempts with **no early exit** —
  an empty candidate list suppressed a log line, never the loop. The socket is
  bound before the ladder but `axum::serve` is not spawned until after it, so a
  client connecting during that window waited rather than being refused, and a
  GUI autostarted at login could render as disconnected. The engine — the sole
  PWM writer, and the evaluator of the thermal emergency — also starts after the
  ladder, so its evaluation began ~31 s late on these machines. **Startup now
  makes exactly one attempt**, for everyone, configured port or not.
- **The rest of the search moved off the critical path entirely.** A missed boot
  adoption used to be terminal for the process lifetime — the only automatic
  reconnect probe lives inside the OpenFan poll loop, which is only spawned when
  boot already adopted something, so a controller that enumerated late was never
  found and the thermal emergency never reached its fans. A detached
  `post_boot_adoption_loop` now keeps looking once the API server is up, for 60 s
  (180 s with a `[serial] port` configured) — a far **longer** search than the
  ~31 s ladder it replaces, at zero boot cost. It drives `POST
  /fans/openfan/rescan`'s own handler rather than probing directly, so it cannot
  skip the DEC-250 identity handshake, the DEC-266 conditional install, the
  poll-loop spawn or the 277-c handle registration, and it shares that endpoint's
  single-flight guard with a user-triggered rescan. It **compares the candidate
  set itself** and probes only when that set changes (plus a small retry budget
  for a board whose tty appears before its firmware answers), so a machine whose
  serial devices never change is never re-probed and `OFN-b`'s saving survives a
  generous window, while a newly attached controller is picked up within ~5 s.
  This also removes
  the ~31 s stall a **configured-but-absent** port imposed on every boot, which
  blocked the API server, both poll loops and the profile engine.
- **Unrelated USB-serial devices are no longer opened up to twelve times per
  boot.** Opening a tty asserts DTR, which resets Arduino-class boards, and
  `auto_detect_port` returned from its libudev pass only on *success* — so a
  machine where nothing identified fell through to its own `/dev/ttyACM0..9` +
  `/dev/ttyUSB0..9` scan and opened every one of the same nodes a second time,
  six attempts deep. Boot now uses the non-opening enumerator DEC-291 built for
  `POST /fans/openfan/rescan` and opens each candidate at most once per attempt;
  `auto_detect_port` itself (still used by the reconnect probe) de-duplicates.
- **Absence is logged at `info`, not `warn`.** The unit ships `RUST_LOG=info` to
  journald, so "No OpenFanController found" appeared in `systemctl status` and
  every support bundle for every hwmon-only user. A **configured** port that does
  not yield a controller stays a warning, and DEC-250's identity-rejection
  warning is unchanged for that port — a tty that opens but is not an
  OpenFanController accepts every write with `Ok`, so that one is a safety
  signal. An auto-enumerated stranger that fails to identify is now `debug`.
- **A control that can deliver to nothing is no longer silent** — new
  `skipped_controls[].reason` token **`backend_unavailable`**. The write phase is
  `if let Some(be) = openfan_be`, so on a machine with no controller an
  `openfan:` member's commands were built, never delivered, never logged and
  never listed: no journal line, no `/status` field, nothing. Reachable by
  ordinary means — a profile exported from a machine that has a controller
  imports cleanly onto one that does not. Deliberately **not** OpenFan-specific:
  an `hwmon:` member on a board with no writable header reports identically.
  Raised only when **every** member is undeliverable; a partly-live control is
  still commanding fans, so it is logged once per activation rather than listed,
  because `/status`'s contract for that list is that nothing is commanded.

Quietening the boot message is only safe *because* of the last item: the case
that genuinely warrants a warning — a profile that expects hardware which is not
there — now has one, and it fires whenever the profile is active rather than only
at startup.

## [2.46.0] — 2026-09-12

**Additive wire field on `/diagnostics/hardware`; `API_VERSION` unchanged.** No
capability, endpoint or error code moves, and no existing field changes shape.
An older GUI ignores it.

**BIOS reclaim counts are now dated.** `hwmon.enable_revert_counts` is
cumulative for the life of the PWM controller and has no reset path — a rescan
deliberately does not rebuild the controller — so a client reading it alone
could not tell a BIOS actively fighting for a header from one reclaim hours ago
that the watchdog had already remediated, and had to present both identically,
for as long as the daemon kept running.

`hwmon.enable_revert_last_seen_ms` reports, per header, the age in milliseconds
of the most recent **counted** reclaim. It dates the evidence rather than
discarding it: the count is unchanged and still cumulative.

Two details that are easy to get wrong, so they are fixed here by construction:

- It is the age of the **reclaim**, not of the last log line. The watchdog
  throttles logging — first reclaim WARN, the rest DEBUG, one summary per
  interval — so a timestamp taken from emission would stop advancing on exactly
  the busy headers whose activity matters most.
- It shares a lifetime with the counts. Both live on the controller and reset
  together, so an age can never describe a different daemon's count.

The field is omitted when empty, and a header that has a count but no age means
the age is **unknown** — never "just now". The daemon takes no view on what
counts as old; that is the client's policy.

## [2.45.0] — 2026-09-11

**Additive wire field on `/status` + `/poll`; `API_VERSION` unchanged.** No
capability, endpoint or error code moves, and no existing field changes shape. An
older client ignores the new key. Pairs with `control-ofc-gui` >= v2.23.0; the GUI
reads the new field from v2.69.0.

**`has_active_profile` on `/status` and `/poll` (`CTRL-d`).** DEC-194 mirrors the
active profile's id and name onto the poll surface and omits both keys whenever no
profile is active. That left clients unable to tell "this daemon is running
nothing" from "this daemon predates the mirror" — so a client could not safely
clear a profile name it had already shown, and the control-ofc GUI left a stale
one in its sidebar and status banner until it reconnected.

The new field is **always serialised, including when false**, for the same reason
`verify_active` is: it is the *presence* of the key that tells a client this daemon
reports the state at all. Three readings:

| wire | meaning |
|---|---|
| key absent | daemon < 2.45.0 — unknown; fall back to `GET /profile/active` |
| `false` | authoritatively nothing is active |
| `true` | `active_profile_id` and `active_profile_name` are present and current |

`active_profile_id` itself is deliberately **unchanged** — it is still omitted
rather than serialised as `""`, because `control-ofc-tray` reads
`active_profile_id.is_none()` as "no profile is active" and an empty string is
`is_some()`.

`control-ofc-tray` is bumped to 2.45.0 with no behaviour change — it ships in this
package and its version is kept in lockstep (`packaging_version.rs`).

See `control-ofc-gui/DECISIONS.md` DEC-355 (the canonical ADR log for both repos).

## [2.44.2] — 2026-09-11

**No wire shape, capability or error code changes; neither the daemon nor the
tray binary is touched.** Packaging only. Pairs with `control-ofc-gui` >= v2.23.0
exactly as 2.44.1 did.

**`control-ofc-tray` now tab-completes (`T1-f`).** The tray has had its own CLI
since it shipped — `--socket <PATH>`, `--version`, `-h`/`--help` — but the
package installed completions for `control-ofc-daemon` only, so typing
`control-ofc-tray --<TAB>` in bash, zsh or fish offered nothing. Three files now
install alongside the daemon's own, in the same three directories.

Two guards ship with them, because a completion file is the kind of thing that
silently stops describing its program. The flag list is **extracted from the
tray's own argument parser** rather than restated in the test, so adding a flag
and forgetting the completions fails at commit time; and a second guard checks
all three files are actually installed by `PKGBUILD`, since files that exist but
are never packaged would look exactly like this fix being done.

## [2.44.1] — 2026-09-11

**No wire shape, capability or error code changes; the daemon itself is
untouched.** Every change here is inside `control-ofc-tray`. Pairs with
`control-ofc-gui` >= v2.23.0 exactly as 2.44.0 did.

**The tray's journal is readable again (DEC-353).** `control-ofc-tray` set its
log filter globally, so `zbus` — which it reaches through `ksni` — logged its
D-Bus handshake and *every* dispatched method call at INFO, raw message bodies
included. Measured on a live Plasma session: one boot's journal held **11 lines
under `control-ofc-tray` and not one of them was the tray's own**. The default is
now this crate at `info` and everything else at `warn`, so a real `zbus` warning
still appears and the tray's own lines are findable. `RUST_LOG` overrides it as
before — `RUST_LOG=debug control-ofc-tray` is unchanged.

This is listed first because the two fixes below report themselves through the
log, and a log nobody can read is not a report.

**A profile switch the daemon refuses no longer looks like one it applied
(DEC-353).** Clicking a profile closes the menu, and the panel leaves the item
you clicked ticked — so a switch rejected by the daemon (a profile that fails
validation, or one deleted since the menu was drawn) was indistinguishable from a
switch that worked, while the previous profile carried on driving the fans. The
next menu now opens with a line naming what is actually running — "Profile not
switched — 'Balanced' is still active" — and refusing to stop profile control says
so too, in its own words. The line clears as soon as an action succeeds.

The wording comes from the re-read that follows the request, never from the
request's own reply, and that distinction is load-bearing: a tray call is bounded
at 300 ms and the daemon applies a switch before it answers, so on a loaded
machine a switch that *worked* can still report a timeout. Deciding from the reply
would have printed "the previous one is still active" underneath a correctly
ticked new profile. Where the re-read fails as well, the line says only that the
daemon is not answering — it does not guess what is driving the fans.

It appears below the thermal warning where both are present, since fans forced to
maximum is the more urgent fact.

**One tray per user no longer depends on nobody else guessing a name
(DEC-353).** The guard used a Linux abstract-namespace socket, which has no
permission check on a fully predictable name — so any other local user could
claim `control-ofc-tray.<uid>` first and the real owner's tray would then exit
quietly at every login, with systemd recording a clean start. It is now an
advisory lock (`flock`) on a file in `$XDG_RUNTIME_DIR`, which no other user can
write to. Nothing about this is visible in normal use: the lock is released by
the kernel however the process exits, a lock file left behind by a crash is
inert, and a duplicate launch still exits quietly.

If `$XDG_RUNTIME_DIR` is missing or is not private to you, the tray now starts
**without** the guard and says so, rather than trusting a shared location. The
worst case is a second tray icon.

## [2.44.0] — 2026-09-10

**No wire shape, capability or error code changes; pairs with `control-ofc-gui`
>= v2.23.0 as before.** The new tray is a client of endpoints that already
existed, so no daemon API moved and no capability flag was added. It launches
whatever `control-ofc-gui` is installed; with **GUI >= v2.68.0** a second launch
raises the existing window instead of opening a duplicate, which is what makes
clicking the tray icon twice harmless. Older GUIs work — they just open a second
window on a double click, exactly as they do from the application menu today.

**New: `control-ofc-tray`, a system-tray client for KDE Plasma (DEC-352).** The
package now installs a second binary and an autostart entry, giving a persistent
Control-OFC icon in the Plasma system tray: right-click for the profile list with
the active one checked, the **running** daemon's version, and an action to open
the GUI; left-click opens the GUI directly.

**It is a client, not a second controller.** The tray calls `GET /status`,
`GET /profiles`, `POST /profile/activate` and `POST /profile/deactivate`, and
nothing else. It holds no lease, writes no PWM, evaluates no curve and reads no
hardware, and the daemon has no knowledge of it and no dependency on it — killing
the tray, or never starting it, changes nothing about fan control. It lives in
its own workspace crate so that boundary is enforced by the crate graph rather
than by convention.

**No API change was needed.** `daemon_version`, `active_profile_id`,
`active_profile_name` and `thermal_state` were already on `GET /status`, so one
request serves the whole menu.

**It costs nothing when idle.** There is no timer and no polling loop: Plasma
calls `AboutToShow` on every context-menu open, and the tray reads the daemon
there and nowhere else, so an idle tray issues zero requests and runs zero
timers.

Measured on this release build: 5.6 MB stripped, linking **only** `libc`,
`libm`, `libgcc_s` and the loader. No OpenGL, Vulkan, EGL, Qt, GTK, wayland or
Mesa library is linked at all, so the tray cannot interfere with a GPU-heavy
workload.

**Behaviour worth knowing:**

- **Double-click is not a thing the protocol has.** Plasma routes a left click
  straight to `Activate` and the StatusNotifierItem interface has no
  double-click member, so a double-click simply opens the GUI twice. The GUI
  gained a single-instance guard in the same release window (GUI 2.68.0) which
  makes the second one raise the first instead.
- **A thermal event is surfaced.** When `thermal_state` is anything but
  `normal`, the menu carries a line saying so. Switching to a quieter profile
  during a thermal event will not do what it looks like it does — the forced
  duties are floors *over* the profile's output (DEC-307) — so a silent tray
  would be misleading. No temperature is named, because the trip point is
  per-machine (DEC-308).
- **An unreachable daemon is a state, not an error.** The menu says so and keeps
  the GUI action working; there is no retry storm and no crash loop.
- Profiles that share a name are disambiguated by id — `/profiles` is a union
  across search directories deduped by id, so duplicate names are normal.
- Autostart is enabled by default via `/etc/xdg/autostart/`, and is one click to
  disable in System Settings → Autostart. `TryExec=control-ofc-gui` means a
  headless, daemon-only install never starts it.

**Packaging:** `deny.toml` gains `Unlicense` for the `ksni` dependency — a
public-domain dedication, i.e. the opposite of the re-linking obligation DEC-043
exists to block. `tray/Cargo.toml` carries a literal version pinned to the
daemon's by a new guard in `packaging_version.rs`; it is deliberately not
`version.workspace = true`, because an inherited version line makes the release
workflow's textual version check fail after the tag is already public.

## [2.43.8] — 2026-09-10

**Two bounding fixes, both from the open-items register (`P8-cc`, `P8-br`).** One
changes an error a client could not act on; the other closes the last unbounded
term in the validation-session reservation.

**A cooling device the daemon rejects no longer spends a slot the client cannot
free (`P8-cc`).** Devices are sanitised on read, so an entry `validate_device`
rejects — a hand-edited `runtime.toml` line, or a member id over the 256-byte
bound added in 2.43.7 — is absent from `GET /inventory/cooling-devices` and its
id is therefore never disclosed. `POST /config/cooling-device` counted the raw
rows anyway, so sixteen rows with one bad entry published fifteen devices and
answered the next create with `409 already_exists "cooling device limit reached
(16)"`, with nothing visible to `DELETE` to make room. The cap now counts the
sanitised list, so the error means what a client can act on. The bad row is not
deleted — it stays in the file and stays reachable by id — it simply stops
costing a slot.

**Daemon-formatted evidence `detail` prose is now bounded at
`VALIDATION_MAX_TEXT_FIELD_BYTES`, like every other ancillary term (`P8-br`).**
A client's session text was bounded at ingest; the daemon's own `format!`-built
`detail` on an evidence entry and on the characterisation, control-path and
verify runs it carries was bounded by nothing. That is ancillary text the 10 MiB
reservation has to cover, and a document which outgrows it is not truncated but
**deleted**, since `prune` reclaims what it cannot read. `EvidenceRef::clamp_detail`
truncates all four on a character boundary at `attach_evidence_for` — the one
route an entry takes into a session — rather than at the ~15 sites that build a
`SweepOutcome`, so a diagnostic added later is bounded without its author
knowing the rule exists. No measurement moved: the reservation's fixture already
took both of the named fields at this bound, so this brings the code up to what
the fixture was already reserving.

**`constants.rs`' "honest limits" note is not empty, and a draft of this change
said it was.** Review caught the claim before it shipped. `findings[].detail` and
`startup_fingerprints[].interpretation` are daemon-`format!`ed by the same
summariser and clamped by nothing, and two of their terms scale with a vector
rather than a token. They are pre-existing, outside this change's blast radius,
and now recorded as `P8-ce` — not as an all-clear. The note lists them plus
`P8-cd`, the unclamped copy on the live `GET /diagnostics/*` response, which is
not a document term at all.

**Internal:** the UTF-8-safe truncation helper had two byte-identical private
copies (`control_paths.rs`, `pwm_baselines.rs`) and needed a third. It is now one
`text::truncate`, with the character-boundary cases — a bound landing
mid-codepoint, and one below the first character — under test for the first time.

**Two cross-repo pins became interlocks instead of workflows (`P8-cb`).**
`wire_field_surface_is_pinned` carried 29 literal `want` arrays and
`get_config_key_set_and_mutability_are_pinned` carried a literal list of the nine
`GET /config` keys, while the GUI declared the same lists in
`tests/fixtures/wire_fields.json` and `tests/fixtures/daemon_config_keys.json`.
Each side was checked only against its own source and **nothing compared the two**,
so a rename fixed here and forgotten in the GUI fixture left that fixture stale
with both suites green. Both tests now read the fixture — carried here as a
byte-identical copy under `daemon/tests/fixtures/`, the `parity_vectors.json`
shape (DEC-126) — so there is one declaration and no second list to forget. The
wire pin also asserts coverage **both ways**: a struct declared in the fixture
with no arm here fails, and so does an arm here for a struct the fixture does not
declare. `parity.yml` gains both files, and the GUI gains a byte-identity test for
each. Test-side only: no shipped code, no wire shape and no behaviour changes, so
this ships inside 2.43.8 rather than taking a version of its own.

## [2.43.7] — 2026-09-09

**A cooling-device member id is now bounded by LENGTH, not only by list count
(DEC-349, register package `G38` — rows `P8-bs`, `P8-ca`).** One behaviour change
on `POST /config/cooling-device`; the rest is test coverage.

**`validate_device` bounded `name`, `kind`, the three sensors and
`device_policy_id` at 256 bytes and bounded the member lists by entry count
alone** — 32 per list, each entry unbounded. Those ids are copied verbatim into a
validation session's `members[].member_id` and `.label`, `radiator_members`,
`sweep_members` and every `findings[]`, `evidence[]` and `startup_fingerprints[]`
entry — `findings` and `evidence` being the largest terms — which is ancillary text
`VALIDATION_MAX_ANCILLARY_BYTES` has to cover and was the one term of it no
constant bounded. It matters because a document that outgrows the reservation is
not truncated but **deleted** — `prune` reclaims what it cannot read — and
`unknown_member` short-circuits when no hwmon controller is present, so on such a
host the ids were accepted unverified. Every entry of `pump_member`,
`radiator_members[]` and `auxiliary_members[]` is now bounded at the same 256
bytes, `400 validation_error` beyond it. No real id comes close: the daemon's own
stable ids run about 40 bytes, so a client posting ids it discovered from
`/hwmon/headers` or `/fans` cannot hit this.

**The reservation was re-measured rather than re-reasoned.** Its fixture used
*realistic* member ids, because while the ids were unbounded there was no worst
case to measure — so the measurement was an under-count for exactly the term this
change bounds. The fixture now takes them at the ingest bound and asserts it does;
the worst-case document realises **8,047,435 bytes** against the 10 MiB
reservation, up from 7,938,808. The ~106 KB of growth is what the reservation had
been silently carrying as an unknown. `constants.rs` drops member ids from its
"honest limits" list, which is down to one (`P8-br`).

**The Phase 8 wire pin was one-sided and is now bilateral.** `G33` declared 16
structs — preflight, control-path discovery, PWM characterisation, steady state,
the startup fingerprint — in the GUI's `wire_fields.json` only. That catches the
GUI dropping a field it should model; it does not catch the daemon renaming one,
because the fixture is static data and never queries a live daemon, so a rename
left the declared list stale and the GUI test green. `wire_field_surface_is_pinned`
now constructs all 29. Verified to bite: renaming `CharSummary.typical_response_ms`
on the wire reddens it.

**What that does and does not buy, stated precisely.** A daemon-side rename now
reds the Rust test, where before it reddened nothing. It does **not** make the two
declared lists self-synchronising: the Rust arm's `want` list and the GUI fixture's
`fields` list are each checked only against their own side, and nothing compares
them to each other. A developer who renames a field and updates the Rust arm — as
they must, to get green — can still leave the fixture stale. Keeping the two in
step remains manual, and the failure message says so by naming all three places to
update.

**A flaky test on the thermal-emergency path, and a precondition that shared the
defect's blind spot (DEC-348, register package `G37` — row `P8-bw`).** Test only.
No product code, wire shape, capability, error code or runtime behaviour changed.

**`hwmon_force_all_completes_every_header_despite_midscan_verify_preempt` raced
its own preemptor.** It spawned a thread to force-take the hwmon lease "mid-scan"
and asserted that `force_all_with_floor` had re-taken it, but nothing checked the
take had actually landed while the scan was still running — so under full-suite
load the thread could be scheduled after the last header, where `Verify` is the
legitimate owner. Measured at 1 failure in 3 full-suite runs against 3/3 passes
in isolation. The product half was never in doubt: the same failing run's
per-header assertions (every `pwmN` forced to raw `255`) passed. It still
mattered, because a test that reddens at random on the emergency path is one that
gets re-run until green.

The preemptor now snapshots the scan's progress while it still holds the
controller lock, and an attempt whose take arrived too late is retried rather
than asserted on. Exhausting the attempt bound is a failure, not a skip.

**The first draft of that fix reproduced the defect inside its own
precondition**, and the mandated fix-out check is what caught it. It gated the
retry on the write log's final length — a quantity the defect moves, because a
missing lease re-take stops the scan writing at the preempt. With the fix
disabled the gate could never open and the test failed with "the window never
opened", pointing at the harness instead of at the missing re-take. It now counts
headers already forced at the moment of the take, which is fixed before the
defect can act; the same disabled-fix run fails with `header pwm2 was not forced
(partial-write bug)`.

**And that message used to name a different header on every run.** The test
fixture gave all eight headers the same `(chip_name, pwm_index)` sort key, so
`headers()`' stable sort left them in `HashMap` order, reseeded per process. The
scan order — and therefore the header named in the failure — was random. Nothing
the tests assert depended on it, but a regression test's red has to be
reproducible to serve as evidence. The fixture now sets a distinct `pwm_index`.


## [2.43.6] — 2026-09-09

**In-code documentation: a retracted safety claim standing at three sites, a
worst case that was really a typical case, and one new guard (DEC-347, register
package `G31` — rows `P8-ao`, `P8-bt`, `P8-as`).** Comments and one test. No
wire shape, capability, error code or runtime behaviour change.

**"The recorder performs no sysfs I/O" was retracted by DEC-335 and still stood
in three places.** `validation/recorder.rs` has documented the narrower
invariant that actually holds since Batch 3a added power sampling — every read
is read-only and none happens under the session slot guard — but the two safety
arguments that *rest* on it (`main.rs`'s spawn site and the `AppState` field
doc) and the prose in `daemon.md` still asserted the retracted form. All three
now state the narrowed invariant and note that it only became true at `start`
with DEC-342.

**Six off-runtime comments called ~5.7 MiB an upper bound.** It is a realistic
two-member session; the document is bounded by `VALIDATION_MAX_SESSION_BYTES` at
28 MiB, and DEC-341 measured the worst case at 24,710,972 bytes. Each of the six
now names the constant. The bare sweep over `daemon/src` **and** `daemon/tests`
returns fifteen sites and all fifteen were read — the other nine already name the
bounding constant or record a measured historical defect, and were deliberately
left alone.

**`GET /diagnostics/preflight` now has a test saying which status codes it may
answer.** The GUI calls that route unconditionally and maps any `404` to "this
daemon has no preflight", which is only sound because `preflight_handler` has no
`404` branch of its own — a property that was argued in prose in two repos with
nothing checking it. `the_preflight_handler_answers_only_200_or_400` asserts the
set of status codes the handler writes, so a `404`, `409`, `410` or `503` added
later fails with a message naming the GUI behaviour it would break.

**Three diagnostics kept the wrong end of what they were bounding, and two watch
loops leaked the run they were watching (DEC-344, register package `G26` — rows
`P8-ap`, `P8-bk`, `P8-g`, `P8-an`).** Daemon-only. No wire shape, capability or
error-code change; one response's *content* changes, described below.

**A sweep that outlived its watch window was left running (`P8-ap`, `P8-bk`).**
Both session-orchestration watch loops have four exits, and only one of them
cancelled. The session-ended branch returns early having called
`cancel_run_fenced`; the deadline break fell through without it — and that is
the exit reached with the sweep *still running*, so it carried on driving the
header and renewing the engine's write-pause, the exact `AUD3-j` shape the
sibling branch was written to close. Since v2.43.0 it had a second consequence:
with `stop_when_diagnostics_complete` the session was then stamped `completed`
while its diagnostic was still going. **The fix narrows that window rather than
closing it, and the difference is worth stating:** cancelling only *requests* a
stop, and the sweep observes the flag at its next step boundary — up to one
settle, or one stability dwell — so the orchestrator can still reach the terminal
hop first. What changes is that the window is now bounded by one step instead of
by the sweep's entire remaining length. The fix is one fenced cancel after each
loop rather than one inside the break — `cancel_run_fenced` acts only on a
*running* run whose `run_id` matches, so it is a no-op by construction for the
other three exits, and its own documentation names those four cases as its
inputs. Not on the thermal path: the forced-duty branch runs above the
`verify_active` gate, so this was lost control intent and a false record, never
lost cooling.

**A fine-grained characterisation request was served its bottom quarter and told
it was the sweep (`P8-g`).** `resolve_points` capped an over-long duty list by
`truncate`, keeping the first 20 ascending values — so 20..100 in steps of 1 came
back as 20-39%. It now thins across the range, keeping the lowest and highest
requested duties. **This changes what an existing request returns**, which is why
it is called out here and in `docs/08`; the response shape is unchanged and no
client branches on it. `resolve_points` is `[SAFETY]`, and both invariants it
exists to guarantee survive by construction — thinning only ever *selects* from
an already-clamped, already-sorted list, so no point can fall below
`max(CHARACTERIZATION_MIN_PCT, floor)` or reach zero, and the index map is
monotonic so the list stays ascending, which is what makes an aborted sweep leave
the header at the highest duty it reached.

**The pump-stall abort could be disarmed by the observation bound (`P8-an`).**
Control-path discovery bounds its tach set at `DISCOVERY_MAX_TACH_CHANNELS`, and
headers are enumerated before monitor-only fans — so on a board with more
tach-carrying channels than the cap, the *target header's own* channel could sit
past the cut. `target_idx` is derived after that truncation, so losing it made
`had_target_tach` false, and `pump_tach_lost` — the one abort separating
"perturbing a healthy pump" from "the pump has stopped" — is gated on having had
a tach at start and could then never fire. The target is now swapped into the
last retained slot before the bound applies, in lockstep across both vectors. Not
reachable on consumer hardware (the cap is ~4x the largest observed count), which
is why it was P3.

**Internal: the `openfan:ch{NN}` member id has one producer and one parser
(register package `G36` — row `P8-bq`).** No behaviour change and nothing
user-visible; recorded because the mapping had six inline copies, two of them on
the single-writer path, and a seventh would eventually have been written
slightly differently. The id shape is byte-identical and pinned by a test,
because that string is persisted in saved profiles and cooling-device configs.
The shared parser reports its two failure modes apart rather than as one
`Option`, so the profile engine's distinct "malformed member_id" and
"unparseable channel" warnings survive unchanged.

**The shipped example config no longer calls it "the 105 C thermal rule"
(register package `G32` — row `P8-ax`).** `packaging/daemon.toml.example` is
installed as `/etc/control-ofc/daemon.toml`, so this is text an operator reads
on their own machine. DEC-308 made the trip point per-machine — 105 °C is the
floor and the fallback, raised to `min(CPU-reported ceiling + 5, 115)` where the
kernel publishes one — and a name with the old constant baked into it is wrong
on every machine that derives a different one. It now reads "the thermal
emergency", with no figure at all: `emergency_threshold_c` on
`/diagnostics/hardware` is the value to read, and DEC-292's lesson is that a
threshold spelled into a *name* falsifies everywhere at once the moment the
threshold moves.

## [2.43.5] — 2026-09-08

**The active Super-I/O probe no longer writes the config-mode unlock its own
packaging guards against (DEC-343, register package `G34` — row `X87-k`).**
Daemon-only. No wire shape, capability or error code changes; pairs with
`control-ofc-gui` >= v2.23.0 as before.

*Why this is a patch:* a hardening fix on an opt-in diagnostic. No response
shape, status code or written duty changes; the only observable difference is
one extra `notes[]` line on the boards concerned, in an array that has always
been free text.

**`POST /inventory/superio/probe` could write the config-mode unlock that hides
a board's Super-I/O — on the exact boards this package protects from it.** DEC-332 measured
that exact sequence latching an ITE eSPI-to-LPC bridge into configuration mode,
hiding the Super-I/O behind it and every fan header on it until the machine is
disconnected at the wall — a reboot does not clear it — and shipped
`control-ofc-superio-guard` to keep `nct6775`/`w83627ehf` from writing it. That
guard is keyed on the curated DMI board table; the daemon's own probe leg was
not, so the one process that knows the board best could still inflict the damage
its packaging prevents everywhere else. The leg is now keyed on the same table.
On an ITE-only board there is no Nuvoton chip behind that write to find, so the
skip costs nothing; every other board — including every board the table does not
list — keeps the leg, because an unbound Nuvoton chip there is exactly what the
probe exists to diagnose.

The reach is narrow and it was pre-existing: the leg is reached only after the
no-enter read and the ITE unlock both came back no-response, and the whole probe
still needs `allow_port_probe`, `CAP_SYS_RAWIO`, and no recognised Super-I/O
already bound. But the population that reaches it is precisely the one DEC-332
is about — a user hunting headers that a latched bridge has hidden — and the
cost of being wrong is a power cut.

**A withheld leg is now reported, not silent.** Without a note the response
said only "found no unbound Super-I/O chip at 0x2E/0x4E", which is the identical
message a genuinely empty base produces: the one machine where the daemon
deliberately declined to look said nothing about having declined. The response
now carries a `notes[]` entry naming the base and the reason, and the generic
line is suppressed where that fuller answer applies. Clients render `notes[]`
already, so no GUI change is required.

Two shapes of wiring bug are made **unrepresentable** rather than merely tested
for: the policy's field is private, so no call site can write a permissive
literal, and its constructor takes the whole `BoardInfo`, so the vendor and the
board name cannot be transposed (both are `String`, and transposing them reads
as "unknown board" and silently re-permits the write). The third shape — handing
that constructor a board the caller invented — the compiler cannot rule out, so
the probe's board lookup is a parameter and a test drives the whole chain from a
fixture DMI tree. `CLAUDE.md` records fourteen occasions where a rule was
extracted, tested, and then not actually applied where it mattered.

## [2.43.4] — 2026-09-08

**Blocking sysfs I/O no longer runs where it can stall an unrelated request
(DEC-342, register package `G25` — rows `P8-v`, `P8-am`, `P8-al`).** Daemon-only.
No wire shape, capability or error code changes; pairs with `control-ofc-gui` >=
v2.23.0 as before.

*Why this is a patch:* three internal robustness fixes. Every response shape,
every status code and every duty written is unchanged, and no client can observe
a difference except that a wedged sensor now degrades one reading instead of a
subsystem.

**Starting a validation session could be stalled by a wedged power sensor, and
every reader of the session slot with it (`P8-v`).** `start()` cleared the
per-session power sampler while holding the session slot guard, and the recorder
tick holds that same mutex across blocking sysfs reads — the hwmon/powercap
discovery walk, then an `energy_uj` read and an amdgpu `power1_average` read on
every tick. An amdgpu read parked on the SMU lock during a GPU reset therefore
blocked `start` *beneath* the guard, and with it `GET /validation/session`,
`recording_session_id()`, `stop_if()` and the shutdown flush — all
non-cancellable acquisitions. `/status` and `/poll` were never affected (separate
mutex). The sampler is now keyed by session id and `start` does not touch it at
all: the tick rediscovers it on a session change, which is where the per-session
RAPL baseline reset moved to. That reset is why the sampler is not simply
`try_lock`ed and skipped — under exactly the contention it would exist to
survive, a carried-over sampler charges the whole idle gap between two sessions
to the second session's first wattage.

**A wedged tach read during control-path discovery no longer parks a runtime
worker (`P8-am`).** Each observation is up to `DISCOVERY_MAX_TACH_CHANNELS + 3`
= 35 blocking `std::fs` reads, taken every 500 ms for the length of a run
(~186 samples) from a plain spawned task. `AIO3-d` accepted that shape on the
characterisation sweep at **three** reads per sample; discovery multiplies it
~11x over a wider chip set. One tach `open(2)` stuck in a driver parked a tokio
worker uncancellably, starving every unrelated task scheduled on it. The sample
now runs on the blocking pool, the same shape DEC-290 uses for the verify path,
so a stuck header consumes a pool thread — sized for exactly that — instead of a
slice of the runtime. Sample cadence, observation windows and every written duty
are unchanged.

*What this does not fix, stated plainly because the original note implied
otherwise.* A wedged read still stalls the run itself: `keepalive()` cannot be
reached with data that never arrives, so the verify pause and hwmon lease can
still lapse, the run can still be superseded, and its restore can still fail
`InvalidLease` with the header left at the perturbed duty — which is at or above
its floor, so never a stopped pump. That outcome is unchanged by this release. A
watchdog for a genuinely wedged run remains its own open item.

**One unreadable hwmon directory no longer abandons the whole CPU-power scan
(`P8-al`).** The per-chip attribute scan used `read_dir(dir).ok()?`, whose `?`
returned from the entire function rather than skipping that chip, so the first
unreadable directory discarded every chip sorted after it. It was benign in
practice because discovery falls through to powercap RAPL, and on the reference
host RAPL is the only path anyway — but the loop read as though it continued, and
a future caller without that fallback would have lost the branch silently. It now
skips the chip, and reads each directory once instead of once per attribute
suffix.

## [2.43.3] — 2026-09-08

**A validation session's persisted document is bounded by a measurement now, not
by an estimate (DEC-341, register rows `P8-s` and `P8-ae`).** Daemon-only. No
wire shape, capability or error code changes; pairs with `control-ofc-gui` >=
v2.23.0 as before.

*Why this is a patch:* one request field gains a normalisation, and two internal
byte budgets are corrected upward. Nothing a client could previously do is
refused, and no response shape moves.

**Repeated diagnostics in `POST /validation/session` are now dropped at ingest.**
Each token was validated individually and the list length was not, so a request
naming the same diagnostic 2000 times was accepted, copied verbatim into the
session document, and re-scanned once per member by the orchestration walk. The
list is now reduced to at most one of each — the response echoes what was actually
taken, so a caller can see it — and the bound is the size of the diagnostic set
rather than a chosen number. The recorded evidence was never affected: the walk
already collapsed repeats, and only the persisted request list grew.

**The guarantee that a session at every cap reads back was unsound.** The
reservation for non-sample content was derived in prose at ~3.4 MiB against a
4 MiB budget. Measured as a realised file it is **7,938,808 bytes**: an event may
carry two 512-byte text fields rather than one, a measurement four, and
`evidence`, `findings` and `startup_fingerprints` were not counted at all.

*No session was actually being lost, and it is worth being exact about that.* The
full worst-case document measures **24,710,972 bytes** and the read cap was
25,165,824 — it fitted, by 444 KiB. What was wrong is that the compile-time
assertion guaranteeing it fits was computed from a reservation covering 4 MiB of a
7.57 MiB reality, so that 1.8% margin was **accidental rather than designed**: one
more field on an event, or a raised event cap, would have crossed it silently while
the reservation still claimed 4 MiB of room. Since v2.35.0 an unreadable session is
classified `TooLarge` and reclaimed by `prune`, so crossing it would have destroyed
an operator's evidence rather than merely wasted disk. The reservation is now
10 MiB and the read cap 28 MiB, both sized above a measurement a test re-takes.

## [2.43.2] — 2026-09-08

**The safety preflight now reads every fan source, not just the motherboard's
(DEC-340, register row `P8-t`).** No wire shape, capability or error code
changes, and no GUI release is required — the same `supporting_cooling` check,
in the same place, simply stops claiming it could not read siblings it can. Pairs
with `control-ofc-gui` >= v2.23.0 as before.

*Why this is a patch:* the report's shape, its check ids and its five state
tokens are unchanged, and no verdict that was `blocked` becomes runnable —
`supporting_cooling` has never blocked anything and still cannot. What moves is
the truth of one advisory row.

**One preflight you have seen before may now read `warn` where it read `ready`,
and that is the fix working.** A verdict is `warn` if any row warns, and
`unknown` rows do not warn — so a cooler whose radiator fans are all on OpenFan
and all currently stopped (zero-RPM mode is normal below a temperature
threshold) used to produce "state could not be read", which is silent, and now
produces "no sibling member is observably running", which is not. It is the same
report a motherboard-header cooler has always given in that state; the two
merely disagreed because one of them could not see its own fans. **It still does
not block** — Start stays enabled and nothing new is refused.

### Fixed
- **"No sibling member's state could be read" on an AIO whose radiator fans are
  on an OpenFan controller.** The preflight resolved every cooling-device member
  through the hwmon fan cache alone, so on the canonical configuration — pump on
  a motherboard header, radiator fans on OpenFan — every sibling fell through to
  *unreadable*, and the check reported an absence of evidence the daemon was
  publishing on `/poll` in the same second. An operator reads that as a fault in
  their cooling. Members are now resolved against the source that owns them:
  OpenFan channels through the OpenFan cache, GPU fans through the GPU cache,
  everything else through hwmon as before.
- **An OpenFan channel that no poll has confirmed now reports as *unread*, not as
  a confident 0 RPM.** The thermal emergency's `force_all_with_floor` mints a
  cache entry for every channel whether or not the firmware reports it, so
  trusting the value would have reported a stopped radiator fan during the very
  emergency that created the entry.

### Internal
- The "observably moving" rule is now one pure function, `preflight::classify_sibling`,
  shared by all three sources rather than restated per source. It takes a
  **measured** duty readback and never a commanded one — for OpenFan, which
  publishes no readback at all, the tach is the only evidence and the absence of
  one is reported as such.
- **Test: the preflight's call site is now covered** (`P8-ab`). Every existing
  preflight test drove the pure `build_report` against hand-built inputs, leaving
  ~90 lines of live-state mapping — the `[SAFETY]` staleness selection, the
  thermal gates, pump protection, the single-flight slot and this defect —
  executed by nothing. Nine tests now drive `preflight_handler` against a real
  `AppState`, and the writer they are given panics if the preflight ever writes.
- **Test: both persisted stores' boot prune is now pinned** (`P8-ac`). Deleting
  either block from `main.rs` previously left the whole suite green while a stale
  PWM→tach control path or learned baseline survived a board or driver change.

## [2.43.1] — 2026-09-07

**A control-path discovery run now re-checks thermal safety before every write it
issues, not once per cycle (DEC-339, register row `P8-u`).** Pairs with
`control-ofc-gui` >= v2.23.0 as before; no wire shape, capability or error code
changes, so no GUI release is required.

*Why this is a patch where the comparable DEC-336 fix (2.42.0) was a minor:* that
release made the daemon perform a refusal it had been publishing and never
executing, which is a **new reachable outcome** — a `409` a compliant client
could not previously receive. This one adds no outcome. Every state, code and
`detail` string it can produce was already producible; only the timing moves, and
one write is skipped in a case where the engine is the writer anyway. A client
that depended on the old timing would be depending on a race it never had a way
to observe.

### Fixed
- **The perturbed duty could be commanded on a temperature reading up to a full
  observation window old.** `POST /hwmon/{id}/discover-control-path` evaluates
  three thermal gates — the 85 °C voluntary abort, the thermal-ladder force
  check, and the DEC-336 staleness refusal — and they ran once at the top of each
  cycle. A cycle issues **two** PWM writes and holds **two** observation windows,
  so the second write, which is the one that can command a header *down* by up to
  `DISCOVERY_DELTA_MAX_PCT` points, was issued on a reading the gates had last
  looked at up to 15 s earlier, and the resulting duty was then held for a second
  window before anything looked again. On a machine already over 85 °C the daemon
  could therefore actively reduce cooling and take about 30 s to notice. All
  three gates now run before **every** write, so the thermal cadence is exactly
  the keepalive cadence — one evaluation per observation window — and the worst
  case between checks halves from two windows to one. DEC-334 fixed the same
  shape on the characterisation path in 2.40.0; this is its sibling.
- **The end-of-run return-to-baseline write could fight the thermal ladder.**
  That write ran unconditionally except for a shutdown check, one line above the
  restore guard that *does* stand down under an active force and then logs that
  the header was "left at the thermal-safety forced duty" — so the header had in
  fact just been moved off it and the log line was false. It now obeys the same
  force skip as the guard below it.
- **A run whose final observation window was measured under a thermal force
  reported `complete` and persisted its result.** Every write is gated, but the
  last window has no write after it, so nothing re-examined the conditions the
  final measurements were taken under. A ladder engagement force-takes the hwmon
  lease and drives every writable header to `max(commanded, forced)`, and it
  re-asserts `pwm_enable = 1` deliberately — which is precisely the value the
  run's own foreign-writer check treats as "still mine", so it could not notice.
  Every watched tachometer therefore moved for a reason unrelated to the
  perturbation, and the run stored those readings as a durable PWM-to-tachometer
  mapping that later runs and the UI treat as measured fact. A wrong mapping is
  worse than no mapping, so the run now aborts and records nothing.
- **A thermal abort could be reported as a lost lease.** Because the ladder
  force-takes the lease, the liveness check fails on the same condition that
  trips the thermal gate; whichever ran first wrote the abort reason. The
  perturbed window consulted liveness first, so a genuine overheat was reported
  as "superseded by a later diagnostic; this run's lease is gone" — pointing the
  operator at a competing diagnostic instead of at the heat. The thermal gate now
  runs first, matching the characterisation sweep.

## [2.43.0] — 2026-09-07

**Run 2 of the session-lifecycle block (DEC-338).** The daemon half of the defect
GUI v2.65.1 made legible: a validation, lifecycle or thermal session had no
terminating condition. Pairs with `control-ofc-gui` >= v2.23.0 as before — the
floor does not move, because this release is purely additive and changes nothing
for a caller that does not ask for the new option. GUI **v2.66.0** is the version
that offers it.

### Added
- **A session can now finalise itself when its diagnostics finish.**
  `POST /validation/session` accepts `stop_when_diagnostics_complete` (default
  `false`), and the session document echoes it back. Until now a session ended
  only when it was stopped by hand or when it reached the sample cap —
  `VALIDATION_MAX_SAMPLES` (7200) x `VALIDATION_SAMPLE_INTERVAL` (1 s), **a flat
  two hours**, not shortened for a real cooler. With every diagnostic requested
  the orchestration finishes in about four minutes and the recorder then ran for
  the remaining hour and fifty-six. The default stays `false` so an older client
  and every `curl` user get exactly the previous behaviour.
  - The stop fires **only when the orchestration walk completes normally**.
    Neither the shutdown early-return nor the superseded early-return takes it:
    a `completed` stamped on the way out of a shutdown would be a fabricated
    verdict where the boot sweep reports `interrupted` honestly, and a superseded
    task has nothing of its own left to stop.
  - It finalises through the id-fenced `stop_if`, off the async runtime, and
    prunes afterwards — so an auto-stop is indistinguishable from an operator
    pressing Stop, including when one stops and starts another session in the gap.
  - Requesting it with an empty `diagnostics[]` is **rejected**
    (`400 validation_error`) rather than ignored: no orchestration task is spawned
    for an empty array, so the session would have recorded to the cap while
    reporting that it would not.
- **New capability `control.validation_auto_stop`.** Gate the option on it. An
  older daemon parses and *drops* the unknown request field rather than rejecting
  it, so it answers `200` and then records for two hours — neither the status code
  nor the body distinguishes the two.

### Documentation
- **The five `POST`/`DELETE` validation routes are documented.** They appeared in
  neither `docs/USER_GUIDE.md` nor `docs/DEVELOPER_HANDOVER.md`, so a user reading
  the guide had **no documented way to stop a session** — which was the
  user-facing consequence of the defect above. Both now carry them, and the user
  guide gains a *How a validation session ends* section stating the two-hour
  ceiling in plain terms.
- **`daemon.md` gains the control-path routes**, of which it contained zero
  occurrences: `POST /hwmon/{id}/discover-control-path`, `GET`/`DELETE
  /diagnostics/control-path` and `GET /diagnostics/preflight` (DEC-333) were
  missing from its route tables entirely.

## [2.42.0] — 2026-09-06

**Two audit findings from the 2026-09-06 cross-stack audit (DEC-336).** Pairs
with `control-ofc-gui` >= v2.64.1. The GUI half is independent — neither change
needs the other, and there is no new capability flag.

### Fixed
- **[SAFETY] Control-path discovery now performs the refusal it publishes.**
  `GET /diagnostics/preflight` has reported `blocked` on a stale temperature
  source since 2.39.0, and nothing on the write path acted on it: the two thermal
  gates compare a temperature against a limit and have no view of how old that
  reading is, so a sensor-polling loop wedged on an unresponsive chip presented
  its last-known-good temperatures indefinitely and both gates kept passing. The
  POST returned `202` and perturbed a fan header on a machine whose real
  temperature was unknown, leaving the refusal to whichever client happened to
  honour the verdict. `POST /hwmon/{id}/discover-control-path` now returns
  `409 validation_error` (retryable) in that state, and a run already in flight
  aborts and restores on the same per-cycle cadence as the other two thermal
  gates. Verdict and refusal are derived from one predicate, so they cannot
  disagree. **Deliberately discovery-only:** fan verify and PWM characterisation
  have shipped without a staleness gate since 2.32.0 and still only *warn*.
- **The v2.41.0 release note named the wrong configuration file for
  `record_startup`, and following it removed pump protection.** It said
  `runtime.toml`; the key belongs to `/etc/control-ofc/daemon.toml`. The
  `[startup]` section of `runtime.toml` accepts `delay_secs` alone and rejects
  unknown keys, so writing it there fails the parse and the **whole file** falls
  back to defaults — taking every saved header-role assignment with it, and with
  them the 30% pump floor, the stop exemption and pump-safe identify, on exactly
  the boards with no usable labels. The note is corrected, `daemon.toml.example`
  now documents the option where it is read, and a test pins both halves.

## [2.41.0] — 2026-09-06

**AIO Phase 8 Batch 3a (DEC-335): lifecycle fingerprint, thermal observation and
steady-state detection.** Pairs with `control-ofc-gui` >= v2.64.0. Additive and
capability-gated on the new `control.thermal_observation` flag.

### Added
- **A thermal observation session kind.** Records how a cooler responds to a
  workload you run yourself — temperature, CPU package power, pump and radiator
  duty and RPM. Control-OFC never starts, stops or controls the workload.
- **CPU package power**, where the machine exposes it. Read from a CPU chip's
  hwmon power attribute if there is one, otherwise derived from the powercap RAPL
  energy counter. Sampled only while a session records, never on the 1 Hz poll.
  A machine that exposes neither reports the value as unknown — never as zero,
  which would claim an idle CPU.
- **Steady-state detection.** A conservative rolling-window test on temperature
  trend and variance decides whether an observation reached equilibrium, and the
  exact criterion is reported alongside the verdict. Too little data reports
  "insufficient data" rather than forcing a conclusion, and a run that never
  settles is reported as an observation about the run — never as a fault in the
  cooler.
- **A startup fingerprint per member.** Override duration, peak and settled RPM,
  and the duty commanded and read back while it happened. A cooler that spins its
  fans up at power-on and then obeys the duty is reported as a device behaviour;
  it is explicitly never reported as failed PWM control.
- **An opt-in startup recording**, `[startup] record_startup` in
  `/etc/control-ofc/daemon.toml`, **off by default**. Records a bounded window at daemon start so the startup
  behaviour above can be captured without somebody being at the machine. It never
  blocks you: starting a session yourself takes over immediately and the partial
  recording is still saved, and these recordings are retained separately so they
  cannot displace sessions you made by hand.

### Changed
- A session document now carries the startup fingerprints and the steady-state
  result. The per-session sample limit was re-derived from a measured sample, so
  the extra fields cannot push a recording past the size it can be read back at.

## [2.40.0] — 2026-09-06

**AIO Phase 8 Batch 2 (DEC-334): PWM behaviour characterisation.** Pairs with
`control-ofc-gui` >= v2.63.0. Additive — no existing route is removed, no
existing field changes meaning, and `api_version` is unchanged. Gated on a new
`control.pwm_behaviour_characterization` capability, separate from
`pwm_characterization`, which keeps its old meaning.

### Added
- `POST /hwmon/{id}/characterize` accepts two optional inputs: `bidirectional`
  and `stability_seconds`. Both are advisory — the daemon clamps the dwell and
  chooses which duties carry one, so the run's cost is bounded server-side.
- **Bidirectional walk.** The clamped duty list is walked **down from the top and
  back up**, so the run ends at its highest duty. Each step reports the leg that
  produced it (`ramp` | `falling` | `rising`); `ramp` is the first step of either
  mode, whose approach direction is unknown because it is entered from the
  captured pre-sweep duty. The walked step count stays inside the existing
  `CHARACTERIZATION_MAX_POINTS` cap, so the worst-case engine write-pause does
  not move.
- **`api/stats.rs`**, a pure statistics module: mean, median, standard
  deviation, coefficient of variation, tach dropouts, robust outlier counting,
  rolling-median settling detection, plateau detection, effective control range
  and hysteresis. Written to be reusable by Batch 3's steady-state detector.
- **Per-step RPM statistics** over the samples the settle window already took,
  plus an optional longer dwell at up to three daemon-chosen duties.
- **Learned response bands**, persisted at `{state_dir}/pwm_baselines.json` and
  pruned at boot by the same stable-header-id rule as the control-path store. A
  band *widens* as runs accumulate; nothing in the control path reads it.
- **`{value, provenance}` on `estimated_physical_rpm`**, plus a per-run
  `provenance` legend. `DevicePolicy` gained `rpm_correction_factor` and
  `correction_source`; **no shipped entry sets them**, and because that type
  derives no `Deserialize`, untrusted input cannot define a correction.
- **`pwm_behaviour_characterization`** as a validation-session diagnostic.
  Requesting it alongside `pwm_characterization` runs only the behaviour sweep —
  it is a strict superset, and both feed the same findings.
- Four observational findings: `pwm_hysteresis`, `rpm_stability`,
  `effective_control_range`, `learned_response_range`. None can report `fail`.

### Fixed
- **`response_latency` no longer implies millisecond precision it does not have.**
  It reported "first RPM change 500–3000 ms" from readings taken every 500 ms.
  The figures are now quantised and the measurement resolution published beside
  them.

### Safety
- **The stability dwell renews the engine pause and the hwmon lease from inside
  its own loop**, on its own cadence, with a compile-time bound derived from that
  cadence rather than copied from the settle window's. The settle bound holds at
  exactly `15 x 2 == 30` — no headroom — so a hold longer than a settle would
  have overrun the pause deadman at any dwell length, and at the maximum dwell
  would also have outlived the 60 s lease.
- **The walk descends first and ends high.** An interrupted run leaves the header
  at or near its maximum duty, which is what keeps the four exits that do not
  restore it benign.
- No walked duty is ever below `max(20, header floor)`, in either direction, and
  0% remains unreachable through this endpoint for any header and any input.

## [2.39.0] — 2026-09-06

**AIO Phase 8 Batch 1 (DEC-333): a shared diagnostic preflight, PWM-to-tach
control-path discovery, and the evidence/provenance foundation.** Pairs with
`control-ofc-gui` >= v2.62.0. Additive: no existing route or field changes
meaning, and `api_version` is unchanged.

### Added
- **`POST /hwmon/{id}/discover-control-path`** plus **`GET`/`DELETE
  /diagnostics/control-path`** — establishes which tach channel(s) a PWM output
  actually drives, by measurement. Returns `202` and runs detached, like
  characterisation, and claims the **same** single-flight verify slot, so at most
  one of the four diagnostics ever drives hardware.

  Deliberately **not** `pwmconfig`'s stop-the-fan model: the perturbation moves
  away from the nearer rail so there is always headroom, every commanded duty is
  clamped into `[max(20, header floor) .. 100]` — **0 % is unreachable for any
  header** — and two cycles are run so repeatability is a real confidence input.
  Reports `confirmed` / `probable` / `ambiguous` / `no_tach_response` /
  `multiple_responses`, and represents a one-PWM-to-many-tach mapping rather than
  assuming one-to-one.
- **`GET /diagnostics/preflight`** — a read-only typed safety report for one
  header and one diagnostic. Takes no lease and no slot, so calling it reserves
  nothing. Consumes the *existing* predicates (`verify_thermal_guard`,
  `header_is_pump_protected`, the verify slot), so there is one definition of
  each rule rather than a second copy.
- **`control_path_discovery`** as an orchestratable validation-session
  diagnostic, attached to `evidence[]` verbatim, with a `control_path_mapping`
  finding. A run that found nothing is `not_observed`, **never** `fail`.
- **Two capability flags**: `control.control_path_discovery` and
  `control.diagnostic_preflight`.
- **`{state_dir}/control_paths.json`** — discovered relationships, keyed by the
  header's stable id and pruned at boot to whatever discovery can still see, so a
  board or driver change invalidates a stale record by construction.
- **Monitor-only tach channels are observable.** `FanInputDescriptor` gains
  `input_path` (not on the wire) so discovery can read a `fanN_input` that has no
  `pwmN`. The 1 Hz poll is unchanged — these are read only for the duration of a
  run, so poll cost stays flat.

### Safety
- **A temperature-staleness predicate**, new. `check_thermal_safety` iterates
  whatever the state cache holds and has never had a view of how old those
  readings are, so a poll loop wedged on an unresponsive chip presented its
  last-known-good temperatures indefinitely and every thermal gate passed on
  them. Now surfaced on the preflight for every diagnostic, and a **blocking**
  condition for control-path discovery. It does **not** change what
  `POST /hwmon/{id}/verify` or `.../characterize` do — those report it as a
  warning, because a preflight that promised a refusal the daemon does not
  perform would be lying about the daemon's own behaviour.
- **A pump-tach-disappearance abort.** A pump-protected header whose tachometer
  was readable at the start of a run and stops reporting mid-run aborts
  immediately and restores. Conditional on the tach having been there to begin
  with, so a pump with no tach at all does not abort every run on that board.
- **Liveness is proved before every observation window, not once per cycle.** A
  cycle holds two windows, so a per-cycle renewal makes the interval `2 × window`
  — equal to `VERIFY_PAUSE_DEADMAN` at the maximum settle before any I/O
  overhead. The pause would then expire mid-run, the engine's write phase would
  resume while discovery was still perturbing, and a second diagnostic could
  force-take the lease so that even the restore failed. Caught in review; never
  shipped. Pinned by a test asserting `renewals == cycles × 2`, whose second limb
  asserts the per-cycle interval does *not* fit, so it can tell the two apart.
- Shutdown is re-checked immediately before each of the three PWM writes.
  `observe` checks at the top of each sample iteration but returns after one more
  read, so shutdown could land in that gap and a write could follow
  `restore_hardware()` — the DEC-290 / 277-c hazard.
- Restoration reuses `characterization::RestoreOnDrop` unchanged — the same two
  deliberate skips (shutdown, thermal force), the same `AUD3-l` restore floor and
  the same load-bearing drop order — rather than growing a second copy.

## [2.38.0] — 2026-09-06

**Packaging + one probe behaviour change.** No API shape change; `api_version`
unchanged. `GIGABYTE_DUAL_CHIP_BOARDS` gains a board, so `expected_chips` on
`GET /diagnostics/hardware` is non-empty for one more DMI match.

### Added
- **A Super-I/O probe guard** (`X87-a`, DEC-332). On Gigabyte boards with an ITE
  Super-I/O, loading `nct6775` or `w83627ehf` can hide the board's *second*
  Super-I/O chip — and every fan header on it — until the machine is physically
  disconnected from mains power. Both drivers write a config-mode unlock to
  0x2E/0x4E **before** reading the device ID, and that write latches an ITE
  eSPI→LPC bridge into configuration mode, where it answers `0x8883` in place of
  the chip behind it. Measured on an X870E AORUS MASTER: 3 of 8 fan headers and
  3 of 9 temperatures lost. The module *fails to load* ("No such device") and
  does the damage anyway.

  The package now ships `/usr/lib/modprobe.d/control-ofc-superio.conf` and a
  guard that declines those two modules on boards known to carry an ITE
  Super-I/O — where they could never have bound in the first place — and runs
  the real `modprobe` everywhere else. `nct6775` is genuinely required on most
  Nuvoton boards and is untouched there. Fail-open: unreadable DMI or any
  unexpected condition loads the module. To override, create a **same-named**
  file at `/etc/modprobe.d/control-ofc-superio.conf` — same-name masking is the
  only override `modprobe.d(5)` guarantees. A differently-named file does *not*
  reliably win: config files are parsed in lexicographic order by basename and
  the first `install` for a module takes effect (measured, kmod 34.2).

### Changed
- **The opt-in Super-I/O port probe reads before it writes** (`X87-g`, DEC-332).
  `probe_base` used to unlock config mode and *then* read the DEVID — the exact
  write proven above to latch a bridge. It now mirrors `it87_find`: a no-enter
  DEVID read first, an unlock only when that returns `0xffff`. A chip already in
  config mode (`FEAT_NOCONF`: it8790/it8792/it87952) is now identified without a
  single unlock byte reaching its port.
- **`0x8883` is no longer reported as a chip.** `is_ite_devid` accepted it, so
  the probe invented a device named `it8883` — on the one code path capable of
  creating the state it was reporting. It is now reported as the bridge it is.
- **X870 AORUS STEALTH ICE is enrolled as a dual-chip board** (`X87-f`,
  DEC-332). It had been held out on the premise that nothing local could reach
  its secondary, which the measurement disproves; it87 #81's own thread records
  its reporter driving `pwmN` on the second chip. Owners now get the dual-chip
  warning, and the board is covered by the guard.

### Fixed
- **Guidance that told users to give up is corrected** (`X87-b`, DEC-332). The
  dual-chip hint, `chip_db.rs`'s failure-mode notes and `README.md` all stated
  that a secondary reading `0x8883` was unreachable with "no local fix". It is
  recoverable — suppress the two modules, then power down **at the wall**, since
  the latch survives both a reboot and a soft power-off. All three now say so
  and point at the step-by-step recovery.
- **A false claim shipped in `/etc/modules-load.d/control-ofc.conf`.** Its
  comment read "Loading a module for a chip that is not present is harmless
  (modprobe exits silently)". Both halves are false on these boards, and the
  file is user-visible on every install.

## [2.37.0] — 2026-09-05

**Additive only:** one new optional response field on `GET /diagnostics/hardware`,
`api_version` unchanged. Pairs with `control-ofc-gui` >= v2.61.0, which renders it;
older clients ignore the field and older daemons omit it.

### Added
- **Board voltage rails are discovered and published** (`WIRE-ag`). Super-I/O chips
  expose their ADC block as `inN_input` in sysfs and the daemon read none of it —
  ten rails sit there on a typical board and nothing looked. `GET
  /diagnostics/hardware` now carries `voltages[]`, each entry `{id, chip_name,
  channel, label, value_v, identified}`.

  **`identified` is the load-bearing field.** Only channels the driver labels are
  identified rails; on the reference board that is 3 of 10. The rest are raw ADC
  pins, and because boards feed rails through resistor dividers the driver knows
  nothing about, an unnamed channel's reading is a real voltage at the pin and is
  *not* the rail voltage. Clients must render the two cases differently.

  Read-only sysfs on the hwmon tree this endpoint already walks: no port I/O, no
  new capability, and unaffected by the Super-I/O port-probe gate. Nothing in the
  daemon reads a rail — no control path, no floor, no threshold. A failed scan
  degrades to an empty list rather than failing the report, and the field is
  omitted entirely when empty.

  Deliberately not published: `inN_alarm` (measured untrustworthy — two channels
  on the reference board assert the bit while reading inside their own min/max
  window), `inN_min`/`inN_max` (driver defaults, not board limits), and GPU core
  voltages — `amdgpu` `vddgfx`/`vddnb` and Intel `i915`/`xe` `in0_input` — which
  are not board rails.

  Voltages are **not** on `/status` or `/poll`, and are **not** sensors: a rail
  moves by millivolts, and `sensors[]` is temperature-shaped (`value_c`,
  `temp_type`, `thresholds`) and feeds curve binding and the thermal-safety path.
  A rail can never be offered as a fan-curve source. See DEC-331.

## [2.36.0] — 2026-09-05

Wave 3 of the 2026-09-05 wire-surface sweep — the daemon-side half. **Additive only:**
five new capability flags, two new optional response fields, one corrected record, and
`api_version` unchanged. Pairs with `control-ofc-gui` >= v2.23.0, which this release makes
the daemon itself state rather than something the release notes assert.

### Added
- **Five capability flags for features that shipped before they had one** (`WIRE-k`).
  `GET /capabilities` gains `control.gpu_fan_verify`, `control.hardware_readiness`,
  `control.superio_port_probe`, `control.preferred_sensors` and
  `control.daemon_config_report`, all `true`. Each route existed for between one and five
  minor versions with no key, so a client had to compare the daemon's **version string** or
  call the route and read a `404` as "unsupported" — and that `404` is indistinguishable
  from a handler's own `404` for an unknown id, which is precisely why
  `pwm_characterization` and `validation_sessions` were given flags when they landed. These
  five simply never were.

  **An absent flag is not a denial.** The keys exist only from this release while the
  features behind them shipped from 1.11.0 onward, so a client that collapsed "absent" into
  `false` would stand five working features down on every daemon in between. `docs/08`
  states the tri-state rule.

- **`/status` and `/poll` say whether the engine is writing** (`WIRE-n`). New
  `verify_active` boolean, `true` while a hardware verify, PWM characterisation, OpenFan
  calibration or validation sweep holds the engine's write pause. The engine keeps
  *evaluating* during such a session and keeps publishing every control's duty in
  `control_outputs[]`; it simply does not apply it, and until now nothing on the wire said
  so — a client rendered a duty nothing was writing. Narrow but reachable: the dialogs are
  modal, yet a validation session keeps recording after its dialog closes.

  **Not a safety signal.** The thermal emergency's `force_all_with_floor` runs well before
  the `verify_active()` gate (DEC-297), so a paused write phase never gated the ladder;
  a client that suppressed a thermal banner on this field would hide a live emergency. It
  qualifies the commanded duty and nothing else.

- **`/diagnostics/hardware` reports the board's firmware-declared header counts** (`X87-d`).
  New optional `board_firmware_counts` — `{platform, special, fan_count, temp_count,
  volt_count}` — decoded from the Gigabyte SIV descriptor `it87` exports at
  `/sys/class/gigabyte/id/gigabyte_siv`. `expected_chips` beside it is a curated DMI lookup
  and therefore an inference, only ever as good as the table; this is a measurement from the
  board, so a client can state the deficit as a fact. The DMI table stays as the fallback and
  stays authoritative for *which* chip should carry them. Compare against
  `hwmon.total_headers`, which is `pwmN`-capable headers only — monitor-only tachometers are
  a disjoint set on `GET /inventory/hwmon`, so the difference is "headers with no
  controllable PWM", not "unreachable headers".

  Read-only sysfs — no port I/O, no `CAP_SYS_RAWIO`, unaffected by the Super-I/O port-probe
  gate. Absent on every non-Gigabyte board, when `it87` is not loaded and when the
  descriptor does not decode; never a zeroed object, because a `fan_count` of 0 would claim
  the board has no fan headers.

### Changed
- **`control.min_supported_gui` is now `2.23.0`, and is the single source of the pairing
  floor** (`WIRE-ac`). It said `2.0.0` — the DEC-165 cutover — while the release notes said
  something else, and there turned out to be **four** numbers claiming to be one contract,
  not the three the register recorded: `2.0.0` in the handler *and* in this file's eight
  most recent entries, `>= v2.23.0` in roughly thirty entries before those, and `>= v2.38.0`
  in the 2.16.0 entry (a forward reference to the GUI that consumes `GET /config`, not a
  floor at all). The handler now publishes the number from one constant and the prose quotes
  it.

  Nothing acts on the field today — the reference GUI's compatibility banner quotes its own
  constant — so the practical effect is a truthful contract rather than a behaviour change.
  The banner it feeds is non-blocking by design (`docs/08`): the daemon is the sole PWM
  writer and controls fans correctly whatever the GUI's age.

### Fixed
- **A failed config reload no longer overwrites a startup degradation** (`WIRE-ao`).
  `runtime_config_degraded` recorded latest-wins, so a `SIGHUP` reload that could not parse
  `runtime.toml` replaced an earlier *startup* record. The two are not equally severe: a
  startup failure drops every `header_roles` assignment — and on a board with no `pwmN_label`
  files that assignment is the only evidence a header drives a pump, so its 30% floor, its
  stop exemption and its pump-safe identify go with it — while a reload failure drops
  nothing, because startup's roles are still in force. The record could therefore read
  `reload` while a hand-assigned pump was already unprotected.

  Now most-severe-wins: a `startup` record stands. Latest-wins is kept *within* the reload
  phase, so a second failed reload still refreshes `detail` rather than serving a stale one.
  Pre-existing since 2.34.0 and already worked around client-side (GUI v2.58.0 never
  reassures on `reload`), which is why it was a P2; the client-side rule stays in `docs/08`
  regardless, since a client cannot tell which daemon it is talking to from this field.

## [2.35.5] — 2026-09-05

**Test-only.** No runtime, API or cooling behaviour changes: every line added lands inside
`#[cfg(test)]` in `daemon/src/api/responses.rs`. This is the daemon half of the GUI's Wave 2
wire-surface sweep (`DEC-329`), released so the pin travels with the package rather than
sitting on `main`. Pairs with `control-ofc-gui` >= v2.0.0 (unchanged floor).

### Added
- **The serialised wire-field surface is pinned on the daemon side** (`WIRE-aj`). For ten
  response structs the daemon now serialises a representative value and asserts the exact
  key set it puts on the wire, against the same declared surface the GUI asserts its models
  against (`tests/fixtures/wire_fields.json` in the GUI repo). Previously each side tested
  only itself, so a field the daemon added or renamed could reach a release with nothing
  comparing the two — which is the systemic reason the v2.58.0 GUI fix went a whole release
  unnoticed. A key added to a response without a corresponding GUI model field now fails
  here, at `cargo test`, instead of silently becoming a field the GUI never reads.
- **The `Limits` block is pinned with the rest of the surface** (`WIRE-d`). Its keys were
  outside the first pass; the GUI now reads the OpenFan spin-down ceiling from this block
  rather than hardcoding 8 seconds, so its shape is load-bearing and is asserted with the
  others.

## [2.35.4] — 2026-09-05

A reporting correction plus the previously-unreleased documentation batch. **No cooling
behaviour changes** — `resolve_policy_floor` is the reporting path and no enforcement site
calls it. Pairs with `control-ofc-gui` >= v2.0.0 (unchanged floor).

### Fixed
- **`effective_min_pwm_pct` no longer advertises a floor nothing enforces** (`WIRE-b`).
  `PwmHeaderEntry::from_descriptor` resolves one policy for *every member* of a cooling
  device, so a radiator or auxiliary fan inherited its device's `generic_pump` policy and
  published `30` — beside `stop_permitted: true`, since identify branches on
  `header_is_pump_protected`, a union in which membership is not a term. Nothing held that
  floor: every site that could apply a *device-policy* floor keys on the same union. (The
  profile-role floor reached through `member_effective_floor` is a different floor, DEC-095,
  and is not what this field reports.) `resolve_policy_floor` now
  returns `0` for any header that is not pump-protected, which is exactly what an ordinary
  chassis header already reported.

  This is the sibling of `AIO7-d` (fixed in 2.35.0) one field over, and the same reasoning
  was **already written into `from_descriptor`** for the no-device case: its comment says
  defaulting to the pump policy "would advertise a 30% floor on ordinary chassis fans that
  the engine does not enforce". The `Some(device)` branch reproduced precisely that.

  **The honesty invariant that exists to catch this was structurally blind to it:**
  `reported_floor_matches_enforced_floor_for_every_shipped_policy` only ever called
  `resolve_policy_floor(p, true)`, and the over-claim lived in the `false` branch. It now
  asserts over both, and a new call-site test pins what `/hwmon/headers` actually publishes
  for a cooling-device member rather than only what the helper returns.

  Deliberately **not** fixed from the other end: making membership a term in
  `header_is_pump_protected` would hand a 30% floor and stop-refusal to every radiator and
  auxiliary fan in a device, which is a real cooling change rather than a reporting one.

### Fixed (documentation)
- **The thermal trip point was described 5 °C low in three places here** (`UDOC-j`):
  `README.md`, `docs/USER_GUIDE.md` and `man/control-ofc-daemon.1.scd` all said the
  per-machine emergency limit is "raised to **match** the CPU's own reported design
  ceiling". `profile_engine::effective_trigger_c` computes
  `min(ceiling + 5 °C, 115 °C)`. The margin is not incidental: a limit set *at* the
  ceiling fires on a healthy part and then latches, because release needs a reading at
  or below 80 °C that a part holding its ceiling never produces — so the docs described
  the exact failure DEC-308 exists to prevent. **No code was wrong**; `daemon.md` and
  `constants.rs` already stated it correctly. `README.md` and `man/` were survivors of
  the DEC-308 sweep too, and are now named in the curator's update-target map.
- **`docs/USER_GUIDE.md`'s operator-facing API table listed two superseded readiness
  routes and omitted the live one** (`UDOC-n`). It named `GET /inventory/readiness`
  (DEC-200) and `GET /inventory/superio` (DEC-202); DEC-207 merged both into
  `GET /inventory/hardware-readiness`, which is the only readiness route any shipped GUI
  calls and did not appear in the document at all. All three are now listed with their
  status.

### Changed
- **The README "Latest release" line is one sentence again** (`UDOC-a`) — it had grown to
  19,566 characters, the release notes of every recent version in a single paragraph at
  the top of the landing page and in `/usr/share/doc/`. Per-version detail lives in this
  file.

## [2.35.3] — 2026-09-04

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). Batch G of the
`/ofc:audit` register triage (DEC-326 and DEC-327). **Fixes a false "BIOS
reclaimed the header" verdict that fired whenever a fan header sat at 100% on an
ITE Super-I/O**, and **stops the unbound-chip advice recommending a driver build
the user is already running**. No wire-format change and no route
change — some published fields stop reporting cases that were never true, and
no field, code or capability moves. Touches no floor, no threshold, no ladder
rung and no capability flag.

### Fixed
- **A fan at 100% is no longer misreported as a BIOS reclaim** (`HOST-a`). The
  `it87` driver reports the PWM mode it *computes* rather than the one it was set
  to: at full duty it answers `pwm_enable = 0` ("full speed"), which every
  `enable != 1` check read as firmware taking the header. A wholly successful
  100% write therefore looked identical to a reclaim. Four things were wrong as a
  result, and all four are fixed:
  - the write path logged `pwm_enable ... reclaimed by BIOS` every tick at 100%
    and inflated the revert count on `/diagnostics/hardware`;
  - **`POST /fans/characterize` aborted its own sweep at the 100% point** and
    reported `interference_detected: true` — a false positive in the one
    diagnostic built to answer "does this header accept writes?";
  - **`POST /hwmon/{id}/verify` returned `pwm_enable_reverted`** for a pump
    header, reachable for any pump idling at 60–65% because the verify duty is
    exactly 100 there;
  - **a validation session recorded `control_reclaimed`** and published the
    `bios_ec_control_reclaim` finding whenever a member curve reached 100% under
    load — which is what a validation session exists to observe.

  The discriminator carries no chip table: `pwm_enable == 0` is only ignored when
  the duty register still holds the 100% the daemon itself commanded. A reclaim
  to *automatic* reports mode 2 and is unaffected, and detection resumes on the
  first observation below 100%. **Cooling behaviour is unchanged in every case** —
  the only suppressed state is one in which the fan is already at maximum.

  Wider than it first appears: the kernel condition is
  `(!has_fanctl_onoff || nr >= 3) && duty == 0xff`, so `pwm4` and above are
  affected on *every* ITE chip, not only on chips lacking the feature bit.

- **A validation session can no longer report "control restored" for a restore
  that never happened.** Found reviewing the fix above: suppressing the false
  reclaim was not enough, because the recorder still advanced its watermark, so
  a member coming back down from 100% pushed a phantom `control_restored`
  event. Since the `control_restoration` finding counts reclaims and restores
  across the whole session rather than per member, one member's ordinary ride
  to 100% could have paid for a *different* member's genuine, never-restored
  reclaim — turning a FAIL into a PASS on the report.
- **A header the daemon no longer owns is reported as reclaimed again.** The
  recorder was reading a cached "last commanded" value that is carried forward
  indefinitely and never cleared when the daemon stops driving a header, so a
  stale command could have masked a real reclaim. It now reads the controller's
  own value, which is cleared when the lease is released.
- **`POST /hwmon/{id}/verify` no longer passes a header the firmware already
  pins at full speed.** Such a header is indistinguishable from the case above
  unless the daemon also held manual mode to begin with, which is now required.

- **The unbound-chip hint no longer tells you to install a driver build you
  are already running** (`HOST-d`). If the loaded `it87` is an out-of-tree
  build, the advice to install `it87-dkms-git` is a no-op — and it was the
  advice being given, so a user in that state had nothing left to try and no
  way to know it. The hint now says so and names the terminal state instead.
  Detected from the module's taint flag (`/sys/module/<name>/taint`), because
  the daemon's own sandbox makes `/lib/modules` unreadable —
  `ProtectKernelModules=true` mounts an inaccessible directory over it.
- **The "Super-I/O driver not loaded" readiness card no longer says the driver
  is not loaded when it is.** The same card covers two different states, and it
  described both as the first; on a machine whose driver loaded but failed to
  bind, it contradicted the recommendation shown beside it. Its `code` is
  unchanged, so nothing in the GUI needs to move.

### Changed
- **The unbound-chip hint no longer recommends `mmio=on`** (`HOST-b`/`HOST-c`).
  It is already the driver default, so the advice named a state that was always
  in effect. The hint now also names the `0x8883` case as a terminal state with
  no local fix, rather than sending the user round a loop.
- **`README.md`'s dual-ITE row reports the outcome per board rather than per
  family.** X870E AORUS ELITE is owner-confirmed working (it87 #89); on X870E
  AORUS MASTER the secondary is unreachable with no local fix (measured
  2026-09-04). The previous text promised the working outcome to both.
- **`chip_db.rs`'s account of device-ID `0x8883` is corrected and its previous
  retraction withdrawn** (`HOST-b`). The comment claimed `0x8883` was a secondary
  stuck in config mode "recovered with `mmio=on`", citing it87 #81. Measured here:
  `mmio` already defaults to true, `0x8883` appears nowhere in the driver while
  the IT87952E *is* supported (so the chip is unreachable, not unsupported), and
  #81 is open — its reporter applied that advice and still lost three fans and a
  pump. The separate `0xFFFF`/`sensors-detect` failure is kept as the real,
  recoverable mode it always was.

## [2.35.2] — 2026-09-04

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Tests only, plus one
`#[doc(hidden)]` test seam — no behaviour change.** Batch D of the
`/ofc:audit` register triage (DEC-324); the daemon's share is `AUD2-i`.

### Added
- **The characterisation `run_id` fence has a regression test that can actually fail**
  (`AUD2-i`). The fence stops a superseded sweep publishing its points, its state and its
  `detail` over the run that replaced it. The test named for it awaited a terminal state
  before starting the second run, so the two never coexisted and both fences could be deleted
  with it green; its docstring now says so and points at the new test. The new one supersedes
  a live run through the DEC-296 expired-deadman steal — the only door through which two runs
  can coexist — and asserts an invariant across *both* sweeps, because the damage repairs
  itself: run B's own terminal write restores `state` and `points`, so an end-state snapshot
  passes with both fences removed. Verified by removing each fence independently and requiring
  the test to go red.
- `StateCache::expire_verify_claim_for_test()` — `#[doc(hidden)]`, called only by that test,
  following the existing `persist_for_test` / `rollback_for_test` precedent. It stamps a live
  claim's deadman as elapsed so the steal branch is reachable without waiting out the real 30 s
  window. No production read path changes and `VERIFY_PAUSE_DEADMAN` remains a literal constant
  at all three of its call sites. Recorded as register row `324-a`.
- `the_verify_deadman_test_seam_has_no_production_caller` — a source scan asserting nothing
  under `daemon/src/` calls that seam, matched in *call position* rather than as a substring
  so it does not match the definition or its own doc comment. It replaces a comment that
  merely asked for the same thing, and is itself verified by planting a caller.

## [2.35.1] — 2026-09-04

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Three P2 fixes from the
`/ofc:audit` register (`AUD3-j`, `AUD3-k`, `AUD3-n`), which are one story: a validation
session did not own the things it started.** No new routes, no new capability flag, no
floor, threshold or safety-rule change, and both parity oracles are byte-identical. One
observable behaviour changes for a client that watches a session-orchestrated
characterisation — see below.

### Fixed
- **Ending a validation session did not end the diagnostic it started** (`AUD3-j`). A
  characterisation sweep runs in a detached task and renews the profile engine's write-pause
  once per point, so it keeps curve control suspended for as long as it runs. Stop and cancel
  finalised the session record only: the orchestrator noticed the session had ended and simply
  returned, discarding the evidence rather than stopping the sweep. For up to
  `CHARACTERIZATION_MAX_POINTS × CHARACTERIZATION_SETTLE_MAX_S` — **20 × 15 s** — after the
  user ended the session, the header was still being swept and **every backend's curve control
  was still suspended**. The orchestrator now asks the sweep to stop, **fenced on the `run_id`
  it was handed at 202**, so a run started by anyone else is never aborted — that fence is the
  whole safety property, and an unfenced abort is a defect this daemon has already been fixed
  for once. **Thermal safety never depended on this**, and it was checked rather than assumed:
  the forced-duty branch runs *above* the `verify_active` gate, so a paused engine still floors
  every output. This was lost control intent, not lost cooling.
  **Two consequences for a client:** `GET /diagnostics/characterization` now reports such a run
  as `cancelled` rather than `complete` — which is not a hardware failure and must not be
  worded as one — and the cancel is cooperative, as `DELETE /diagnostics/characterization`
  has always been, so the current point finishes its settle (≤ 15 s) and the header is restored
  before the run goes terminal. A progress view should expect the run to stay `running` briefly
  after the session has already returned its summary. There is no capability flag separating
  this from the older behaviour; branch on the daemon version if you must distinguish them.
- **Starting a session could park a worker behind a wedged sysfs write** (`AUD3-k`).
  `ValidationEngine::start` held the session slot across an unbounded blocking acquisition of
  the hwmon controller lock — the exact ordering the recorder's own `[LOCK ORDER]` contract
  states must never happen, reached from the one entry point that ignored it. The engine holds
  that same mutex across a PWM write, and a wedged write is a recorded failure mode, so a
  `POST /validation/session` arriving during one blocked indefinitely **while holding the
  slot** — which then blocked the recorder, `GET /validation/session` and the orchestrator's
  own liveness check behind it. Not a deadlock; precisely the starvation the module was built
  to prevent. The baseline read now uses the same short timeout the sampling tick does, and is
  taken *before* the slot rather than under it. A baseline it cannot read costs one spurious
  event on the first sample, never a refused session — the same trade the tick has always made.
- **The session's expensive writes ran on the async runtime** (`AUD3-n`). Persisting a session
  serialises a document of up to ~5.7 MiB and performs `write` + `fsync` + `rename` + a
  directory `fsync`. That ran inline on the worker threads the 1 Hz profile engine — and
  therefore the thermal-safety decision — is scheduled on: every 30 sampling ticks, and on the
  start, stop and cancel request paths. `stop` did it on the request path **and then** ran a
  strictly cheaper listing off-runtime, so the cheap half was already careful and the expensive
  half was not. All four sites now go through the blocking pool. The shutdown flush stays
  inline on purpose: handing the last write to a pool whose runtime is being torn down would
  trade a certain flush for a possible one.

- **A broken finaliser was reported as an absent session.** Found in review of the above. When
  the off-runtime finalise task fails — a panic in the summariser, or the runtime shutting down
  — `POST /validation/session/stop` and `DELETE /validation/session` answered
  `404 not_found "no validation session has been started"` while the session was **still
  installed and still recording**. A client would conclude the session did not exist, stop
  offering to stop it, and then be refused `409`-equivalent `AlreadyRecording` on its next
  start. The two facts are now distinct: no session is still a `404`, a finaliser that broke is
  a `500 internal_error`.
- **The shutdown flush could resurrect a cleanly-stopped session.** Also found in review. The
  recorder's last write called the store directly instead of going through the engine, so it
  shared neither the write-ordering lock nor the stale-write guard every other writer uses. A
  `stop` in flight when the daemon is signalled to shut down could therefore have its
  `completed` document overwritten by the flush's older `recording` snapshot — and the next
  boot's sweep would then mark a cleanly-stopped session `interrupted` and discard its
  findings. Pre-existing rather than introduced here; fixed because this release re-reasoned
  about that exact line.

### Internal
- `daemon.md` and `docs/08_API_Integration_Contract.md` record the session-ends-the-diagnostic
  behaviour and its two client-visible consequences. Full rationale, the two shape choices, the
  review outcome and the alternatives rejected: **DEC-323**.

## [2.35.0] — 2026-09-04

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Three P2 fixes from the
`/ofc:audit` register (`AUD3-l`, `AIO7-d`, `AUD3-c`), which are one failure story: the
daemon could drive or describe a pump wrongly.** No new routes and no new capability flag.
One published field changes its value for some headers — see below — and a third
cross-repo parity oracle is added. Both existing oracles are byte-identical.

### Fixed
- **A diagnostic could restore a pump to a stop** (`AUD3-l`). `resolve_points` and
  `verify_test_duty` have always floored the duty a diagnostic writes on the way *in*, and
  `characterization.rs`'s own module doc claimed on that basis that "0% is unreachable
  through this module". It was not: both the verify restore and `RestoreOnDrop` wrote the
  **captured pre-sweep duty** straight into `set_pwm`, which applies no floor of its own.
  A pump-protected header whose duty read 0 was therefore swept correctly and then restored
  to 0 — with `pwm_enable=1` asserted by the write, which is what turns a firmware-controlled
  0 into a stopped pump nothing will revise: until the engine's next tick if the header is a
  controlled member, and indefinitely if no profile is active. Both restores now clamp to
  `max(HARD_PUMP_CPU_FLOOR_PCT, captured)` for pump-protected headers **only** — an ordinary
  fan is still put back exactly where it was found, 0 included, because raising it would be
  a behaviour change rather than a safety fix. Newly reachable rather than merely old:
  Phase 5's orchestrator aims both diagnostics at `device.pump_member` by default.
  **Honest limit:** whether a pump header can read 0 under BIOS automatic control was not
  reproduced on the development machine (its auto-mode headers read 63/255), so the trigger
  remains unverified against hardware; the unguarded code path was not in doubt.
- **`stop_permitted` was published from the cooling device's policy, not from the predicate
  the daemon obeys** (`AIO7-d`). `PwmHeaderEntry::from_descriptor` resolves one policy for
  **every member** of a device, so a radiator fan in an AIO inherited `GENERIC_PUMP`'s
  `supports_stop: false` and was advertised unstoppable — while `POST /fans/{id}/identify`
  branches on `header_is_pump_protected`, in which cooling-device membership is not a term,
  and stopped it. Live-reproduced on an X870E AORUS MASTER. The published value is now
  exactly `!header_is_pump_protected`, which is what
  `docs/08_API_Integration_Contract.md` has always said it was. **The dangerous direction
  was the second one:** a header named as a `pump_member` *without* a pump role was promised
  `stop_permitted: false` while identify drove it to 0 — a pump stopped while every client
  was told it would not be. Deliberately **not** fixed from the other end: making membership
  a term in `header_is_pump_protected` would hand a 30% floor and stop-refusal to every
  radiator and auxiliary fan in a device, which is a real cooling change. `supports_stop`
  is still published as part of the policy descriptor; it is simply no longer conflated with
  a per-header prediction.

### Added
- **A third cross-repo parity oracle, `header_role_classification.json`** (`AUD3-c`). The GUI
  hand-mirrors this daemon's `classify_header_role` label branches, because a daemon older
  than 2.31.0 publishes no `stop_permitted` and the reconstruction is then the only answer
  available. The two copies were **in agreement** — this closes the absence of a *gate*, not
  a drift. The direction of harm is the unsafe one: if this daemon learns a new
  pump-classifying label and the GUI's copy does not, the GUI concludes "not protected" and
  the wizard offers to stop a real pump. 29 cases of
  `(chip_name, pwm_index, label) -> (role, pump_protected)`, asserted on both sides and
  gated by `parity.yml`, which now compares three fixtures instead of two.

### Compatibility
`stop_permitted` changes value for one population: **radiator and auxiliary members of a
cooling device that are not themselves pump-protected**, which now correctly report `true`.
No client needs updating — the GUI already reads the field as `not stop_permitted` meaning
"pump protected", which is the semantics this restores. There is no capability flag
separating the old behaviour from the new, so a client that must distinguish them should
branch on the daemon version.

## [2.34.0] — 2026-09-04

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Three P2 fixes from the
`/ofc:audit` register (`AUD3-b`, `AIO1-d`, `AUD3-m`), which are one failure story told
three ways: a user-assigned pump role could vanish from `runtime.toml`, and nothing
reported it.** One additive `/status` + `/poll` field, no new routes, no new capability
flag, **no floor, threshold or safety-rule change**, and both parity oracles are
byte-identical. A client that ignores the new field sees byte-identical behaviour.

Why the three ship together: `RuntimeConfig::default()` carries **no `header_roles`**, and
on the boards this programme exists for (an it8696 publishing no `pwmN_label` files) a
user's `pump` assignment is the only evidence a header drives a pump. So any path that
loses or corrupts that file removes the header's 30% floor, its stop exemption and its
pump-safe identify — and until now every one of those paths was silent.

### Fixed
- **Two concurrent writes to the same file could publish a hybrid document** (`AUD3-b`).
  `atomic_io::write_atomic` derived its scratch file as a **fixed** `{path}.tmp` and
  opened it with `File::create`, which truncates — so two writers shared one scratch file
  and each could overwrite the other's partial content before renaming the result into
  place. `validation::recorder` documented that hazard and carried a private save lock;
  the other four call sites (`runtime_config`, `daemon_state`, `profile_store`,
  `validation::store`) did not. The scratch name is now unique per call
  (`.{name}.tmp.{pid}.{counter}`) and hidden, so **all five call sites are fixed at once
  and a sixth cannot reintroduce it** — the hazard no longer lives in a rule each caller
  must independently know. Cleanup now also runs on *every* failure rather than only a
  failed rename, because unique names would otherwise turn a bounded leak into a
  per-failure one. The GUI's twin helper (`paths.py::atomic_write`) has always done this
  correctly with `mkstemp`; the two had diverged on precisely the safety-relevant axis.
- **Two concurrent `/config/*` setters lost one edit, and both reported success**
  (`AIO1-d`). Every setter is load the whole file → change one key → write the whole file
  back → commit in memory, and nothing ordered two of them: the later `save_to` won the
  file, the later commit won the cache, and **both requests answered `updated: true`**.
  All twelve `/config/*` write routes (eleven acquisition sites — the two preferred-sensor
  routes share a helper) now serialise on one `tokio::sync::Mutex` taken by
  `runtime_for_update` itself, so a new setter cannot acquire the config without acquiring
  the lock. **The consequence was asymmetric, which is why this is P2 rather than
  cosmetic:** losing a poll-interval edit is annoying, but a `/config/cooling-device`
  write landing from a stale base **dropped the `/config/header-role` edit that preceded
  it** — and since v2.31.0 the GUI's Configure-AIO flow posts those two back to back in
  one user action, so the window was opened by ordinary use rather than by two operators
  racing. Measured: with the lock removed, three of six concurrent edits vanished and the
  pump role was dropped entirely, every request still returning `200 updated: true`.
  Two subtleties the review caught, neither of which the lock alone covers:
  `/config/profile-search-dirs` derives its value from the *current* list rather than from
  the request body, so its merge base is now read **inside** the guard — read outside, the
  lost update survived the lock; and `POST /config/header-role` releases the guard before
  building its response, because that response resolves the header's effective role through
  `hwmon_controller` — the one lock the engine holds across a blocking sysfs write — and
  holding both would have stalled every config route behind a wedged header rather than the
  one request that touched it.

### Added
- **`runtime_config_degraded` on `GET /status` and `GET /poll`** (`AUD3-m`) — additive,
  `api_version` unchanged, **omitted when the config loaded cleanly**. Reports that
  `runtime.toml` could not be read or parsed and the daemon fell back to defaults:
  `{reason: "unreadable" | "malformed", path, detail, phase: "startup" | "reload"}`.
  `RuntimeConfig::load_from` has always degraded silently — deliberately, so a corrupt
  file cannot stop the daemon booting — but the defaults it returns carry no
  `header_roles`, so a failed load removes every user-assigned pump role's 30% floor with
  **one `warn!` in the journal as the entire notification**. No endpoint reported it;
  this was checked by grep before the field was added. A *missing* `runtime.toml` is not
  a degradation and is not reported: that is first boot, and defaults are the correct
  answer there. The field is **sticky for the daemon's lifetime** — a later successful
  `POST /config/*` repairs the file, but nearly every runtime-mutable key is consumed once
  at startup, so clearing it would claim a recovery that did not happen.

## [2.33.1] — 2026-09-04

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Two P1 bug fixes** found by
`/ofc:audit` (register rows `AUD3-h`, `AUD3-i`). No new routes, no new capability flag,
**no floor, threshold or safety rule changes**, and both parity oracles are byte-identical.

### Fixed
- **`POST /config/cooling-device` rejected the OpenFan radiator fans the GUI itself
  offers** (`AUD3-h`). Member ids were validated against hwmon PWM headers only, so an
  OpenFan channel — which the GUI's radiator picker presents alongside writable hwmon
  headers, and which the wizard posts verbatim — failed with
  `400 unknown hwmon header id: openfan:chNN` on any machine that had *any* hwmon header.
  That is every motherboard-AIO machine, i.e. exactly the hardware the cooling-device
  feature exists for, so the Fan Wizard's AIO step could not be completed with an
  OpenFan-driven radiator fan.

  Membership is now checked **per source**, across hwmon headers and OpenFan channels.
  The rejection message is `unknown member id: {id}`. Deliberately not a flat union: the
  documented "an undiscovered source does not judge its members" escape is preserved
  per-source, because a union would have *tightened* the hwmon-absent case and rejected
  hwmon members that are accepted today. A GPU fan id is still rejected once hwmon is
  discovered — a GPU fan is never an AIO radiator fan.

- **A validation session at its own sample cap became permanently unreadable**
  (`AUD3-i`). The store wrote with no byte bound and read under the 4 MiB config cap,
  while a sample carries one entry per cooling-device member — so the sample cap bounded
  the row count while the file size scaled with the topology: **3.6 MiB at one member,
  5.7 MiB at two, 7.8 MiB at three.** Everything from two members up — the topology this
  whole programme exists for — exceeded the read cap. The file was written successfully
  and then invisible to every read path at once: `GET /validation/sessions/{id}` returned
  500, the session was absent from the listing, the boot sweep could never rewrite it as
  `interrupted`, and retention could never delete it, so it also leaked disk permanently.

  The session's effective sample cap is now **derived at start from its member count**
  against a byte budget, and the store reads under its own cap, which a compile-time
  assertion keeps above the write budget so the two cannot drift apart again. **No
  behaviour change for any realistic cooler** — a pump plus up to four radiator fans
  still records the full 7200 samples. `prune` now also deletes a session too large to
  read back, which is the only way an already-written one can be reclaimed; a merely
  unparseable session is still left alone, because a serde slip or a transient read error
  must not destroy every retained recording. **Retention is also reconciled at boot**, not
  only when a session stops — a file written by a 2.33.0 daemon is invisible to every
  normal path, so upgrading and never running another validation would otherwise have kept
  the orphan for ever. `prune` also re-stats immediately before removing, so a file that
  became readable between the scan and the delete — a flush landing in that window — is
  spared rather than taking a live recording with it.

### Added
- **Length bounds on the free-text fields of both new write paths**, found by the review of
  the two fixes above rather than by the audit. Each is `400 validation_error`, never a
  silent truncation:
  - `POST /config/cooling-device` — `name`, `kind`, the three sensor ids and
    `device_policy_id` are bounded at 256 bytes. **`preferred_sensor` is copied into every
    validation sample**, so an unbounded one scaled the session document without limit and
    reproduced the very defect the byte budget was added to fix — a route the budget could
    not see, because the probe that derives it had *assumed* a maximum sensor-id length
    instead of measuring the session's own. A guess is not a bound; it now measures.
  - `POST /validation/session/event` and `/measurement` — `detail`, `kind`, `unit`, `note`
    and `member_id` are bounded at 512 bytes, and a user-metadata *key* at 128. These
    arrays were capped by count but not by size, so unbounded text could push a document
    past the store's read cap — and because such a session is now *pruned*, that would have
    destroyed an operator's evidence rather than merely wasted disk.

  Bounding at ingest is what makes "too large to read" mean "written by a daemon older than
  this one", and therefore safe to reclaim. It also partly closes register row `AUD3-m`.

### Changed
- Documentation only: the "~1 MB per capped session" figure was wrong by up to an order
  of magnitude and is corrected at all four sites that carried it. The flush-cadence
  note's write-volume arithmetic was derived from it and was therefore also wrong —
  ~940 MiB across a three-member session, not ~120 MB. The cadence is unchanged and the
  question is recorded as `AUD3-x` rather than decided here.

## [2.33.0] — 2026-09-03

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Additive only** — one optional
new field on two existing responses. No new routes, no new capability flag, **no floor,
threshold or safety rule changes**, and both parity oracles are byte-identical. A client
that ignores the new field sees byte-identical behaviour.

### Added
- **`FanEntry.pwm_commanded_pct`** on `/fans` and `/poll` — the duty the daemon last
  **commanded** for an hwmon header, as a percent (AIO-MB Phase 6, DEC-318). The command
  half of the pair whose readback half is `pwm_readback_pct` (DEC-317), and the field a
  client should read when it needs the value the daemon actually chose.

  **Single-producer, which is the whole point.** Only the hwmon write path sets it. That
  makes it unambiguous in a way `last_commanded_pwm` is not: for an hwmon header, that
  older field carries whichever of the poll's sysfs readback and the engine's command
  wrote last (register row `AIO5-a`), so for an *uncontrolled* header it reports a
  readback despite its name. `last_commanded_pwm` is deliberately **unchanged** — its wire
  meaning is long-established and repairing it in place would alter what an uncontrolled
  header reports.

  Phase 6 §6 requires requested PWM and hardware readback as separate numbers on the GUI's
  Hardware page, because collapsing them makes a write failure, a BIOS/EC reclaim and a
  device-side override indistinguishable from one another. Only an hwmon header has both
  axes; an OpenFan channel and a GPU fan emit `None` rather than echoing their command
  back as though it were a reading. Absent means "the daemon has never commanded this
  header" — never 0%.

  Published through the state cache rather than read from `HwmonPwmController` on demand,
  so `/poll` never takes the hwmon lock: the engine holds that lock across a sysfs write,
  and a blocked write would otherwise stall the 1 Hz poll. The cache carries the field
  forward across a poll refresh for the same reason it carries `pwm_readback_pct` forward
  across a write — each producer sends `None` for the other's field, and without the merge
  the poll would blank the command within a second of every engine write.

## [2.32.0] — 2026-09-03

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Additive only** — six new
routes behind a new capability flag, one optional new field on two existing responses, and
a new `{state_dir}/validation/` directory. **No floor, threshold or safety rule changes**:
a validation session commands no hardware of its own, and both parity oracles are
byte-identical. A client that ignores everything new sees byte-identical behaviour.

### Added
- **Validation sessions: record what a cooler actually did, and produce evidence about
  it** (AIO-MB Phase 5, DEC-317). `POST`/`GET`/`DELETE /validation/session`, plus
  `/validation/session/stop`, `/validation/session/event`,
  `/validation/session/measurement`, `GET /validation/sessions` and
  `GET /validation/sessions/{id}` — all gated on the new `control.validation_sessions`
  capability. A session samples PWM, RPM, temperature, ownership and thermal state at
  1 Hz against a configured cooling device, derives a timeline of lifecycle events, and
  finalises into a typed summary with explicit result states.

  **The engine is an observer that may orchestrate.** It performs no sysfs I/O, plants no
  hooks in the profile engine or the write path, and contains no code that commands a
  duty. Where a session is asked to run a diagnostic it invokes the **existing** PWM
  verify or Phase 3 characterisation, which already own the hwmon lease, the pump floor
  clamp, the thermal refusal and restore-on-drop. It therefore acquires no second PWM
  ownership path, and cannot lower a floor or stop a pump.

- **Result semantics are explicit, and absence is never success.** Findings carry stable
  tokens — `pass`, `fail`, `observed`, `not_observed`, `not_tested`, `unknown`,
  `unavailable`, `interrupted`. A capability the hardware does not expose is
  `unavailable` and never `fail`; a diagnostic nobody ran is `not_tested` and never
  `pass`. A possible device-side override is preserved as `observed` evidence rather than
  reported as a failed write, so motherboard PWM control is not misclassified as broken.

- **`FanEntry.pwm_readback_pct`** on `/fans` and `/poll` — the hardware readback of
  `pwmN` as a percent, for hwmon headers. Optional; absent means "the daemon did not say",
  never 0%. `last_commanded_pwm` is unchanged.

- **Sessions survive a restart as `interrupted`, not as a silent gap.** Each session is
  persisted under `{state_dir}/validation/`, last five retained. At startup any session
  still marked `recording` is rewritten as `interrupted` with the timestamp of its last
  real sample. **No telemetry is fabricated for the gap.**

- **A hard sample cap of 7200 (two hours at 1 Hz), then the session finalises itself.**
  Deliberately cap-and-stop rather than a ring buffer: a ring evicts the *oldest* samples,
  which are the startup and self-bleeding evidence a session exists to capture.

### Changed
- `StateCache` gained a non-consuming `resume_generation` counter. The existing
  `take_resume_flag` is a swap with one owner, so a second observer would steal the event
  from it; `openfan_write_generation` could not be reused because it also fires on serial
  reconnect and so answers a different question.

### Notes
- `HwmonFanState.last_commanded_pwm` has two producers meaning different things — the
  poll writes the sysfs readback, the write path writes the commanded value. Recorded as
  register row `AIO5-a`; unchanged here by design, because pointing the poll at the new
  field instead would alter what an uncontrolled header reports on the wire.

## [2.31.0] — 2026-09-03

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Additive only** — three new
routes behind a new capability flag, optional new fields on two existing responses, and a
new top-level `runtime.toml` section. **No floor, threshold or safety rule changes**: the
device-policy table ships generic entries only, whose pump floor *is* the constant the
engine already enforced, so a client that ignores everything new sees byte-identical
behaviour.

### Added
- **A cooler is now one device, not three coincidentally related channels** (AIO-MB
  Phase 4, DEC-316). `GET /inventory/cooling-devices`, `POST /config/cooling-device` and
  `DELETE /config/cooling-device/{id}`, gated on the new `control.cooling_devices`
  capability, describe a pump header, its radiator fans, auxiliary members and an advisory
  temperature source as one named assembly. Persisted as a top-level `[[cooling_devices]]`
  array in `runtime.toml`.

  **Topology is metadata and the profile engine never reads it.** Naming a header as a
  device's `pump_member` confers no pump protection — the 30% floor and pump-safe identify
  still come from `POST /config/header-role`, which is a separate call.
- **A trusted device-capability policy model.** A device selects a policy *by id*; the
  numbers live compiled into the daemon. The Rust `DevicePolicy` type derives no
  `Deserialize`, so no inbound payload can construct one, and the endpoint additionally
  rejects `minimum_safe_pwm` and its siblings by name rather than ignoring them. An
  absolute 20% backstop clamps every resolution regardless of table contents.

  Only generic policies ship in this release, so **no floor moves**.
- **Headers report the floor the daemon will actually enforce.** `effective_min_pwm_pct`,
  `stop_permitted` and `cooling_device_id` join `/hwmon/headers` and `/inventory/hwmon`,
  so a client can display the enforced number instead of re-deriving it from labels and
  chip names. All optional: absent means "this daemon did not say", never zero.
- **A read-only header capability audit.** `pwm_freq_hz`, `rpm_min_threshold`,
  `rpm_max_threshold` and `tach_pulses_per_rev` on `/hwmon/headers`, plus
  `supported_pwm_enable_modes` from a cited driver table (`it87` → `[0,1,2]`, `nct6775` →
  `[0,1,2,3,4,5]`; empty means **unknown**, not "none"). Pure reads — no new write path.
- **`fan_alarm` and `pwm_enable_mode` on `/poll`** — the driver's own `fanN_alarm` bit and
  the live `pwmN_enable` mode for an hwmon header. Both sampled at 1 Hz rather than frozen
  into the header snapshot: an alarm captured at discovery would read "clear" while a fan is
  failing, and the daemon writes `pwmN_enable` itself when it takes a header over, so a
  captured mode would report the pre-takeover value for the process lifetime. Absent means
  not known, never "no alarm".

### Changed
- The pump-protection union now has exactly one definition, `roles::is_pump_protected`;
  `AppState::header_is_pump_protected` is the lookup wrapper around it. Forced rather than
  chosen — three header-mapping call sites hold the controller lock the wrapper re-takes,
  so a second copy of the predicate was the alternative to a deadlock.

## [2.30.0] — 2026-09-02

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Additive only** — one new
field on an existing response, and one field whose value is corrected. No route, no
capability flag and no safety rule changes; a client that ignores the new field sees the
same behaviour it saw against v2.29.0, minus the false value.

### Fixed
- **A characterisation run reported `restore_failed: false` on three exits where it had
  deliberately NOT restored the header** — the daemon was shutting down (DEC-290), the
  thermal ladder was forcing (DEC-295), or the pre-sweep duty could not be read at all.
  The field's own contract says `false` means the header is back where the sweep found
  it, so each of those published a header parked at the last swept duty as a success,
  and nothing re-runs a skipped restore once the force clears. The verify path has
  reported its shutdown skip since DEC-290; characterisation was the outlier.
  (Audit row `AUD2-c`, DEC-315)

### Added
- **`restore_outcome` on `GET /diagnostics/characterization`** — a stable token saying
  *why*: `pending` | `restored` | `write_failed` | `skipped_shutting_down` |
  `skipped_thermal_force` | `no_original_duty`. `restore_failed` is now derived from it,
  so the two cannot disagree. The reason is load-bearing rather than decorative: on
  `skipped_thermal_force` the header is being held high on purpose, and the advice the
  old single "restore failed" message warranted — re-assert your intent — is the one
  action a client must not take until the ladder releases.

### Changed
- **A sweep that aborted before writing anything now reports `restored`.** It left the
  header exactly where it found it, and raising a finding there would trade the old
  false success for a false alarm. The two authority skips are also checked *before*
  the unreadable-duty case, so where both apply the client hears about the authority.
- Narrowed the documented claim that the pre-sweep duty is "restored on every exit path"
  to "every exit path on which nothing else owns the header", matching the narrowing
  DEC-295 already applied to DEC-134's identical claim for calibration. Corrected in
  `daemon.md`, `docs/USER_GUIDE.md` and the module's own header.

## [2.29.0] — 2026-09-02

Pairs with `control-ofc-gui` >= v2.23.0 (unchanged floor). **Additive only** — three new
endpoints and one capability flag; every existing route, response shape and safety rule is
untouched. A client that does not know the flag behaves exactly as it did against v2.28.1.

AIO-MB Phase 3: a deeper PWM/RPM response diagnostic, **alongside** the quick verify.

### Added
- **`POST /hwmon/{header_id}/characterize`** — walks a motherboard PWM header across a
  series of duties (30-100% by default) and reports what it measured at each. Returns
  `202` immediately and runs the sweep daemon-side; the client polls. Gated on the new
  `control.pwm_characterization` capability.
- **`GET /diagnostics/characterization`** — the current or most recent run, including
  points measured so far, so a client can show progress while the sweep is still going.
  `404` when no run has ever been started.
- **`DELETE /diagnostics/characterization`** — asks a running sweep to stop. Cooperative:
  the current point finishes settling, then the header is restored.
- **Command acceptance, PWM readback and physical RPM response are three separate
  verdicts**, never one pass/fail. A pump whose firmware overrides PWM during its startup
  or self-bleeding period reports a *correct* readback with RPM pinned high; collapsing
  the axes would call that a write failure, which is the wrong conclusion. The summary
  reports it as `possible_device_override` instead, and derives monotonicity, a candidate
  dead zone, a candidate PWM clamp and whether another controller interfered.

### Safety
- **0% is unreachable through the new endpoint, for every header and every input.** Points
  are clamped into `[max(20, header floor) .. 100]`, where a pump-protected header's floor
  is the existing hard 30%. One flat rule rather than a role-conditional branch — the
  invariant is then provable in a single assertion instead of being only as correct as
  role resolution is on every board. The pump term reads the **union** predicate
  `header_is_pump_protected`, never the wire `role` (DEC-312), so a user who relabels a
  `PUMP` header `chassis_fan` does not strip its floor.
- **Points are swept ascending**, so a sweep that aborts part-way leaves the header high
  rather than low.
- **The pre-sweep duty is restored on every exit path** — completion, cancellation, a
  failed write, a reclaim, a thermal abort, and the runtime dropping the detached task at
  shutdown. Two skips, both deliberate: while the thermal ladder is forcing (DEC-295,
  restoring would lower the header back under the forced duty) and while the daemon is
  shutting down (DEC-290/277-c, firmware already owns the header).
- **The run claims the existing single verify/calibrate slot**, so a characterisation, a
  verify and a calibration can never drive hardware at the same time, and the engine's
  write phase is paused for its lifetime. The pause deadman is renewed once per point
  (DEC-296) so it measures liveness rather than total duration.
- **The thermal ladder still outranks the diagnostic, unchanged.** The forced-safety
  branch runs and `continue`s before the engine's `verify_active()` gate, exactly as
  before; a sweep is *refused* while the system is hot (`409 thermal_abort`) or while the
  ladder is forcing (`409 validation_error`, retryable), and aborts if either becomes true
  mid-run.
- A header reclaimed by BIOS/EC mid-sweep (`pwm_enable != 1`) aborts the run and is
  reported, rather than continuing to measure a header something else is driving.

### Changed
- `hwmon_verify_handler`'s inner sysfs-read closure is now the shared
  `read_header_state()`, used by both the verify and the sweep. No behaviour change —
  extracted so the two cannot drift apart (DEC-276's lesson).

## [2.28.1] — 2026-09-02

Pairs with `control-ofc-gui` ≥ v2.23.0 (unchanged floor). **Documentation only — no code
change.** The daemon binary is byte-for-byte 2.28.0; this release exists so the corrected
`USER_GUIDE.md` reaches the copy installed at `/usr/share/doc/control-ofc-daemon/`.

### Changed
- **The support table no longer says a hwmon AIO pump runs at a constant speed.** That was
  the GUI's rule (DEC-157) and the GUI has retracted it (DEC-312): whether a pump should
  hold a fixed speed is a property of the cooler, not a fact about pumps, and vendors
  contradict each other about their own hardware. The row now reads "fixed speed or a
  temperature curve, always floored at 30%", and notes that a motherboard-connected pump
  is configured the same way once its header carries the `pump` role — assigned by GUI
  v2.51.0, or by `POST /config/header-role` for any other client.

## [2.28.0] — 2026-09-01

Pairs with `control-ofc-gui` ≥ v2.23.0. **Additive only** — new fields, one new endpoint,
one new capability flag; nothing removed or reshaped. One *behaviour* narrows deliberately:
a fan header classified as a pump is no longer stopped during identification.

AIO-MB Phase 1 (DEC-311): motherboard-connected AIO pumps become first-class, without a
vendor-specific backend.

### Added
- **Per-channel PWM header roles.** `GET /hwmon/headers` and `GET /inventory/hwmon` gain
  `role` (`unknown` | `cpu_fan` | `pump` | `radiator_fan` | `chassis_fan`) and `role_source`
  (`none` | `label` | `chip_mapping` | `user_assigned`). This is orthogonal to the existing
  chip-level `is_aio`, whose meaning is unchanged: a pump on a motherboard `AIO_PUMP` header
  is `role: "pump", is_aio: false`, which is exactly the case `is_aio` could never express.
  Clients must render an unrecognised token rather than dropping the header.
- **`POST /config/header-role`** — assign or clear a header's role
  (`{"header_id": "...", "role": "pump"}`; `"role": null` clears). Persisted in `runtime.toml`
  under `[hardware.header_roles]` and effective **immediately**, not at next start. This is
  the mechanism that makes the feature work on real hardware: many boards' Super-I/O chips
  publish no `pwmN_label`/`fanN_label` files at all (measured: `it8696` exposes five channels
  and zero label files), so there is no label evidence to infer from and the user's assignment
  is the only signal that a header drives a pump.
- **Capability `control.header_roles`.** Gate any pump-specific UI wording on it — an older
  daemon stops pumps, so "the pump will only change speed" is untrue against one.

### Changed
- **Fan identify no longer stops a pump.** The request is unchanged (`action: "stop"`); the
  daemon decides what it means from the header's role and reports it in the new `mode` field:
  `"stop"` (forces 0, floor-exempt — every non-pump role, unchanged) or `"pump_perturb"`
  (shifts the duty ~25 points clear of the baseline, **upward wherever there is headroom**,
  clamped into `[30, 100]` — never 0, never below the pump floor). `identify_pwm_percent` and
  `baseline_pwm_percent` accompany it, and `mode` + `identify_pwm_percent` also appear on each
  `/status` + `/poll` `fan_identify[]` entry.

  This supersedes DEC-166's "floor-exempt — even a pump". That rule assumed finding a pump
  *requires* stopping it; an audible RPM change identifies it just as well, and losing coolant
  flow to locate a header is not a trade to make on a user's behalf. Because the daemon owns
  the decision, a GUI built against an older daemon gets the safe behaviour for free.
- **A user-assigned `pump` role earns the 30 % hard floor and the stop-snap exemption**, as a
  union term — it can add a floor, never remove one. Roles the daemon *infers* change no floor
  at all: every inferable pump was already inside the existing floor set, which is why both
  parity oracles (`role_classification.json`, `parity_vectors.json`) pass unchanged with their
  fixtures untouched.

### Fixed
- **`POST /hwmon/{id}/verify` no longer drives a pump below the daemon's own pump floor.** Its
  downward test duty was a flat 20 % — under the 30 % floor the same daemon enforces on every
  eval tick — so verifying a pump that idles above 50 % (the normal case for a motherboard AIO)
  under-drove it for the ~6 s settle window. Verify was the one write path that never consulted
  `member_effective_floor`. A pump header now prefers the upward 80 % test and clamps the
  downward fallback at the floor; ordinary headers keep the 20/80 pair exactly as before.
  (Register row `AIO1-a`.)

### Known limit
- Identify remains a structural no-op for a fan that no control in the **active profile**
  commands — it rewrites the engine's command vector, and an uncommanded fan produces no
  command to rewrite — while still returning `200` with a deadman TTL. Pre-existing, unchanged
  here, and deliberately out of scope; recorded as register row `AIO1-b` for Phase 2.

## [2.27.0] — 2026-09-01

Pairs with `control-ofc-gui` ≥ v2.23.0; **no wire, schema or API break** — no field was
added, removed or reshaped. `rpm` was already optional and a fan's absence already meant
"not currently readable"; what changed is that the daemon now only populates them when it
actually measured something.

### Fixed
- **Fan telemetry is now only reported when it was actually measured.** Three
  places published values that looked like readings and were not:
  - **A GPU fan's `age_ms` was reset by a *command*.** `set_gpu_fan_commanded_pct`
    refreshed the timestamp that `/fans` publishes as the reading's age, so a
    commanded GPU fan reported an age near zero beside an `rpm`/`duty_pct` frozen
    at whatever the last real poll saw. This is byte-for-byte the defect DEC-302
    removed from the OpenFan path one function above it, and was the surviving
    instance of it. (`OFS-k`.)
  - **A never-polled OpenFan channel reported `rpm: 0` as though measured.** The
    zero was the struct's initial value; `rpm_polled` already recorded the
    difference and was already consulted for `stall_detected`. Such a channel now
    omits `rpm` — which is what that optional field is for — instead of being
    indistinguishable on the wire from a genuinely stalled fan. A channel that was
    polled and genuinely read zero is unaffected. (`OFS-l`.)
  - **A hwmon fan header that stopped reading was published forever.** The cache
    only ever inserted, so a header whose chip unbound mid-session kept an entry
    whose `age_ms` climbed without bound. Entries nothing has refreshed for five
    poll intervals are now evicted. A fan under active control cannot be evicted —
    every engine write refreshes its entry, which is why this keys on the entry's
    age rather than on a poll-failure streak. (`OFS-m`.)

  No wire shape changed: `rpm` was already optional, and fan absence already meant
  "not currently readable". Nothing on the control or safety path reads this map —
  the engine takes headers from `HwmonPwmController` — so fan control is unaffected.

## [2.26.0] — 2026-09-01

Pairs with `control-ofc-gui` ≥ v2.23.0; **no wire, schema or API break** — no field
was added, removed or reshaped. `thermal_safety.emergency_threshold_c` on
`/diagnostics/hardware` keeps its type and its meaning ("the emergency trip point")
and only stops being the same number on every machine, so a client that renders it —
which is what every released GUI does — needs no change. A client that hardcoded 105
would now be wrong, and never had licence to.

### Fixed
- **[SAFETY] The thermal ladder's lower rungs could REDUCE cooling.** Two of the
  three forced duties were applied as a *replacement* for the active profile's
  output rather than as a floor over it, so the safety path could command fans
  **down**. The 60% recovery step is the reachable case: it fires on the tick a
  CPU crosses back down through the release point — still hot, seconds after a
  105 °C excursion — and drove every OpenFan channel and writable hwmon header
  to exactly 60%, overriding a curve asking for far more. Measured on the
  project's own test fixture: a curve asking for **84%** at 70 °C was driven to
  **60%**. The 40% no-CPU-sensor fallback did the same to a control driven by a
  still-healthy GPU or coolant sensor. The 105 °C emergency itself was never
  affected — 100% is the maximum, so replacement and floor coincide there.

  Each output now receives `max(commanded, forced)`. An output no control
  commands still receives the forced duty unchanged, which is what preserves the
  emergency's **reach** — the property whose loss was the v2.38.0 P1. The change
  is monotone by construction: no fan is ever driven lower than before.

  The module doc had called the 60% step a "recovery floor" since it was written,
  which is how this survived — the name and the behaviour disagreed on a safety
  path, and only the name was ever read. (DEC-307, register row `D1-j`.)

### Changed
- **[SAFETY] The thermal emergency trip point is now per-machine, derived from the
  CPU's own reported design ceiling.** A single global 105 °C cannot be right for
  every part, because a CPU is *designed* to sit at its Tjmax under sustained
  load: an Intel Core Ultra 200S desktop (ceiling ~105 °C) could latch the
  emergency while perfectly healthy and then never release, since release needs a
  reading at or under 80 °C that a part holding Tjmax never produces. A Core Ultra
  laptop (~110 °C) sat above the trigger at its own ceiling.

  Where the kernel publishes `tempN_crit` — which `coretemp` documents as the
  maximum junction temperature, read from a model-specific register — the daemon
  now uses `min(ceiling + 5 °C, 115 °C)`. The derivation is **raise-only** (no
  machine trips below 105 °C, so nobody regresses), **capped** (a lying chip
  cannot push the trip point past the CPU's own THERMTRIP and silently disable
  the emergency), and gated on authoritative CPU chips only — a Super-I/O
  `CPUTIN` publishes a `crit` too, and it is a board alarm point, not the CPU's
  ceiling. Recomputed each tick, since sensors come and go.

  The 5 °C margin is chosen so mainstream Intel 12th-14th gen desktop (~100 °C)
  lands on exactly the historical 105 — the common case is unchanged.

  **In practice this is Intel-only, and that is the honest description.** Measured
  on real hardware: `k10temp` on Zen publishes no `crit` at all, only
  `temp1_input` and `temp1_label`. AMD therefore keeps the 105 °C floor — which
  is the right outcome rather than a gap, since with a ~95 °C ceiling AMD was
  never the broken case.

  `/diagnostics/hardware` now reports the trip point the engine actually acted on
  rather than the constant, published in the same write as `thermal_state` so the
  two cannot disagree. **A client must render that field, not assume 105.**
  (DEC-308, register row `D1-q`.)
- **An expiring manual override is now swept during a thermal hold, not after
  it.** The forced path used to short-circuit before the override sweep, so an
  override that lapsed mid-emergency kept its deadman auto-restore deferred and
  stayed listed on `/status` until the ladder released — which on a long
  105 °C→80 °C descent is minutes. A consequence of evaluating the profile on
  forced ticks (above), not a separate fix. (DEC-307.)
- **`skipped_controls[]` is live during a thermal event rather than frozen.**
  Forced ticks now evaluate curves, so they also commit what they learned: a
  control that becomes unresolvable during a hold is reported at once instead of
  at the end of it. This also closes a silence the same change would otherwise
  have introduced — activating a profile mid-hold cleared the list, and nothing
  on the forced path could refill it. (DEC-307.)

## [2.25.0] — 2026-09-01

### Changed
- **A `CPUTIN` pin on any Nuvoton `nct67xx` chip is no longer treated as a CPU
  temperature on ASUS boards** — previously only `nct6776` was (DEC-294). The
  kernel's remedy is scoped to the board, not the chip, so every sibling fell
  through and was promoted to a CPU sensor. lm-sensors#283 is the case: an
  `nct6775` reporting `CPUTIN` at 123.5 °C beside a `coretemp` package
  temperature of 42.0 °C — fresh, in range, 81.5 °C wrong, and enough on its own
  to pin every fan at 100% until reboot. **The vendor gate is unchanged**: the
  same chip on a non-ASUS board keeps its CPU classification.

### Added
- **A CPU temperature that is absurd next to the motherboard is now quarantined
  instead of trusted.** A `CpuTemp` reading is rejected only when it is *both*
  more than 15 °C below the hottest motherboard sensor *and* below 10 °C — either
  condition alone would misfire on an idle CPU under a hot VRM, or on a genuinely
  cold machine. It fails open when no motherboard sensor is present. This closes a
  silent failure: with a single CPU sensor reading 0 °C, nothing out-ranked it, the
  absent-sensor floor never engaged because a sensor *was* present, and every fan
  curve ran at 0 °C with nothing logged. Rejections take the existing quarantine
  path, so they appear in `unavailable_sensors[]` and recover on their own.

### Fixed
- **`safety.rs` described the 60% recovery step as a "floor". It is not** — it is a
  replacement, and it can drive fans *down* from 100% immediately after an
  excursion. The false claim is corrected; the behaviour is tracked as `D1-j` and
  is deliberately **not** fixed here, because the correct form needs a
  `SafetyWriteBackend` trait change and interacts with the shared `BoundedWrite`
  invariant (DEC-289/298/299).
- **A `PLAUSIBLE_MAX_C` comment justified itself with "hardware THERMTRIP fires
  around 125 °C", a figure that cannot be sourced.** AMD publishes no THERMTRIP
  value in either public PPR; Intel publishes ~130 °C. The constant (250 °C) is
  unchanged and was never wrong — the *reason* was.
- **Two operator-visible log lines restated the thermal trip point instead of
  reading it** — the startup "thermal safety rule active" line and the poll-interval
  clamp warning — so either would have misreported the threshold the moment it
  changed. Both now read the constant. Roughly thirty comments and documents that
  spelled the number into the *name* of the rule ("the 105 °C emergency") are now
  number-free, and the daemon's own tests derive the trigger from the constant
  rather than restating it.

### Note
- **A raise of the thermal trigger to 110 °C was implemented and withdrawn before
  release (DEC-305).** Intel Core Ultra 200S desktop has a Tjmax of exactly 105 °C,
  so a healthy chip can latch the emergency and never release — but 110 °C is
  exactly Core Ultra *mobile* Tjmax, so the raise would have moved the same fault
  onto laptops. With confirmed design ceilings of 95 / 100 / 105 / 110 °C across
  AMD and Intel families, no single global trigger is correct. The threshold is
  **unchanged at 105 °C** and a vendor/family-aware trigger is scheduled as its own
  change.

## [2.24.3] — 2026-08-31

No behaviour change — a test correction and three documentation corrections.

### Fixed
- **The test guarding a false "healthy engine" report did not actually test it.**
  DEC-299 fixed `GpuBackend::apply`'s lock-unavailable early return, which is
  taken precisely when a wedged GPU write is holding the write lock and which
  used to return without refreshing the write-progress flag — so `/status` would
  report a healthy engine indefinitely while a fan held its last duty. The fix
  was right; its regression test was not. Validity-checked by removing the fix,
  the test still passed.

  The reason was not what the register row guessed. Ticks 3-5 never reached the
  lock at all: they re-commanded the same duty tick 1 had already failed, so the
  60s failure cooldown emptied the pending writes and the tick returned down a
  different path that reports "stalled" for an unrelated reason. They now command
  a duty that escapes both the failure cache and the 5% coalescer, and the test
  asserts a new `#[cfg(test)]` accessor proving each tick took the lock-skipped
  branch — the path, not merely the outcome. Re-validity-checked: with the fix
  removed it now fails at tick 3, as it always should have. No production
  behaviour changed. (299-a)

### Documentation
- `daemon.md` gated bounded backend writes on daemon `>= 2.23.2`, a version that
  was never tagged. That work shipped in **2.23.5**. (298-a)
- The `[2.23.5]` entry below quoted two engine-health reason strings that this
  daemon has never emitted — draft wording that was narrowed before release. It
  now quotes what `health/staleness.rs` actually returns. (298-a)
- The comment in `health/staleness.rs` explaining why both strings are worded
  narrowly said they "were removed", which reads as a reword of shipped text.
  They were removed *in draft*, before DEC-289 shipped at all. That distinction
  is why the GUI contract's copy of the wider wording looked like drift to be
  reconciled for the whole life of the feature, rather than text that was never
  emitted. (298-a)

## [2.24.2] — 2026-08-31

### Fixed
- **Subsystem health said "readings fresh" while readings were ageing (DEC-302).**
  `/status` reports a freshness figure for the `openfan` and `hwmon` subsystems, but
  the number behind it only ever recorded whether the poll loop *returned* — not
  whether the data it returned covered anything. A frame carrying three of ten
  OpenFan channels refreshed that timestamp exactly like a full one, so `/status`
  reported `openfan: ok — readings fresh` while `/poll` showed seven channels whose
  age climbed without bound. Nothing caught it: a short frame is a *successful*
  read, so it never counted as a failed poll and never produced a log line.

  The `openfan` entry now answers the two questions separately — is the poll loop
  alive, and is the data fresh — and reports the worse of the two, with a reason
  naming which one fired. An incomplete frame is also logged now, once when coverage
  breaks and once when it returns. It still does **not** count as a failed poll: the
  link answered, and a reconnect would reset the controller (DEC-291).

  `hwmon` deliberately still reports poll liveness alone. Its coverage is already
  owned by sensor discovery and the DEC-193 quarantine, and it holds some readings
  frozen **on purpose** (DEC-272), so a reading's age is not a freshness signal
  there — applying the same rule to it reported the safety machinery's own
  protective state as a fault. For per-sensor hwmon freshness, read
  `sensors[].age_ms` and `unavailable_sensors[]`, which answer it directly.

- **A PWM command made a stale fan reading look fresh (DEC-302).** Commanding a duty
  on an OpenFan channel refreshed that channel's *reading* timestamp, which `/fans`
  and `/poll` publish as the fan's `age_ms`. The fan therefore reported an age near
  zero beside an RPM frozen at whatever the last real poll had measured — and the
  stall verdict was computed from that frozen value.

  This was widest exactly where it mattered least tolerable: a 105 °C thermal force
  writes every channel, and a short write acknowledgement completes on a degraded
  link far more readily than a full RPM read, so "poll dead, writes still acking"
  showed every fan as freshly measured while nothing was measuring.

- **A machine with no OpenFanController reported itself permanently unhealthy
  (DEC-302).** With no controller attached, nothing ever polls one, so the `openfan`
  subsystem sat at `crit — "never received data"` for the life of the process and
  dragged `overall_status` to `"crit"` with it. Every hwmon-only machine showed a
  permanently red health ribbon. The entry now says there is no controller, and
  still reports `crit` normally when one is attached and its poll loop has died.

## [2.24.1] — 2026-08-31

### Fixed
- **The OpenFan serial link now correlates each reply with the command it answers,
  so a `SetPwm` acknowledgement can no longer be cached as a fan's RPM (DEC-301).**
  `send_command` accepted the first `<`-prefixed frame it read, whatever request that
  frame actually answered. The link has two independent 1 Hz users behind one mutex
  sharing one stateful reader — the poll loop's `ReadAllRpm` and the profile engine's
  per-channel `SetPwm` — so a single reply left unread put the pipeline permanently one
  frame behind, and every later reply was misattributed until it drained.

  The visible consequence was **fabricated fan telemetry**. A `SetPwm` ack is a
  *single*-channel frame whose payload is the echoed raw PWM byte, so the poll loop
  periodically cached one channel with an "RPM" that was really its commanded duty.
  Measured against a running daemon at idle: **154 of 1600 fan readings (~10%)** carried
  an echo-fabricated RPM, with values that were exactly `percent_to_raw()` of that
  channel's commanded percent (35% -> 89, 34% -> 87, 28% -> 71, 29% -> 74). This broke the
  project's standing rule that `rpm` is hardware-measured and `last_commanded_pwm` is
  daemon-tracked and the two are never conflated.

  The discriminator needed to prevent this was already on the wire and already parsed —
  `decode_line` has always returned `command_code` — but until now **nothing outside the
  test suite read it**.

  Correlation is on the opcode **and, for a per-channel command, the channel the
  controller echoes back**. The opcode alone is not sufficient and assuming it was is
  the one defect both DEC-301 reviewers found independently: every per-channel write
  carries opcode `0x02`, so a one-frame offset *within* a tick's ten back-to-back
  writes — exactly the shape of `force_all` at 105 °C — would match on opcode and slip
  through, each write confirmed by its predecessor's acknowledgement and the last one
  never acknowledged at all.

  A reply that does not answer the command is now drained rather than returned, which
  *resynchronises* the link within a single exchange instead of erroring (an error would
  never heal: the next exchange would inherit the same offset). The drain is bounded by
  the existing wall-clock deadline and by a new `MAX_STALE_FRAMES` cap, reported as a
  distinct "did not resynchronise" protocol error so it cannot be mistaken for the
  debug-line error.

  **This also restores the meaning of a write acknowledgement, including on the
  emergency path.** `set_pwm` discards the response and treats `Ok` as "the controller
  took it", and the 105 C `force_all` writes through that same path — so an OpenFan
  emergency write used to be confirmed by whatever frame happened to be next in the
  buffer. It is now confirmed only by an acknowledgement for **that channel**.
  **Operators should expect
  this to surface genuine write failures that were previously swallowed**: a write the
  controller never acknowledges now times out and is reported, where before a stale frame
  could stand in for the missing ack. That is the intended direction — it is the same
  failure mode DEC-250 closed for the wrong-device case, reopened here by framing desync
  on the right device.

### Changed
- **The Rust toolchain is pinned, so the local quality gate and CI compile with the
  same compiler (DEC-300).** `cargo clippy -- -D warnings` makes every new clippy
  lint a hard error, and clippy ships new lints every six weeks. Nothing pinned a
  toolchain, so the local gate ran rustup `stable` while CI's
  `dtolnay/rust-toolchain@stable` floated to the newest release — on 2026-08-29 that
  was 1.97.1 locally against 1.98.0 in CI. A lint that does not exist in the
  developer's compiler cannot be caught locally at any level of diligence: the gate
  passes, then CI fails.

  This was most of the CI failure history rather than a hypothetical. **15 of the
  daemon's 16 CI failures were the same lint** — `clippy::collapsible-match` in
  `daemon/src/profile.rs` — across three rustc versions between 2026-07-01 and
  2026-07-21, almost all on release commits. Since DEC-263 made `ci-green` a
  fail-closed gate on `github-release`, that drift no longer merely reds a run: it
  blocks publication *after the tag is already public*, forcing a delete-and-retag.

  A new `rust-toolchain.toml` pins the compiler and its components; the rustup shim
  installs it automatically, so the local gate self-corrects. CI installs from that
  file rather than restating a version — a second source of truth is the exact
  failure DEC-258 recorded for the GUI's `ruff` pin.

  **The Arch package build is deliberately unaffected** and keeps building against
  whatever `rust` Arch ships, which is correct for a distro package: `PKGBUILD`
  exports `RUSTUP_TOOLCHAIN=stable`, which overrides the file, and a non-rustup cargo
  ignores it entirely. The clean room also runs only `cargo build`/`cargo test`,
  never clippy.

### Added
- **An advisory `clippy-next` CI job runs clippy against floating latest stable
  (DEC-300).** A pin nobody bumps accumulates lint debt and then breaks loudly with
  no attribution to any single commit. This job is `continue-on-error`, so it can
  never block a merge or a release; when it goes red, a toolchain bump is due. It
  forces `RUSTUP_TOOLCHAIN=stable` because the repo's own pin file would otherwise
  override it and the job would silently re-test the pinned compiler.

### Fixed
- **CI now runs the gate the documentation says it runs (register row AUD-t).**
  `.github/workflows/ci.yml` passed `--all-features`, which selects nothing because
  the crate declares no `[features]` table, and never ran `cargo test --doc`, which
  `--all-targets` suppresses — while `CLAUDE.md § Quality gates` called itself the
  single source of truth. No coverage was lost (the crate has zero doctests, verified
  again here); what was missing was any signal that the first doctest anyone wrote
  would have had no CI coverage.

Pairs with `control-ofc-gui` ≥ v2.23.0; **no wire, schema or API break** —
`api_version` stays 1, the error envelope is unchanged, and no request or
response shape moved. Both changes are internal to the daemon: one pins the
Rust toolchain so the local gate and CI share a compiler, the other corrects
the OpenFan serial link's request/response correlation. The GUI needs no
change to benefit — `rpm` for OpenFan channels simply stops carrying
occasional echoed PWM values. Operators may see OpenFan write failures
reported that were previously swallowed; that is the fix working, not a
regression.

## [2.24.0] — 2026-08-29

### Fixed
- **A motherboard sensor that is documented as "not connected" could pin every
  fan at 100% until the daemon restarted — on a cold machine.** The kernel's own
  hwmon documentation says that on various ASUS boards with the NCT6776F, the
  `CPUTIN` pin is not really connected and reports unreasonable temperatures; the
  canonical symptom is a near-constant ~115 °C at idle. The daemon classified that
  channel as a **CPU** temperature, and the thermal ladder takes the *hottest* CPU
  sensor — so one disconnected pin outranked every healthy sensor on the board,
  tripped the 105 °C emergency, and never released, because release requires a
  reading at or below 80 °C and a stuck 115 never gets there. Fans forced to 100%,
  profile evaluation skipped, no way back short of a restart.

  Nothing upstream could catch it. 115 °C is a *plausible* reading, so the range
  check added in 2.23.1 accepts it and the read **succeeds** — and the DEC-193
  quarantine only ever sees sensors that fail to read. The value was wrong, not
  malformed.

  That chip, on that vendor, with that label is now classified as a motherboard
  temperature, so it never reaches the ladder at all. The rule is gated on all
  three: the same chip on a non-ASUS board wires the pin normally and keeps its
  CPU classification, because discarding a real CPU sensor is the worse fault of
  the two. The GUI has flagged this exact combination as bogus in its sensor
  detail view for a long time — it simply had no way to tell the daemon, which is
  the part this fixes. (DEC-294)

- **`nct6776`'s `PECI` and `TSI` channels were not recognised as CPU
  temperatures.** The chip was missing from the Nuvoton family branch entirely, so
  it fell through to a generic label fallback that matches `cpu`/`tctl`/`tccd` and
  knows nothing about `peci` or `tsi` — meaning the two sources the kernel
  documentation tells you to *prefer* on this chip were treated as motherboard
  sensors and were unusable for CPU-driven fan curves. They now classify as CPU
  temperatures, as they already did on every sibling chip. Without this, the fix
  above would have demoted the bad sensor and left an affected board with no
  usable CPU temperature at all. (DEC-294)

- **A curve bound to the demoted sensor now holds instead of driving on a frozen
  value.** CPU-kind sensors are exempt from the curve evaluator's staleness check;
  a motherboard-kind one is not. So if you had a fan curve pointed at that
  `CPUTIN` channel and its readings go stale, the control is now skipped and its
  fans hold their last duty — where previously it kept driving on the frozen
  number. Fans never go *down* as a result, and the control is listed in
  `skipped_controls` so the GUI shows why. (DEC-294)

- **A thermal emergency no longer floods the journal with failures it was never
  going to avoid.** During a 105 °C hold the emergency writes every hwmon header
  it can find — including ones discovered read-only, which can only ever fail
  with a permission error. The ordinary control path has skipped those for a long
  time; the emergency path did not, and unlike the ordinary path its failures are
  logged immediately rather than through a throttle. One read-only header
  therefore produced one `THERMAL SAFETY … FAILED` line every second for the
  entire 105 → 80 °C hold, burying any genuine write failure at exactly the moment
  it mattered. Read-only headers are now skipped, as they already were elsewhere.
  This does not change what the emergency can actually drive, with one honest
  exception: `is_writable` is decided by a probe at startup and defaults to
  "no" if that probe errors, and the header set is not rebuilt afterwards. A
  header whose probe failed at boot is now permanently outside the emergency,
  where before it was attempted every second. Such a header was already
  excluded from *all* normal fan control for the same reason, so the two paths
  now agree — but the exclusion is real and is recorded rather than glossed.
  (DEC-295)

- **An OpenFan calibration can no longer fight thermal safety for the fan.**
  Calibration refused to run above 85 °C, but the thermal emergency triggers at
  105 °C and does not release until 80 °C — so in the whole band between 80 and
  85 °C the temperature check passed while the daemon was still forcing every fan
  to 100 %. A sweep starting there would drive the channel through its steps
  beginning at 0 %, with the emergency re-forcing 100 % a second later, for the
  length of the sweep. Separately, the sweep's final act is to restore the duty it
  recorded before starting, which could put a channel back to (say) 30 % under an
  active emergency.

  Calibration now refuses to start or continue while thermal safety is forcing a
  duty, and skips the restore in that case, leaving the fan at the forced duty and
  logging why — otherwise it reads as a stuck fan. Aborting above 85 °C is
  unchanged, and a normal abort still restores as before.

  The refusal is a `409` telling the client it may retry, not a `400` — the
  condition is a transient state of the daemon, not a bad request, and it clears
  on its own. **Two consequences worth knowing.** On a machine with no CPU
  temperature sensor at all the daemon holds a permanent 40 % fallback, so
  calibration is refused indefinitely there; the message names the state so it
  does not read as an unexplained failure on a cool machine. And a skipped
  restore is not retried later: while the force holds, the engine keeps writing
  the forced duty, but once it releases an idle daemon with no active profile
  commands nothing, so the channel stays at that duty instead of returning to
  its pre-calibration value. Re-running calibration, or activating a profile,
  restores normal control. (DEC-295)

- **One abandoned fan diagnostic could disable every later one until the daemon
  restarted.** The verify slot — shared by the hwmon verify, the GPU fan verify
  and OpenFan calibration — carries a deadman so that an abandoned diagnostic
  cannot pause fan control forever. That deadman only ever did half its job: it
  released the engine, which resumed writing on schedule, but it never released
  the *slot*. So a single diagnostic whose cleanup did not run left the daemon
  believing one was permanently in progress, and every subsequent verify and
  calibration was refused with "already in progress" — for the life of the
  process, with nothing actually running.

  This became reachable rather than theoretical in 2.23.3, which moved the
  cleanup inside the blocking write task so it could not be skipped by a client
  disconnect. That is the right place for it, but it means a write wedged in the
  kernel now holds the slot too.

  An elapsed deadman now frees the slot as well. Two things make that safe.
  Releasing is ownership-checked, so a wedged diagnostic that finally returns
  minutes later cannot cancel the pause belonging to whichever diagnostic started
  after it — which would have let the engine write over that one's test value and
  report a false result. And the deadman measures whether a diagnostic is still
  *alive* rather than how long it has run: a verify checks in after its settle
  period, so one that is merely slow keeps its slot instead of being superseded
  and having its restore fail. Both decisions are logged, because the situation
  they exist to survive should not be silent. (DEC-296)

- **An abandoned GPU fan verify left the card's fan pinned at its test speed.**
  The GPU verify writes a test speed, waits, reads back, then restores — but the
  wait was an async pause, so if the client went away (or the GUI's own timeout
  fired) the request was dropped mid-wait and the restore never ran. This is the
  same defect fixed for the motherboard fan verify in 2.23.3, one endpoint over,
  in a handler whose own notes claimed it already mirrored that one. The whole
  sequence now runs as a unit that cannot be abandoned part-way.

  Two things made it worse than it looked. The verify never told the daemon's
  cache what it had commanded, so the cache still reported the pre-verify duty —
  and the fan-control engine skips writes within 5% of what it believes is
  already set, meaning even an active profile would not have corrected the
  stranded fan. And with no prior duty recorded, the skipped restore was the one
  that hands the fan back to firmware control, so the card was left on a flat
  manual curve instead. The direction is at least the safe one: the test speed is
  deliberately biased upward, so a stranded fan runs fast, never slow. (DEC-297)

- **An abandoned OpenFan calibration left the channel at whatever step it had
  reached** — and the early steps are 0%, so that one strands a fan SLOW. The
  sweep now restores the pre-calibration duty on every exit, including
  cancellation. Deliberately not by making the sweep uncancellable: a sweep runs
  up to five minutes, and forcing it to completion after the client has gone would
  hold the fan-diagnostic slot for that whole time. (DEC-297)

- **A fan verify could start while thermal safety was forcing every fan.** Verify
  refused to run above 85 °C, but the thermal emergency triggers at 105 °C and does
  not release until 80 °C — so, exactly as for calibration in this same release,
  the band between 80 and 85 °C passed the check while the daemon was still forcing
  100%. A verify starting there drives its target fan to a test duty against that
  force. It now refuses, with the same retryable `409` and the same
  state-naming message calibration uses. (DEC-297)

- **A thermal emergency could take an extra second to reach a slow fan device.**
  When a fan write from the previous second had not finished, the next tick
  waited for it — and then issued nothing of its own, dropping the commands it
  had just computed. If that was the tick where the CPU crossed 105 °C, the first
  forced write did not go out until the tick after; against a device
  consistently slower than the one-second write budget, forced writes went out
  every *other* second. Each tick now issues its own write after collecting the
  previous one, and the two share a single budget so the tick still costs no
  more than it did.

  This was a delay, not a loss of reach: a write stuck in a driver holds the
  device lock, so an emergency write issued alongside it would have queued behind
  it anyway. It is also strictly better than the behaviour before 2.23.2, where a
  stuck write froze the engine and the forced write never went out at all.
  (DEC-298)

- **A wedged GPU fan write could still freeze fan control entirely — the last of
  the three write paths to be fixed.** 2.23.2 bounded the motherboard and OpenFan
  write paths so a device that stops responding cannot stall the daemon's control
  loop; the GPU path was left, and said so. It is now bounded too. Until this, a
  GPU write stuck in the driver meant the loop never completed another pass: the
  105 °C emergency never ran again, and because the task was still *alive* the
  daemon's own death-detection never fired either.

  Two things blocked it, and the second was not in the original report. The
  obvious one is the write itself. The other is that the loop waited
  **indefinitely for the GPU write lock**, which a GPU fan verify holds for its
  whole multi-second window — so a verify starting at the wrong moment froze the
  loop with no stuck device involved at all, and 2.23.5's cancel-safety work made
  that window more reachable. The loop now waits a bounded time for that lock and
  skips the GPU for that tick if it cannot have it, which is also what keeps the
  bound honest: a stuck write keeps the lock, so the next tick cannot start a
  second one.

  A stalled GPU write is now reported like the other two, so `/status` says so
  instead of showing a healthy daemon while fans hold their last duty. (DEC-299)

### Notes
- The reader's plausible-range check is unchanged, and deliberately so. It still
  cannot separate a real 105-125 °C over-temperature from a stuck sensor reading
  the same value — no reader-level bound can. This release removes the one
  instance of that class which is kernel-documented and reachable; the general
  case remains open and is tracked as `AUD-x`.

Pairs with `control-ofc-gui` ≥ v2.23.0; **no wire, schema or API break** —
`api_version` stays 1 and the error envelope is unchanged. Two endpoints
(`POST /hwmon/{id}/verify` and `POST /gpu/{id}/fan/verify`) and the OpenFan
calibrate endpoint can now return a refusal they never returned before —
`409 validation_error` with `retryable: true` — while the thermal ladder is
forcing a duty. That is an additive *outcome*, not a shape change: v2.49.2 or
newer presents it as the soft "let it cool, then retry" notice it is, and older
GUIs still show it, but worded as a verify error.

## [2.23.5] — 2026-08-28

Five fixes from a cross-stack audit, **two of them on the thermal-safety path**.
Every one was latent rather than live — each needed a second thing to go wrong
first — which is exactly why they had survived. Released as one version: 2.23.1
through 2.23.4 were incremental steps during the same session and were never
tagged or published.

### Fixed
- **A CPU sensor reporting a wildly out-of-range value could pin every fan at
  100% until reboot.** An hwmon temperature outside the plausible range was
  **clamped to 250.0 °C**
  rather than rejected — and the log line calling it "almost certainly garbage"
  was written immediately before the code went on to use it anyway. 250 °C then
  won every comparison downstream: the thermal ladder takes the **hottest** CPU
  sensor, so one faulty chip outranked every healthy one; the emergency latches
  at 105 °C and only releases at 80 °C, which 250 never reaches. The result was a
  permanent, unrecoverable thermal emergency — all OpenFan and writable hwmon
  fans forced to 100%, profile evaluation skipped, no way back short of a
  restart. It was also invisible: the DEC-193 quarantine evicts a sensor that
  fails to *read*, and a clamped read is a success, so the faulty sensor never
  appeared in `unavailable_sensors[]`.

  An implausible reading is now a **read error**. The sensor goes into the
  existing DEC-193 quarantine — logged once, surfaced on `/status` + `/poll` as
  `unavailable_sensors[]`, evicted from the live set, and **un-quarantined
  automatically as soon as it reads sanely again**. A transient glitch therefore
  costs nothing, and a persistently faulty sensor becomes visible instead of
  silently deafening. If it was the only CPU sensor, the already-adjudicated
  absent-sensor path applies its 40% floor, which is recoverable; the old
  behaviour was not. Triggered by any `temp*_input` returning garbage — a
  misprobed it87/nct6775, or a post-resume `k10temp` glitch. (DEC-288)

  **Scope of the fix, stated precisely:** this covers garbage *outside*
  [-50, 250] °C. A faulty reading that lands *inside* [105, 250] °C — a saturated
  8-bit thermistor reporting 127 °C, say — is indistinguishable from a genuine
  over-temperature and still latches the emergency indefinitely. Widening the
  bound is not the answer, because 105-125 °C are legitimate readings. That
  residual is recorded, not fixed here.

  Two consequences worth knowing. A latched emergency whose **sole** CPU sensor
  then breaks now falls from 100% to the 40% no-sensor floor after roughly seven
  seconds, rather than holding 100% forever — the daemon can no longer confirm
  the emergency is live, and DEC-190 already chose the safe floor for exactly
  that blindness. And a sensor that glitches *intermittently* (never five
  consecutive failures) no longer logs at all, because the per-read warning was
  removed; it will not appear in `unavailable_sensors[]` either.
- Removed a 1 Hz journal warning in the same path: the implausible-value log ran
  on **every** read of a faulty sensor. The DEC-193 tracker now owns that
  logging, once per quarantine transition — which is the spam it was built to
  collapse.

- **A fan write wedged in a kernel driver froze the entire profile engine — and
  with it the 105 °C thermal emergency.** The engine awaited every backend's
  blocking write without a bound, so a sysfs write that blocked in the driver
  meant the tick never finished, the loop never came round again, and
  `force_all` never ran for *any* backend — not just the stuck one. Nothing
  recovered it: DEC-266's supervision fires when the engine task **dies**, and a
  wedged task is very much alive, so the daemon sat there indefinitely holding
  the fans at whatever duty they happened to have.

  Each backend's join is now bounded to one tick, so one stuck device can no
  longer hold the loop: thermal safety and the other backends keep running. The
  wedged write is **not** abandoned — its handle is held and re-awaited on the
  next tick, never re-issued. That detail is the fix's load-bearing half:
  `spawn_blocking` work cannot be cancelled, so retrying each second would strand
  one blocking thread per tick and exhaust tokio's pool in about eight minutes,
  starving the very writer this protects.

  This mirrors the read side, bounded the same way in DEC-272; the write path was
  simply never swept. (DEC-289)

- **A hardware verify abandoned mid-flight left the fan header stuck at its test
  duty.** `POST /hwmon/{id}/verify` writes a deliberately different PWM, waits six
  seconds for the fan to settle, then restores the original. The wait was an
  `await`, and the restore sat after it — so if the client disconnected, or the
  GUI's own 12-second timeout fired first, the request future was dropped and the
  restore simply never ran. Both RAII guards released cleanly, which is why this
  looked safe; the *duty* had no such protection. For any header previously above
  50% the test value is **20%**, so a pump or fan could be left at 20% with
  nothing to put it back — permanently, whenever no active profile owned that
  header, because then nothing else ever writes it.

  The whole sequence — test write, settle, read-back, restore — now runs as a
  single uncancellable unit, with the lease and engine-pause guards moved inside
  it. A dropped request no longer stops any of it. Clients need no change; what
  changed is that the old behaviour was unsafe to rely on. (DEC-290)

- **A verify caught by daemon shutdown no longer re-latches the header into
  manual mode.** Making the sequence uncancellable also made it survive the
  shutdown that used to cancel it, and the daemon's hardware restore is supposed
  to be the last writer. Left alone, a verify interrupted by SIGTERM would write
  its duty *after* the restore had handed the header back to firmware — and the
  PWM watchdog, seeing the restore's `pwm_enable=2`, would read it as a BIOS
  reclaim and re-assert manual mode, leaving the fan latched at a fixed duty with
  no writer left. The restore is now skipped once shutdown is signalled;
  `restore_failed` reports it, so a caller still knows the header was not put
  back. Firmware control is the safer end state. (DEC-290)

- **Every OpenFan rescan reset the attached controller, including the ones it
  refused.** The rescan cooldown (added so a client looping on a failing rescan
  could not hold Arduino-class boards in reset) compared the list of candidate
  serial ports — and building that list called `auto_detect_port`, which
  *opens* each candidate to identify it. Opening asserts DTR, which is the reset.
  So the board was already reset by the time the cooldown decided to refuse, and
  the cooldown rationed nothing. The handler's own comment asserted the opposite
  — "it does NOT open anything" — which is why this stood.

  Enumeration and identification are now separate. The cooldown compares ports
  listed by a libudev/sysfs scan that opens nothing; the single identifying probe
  happens afterwards, past the cooldown and the single-flight guard, where it
  always belonged. **A refused rescan now opens no `ttyACM`/`ttyUSB` candidate.**
  Stated that precisely on purpose: `available_ports()` still opens the devnode of
  any tty whose parent driver is `serial8250`, which the shipped unit blocks via
  `DeviceAllow`, so "touches no hardware at all" would be the same kind of
  over-broad claim that hid this defect in the first place. (DEC-291)
- **The cooldown is now evaluated before the already-connected no-op**, so a
  repeated probe meets it first rather than having two earlier branches step in
  front. Trade-off, stated plainly: a client that has just adopted a controller
  and asks again within the cooldown window gets `409` instead of the
  informative `already_connected` payload. The refusal message no longer claims
  the earlier probe "found nothing", because under this ordering that is not
  something it can know. (DEC-291)

- **`GET /diagnostics/hardware` reported the thermal thresholds from a different
  place than the daemon acts on.** `emergency_threshold_c` and
  `release_threshold_c` were bare literals in the response builder, independent of
  the `ThermalSafetyRule` that actually latches and releases the emergency — so
  moving the trip point would have left the daemon *reporting* 105 °C while
  *acting* on something else, with a compile-time assert still guarding the old
  value and the GUI rendering the stale number verbatim as "Limit: N °C".
  Latent, not live: every copy agreed. That is exactly why it would have been
  found the hard way.

  The trip point and release point now have one definition, in `constants.rs`
  (where the daemon's own architecture rule says constants live). The rule reads
  it, the API response reads it, and the compile-time assert that calibration
  aborts below the emergency reads it. **No threshold changed value.** (DEC-292)

### Fixed (tests)
- The three OpenFan-rescan integration tests were **non-deterministic on any
  machine with real serial hardware** — measured 7 of 10 runs failing. Both
  fixes above were needed: with only the reordering they still failed 1 in 10,
  and with only the enumeration split they failed every run. Now **10 of 10
  green**. They still open the port once on the paths that legitimately adopt a
  controller; only the refused paths are now hardware-free.

### Changed
- **`/status` gained two engine reasons**, because the fix above would otherwise
  have *hidden* the problem it fixes: with the loop no longer frozen, both engine
  timestamps advance normally and a wedged writer would present as a healthy
  engine. The `engine` subsystem now reports `warn` / "a backend write has not
  returned yet — it is still in flight" and, past the same wedged threshold
  DEC-259 derived, `crit` / "writes wedged — a backend write has not returned
  and nothing is reaching those fans". No new field and no shape change —
  `reason` is free text and always has been. (DEC-289)

- The engine write-pause is now held for the remainder of a verify's settle even
  if the caller disconnects, rather than releasing early — the direct consequence
  of making the sequence uncancellable. **The 105 °C emergency is unaffected:**
  `force_all` runs before the pause gate and always has.

- The integration test for that endpoint asserted the reported values against
  literals, which pinned the numbers but not the link. It now asserts the
  reported values equal what a `ThermalSafetyRule` actually acts on, so the two
  cannot drift apart again — with the literal check kept alongside as a
  deliberate tripwire, so the trip point still cannot be moved silently.

### Known limitation
- The **GPU** backend's write join is still unbounded, so a wedge there can still
  hold the loop. Its blocking task carries an owned lock guard, so bounding it
  needs a different design (`try_lock` semantics that also touch the GPU verify
  path) and it was deliberately left out of a `[SAFETY]` change about blocking
  joins rather than rushed. Tracked as `AUD-a2`.

## [2.23.0] — 2026-08-27

### Added
- **A profile search directory can now be removed, not only added.**
  `POST /config/profile-search-dirs` accepts `{"remove": [...]}` alongside
  `{"add": [...]}`; at least one is required. The endpoint was add-only and
  merge-only, so the list could only grow — a client that re-registered a moved
  profiles directory left the old entry behind permanently, invisible to any UI
  and removable only by hand-editing a root-owned `runtime.toml`. Removals are
  applied **before** additions, so `{"add": [new], "remove": [old]}` is a single
  atomic "move"; a path named in both ends up present, because losing a
  directory the caller still wants is the worse outcome. Advertised as
  `control.profile_search_dir_remove` in `GET /capabilities` — **check the flag,
  do not probe**: an older daemon does not `404` a `remove`, it parses only
  `add` and silently ignores the rest, so an ungated call reports success having
  pruned nothing.
- Three edits are refused outright, all `400 validation_error`: removing
  `/etc/control-ofc/profiles` (it holds the admin-installed profiles); any edit
  whose result would be an **empty** search path — `activate_profile` resolves
  against that list, so emptying it is an unrecoverable soft-lock reachable from
  an unprivileged call; and any edit whose result no longer contains the
  daemon's **profile store of record**. That last one matters more than it
  looks: the store of record is *defined* as the first search dir, and it is the
  write target for profile create and delete, so dropping it would silently
  redirect every profile write for the rest of the process's life. It is
  asserted on the result rather than on the request, so removing and re-adding
  it in one call is still fine.
- Removal is peer-uid confined like addition (DEC-205), but by a predicate that
  does **not** require the directory to still exist. Reusing the add predicate
  would have refused exactly the entries this feature exists to clean up: a
  stale search dir is usually stale *because* the directory is gone. It accepts
  the raw path, falling back to its canonical form when that resolves — the add
  path validates canonically but persists the raw string, so without both legs a
  directory added through a symlink was storable and permanently unremovable.

### Fixed
- **A home directory of `/` confined nothing.** `path_is_within` is
  component-wise and every absolute path starts with the root component, so `/`
  as a confinement root accepted anything — and 26 accounts on a stock Arch
  install have `/` as their home (`nobody`, `http`, `cups`, `dbus`, `polkitd`,
  `qemu`, `git`…) while the socket is 0666. Pre-existing in the DEC-205 add
  path; `remove` would have made it destructive. `/` and `/nonexistent` are now
  treated as unresolvable and fail closed, in both predicates and in the
  resolver.
- A `remove`-only request from a caller whose uid or home cannot be resolved no
  longer reports "refusing to **add** profile search directories".

### Changed
- `POST /config/startup-delay` now answers with the shared DEC-243 setter shape
  (`key` / `value`) alongside the original `delay_secs`, so one client-side
  parser covers every `POST /config/*`. It was the one config route that
  predated that shape, which is why the GUI could not drive it through the same
  guarded write path as its siblings and kept a local mirror instead.
- `POST /config/profile-search-dirs` now rejects a wrong-shaped payload instead
  of silently dropping it: a non-array, or an array holding a non-string, is
  `400 validation_error`. Previously `{"add": [null]}` was indistinguishable
  from `{"add": []}` and reported success having applied nothing.
- `GET /config`'s key set and per-key mutability are now pinned by a test, so a
  new daemon config key cannot arrive unnoticed by the GUI. That gap is what let
  `profiles.search_dirs` sit writable-but-unsurfaced since the endpoint shipped.

Pairs with `control-ofc-gui` ≥ v2.23.0; **no wire, schema or API break** —
`api_version` stays 1 and every change is additive. `remove` on
`POST /config/profile-search-dirs` is advertised as
`control.profile_search_dir_remove` in `GET /capabilities`; v2.49.0 or newer
drives it from the Settings ▸ Daemon Configuration card, and older GUIs simply
never send it.

## [2.22.0] — 2026-08-23

### Added
- **The status the GUI polls now reports what speed each control is actually
  asking for.** Until now it reported which controls had stopped working, but
  never what the working ones were doing — so a live Controls card had no way to
  show a running figure and simply showed a dash forever. The daemon now
  publishes each control's applied output once per tick, whether that came from
  its curve or from a manual override, because the question a card is trying to
  answer is "what are the fans doing?" and an override is just as real an answer
  as a curve.
  Read it as a *control-wide* figure, not a per-fan one: an individual fan can
  sit above it on a safety floor, or below it on a graphics card that diverges,
  and each fan already reports its own commanded duty separately. A control that
  is not being evaluated is simply absent from the list rather than reported as
  zero — including for the whole of a thermal emergency, which drives the fans
  directly and bypasses controls entirely. Older clients are unaffected: the
  field is omitted when empty, so nothing about the existing shape changes. 277-k.
- **A control nothing can drive now shows up in the daemon's overall health.**
  A live engine ticking perfectly over a control whose curve cannot be resolved
  used to report as a completely healthy daemon, while the fans on that control
  sat holding their last speed. The only signs were one line in the log and a
  chip on one page — and that chip is hidden whenever a manual or external
  override is showing.
  Health now carries a `controls` entry that goes to *warn* while any control is
  unresolvable, and says how many and for how long. Deliberately **warn and not
  critical**: those fans are not stopped, and the 105 °C emergency does not go
  through controls at all, so it still reaches every OpenFan channel and writable
  motherboard header regardless. Critical stays reserved for a subsystem that has
  genuinely failed.
  Note this is louder than the equivalent treatment of unreadable *sensors*,
  which deliberately does not affect overall health. That difference is
  intentional: an unreadable sensor is a cause, is often harmless (a WiFi radio
  that is switched off), and frequently drives nothing — whereas a skipped
  control is the consequence, and always means a real fan is uncommanded. 277-j.

### Fixed
- **A graphics card that stops responding could still hang shutdown — the case
  the last release named and left open.** v2.21.1 put a time limit on handing
  the motherboard fan headers back to firmware, and said plainly that a graphics
  card whose fan-curve write wedged the same way could still hold shutdown open
  because that step runs first. This release closes it.
  Both steps now share one time limit instead of one of them having a
  hand-rolled limit and the other having none. That distinction mattered more
  than it sounds: bounding only the *second* of two steps achieves nothing at all
  while the first one can block forever ahead of it, which is exactly the shape
  that shipped. A wedged machine now costs about twenty-three seconds to stop,
  rather than never returning.
  The honest limits are unchanged and are not quietly upgraded here: stopping is
  guaranteed, **restoring the hardware is not**. If a chip or card has genuinely
  stopped accepting writes then nothing can restore it, and those fans hold their
  last speed until something takes them over again. 278-c.
- **The same unbounded write could also hang the daemon while it was crashing.**
  When a fatal error takes the daemon down, it tries to hand the fans back to
  firmware before it goes. Those writes had no time limit either, so a chip that
  had stopped responding could park the daemon mid-crash: a process that neither
  controlled the fans nor finished dying, which is the worse of the two outcomes
  because systemd can restart a daemon that has actually exited. It now gives up
  after three seconds and exits anyway. 278-a.
- **A control that stopped working no longer goes quiet during a thermal
  emergency.** The list of controls the daemon cannot resolve was being wiped for
  the entire duration of a 105 °C event, and stayed empty for a further three
  seconds after recovery — so the one place that says "nothing is commanding
  these fans" went silent exactly when someone was most likely to be looking at
  it, and the chip in the GUI blinked off and back.
  The cause was that the emergency reused the same reset as switching profiles,
  which correctly forgets that list because the next profile's controls are
  different. An emergency is not a profile switch: it says nothing about whether
  a curve resolves. The list now holds steady across the event.
  Worth knowing how to read it during one: a control listed here means *its curve
  cannot be resolved*, not *this fan is stopped*. The emergency drives OpenFan
  channels and writable motherboard headers directly — graphics-card fans are
  excluded from it by design — so a graphics-bound control with a broken curve
  genuinely is uncommanded throughout, which is precisely the case that must not
  go silent. 277-i.
- **Repeatedly asking the daemon to re-scan for an OpenFan controller now has to
  wait between attempts — unless something actually changed.** Two scans could
  never run at once, but nothing stopped a client firing them back to back forever,
  and each scan asserts DTR on every candidate serial port, which *resets*
  Arduino-class boards. So a client looping on a failing scan was holding unrelated
  hardware in reset.
  Scans against the *same* set of ports are now spaced ten seconds apart. Plugging
  a controller in and scanning again straight away is **not** delayed: a new device
  means a new port, and that is the case this button exists for. A successful scan
  never meets the wait at all, because the request returns immediately once a
  controller is connected. If you do hit the wait, the message says how long is
  left. 10-e.

### Internal
- A poll loop for an OpenFan controller adopted *after* startup is now drained at
  shutdown along with the rest, instead of merely being told to stop. No
  behaviour change today — that loop only reads status and RPM, so there was no
  risk of a late write — but "the restore is the last thing to touch the
  hardware" was simply not established for a controller adopted this way, and any
  future write added there would have broken that silently. 277-c.
- The release workflow built its cross-repo notification with a shell here-doc
  that expanded the tag before sending it. A tag containing shell substitution
  would have been executed, in a job holding the publishing token. Pushing a tag
  already requires write access, so this was not an escalation — but it was a
  shell-injection sink next to a credential. It is now built with `jq`, which
  also escapes the value properly. 277-p.
- Re-checked whether the IT8689E chip should start reporting as mainline-supported
  now that kernel 7.1 carries fan control for it. **Still no, and the schedule
  moved further out rather than closer**: the 6.12 and 6.18 long-term kernels were
  both extended to December 2028, and 6.12 is what Debian 13 and RHEL 10 ship, so
  "7.1 or newer is the common baseline" cannot become true before then. Saying
  "mainline: yes" today would steer people off the DKMS driver they still need.
  Next re-check scheduled for 2027-08, so this stops being re-derived at every
  audit. 10-a.

## [2.21.1] — 2026-08-22

### Fixed
- **Stopping the daemon could still hang indefinitely if a fan-control chip
  stopped responding mid-write.** The previous release bounded how long shutdown
  waits for a stuck temperature *read*, and named the remaining case in its own
  notes: handing the fans back to firmware waits on the fan-control chip, so a
  chip that wedges while being *written* to could hold the daemon open past every
  limit. That is the case this release closes.
  Handing the fans back now has a time limit of its own — on both halves of the
  job. The daemon gives the stuck chip three seconds to let it read which fan
  headers exist, and another three seconds for the hand-back itself, then stops
  waiting and moves on regardless. A motherboard chip that wedges now costs about
  twenty seconds, instead of never returning at all.
  **What that does and does not promise, stated plainly.** Two limits, both
  deliberate. First, the hand-back is not guaranteed to succeed: if the chip has
  stopped responding to writes, nothing can restore it, and those fans hold their
  last speed until something takes them over again — what changes is that the
  daemon no longer waits forever for it. Second, this covers the **motherboard**
  fan headers. A graphics card whose fan-curve write wedges the same way can still
  hold shutdown open, because that hand-back runs first and is not yet bounded.
  That is a narrower, rarer path than the one fixed here, it is tracked, and it
  will be closed on its own rather than bolted onto a safety fix at the last
  minute.
  **This mattered most where nothing else could rescue it.** When the daemon
  restarts itself after an internal failure, systemd runs no stop job — so the
  30-second cap and the backup restore script added last release do not apply.
  On that path a wedged hand-back meant the daemon stayed alive with nothing
  driving the fans and no way out. Fans held their last speed throughout, so this
  was never a case of fans stopping; the risk was that they stayed pinned where
  the failed run left them, with no thermal management, until the machine was
  power-cycled.
  One narrow case remains and is not claimed as fixed: if the stuck write is
  itself the one that switches a header into manual mode, it can land after the
  hand-back and re-latch that single header. 277-b.

## [2.21.0] — 2026-08-22

### Added
- **A fan that has silently stopped being controlled now says so.** If a control
  cannot work out what speed to ask for, it asks for nothing and its fans hold
  their last speed. That is the right thing to do — going blind must never make
  a fan slower — and for a passing cause it fixes itself within a second.
  What was missing is the cause that never fixes itself: a combined curve
  pointing at a curve you have since deleted, or a mirrored control whose target
  is itself not running. That fan stops responding for good, and until now the
  daemon said nothing at all about it: nothing in the log at its normal setting,
  and nothing on the API. The only symptom was a fan that no longer changed
  speed.
  Such a control is now named in the log once, and listed on the status the GUI
  polls, with the reason and how long it has been that way. It is reported after
  three seconds rather than instantly, so a sensor hovering on the edge of its
  freshness limit cannot fill the log. Nothing about fan control changes — this
  only makes an existing silence audible. 273-i.

### Fixed
- **Stopping the daemon could hang for a minute and a half.** When a temperature
  chip stops responding, the daemon deliberately leaves the outstanding read
  alone rather than piling more on top of it — that is what keeps a wedged chip
  from starving fan control. The cost was at shutdown: the runtime waited for
  that read to finish before letting the process exit, and it never would. Under
  systemd that meant `systemctl stop`, a reboot, or a package upgrade sat for
  about 90 seconds before the daemon was killed outright; started from a
  terminal, Ctrl-C hung with nothing to break the wait at all.
  Shutdown is now bounded: the daemon gives any outstanding read two seconds and
  then exits regardless. **Fan control was never at risk** — the hardware is
  already handed back to firmware before this point, and a restart-forcing exit
  never took this path — so what changes is only how long the machine waits.
  The packaged service file also caps systemd's own patience at 30 seconds
  instead of its 90-second default, as a backstop rather than the mechanism —
  chosen to stay clear of the roughly 14 seconds an ordinary stop can take on a
  loaded machine, so that hitting the cap remains a signal that something is
  actually wrong.
  One older case is not covered by either, and is not new here: the hand-back
  itself waits on the fan-control chip, so a chip that wedges *mid-write* can
  hold the daemon open past every limit above — and on the restart path there is
  no systemd stop job to cut that short. Fans hold their last speed throughout.
  273-b.

### Changed
- **The "sensor not found" warning now describes what actually happens.** Saving
  or importing a profile that names a sensor your machine does not have is
  allowed — moving a profile between machines is expected — and it warns. The
  warning said the control would "hold a safe fallback until it appears", which
  has not been true of either case since 2.20.1 and was never quite true of one
  of them. A curve using the missing sensor on its own does not command its fans
  at all: they hold their last speed. A combined (Mix) curve keeps running on
  the inputs it does have. The warning now says both, because a warning you
  cannot act on correctly is worse than no warning. 273-h.

### Internal
- **A crash while shutting down no longer reintroduces the hang.** The bound
  added above covered the normal path only; a panic on the way out unwound
  straight past it and went back to waiting forever — restoring the problem
  precisely when something had already gone wrong. It is now bounded on both
  paths.
- **The "not being controlled" list can no longer be read half-written.** It was
  cleared at the start of each cycle and refilled at the end, so a client asking
  in between was told nothing was wrong. The daemon and the GUI both work on
  one-second cycles that drift through each other, so that gap would be hit
  periodically rather than never — the warning would blink off for a moment at a
  time. It is now published once per cycle, as a single value.
- **The exit that recovers from a dead fan-control engine is now tested.** If
  the engine dies, the daemon restores the hardware and then exits non-zero so
  systemd restarts it with a working engine — and that "and then" is the whole
  point: exiting first would leave fans latched at whatever the dead engine last
  set. Neither half was pinned by a test, because a process exit cannot be
  observed from inside the process that performs it. Deleting the exit, or
  moving it ahead of the hardware restore, left every test passing. Both are now
  checked by running the real shutdown in a separate process and reading its
  exit code and its effect on the hardware. 273-a.
- A rejected publishing token now says so. When the token that tells the package
  repository about a new release expires, the release itself still succeeds and
  looks complete — only publication silently stops, and the next day's check
  reports it as "the repository is stale", which points at the wrong thing
  entirely. That misdiagnosis has already cost two releases. The failure is now
  named, along with the fact that no new tag is needed to recover. OPEN-07b.

Pairs with `control-ofc-gui` ≥ v2.23.0; **no wire, schema or API break** —
`api_version` stays 1 and `skipped_controls[]` is purely additive, so existing
GUIs need no upgrade. v2.44.0 or newer renders it as a "Not controlled" badge.

## [2.20.1] — 2026-08-21

### Fixed
- **A fan could stop responding to a hot CPU when a different sensor went
  quiet.** 2.20.0 stopped a combined (Mix) curve from quietly *lowering* its fan
  speed while one of its inputs was unavailable — but it did so by switching the
  control off entirely for as long as that input was missing. That is the right
  answer only when the missing input was the one asking for speed. When the
  *surviving* input is the hot one it is exactly wrong: a live CPU reading of
  95 °C sat there while the fan stayed at its old speed, waiting for the 105 °C
  emergency. Reaching it needed nothing unusual — one GPU or coolant sensor
  falling behind, being dropped after a missed read, or being quarantined, is
  enough, and a quarantine can last indefinitely.
  A combined curve now runs on the inputs it still has, and is separately
  forbidden to command *less* than it already was until they are all back. Both
  halves are needed: the first lets a hot survivor drive the fan, the second is
  what stops the speed falling while the daemon cannot see. Curves whose inputs
  all disappear still hold, as before. DEC-272.
- **A combined curve naming a sensor your machine does not have went silent.**
  Same cause. Profiles are allowed to name sensors that are not present — moving
  one between machines is expected, and an unknown sensor is a warning rather
  than an error — but in 2.20.0 such a control was never commanded at all, where
  before it ran on the sensors that do exist. It does again. DEC-272.
- **One unreadable chip could switch off sensor cleanup for the whole session.**
  2.20.0 stopped trusting a hardware scan that could not read some chip, so that
  a momentary failure would not be mistaken for the chip being removed. But it
  distrusted the *whole* scan, and a chip that fails to enumerate contributes
  nothing that would trigger a re-scan — so with a persistently unreadable chip
  present, readings for genuinely removed sensors were never cleaned up again,
  which is the condition the missing-sensor safety fallback needs in order to
  fire. The distinction is now drawn per chip: the unreadable chip's readings are
  protected, everything else is cleaned up as normal. DEC-272.
- **A sensor could be renamed by a failed label read.** A temperature sensor's
  label is not decoration — it forms part of the sensor's identity and decides
  whether it counts as a CPU sensor. If the label file existed but would not
  read, it silently became empty, which renamed the sensor and, on some
  motherboard chips, reclassified a CPU sensor as a generic one. The scan looked
  clean, so the old reading was then discarded as though the sensor had gone.
  A label that will not read now fails its chip for that scan, which protects its
  readings instead of replacing them. DEC-272.
- **`systemctl stop` could take an unpredictable time to finish** in the OpenFan
  polling path, for the same reason it could in the sensor path — a stop request
  and a due poll arriving together were chosen between at random. Stop requests
  now always win in both. DEC-272.

### Internal
- Continuous integration now builds and tests against the committed lockfile. It
  did not, and cargo quietly regenerates that file when it is out of date, so a
  version bump that forgot to update it passed every check here and failed only
  in the packaging build — after the release tag was already public. A test that
  tried to catch the same thing by reading the lockfile could never have worked:
  running it was what repaired the file it was inspecting. DEC-272.
- The release changelog check now also rejects an empty section, which fails the
  release the same way a missing one does, and equally late. DEC-272.

## [2.20.0] — 2026-08-21

### Fixed
- **A wedged sensor read froze the whole feed behind a ticking engine.** The
  poll task is supervised, but supervision fires on a task *dying*, and a
  blocking sysfs or NVIDIA-driver read that never returns leaves the task alive —
  so nothing fired and the daemon went on controlling fans from readings that
  could no longer change. `/status` was not silent about it: subsystem staleness
  still crossed to `warn` and then `crit` as the readings aged. What stayed
  reassuring was the engine heartbeat, which measures the control loop rather
  than the feed underneath it, so the one indicator that looked healthiest was
  the one least able to see the fault. The blocking read is now bounded by the same freshness
  budget the safety ladder uses, past which a still-running read cannot produce a
  value that rule would act on anyway. Crucially the loop does not start a second
  read behind a stuck one: a blocking read cannot be cancelled, so one per tick
  would exhaust the thread pool in minutes and starve every other blocking
  operation in the process — including the fan writes. DEC-272.
- **A sensor that disappeared was never forgotten.** Cached readings were only
  ever added, so a sensor that vanished — a driver unloaded, hardware removed —
  left its last temperature in place for as long as the daemon ran, ageing into
  "stale" but never "gone". That kept the no-sensor safety branch from reaching
  the very case it was written for. Vanished sensors are now evicted; ones that
  are merely unreadable keep their existing quarantine path. DEC-272.
- **Fan curves kept running on frozen sensors.** Only the CPU safety rule checked
  whether a reading was still current. A frozen GPU or coolant sensor drove its
  curve indefinitely with the system reporting normal — the same silent failure
  already fixed for the CPU, one level out, on sensors with no thermal rule of
  their own to catch it. A curve whose sensor has stopped updating now holds its
  fans at their last speed instead of tracking a temperature that is no longer
  real. CPU sensors are deliberately unaffected: the thermal ladder already
  decides what a stale CPU reading means, and taking that over would have frozen
  a fan mid-ramp instead of letting it keep climbing. DEC-272.

- **A frozen sensor could make a Mix curve command LESS cooling, not hold.** The
  freshness rule above drops a stale sensor out of curve evaluation, and for an
  ordinary curve that skips the control so its fans hold. A Mix curve instead
  dropped just the unresolvable input and recombined whatever was left, so
  `max(CPU, GPU)` with a frozen GPU sensor quietly became `max(CPU)` — and a fan
  running at 100% for a GPU last seen at 85 °C fell to 36% in a single tick, with
  nothing to damp it. `subtract` was stranger still: losing the first input
  promoted the second to take its place, so the result could jump instead of
  fall. A Mix now holds if *any* of its inputs is unavailable, which is what the
  rest of the freshness work already promised. DEC-272.
- **A sensor that could not be read was mistaken for one that had been removed.**
  Discovery skips a chip whose own identity file will not read, so one bad chip
  cannot hide every other one — but it then reported the shortened list as if it
  were the whole truth, and the new vanished-sensor eviction believed it. A
  single transient failure on the CPU chip therefore deleted a live CPU
  temperature, which reads as *gone* rather than *stale* and so bypasses the rule
  that holds a thermal emergency's fans up while a reading ages. A latched 105 °C
  emergency dropped from 100% to the 40% no-sensor floor and back on the next
  successful scan. Eviction is now suspended until a scan completes cleanly; a
  genuinely removed chip leaves no directory behind, is never "skipped", and is
  still evicted at once. DEC-272.
- **A wedged sensor read made shutdown take an unpredictable amount of time.**
  Once a stuck read's budget elapsed, the loop had a due tick and a pending stop
  request ready at the same moment and chose between them at random, so
  `systemctl stop` was observed taking 4.5 s, 9.5 s or longer with no upper
  bound — the window in which systemd gives up and kills the process outright,
  leaving the fans wherever they were. A stop request is now always taken first.
  DEC-272.

### Internal
- The main loop's shutdown decision — including whether a failure warrants a
  restart — is extracted and covered by tests. Every safety behaviour built on it
  since 2.18, when engine supervision landed, was previously verified only by
  reading the code. DEC-272.

## [2.19.0] — 2026-08-20

### Fixed
- **A poll interval the safety rule cannot supervise disabled the 105 °C ladder.**
  DEC-269 capped the CPU staleness budget at 30 s so a hand-edited
  `poll_interval_ms` could not buy the emergency rule a five-hour trust window.
  Past a 6 s cadence, though, the cap stopped the budget tracking the poll
  interval at all: the 5x headroom eroded towards 1x, so ordinary readings began
  to look stale, and beyond the 30 s ceiling it inverted outright — every reading
  stale the moment it landed, the ladder (which only runs on a fresh reading)
  never firing, and every fan pinned at the 40% no-sensor floor while `/status`
  reported a healthy ticking engine. Both directions silently disable thermal
  protection. The budget now floors at one poll period, and the effective
  interval is clamped to the slowest cadence the rule can actually supervise —
  derived from the two constants rather than written down twice. Clamped
  with a warning rather than rejected: a fan controller that will not boot over
  a config typo is worse than one polling faster than it was told. Only the
  hand-edited admin config file could reach this (there is no CLI flag for it);
  the API's 250–2000 ms range is unaffected. DEC-270.
- **Going blind must never reduce cooling — including when nothing is latched.**
  The rule above was written for output the safety ladder was *already* forcing,
  which left the commonest case out. With no emergency active, a wedged sensor
  read at 104 °C dropped every fan from a curve output of ~85% to the 40%
  no-sensor floor — a reduction in cooling caused purely by losing sight, and a
  plausible route *to* 105 °C, at which point the emergency cannot fire because
  the daemon is blind. While the last known temperature was at or above the
  release threshold the fallback is now suppressed entirely and fan curves keep
  running on that reading, exactly as they did before any of this existed. The
  40% floor still applies once the last thing the daemon knew was that the
  system was cool, which is the case it was written for. DEC-269.
- **`thermal_state` could report one duty while the daemon forced another.** Past
  the five-cycle debounce, a stale reading during recovery reported
  "no_sensor_fallback" — which means 40% — while actually holding the 60%
  recovery floor. The same ordering that decides the duty now decides the label,
  so the two cannot disagree. DEC-269.
- **The safety log stated a percentage it was not forcing.** It hardcoded "40%"
  at the moment the debounce tripped, which became false as soon as anything
  could outrank the fallback — at exactly the moment, a live emergency going
  blind, when an operator most needs the log to be true. It is now written from
  the decision rather than from the branch that proposed it, and names whether
  the sensor is missing or merely no longer updating. DEC-269.
- **A restart could be silently lost when two tasks died together.** The
  both-causes reporting added alongside the supervision work was gated behind
  the restart flag, so if the IPC server's death won the race while the profile
  engine had also died, the engine's death went unlogged *and* the process
  exited cleanly — leaving systemd with no reason to restart it. Both causes are
  now checked unconditionally and either one demands the restart. DEC-269.
- **Losing sight of a CPU sensor could *reduce* cooling mid-emergency.** The
  freshness filter below treats a stale reading as no reading — which is right
  for deciding whether to act, and wrong for deciding what to do while a 105 °C
  emergency is already latched. A single poll leg overrunning the budget, with
  the task still alive so nothing restarts, dropped every fan from 100% to the
  40% no-sensor floor on a CPU last measured at 95 °C. It also flapped: at a
  ~5 s leg against a 1 Hz engine the reading crosses the budget on alternate
  ticks, oscillating 100/40/100 during a thermal emergency.

  The cause was collapsing two different things into one. A vanished sensor is
  evidence of nothing, and 40% was chosen for it deliberately; a six-second-old
  reading of 95 °C is strong evidence the machine is still hot. The reading is
  now classified three ways rather than two, and a stale one **holds whatever
  the rule is already forcing** — 100% latched, 60% mid-recovery — while still
  being barred from driving the rule's state machine, so it cannot clear a latch
  however cool it reads. A sensor that genuinely vanishes mid-emergency still
  forces 40%, exactly as before. The invariant, now stated in the code and
  pinned by a test over the whole matrix: losing sight of a sensor must never
  lower an already-forced safety output. DEC-269.
- **`cpu_sensor_found` could contradict the state printed beside it.** It was
  still answered from the raw sensor list while the state beside it came from
  the age-filtered rule, so `{"state": "no_sensor_fallback", "cpu_sensor_found":
  true}` became reachable — and the GUI renders both on one line. It now applies
  the same freshness budget the rule applies. DEC-269.
- **The staleness budget is now bounded.** `poll_interval_ms` is validated only
  as `>= 100`; the 250–2000 ms clamp lives on the API route, not the config file.
  An admin typo of `3600000` would otherwise have handed the 105 °C rule a
  five-hour budget, silently disabling it. Capped at 30 s regardless, and the
  multiply saturates rather than wrapping — a wrapped budget would have been the
  worst possible direction, marking everything permanently stale. DEC-269.
- **The 105 °C rule could be reading a temperature that had stopped changing.**
  It takes its input from a cached sensor map that carries no freshness filter,
  so if the hwmon poll task died the last reading was returned forever. Every
  consequence of that was silent: the emergency could never trigger, because the
  number it watches could no longer rise; the no-CPU-sensor fallback could never
  engage either, because the sensor was not *missing*, only frozen; and the
  engine's liveness heartbeat stayed green throughout, because the engine really
  was ticking — on stale data. A reading older than five poll intervals is now
  treated as no reading at all, which routes it into the no-sensor handling the
  daemon already had and already tested. The budget follows the configured poll
  interval rather than being a fixed number, because that interval has no upper
  bound and a fixed one would mark a legitimately slow system permanently stale
  and pin its fans at the fallback speed. DEC-267.
- **The hwmon poll task is now supervised, like the profile engine.** Making a
  frozen feed safe and visible is not the same as making it recoverable: without
  this the daemon would sit at the fallback fan speed indefinitely with no path
  back. It is the only writer of the sensor map the thermal rule reads, so its
  death is now treated the same way the engine's is — restore every fan to
  firmware control, then exit so systemd restarts the daemon with a live feed.
  DEC-267.

### Changed
- **A coordinated release no longer guesses how long to wait for its pair.**
  `pacman-repo` assembles from whichever Release of each project is latest when
  it runs, so in a joint GUI + daemon release the two `release.yml` runs finishing
  minutes apart could publish this package against the other's *previous* version
  and serve that pair as current. v2.18.0 handled it with a blind `sleep 180`,
  which on 2026-08-17 was not close to enough — the two runs finished 9m29s apart
  — and on a solo release was three idle minutes for nothing. The wait now polls
  for the actual condition: whether the GUI repository has published its Release
  yet, measured on its `GitHub Release` job. Solo releases dispatch immediately;
  paired ones wait only as long as they must. Any API problem falls back to the
  old fixed settle, so this can never turn a complete release red. DEC-270.

## [2.18.0] — 2026-08-17

### Added
- **An OpenFanController can now be adopted without restarting the daemon.** The
  daemon looked for one only while it was starting up, and stored the result
  somewhere nothing could later change. A controller that enumerated a moment too
  late — or that failed its identity handshake once on a particular boot — was
  therefore invisible for the rest of the daemon's life, with a warning in the
  journal as the only sign. That cost more than fan control: the 105 °C thermal
  emergency drives OpenFan fans through that same connection, so it silently had
  no path to them either, while the status endpoint went on reporting a healthy
  daemon. `POST /fans/openfan/rescan` looks again and installs what it finds, and
  the profile engine picks it up on its next tick — the engine is what actually
  writes, so a route that adopted a controller the engine never saw would have
  fixed nothing. Adoption uses the same identity-verified path as startup, so a
  port that opens but is not an OpenFanController is still refused. Advertised as
  `control.openfan_rescan`. DEC-265.

### Fixed
- **A GPU reporting a backwards range could kill fan control outright.** The fan
  curve's allowed temperature range is read from text the device supplies, and
  nothing checked that the low end was actually below the high end. A reversed
  line would reach a bounds function that treats that as a programming error and
  aborts — on the once-a-second write path, which is the only thing writing fan
  speeds at all. The range is now rejected as nonsense and the safe default used
  instead, and the bounds call no longer aborts even if one slips through. Same
  defect class as the one fixed in the previous release; this was its unswept
  sibling. DEC-265.
- **A panic that the runtime contained no longer resets every fan to automatic.**
  The safety net that hands fans back to firmware control fired on *any* panic on
  *any* thread — but a panicking background task is caught and the daemon carries
  on. One contained panic therefore dropped every GPU curve and motherboard fan to
  firmware defaults underneath a profile engine that was still running and put its
  curve back a second later, announcing that it was aborting while it did no such
  thing. Only a panic that genuinely ends the process now triggers the reset.
- **Three failures that were logged but not counted, or not logged at all.** A
  panicking fan-poll task did not count towards the failure total that triggers a
  reconnect, so the one fault that never fixes itself was also the one that could
  never prompt a retry. A panicking GPU write was indistinguishable in the log
  from a fan that merely refused the write. And if the shutdown signal's sender
  went away, the engine's wait returned instantly and forever — turning a
  once-a-second loop into one that spins a CPU core flat out, while the health
  endpoint reported it as perfectly on time, because it *was* ticking. DEC-265.
- **The profile engine is now supervised, so its death is no longer silent.**
  The panic-hook change above is right — a contained panic should not lurch every
  fan to firmware defaults — but it presumed something that was not true. The
  engine runs on a tokio worker thread, so a panic inside its own tick body is
  "contained" by construction: the task died, nothing polled its handle until
  shutdown, and the process stayed up answering `/status` while every hwmon
  header sat latched in manual at a frozen duty with the BIOS locked out, the GPU
  on a frozen curve, and no 105 °C emergency. `Restart=on-failure` never fired
  because nothing exited. That is strictly worse than the pre-existing behaviour
  the panic hook used to provide, and the bug class is live — two audits have
  each found a `clamp` panic on that exact path, including one fixed in this
  release. The engine now reports its own death, on a drop guard so an unwinding
  panic reports it too, and the daemon responds by running the same ordered
  restore a SIGTERM would and then exiting non-zero so systemd restarts it with a
  live engine. DEC-266.
- **A GPU reporting a negative or over-100% fan-speed bound is no longer
  trusted.** The inverted-range guard added above compared the two bounds as
  signed integers and *then* narrowed them to bytes, so the conversion could
  reintroduce the inversion the guard exists to reject: `-1% 100%` passed and
  became `(255, 100)`, and `0% 300%` became `(0, 44)`, silently capping every GPU
  fan at 44%. Bounds are now converted before they are compared, and a hotspot
  range wide enough to overflow the downstream subtraction is rejected as
  implausible. DEC-266.
- **A rescan whose HTTP request timed out no longer throws away the controller it
  found.** Probing and adoption ran inside the request handler, so a client that
  gave up — the GUI allowed 5 s, and a sweep across several unresponsive
  USB-serial devices takes longer — caused the handler's future to be dropped.
  The probe itself cannot be cancelled, so it ran to completion, identified a
  controller, and then discarded it, losing the very thermal-emergency leg the
  route exists to restore. It also released the single-flight flag early, so a
  retry raced the still-running orphan for the same port. Probe, adoption and
  flag release now live in a detached task; the request only waits for the
  answer. DEC-266.
- **Two rescans racing could each install a controller.** The "one is already
  connected" check and the single-flight claim are two adjacent statements, and
  on a multi-threaded runtime two requests on different threads can still
  interleave between them: the loser probes for seconds, then installs its
  controller over the winner's. The engine only looks for a controller while it
  has none, so it would have gone on writing through the first one while a poll
  loop read RPM from the second. The install is now made under the same lock
  that checks, so the second probe is discarded and answers as the idempotent
  repeat it is. DEC-266.
- **A rescan no longer reports "nothing found" while still holding the lock that
  says one is running**, which made an immediate retry fail as a conflict with
  the attempt that had just finished. DEC-266.

- **A release can no longer be published from a commit whose tests failed.** The
  GUI's v2.41.0 shipped that way; this repo has the identical structure and is
  fixed alongside it rather than waiting for its turn. The publish workflow gated
  on a clean-room package build — proof that the package *assembles* — and had
  never looked at the test suite at all, so the two gates were unrelated and a
  red suite could not stop a Release. The tag push and the test run happen in
  different workflows, so there is nothing to depend on directly; the publish job
  now asks the Checks API for the tagged commit's own CI result and refuses to
  proceed unless it passed. It fails closed: a commit with *no* CI run is not
  treated as innocent, because that is exactly what tagging something never
  pushed to `main` looks like. DEC-263.
- **The cross-repo oracle check stops reporting drift that isn't there.** The
  guard compares this repo's copy of the shared parity fixtures against the GUI's,
  but it read the peer's `main` at the instant it ran — and the rule it enforces
  asks for coordinated changes to land daemon-first. This side of the pair
  therefore failed on every coordinated change *by construction*, on files that
  were already byte-identical, purely because the GUI's push had not landed yet.
  That is what happened at v2.17.0. It now re-reads the peer for a bounded window
  before calling it drift; a genuine drift is still reported, and a peer that is
  already up to date still passes immediately.

## [2.17.0] — 2026-08-10

### Changed
- **The cross-stack role-classification oracle now pins the classifier that
  actually decides the floor.** It checked `member_is_pump_or_cpu`, while the
  runtime floor and the stop-snap exemption are decided by the wider
  `member_needs_hard_floor` (DEC-252) — so the GUI and daemon could disagree
  about a renamed pump with nothing in CI noticing. Five vectors added, including
  a PCI-BDF id whose colons break naive label parsing. DEC-257.

### Fixed
- **A fan parked at 0% is no longer stranded by a reconnect.** The new
  write-generation invalidation cleared each channel's last-commanded value but
  not its stop clock — the one combination the 8 s stop-timeout rejects, and one
  the timeout's own note calls unreachable because "any non-zero write clears the
  timer; a repeat 0% coalesces". That loop is exactly the tracking-state write
  outside `set_pwm` the note guards against. A channel legitimately held at 0%
  therefore lost its coalesce on the first tick after a resume or reconnect, hit
  the expired timer, and failed — and since the write never landed, nothing
  changed, so it failed again every second forever. The fan sat at whatever duty
  the re-enumerated device powered on with, which is the precise failure the
  invalidation exists to prevent, while the daemon reported 0% and raised a link
  alert on healthy hardware. The stop clock is now reset with it.
- **`POST /gpu/{id}/fan/verify` now takes the GPU write lock.** The engine and
  `fan/reset` were serialised against each other, but verify — which writes a
  test curve and restores it, both multi-write PMFW commits — was not, so a reset
  arriving mid-verify could interleave. Verify holds the lock for its whole
  window rather than per-commit, because it sleeps between writing and reading
  back. `fan/reset` therefore now waits *boundedly* for the lock and returns a
  clear `409` instead of blocking past the GUI's 5 s timeout; the bound is what
  distinguishes the two callers it can collide with — wait out an engine tick,
  report a conflict for a verify.
- **The engine no longer reports itself dead while it is saving your hardware.**
  Liveness was a single timestamp taken at the start of each tick, so a *slow*
  tick and a *stopped* engine looked identical — and the daemon reported the
  worse of the two. A thermal `force_all` walks all ten OpenFan channels, each
  bounded by `serial.timeout_ms`, so a degraded-but-open serial link makes a
  legitimate tick take 5–10 s. In exactly that situation `/status` read
  `crit` — *"not ticking — fan control and thermal safety are stalled"* — while
  the engine was in the middle of driving the 105 °C emergency. The inverse of
  the truth, in the one state where it matters most, and self-repeating because a
  failed write does not advance the coalescing cache. The engine now stamps both
  the start and the completion of each tick: a tick in flight reports as busy
  (and says so), a tick that has genuinely stopped still reports `crit`, and a
  tick that never finishes escalates past 30× the period. Widening the threshold
  would have bought the same silence for a real death. DEC-259.
- **A fan can no longer be left at the firmware default after a reconnect.**
  OpenFan writes are coalesced when the value equals the last commanded one,
  which is only sound while that cache reflects the device — and nothing
  invalidated it when the device changed underneath. After a USB re-enumeration
  the poll loop swaps in a new transport, but the per-channel cache still
  described the old session, so every subsequent identical command was coalesced
  into silence: the fan sat wherever the firmware left it while the daemon
  reported the commanded value. The same gap existed across a system resume,
  where hwmon has always cleared its equivalent state. Both now bump a write
  generation the controller watches. Whether this firmware actually resets duty
  on re-enumeration is not determinable from the protocol, so this assumes it
  might; the cost when it did not is one redundant write per channel, once.
  DEC-256.
- **Resetting a GPU fan to automatic is now mutually exclusive with the engine's
  own writes, and cannot be undone by a later failure.** Review of the first
  attempt found three ways it still broke. A PMFW curve write is N point writes
  plus a commit and a reset is `"r"`+`"c"`, so with no lock between them they
  could interleave into a curve that is neither the profile's nor firmware-auto —
  a state nothing reconciles. A second reset that *failed* cleared the
  stand-off flag a first, **successful** reset owned, handing the fan back to the
  engine after the API had confirmed the reset — no concurrency needed, just two
  clicks. And because the flag was claimed in the handler rather than in the
  write task, a client disconnect (the GUI allows 5 s) dropped the request
  between claim and rollback, stranding the fan: relinquished, never reset, and
  skipped by the engine for the rest of the process's life. GPU writes now take a
  shared lock, the claim and its rollback both happen inside the uncancellable
  write task, and a rollback only undoes what that call actually claimed.
  DEC-255.
- **A configuration file that cannot be parsed is moved aside, not overwritten
  and not a dead end.** The previous release refused the write, which protected
  the file but left every `POST /config/*` returning 503 permanently — and the
  realistic trigger is not corruption but a **daemon downgrade**, since each
  config section rejects unknown keys. Settings were then simultaneously not
  applied and not settable, with no documented way out. The file is now renamed
  to `runtime.toml.invalid-<timestamp>` and the daemon carries on: the user's
  bytes survive verbatim, and the next setter works. DEC-255.
- **The serial reconnect path verifies identity too.** Auto-detection probes a
  port on its own file descriptor and then closes it; reconnect re-opened the
  path and adopted that second descriptor without re-checking. "Openability is
  not identity" (DEC-250) applies most on the path that runs continuously.
  DEC-255.
- **Resetting a GPU fan to automatic can no longer be undone by the engine's own
  in-flight write.** `POST /gpu/{id}/fan/reset` set its "engine, stand off" flag
  *after* writing firmware-auto, while the engine checks that flag on the async
  worker before dispatching its sysfs write to the blocking pool. An engine write
  already past that check could therefore land on top of the reset — and because
  the fan counted as relinquished by then, the engine skipped it on every later
  tick and never corrected it. The GPU stayed pinned on a stale flat curve until
  the next profile activation or a daemon restart. The flag is now claimed before
  the write, so it covers the whole reset, the engine re-checks it at the last
  moment before touching sysfs (mirroring the existing verify guard), and a reset
  that *fails* hands the fan back instead of stranding it. DEC-254.
- **A renamed pump keeps its 30 % floor.** The pump/CPU hard floor was decided
  entirely from `member_label`, which the client writes and the GUI fills from a
  display-name tier list — so renaming a `PUMP` header to something like
  "Radiator Top" dropped it to the ordinary 20 % floor and removed its
  stop-snap exemption, with nothing to catch it. The daemon had the real label
  the whole time: it is embedded in the member's own stable id
  (`hwmon:chip:device:pwmN:LABEL`). At eval time the floor now applies if
  *either* label says pump/CPU — a union, so the daemon's view can only ever add
  a floor, never remove one the profile asked for. **Limit worth knowing:** when
  a chip publishes no label file the daemon synthesises `pwmN`, and it reads no
  `/etc/sensors.d`, so on such a board this adds nothing and the author's label
  remains the only signal. `validate()`'s rejection is deliberately unchanged, so
  upgrading the daemon ahead of the GUI cannot start refusing profiles the GUI
  still bakes. DEC-252.
- **A configuration file that cannot be read is no longer overwritten.** Every
  `POST /config/*` is load → change one key → save, and the loader falls back to
  defaults when a file is malformed or unreadable. The write that followed did
  not merely ignore the bad file — it replaced **every other setting in it** with
  a default, permanently, behind a single journal warning. Setters now refuse
  with `503 persistence_failed` and leave the file untouched; the boot path still
  tolerates a corrupt file so the daemon always starts. DEC-252.
- **A serial port is no longer trusted just because it opened.** Startup accepted
  the first candidate `RealSerialTransport::open` succeeded on — and that
  succeeds on any readable tty, so a configured-but-wrong `/dev/ttyACM*` (a
  modem, an Arduino, a 3D printer) was adopted as the fan controller and the
  search stopped there, discarding the correctly auto-detected port sitting next
  in the candidate list. Because writes to an indifferent device return success,
  nothing ever surfaced: no failure was logged, `/status` reported OpenFan
  healthy, and the 105 °C emergency's `force_all` reported success while not one
  OpenFan-attached fan was being driven. `serial.port` is settable by any local
  user over the socket and persists in `runtime.toml`, so this survived reboots.
  A candidate must now answer the same `ReadAllRpm` handshake auto-detection
  uses; one that does not is skipped and the next candidate is tried. DEC-250.
- **The profile engine can no longer be killed by a profile it loaded itself.**
  The engine's step-rate limiter used `f64::clamp`, which panics when its bounds
  are inverted or non-finite. A `step_up_pct` / `step_down_pct` pair summing
  below zero inverted them, so the engine wrote once and then aborted on its
  next tick — taking the daemon's sole PWM writer *and* the 105 °C thermal rule
  down with it. Nothing supervises that task, so the process stayed up and
  `/status` kept answering 200 with a frozen `thermal_state`: fans ran on
  whatever value the last good tick left behind, indefinitely, with every
  health signal green. Reachable without the API — `validate()` bounds those
  fields, but the boot paths (CLI `--profile` and persisted-state restore)
  deliberately skip it, so a hand-edited or corrupt profile on disk was enough.
  Negative caps now read as "no movement in that direction" and the control
  holds its previous output instead. DEC-249.
- **Profiles loaded from disk are range-checked.** The load-time net already
  refused an oversized profile; it now also refuses out-of-range and non-finite
  numbers, mirroring `validate()`'s bounds exactly, so nothing the API would
  reject can reach the engine from disk either. Deliberately numeric-only —
  a profile referencing a sensor or header this machine does not currently have
  still loads, as before. DEC-249.

### Changed
- **Profile and configuration writes no longer run on the async worker threads** —
  now genuinely all of them. The first pass converted only five of the eight
  `POST /config/*` setters; profile-search-dirs, startup-delay and the two
  preferred-sensor endpoints still fsynced inline, so the stated invariant was
  not actually established. DEC-252/255.
  `write_atomic` does write + fsync + rename + a directory fsync, which was
  unbounded wall-clock time on the same runtime the 1 Hz profile engine — and so
  the 105 °C decision — is scheduled on. Moved to `spawn_blocking`, matching what
  the GPU and hardware-diagnostics handlers already do. The runtime is
  multi-threaded (one worker per core), so no single write could starve the
  engine on its own; this removes the coupling instead of leaving engine timing
  dependent on core count. DEC-252.

### Added
- **`engine` subsystem on `GET /status` and `GET /poll`** — profile-engine
  liveness alongside the existing `openfan` and `hwmon` freshness entries, and
  the missing half of the fix above: a stalled engine now escalates
  `overall_status` to `"crit"` instead of hiding behind fresh poll data. Judged
  against the engine's fixed 1 Hz tick rather than the configurable
  `poll_interval_ms`, so raising the poll interval cannot widen what counts as
  a live engine. Additive and appended to `subsystems[]` — `api_version` is
  unchanged and index-based readers are unaffected. DEC-249.

## [2.16.0] — 2026-08-07

**The daemon can now be asked what it is configured with.** `GET /capabilities`
carried devices, features and limits but nothing about configuration, and the
writable knobs were write-only — a client could set the startup delay but never
read it back, so it had to keep a local guess that could silently disagree.
Pairs with GUI ≥ v2.38.0; API version stays 1 and every change is additive.
DEC-243.

### Added
- **`GET /config`** — the effective merged configuration. Each key reports its
  on-disk `value` (what a restart would produce), the `running_value` this
  process actually started with, its `source` (`runtime` / `admin` / `default`),
  and whether it is `mutable`, `requires_restart`, or has a saved change still
  waiting (`restart_pending`). `source: "runtime"` is the one that matters most
  in practice: it is how an operator discovers that a `daemon.toml` edit is being
  shadowed by an API write, which previously showed up only as a single `info`
  line at startup.
- **Five more writable keys**, all persisted to `runtime.toml` via the existing
  ADR-002 overlay rather than a privileged helper: `POST /config/poll-interval`
  (250–2000 ms), `/config/serial-port` (validated against the serial transport's
  own allowlist and length-capped — the daemon opens this path as root),
  `/config/serial-timeout` (50–1000 ms), `/config/allow-port-probe` and
  `/config/nvidia-telemetry`. The two interval bounds are deliberately tighter
  than what `daemon.toml` accepts; see **Changed** below.
- **Honest reporting for the two opt-ins.** `allow_port_probe` and
  `enable_nvidia_telemetry` each need a root-installed systemd drop-in *as well
  as* the config flag. Both the write response and `GET /config` carry
  `requires_privilege` saying so, so a client cannot truthfully show the feature
  as enabled on the strength of the flag alone.

### Fixed
- **`runtime.toml` no longer loses everything on a downgrade.** The top-level
  `RuntimeConfig` used `deny_unknown_fields`, and `load_from` treats any parse
  error as "malformed → use defaults". An older daemon started against a
  `runtime.toml` written by a newer one would therefore discard **every** runtime
  setting — profile search directories and startup delay included — and the next
  successful write would make that loss permanent. Unknown *sections* are now
  ignored; each section keeps `deny_unknown_fields`, so a typo inside a known
  section still fails loudly.
- **The advertised OpenFan stop timeout is derived, not restated.**
  `GET /capabilities` hardcoded `openfan_stop_timeout_s: 8` beside a
  `STOP_TIMEOUT` constant of 8 s — correct by coincidence, and silently wrong the
  moment the constant moved. Clients size their identify/stop timeouts from this
  value.
- **The manpage no longer overstates SIGHUP.** It claimed polling intervals were
  refreshed on reload; only the profile search directories are re-applied to the
  running daemon. Everything else is consumed once at process start.

### Changed
- `ipc.socket_path` and `state.state_dir` are reported by `GET /config` but are
  **deliberately not writable**: a bad socket path locks every client out of the
  daemon permanently, and moving the state directory orphans `runtime.toml`
  itself along with the daemon-owned profile store.
- **A configured serial port that cannot be opened now falls back to
  auto-detection.** Previously a configured port suppressed auto-detection
  entirely, so a wrong or stale path left the daemon with no OpenFan connection
  at all — and the thermal-emergency path to those fans is conditional on that
  connection existing. This is also simply what an operator wants when a device
  is renamed or unplugged.
- **The API's bounds for the poll interval (250–2000 ms) and serial timeout
  (50–1000 ms) are tighter than the config file's.** Both values bound how
  quickly the 105 °C rule can see a temperature and act on it, and unlike the
  config file the API is reachable by any local user.
- **`POST /config/serial-port` validates against the serial transport's own
  allowlist** rather than a second, looser check of its own, and caps the length.
  Two copies of a security check drift apart; the looser one accepted paths the
  transport then rejected.
- `GET /config` reports `profiles.search_dirs` as **not** requiring a restart,
  because its setter applies it immediately, and reads its running value from
  live state rather than the startup snapshot. Reporting otherwise produced a
  permanent, unclearable "restart required" for a change already in effect.

## [2.15.0] — 2026-08-04

**`sudo pacman -Syu` upgrades Control-OFC again.** DEC-240 retired the AUR and left
`pacman -U` from a GitHub Release as the only path, which meant upgrading was a manual
chore. This release adds a signed pacman repository served from GitHub, restoring
one-command upgrades without depending on the AUR. No daemon code changed, and **no
wire, schema, or API-shape change** (`api_version` stays 1; `GET /capabilities` is
untouched, so existing GUIs need no upgrade). Pairs with `control-ofc-gui` ≥ v2.23.0.
DEC-241.

### Added
- **A signed pacman repository — [`Plan-B-Development/pacman-repo`](https://github.com/Plan-B-Development/pacman-repo).**
  Trust one key, add one `pacman.conf` stanza, and the daemon then upgrades with your
  normal `sudo pacman -Syu`. Every package and the repository database are GPG-signed
  and served with `SigLevel = Required`, so pacman refuses anything not signed by the
  project key. The repository carries both packages, so `pacman -Sy control-ofc-gui`
  installs the pair.
- **`notify-repo` release job.** On a tag push, once the GitHub Release exists, the
  release workflow tells the repository to rebuild itself from it. It declares
  `needs: github-release` deliberately: the assembler pulls from the *latest* Release,
  so firing early would rebuild the repository around the previous version and serve a
  stale package as current.
- **Three regression tests** pinning that wiring — the job's existence, its
  `needs: github-release` ordering, its tag-push gating, the dispatch endpoint, and the
  use of the cross-repo token rather than the ambient `GITHUB_TOKEN`. Every one of those
  failures is silent: the release goes green, the Release object is correct, and users
  simply never receive the update.

### Changed
- **The README leads with the repository install.** The one-off `pacman -U` path
  remains documented as the no-`pacman.conf` alternative, and the DEC-240 note that the
  AUR package is frozen at v2.13.0 still stands. The out-of-tree DKMS drivers in the
  prerequisites table are unaffected third-party AUR packages, as before.

## [2.14.0] — 2026-08-04

**The AUR is retired as a publishing channel — GitHub is now the sole release target.**
This is a release-infrastructure and documentation change: no daemon code changed, and
**no wire, schema, or API-shape change** (`api_version` stays 1; the `GET /capabilities`
payload is untouched, so existing GUIs need no upgrade). Pairs with `control-ofc-gui`
≥ v2.23.0. DEC-239, DEC-240.

### Added
- **Every GitHub Release now carries the clean-room-built Arch package as a downloadable
  asset, with a keyless Sigstore build-provenance attestation over it.** The AUR is a
  third-party service that goes read-only for maintenance without notice: the 2026-08-02
  freeze took the *entire* AUR down to two accepted pushes in a day and stranded the
  GUI's v2.34.0 for over 24 hours. `pacman -U` on the Release asset is now a complete
  install path that depends on GitHub alone, and `gh attestation verify` proves the bytes
  came from this repo's CI. The package — built by the same clean-room job that runs a
  full `cargo build --release` + `cargo test` — was already being thrown away, so
  attaching it costs nothing. DEC-239.

### Changed
- **Releases no longer publish to the AUR.** The `aur-publish` CI job is gated to a
  manual `workflow_dispatch` and never runs on a tag push, so a release that is fully
  published by the only channel that matters no longer reports red because a third party
  is down. A tag push now runs exactly two jobs: `build-test` → `github-release`. The job
  is kept rather than deleted — one `gh workflow run release.yml -f tag=vX.Y.Z` resumes
  publishing if the AUR ever becomes viable again. DEC-240.
- **The PKGBUILD-pkgver and `daemon/Cargo.toml`-version guards moved into `build-test`.**
  They previously lived inside `aur-publish`, so gating that job to manual dispatch would
  have silently dropped both from every tag push. They now run on both paths and, because
  `build-test` gates the Release, a forgotten version bump blocks the Release itself
  rather than only the AUR push — strictly stronger than before. DEC-240.
- **The README leads with the prebuilt-package install** and demotes the AUR to a note
  recording that `control-ofc-daemon` is frozen there at v2.13.0. `pacman -U` upgrades an
  AUR-installed copy in place: same package name, and the newer version outranks the
  frozen one, so no AUR helper pulls you backwards. **The out-of-tree DKMS drivers in the
  prerequisites table (`it87-dkms-git`, `nct6687d-dkms-git`, `nct6686d-dkms-git`) are
  separate third-party AUR packages and are unaffected** — install them from the AUR as
  before.
- **The GitHub Release is now gated on the clean-room package build.** The two jobs
  previously ran in parallel, so a tag whose `PKGBUILD` did not build (or whose tests
  failed) still produced a Release. The attached asset is now always the exact artifact
  CI verified.

## [2.13.0] — 2026-07-30

Security and hardening release from the 2026-07-29 full cross-stack audit. Fixes a
denial-of-service any local user could trigger. **No wire, schema, or API-shape
change** (`api_version` stays 1); the `GET /capabilities` payload is untouched, so
existing GUIs need no upgrade. Pairs with `control-ofc-gui` ≥ v2.23.0 (v2.33.0
mirrors the new limits client-side). DEC-237.

### Fixed
- **A crafted profile could crash the daemon and keep it crashing across reboots
  (denial of service).** Mix and Sync curves resolve their dependencies by recursion,
  and while *cycles* were rejected, *depth* was not bounded — a long but perfectly
  legal acyclic chain overflowed the stack and aborted the process on the next
  control tick. Because activating a profile persists it as the active one, the
  daemon then re-loaded the same profile at boot and aborted again, leaving the
  service in a failed state until manually cleared. The socket is world-accessible by
  design, so any local user could reach it.

  Cooling was never at risk: the shutdown path restores firmware fan control on every
  exit, including a crash. The impact was loss of daemon-managed fan control until an
  operator intervened.

  Profiles are now capped at 256 curves and 256 controls — over ten times any real
  setup — rejected with `TOO_MANY_CURVES` / `TOO_MANY_CONTROLS`. The cap is enforced
  at three independent points: profile validation, profile loading (the boot paths
  deliberately skip validation, so this is the one that closes the reboot loop), and
  the evaluator itself, which now bails out of over-deep Mix and Sync resolution and
  falls back to holding the fan rather than crashing.

### Changed
- Runtime configuration is read through the size-capped file reader used elsewhere.
- A read lock is no longer held across filesystem calls while resolving profile paths.

### Documentation
- Corrected comments that claimed several evaluator functions had to stay identical to
  GUI code deleted at the 2.0.0 single-writer cutover. The golden-vector fixtures are
  the real cross-stack oracle; the daemon owns this evaluation outright.

## [2.12.3] — 2026-07-26

Contributor-facing comment and documentation cleanup from the 2026-07-26 audit — **no behavioural
change** (`api_version` stays 1). Pairs with `control-ofc-gui` ≥ v2.23.0 (patch v2.30.2). DEC-232.

### Changed
- Reworded three stale "GUI stand-down" / DEC-132 comments to the DEC-165 sole-writer model: since
  the 2.0.0 cutover the GUI holds no control loop, so it surfaces a display-only thermal-safety
  banner rather than standing a loop down.
- Both READMEs no longer call `docs/DEVELOPER_HANDOVER.md` "the full API reference" — it is
  developer onboarding; `daemon.md` is the architecture overview.
- Added a DEC-199 rationale comment to the ExecStopPost fan-restore script (it writes through the
  `/sys/class/*` symlinks, but the service sandbox grants write via `ReadWritePaths=/sys/devices`).

## [2.12.2] — 2026-07-21

Audit-2026-07-21 remediation (daemon side): three profile-engine correctness
fixes, a CI-clippy unblock, doc/packaging hygiene, and test hardening. **No
wire, schema, or API change** (`api_version` stays 1). Pairs with
`control-ofc-gui` ≥ v2.23.0 (unchanged — no new GUI feature required).

### Fixed
- **CI clippy failure on `main` cleared (2026-07-21 audit Phase 8, PKG-2
  finding).** The profile `sync`-curve validation arm was a bare `if` that
  Rust 1.95+'s `clippy::collapsible_match` (implied by the CI gate's
  `-D warnings`) rejects; the daemon's CI job had been red since the
  toolchain rolled forward. Bound `sync_control_id.as_str()` once and reshaped
  the arm to match its `trigger`/`mix` siblings — behaviour identical
  (`validate_sync_dangling_ref_is_error` unchanged), now clean under clippy
  1.97.1 (the runner's current `stable`).
- **GPU verify race closed at the blocking-task boundary (2026-07-21 audit
  CONC-1).** The engine's GPU write task now re-checks the verify write-pause
  *inside* the blocking task, immediately before the PMFW sysfs write —
  previously only the async-side checks ran, so a `POST /gpu/{id}/fan/verify`
  starting in the dispatch gap could have its test value overwritten and
  report a false verify result. A pause-skip records no outcome (it is
  neither a success nor a cached failure), mirroring the OpenFan in-closure
  re-check (DEC-191). Unit-tested via the extracted `gpu_blocking_write`
  helper; kill-verified.
- **Steady 0 % holds no longer inflate OpenFan failure streaks (2026-07-21
  audit CONC-2).** `FanController::set_pwm` now coalesces a same-value repeat
  *before* the 8 s stop-timeout check. Previously a curve or identify-stop
  legitimately holding 0 % returned `Validation` on every tick past 8 s,
  growing per-channel failure streaks (and potentially the whole-link alert)
  on a healthy link. The timeout still rejects a wire-bound 0 % against an
  expired stop timer (channel-tracking drift) as defence-in-depth.
  Kill-verified both ways.
- **Thermal-state cache write is now unconditional (2026-07-21 audit
  CONC-3).** `set_thermal_override_state` dropped its read-lock
  compare-and-skip fast path (EFF-4), which was lossless only under an
  unenforced single-writer invariant. One uncontended write lock per 1 Hz
  tick; no observable behaviour change.

### Security
- **`POST /profile/activate` no longer leaks the store path in its error
  envelope (2026-07-21 audit SEC-2, recorded under DEC-223).** A corrupt or
  unreadable stored profile now returns the generic `400 validation_error`
  message "profile could not be read or parsed"; the path-bearing read/parse
  detail goes to the daemon log only — matching the DEC-173 posture of the
  CRUD save/delete handlers. HTTP status and error code are unchanged.
  Regression test:
  `profile_activate_parse_error_returns_generic_message_without_path`.

### Changed
- **systemd unit: `Group=root` pinned explicitly (2026-07-21 audit PKG-3; no
  behaviour change).** systemd already derived the group from root's passwd
  entry, so this is a no-op today — it documents intent and stops a future
  `User=` change (or `DynamicUser=`) from silently inheriting an unexpected
  primary group. `systemd-analyze verify` clean.
- **Regression-test hardening (2026-07-21 audit remediation, Phase 4; no
  behaviour change).** `OpenFanBackend::apply` gains backend-level coverage:
  exact pct→wire frame translation (channel-index hex + raw byte, `>0203FF` /
  `>020000`), and a CONC-2 propagation test proving a coalesced steady-0 %
  hold keeps the per-channel and link-down failure streaks clear. The
  baseline (store-less) `/capabilities` integration test now pins
  `control.autonomous_control = true` — previously only the store-enabled
  test asserted the flag the GUI's startup control gate depends on.
- **Docs/packaging text corrections (2026-07-21 audit remediation, Phase 1; no
  code, wire, or behaviour change).** `daemon.md`'s endpoint table now records
  DEC-218 on `POST /profile/deactivate` (clears control-overrides, not
  identify-stops) and extends the override row's clear condition to
  activation/deactivation; `docs/DEVELOPER_HANDOVER.md`'s module map gains
  `assessment.rs` and the `/inventory/hardware-readiness` route (DEC-207);
  the `modules-load.d` comment and the man page now point at the GUI's
  **Hardware** page (the tabbed Diagnostics page was retired in the GUI
  redesign, GUI DEC-216); `daemon.md` documents the accepted bounded-risk
  posture of floor-exempt identify-stops (any local uid, deadman + thermal
  `force_all` backstops — 2026-07-21 audit accept+document).

## [2.12.1] — 2026-07-18

Test-coverage hardening only — **no runtime behaviour, wire, or API change**
(`api_version` stays 1). Regression tests added in the 2026-07-18 `/test-tests`
pass; every change is inside `#[cfg(test)]`/integration-test code. Pairs
(unchanged) with `control-ofc-gui` ≥ v2.23.0.

### Changed
- **Regression-test hardening (no behaviour change).** Added tests around
  thermal-safety `force_all` (per-header value assertions — proves every header,
  not just the first, is driven to the emergency duty and survives a mid-scan
  lease preemption), manual-override take/renew/release lifecycle, the sensor-
  failure quarantine tracker, and hwmon readiness classification. These pin
  behaviour that already shipped in v2.12.0; no source logic changed.

## [2.12.0] — 2026-07-17

Thermal-safety and override-lifecycle hardening from the 2026-07-15 cross-stack
audit remediation, plus systemd-unit tightening. No breaking changes; no wire or
API-version change (`api_version` stays 1). Coordinated with `control-ofc-gui`
≥ v2.23.0 — the changes are daemon-internal, so older GUIs are unaffected. DEC-218.

### Fixed
- **Thermal `force_all` completes every header despite a mid-scan lease preemption.**
  When a `Verify` write preempts the thermal-safety lease mid-scan, `force_all` now
  re-takes the `ThermalSafety` lease and retries that one header, atomically under
  the controller lock (bounded — a persistent preemptor cannot thrash), so a 105 °C
  emergency forces every OpenFan + writable hwmon fan even under contention. Mutation-
  proven regression test.

### Changed
- **`POST /profile/deactivate` clears standing control-overrides (DEC-218).** Symmetric
  with DEC-189 (activate clears overrides): deactivating a profile now clears all
  control-overrides under the `active_profile` lock, so a renew after deactivation
  returns `404 override_expired` instead of surviving into the idle (no-profile) state.
  Identify-stops are preserved.
- **systemd unit hardening.** The OpenFan-TTY `DeviceAllow` drops the `mknod` bit
  (`rwm`→`rw`; the daemon only opens existing device nodes); `RuntimeDirectoryMode=0755`
  and `User=root` are pinned explicitly (the DEC-049 non-root-GUI socket-access model
  depends on the world-traversable runtime directory). No privilege change.

## [2.11.0] — 2026-07-13

A single shared hardware-assessment snapshot behind the readiness + Super-I/O
endpoints (one coalesced passive scan instead of three), a new combined `GET
/inventory/hardware-readiness` endpoint for the GUI's merged "Cooling Hardware
Readiness" page, and a Super-I/O classification fix. No breaking changes; pairs
with `control-ofc-gui` ≥ v2.13.0 (older GUIs keep using the existing endpoints).
DEC-207.

### Added
- **Combined `GET /inventory/hardware-readiness` (DEC-207).** One atomic fetch
  returning the readiness `rollup` + `overall` + `items`, the passive `superio`
  report, `scanned_age_ms`, and a monotonic `generation`, so the GUI's merged page
  gets a consistent snapshot in one request. `?refresh=true` forces a fresh
  (coalesced) scan. Additive and 404-gated on older daemons.

### Changed
- **Single shared hardware-assessment snapshot (DEC-207).** `/inventory/readiness`,
  `/inventory/superio`, the combined endpoint, and the `/status` + `/poll` rollup
  are now served from ONE cached passive scan (cache snapshot + `/sys` walk +
  `runtime.toml` read + Super-I/O detect) instead of each recomputing Super-I/O
  detection independently. A single-flight coordinator coalesces simultaneous
  requests into one scan; a short freshness window lets a passive Super-I/O GET
  reuse a recent readiness scan; the 1 Hz poll path still only clones the small
  cached rollup (kept in lockstep with the full snapshot). `POST /hwmon/rescan` now
  also refreshes the assessment (deferred, so the descriptor set rebuilds first).

### Fixed
- **Ordinary hwmon chips are no longer reported as Super-I/O (DEC-207).** The
  passive detector now gates bound-hwmon evidence on a Super-I/O family check, so
  ordinary sensor chips such as `amdgpu`, `k10temp`, `nvme`, and `spd5118` no
  longer appear as "Unrecognized Super-I/O" entries in `/inventory/superio`. An
  active-probe hit for a chip already seen passively now folds into that chip's
  card (unioning the `port_probe` evidence) instead of producing a duplicate.

## [2.10.0] — 2026-07-13

Additive hardware-readiness rollup on the poll surface for the GUI's new Dashboard
cooling-readiness health chip (DEC-206). No breaking changes; pairs with
`control-ofc-gui` ≥ v2.12.0 (older GUIs ignore the new field).

### Added
- **Compact readiness rollup on `/status` + `/poll` (DEC-206).** `StatusResponse`
  gains an optional `readiness` object — `{overall, critical, warning, info,
  top_summary, top_code}` — derived from the same items `/inventory/readiness`
  returns. It is cached in `AppState` and refreshed only on discovery-changing
  events (startup, a preferred-sensor change, and each `/inventory/readiness`
  GET), so the 1 Hz poll only clones a small struct: `build_readiness` stays pure
  and the expensive readiness scan (cache snapshot + sysfs walk + `runtime.toml`
  read + Super-I/O detect) never runs on the hot path. Omitted by daemons
  predating the field (and until the startup seed runs), so the wire shape is
  unchanged for older clients.

## [2.9.0] — 2026-07-11

Security + hardening follow-up from the 2026-07-08 audit (Wave 2, DEC-205). No
breaking changes; pairs with `control-ofc-gui` ≥ v2.11.1 (unchanged — the GUI
already surfaces the daemon's message).

### Security
- **`POST /config/profile-search-dirs` is now peer-uid-confined (DEC-205).** On a
  multi-user host, a non-root client could previously register any absolute
  directory as a profile search path. The daemon now reads the connecting
  peer's uid from `SO_PEERCRED` (threaded through axum via
  `into_make_service_with_connect_info`) and, for a non-root caller, only
  accepts directories that exist and canonicalize to within that user's own
  home directory (closing symlink/`..` escapes). Root and CLI callers stay
  unrestricted; an unresolvable uid or home fails closed. The file-picker UX for
  single-user desktops is unchanged.

### Changed
- **NVML is loaded from absolute paths first (DEC-205).** `libnvidia-ml.so.1` is
  now resolved by trying `/usr/lib`, `/usr/lib64`, then
  `/usr/lib/x86_64-linux-gnu`, falling back to the bare SONAME last, so a
  hardened service with a minimal linker search path still finds it. The three
  absolute-path candidates are immune to `LD_LIBRARY_PATH` redirection; only the
  bare-SONAME fallback remains susceptible, so `LD_LIBRARY_PATH` must not be set
  for the service. Still gated behind `enable_nvidia_telemetry` (off by default);
  no new config key.

## [2.8.1] — 2026-07-09

Post-release hardening for the DEC-200/202/203/204 features (audit 2026-07-08,
Wave 1). Bugfix + docs + packaging + tests; no contract change, no new DEC. Pairs
with `control-ofc-gui` ≥ v2.11.0 (unchanged).

### Fixed
- **The active Super-I/O probe no longer leaves a chip in config/unlock mode when
  a DEVID read fails (DEC-203).** In `probe_base`, an `io::Error` from the DEVID
  read after `ite_enter`/`nuvoton_enter` returned via `?` without running the
  matching `*_exit`. A new RAII `SioExitGuard` (mirroring the `CalibrationGuard`
  idiom) now issues the exit on every path out — including the error path — while
  logging a failed exit write at `debug` rather than masking the original read
  error. Regression tests cover both the ITE and Nuvoton legs.

### Documentation
- `daemon.md`: added the six missing `hwmon/` modules (`chip_db`, `classify`,
  `inventory`, `readiness`, `superio`, `superio_probe`) and `api/handlers/
  inventory.rs` to the module map; added `GET /inventory/superio` and `POST
  /inventory/superio/probe` to the endpoint tables; corrected the
  `kernel_warnings` summary (the RDNA3/4 hang spans kernel 6.18.x **and** 6.19.x;
  the R9700 SMU mismatch is device-scoped to PCI 0x7551, not kernel-tied).
- `docs/USER_GUIDE.md`: added NVIDIA GPU rows to the supported-hardware table
  (read-only — `nouveau` temps + fan RPM, opt-in NVML temps + measured duty).
- `daemon/README.md`: added a v2.8.0 upgrade note for the two opt-ins (NVML
  telemetry, Super-I/O port probe) and their systemd drop-in examples.
- `SECURITY.md`: documented the opt-in `/dev/port` (Super-I/O probe) and
  `/dev/nvidia*` (NVML) device-access boundaries.

### Packaging
- PKGBUILD: added an `nvidia-utils` optdepend (the NVML runtime for the opt-in
  telemetry; the open `nouveau` driver does not need it).
- `.install`: `post_upgrade` now flags any hand-installed opt-in drop-in for
  review against the updated example.
- `daemon.toml.example`: reworded the top NOTE — the startup delay and profile
  search dirs are admin-owned base defaults that `runtime.toml` overlays on API
  write, not settings that live *only* in `runtime.toml`.

### Tests
- `ipc_integration`: added coverage for `GET /inventory/superio`, `POST
  /inventory/superio/probe` (disabled-by-default), an NVIDIA `nvidia_gpu`
  temperature on `/sensors`, and `duty_pct` wire serialization including the `0`
  edge (only `None` is omitted).

## [2.8.0] — 2026-07-08

NVIDIA GPU support — Phase 1 (read-only telemetry). Ships coordinated with GUI
v2.11.0, which consumes these surfaces (the GUI closes the DEC-047 idle-fan gap
and renders `nvidia_gpu` capability/diagnostics + the `duty_pct` field). Pairs
with `control-ofc-gui` ≥ v2.11.0. Additive and version-skew tolerant — a client
that predates these fields simply ignores them. Fan **write** control is a
deliberately deferred Phase 2 (needs NVIDIA hardware to validate); the NVML path
is experimental/unverified on real hardware and **off by default**.

### Added
- **NVIDIA discrete GPU read-only sensing via the open `nouveau` driver
  (DEC-204).** GPU temperatures appear on `/sensors` with `source: "nvidia_gpu"`
  (kind `GpuTemp`), and fan RPM on `/fans` with id `nvidia_gpu:<PCI_BDF>` and no
  `last_commanded_pwm` (read-only). Detection is hwmon-based (`name ==
  "nouveau"`), mirroring the Intel Arc read-only leg (DEC-121). **No fan
  writes** — the writable nouveau `pwm1` is excluded from hwmon PWM-header and
  monitor-only-fan discovery (shared `is_gpu_owned_hwmon_chip` predicate,
  alongside the `amdgpu` DEC-102 exclusion), so the profile engine can never
  drive a GPU fan.
- **Opt-in, read-only NVIDIA telemetry via NVML (proprietary driver, DEC-204).**
  Where the proprietary driver exposes no hwmon node, the daemon can dlopen
  `libnvidia-ml.so.1` and read GPU temperature + fan telemetry. **Off by
  default** (`[detection] enable_nvidia_telemetry = false`); also needs an opt-in
  `/dev/nvidia*` systemd drop-in (`nvidia-telemetry.conf.example`, shipped to
  `/usr/share/doc`, NOT installed). **Experimental — the NVML path is unverified
  on real hardware**; it degrades to a no-op when NVML is absent and NEVER writes
  to any GPU. All `unsafe` FFI is isolated in `hwmon/nvml_sys` (hand-written
  bindings cross-verified against `nvml-wrapper-sys`); adds the `libloading`
  dependency (ISC — see `deny.toml`).
- **`duty_pct` on `/fans` (additive, DEC-204).** A measured/firmware-reported
  current fan duty %, present only for sources that expose a duty readback
  (NVIDIA via NVML). Distinct from `last_commanded_pwm` (commanded) — never
  conflated. **May exceed 100** (NVML expresses it as a % of the product's
  max-noise-tolerance fan speed, not a hard ceiling). Optional/omitted when
  absent, so older clients are unaffected.
- **NVIDIA `/capabilities` + `/diagnostics/hardware` surfaces (DEC-204).**
  `GET /capabilities` gains an additive read-only `devices.nvidia_gpu` block
  (present, display_label, model_name, pci_bdf/pci_id, driver
  `"nouveau"`/`"nvidia"`, driver_version, `fan_control_method`
  `"read_only"`/`"none"`, fan_rpm_available, is_discrete)
  — mirroring the Intel Arc capability (DEC-121), with **no `fan_write_supported`**
  (never writable). `GET /diagnostics/hardware` gains a matching additive
  `nvidia_gpu` block with the identity + a truthful "why fan control is
  unavailable" note. Both are fed by a unified `nvidia_gpus` identity (nouveau +
  NVML), gathered once at startup: the proprietary NVML leg supplies the real
  model name + driver version (via the added `nvmlDeviceGetName` /
  `nvmlSystemGetDriverVersion` getters), the open nouveau leg a generic
  "NVIDIA D-GPU" label.

## [2.7.0] — 2026-07-07

Built-in Super-I/O chip detection (DEC-202/203). Passive detection is report-only
and never touches hardware — it tells you which motherboard sensor/fan driver to
load, it does not load it. A separate, **off-by-default** active `/dev/port` probe
(DEC-203) can identify an unbound chip on request; it needs an explicit config
flag *and* a `CAP_SYS_RAWIO` systemd drop-in, so the default install is unchanged.

Pairs with `control-ofc-gui` ≥ v2.10.0.

### Added
- **`POST /inventory/superio/probe` — opt-in ACTIVE Super-I/O port probe
  (DEC-203).** A deliberate, one-shot `/dev/port` read of the Super-I/O config
  ports (0x2E/0x4E) that identifies an **unbound** chip passive detection cannot
  see, so the user can be told which driver to load. **Off by default** — needs
  both `[detection] allow_port_probe = true` and an opt-in `CAP_SYS_RAWIO`
  systemd drop-in (shipped as `superio-port-probe.conf.example`, NOT installed;
  the default unit stays fully hardened). Refuses to probe at all when any
  Super-I/O driver is already bound, skips any ACPI-reserved base, and refuses if
  `/proc/ioports` can't be read; reads only chip-ID registers; never writes a config
  value or `force_id`; safe-Rust `/dev/port` I/O; fails gracefully under kernel
  lockdown (Secure Boot). `GET /inventory/superio` gains `port_probe_available`
  + `port_probe_reason` so the GUI can gate its advanced probe button.
- **`GET /inventory/superio`** — passive Super-I/O detection report. Composes the
  DMI board table, bound hwmon chips, `/proc/modules`, `/dev/kmsg`, and ACPI
  I/O-port conflicts into a per-chip presence report (vendor, evidence,
  confidence, bound driver) with an allowlisted, caveated "load this driver"
  recommendation for unbound chips. x86-gated (`arch_supported:false` elsewhere);
  read-only. Additive — older GUIs ignore it, and it 404s on older daemons like
  the other `/inventory/*` routes.
- **Super-I/O guidance in `GET /inventory/readiness`.** The readiness list now
  gains `superio_driver_unloaded` / `superio_acpi_conflict` items so
  board-specific "your chip has no driver loaded" guidance appears alongside the
  generic `no_pwm_controls` item.
- **`hwmon::superio` passive detector** (the engine behind the endpoint): a
  dependency-injected `SuperIoEvidence` trait for hardware-free testing, and a
  collision(DEC-106)/ACPI/DKMS-aware recommendation engine that only ever names
  an allowlisted module and never suggests a risky parameter.
- **Extended Super-I/O chip recognition to the full driver-family set** — ITE,
  Nuvoton, Winbond (`w83627ehf` / `w83627hf`), SMSC (`smsc47m1` / `smsc47b397` /
  `dme1737` / `sch5627` / `sch5636`), National (`pc87360` / `pc87427`) and Fintek
  — each mapping verified against that driver's kernel documentation.
  `GET /diagnostics/hardware` now lists these additional known modules.

### Fixed
- **Fintek chip→driver mapping.** `F71805F` / `F71806F` / `F71872F` now correctly
  resolve to the `f71805f` driver instead of `f71882fg` (they are separate
  drivers); the daemon previously would have pointed owners of those chips at a
  module that will not bind.

### Changed
- Internal refactor: the chip↔driver knowledge base and passive detection
  primitives moved from `api::diagnostics` into a new single-source-of-truth
  `hwmon::chip_db` module; `api::diagnostics` is now a thin re-export shim. No
  behaviour change to `GET /diagnostics/hardware` beyond the additions above.

_Version: deferred — batched with the upcoming Super-I/O feature under
[Unreleased]._

## [2.6.0] — 2026-07-05

Daemon-owned, read-only CPU/hwmon/PWM discovery + readiness (DEC-200), plus a
verify thermal-abort precondition (DEC-201). API v1 — the only change to an
existing endpoint is the new `409 thermal_abort` refusal on the verify routes;
everything else is additive. The profile engine remains the sole PWM writer —
discovery never writes hardware.

Pairs with `control-ofc-gui` ≥ v2.0.0 (additive — any 2.x GUI keeps working). The
new inventory / readiness / preferred-sensor / verify-thermal surfaces are
consumed by `control-ofc-gui` ≥ v2.9.0, which degrades gracefully on older
daemons (unknown routes 404 and the new UI shows an "unavailable" state).

### Added
- **Read-only hwmon inventory — `GET /inventory/hwmon`.** A structured snapshot
  of hwmon-visible hardware for the GUI: temperature sensors, controllable PWM
  headers, and **monitor-only fan tachometers** (`fanN_input` with no matching
  `pwmN`, previously invisible to the API). Never writes hardware.
- **Fine-grained temperature-sensor classification.** Each inventory temp sensor
  gains an advisory `classification` (cpu_package / cpu_core / cpu_tctl /
  cpu_tdie / motherboard_temp / vrm_temp / chipset_temp / gpu_temp / disk_temp /
  coolant_temp / unknown_temp), a `confidence` (high/medium/low/unknown), and a
  plain-English `rationale`. It **refines** the coarse `kind` — a sensor's `kind`
  and the daemon's thermal safety are unchanged. A deterministic, explainable
  `default_cpu` recommendation is included: advisory only, never a silent
  replacement of a user's choice.
- **Structured hardware-readiness list — `GET /inventory/readiness`.** An
  actionable diagnose-and-guide list (`items[]` with a stable `code`, severity
  `ok`/`info`/`warning`/`critical`, component, summary, detail, recommended
  action, and per-item `can_automate`/`blocks_monitoring`/`blocks_control`/
  `affects_safety`/`reboot_may_be_required` flags) plus an `overall` rollup.
  Covers CPU-sensor presence (safety-relevant), default-CPU confidence, PWM
  controls present/read-only/unverified, monitor-only tachometers, quarantined
  sensors (DEC-193), and unclassified sensors. Read-only — the daemon never
  mutates the system or auto-remediates.
- **Persisted preferred CPU / motherboard sensor (Phase 5).**
  `POST /config/preferred-cpu-sensor` and `POST /config/preferred-mb-sensor`
  store a user-approved sensor by stable id in `runtime.toml`
  (`{"sensor_id":"<id>"}` sets, `null` clears; validated against the live sensor
  set; persist-first with `503 persistence_failed` on write error). The preferred
  CPU sensor is reflected in `/inventory/hwmon` `default_cpu` (`source:"user"`)
  and echoed under `preferences`; `/inventory/readiness` gains
  `selected_cpu_sensor_missing` / `selected_mb_sensor_missing` when a chosen
  sensor later disappears. Advisory — thermal safety still uses the hottest
  CpuTemp, and a stale selection is never blindly applied.

### Changed
- **Fan verify refuses to start while the system is hot (DEC-201).**
  `POST /hwmon/{header_id}/verify` and `POST /gpu/{gpu_id}/fan/verify` now return
  `409 thermal_abort` when any sensor exceeds the 85 °C verify limit
  (`CALIBRATION_MAX_TEMP_C`). A verify pauses the engine's write phase for its
  window (DEC-191), which also suppresses the 105 °C thermal `force_all` — so a
  fan diagnostic must not run during a thermal event. Reuses the calibrate
  sweep's `check_thermal_safety` via a shared `verify_thermal_guard`, so verify
  and calibrate share one thermal gate and threshold. This is the "abort on high
  temperature" requirement of the Phase-6 safe-verify design; every other Phase-6
  requirement was already satisfied by the existing verify path
  (DEC-098/101/120/165/191).

## [2.5.2] — 2026-07-04

Packaging bugfix: motherboard (hwmon) and GPU fan writes failed with `EROFS`
under the systemd sandbox, so those fans were never actually driven by the
daemon (they stayed in BIOS/PMFW automatic mode) and journald was spammed at
1 Hz. No API, wire-contract, or control-loop behaviour change. No GUI release
(GUI stays v2.8.3; a docs-only troubleshooting cross-reference was added there).

### Fixed
- **Motherboard + GPU fans are controllable again under the packaged systemd
  unit (DEC-199).** `ProtectKernelTunables=true` remounts all of `/sys`
  read-only (`ProtectSystem=strict` alone does not — it exempts `/sys`), and the
  writable carve-out was `ReadWritePaths=/sys/class/hwmon /sys/class/drm`. But
  those class directories hold only symlinks, and sysfs decides writability by
  the symlink *target* inode's mount: every `pwm*`, `pwm*_enable`, and GPU
  `fan_curve` write resolves through those symlinks to `/sys/devices/...`, which
  stayed read-only — so each write failed with `EROFS` ("Read-only file system",
  os error 30). The carve-out is now `ReadWritePaths=/sys/devices`, which covers
  the real inodes while keeping `/proc/sys`, `/sys/kernel`, and `/sys/module`
  read-only (strictly better hardening than dropping `ProtectKernelTunables=`).
  The stop-time restore helper's `[ -w ]` guard — which silently no-op'd on the
  read-only mount — works again with the same fix. Latent since the first
  packaging commit (2026-04-08); only ever hit on installed packages driving
  motherboard/GPU fans (dev runs and OpenFan-only setups never exercise the
  `/sys` write path). A new `packaging_version` test asserts the carve-out covers
  `/sys/devices`, so a revert to the class-level path fails `cargo test`.

### Changed
- **The hwmon backend throttles its write-failure log (DEC-199).** A persistent
  motherboard-fan write failure previously re-logged at WARN every 1 Hz tick; it
  now logs once on the first failure, then a "still failing" summary every
  `HWMON_FAIL_SUMMARY_INTERVAL` (300) ticks, plus an INFO recovery line when the
  member writes again — tracked per member, so one stuck header is isolated from
  healthy siblings (mirrors the OpenFan per-channel discipline; the GPU backend
  already had a 60 s fail-cooldown). Keeps the journal quiet if a hwmon write
  ever fails for another reason (e.g. a genuinely BIOS-locked chip).
- **`post_install` surfaces the fan-control hardware prerequisites** — the
  Super I/O DKMS driver, `acpi_enforce_resources=lax`, and
  `amdgpu.ppfeaturemask=0xffffffff` — that otherwise leave a discovered header or
  GPU fan silently uncontrolled, and points at the GUI's Hardware Readiness card
  for the per-machine fix.

## [2.5.1] — 2026-07-04

Packaging + supply-chain hardening pass (Cluster 6 + 7). No API, wire-contract, or
control-loop behaviour change — the systemd unit, the release CI, the stop-time restore
script, and a dev-only advisory/licence cleanup. Verify on real hardware after install.

### Security
- **systemd unit hardened.** The service now drops the six capabilities the daemon
  provably never uses (`CapabilityBoundingSet=~CAP_NET_ADMIN CAP_NET_RAW CAP_SYS_PTRACE
  CAP_SYS_RAWIO CAP_SYS_MODULE CAP_SYS_BOOT` — `CAP_DAC_OVERRIDE` is deliberately KEPT as
  insurance for a board whose sysfs node is not root-owned), runs in a private network
  namespace (`PrivateNetwork=true`) restricted to `AF_UNIX`/`AF_NETLINK`
  (`RestrictAddressFamilies`; the daemon's only socket is the filesystem Unix socket and
  libudev port enumeration needs netlink — `AF_INET`/`INET6`/`PACKET` are blocked), and
  adds `ProtectClock`, `ProtectHostname`, `ProtectProc=invisible`,
  `SystemCallArchitectures=native`, `UMask=0027`, `StateDirectoryMode=0700`, and
  `StartLimitIntervalSec=60`/`StartLimitBurst=5`. `ProcSubset=pid` is deliberately NOT
  set — it would hide `/proc/{modules,ioports,cpuinfo}`, which the live
  `GET /diagnostics/hardware` handler reads. Removed the redundant
  `DeviceAllow=char-usb_device rwm`: the serial transport is a tty (no libusb linked), and
  the `char-ttyACM`/`char-ttyUSB` rules already cover every device.
- **Resolved RUSTSEC-2026-0190** (unsound `anyhow <1.0.103`) by bumping the lockfile to
  1.0.103. anyhow reaches the tree only as a dev-only, wasm-target-gated transitive dep
  (via `tempfile → getrandom`) and is absent from the shipped binary, but the fix is a
  free one-crate lock bump. Separately documented in `deny.toml` that the `unescaper`
  deprecated-SPDX (`GPL-3.0/MIT`) cargo-deny `parse-error` is a benign, exit-0 warning we
  accept — the crate passes under MIT (`cargo deny check` stays green); a hash-pinned
  `clarify` isn't worth the per-bump maintenance and no waiver is warranted.

### Changed
- **Release CI asserts `daemon/Cargo.toml` version == tag** (mirrors the existing
  PKGBUILD-pkgver-vs-tag guard), backed by a new `packaging_version` test pinning the crate
  version to `packaging/PKGBUILD` `pkgver`. A version/tag drift now fails `cargo test` and
  the AUR publish instead of shipping a mislabelled package.
- **The stop-time restore script uses `shopt -s nullglob`** so a no-match hwmon/GPU glob
  expands to nothing instead of the literal pattern.

Pairs with `control-ofc-gui` ≥ v2.0.0 — packaging only; no GUI change.

## [2.5.0] — 2026-07-03

Audit-2026-07-03 Cluster 2: post-2.0.0 demolition-debris cleanup. No runtime behaviour
change except the SSE removal (a documented-but-unused endpoint leaves the API surface).

### Removed
- **`GET /events` SSE endpoint removed (DEC-198).** The Server-Sent Events stream had zero
  consumers — the GUI is poll-only and DEC-164 deferred SSE past 2.0.0. Deleted the endpoint,
  the `sse_clients` counter, the four `SSE_*` constants, the `too_many_clients` error code, and
  `futures-util` as a direct dependency. **Contract change:** `GET /events` and the
  `503 too_many_clients` code leave the API; the GUI's `docs/08` is updated in lockstep. Only a
  hypothetical external SSE client is affected (there are none).
- **Dead serial write surface (~490 lines).** Removed the never-called
  `FanController::set_pwm_all` / `set_target_rpm` (and their `Command` variants + result types),
  `StateCache::set_openfan_commanded_pwm_all`, the unused `MAX_PWM` / `MAX_RPM` consts,
  `hwmon::collect_sensors`, `WriteBackend::name()`, and two never-constructed error variants —
  all `pub` (so invisible to the dead_code lint) but with zero production callers since the
  2.0.0 sole-writer cutover.

### Changed
- **hwmon PWM writes are arbitrated by a typed in-process token, not a client lease (DEC-197).**
  Replaced the arbiter's free-form `owner_hint: String` with
  `enum HwmonWriter { Engine, Verify, ThermalSafety }` and removed three dead members
  (`is_expired` / `ttl_seconds` / `created_at`). Behaviour-preserving — same state machine and
  per-write fence, log strings unchanged; the client-lease protocol was already retired at 2.0.0
  (DEC-165). Verify/calibration exclusion stays arbiter-based so a thermal emergency can still
  preempt a verify's in-flight restore write.
- **Trigger-curve defaults (40 / 60 / 30 / 80) extracted to named `pub(crate)` constants** —
  values byte-for-byte unchanged (cross-stack GUI parity, DEC-126/149).
- Corrected five stale post-2.0.0 code comments that described a GUI write-loop, a GUI lease, or
  a PWM floor that no longer exist.

### Security
- **Config / state / profile JSON reads are capped at 4 MiB** (`atomic_io::read_to_string_capped`,
  matching the GUI's `load_json_capped`) instead of being buffered whole by the long-lived root
  process, and the HTTP request-body limit is now explicit (`DefaultBodyLimit::max(4 MiB)`).

Pairs with `control-ofc-gui` ≥ v2.0.0 — the GUI needs no code change; its `docs/08` tracks the
SSE removal.

## [2.4.2] — 2026-07-02

### Fixed
- **Driver-state comments in `diagnostics.rs` refreshed to July 2026 (lockstep with GUI
  v2.6.2).** Corrected the `chip_driver_in_mainline` / `KNOWN_MODULES` rationale: mainline
  kernel 7.1 added IT8689E fan *control* (commit `66b8eaf` — six PWM, `FEAT_FANCTL_ONOFF`;
  released 2026-06-14), not "sensor support"; the enum reference is now verified against
  v7.1 / 7.2-rc1.
- **Corrected the X870 AORUS STEALTH ICE comment + test.** Its secondary is an IT8696E +
  IT87952E pair — not an undriveable "IT8883" (device-ID `0x8883` is only a stuck-config-mode
  symptom; a clean read is `0x8695`), recovered with `mmio=on` (frankcrawford/it87 #81/#70).

`it8689` deliberately still reports NOT-mainline (DEC-144 policy — 7.1 is not yet the common
kernel). Comment- and test-only; no behaviour or contract change. (Follow-up flagged:
enrolling STEALTH ICE in the dual-chip detection table.) Pairs with `control-ofc-gui` ≥ v2.0.0.

## [2.4.1] — 2026-07-01

### Internal
- **`profile_engine/mod.rs` split into submodules (Cluster C).** The pure leaf
  functions — curve/deadband/trigger + Mix/Sync composites + topo ordering
  (`curve_eval.rs`), the tuning pipeline + per-member floor (`tuning.rs`), and the
  thermal-safety decision (`safety_tick.rs`) — moved out of the ~4.1k-line
  god-module into sibling `pub(crate)` modules, leaving `mod.rs` as the 1 Hz loop +
  evaluation orchestrators + state + tests. Pure code-move, no behaviour change;
  the parity oracle and the DEC-190/188/167/149 safety tests are unchanged and green.

Internal refactor only — no behaviour or contract change. Pairs with `control-ofc-gui` ≥ v2.0.0.

## [2.4.0] — 2026-07-01

### Added
- **`active_profile_id` / `active_profile_name` on `/status` + `/poll` (DEC-194).** The currently-active
  profile is mirrored onto the status surface so a client can reflect an external activation (CLI
  `--profile`, another client, systemd) within one 1 Hz poll instead of waiting for its periodic
  `/profile/active` refresh. Both fields are omitted when no profile is active (additive; `api_version`
  unchanged).

### Internal
- **Per-commit/PR CI (`.github/workflows/ci.yml`).** `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test`, and a per-PR `cargo deny` supply-chain check now run on every push/PR to `main` — the
  standard gates previously ran only at release-tag time (inside `release.yml`).
- **Parity-oracle drift guard (`.github/workflows/parity.yml`, DEC-195).** On any change to
  `daemon/tests/fixtures/parity_vectors.json`, CI checks out the GUI repo and asserts the two
  `parity_vectors.json` copies (DEC-126) are byte-identical — closing the only gap where the shared
  evaluator oracle could diverge with both repos' CI green.
- **Override/identify error-envelope tests hardened.** `ipc_integration.rs` now pins the full error
  envelope shape ({code, message, retryable, source}) on a representative override path and the
  `error.code` on the unknown-fan 404; a serde test pins `override_token` as required (no silent default).

Additive `/status` + `/poll` field (DEC-194) plus test/CI hardening (DEC-195); no `api_version` or
runtime behaviour change. Pairs with `control-ofc-gui` ≥ v2.0.0.

## [2.3.0] — 2026-06-27

### Fixed
- **A present-but-unreadable sensor no longer spams the journal (DEC-193).** A sensor that is
  discovered but fails every read — the canonical case is an `ath12k` WiFi-radio temperature
  returning `ENETDOWN` while the radio is soft-blocked — used to emit a `WARN Failed to read
  sensor …` every tick (1 Hz) *and* a `WARN Re-discovering sensors after persistent read failures
  …` every 5 ticks forever (the read-failure→re-discovery recovery, meant for a device *unbound*
  mid-session, kept re-finding the still-present descriptor and rebuilding the streak). A new
  `SensorFailureTracker` collapses this into a bounded two-line story per sensor: it earns exactly
  one re-discovery probe, then — if still failing — is **quarantined** (logged once, suppressed
  thereafter) and recovers silently with a single info line when it reads again. A genuinely
  unbound descriptor is still dropped as before.

### Added
- **`unavailable_sensors` on `/status` + `/poll` (DEC-193).** Quarantined sensors are surfaced
  (id, label, read-error reason, and how long they have been unavailable) for display only, and
  their stale cached reading is evicted from `sensors` so a sensor that goes unreadable is never
  served at its last value. Omitted from the wire when empty (additive; `api_version` unchanged).
- **`control_eligible` on each sensor entry (DEC-193).** `false` for wireless-radio PHY temps
  (e.g. `ath12k`/`iwlwifi`), which must never drive a fan curve — they read `ENETDOWN` whenever
  the radio is down. Advisory: the GUI drops them from its curve-source picker; the engine never
  consults it and display is unaffected.

## [2.2.3] — 2026-06-27

### Fixed
- **A fan no longer stays stuck off when `step_up_pct < stop_pct` (audit P3-2, DEC-192).** On the
  stopped → on transition the per-cycle step-rate cap could hold a starting fan's output below
  `stop_pct`; the stop-snap then zeroed it and the start-threshold (gated on a positive output)
  could never fire, so the fan stayed off until the 105 °C thermal force. The start-threshold now
  judges "is the fan genuinely meant to run?" on the pre-step-rate demand and spins the fan up to
  `start_pct` whenever the curve + floor demand survives the stop threshold. Default profiles
  (`step_up_pct = 100`, `stop_pct = 0`) are byte-identical to before, so the `tuning_sequence`
  parity oracle is unchanged. Triggered only by the non-default `step_up_pct < stop_pct` combination.

### Internal
- **GPU fail-cooldown is now driven by an injectable clock (audit P3-7).** `GpuBackend` called
  `Instant::now()` directly, so the 60 s `GPU_FAIL_COOLDOWN` retry-suppression path could not be
  exercised under deterministic time. It now takes an `Arc<dyn Clock>` like the override table and
  lease manager, with a fake-clock test covering the cooldown.
- **Fewer per-request and per-tick allocations (audit EFF-1/EFF-2/EFF-4).** `/poll` and `/status`
  read the cache under a shared guard (`cache.read_with`) instead of cloning the whole `DaemonState`;
  `HistoryRing::record` only allocates a key when the entity is new; the thermal-state setter skips
  its exclusive write + `String` allocation when the value is unchanged (the engine writes it every
  tick, almost always `"normal"`).
- **The engine caches its per-activation evaluation plan (audit EFF-3).** The topological control
  order and the curve-id → index map are computed once per profile activation and reused each tick
  rather than rebuilt every second, invalidated whenever the active profile changes or the engine
  re-anchors (DEC-188 activation-epoch bump).

## [2.2.2] — 2026-06-27

### Fixed
- **Hardware verify can no longer be clobbered by an in-flight engine tick (audit P2-1).** The
  profile engine paused its write phase once per tick, before three awaited backend writes, so a
  verify starting *after* that check but during the awaits could still be overwritten. Each backend
  now re-checks the pause in-flight: hwmon refuses to adopt the verify's force-taken lease (reuses
  only its own or a thermal-safety lease, never `"verify"`), GPU re-checks per fan (it has no lease —
  DEC-045), and OpenFan re-checks per channel. Thermal-safety reuse is preserved, so there is no
  post-emergency hwmon stall. The verify handler also releases its force-taken lease on every exit
  path (RAII guard), so a cancelled or disconnected verify can no longer strand engine hwmon writes
  for the lease TTL. Effect was a rare, self-correcting false-negative verify verdict.
- **A dead OpenFan channel among healthy ones now trips the SAFETY alert (audit P3-5).** The engine's
  OpenFan write-failure counter was a single shared value that reset on *any* channel's success, so a
  persistent single-channel fault was masked. It is now a per-channel consecutive-failure streak
  (reset only by that channel's own success), with a distinct whole-link "serial link down" signal.
  Logging/alerting only — no control change.

### Changed
- **A CPU-sensor dropout during a latched thermal emergency now forces the no-sensor floor
  immediately (DEC-190).** Previously the first few cycles of such a dropout forced nothing —
  `evaluate()` cannot run without a reading and the 5-cycle no-sensor fallback had not yet tripped —
  so fans briefly fell from forced-100% to profile control while `thermal_state` still reported
  `"emergency"`. The daemon now forces `NO_SENSOR_SAFE_PCT` (40%) from the first missing cycle when an
  emergency is latched and reports `"no_sensor_fallback"` coherently. The normal-operation 5-cycle
  no-sensor debounce is unchanged (a transient blip with no emergency does not spin fans).

### Added
- **OpenFan calibration pauses the profile engine for the sweep (DEC-191).** `POST
  /fans/openfan/{ch}/calibrate` now claims the engine write-pause (sized to the whole sweep) so an
  active profile cannot overwrite each step's test PWM and corrupt the derived start/stop PWM — the
  OpenFan backend has no lease to fence it, unlike hwmon. The pause shares the verify single-flight
  slot, so a calibration and a hardware verify are mutually exclusive (`409` either way).

## [2.2.1] — 2026-06-26

### Fixed
- **Manual overrides are now scoped to the active profile (DEC-189).** A `POST /control/{id}/override`
  taken against one profile could survive into the next and pin a *same-id* control (e.g. `cpu`)
  there. Activating a profile now clears every standing control-override while holding the
  `active_profile` lock, and the override-take handler holds that same lock across its
  control-existence check and the insert — closing a check-then-act race where a concurrent
  `POST /profile/activate` could strand an override against a control absent from the now-active
  profile. Fan-identify stops are per physical fan and deliberately survive a switch. Not a thermal
  hole (overrides are floor-clamped and deadman-bounded, and the GUI already released them on a
  switch); this hardens the headless/orchestration path.

### Changed
- **Profile deactivation resets the hwmon coalescing state (audit P3-3).** `POST /profile/deactivate`
  now pairs the profile-engine lease release with `on_lease_released()` — matching the thermal
  force-take path — so a later reactivation re-asserts `pwm_enable=1` from a clean slate after the
  deactivated gap. Defense-in-depth alongside the existing per-write `pwm_enable` watchdog (the
  verify path deliberately does not reset — it restores the header value with no writer handoff).
- **GPU-fan relinquish clear is now atomic with activation (audit P3-4).** The clear of GPU fans
  relinquished to firmware-auto runs inside the `active_profile` lock, so the engine can no longer
  evaluate a freshly-activated profile and skip a still-relinquished GPU fan for one ~1 s tick.

## [2.2.0] — 2026-06-26

### Added
- **Steady-state deadband safety valve (DEC-188).** The 2°C falling-temperature deadband
  (DEC-096) now self-releases for one tick after `DEADBAND_MAX_HOLD_CYCLES` (~30 s) of holding, so a
  temperature that settles just inside the band re-anchors to its true curve value instead of
  pinning the pre-settle fan speed indefinitely. Mirrors CoolerControl's "fan speed unchanged for
  30 s → bypass hysteresis" rule: the streak counts only consecutive *held* ticks and resets the
  moment the output re-evaluates (the reading leaves the band), so the valve fires solely to release
  an output that has sat unchanged for the full window and cannot reintroduce oscillation.

### Fixed
- **Re-applying the active profile now takes effect immediately (DEC-188).** Editing the active
  profile's curve and re-activating it (same profile id) previously left fans unchanged for tens of
  seconds on an idle/stable machine — the falling-temperature deadband held the pre-edit output
  until the temperature drifted out of the band. `POST /profile/activate` now bumps an
  activation-epoch counter on `StateCache` (read by the engine under the `active_profile` lock, so
  the profile swap and the bump are observed together), re-anchoring all per-control cross-tick
  state so the new curve is applied on the very next tick. Switching to a *different* profile id
  already re-anchored; this closes the same-id gap.

### Documentation
- Cross-linked the GUI manual's new first-time-user pages — OpenFan Controller and Understanding
  Motherboard Fan Control — from the README documentation index and the USER_GUIDE intro. Docs only;
  no code or API change.

## [2.1.0] — 2026-06-19

Hardening and correctness release on top of the 2.0.0 daemon-owned-control cutover. Pairs with
`control-ofc-gui` ≥ v2.0.0.

### Fixed
- **The hard pump/CPU floor can no longer be defeated by `stop_pct` (DEC-167).** A control with a
  pump/CPU member and a non-zero `stop_pct` could be snapped to 0% despite its role floor. The
  engine now skips the stop-snap for hard-floored members, and `validate()` rejects a non-zero
  `stop_pct` on a pump/CPU control (`PUMP_STOP_FORBIDDEN`).
- **The profile engine evaluates on a fixed 1 Hz interval (DEC-168)** via `tokio::time::interval`
  (`MissedTickBehavior::Skip`) instead of `sleep`-after-work, removing period drift, plus a
  shutdown mid-tick guard.

### Changed
- **Retired the lease capability surface (DEC-170).** `/capabilities` no longer advertises
  `lease_required` / `lease_required_for_hwmon_writes`; verify-effectiveness failures map to
  `503 hardware_unavailable` (`403 lease_required` / `409 lease_already_held` are emitted by no
  route). `/status` drops the dead `counters` / `last_error_summary` envelope.

### Security
- **Hardened profile-id validation and on-disk confidentiality (DEC-173).** `is_safe_profile_id`
  now rejects ids over 128 bytes or containing control characters (a clean `400` instead of a
  filesystem `500`); the profile-store, state, and runtime-config directories are created `0o700`
  (owner-only); and `500`/`503` error responses no longer leak internal filesystem paths.

### Build
- **Added a `cargo-deny` license/advisory gate (DEC-174).** `deny.toml` encodes the project's
  license policy (DEC-043 no-LGPL, DEC-155 serialport MPL-2.0); `cargo deny check` runs at release
  time alongside `cargo audit`.

## [2.0.0] — 2026-06-19

**Breaking — daemon-owned control cutover (DEC-159 / DEC-165).** The daemon's profile engine is now the
**sole writer** of every fan backend (OpenFan, hwmon, GPU PMFW). The 30 s `gui_active` defer window is
deleted — the daemon no longer steps aside for a GUI writer, because there is no GUI writer. The paired
`control-ofc-gui` ≥ v2.0.0 is an editor/viewer/controller-of-intent that never writes PWM. Pairs with
`control-ofc-gui` ≥ v2.0.0.

### Changed (breaking)
- **Engine is primary.** `profile_engine` evaluates the active profile and writes every tick with no
  deferral; the `gui_active` / `record_gui_write` machinery (retiring DEC-071 / DEC-074 / DEC-093) is
  removed.
- **`GET /capabilities`** gains `control.autonomous_control = true` and `control.min_supported_gui =
  "2.0.0"`, so a 2.0 GUI can detect a daemon that has actually flipped (1.19–1.21 advertised the
  `control` block while still deferring — block presence is not the discriminator).

### Removed (breaking)
- Bare PWM write endpoints: `POST /fans/openfan/{ch}/pwm`, `POST /fans/openfan/pwm`,
  `POST /fans/openfan/{ch}/target_rpm`, `POST /hwmon/{id}/pwm`, `POST /gpu/{id}/fan/pwm`.
- The hwmon lease API: `POST /hwmon/lease/take` / `release` / `renew` and `GET /hwmon/lease/status`.
  The engine now holds the hwmon lease internally; `POST /hwmon/{id}/verify` runs under that internal
  lease and no longer accepts a `lease_id`.

### Safety
- The 105 / 80 / 60 °C thermal ladder is unchanged and remains the absolute backstop; GPU fans stay
  excluded (DEC-130). The role-aware pump/CPU floor is enforced by the engine every tick (DEC-162), and
  manual overrides (DEC-163) are floor-clamped with a daemon-clock deadman.

### Packaging
- PKGBUILD gains `conflicts=('control-ofc-gui<2.0.0')` to refuse a partial upgrade — the 2.0 GUI cannot
  control fans against a pre-2.0 daemon.

## [1.21.0] — 2026-06-18

Daemon-owned **manual-override + fan-identify API** (DEC-163 / DEC-166) — Phase 4 of the GUI→daemon
control migration. Additive and backward-compatible: the daemon gains an expiring, fencing-guarded
override subsystem, but it is **dormant until the 2.0.0 cutover** — the current GUI does not call these
endpoints, and while the GUI is the active writer the engine defers, so the override overlay is computed
but never written to hardware. **No runtime behaviour change.** Pairs with `control-ofc-gui` ≥ v1.39.0.

### Added
- **Manual override API** — `POST /control/{id}/override` pins a control's members to a fixed PWM;
  `POST /control/{id}/override/renew` extends it; `DELETE /control/{id}/override` reverts immediately.
  Each grant is a renewable, **expiring lease** (15 s TTL, renew ~5 s) judged on the daemon's own
  monotonic clock, so a frozen/crashed/slept GUI cannot strand fans — the override **fails safe to
  autonomous curve control** on expiry. A single monotonically increasing `override_token` is both the
  grant identity and the fencing token, so a thawed GUI holding a stale token cannot silently re-pin
  fans (Kleppmann fencing; the daemon is both lock service and resource).
- **Fan-identify API** — `POST /fans/{fan_id}/identify` (`action: "stop" | "restore"`) stops or restores
  a single fan for physical identification, **auto-restoring** after a short deadman TTL.
- **`/capabilities`** — `control.manual_override` and `control.fan_identify` now advertise `true`.
- **`/status`** — surfaces active overrides and fan-identify holds with their remaining TTL (each array
  is omitted when empty, so the common-case wire shape is unchanged).
- **Error codes** — `stale_fencing_token` (HTTP 409) and `override_expired` (HTTP 404) within the
  existing error envelope.

### Safety
- Strict precedence is preserved: **105 °C thermal force > identify-stop (floor-exempt) > override
  (floor-clamped) > curve**. An override's PWM is still clamped by the hard pump/CPU floor (~30 %,
  DEC-162) and the GPU 0 % floor (DEC-119) — a stuck or fat-fingered override can never strand a pump
  or CPU below its safety minimum; identify-stop is deliberately floor-exempt (you must be able to stop
  a pump to physically find it) and bounded by its deadman.
- Expiry is swept on the daemon's monotonic clock every tick with **no client cooperation**; the
  per-grant TTL is capped (a client extends an override by renewing, never by one long grant). There is
  no absolute max-duration cap — a live renewing GUI proves the user is present, and the 105 °C force
  remains the ultimate backstop.

### Notes
- No schema bump (still v7). The override/identify table is in-memory and intentionally dropped on
  daemon restart (fail-safe to curve). The override handlers do **not** mark the engine `gui_active`.
- The monotonic `Clock` seam used by the hwmon lease was promoted to a shared module and reused by the
  override deadman.

## [1.20.0] — 2026-06-17

Daemon-enforced **role-aware minimum-PWM floor backstop** (DEC-162) — Phase 3 of the GUI→daemon
control migration. Additive and backward-compatible: the pump/CPU floor is now independently validated
and enforced by the daemon, but the behaviour is **dormant until the 2.0.0 cutover** (it is reached
only via the profile CRUD/activate path the current GUI does not call, and the eval-time clamp only
emits while the daemon engine is the writer). **No runtime behaviour change.** Pairs with
`control-ofc-gui` ≥ v1.39.0.

### Added
- **Role-floor validation backstop** — `DaemonProfile::validate()` now rejects a profile whose control
  has a pump/CPU member declaring `minimum_pct` below the 30 % hard pump floor, as a `FLOOR_TOO_LOW`
  field violation (HTTP 400). The daemon classifies pump/CPU members **independently** (document-only:
  a `cpu`/`pump`/`aio` label hint, or a known liquid-cooler chip embedded in the member id) rather than
  trusting the GUI-stamped number — defense-in-depth for a safety-critical value.
- **Eval-time floor clamp** — the profile engine independently raises a pump/CPU member to at least the
  30 % floor on every tick regardless of the declared `minimum_pct`, generalising the existing DEC-119
  per-member GPU flooring into one effective-floor rule. This protects a profile that reaches the
  engine **un-validated** (loaded at boot via `resolve_initial_profile`, or hand-edited on disk), so a
  too-low floor can never strand a pump.

### Notes
- Pump/CPU only; GPU members stay floored at 0 % (DEC-119 — PMFW owns the minimum); chassis fans keep
  the GUI-baked advisory floor. The per-control `stop_pct` still takes precedence over the floor
  (unchanged tuning-pipeline order), and the 105 °C thermal force still overrides everything.
- No schema bump (still v7); no new endpoint — `FLOOR_TOO_LOW` is a new `reason` within the existing
  `field_violations`. GUI↔daemon role classification is pinned byte-for-byte by a shared
  `role_classification.json` test fixture.

## [1.19.0] — 2026-06-17

Daemon-owned **profile storage + CRUD/validation API** — Phase 1 of the GUI→daemon control
migration (DEC-160). Additive and backward-compatible: the daemon becomes the store of record for
profiles but still defers to the GUI's control loop, so there is **no runtime behaviour change**.
Pairs with the existing **GUI v1.41.0** — the GUI does not yet use these endpoints.

### Added
- **Profile CRUD API** — `GET /profiles`, `GET /profiles/{id}`, `POST /profiles`,
  `PUT /profiles/{id}`, `DELETE /profiles/{id}`. `POST`/`PUT` accept `?validate_only=true` to
  validate without persisting. Profiles are stored as `{state_dir}/profiles/{id}.json`
  (`/var/lib/control-ofc/profiles/`) — the daemon-owned store, prepended as the primary profile
  search dir so a stored profile is activatable by id and shadows a same-id read-only preset.
- **`DaemonProfile::validate()`** — structural + intra-profile referential validation returning
  hard `errors` and soft `warnings`. Hard errors: non-finite numbers, out-of-range percentages,
  >256 curve points, trigger idle ≥ load, dangling `curve_id`/`mix_curve_ids`/`sync_control_id`,
  and Mix/Sync dependency cycles. An unknown `sensor_id` is a **warning, not an error**, so a
  profile authored on another machine still stores, validates, and activates (the engine tolerates
  a missing sensor at eval time, and the 105 °C thermal force backstops).
- **`control` capability block** on `GET /capabilities`
  (`{profile_storage, curve_evaluation, manual_override, fan_identify, min_supported_gui}`) so a
  client can detect daemon-owned-control support. `profile_storage` and `curve_evaluation` are
  `true`; the rest are reserved for later migration phases.
- Structured **`field_violations`** in the error-envelope `details` (additive), plus error codes
  `already_exists` (409, duplicate create) and `profile_in_use` (409, deleting the active profile).

### Changed
- `POST /profile/activate` now **validates** the loaded profile and rejects a hard-invalid one,
  leaving the previously active profile running.

### Notes
- The store persists the uploaded profile document verbatim (lossless / forward-compatible) — it
  does not re-serialise the daemon model. Writes reuse the crash-safe atomic writer (0600).
- `PUT /profiles/{id}` updates stored desired-state only; it does not hot-reload a running active
  profile — re-activate to apply.
- No profile-schema bump (still v7); no systemd/packaging change (the store lives under the
  existing `StateDirectory`).

## [1.18.0] — 2026-06-16

First-class **liquid-cooler (AIO) support — Phase 1** (hwmon-only). Pairs with **GUI v1.39.0**
(DEC-156).

### Added
- **`SensorKind::CoolantTemp`** (serialised `"coolant_temp"`) and a centralized `hwmon::aio`
  module that recognises NZXT Kraken (`x53`/`z53`/`kraken2023`/`kraken2023elite`/`kraken2`) and
  Aquacomputer (`d5next`/`highflownext`/`leakshield`) coolers, classifying coolant by cooler chip
  or `coolant`/`water`/`liquid` label.
- **`is_aio`** flag on each PWM header (`GET /hwmon/headers`), so the GUI can cluster and floor
  pumps without re-deriving hardware knowledge.
- **Dynamic `aio_hwmon` capability** — `{present, status, pump_writable, coolant_available}`
  (additive superset of the old `{present, status}`); `aio_usb` stays `unsupported` (USB-only
  coolers are out of scope — the daemon never opens USB-HID).

### Changed
- The poll loop now populates `AioPumpState` + the `aio` subsystem freshness timestamp when a
  coolant sensor is present (wires the previously-dead `cache.update_aio()`).

### Notes
- **No coolant safety rule** — `safety.rs` is unchanged (CPU-only); pump writability rides the
  existing per-channel `is_writable` permission bit (the kernel exposes a writable `pwmN` only for
  controllable channels). No profile-schema bump.

## [1.17.3] — 2026-06-12

Cleanups from the 2026-06-12 code audit. Daemon-only; pairs with the existing **GUI v1.38.0**.

### Changed
- **`chip_name` is non-optional in the sensors API (P2-D).** Hwmon/GPU discovery always yields
  a chip name (or skips the device), so the response field is now `String` instead of
  `Option<String>` — the wire format is unchanged (it was always present).

### Fixed
- **Deterministic lease-expiry tests (P2-F).** `LeaseManager` accepts an injectable clock; the
  expiry/renewal tests advance a fake clock instead of `thread::sleep`, removing
  CI-load-dependent flakiness. No production behaviour change (production uses the real clock).

### Other
- Added `NOTICE.md` acknowledging the MPL-2.0 `serialport` dependency (P2-H / DEC-155).

## [1.17.2] — 2026-06-12

Concurrency fix (from the 2026-06-12 code audit). Daemon-only — pairs with the existing
**GUI v1.38.0**.

### Changed
- **Profile-engine hwmon writes lock per command, not per batch.** `HwmonBackend::apply`
  previously held the controller mutex across the whole multi-header tick (lease-acquire + every
  `set_pwm` + renew), starving concurrent API requests (`GET /hwmon/headers`, lease ops) for the
  duration of the batch. It now locks once per header — matching the sibling `force_all` and
  OpenFan paths (DEC-099) — so requests interleave between writes. A force-take mid-tick fails the
  remaining writes with `InvalidLease` and the next 1 Hz tick re-applies (audit P1-D / DEC-154).

## [1.17.1] — 2026-06-12

Shutdown-ordering and thermal-safety hardening (from the 2026-06-12 code audit).
Daemon-only — pairs with the existing **GUI v1.38.0**.

### Fixed
- **Graceful shutdown stops the IPC server before restoring hardware.** Shutdown now stops
  accepting IPC connections and drains in-flight requests, then drains the poll/engine tasks,
  and only then restores fans to automatic (`pwm_enable=2`) — so a late client write can no
  longer re-enter manual mode after the restore. Each wait is timeout-bounded, so a lingering
  connection can never block the safety restore (extends DEC-146; audit P1-A).

### Changed
- **Thermal safety re-asserts manual mode on force-take.** When thermal safety force-takes the
  hwmon lease it now resets the per-header write-coalescing state, so it unconditionally
  re-writes `pwm_enable=1` on its first forced write — defense in depth alongside the existing
  per-write readback watchdog (audit P1-E).

## [1.17.0] — 2026-06-12

Mix and Sync composite curve types (DEC-150/151) — the final phase of the
curve-library expansion, retiring the single-sensor rule (DEC-014 → DEC-152).
Pairs with **GUI v1.38.0**; both evaluators learn dependency-graph evaluation with
cycle detection in lockstep and the DEC-126 parity fixture stays byte-identical.

### Added
- `"mix"` curve type — combines other curves' raw outputs (each at its own sensor)
  via `max`/`min`/`average`/`sum`/`subtract`, clamped 0–100. Evaluated by a
  recursive `resolve_curve_output` + `combine_mix` with a path-based visited-set
  cycle guard. Bypasses the 2°C deadband. Mirrors the GUI's `_resolve_curve_output`
  / `_combine_mix`.
- `"sync"` curve type — mirrors another control's current-tick tuned output +
  offset. `evaluate_profile` now evaluates controls in a stable topological order
  (`topological_control_order`, byte-identical to the GUI's `_ordered_controls`)
  and reads the target from a new per-tick `tick_outputs` map (not the
  previous-tick, step-rate-entangled `last_output`). Bypasses the deadband; the
  ordering is a no-op for Sync-free profiles.
- Mix/Sync parity vectors (multi-sensor Mix, mirror-before-target Sync) plus
  profile-engine unit tests for the combine functions, topological order, and
  cycle fallback.

### Changed
- Profile `default_version` → **7**. New optional `CurveConfig` fields
  (`mix_function`, `mix_curve_ids`, `sync_control_id`, `sync_offset_pct`).
  Additive and backward-compatible (unknown curve types still fall back to 50%).

## [1.16.0] — 2026-06-12

Trigger (two-state latch) curve type (DEC-149) — second of three phased
curve-library additions. Pairs with **GUI v1.37.0**; both evaluators learn the
stateful latch in lockstep and the DEC-126 parity fixture stays byte-identical.

### Added
- `"trigger"` curve type. The pure `evaluate_curve` returns the cold-start value
  (load at/above the load temp, else idle); the latch lives in the profile engine
  (`evaluate_trigger` + `ProfileEngineState::trigger_latch`), which **bypasses the
  2°C deadband** since the trigger owns its idle..load hysteresis. Mirrors the
  GUI's `_evaluate_trigger`; pinned by a new `tuning_sequence` parity vector.

### Changed
- Profile `default_version` → **6**. `CurveConfig` now derives `Default` to absorb
  new curve-type fields cleanly. Additive and backward-compatible.

## [1.15.0] — 2026-06-12

Stepped (staircase) curve type (DEC-148) — first of three phased curve-library
additions. Pairs with **GUI v1.36.0**; both evaluators learn `stepped` in
lockstep and the DEC-126 parity fixture stays byte-identical.

### Added
- `evaluate_stepped` in `profile.rs`: a `"stepped"` curve holds each point's
  output until the next point's temperature is reached (lower-point-wins,
  half-open segments), clamping below-first / at-or-above-last, empty → 50%.
  Mirrors the GUI's `_interpolate_stepped`; pinned by new `curve_eval` parity
  vectors.

### Changed
- Profile `default_version` → **5**. Additive and backward-compatible: older
  profiles deserialise unchanged and unknown curve types still fall back to 50%.

## [1.14.2] — 2026-06-07

2026-06 function/efficiency audit remediation (DEC-146 P2/P3) plus
release-workflow hardening. Pairs with **GUI v1.34.0**. Hot-path
efficiency, deterministic sensor wire ordering, blocking-pool backend
writes, and joined shutdown — no wire-shape, control-loop-semantics, or
safety-path changes.

### Changed
- Release workflow: `actions/checkout` bumped v4 → v6 (Node 24, ahead of
  GitHub's 2026-06-16 forced default), and all release-workflow actions
  are now pinned to full commit SHAs with version comments (immutable
  supply-chain posture; the AUR deploy action holds the publishing key).
- Profile-engine hot path (DEC-146 P2): curve sensor lookup is now O(1)
  against the id-keyed map (was a per-control linear scan every tick),
  and one sensors snapshot per tick is shared by the thermal-safety leg
  and curve evaluation (was two full map clones per second — and the two
  legs could observe different snapshots within a single tick).
- OpenFan poll no longer clones the entire daemon state every second to
  preserve `last_commanded_pwm` — the cache preserves it on update,
  mirroring the GPU-fan path, with a regression test (DEC-146 P2).
- hwmon PWM-write/verify handlers look headers up O(1) via the new
  `header(id)` accessor instead of building and sorting the full header
  Vec per request, and the descriptor→wire mapping is a single
  `From<&PwmHeaderDescriptor>` impl (was duplicated field-for-field in
  two handlers) (DEC-146 P2).
- Profile-engine OpenFan/hwmon writes (and both thermal-safety
  `force_all` paths) now run on the blocking pool via `spawn_blocking`,
  matching the GPU backend and both poll loops (DEC-146 P3-8) — a
  thermal-emergency serial sweep could previously pin a tokio worker for
  up to 10 × 500 ms. Lock-per-command (DEC-099) semantics unchanged.

### Fixed
- Graceful (non-systemd) shutdown joins the poll and profile-engine
  tasks — timeout-bounded — BEFORE restoring GPU/hwmon fans to automatic
  (DEC-146 P3-9), closing the race where an in-flight engine write could
  land after the restore and leave hardware in manual mode at process
  exit. Production systemd runs were already covered by ExecStopPost.
- `/sensors`, `/poll`, and SSE sensor arrays are now actually sorted by
  id, as `build_sensor_entries`' doc comment always claimed —
  deterministic wire order across restarts/rescans, sparing the GUI
  sensor panel spurious rebuilds (DEC-146 P2).

### Fixed
- aur-publish: ship an AUR-side `.gitignore` (tarballs, `src/`, `pkg/`) via
  the deploy action's `assets` input. With `assets` set the action stages
  `git add --all`, so updpkgsums' downloaded source tarball was committed
  to the AUR repo (the v1.14.1 tarball slipped under aurweb's ~488 KiB
  blob cap; the GUI's 2 MB tarball was rejected outright). The
  `.gitignore` keeps source blobs out of AUR commits from the next
  release onward.

## [1.14.1] — 2026-06-07

DEC-145 guidance pass. Pairs with **GUI v1.33.0**. Documentation and
packaging text only — no code, wire-shape, control-loop, lease, or
safety-path changes.

### Added
- README Prerequisites: **UEFI Secure Boot row** — unsigned `*-dkms-git`
  modules build but fail to load under Secure Boot (`Key was rejected by
  service`); links the GUI manual's new Driver Setup § Secure Boot
  walkthrough (detection, disable-vs-sign, CachyOS IMA caveat).
- README Prerequisites: explicit informational / as-is / at-your-own-risk /
  no-liability note — the first user-facing risk language daemon-side
  (previously only the MIT LICENSE carried it).

### Changed
- **sensors-detect stance unified with the GUI (DEC-145):** USER_GUIDE,
  the modules-load.d comment, and the lm_sensors optdepends now present
  `sensors-detect` as a last resort behind the GUI readiness report, with
  the sensors-detect(8) risk quote ("SMBus lockup to permanent hardware
  damage") and the dual-chip Gigabyte warning.
- README coexistence note now names fan2go and instructs stopping other
  fan controllers before the daemon drives the same headers (PWM sysfs is
  single-writer; last writer wins), linking the GUI Setup Checklist.

### Fixed
- Stale GUI path "Diagnostics → Fans → Hardware Readiness" in README (×2)
  and the post-install message — the readiness report lives at
  **Diagnostics → Troubleshooting** since GUI v1.26.0 (DEC-124).
- aur-publish now ships `packaging/*.install` via the deploy action's
  `assets` input — post-install message fixes previously never reached the
  AUR (only PKGBUILD + .SRCINFO were pushed).

## [1.14.0] — 2026-06-07

2026-Q2 it87/SIO knowledgebase refresh (DEC-144) + the DEC-143 supply-chain
integrity change. Pairs with **GUI v1.32.0**. Diagnostics data tables and
packaging only — no wire-shape, control-loop, lease, or safety-path changes.

### Added
- **Dual-chip board coverage (DEC-144):** `GIGABYTE_DUAL_CHIP_BOARDS` gains
  **X870E AORUS ELITE** (it8696+it87952 — owner-confirmed in
  frankcrawford/it87 #89; the substring also covers the 2026 ELITE X3D
  refresh) and **X670 AORUS ELITE AX** (it8689+it87952 — driver DMI table
  annotation). The X870 AORUS ELITE WIFI7 ICE single-chip ambiguity is
  documented in-code (no behavioural change; exact-match lookup deferred
  pending owner evidence).

### Fixed
- **`chip_driver_in_mainline` reports IT8622E as mainline (DEC-144)** — the
  chip is in the mainline it87 `enum chips` (verified against
  torvalds/linux v6.17) but was missing from the list, so the diagnostics
  chips table falsely told IT8622E owners they needed the DKMS build.
  `it8689` deliberately stays out-of-tree (mainline 7.1 support is
  sensors-only and unreleased; Gigabyte fan control still needs the DKMS
  MMIO path) — guarded by an intent-lock test.

### Changed
- **README dual-chip prerequisite row (DEC-144):** "update `it87-dkms-git`"
  is now the headline remediation — 2026-03+ upstream builds default
  `mmio=on` (PR #95) and merge the ISA-bridge MMIO/H2RAM path (PR #102), so
  current builds enumerate *and control* the secondary chip by default;
  `mmio=on` is the pre-2026-03 fallback. Counter-case: IT8665E boards need
  `mmio=off` (frankcrawford/it87 #106 regression).
- **Supply-chain integrity: `Cargo.lock` is now committed** and the PKGBUILD
  fetches `--locked`, so the dependency set `cargo audit` scans at release
  time is exactly the set the clean-room CI build compiles and ships.
  Previously the lock was gitignored, the tag tarball contained none, and the
  AUR build freshly re-resolved every dependency at build time — the audited
  set and the shipped set could differ, and builds were not reproducible.
  AUR delivery starts with this release: CI regenerates the published
  PKGBUILD from `packaging/PKGBUILD` at tag push (`release.yml` aur-publish),
  and earlier tag tarballs contain no lock for `--locked` to apply to.
  No runtime behaviour change. (DEC-143)

## [1.13.0] — 2026-06-06

2026-06-05 audit remediation. Pairs with **GUI v1.30.0**. Additive wire-contract
change (`thermal_state` in `GET /status`) — older GUIs ignore the new field.

### Added
- **`thermal_state` in `GET /status`** (`"normal" | "recovery" | "emergency" |
  "no_sensor_fallback"`) — surfaces the profile engine's thermal override state
  (previously visible only via `/diagnostics/hardware`) so the GUI can stand
  its control loop down while the daemon forces safety PWM. `API_VERSION`
  unchanged. (DEC-132)
- **Sensor descriptor cache.** The hwmon polling loop discovers sensors once
  and re-reads only `temp*_input` per tick (~25 sysfs ops/s instead of ~340 on
  a typical board). Re-discovery runs on `POST /hwmon/rescan`, on a 5-tick
  read-failure streak (device unbound), and on every tick while no CpuTemp
  sensor is cached (late `k10temp`/`coretemp` modprobe keeps the 40% fallback
  releasable). Kindest to `asus_wmi_sensors` boards, whose kernel doc warns
  against frequent WMI polling. `/hwmon/rescan` now also refreshes the cached
  sensor descriptors (labels, types, DEC-117 threshold snapshot). (DEC-133)

### Fixed
- **GPU GUI-priority arbitration.** The 5%-coalesced early return in
  `POST /gpu/{id}/fan/pwm` now records GUI liveness, and the profile engine's
  GPU write suppression uses the shared 5% threshold (`GPU_COALESCE_DELTA_PCT`)
  instead of exact-match. Previously a slow temperature ramp (1–4% deltas) let
  `gui_active()` lapse mid-session and the engine then committed a full PMFW
  curve (an SMU transaction) on every 1% change while the GUI believed it was
  in control. (DEC-131)
- **Calibration restores on every exit path.** The calibrate handler now
  delegates to the single tested sweep implementation, which restores the
  pre-calibration PWM on success, thermal abort, **and** failed PWM writes
  mid-sweep — previously a write failure returned early and could park a fan
  at a sweep step (including 0%). (DEC-134)
- **Forced overrides reset engine tuning state.** Post-emergency evaluation
  starts from a fresh anchor instead of step-rate-clamping against the
  pre-emergency `last_output` the hardware no longer holds.

### Changed
- **Docs/comments/logs no longer claim GPU fans are forced during thermal
  emergencies.** The 105°C emergency and 40% no-sensor fallback force all
  OpenFan channels and writable hwmon headers only; GPU fans are deliberately
  excluded — AMD PMFW firmware owns GPU thermal protection (junction-temp
  throttling, firmware fan ramp) independently of OS fan control. There is no
  GPU emergency threshold. (DEC-130)

### Internal
- **Profile engine decomposed** (DEC-135): pure `evaluate_safety_tick` (unit
  tests cover the full 105/80/60/no-sensor ladder) + a `WriteBackend` trait
  with `OpenFanBackend`/`GpuBackend`/`HwmonBackend` owning all per-backend
  gating. `SafetyWriteBackend` is implemented only by OpenFan/hwmon, making
  the DEC-130 GPU exclusion structural. Behaviour-preserving — all
  pre-existing loop tests pass unchanged; 28 tests added across the audit
  fixes (daemon total 593).

## [1.12.2] — 2026-06-05

### Fixed
- **Graph-curve fallthrough aligned with the GUI.** `evaluate_graph` returned a
  hardcoded `50%` for the (effectively unreachable) non-monotonic fallthrough
  case; it now returns the last point's output, matching the GUI's
  `_interpolate_graph`. Defensive — no behaviour change for any editor-authored
  profile. (DEC-126)

### Internal
- **Cross-stack evaluator parity harness.** Added `daemon/tests/fixtures/parity_vectors.json`
  (byte-identical to the GUI's canonical copy) and two `profile_engine.rs` tests
  that assert the same hand-authored oracle the GUI checks — pinning headless
  evaluation (curve interpolation + deadband/step/start-stop/mixed-GPU tuning) to
  GUI-driven evaluation. (DEC-126)

## [1.12.1] — 2026-06-05

### Internal
- Deduplicated the verify-wait constant. The hwmon and GPU verify endpoints now
  share a single `constants::VERIFY_WAIT_SECONDS` (6 s) — `api/handlers/hwmon_ctl.rs`
  aliases it instead of defining a parallel `6` literal, and `gpu.rs` references
  the same constant — so the two settle windows can no longer silently drift
  apart (which would have violated the GUI's `VERIFY_PAUSE_SAFETY_MS` ≥9 s
  headroom). Behaviour is unchanged (still 6 s); the compile-time
  `assert!(VERIFY_WAIT_SECONDS >= 4)` floor is retained. (DEC-101)

## [1.12.0] — 2026-06-04

Intel discrete GPU (Arc) monitoring (**DEC-121**, daemon-relevant subset). Pairs
with **GUI v1.24.0**. Additive wire-contract change — older GUIs ignore the new
fields.

### Added
- **Intel discrete GPU detection** (`hwmon::intel_gpu_detect`) for the `xe`
  (Battlemage / Arc B-series) and `i915` (Alchemist / Arc A-series) drivers.
  Detection keys on the hwmon chip name, which both drivers register **only for
  discrete GPUs** — an unambiguous discrete-GPU signal.
- New sensor `source: "intel_gpu"` (kind `gpu_temp`) for `xe`/`i915` temps;
  `SensorSource::IntelGpu` / `DeviceLabel::IntelGpu`.
- New read-only GPU fan entities `intel_gpu:{pci_bdf}` in `GET /fans` (RPM from
  `fan1_input`; never a commanded PWM).
- Additive `intel_gpu` object on `GET /capabilities` and an `intel_gpu` block on
  `GET /diagnostics/hardware` (with a truthful firmware-managed explanation).

### Notes
- **Read-only by design.** The kernel `xe`/`i915` hwmon interface exposes fan
  RPM and temperature only — there is no PWM/write path (fan control is
  firmware-managed). `fan_control_method` is always `"read_only"` or `"none"`;
  no lease, PMFW curve, ppfeaturemask, overdrive, or shutdown reset apply.
- Only device ID `0xE20B` (Arc B580) maps to a marketing name; other IDs report
  the generic "Intel D-GPU".

## [1.11.0] — 2026-06-04

GPU fan active verification endpoint (**DEC-120**, daemon-relevant subset).
Pairs with **GUI v1.23.0**. Additive wire-contract change — older GUIs ignore
the new route.

### Added
- `POST /gpu/{gpu_id}/fan/verify` (no lease) — drives a test speed biased
  upward (so cooling is never reduced on a hot GPU), waits
  `GPU_VERIFY_WAIT_SECONDS = 6 s`, reads back the applied PMFW `fan_curve` (or
  legacy `pwm1`) plus `fan1_input` RPM and `fan_zero_rpm_enable`, restores the
  prior state, and classifies the outcome: `effective`, `curve_not_applied`,
  `no_rpm_effect`, `zero_rpm_suppressed`, `rpm_unavailable`, `write_failed`, or
  (legacy path) `pwm_enable_reverted`. Catches the silent GPU failures static
  diagnostics miss (`ppfeaturemask` bit 14 unset, SMU mismatch, BIOS overdrive
  lock) without flagging a healthy zero-RPM idle or the OD_RANGE clamp as a
  fault.
- `hwmon::gpu_fan` helpers `flat_speed_pct`, `parse_zero_rpm_enabled` /
  `read_zero_rpm_enabled`; constant `GPU_VERIFY_WAIT_SECONDS`.

## [1.10.0] — 2026-06-04

GPU detection/diagnostics hardening + headless per-member GPU floor
(**DEC-119**, daemon-relevant subset). Pairs with **GUI v1.22.0**. The
diagnostics additions are an additive wire-contract change — older GUIs ignore
the new keys.

### Changed
- **The profile engine no longer soft-floors GPU fans in a mixed control.**
  When a profile runs headless, a GPU member grouped with chassis/CPU fans now
  idles to its own 0% floor while the non-GPU members keep the control's
  `minimum_pct` — the same per-member flooring the GUI does, so headless and
  GUI-driven evaluation stay consistent (DEC-096). The PMFW write path still
  clamps to the firmware OD_RANGE (~15%) and honours `fan_zero_rpm`.

### Added
- **`/diagnostics/hardware` now reports AMD GPUs that exist in PCI space but
  have no `amdgpu` driver bound.** New top-level `amd_pci_devices` (per-device
  `pci_bdf` / `pci_device_id` / `driver` / `amdgpu_bound` / `hwmon_present`)
  scans `/sys/bus/pci/devices` independently of hwmon, so a blacklisted /
  KMS-failed / vfio-pci-passed-through GPU — which produces no hwmon node and
  was previously invisible — is now surfaced. New top-level
  `amdgpu_module_loaded` (`/sys/module/amdgpu`) distinguishes "module not
  loaded" from "loaded but unbound".
- **`GpuDiagnostics` gains firmware-context fields:** `fan_speed_min_pct` /
  `fan_speed_max_pct` (the PMFW `fan_curve` `OD_RANGE` fan-speed bounds — the
  firmware-enforced ~15% minimum), `fan_minimum_pwm` (best-effort parse of
  `gpu_od/fan_ctrl/fan_minimum_pwm`), `amdgpu_driver_bound`, and
  `kernel_warnings` (the same advisory catalogue as
  `/capabilities.amd_gpu.kernel_warnings`, duplicated so the support bundle is
  self-contained).

All new fields use `#[serde(default)]` / `skip_serializing_if`, so the change
is additive and non-breaking. No new dependencies; the daemon never writes
`fan_minimum_pwm`.

## [1.9.0] — 2026-06-03

Curated hwmon temperature-threshold attributes on `SensorEntry` (**DEC-117**).
Pairs with **GUI v1.20.0**, which surfaces the new fields in the
Diagnostics > Sensors detail dialog and the inline alarm chip. Additive
wire-contract change — older GUIs ignore the new key.

### Added
- **`thresholds: Option<SensorThresholdsResponse>` on every `SensorEntry`**
  in `/sensors` and `/poll`. The daemon reads a curated subset of
  hwmon temperature-threshold sysfs attributes once at discovery time and
  on every `POST /hwmon/rescan`:
  - `tempN_max`, `tempN_min`, `tempN_crit`, `tempN_crit_hyst`,
    `tempN_emergency`, `tempN_emergency_hyst`, `tempN_lcrit`,
    `tempN_offset` — temperature thresholds in °C.
  - `tempN_alarm`, `tempN_max_alarm`, `tempN_crit_alarm`,
    `tempN_fault` — alarm/fault bits (sampled at discovery, not refreshed
    per poll cycle).
- **Daemon-side plausibility filter** for threshold values. Anything
  outside `[-50, 200] °C` is dropped (catches kernel INT_MIN/INT_MAX
  placeholders), and the `it87`-family `tempN_max == 0` "register not
  configured" sentinel is dropped specifically for `it8*` chips. Every
  threshold sub-field uses `#[serde(skip_serializing_if = "Option::is_none")]`
  so the wire shape is the minimal honest set — a sensor with only `crit`
  configured emits `{"thresholds": {"crit_c": 105.0}}`, not 12 null fields.
- New `SensorThresholds` struct in `hwmon::types` + serialisation twin
  `SensorThresholdsResponse` in `api::responses`. The values propagate
  through `SensorReading` → `polling::to_cached` → `CachedSensorReading`
  → `build_sensor_entries` → `SensorEntry`.

### Tested
- 6 new unit tests in `hwmon::discovery` covering the curated attribute
  set, k10temp-empty handling, garbage-value filtering, the `it87`
  `max=0` quirk (and that the filter is scoped to `it87`-family chips
  only), alarm-bit reading, and graceful handling of a malformed alarm
  bit. 1 new schema test in `api::responses` verifying the JSON shape
  with and without thresholds.
- All 458 daemon unit tests pass.

## [1.8.4] — 2026-06-03

Internal efficiency and code-health fixes from a full cross-stack audit. No
behaviour or wire-contract changes; pairs with **GUI v1.19.1**.

### Changed
- **`/diagnostics/hardware` no longer blocks the async runtime.** The handler's
  ~6 blocking sysfs/procfs reads (`/proc/modules`, `/proc/ioports`, DMI,
  `/proc/cpuinfo`, kmsg, `ppfeaturemask`) now run on `spawn_blocking`, mirroring
  the OpenFan write handlers (DEC-099), so a slow read can't stall a Tokio worker.
- **Profile-engine 1 Hz loop trims per-tick state clones.** Added cheap
  `StateCache::gui_active()` / `sensors_snapshot()` / `gpu_fans_snapshot()`
  accessors; the GUI-active + GPU-write-suppression site no longer deep-clones
  the full sensor map every tick just to read a bool and the GPU-fan map.
- **`take_resume_flag()` is now the single resume-flag consumer.** The hwmon PWM
  controller calls it instead of an inline `resume_detected.swap()`, removing the
  duplicated atomic incantation.

### Tested
- New unit tests for the three `StateCache` accessors and `take_resume_flag`'s
  swap-and-clear semantics, plus an end-to-end IPC test for
  `GET /diagnostics/hardware`.

## [1.8.3] — 2026-06-02

Kernel-warning catalogue correctness fix (**DEC-114**), from a cross-repo
documentation audit that re-verified every externally-sourced hardware
claim against primary sources. Safety-relevant: the RDNA3/RDNA4 hard-hang
warning was too narrow and recommended an also-affected kernel. Pairs with
**GUI v1.19.0** (which bundles the never-separately-released v1.18.1), landing
the matching guidance text, a device-ID fix, and the doc corrections + citations.

### Fixed
- **RDNA3/RDNA4 hard-hang warning now covers kernel 6.18 _and_ 6.19.** The
  regression affects both series (Phoronix EOY 2025; ROCm #6101 reports
  kernel panics on 6.18.20 and 6.19.10), but `detect_kernel_warnings` only
  fired on 6.19.x and the message recommended "roll back to 6.18 LTS" — an
  also-affected kernel. It now fires on 6.18.x and 6.19.x and recommends a
  verified-safe **6.15–6.17** longterm kernel. The id was renamed
  `rdna_hang_kernel_6_19_x` → `rdna_hang_kernel_6_18_6_19` so the GUI
  re-prompts users who acknowledged the earlier, unsafe advice.
- **R9700 SMU-mismatch warning re-characterised and de-scoped from 7.0.x.**
  ROCm #6101 is an SMU interface-version mismatch (firmware v50 vs driver
  v46) that leaves no working fan-control path — `pwm1` is read-only and
  commanded changes have no effect — and it persists across every tested
  kernel (6.14, 6.17, 7.0), not just 7.0.x. The warning is now scoped by PCI
  device ID (`0x7551`) rather than kernel version (suppressed only inside the
  6.18/6.19 hang range, where the hang warning dominates), the "accepts
  writes but silently ignores them" wording is corrected, and the id was
  renamed `smu_mismatch_navi48_r9700_kernel_7_0` → `smu_mismatch_navi48_r9700`.
- **nct6687/nct6775 collision wording.** The diagnostics banner and the
  `ModuleCollisionInfo` doc now state that the out-of-tree driver's `0xd450`
  claim was _historical_ and was removed upstream in Fred78290/nct6687d
  PR #164 (2026); the detector still fires (already-loaded modules and
  not-yet-updated packages remain at risk) and the remediation now points to
  updating the driver as the durable fix.

### Tested
- `kernel_warnings` tests updated for the new ranges and ids: `rdna4_on_6_18`
  now asserts the hang warning fires (was: asserts empty — this is the
  unsafe-regression guard), `r9700_on_7_1` now asserts the SMU warning
  persists, and new `r9700_on_6_18_warns_hang_only` / `r9700_on_6_17_warns_smu`
  cover hang-vs-SMU precedence and the broadened device scope.

## [1.8.2] — 2026-06-01

Intel platform foundation (DEC-110): adds CPU vendor detection on
`/diagnostics/hardware` and registers `intel_pch_thermal` in
`KNOWN_MODULES`. No control-loop, lease, or write-path behaviour
changes; additive wire-shape only. Pairs with **GUI v1.15.0**, which
consumes the new field for platform-scoped vendor quirks and ships
the Intel motherboard fan-control guide.

### Added
- **`cpu_vendor: String` field on `HardwareDiagnosticsResponse`**,
  populated by a new `read_cpu_vendor()` helper that parses the
  first `vendor_id` line from `/proc/cpuinfo`. Normalises
  `"GenuineIntel"` → `"Intel"`, `"AuthenticAMD"` and
  `"HygonGenuine"` → `"AMD"`, anything else → `""`. Serialised
  with `skip_serializing_if = "String::is_empty"` so the wire is
  unchanged when detection fails (hypervisors, unreadable file).
- **`intel_pch_thermal` row in `KNOWN_MODULES`** (`in_mainline =
  true`). The driver registers a hwmon device exposing the PCH
  temperature as `temp1_input`; it is sensor enrichment only, not
  a fan-control path. Adding it lets the diagnostics modules table
  honestly report the loaded module on Intel systems.

### Documented (in-code rationale)
- **`x86_pkg_temp` deliberately excluded** from `KNOWN_MODULES` with
  an inline comment. The kernel `x86_pkg_temp_thermal` driver
  registers with `.no_hwmon = true` and only appears as a thermal
  zone, never under `/sys/class/hwmon`. `coretemp` is the correct
  hwmon source for Intel CPU package temperature.

### Tested
- Nine new diagnostics unit tests (`api/diagnostics.rs` test
  module): GenuineIntel → "Intel", AuthenticAMD → "AMD",
  HygonGenuine → "AMD", KVM hypervisor → "", missing vendor_id →
  "", unreadable file → "", multi-CPU first-match selection,
  `intel_pch_thermal` present in `KNOWN_MODULES` with mainline=true,
  `x86_pkg_temp` confirmed absent from `KNOWN_MODULES`.

## [1.8.1] — 2026-06-01

Audit remediation pass following a cross-stack `/audit effort=max` sweep
on 2026-06-01. One safety-relevant signal-handling fix and three
documentation corrections. No new features, no wire-shape changes.
Pairs with **GUI v1.14.1**, which lands the matching `paths.atomic_write`
parity backport and dead-code sweep on the Python side.

### Fixed
- **Daemon now handles SIGTERM as a graceful-shutdown signal**
  (`daemon/src/main.rs`). Previously the `tokio::select!` block only
  registered `tokio::signal::ctrl_c()` (SIGINT), so `systemctl stop` —
  which sends SIGTERM by default — terminated the process before the
  in-process graceful path could run: `shutdown_tx.send`, GPU
  fan-curve reset, hwmon `pwm_enable=2` restore, and the IPC server
  join were all silently skipped. External safety was preserved by
  `ExecStopPost=control-ofc-restore-auto`, so this was a *cleanliness*
  bug rather than a *safety* bug, but the inline comment claiming
  "SIGINT/SIGTERM" was misleading. SIGTERM registration is fail-soft:
  if the handler cannot register (rare — unusual sandbox policies),
  the daemon logs a warning and falls back to SIGINT-only behaviour.
- A new integration test in `daemon/tests/signal_handling.rs` pins the
  SIGTERM dispatch behaviour: register the stream, self-deliver SIGTERM
  via `libc::kill(getpid(), SIGTERM)`, verify the stream wakes within
  5 s. Each integration-test file is its own binary, so the signal
  cannot leak into other tests.

### Documentation
- **`daemon.md` KernelWarning schema corrected**. The doc previously
  claimed the entry carried four fields — `id`, `severity` (with values
  `info / warn / high / critical`), `summary`, and `reference_url`. The
  actual wire shape per `daemon/src/hwmon/kernel_warnings.rs` is
  three fields — `id`, `severity` (values `info / medium / high /
  critical` — no `warn` variant), `message`. The GUI's contract spec
  (`08_API_Integration_Contract.md`) already had the correct shape;
  only `daemon.md` had drifted.
- **`docs/USER_GUIDE.md` verify-duration corrected** to ~6 s (was
  ~3 s). DEC-101 raised the verify wait to 6 s — slow-spinning fans
  need more settle time — and the GUI's per-call timeout was already
  bumped to 12 s in lockstep. The daemon's USER_GUIDE missed the
  update.
- **README "Latest release" line** refreshed to v1.8.1 (was 2 minor
  versions stale).

### Tests
- 485 tests passing (was 482; +3 new signal-handling integration tests).
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo audit` reports 0 vulnerabilities across 143 crates.

## [1.8.0] — 2026-05-13

Pairs with **GUI v1.13.0**. Combined release of two prior `[Unreleased]`
waves: the DEC-107 mutation-driven test-tests hardening pass (+16 daemon
tests, two non-breaking internal-API additions) and the DEC-108 `/audit`
follow-up hardening pass (`cargo fmt` fix, crash-safe atomic writes,
`set_pwm_all` broadcast coalescing, and 3 new IPC integration tests for
`POST /profile/activate` path-traversal protection). All additions are
backward-compatible on the wire and the internal API; older GUIs
interoperate with the new daemon and the new GUI interoperates with
older daemons without behavioural change. Total daemon tests: 482
passing (435 unit + 44 IPC integration + 3 main).

### Wave 1 — DEC-107 test-tests audit hardening

Test-tests audit hardening pass. A `/test-tests` mutation-driven audit
identified equivalent-mutant survivors in several core modules; this
pass closes the highest-value gaps with 16 new daemon tests and two
small ergonomic additions to the controller's observable surface. Pairs
with **GUI v1.13.0 Wave 1** (see GUI DEC-107 for the cross-stack rationale).

### Added (non-breaking, internal-API)
- **`LeaseError::Expired` variant.** Previously `validate_lease` and
  `renew_lease` returned `LeaseError::InvalidLease` for both "wrong
  lease id" and "matched id but TTL expired"; mutation testing showed
  the match guard at `hwmon/lease.rs:140` (the `==` ↔ `!=` flip) was
  observationally invisible. Now: `InvalidLease` for id mismatch,
  `Expired` for TTL elapsed. HTTP wire shape unchanged — both still
  map to `403 lease_required` via the `_` wildcard in
  `hwmon_control_error_response`. Internal callers can now
  distinguish renew-vs-reacquire without log parsing.
- **`HwmonPwmController::verify_mismatch_counts()` accessor.** Mirrors
  the existing `enable_revert_counts()` pattern. Returns a
  `&HashMap<String, u64>` of cumulative PWM verify-after-write
  mismatches per header (write N, readback ≠ N). Strong signal of
  BIOS clamping, EC interference, or an in-process concurrent writer.
  No HTTP exposure yet — added primarily to make the existing
  mismatch path observable in unit tests; future
  `/diagnostics/hardware` work may surface it.

### Tests
- **9 new lease tests** in `hwmon::lease::tests` pinning the wrong-id
  vs expired distinction (`validate_lease_distinguishes_wrong_id_from_expired`,
  `renew_lease_distinguishes_wrong_id_from_expired`) plus updates to
  the prior `*_fails_when_expired` tests to assert the new variant.
- **4 new boundary tests** in `health::cache::tests` for `gui_active()`
  at the 30s deferral window: 1s inside, 29s inside, exactly at 30s
  (the strict `<` predicate makes this *inactive*), and 31s past.
  Catches `<` ↔ `<=` mutations on the deferral predicate.
- **1 new integration test** in `profile_engine::tests`
  (`loop_defers_openfan_writes_when_gui_active`) running the loop
  with `record_gui_write()` called beforehand and asserting that
  zero SetPwm commands reach the mock transport.
- **2 new interpolation tests** in `profile::tests`
  (`evaluate_graph_interpolation_asymmetric_mid_segment`,
  `evaluate_graph_picks_correct_segment_for_mid_curve_temp`) using
  asymmetric temperatures (47°C, 73°C, 67°C) so the formula's `+` and
  `-` operators can't be mutated invariantly via halfway-point symmetry.
- **2 new chip-classification tests** in `hwmon::discovery::tests`:
  a single table-driven test covering every chip-name arm and every
  label-keyword sub-branch (37 rows), plus a precedence test pinning
  "cpu wins over gpu" in the fallback heuristic.
- **1 new range-boundary test** in `hwmon::pwm_control::tests`
  (`set_pwm_boundary_100_accepted_101_rejected`) explicitly
  asserting `pwm_percent=100` succeeds and `=101` raises
  `Validation` with the expected message.
- **3 new verify-mismatch tests** in `hwmon::pwm_control::tests`
  (`verify_mismatch_increments_counter`,
  `verify_no_mismatch_when_readback_matches_write`,
  `verify_mismatch_accumulates_across_writes`) using a new
  `ClampingSysfsWriter` mock whose `read_file` returns a value
  different from the last write. Catches mutations to the
  `actual_raw != raw` predicate at `pwm_control.rs:390`.
- **1 new API-mapping test** in `api::handlers::hwmon_ctl::tests`
  (`hwmon_control_error_response_maps_expired_lease_to_403`)
  confirming the new variant preserves the existing HTTP contract.
- **Total: 451 → 467 tests pass** (407 unit + 41 IPC integration + 3 main
  → 423 unit + 41 IPC integration + 3 main).

### Documentation
- Cross-stack rationale recorded in GUI **DEC-107**. Daemon
  `DECISIONS.md` carries a daemon-specific mirror of the same DEC-ID.

---

### Wave 2 — DEC-108 `/audit` follow-up hardening pass

A post-v1.7.0 `/audit` of both repos surfaced multiple issues; this
section captures the daemon-side fixes. See DEC-108 for the full
rationale.

#### Fixed (daemon)
- **`cargo fmt --check` failure on `main` resolved.** DEC-107
  committed two files (`hwmon/discovery.rs` table-driven test row
  and `hwmon/pwm_control.rs` boundary test) without running
  `cargo fmt`. The next release's gate would have blocked. Fixed
  by `cargo fmt --all`. (P1-A)
- **Atomic writes are now crash-safe.** Both
  `daemon_state::save_state_to` and `RuntimeConfig::save_to` did
  `std::fs::write(tmp) + rename` — atomic against process crash
  but not against power loss (the rename can land in the journal
  while data is still in the page cache, leaving a zero-length
  file on next mount). New `daemon::atomic_io::write_atomic`
  helper does `File::create → write_all → sync_all →
  set_permissions(0o600) → rename → parent-dir fsync`. Both call
  sites now use it. (P1-B)

#### Added (daemon)
- **`pub mod atomic_io`** — new module with `write_atomic(path:
  &Path, bytes: &[u8]) -> Result<(), String>`. Mirrors the GUI's
  `paths.atomic_write` shape. 9 unit tests cover content,
  permissions, tmp-cleanup on rename failure, large payloads, and
  empty content.
- **`SetPwmAllResult.coalesced: bool`** (controller-internal) and
  **`SetPwmAllResponse.coalesced: bool`** (wire-visible). When
  every channel already holds the requested value, `set_pwm_all`
  returns `coalesced: true` with no serial write and no cache
  update. Mirrors the single-channel `SetPwmResult` pattern.
  Additive on the wire — older GUIs ignore the field. (P2-B)

#### Tests (daemon)
- **+9 atomic_io unit tests** locking the write/fsync/rename
  sequence, owner-only permissions, tmp-cleanup-on-rename-failure,
  empty/large-content correctness, and the tmp-path naming
  convention.
- **+3 controller tests** for the new `set_pwm_all` coalescing
  behaviour (`set_pwm_all_coalesces_when_all_channels_already_at_value`,
  `set_pwm_all_does_not_coalesce_when_value_changes`,
  `set_pwm_all_does_not_coalesce_when_one_channel_diverges`).
- **+3 IPC integration tests** for `POST /profile/activate`
  path-traversal protection (P2-A): outside any search dir → 400,
  symlink chained outside → 400 (post-canonicalize check), inside
  search dir → 200 + state mutation.
- **Total: 467 → 482 tests pass** (423 → 435 unit + 41 → 44 IPC
  + 3 main).

#### Documentation
- Cross-stack rationale recorded in GUI **DEC-108** (authoritative
  full log). This `CHANGELOG.md` and daemon `DECISIONS.md` carry
  the daemon-relevant subset.

---

## [1.7.0] — 2026-05-13

Pairs with **GUI v1.12.0**. Coordinated AMD-board-support hardening
release covering AM4 400-series (DEC-105) and AM4 500 / AM5 600 / AM5
800 (DEC-106) in one minor bump. Adds one new optional field on
`/diagnostics/hardware` (`module_collisions`); the rest is additive
chip-detection data and detector refinement. Older GUIs silently
ignore the new field; the daemon emits the same field shape regardless
of whether the suppression refinement is in effect.

### DEC-106 — AM4 500 / AM5 600 / AM5 800 dual-Nuvoton refinement

The wire shape of `/diagnostics/hardware.module_collisions` is
unchanged; the daemon behaviour is refined to emit FEWER entries on
legitimate dual-Nuvoton boards. Older daemons that predate this work
continue to emit the broader (sometimes false-positive) result on
boards like the ASRock X870E Taichi Lite.

#### Features
- **`detect_module_collisions` now accepts a `chips: &[ChipBinding]`
  slice.** When the canonical `(nct6687, nct6775)` pair is loaded AND
  `chips` shows two distinct nct6 chips at distinct `device_id`s, the
  collision is suppressed — each driver legitimately owns its own
  physical chip and no chip-ID overlap can occur. The original
  brick-risk detection (single chip + both modules loaded) is
  unchanged. Defensive: empty `chips` still emits CRITICAL (no
  evidence of separation → assume the brick shape).
- **Two new AM4 500-series & AM5 800-series Gigabyte AORUS dual-chip
  entries** in `GIGABYTE_DUAL_CHIP_BOARDS`: **B550 VISION D**
  (it8688 + it8792, verified against upstream lm-sensors config
  `configs/Gigabyte/GA-B550-VISION-D.conf`) and **B850-AI-TOP**
  (it8696 + it87952, per frankcrawford/it87 issue #93). The X870
  AORUS STEALTH ICE board is deliberately not in this table — its
  IT8883 secondary chip has no Linux driver and a permanent
  missing-chip warning would be useless.

#### Tests
- `api::diagnostics::tests` gains seven new tests:
  - `detect_module_collisions_suppressed_on_legitimate_dual_nuvoton_board`
    — Taichi Lite shape (NCT6686 + NCT6799 at distinct IDs) is
    silent.
  - `detect_module_collisions_still_critical_for_single_chip_collision`
    — single nct6 chip with both modules loaded still surfaces
    CRITICAL.
  - `detect_module_collisions_critical_when_chips_unknown` — empty
    `chips` defensively keeps the CRITICAL behaviour.
  - `detect_module_collisions_non_nct6_chips_ignored_for_suppression`
    — k10temp / amdgpu / other non-nct6 entries cannot satisfy the
    "two distinct nct6 chips" suppression rule.
  - `expected_chips_b550_vision_d_pairs_it8688_with_it8792`,
    `expected_chips_b850_ai_top_pairs_it8696_with_it87952`, and
    `expected_chips_x870_aorus_stealth_ice_not_in_table` cover the
    dual-chip table edits.

### DEC-105 — AM4 400-series hardening + new `module_collisions` field

Adds one new optional field on `/diagnostics/hardware` plus AM4
400-series additions to the chip-detection data tables. Older GUIs
silently ignore the new field; older daemons (without this work) just
return no key for it and the GUI defaults to `[]`. No breaking
contract change.

#### Features
- **`module_collisions: Vec<ModuleCollisionInfo>` on
  `GET /diagnostics/hardware` (DEC-105).** The daemon now scans
  `/proc/modules` for known-bad simultaneous-load pairs and reports
  any matches as a CRITICAL severity entry with a remediation string.
  The seed entry is `(nct6687, nct6775)` — the out-of-tree `nct6687`
  driver declares chip ID `0xd450`, the same ID assigned to legitimate
  NCT6797D by `drivers/hwmon/nct6775-platform.c`. When both modules
  are loaded the wrong driver can claim the chip and write into
  non-volatile fan registers, with at least one upstream report
  (ublue-os/bazzite #4498) documenting a permanently bricked CPU_FAN
  header on MSI hardware. NCT6797D is common on AM4 400-series MSI
  boards (B450M MORTAR, X470 GAMING PRO CARBON, MAG B450 TOMAHAWK
  MAX). The field is emitted only when non-empty
  (`skip_serializing_if = "Vec::is_empty"`).
- **AM4 400-series Gigabyte AORUS dual-chip lookups (DEC-105).**
  Added X470 AORUS ULTRA GAMING (verified against upstream lm-sensors
  config), X470 AORUS GAMING 5 WIFI, X470 AORUS GAMING 7 WIFI, and
  B450 AORUS PRO (substring covers PRO WIFI). All IT8686E + IT8792E.
  Pre-existing B450 AORUS PRO-CF entry preserved.
- **`asus_atk0110` recognised in `KNOWN_MODULES` (DEC-105).** Closes a
  real ASUS diagnostic gap — the diagnostics page can now report this
  driver as loaded and the GUI's chip-guidance entry advises that it
  is a read-only ACPI sensor path, not a PWM-write path.

#### Tests
- `api::diagnostics::tests` gains:
  - `detect_module_collisions_flags_nct6687_with_nct6775` /
    `_silent_when_only_*_loaded` / `_returns_empty_on_unreadable_path`
    covering the new helper.
  - `expected_chips_x470_aorus_ultra_gaming_pairs_it8686_with_it8792`
    confirming the AM4 chip pairing matches the upstream lm-sensors
    config.
  - `expected_chips_b450_aorus_pro_uses_am4_400_chip_pair` and
    `expected_chips_b450_aorus_pro_wifi_resolves_via_substring` for
    the B450 generation.
  - `asus_atk0110_recognised_in_known_modules` regression test.

#### Safety / framing
- **Remediation string for the `(nct6687, nct6775)` collision now
  requires chip-ID verification first.** The string emitted in
  `module_collisions[].remediation` instructs the user to run
  `cat /sys/class/hwmon/hwmon*/name` BEFORE any blacklist command, and
  shows both the `blacklist nct6775` (for genuine NCT6687-R boards) and
  `blacklist nct6687` (for NCT6797D / NCT6798D boards) paths as
  parallel alternatives. Protects users from blindly following the
  banner and inadvertently removing their working driver.

## [1.6.5] — 2026-05-08

Packaging-UX release. Pairs with **GUI v1.11.3**. Cuts the daemon's
post-install message from 106 lines to ~31 and removes a long-stale
v1.1.2 awk migration so paru's PKGBUILD-review pager has materially
less to show on a fresh install. No daemon behavioural changes — same
binary, same API, same systemd integration.

### Changed
- **`packaging/control-ofc-daemon.install` slimmed (DEC-104).**
  Post-install message is now four lines (start command, modules-load
  hint, pointer to `man control-ofc-daemon`, pointer to the GUI's
  Diagnostics → Fans → Hardware Readiness card) instead of the previous
  4-step walkthrough. The 30-line per-bootloader kernel-parameter guide
  was duplicated content — the man page already covers it. Pacman's
  install-script invocation is unchanged; only the user-facing text is
  shorter.
- **`hyper-util` moved from `[dependencies]` to `[dev-dependencies]`.**
  The crate is consumed solely by `daemon/tests/ipc_integration.rs`
  (`use hyper_util::rt::TokioIo;`). Pre-fix, the production binary
  linked it for nothing. No runtime impact; `cargo build --release`
  output is marginally smaller.

### Removed
- **`_strip_legacy_runtime_sections` awk migration (DEC-104).** The
  helper auto-stripped `[profiles]` and `[startup]` sections from
  `/etc/control-ofc/daemon.toml` on `post_upgrade` to protect users
  upgrading from <1.1.2 directly. With v1.6.5 five minor releases past
  v1.1.2, every upgrader has had the migration applied long ago, and
  the 50-line shell helper was dead weight in paru's review pager.
  Anyone leapfrogging from <1.1.2 → 1.6.5 (vanishingly rare in practice)
  will need to remove those sections manually if present.

### Added
- **Install-UX tip in `daemon/README.md` (DEC-104).** Footnote-style
  note describing paru's PKGBUILD-review pager (the "press `q`"
  prompt new users see on first install) and how to opt out via
  `paru -S --skipreview` or `SkipReview` in `~/.config/paru/paru.conf`.
  Phrased as a tip, not the canonical install command — paru's review
  is a security feature and we are not normalising "skip review by
  default" for an Arch audience.
- **`daemon/Cargo.toml` `[package]` metadata.** Added `license = "MIT"`,
  `description`, `repository`, and `authors` fields. Pre-fix, `cargo
  metadata` reported the daemon as `UNSPECIFIED` license, and SBOM
  tooling / `cargo-deny` / `cargo publish` would surface it as an
  unidentified package. Aligns the daemon crate with the `[package]`
  hygiene the AUR PKGBUILD already enforces externally.

### Documentation
- **DEC-104 added** in this repo and mirrored in the GUI repo. Records
  the investigation that traced the "press `q`" complaint to paru's
  default review pager (not a SHA256 issue), the alternatives considered
  (custom signed pacman repo rejected as disproportionate for a
  single-author project), and why the in-package fix is limited to
  cutting our own pager content + documenting paru's opt-out.

## [1.6.4] — 2026-05-08

Bug-fix release. Pairs with **GUI v1.11.1**. Stops a 1 Hz error storm
on RDNA3+ AMD GPU systems where the GPU's read-only hwmon `pwm1` shadow
was being treated as a controllable fan header.

### Fixed
- **AMD GPU `pwm1` excluded from hwmon discovery (DEC-102).**
  `hwmon::pwm_discovery::discover_device_pwm` skips entries whose
  `chip_name == "amdgpu"` early — before any `pwmN` enumeration. Pre-fix,
  the daemon advertised `hwmon:amdgpu:0000:XX:XX.X:pwm1:pwm1` in
  `GET /hwmon/headers`; clients (the GUI in particular) could bind it to
  a profile control and the resulting 1 Hz `POST /hwmon/.../pwm` flood
  produced a 503/`Permission denied (os error 13)` storm in the journal.
  RDNA3+ kernels expose that file read-only without `pwm1_enable`, so
  the write can never succeed; GPU fan control belongs on
  `/gpu/{id}/fan/...` with the `amd_gpu:` member prefix exclusively.
- **`POST /hwmon/{header_id}/pwm` returns `400 feature_unavailable`
  when the targeted header's discovered `is_writable=false` (DEC-102).**
  Defense-in-depth: any future chip exposing a read-only `pwmN`
  (BIOS-locked motherboard headers, etc.) now produces a clean
  non-retryable error envelope (DEC-094 shape, mirroring DEC-098's
  GPU handler) instead of mis-classifying kernel `EACCES` as
  `503 hardware_unavailable + retryable: true`. Lease-validation order
  preserves precedence so a permanently read-only header reports
  `feature_unavailable` even when the caller's lease is invalid.

### Tests
- 4 new integration tests in `daemon/tests/ipc_integration.rs`:
  `hwmon_set_pwm_read_only_header_returns_400_feature_unavailable`,
  `hwmon_set_pwm_read_only_header_takes_precedence_over_lease`,
  `hwmon_set_pwm_unknown_header_returns_404`, and
  `hwmon_discovery_excludes_amdgpu_end_to_end_via_ipc` (which builds a
  fake hwmon root with both `it8696` and `amdgpu` chips, runs real
  `discover_pwm_headers` against it, hands the result to a real
  `HwmonPwmController`, and confirms `GET /hwmon/headers` over the
  IPC socket returns only the motherboard chip).
- 3 new unit tests in `pwm_discovery::tests` —
  `discover_amdgpu_excluded`, `discover_amdgpu_excluded_even_with_enable_file`,
  `discover_amdgpu_excluded_alongside_motherboard_chip` — and
  `discover_without_enable_file` reworked against `nct6798` (the
  legitimate non-amdgpu case the test was meant to cover).

## [1.6.3] — 2026-05-07

Pairs with **GUI v1.11.0**. PWM verify timing fix and dual-chip
diagnostics support, plus a corrected `it87` mainline flag for the
Diagnostics modules table.

### Changed
- **PWM verify wait raised from 3 s to 6 s (DEC-101).** The
  `hwmon_ctl::VERIFY_WAIT_SECONDS` constant doubles so slow-spinning
  fans (pumps, large 140 mm chassis fans) settle their RPM in time to
  be classified correctly. Previous 3 s wait produced false
  `no_rpm_effect` verdicts. Worst-case round-trip is now ~7.5 s; the
  GUI's verify HTTP timeout (12 s) and pause-safety auto-resume (9 s)
  must stay above this value.
- **`KNOWN_MODULES` `it87` flag flipped from `true` to `false`
  (DEC-101).** The *module name* exists in mainline but every chip we
  target (IT8625E / IT8686E / IT8688E / IT8689E / IT8696E /
  IT87952E) requires the out-of-tree `frankcrawford/it87` DKMS build.
  Marking the module as not-mainline keeps the GUI's modules-table
  column truthful. The chip-level `chip_driver_in_mainline` is
  unchanged and still per-chip accurate.

### Added
- **`/diagnostics/hardware.expected_chips` (DEC-101).** Curated
  DMI-board → chip-list lookup covering known dual-IO Gigabyte boards
  (X570/X670/X870/Z690/Z790 AORUS series and TRX40). When a board is
  in the table, the field lists the chip names that should appear in
  hwmon. Empty for unknown boards. Skipped from the wire when empty
  so older clients see no shape change.
- **`/diagnostics/hardware.kernel_detected_chips` (DEC-101).** Best-
  effort chip names parsed from `/dev/kmsg` `it87:` log lines.
  Populated when the kernel ring buffer is readable
  (default Arch `kernel.dmesg_restrict=0`); empty otherwise. Useful
  for distinguishing "kernel saw the chip but driver did not bind"
  from "kernel never saw the chip"; not authoritative.

### Tests
- 11 new unit tests in `api::diagnostics::tests` covering the
  curated lookup table, the kmsg parser (deduplication, false-positive
  rejection, short-code rejection, empty input), the file-fixture
  driver, and the `it87` mainline flag.
- Two `ipc_integration.rs` HwmonVerifyResponse fixtures bumped from
  `wait_seconds: 3` to `6`.

## [1.6.2] — 2026-05-07

Audit-driven hygiene pass. Pairs with **GUI v1.10.2**. Three behavioural
fixes (one wire-contract addition, one safety-restore gap, one
dual-writer-guard bug) plus internal cleanup of misleading naming and
unused dependencies.

### Fixed
- **GPU `POST /gpu/{id}/fan/reset` now records GUI activity (DEC-100).**
  All other GPU/OpenFan/hwmon write handlers call `record_gui_write()`
  on success; the reset path was the lone exception. With a profile
  active, the next 1 Hz profile-engine tick (~1 s after the reset call)
  re-asserts the profile's commanded curve and silently undoes the
  user's reset. Both the PMFW reset arm and the legacy-pwm1 reset arm
  now call `record_gui_write()` so the profile engine defers for the
  same 30 s window it gives to a `set` call.
- **`POST /hwmon/{id}/verify` surfaces a failed restore-PWM write
  (DEC-100).** The handler previously did `let _ = ctrl.set_pwm(...)`
  on the post-verify restore — a `HwmonControlError` (lease expired
  mid-call, EINVAL, etc.) was silently swallowed and the header was
  left at the test value (20 % or 80 %) without any caller-visible
  signal. The handler now logs at warn-level and returns
  `restore_failed: true` in the response body so the GUI can prompt
  the operator to re-set the desired PWM. The new field is
  `#[serde(skip_serializing_if = "is_false")]`, so older clients see
  the same wire shape they always have.
- **`packaging/control-ofc-restore-auto.sh` now restores
  `fan_zero_rpm_enable=1` (DEC-100).** The graceful-shutdown panic hook
  already re-enables PMFW zero-RPM on SIGTERM and panic, but
  `ExecStopPost` runs even when the process was SIGKILLed or OOM-killed
  before any userspace code ran. Without this, a daemon crash that
  occurred while zero-RPM was disabled (i.e. between a `set_static_speed`
  and the next `reset_to_auto`) would leave the GPU fan running
  continuously at idle. The script now writes `1\n` + `c\n` to every
  `fan_zero_rpm_enable` sysfs file alongside the existing `r\n` + `c\n`
  curve restore.
- **`SSE_MAX_CLIENTS` admission survives tight CAS contention
  (DEC-100).** `events_handler` now bounds admission to
  `SSE_ADMISSION_ATTEMPTS = 4` with a `tokio::task::yield_now()`
  between failures (whether the CAS lost to a concurrent change or the
  counter was at the limit). Concurrent disconnects and CAS losers can
  settle before we surface 503 to the caller. Single-client load and
  cold-start admission are unaffected.

### Renamed
- **`GPU_PMFW_WRITE_RETRIES` → `GPU_PMFW_NUM_CURVE_POINTS` (DEC-100).**
  Audit found the constant's doc comment described "PMFW write retries
  before giving up" — but the constant is the `num_points: u8` argument
  to `set_static_speed` / `set_static_speed_with_zero_rpm`, controlling
  how many curve-point indices are written. There is no retry logic in
  the GPU PMFW write path. A maintainer who reduces this to "suppress
  retries" would silently shrink the curve. The new name and doc
  comment reflect what it actually does.

### Documentation
- **`docs/USER_GUIDE.md`** now references the current v4 profile schema
  (was v3); the migration sentence makes clear that v3 and earlier
  profiles auto-migrate on load.
- **`packaging/profiles/quiet.json`** bumped to `version: 4`. The
  `members: []` shape is unchanged so the role-aware `minimum_pct`
  migration is a no-op for the example profile.

### Removed
- **Unused dependencies** — `ctrlc`, `tokio-stream`, `tower`. All three
  were declared in `daemon/Cargo.toml` but never imported. Confirmed
  via `cargo machete` and `grep`. Drops one dependency per
  `cargo audit` and shaves a small amount of build time / binary mass.

### Tests
- 4 new tests:
  - `gpu_reset_fan_records_gui_write` — integration test that exercises
    the full reset handler against a tempdir-backed PMFW GPU and
    asserts `cache.snapshot().gui_active()` becomes true.
  - `hwmon_verify_response_omits_restore_failed_when_false` — wire
    contract: older clients see no extra field.
  - `hwmon_verify_response_includes_restore_failed_when_true` — wire
    contract: newer clients see the flag and can warn the operator.
  - `test_app_state_with_writable_pmfw_gpu` (helper) — reusable test
    fixture for future PMFW-write-path coverage.
- 374 lib + 3 (config) + 37 IPC = **414 tests pass** (was 411 before
  this pass).
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `cargo machete` reports no unused dependencies.

### Decisions
- **DEC-100 (cross-repo):** audit-pass-2 remediations covering
  warning-signal immediacy, verify `restore_failed` contract addition,
  GPU reset records `gui_active`, lease retry-timer suspend, GPU ZRP
  restore script, SSE admission retry, and the
  `GPU_PMFW_NUM_CURVE_POINTS` rename.

---

## [1.6.1] — 2026-05-06

Audit-driven correctness fixes. Pairs with **GUI v1.10.1**. No new user-
visible features — every change is either a contract correction or an
internal change that improves runtime fairness under load.

### Fixed
- **GPU `feature_unavailable` now actually fires for read-only RDNA3/4
  hardware (DEC-098).** The legacy hwmon dispatch arm in
  `gpu_set_fan_handler` and `gpu_reset_fan_handler` previously gated on
  `gpu.has_pwm` alone. RDNA3/RDNA4 GPUs booted without
  `amdgpu.ppfeaturemask=0xffffffff` expose `pwm1` read-only and lack
  `pwm1_enable`, so `set_legacy_pwm` would attempt to write
  `pwm1_enable`, fail with ENOENT, and surface
  `503 hardware_unavailable + retryable: true` — wrong code (capability
  gap, not transient hardware fault) and wrong retry semantics
  (permanent, not retryable). Both handlers now consult a canonical
  helper `AmdGpuInfo::can_write_legacy_pwm()` and return
  `400 feature_unavailable + retryable: false`. The capability scoring
  in `status::capabilities_handler` consumes the same helper so
  `/capabilities` and the actual handler outcome cannot disagree.
  When the GPU shape matches RDNA3/4 without overdrive, the error
  message includes the `amdgpu.ppfeaturemask=0xffffffff` hint that
  unlocks PMFW.
- **OpenFan and hwmon API write handlers now run on `spawn_blocking`
  (DEC-099).** `set_pwm_handler`, `set_pwm_all_handler`,
  `set_target_rpm_handler`, and `hwmon_set_pwm_handler` previously held
  their parking_lot locks on a tokio worker thread directly, pinning
  runtime workers for hundreds of milliseconds per write. They now
  match the convention already used by the GPU handlers and the
  polling loop. Callers see no behavioural change; the runtime stays
  responsive for other tasks while a serial or sysfs write is in
  flight.
- **Thermal-emergency override scan releases the `FanController` mutex
  between channels (DEC-099).** The scan previously held the mutex
  across all 10 OpenFan channels (~5 s upper bound) plus the hwmon
  headers, serialising every concurrent GUI write request behind it.
  Per-channel re-locking lets GUI requests interleave; if a GUI write
  briefly defeats safety, the next 1 Hz tick re-asserts the forced
  value, so the safety net still holds.

### Added
- **`amd_gpu.kernel_warnings` capability field (DEC-098).** Populated
  from a new `crate::hwmon::kernel_warnings` module that reads
  `/proc/sys/kernel/osrelease` and matches the running kernel against
  published amdgpu regressions:
  - `rdna_hang_kernel_6_19_x` (Critical) — kernel 6.19.x on any RDNA3
    or RDNA4 GPU; Phoronix-confirmed hard hang (Dec 2025).
  - `smu_mismatch_navi48_r9700_kernel_7_0` (Critical) — kernel 7.0.x
    on R9700 (PCI 0x7551) with PMFW exposed; ROCm Issue #6101 silent
    fan_curve write failure. Scoped narrowly to 0x7551; RX 9070 XT
    (0x7550) on the same kernel is not affected.

  The field uses `#[serde(skip_serializing_if = "Vec::is_empty")]`,
  so older clients that don't know about it see no change in
  `/capabilities` output.

### Tests
- Two new integration tests pin the read-only-RDNA case:
  `gpu_set_fan_read_only_rdna_returns_400_feature_unavailable` and
  `gpu_reset_fan_read_only_rdna_returns_400_feature_unavailable`,
  plus a new `test_app_state_with_read_only_gpu` fixture in
  `daemon/tests/ipc_integration.rs`. The fixture covers the
  `has_pwm=true, has_pwm_enable=false, fan_curve_path=None` shape that
  RDNA3/4 hardware without overdrive presents.
- 17 new unit tests cover `kernel_warnings` parsing, severity, and
  device-id scoping (positive cases for 6.19 + RDNA3/4, negative for
  RDNA2; positive for R9700 0x7551 on 7.0, negative for RX 9070 XT
  0x7550 on the same kernel).
- 374 lib + 34 integration + doc tests pass.

### Documentation
- `DECISIONS.md`: DEC-098 (legacy-PWM gate + kernel_warnings),
  DEC-099 (`spawn_blocking` + per-channel mutex).
- `CLAUDE.md` cross-stack updates land in the GUI repo (DEC-098 and
  DEC-099 are mirrored there as authoritative).

---

## [1.6.0] — 2026-05-02

Headless profile-mode parity and per-GPU zero-RPM control. Pairs with
**GUI v1.10.0**. The daemon can now drive fans autonomously per a
GUI-authored profile with the same audible behaviour as GUI-driven mode,
honour each GPU's user-chosen idle-fan-stop preference, and surface a
clean `/profile/deactivate` endpoint so the GUI can stop curve writes
without restarting the daemon.

### Added
- **`POST /profile/deactivate` endpoint (DEC-097).** Clears the active
  profile, persists the cleared state to `daemon_state.json`, releases
  any held `profile-engine` hwmon lease, and refreshes the GUI-activity
  marker so the engine doesn't immediately re-take a lease. Idempotent —
  calling on an already-deactivated daemon returns 200 with both
  `previous_profile_id` and `previous_profile_name` set to `null`.
  Distinct from `/profile/activate` with a missing path (which 404s).
  GUI leases (any owner other than `profile-engine`) are explicitly
  preserved so manual GUI writes continue uninterrupted.
- **2 °C falling-temperature deadband in the profile engine (DEC-096).**
  `evaluate_profile` now mirrors the GUI's
  `_evaluate_curve_with_hysteresis` (`HYSTERESIS_DEADBAND_C = 2.0`):
  when the current temperature has fallen ≤ 2 °C below the last
  transition anchor, the previous curve output is held instead of
  re-interpolated. Closes the audible parity gap where headless mode
  oscillated at curve transitions while GUI-driven mode behaved
  smoothly with the same profile. Per-control state lives in
  `ProfileEngineState` alongside the existing tuning state and is
  cleared on profile change or deactivation. Five new unit tests cover
  hold-on-fall, release below threshold, anchor advance on rise, anchor
  stationary for sub-0.5% deltas, and clearing on profile swap.
- **Per-GPU `fan_zero_rpm` flag in profile schema v4 (DEC-095).**
  `ControlMember` now carries an optional `fan_zero_rpm` boolean
  (default false). When true on an `amd_gpu` member, the daemon
  preserves PMFW `fan_zero_rpm_enable` while writing the curve so the
  GPU honours its idle fan-stop threshold. When false (or omitted on a
  legacy v3 profile), the daemon disables zero-RPM as before — the
  pre-1.6.0 behaviour is exactly preserved by the safe default.
- **`hwmon::gpu_fan::set_static_speed_with_zero_rpm` helper** with an
  explicit `preserve_zero_rpm: bool` parameter. The legacy
  `set_static_speed` is now a thin wrapper that hard-codes
  `preserve_zero_rpm=false`, so the manual-write path through
  `/gpu/{id}/fan/pwm` is unchanged. Two new unit tests cover the new
  path (preserve=true skips the disable; preserve=false matches
  legacy behaviour).

### Changed
- **Profile schema default version is now 4.** v3 profiles deserialise
  unchanged because all v4 fields use `#[serde(default)]`. The version
  warning still fires below 3 to flag truly legacy profiles. Two new
  unit tests in `profile.rs` verify that v3 profiles default
  `fan_zero_rpm` to false and v4 profiles round-trip the user's
  explicit choice.

### Profile engine state — new fields
`ProfileEngineState` gains `last_curve_output` and
`last_transition_temp` maps (per control id) to back the deadband.
`deactivate()` and `sync_profile_id()` clear them alongside the
existing `last_output` so the deadband doesn't leak across profile
swaps. New public read-only accessors (`last_curve_output`,
`last_transition_temp`) expose the state for tests.

### Tests
- Daemon test count: 357 unit + 32 integration + 3 doc = 392 (was
  347 + 28 + 3 = 378). +14 tests covering deadband behaviour,
  fan_zero_rpm round-tripping in `set_static_speed_with_zero_rpm`,
  v3-vs-v4 profile loading, and the four new `/profile/deactivate`
  integration scenarios (clears state, idempotent, releases the
  profile-engine lease, preserves GUI leases).
- The three existing tuning tests
  (`tuning_step_up_rate_limits_large_jump`,
  `tuning_step_down_rate_limits_large_drop`,
  `tuning_start_threshold_jumps_from_zero`) now bump the sensor
  temperature each cycle so the new deadband releases — they
  exercised the tuning pipeline at a fixed temperature, which the
  deadband would otherwise hold across.

### Why
Cross-stack audit found that headless profile-mode was technically
correct but audibly different from GUI-driven mode (no deadband),
that profile deletion left a phantom profile driving fans until
daemon restart, and that the daemon couldn't honour a user's choice
to keep zero-RPM idle on a GPU governed by a curve. This release
closes all three gaps. The role-aware safety floor decision (option B
from the audit) is GUI-side and lives in **GUI v1.10.0**; the daemon
intentionally does not enforce a per-role floor, preserving the
established "GUI owns curve safety policy, daemon owns thermal
emergency" architectural split (CLAUDE.md, DEC-022).

## [1.5.6] — 2026-04-30

Packaging hygiene release. No code, contract, or behaviour changes.

### Fixed
- **`depends=` now declares `libgcc` directly.** The release binary's
  ELF `NEEDED` entries include `libgcc_s.so.1`, which was previously
  satisfied only transitively through `systemd-libs → libgcc`. Declaring
  it directly is namcap best practice and removes the implicit-dependency
  warning. No effect on already-installed users (the resolver already
  pulled `libgcc` in via the chain), but the chain is no longer fragile
  if `systemd-libs` ever drops it.
- **`packaging/.SRCINFO` is now tracked in-repo.** Was previously in
  `.gitignore` and only maintained inside the AUR clone. The
  `.gitignore` entry is removed and the file is regenerated against
  the current PKGBUILD; the new pre-commit hook keeps it in sync going
  forward (see Tooling).

### Changed
- **`makedepends=` no longer lists `cargo`.** The `rust` package
  provides `cargo`; listing both is redundant.

### Tooling
- **New `.githooks/pre-commit`** that auto-regenerates
  `packaging/.SRCINFO` whenever `packaging/PKGBUILD` is staged for
  commit, so the in-repo file cannot drift again. Opt-in via:
  `git config core.hooksPath .githooks`. The hook is a no-op when
  `makepkg` is not on PATH (e.g. on non-Arch CI).

## [1.5.5] — 2026-04-30

Patch release fixing a man-page rendering bug. No code changes.

### Fixed
- **Man page em-dash rendering.** `man control-ofc-daemon` rendered every
  em-dash (U+2014) as a doubled `——` on systems with groff 1.24+ (current
  Arch). The groff 1.24 `tty.tmac` defines `.char \[em] \[em]\[em]` for
  UTF-8 output to approximate the typographic em-quad width on a
  half-width cell grid; passing literal U+2014 through scdoc therefore
  emits `——`. Replaced em-dashes in `man/control-ofc-daemon.1.scd`
  (NAME, AMD GPU SUPPORT, and bootloader sections) with the canonical
  man-page convention `--`. `man control-ofc-daemon` now reads
  correctly, including the bootloader checklist. Verified with
  `groff -man -K utf8 -Tutf8 -ww` (zero warnings) and `man -l`.
  Degree-sign rendering (`105°C`, `80°C`) is unaffected and continues
  to render as a single character.

## [1.5.4] — 2026-04-30

Install-experience and documentation packaging release. No changes to the
daemon binary's runtime behaviour, IPC contract, or persistence format.
Existing operator config (`/etc/control-ofc/daemon.toml`) and runtime
config (`/var/lib/control-ofc/runtime.toml`) load unchanged.

### Added
- **Man page ships.** `man control-ofc-daemon` now renders a manual that
  documents CLI flags, environment variables, files, signals, and a new
  AMD GPU section covering `amdgpu.ppfeaturemask=0xffffffff` per
  bootloader (GRUB, systemd-boot, rEFInd, Limine).
- **Shell completions ship** for bash, zsh, and fish under
  `/usr/share/{bash-completion,zsh/site-functions,fish/vendor_completions.d}/`.
  Tab-completion of `control-ofc-daemon --` works after install.
- **User documentation ships in `/usr/share/doc/control-ofc-daemon/`:**
  `README.md`, `CHANGELOG.md`, `daemon.md`, `USER_GUIDE.md`,
  `DEVELOPER_HANDOVER.md`, and the new `ADRs/` directory.
- **`docs/ADRs/001-ipc-transport.md`** and
  **`docs/ADRs/002-runtime-config-split.md`.** The codebase had eight
  outstanding cross-references to these ADRs; they are now written and
  the references resolve.
- **`SECURITY.md`** at the repo root, listing the supported version
  range, contact, and the daemon's privilege/sysfs/socket boundaries.

### Changed
- **`post_install` message is rewritten to remove the dead-end "Install
  the GUI: yay -S control-ofc-gui" line.** A user installing both
  packages via `paru -S control-ofc-daemon control-ofc-gui` previously
  saw a self-referential nudge to install the GUI they were already
  installing, with a different AUR helper. The new message focuses on
  the three things the daemon's installer can usefully tell the user:
  enable command, modules-load hint, and where to find the man page
  and USER_GUIDE.
- **AMD GPU kernel-parameter advice in `post_install` is narrower and
  pointed at the man page** rather than embedding bootloader-specific
  steps in the install transcript. The man page's new AMD GPU section
  now carries the per-bootloader detail.
- **Man page recovery wording corrected:** the daemon's safety floor
  is a 60 % PWM recovery floor applied for one cycle, not a "recover
  at 60°C" temperature threshold (which was never the implementation).
  Matches the wording in `daemon.md`, `USER_GUIDE.md`, and the
  daemon's `safety.rs`.

### Packaging
- Adds `scdoc` to `makedepends`. Builds the man page via
  `scdoc < man/control-ofc-daemon.1.scd` in `build()` and installs
  to `/usr/share/man/man1/control-ofc-daemon.1`.
- `sha256sums` switched to `SKIP` pending the post-tag-push checksum
  refresh — same pattern as previous releases.

### Why
A `/audit documentation` pass on both repos plus a fresh
`paru -S control-ofc-daemon control-ofc-gui` test surfaced eight
install-experience defects, of which four were on the daemon side:
no man page despite the source being checked in, no shell completions,
a `/usr/share/doc/control-ofc-daemon/` directory with only a udev
rules example (the `post_install` message advertised a "Full setup
guide" there), and a self-referential `yay -S control-ofc-gui` line.
This release fixes all four. See the GUI repo v1.9.1 entry for the
matching client-side changes.

## [1.5.3] — 2026-04-28

Truthfulness patch for `POST /hwmon/{header}/verify` — the `details`
strings now acknowledge that a register change during the 3 s test
window can come from another in-process writer (lease holder,
thermal-safety override) and not only from BIOS/EC reclaim. Pairs with
**GUI v1.8.0** which also pauses the GUI control loop during a verify
call to eliminate the dominant racer (the GUI's own 1 Hz tick).

### Changed
- **`pwm_value_clamped` and `pwm_enable_reverted` `details` strings**
  in `classify_verify_result` now name BIOS/EC firmware as the most
  likely cause but call out the concurrent-writer alternative
  explicitly, with a "Re-run with no profile active and no other
  client writing" disambiguation hint. Wording-only — the response
  shape, the `result` enum values (`effective` / `pwm_enable_reverted` /
  `pwm_value_clamped` / `no_rpm_effect` / `rpm_unavailable`), HTTP
  status codes, and error envelope are all unchanged. The GUI's
  existing `hwmon_guidance.verification_guidance` lookup keeps
  matching without coordinated GUI redeploys.

### Unchanged (explicitly verified)
- Verify wait duration (3 s), test PWM choice (20 % or 80 % depending
  on initial), and classification thresholds (`delta > 10` raw for
  clamped) are all preserved.
- No new error-envelope variants. No schema changes.
- The pre-existing `pwm_enable_reverted` BIOS/EC reclaim story
  (DEC-074, AORUS Smart Fan watchdog) still applies — the wording
  change just acknowledges the second possible cause.

### Tests
- Three new unit tests in `hwmon_ctl.rs::tests` covering the new
  wording for both result variants and asserting the `result` enum
  values are unchanged after the rewording.

### Why
A `/investigate-bug` pass on the X870E AORUS MASTER traced the user's
"PWM control isn't working" report back to a misclassified verify
result. The board controls correctly; the GUI's own control loop was
racing the daemon's verify wait, and the classifier blamed BIOS/EC
even though the racer was an internal Linux writer. GUI v1.8.0 fixes
the race itself; this daemon patch fixes the wording so a residual
race (e.g. an external CLI tool) does not produce a misleading verdict.
See `PWM_VERIFY_REMEDIATION.md` in the GUI repo for the full
investigation and approved plan.

## [1.5.2] — 2026-04-25

Operator-experience patch: stop the journal-spam side effect of the
pwm_enable watchdog without weakening the watchdog itself. Pairs with
**GUI v1.7.1**.

### Changed
- **Throttled pwm_enable watchdog log emission.** `HwmonPwmController` no
  longer emits one `WARN` line per second per affected header when the
  BIOS/EC reclaims `pwm_enable`. Each header now produces a single `WARN`
  on the first reclaim, subsequent reverts log at `DEBUG`, and a single
  `INFO` summary fires every 60 s with the delta and cumulative count.
  On a Gigabyte X870E AORUS MASTER (IT8696E) this drops journal volume
  from ~3,600 entries/hour per active hwmon-controlled header to ~60/hr,
  while preserving the existing remediation behaviour.

### Unchanged (explicitly verified)
- The watchdog still acts on **every** reclaim event — only the log
  emission is throttled. Manual mode (`pwm_enable=1`) is re-written and
  the PWM value re-issued exactly as before.
- The cumulative `enable_revert_counts` figure exposed via
  `GET /diagnostics/hardware` increments per event, regardless of
  whether the event produced a `WARN`, `DEBUG`, or `INFO` line. Tests
  pin this invariant.

### Tests
- Six new unit tests in `pwm_control.rs` covering: first-event WARN,
  subsequent DEBUG within the 60 s window, single INFO summary at the
  interval boundary with correct delta, full one-hour schedule
  (`1 WARN + 59 INFO + 3540 DEBUG = 3600 events`), per-header state
  isolation, and the load-bearing "throttling never gates the counter"
  invariant.

## [1.5.1] — 2026-04-23

Follow-up audit remediation on v1.5.0. Pairs with **GUI v1.6.1**.
Three small wire-contract fixes plus documentation hygiene — no behavioural
change to the safety or control paths.

### Fixed
- **GPU "no fan path" error envelope.** `POST /gpu/{id}/fan/pwm` and
  `POST /gpu/{id}/fan/reset` now return HTTP 400 `feature_unavailable`
  (retryable:false, source:"validation") when the addressed GPU has
  neither a PMFW `fan_curve` nor legacy `pwm1` write path. Previously
  returned HTTP 400 with `hardware_unavailable` (retryable:true), which
  contradicts the documented contract (`hardware_unavailable` is a 503
  code and the condition is permanent for the device, not retryable).
  Two new integration tests lock in the new shape.
- **`POST /hwmon/{id}/verify` lease-expiry mapping.** The verify handler
  re-issues a PWM write after its up-front `validate_lease` check. If the
  lease TTL expired between those two points, the write error was being
  mapped to HTTP 500 `internal_error` instead of HTTP 403 `lease_required`.
  The handler now delegates to the shared `hwmon_control_error_response`
  mapper used by every sibling hwmon handler. Two new unit tests cover the
  mapping.
- **SSE `too_many_clients` source field.** `GET /events` now reports
  `source: "internal"` for the client-cap rejection (was `"validation"`,
  which is wrong for a transport-level condition — the request shape is
  fine, the server-side cap is the limiting factor).

### Added
- **`ErrorEnvelope::feature_unavailable`** — new helper for the "endpoint
  exists, device exists, device lacks this capability" case. Distinct
  from `hardware_unavailable` (transient / retryable) and
  `validation_error` (malformed request shape).

### Changed
- **Docs: stale working-doc link removed.** `AmdGpuCapability.pci_id`
  doc comment in `api/responses.rs` no longer points at the deleted
  GUI-side `docs/23_Contract_Mismatch_Backlog.md`; replaced with a
  reference to GUI `CHANGELOG.md` v1.6.0 and `DECISIONS.md` DEC-042.

## [1.5.0] — 2026-04-23

Contract-mismatch remediation (15-item cross-stack sweep). Pairs with
**GUI v1.6.0**. The headline change is M1 — the profile engine now applies
the full per-control tuning pipeline, so headless profile-mode output is
identical to GUI-driven output for the same profile. See the GUI's
`docs/23_Contract_Mismatch_Backlog.md` for the full investigation.

### Added
- **M1: Full tuning pipeline in the profile engine.** `evaluate_profile`
  previously applied only `offset_pct` and `minimum_pct`, silently ignoring
  `step_up_pct`, `step_down_pct`, `start_pct`, and `stop_pct` even though
  they were deserialised from the profile. The engine now applies all six
  stages in the same order as the GUI's `ControlLoopService._apply_tuning`:
  offset → minimum → step-rate limit → stop-snap → start-hysteresis →
  clamp. A new task-local `ProfileEngineState` tracks pre-rounding `f64`
  `last_output` per control across 1 Hz cycles, clears on profile-id
  change, and clears on deactivation. The wire PWM uses round-to-nearest
  so `49.6` becomes `50` (matches the GUI's `round(pwm_percent)`).
  Ten new unit tests cover the pipeline stages and state lifecycle.
- **M11: `/capabilities` and `/diagnostics/hardware` emit both `pci_id`
  and `pci_bdf`.** Same BDF string under both names so callers aligned to
  either convention keep working during the transition window. Legacy
  names are documented as deprecated; will be removed in a future major
  version. Three new serialization tests.
- **Integration tests for status-code consistency** (`daemon/tests/ipc_integration.rs`):
  `/hwmon/{id}/verify` returns 503 when no controller is present,
  `/gpu/{id}/fan/pwm` and `/gpu/{id}/fan/reset` return 404 for unknown
  GPU ids (validation, not hardware).

### Changed
- **M12: `/hwmon/{id}/verify` returns 503 `hardware_unavailable`** when
  the controller is absent, matching every sibling hwmon handler.
  Previously returned 404 `validation_error`, which implied the endpoint
  itself was missing.
- **M13: GPU fan write/reset `hardware_unavailable` now returns 503**,
  not 500. Four match arms in `gpu.rs` (legacy + PMFW, set + reset) were
  inconsistent with the documented contract. `spawn_blocking` task
  failures correctly remain 500 (`internal_error`); unknown GPU id
  correctly remains 404 (`validation_error`).

## [1.4.2] — 2026-04-22

Audit remediation. Pairs with GUI v1.5.2.

### Changed
- **Profile engine's hwmon phase now also respects `gui_active`.** Previously
  only OpenFan (DEC-074) and GPU (DEC-071) writes deferred when the GUI had
  written via the API in the last 30s — hwmon writes only skipped based on
  lease ownership, leaving a narrow race during GUI startup and lease
  lapses. The three phases now share `DaemonState::gui_active()` and behave
  uniformly (DEC-093).
- **Comment on `ControlMember.source`** extended from `"openfan" or "hwmon"`
  to also include `"amd_gpu"`, which the profile engine already dispatched on.

### Added
- **`DaemonState::gui_active()` helper** factored out of `profile_engine.rs`,
  covered by three unit tests (fresh cache, post-write, post-timeout).
- **Integration test for `GET /poll`** locking the top-level response shape
  consumed by the GUI's 1 Hz polling loop (`api_version`, `status`,
  `sensors`, `fans`).
- **Daemon `DECISIONS.md`** summarising daemon-relevant ADRs (DEC-049,
  DEC-053, DEC-070, DEC-071, DEC-073, DEC-074, DEC-093) and cross-referencing
  the authoritative GUI file.

## [1.4.1] — 2026-04-22

Code quality and maintainability improvements from comprehensive audit.

### Changed
- **Split `handlers.rs` monolith** (1930 lines) into 8 focused submodules:
  `status`, `openfan`, `gpu`, `hwmon_ctl`, `profile`, `config`,
  `hw_diagnostics`, and shared helpers in `mod.rs`. All API paths unchanged.
- **Deduplicated PWM conversion functions.** `percent_to_raw` and
  `raw_to_percent` consolidated into new `pwm` module, replacing 4 duplicate
  definitions across `serial/controller`, `hwmon/pwm_control`, `api/handlers`,
  and `polling`.
- **Extracted legacy GPU sysfs writes** from inline handler code into
  `gpu_fan::set_legacy_pwm()` and `gpu_fan::reset_legacy_to_auto()`. Pre-RDNA3
  GPU fan control is now testable and returns typed `HwmonError` instead of
  swallowing IO errors.
- **Deduplicated status-building logic.** `status_handler` and `poll_handler`
  now share `build_status_response()` instead of duplicating 30 lines of
  identical subsystem/uptime/GUI-last-seen construction.

### Removed
- **Dead code cleanup.** Removed unused `IpcError` and `ErrorKind` types from
  `error.rs` (never referenced outside their own module).

## [1.4.0] — 2026-04-21

Sensor metadata enrichment for GUI classification and tooltip support.

### Added
- **Sensor metadata enrichment.** `/sensors` and `/poll` API responses now
  include `chip_name` (hwmon driver name from sysfs) and `temp_type`
  (thermistor type code from `tempN_type` sysfs) fields for each sensor.
  Enables the GUI to classify sensors with provenance-aware confidence levels.
- **Expanded sensor classification coverage.** Daemon now reads and exposes
  driver metadata for nct6775 family, nct6683/6686/6687, asus_ec_sensors,
  asus_wmi_sensors, gigabyte_wmi, and sbtsi_temp drivers.
- **`tempN_type` sysfs reading** during sensor discovery. Type codes: 3 =
  thermal diode, 4 = thermistor, 5 = AMD TSI, 6 = Intel PECI. Absent when
  the driver does not expose type information.
- **Label-based heuristics** for sensor kind classification during discovery.
  AMD TSI labels map to CpuTemp kind, Intel PECI labels map to CpuTemp kind,
  improving automatic categorization without manual configuration.

## [1.3.0] — 2026-04-21

Motherboard PWM diagnostics: BIOS interference detection, board identification,
and PWM effectiveness verification.

### Added
- **`pwm_enable` watchdog.** Every `set_pwm()` call now reads back `pwm_enable`
  to detect BIOS/EC reclaim (Gigabyte SmartFan 5/6, MSI Smart Fan, etc.). If
  the firmware has overridden manual mode, the daemon re-writes `pwm_enable=1`
  and forces a full PWM re-write. Cumulative revert counts are tracked per
  header and exposed in `/diagnostics/hardware`.
- **DMI board identification.** `/diagnostics/hardware` now includes a `board`
  object with `vendor`, `name`, and `bios_version` from SMBIOS/DMI sysfs.
  Enables the GUI to provide vendor-specific guidance (e.g., Gigabyte SmartFan
  degenerate-curve workaround instructions).
- **`POST /hwmon/{header_id}/verify` endpoint.** Behavioural test that writes
  a test PWM value, waits 3 seconds, then reads back `pwm_enable`, PWM, and
  RPM to classify the result as `effective`, `pwm_enable_reverted`,
  `pwm_value_clamped`, `no_rpm_effect`, or `rpm_unavailable`. Requires a
  valid hwmon lease.
- **System suspend/resume detection.** The polling loop compares
  `CLOCK_BOOTTIME` vs `CLOCK_MONOTONIC` to detect resume events. On resume,
  all per-header `manual_mode_set` flags are cleared, forcing the next
  `set_pwm()` to re-establish manual mode. Combined with the watchdog, this
  handles the common "fans revert after suspend" problem.
- **`enable_revert_counts` in diagnostics.** The `hwmon` section of
  `/diagnostics/hardware` now includes a per-header map of cumulative BIOS
  reclaim events, allowing the GUI to surface interference warnings.

## [1.2.0] — 2026-04-21

Hardware diagnostics API expansion for the GUI's new hardware readiness feature.

### Added
- **`GET /diagnostics/hardware` endpoint.** Returns comprehensive hardware
  readiness data: detected hwmon chips with driver identification, kernel
  module load status, ACPI I/O port conflict detection, GPU diagnostic
  details (PCI device ID, revision, ppfeaturemask), and thermal safety
  rule state. Enables the GUI to surface actionable guidance for hardware
  that requires out-of-tree drivers or BIOS configuration.
- **`device_id` field in `/hwmon/headers` response.** Each PWM header now
  includes the stable device identifier (PCI BDF or platform device name)
  used for chip instance disambiguation.
- **GPU PCI details in `/capabilities` response.** `amd_gpu` section now
  includes `pci_device_id`, `pci_revision`, and `gpu_zero_rpm_available`
  fields for precise GPU identification and diagnostic display.
- **Thermal safety state in cache.** Profile engine now reports thermal
  override state ("normal", "emergency", "recovery") to the state cache,
  surfaced via the new diagnostics endpoint.
- **ACPI conflict detection.** Scans `/proc/ioports` at request time for
  I/O port range overlaps between ACPI OperationRegions and known Super
  I/O chip addresses (Nuvoton 0x0290–0x0299, ITE 0x0A40–0x0A4F, etc.).
- **Kernel module detection.** Reads `/proc/modules` to check which hwmon
  driver modules are loaded, cross-referenced against expected drivers for
  detected chip names. Identifies out-of-tree vs mainline status.

## [1.1.6] — 2026-04-17

Safety and robustness hardening from full audit pass.

### Added
- **Panic hook for hardware safety.** Installs a `std::panic::set_hook`
  handler at startup that attempts to restore GPU fan curves (write `r\n` +
  `c\n` to PMFW `fan_curve`) and reset hwmon `pwm_enable` to `2` (automatic)
  on unrecoverable panic. Uses `OnceLock` to share restore targets with the
  panic handler without locking.

### Fixed
- **GPU `reset_to_auto()` skips zero-RPM re-enable on curve reset failure.**
  If `fan_curve` reset (`r\n` + `c\n`) failed, the function returned early
  without re-enabling `fan_zero_rpm_enable`. Now always attempts zero-RPM
  re-enable regardless of curve reset outcome, since PMFW writes are
  non-atomic and partial failure is expected.
- **Silent `daemon_state.json` load failures.** `load_state()` used
  `unwrap_or_default()` which silently dropped parse and I/O errors. Now
  logs explicit warnings for both corrupt JSON and unreadable files before
  falling back to defaults.
- **Config tests flaky under parallel execution.** `profiles_default_*` tests
  mutated `HOME`/`XDG_CONFIG_HOME` env vars, causing races when tests ran
  concurrently. Extracted a pure `profile_search_dirs_for(home, xdg_config)`
  function and rewrote tests to call it directly without env var mutation.

## [1.1.5] — 2026-04-17

Packaging improvement. No daemon logic changes.

### Added
- **Automatic Super I/O module loading.** Ship
  `/etc/modules-load.d/control-ofc.conf` that loads `nct6775`, `it87`,
  `w83627ehf`, and `drivetemp` at boot via `systemd-modules-load`. These
  are ISA-port-based chipset drivers that the kernel cannot auto-detect
  — without them, motherboard fan headers and some sensors are invisible
  to the daemon. Loading a module for absent hardware is harmless.
- `lm_sensors` added as `optdepends` for users whose hardware requires
  `sensors-detect` beyond the built-in module list.
- Hardware sensor modules section added to `docs/USER_GUIDE.md`.
- Version bumped to 1.1.5 (`daemon/Cargo.toml`, `packaging/PKGBUILD`).

### Changed
- **Streamlined install messages.** `post_install` reduced from 31 lines to
  10 — essential action first (enable service), sensor module loading,
  next steps (install GUI, GPU kernel param, docs link). Niche content
  (udev rules, config paths, profile details) moved to `USER_GUIDE.md`.
- **`post_upgrade` trimmed.** Removed stale 1.1.0 migration message (shim
  already removed in 1.1.3). Auto-strip function retained as safety net.
- Cross-references between daemon and GUI packages in install messages.

## [1.1.3] — 2026-04-12

Security hardening, error handling cleanup, and test coverage pass.
All quality gates remain green at 312 tests (290 unit + 22 integration).

### Security
- **SEC-1:** Reject path traversal (`..`) in profile name lookup (`find_profile`).
- **SEC-2:** Bound serial `read_line` with `Read::take(4096)` to prevent OOM from
  a malfunctioning device sending data without a newline terminator.
- **SEC-4:** Reject `..` in profile search directory paths passed via API.
- **SEC-7:** Reject `..` and null bytes in serial port path validation
  (`real_transport.rs`).

### Fixed
- **SSE stream omitted GPU fans.** The SSE `events_handler` built fan entries
  inline instead of using the shared `build_fan_entries()` helper, so GPU fan
  state was missing from the real-time stream. Now shares the same builder as
  `/fans` and `/poll`.
- **SSE client limit had a TOCTOU race.** Replaced `fetch_add` counter with
  `compare_exchange` CAS loop so two clients arriving simultaneously cannot
  both pass the `SSE_MAX_CLIENTS` check.
- Calibration PWM restore failures now logged instead of silently dropped.
- Lease renewal failures now logged at WARN.
- SIGHUP config reload failures now logged at ERROR (previously only returned
  a string that was silently dropped in one branch).

### Changed
- **Removed stale `migrate_legacy_runtime_keys`.** The one-release migration
  shim for `[profiles]`/`[startup]` from `daemon.toml` → `runtime.toml` was
  past its v1.1.0 deadline. Removed dead code from `main.rs`.
- **Removed dead `ConfigError::NotFound` variant and unused `DaemonError` enum**
  from `error.rs`.
- **Service unit: `SupplementaryGroups` reduced to `uucp` only.** `dialout`
  (Debian/Ubuntu) was dropped because systemd rejects the entire directive if
  any named group does not exist on the host. On Arch/CachyOS (the primary
  target), only `uucp` exists. Debian/Ubuntu users should add `dialout` via
  a systemd drop-in override.
- Extracted `apply_config_reload()` from the SIGHUP handler for testability.
- Shared `build_fan_entries` / `build_sensor_entries` between REST handlers
  and SSE stream, eliminating ~60 lines of duplication.
- Version bumped to 1.1.3 (`daemon/Cargo.toml`, `packaging/PKGBUILD`).

### Added
- 7 new tests: SSE CAS client limiting, `GuardedStream` counter drop,
  config reload (3 unit tests), GPU fan profile engine member evaluation.

## [1.1.2] — 2026-04-11

Packaging / installation cleanup pass. No daemon code changes — all quality
gates (`fmt`, `clippy -D warnings`, `cargo test`) remain green at 305 tests.
Addresses P1/P2 findings from the installation & systemd-config audit.

### Changed
- **udev rules are now documentation-only.** The shipped
  `99-control-ofc.rules` moves from `/usr/lib/udev/rules.d/99-control-ofc.rules`
  (where it was matching nothing because it still contained XXXX/YYYY
  placeholders) to `/usr/share/doc/control-ofc-daemon/99-control-ofc.rules.example`.
  The daemon already auto-detects the OpenFanController via
  `serial/real_transport.rs::auto_detect_port`, so no udev rule is required
  for normal operation. Users who want a stable `/dev/control-ofc-controller`
  symlink can `install -m644` the example into `/etc/udev/rules.d/` and edit
  there — following the canonical override pattern from `udev(7)`, so edits
  survive package upgrades.
- **Example profile shipped.** `/etc/control-ofc/profiles/quiet.json` now
  exists on fresh installs as a schema-valid example with an intentionally
  empty `members` array — safe to leave in place, drives no fans until the
  user customises it. Added to `backup=()` so pacman preserves user edits
  across upgrades via the standard `.pacnew`/`.pacsave` flow.
- **Rewrote the udev rules file header** so it explicitly documents the
  override path (`/etc/udev/rules.d/` overrides `/usr/lib/udev/rules.d/`
  overrides `/usr/share/doc/...`), the VID/PID discovery command, and the
  fact that the rule is optional. Previously the header told users to
  `cp` a file that doesn't exist at the path it suggested.
- **Service unit: dropped redundant `ReadWritePaths=/run/control-ofc
  /var/lib/control-ofc`.** systemd.exec(5) guarantees `RuntimeDirectory=`
  and `StateDirectory=` paths are writable under `ProtectSystem=strict`
  without an explicit `ReadWritePaths=` entry
  ([systemd#29798](https://github.com/systemd/systemd/issues/29798)).
  Only the `/sys/class/hwmon` and `/sys/class/drm` paths still need
  explicit allow-listing.
- **`post_install` and `post_upgrade` echoes rewritten** to mention the
  example profile, the auto-detect behaviour (no udev rule needed), and
  the new docs-only rules path.

### Added
- **`post_upgrade` auto-strips legacy `[profiles]` / `[startup]`
  sections from `/etc/control-ofc/daemon.toml`.** ADR-002 marks those
  sections as hard parse errors in 1.2.0; previously users would hit a
  startup crash the moment they upgraded past the shim window. The hook
  now backs the original file up to `daemon.toml.pre-1.1.2.bak` and uses
  a conservative `awk` script (top-of-line section headers only) to
  rewrite it in place. Safe to re-run; no-op when the sections are
  already absent. Preserves mode/owner from the original via
  `chmod --reference=` / `chown --reference=`.

### Not changed (flagged in audit, verified OK as-is)
- `systemctl daemon-reload` and `udevadm control --reload-rules` on
  upgrade are already provided by the base `systemd` package via
  `/usr/share/libalpm/hooks/30-systemd-daemon-reload-system.hook` and
  `/usr/share/libalpm/hooks/35-systemd-udev-reload.hook`, which trigger
  on any file installed under `/usr/lib/systemd/system/*` or
  `/usr/lib/udev/rules.d/*`. Our PKGBUILD uses those paths, so duplicating
  the reload in our `.install` hook would fire the same hook twice per
  transaction. Dismissed as a false positive from the audit.
- `daemon_state.rs` and `main.rs::resolve_initial_profile` already log at
  INFO / WARN when the persisted profile path or state file is missing
  (`main.rs:406-408`, `daemon_state.rs:80-84`). The audit claim that
  `daemon_state.rs:170` hardcoded a runtime reference to `quiet.json`
  turned out to be a test string literal; no runtime fix needed.

### Risk notes
- The `post_upgrade` TOML rewrite touches admin-owned config, which the
  ADR-002 "daemon never rewrites admin config" rule normally prohibits.
  The rule is scoped to the daemon process; the pacman `.install` hook is
  the packaging system performing a documented migration, which is an
  established Arch pattern. The backup file makes the change reversible.
- No changes to thermal safety, sysfs writes, serial reconnect, profile
  engine, IPC server lifecycle, or any other safety-critical path.

## [1.1.1] — 2026-04-11

### Fixed
- **First-run failure when the binary is invoked directly.** A tester
  reported that running `control-ofc-daemon` from a terminal as a regular
  user produced `ERROR IPC server error: Permission denied (os error 13)`
  but the daemon kept running, with the profile engine and polling loops
  live but no way to reach them. Root cause: the IPC server task's
  `create_dir_all("/run/control-ofc")` / `UnixListener::bind` both require
  root and the systemd-managed `RuntimeDirectory=control-ofc`; the error
  was logged and then ignored instead of terminating the daemon. Fix:
  - New `preflight_check` in `main.rs` runs **before any subsystem
    spawns**. It verifies `geteuid() == 0`, creates and probes the state
    directory for writability, and binds the IPC socket (with stale-file
    removal and the 0o666 chmod). Any failure prints an actionable
    stderr message pointing to `sudo systemctl enable --now
    control-ofc-daemon` and exits(1) — no more half-started zombie.
  - `api::server::serve` now takes an already-bound `UnixListener`
    instead of a path, so the bind happens once, synchronously, at
    startup. The mkdir / stale-remove / bind / chmod dance moved out of
    the async task.
  - The main `tokio::select!` now watches an `ipc_dead_rx` channel; if
    the IPC task ever exits with an error post-startup, the main loop
    breaks and the daemon shuts down cleanly (GPU reset, hwmon restore,
    socket cleanup) instead of running headless.
  - Specific error messages for `PermissionDenied` (points to systemctl)
    and `AddrInUse` (points to `systemctl status control-ofc-daemon`).

### Added
- **`--allow-non-root` hidden developer flag.** Skips the preflight EUID
  check for devs who want to run the binary directly with overridden
  `ipc.socket_path` and `state.state_dir`. Not listed in user-facing docs;
  mentioned only in `daemon.md` under the Running section. Hwmon / GPU /
  serial writes still require root regardless, so this is strictly for
  local IPC experimentation.
- **`libc` dependency** (0.2) for the `geteuid` call. Tiny, stable,
  already transitively present.

### Changed
- **`post_install` hint reordered.** The "start via systemctl" line is
  now the first thing users see, with an explicit "do NOT run the binary
  directly" follow-up. Previously this hint sat fourth in the list and
  was easy to skip.
- **`daemon.md`** gained a "Running" section that explains systemd is the
  only supported invocation path and what the preflight failure looks
  like.
- **Version bumped to 1.1.1** (`daemon/Cargo.toml`, `packaging/PKGBUILD`).
  Per project policy: any change on top of a local 1.1.0 bumps to 1.1.1.

### Risk notes
- Pre-1.1.0 versions wrote `persist_profile_search_dirs` back to
  `daemon.toml` under `ProtectSystem=strict`, which would also have
  failed under systemd; the runtime.toml split in 1.1.0 already fixed
  that. 1.1.1 only closes the remaining "binary run by hand" failure
  mode.
- No changes to thermal safety, sysfs writers, profile engine, or shutdown
  cleanup paths. Scope is bounded to startup validation and IPC task
  lifecycle.

## [1.1.0] — 2026-04-11

### Added
- **Runtime config split (ADR-002).** Daemon-mutable settings now live in
  `/var/lib/control-ofc/runtime.toml`, separate from admin-owned
  `/etc/control-ofc/daemon.toml`. The split mirrors the NetworkManager
  `NetworkManager-intern.conf` pattern: admin config is loaded first, runtime
  config is overlaid on top, and only the runtime file is ever rewritten by
  the daemon. SIGHUP re-reads and re-applies both. Full rationale and
  alternatives in `docs/ADRs/002-runtime-config-split.md`.
- **`runtime_config.rs` module.** `RuntimeConfig` struct with
  `#[serde(deny_unknown_fields)]`, atomic `save_to` (tmp+rename, 0600), and
  11 unit tests covering load/save roundtrip, defaults, malformed handling,
  missing parent dir creation, and owner-only permissions.
- **`ErrorEnvelope::persistence_failed` constructor.** Returns the new
  `persistence_failed` error code with `retryable: true` and
  `source: "internal"` for handlers that cannot persist state to disk.
- **Packaging: `/etc/control-ofc/profiles` directory.** PKGBUILD now
  creates the admin profile drop-in directory so operators can deposit
  curves without a `mkdir -p` dance on first install.

### Fixed
- **`POST /config/profile-search-dirs` and `POST /config/startup-delay`
  were silently losing writes across restarts.** Under
  `ProtectSystem=strict`, `/etc/control-ofc` is not in `ReadWritePaths=`,
  so the previous handlers hit `EROFS` when rewriting `daemon.toml`. The
  write failure was logged at WARN and the in-memory state updated anyway,
  producing "daemon forgets my settings after reboot" reports. Handlers
  now persist to `runtime.toml` inside the state directory (which *is*
  a `StateDirectory=`-managed writable path), **persist before mutating
  in-memory state**, and return `HTTP 503 persistence_failed` on any
  write error so the GUI can surface the failure. State can no longer
  diverge between RAM and disk.
- **`daemon_state.rs` comment drift.** Stale comment claiming the parent
  state dir was 0o700 replaced with an accurate description of
  `StateDirectoryMode=` defaulting to 0o755 and the file's 0o600 bits
  being the actual confidentiality boundary.

### Changed
- **`daemon.toml` is no longer rewritten by the daemon.** Admin-authored
  comments and formatting are preserved across restarts and package
  upgrades. The `persist_profile_search_dirs` and `persist_startup_delay`
  functions (and their tests) have been deleted from `config.rs`.
- **`packaging/daemon.toml.example`** now documents only the admin-static
  keys and points to `runtime.toml` for the daemon-managed ones.
- **Version bumped to 1.1.0** (`daemon/Cargo.toml`, `packaging/PKGBUILD`).

### Migration (one-release shim; removed in 1.2.0)
- `DaemonConfig` still parses `[profiles]` and `[startup]` from
  `daemon.toml`. On first start after upgrade, `migrate_legacy_runtime_keys`
  copies those sections into `runtime.toml` if the runtime file does not
  already contain them. The legacy sections in `daemon.toml` are **not**
  deleted (the daemon never rewrites admin-owned config) but are shadowed
  from that point forward. An INFO line logs which keys were migrated.
- **1.2.0 will make `[profiles]` / `[startup]` in `daemon.toml` a hard
  parse error.** Operators should remove those sections at their leisure
  during the 1.1.x window.

### Future release candidate
- Optional `200 OK + { persisted: false, advisory: "..." }` contract for
  persistence failures, instead of 503. Documented in ADR-002 as deferred
  work; revisit if users report disk-full / read-only `/var/lib` scenarios
  where they still want the in-memory change to take effect.

## [1.0.1] — 2026-04-11

### Added
- **`.github/workflows/release-aur.yml`** — GitHub Actions workflow that publishes to the AUR automatically when a release tag (`v*.*.*`) is pushed. Strict verify-and-fail: refuses to publish if `packaging/PKGBUILD` was not bumped before tagging, or if its `sha256sums` does not match the GitHub release tarball. Delegates the AUR clone/commit/push to [`KSXGitHub/github-actions-deploy-aur@v4.1.2`](https://github.com/KSXGitHub/github-actions-deploy-aur), which runs inside an Arch container and regenerates `.SRCINFO` automatically. Requires a one-time `AUR_SSH_PRIVATE_KEY` repository secret.
- **`scripts/release-aur.sh`** — manual fallback that mirrors the workflow's behaviour. Verifies the GitHub tarball sha256 matches `packaging/PKGBUILD`, clones (or ff-pulls) `ssh://aur@aur.archlinux.org/control-ofc-daemon.git` into `~/Development/aur/control-ofc-daemon/`, regenerates `.SRCINFO` via `makepkg --printsrcinfo`, and commits/pushes with explicit confirmation prompts (`--yes` to skip, `--no-push` to stage only). Run from the repo root as `./scripts/release-aur.sh <version>` after bumping `packaging/PKGBUILD`.

### Fixed
- **Profile activation creates a write dead zone when the GUI was recently active.** `POST /profile/activate` swapped in the new profile but did not refresh `last_gui_write_at`, so if the GUI had written within the last `GUI_ACTIVITY_TIMEOUT` (30s) the profile engine continued deferring to the GUI while the GUI, believing nothing had changed (the profile name is identical), never issued a new write. Result: OpenFan fans held their previous PWM for up to a minute after activation. Fix: `activate_profile_handler` now calls `state.cache.record_gui_write()` immediately after applying the new profile, giving the GUI a fresh 30s window of exclusive write ownership over the new curves. The matching GUI-side fix (an explicit `reevaluate_now()` to bypass the suppressed `active_profile_changed` signal) is tracked in the GUI CHANGELOG.
- **Boot-time OpenFanController detection race.** On cold boot the daemon could
  start before the `cdc_acm` kernel module loaded, at which point systemd
  silently dropped `DeviceAllow=char-ttyACM rwm` (class unresolved in
  `/proc/devices`). The USB device then appeared shortly after, but every open
  returned `Operation not permitted` because the cgroup device filter never
  included a ttyACM rule. Manual `systemctl restart` masked the issue because
  `cdc_acm` was loaded by then. Fixed by adding
  `Wants=modprobe@cdc_acm.service` + `After=modprobe@cdc_acm.service` to the
  unit's `[Unit]` section, per the workaround documented in
  `systemd.resource-control(5)`. Reinstall the service file (or reinstall the
  package) and run `systemctl daemon-reload` to pick up the change.

## [1.0.0] — 2026-04-08

Content update establishing the daemon's canonical paths, service names, and
identifiers used from this release onward: the `control-ofc-daemon` crate and
binary, the `control-ofc-daemon.service` systemd unit, the
`/run/control-ofc/control-ofc.sock` Unix socket, the `/etc/control-ofc/` and
`/var/lib/control-ofc/` runtime directories, the `CONTROL_OFC_CONFIG`
environment variable, and the `99-control-ofc.rules` udev rules with the
`/dev/control-ofc-controller` symlink.

## [0.7.2] — 2026-04-08

### R70 — Pre-release Security Hardening (V5 Phase 6)

Addresses Rust daemon findings from the V5 Phase 6 security & dependencies audit.

- **S3 (P2):** State file (`daemon_state.json`) now explicitly set to 0o600 (owner-only) before atomic rename. Defense-in-depth — parent dir is already 0o700 via systemd `StateDirectory=`. Added permission verification test.
- **S4 (P3):** Documented why root is required and why `CapabilityBoundingSet` is intentionally deferred in `control-ofc-daemon.service`.
- **S5 (P3):** Documented that sysfs path inclusion in error responses is intentional (public paths, local-only socket, diagnostic value).

## [0.7.1] — 2026-04-08

### R68 — Pre-release API Contract Cleanup (V5 Phase 3)

Resolves F1 and F3 from the V5 Phase 3 cross-boundary API contract audit.

- **F1 (P2):** Removed dead `publish_interval_ms` field from `PollingConfig`. This was a telemetry vestige — never referenced by runtime code after the R52 telemetry de-scope. The field, its default function, validation rule (`must be >= poll_interval_ms`), startup log line, config example, and user guide entry are all removed. **Breaking:** existing `daemon.toml` files containing `publish_interval_ms` under `[polling]` will now fail to parse (`deny_unknown_fields`). Remove the line to fix.
- **F3 (P3):** Fixed health module docstring — replaced stale "telemetry stats" with "AIO stats".
- Deleted `rejects_publish_less_than_poll` test (tested only the removed validation).
- Updated 5 tests to remove `publish_interval_ms` from test TOML strings and assertions.

## [0.7.0] — 2026-04-07

### R67 — Pre-release Rust Daemon Quality Remediation (V5 Phase 1)

Resolves all 17 findings from the V5 Phase 1 Rust daemon code review.

#### Quality gates (F1)
- `cargo fmt` and `cargo clippy -D warnings` now pass cleanly.
- Derived `Default` on `StartupConfig`; use `.is_multiple_of()` idiom.

#### P1 — Fixed before release
- **F2:** Replaced all `serde_json::to_value().unwrap()/expect()` in handlers with a `json_ok()` helper that returns HTTP 500 with proper error envelope on serialization failure.
- **F3:** Error suppression in polling loops now logs every 60th consecutive error (~1/min) instead of going permanently silent after 4 failures.
- **F4:** Migrated `profile_search_dirs` from `std::sync::RwLock` to `parking_lot::RwLock` — no more poison-panic risk, consistent with rest of codebase.

#### P2 — Fixed soon
- **F5:** Thermal safety thresholds kept as compile-time constants (decision: configurability adds risk without clear demand). Documented in audit.
- **F6:** Created `constants.rs` module — consolidated 12+ scattered operational constants (stall threshold, SSE limits, GPU coalescing, serial baud, probe range, stop timeout, GUI activity timeout, etc.). Eliminated duplication (stall threshold x6 -> x1, channel count x2 -> x1).
- **F7:** Extracted `build_sensor_entries()` and `build_fan_entries()` helpers, eliminating ~120 lines of duplication between `sensors_handler`/`fans_handler` and `poll_handler`.
- **F8:** Config parse failures now log a warning before overwriting with defaults (was silent `unwrap_or_default`).
- **F9:** SSE client connection limit (max 5) with HTTP 503 rejection and proper error envelope. `GuardedStream` wrapper ensures counter accuracy on client disconnect.
- **F10:** `SystemTime` before-epoch fallback now logs and skips the sample instead of recording timestamp 0.
- **F11:** Removed duplicate `cmd.member_id.clone()` in GPU profile engine path.
- **F12:** Eliminated unnecessary full `PwmHeaderDescriptor` clone in `set_pwm()` — extracts only the needed fields.

#### P3 — Fixed for convenience
- **F13:** Replaced magic `255` in PWM raw conversion with `protocol::MAX_PWM`; replaced magic `5` GPU retry with `constants::GPU_PMFW_WRITE_RETRIES`; moved calibration `MAX_TEMP_C` to `constants::CALIBRATION_MAX_TEMP_C` with documented relationship to safety trigger.
- **F14:** Calibration parameter clamping now logs when user-requested values are modified.
- **F15:** Serial timeout error messages now report actual configured timeout instead of hardcoded 500ms.
- **F16:** `read_hwmon_fan_states` now logs at debug level when sysfs reads fail or headers are dropped.
- **F17:** Profile engine logs a warning when openfan commands are dropped due to malformed `member_id`.

#### Tests
- 6 new tests (json_ok, build_sensor/fan_entries, stall detection, malformed config), 290 total (268 unit + 22 integration).
- 4 compile-time const assertions guard safety invariants.

## [0.6.1] — 2026-04-07

### R65 — Configurable Startup Delay
- **Feature:** `[startup] delay_secs` in daemon.toml — configurable delay before device detection after boot (0-30s, default 0).
- **Feature:** `POST /config/startup-delay` API endpoint — GUI can set the delay, daemon persists to daemon.toml. Takes effect on next restart.
- 3 new config tests (parse, default, validation), 284 total.

## [0.6.0] — 2026-04-07

### R64 — Runtime Config Reload + Profile Search Dirs API
- **Feature:** SIGHUP config reload — daemon re-reads `daemon.toml` and updates profile search dirs in memory. Enables `systemctl reload control-ofc-daemon`.
- **Feature:** `POST /config/profile-search-dirs` API endpoint — GUI (or any client) can add profile search directories at runtime. Daemon validates, updates in-memory state, and persists to `daemon.toml` atomically.
- **Feature:** Multi-user support — each GUI user can register their profile directory via the API; the daemon merges all dirs and preserves `/etc/control-ofc/profiles`.
- **Fix:** `profile_search_dirs` in AppState is now `RwLock<Vec<PathBuf>>` — safely mutable at runtime.
- Added `ExecReload=/bin/kill -HUP $MAINPID` to systemd service file.
- Added `config_path` to AppState so handlers can persist config changes.
- 2 new config persistence tests, 281 total.

## [0.5.9] — 2026-04-07

### R63 — Fix Profile Activation Path Validation (completes R62)
- **Fix:** `default_profile_search_dirs()` now falls back to `/root/.config/control-ofc/profiles` when neither `HOME` nor `XDG_CONFIG_HOME` is set (common for systemd services running as root without `User=`).
- **Fix:** systemd service file now sets `Environment=HOME=/root` so the daemon's environment always has HOME.
- **Fix:** `activate_profile_handler` logs a warning when all configured search directories fail canonicalization (empty allowed list).
- 2 new config tests (HOME unset fallback, HOME set preference), 279 total.

## [0.5.8] — 2026-04-07

### R62 — Configurable Profile Search Directories
- **Feature:** Profile search directories now configurable via `[profiles] search_dirs` in `daemon.toml`. Replaces hardcoded HOME-based detection that failed when daemon runs as root.
- **Fix:** Path validation now canonicalizes both the incoming profile path AND each search directory before comparison (CWE-22 hardening).
- **Fix:** `find_profile()` now accepts explicit search dirs instead of using hardcoded paths internally.
- Updated `daemon.toml.example` with `[profiles]` section documentation.
- 3 new config tests, 274 total (251 unit + 22 integration + 1 existing).

## [0.3.0] — 2026-03-31

### Release Generalisation — Cross-System Readiness
- **Config path override:** Daemon config path now overridable via `--config` CLI arg or `$CONTROL_OFC_CONFIG` env var (default: `/etc/control-ofc/daemon.toml`). Supports container deployments and dev testing.
- **Serial fallback expanded:** Direct probe fallback now scans `/dev/ttyUSB0-9` in addition to `/dev/ttyACM0-9`, covering FTDI/CH340 adapters when libudev is unavailable.
- **Service file portability:** `DeviceAllow` now uses `char-ttyACM` and `char-ttyUSB` class wildcards instead of hardcoded `/dev/ttyACM0-1`. `SupplementaryGroups` includes both `uucp` (Arch) and `dialout` (Debian) — systemd ignores missing groups.
- **Documentation:** Added serial setup instructions, VID/PID discovery, udev rule configuration, and config override usage to USER_GUIDE and DEVELOPER_HANDOVER.
- 1 new test: `load_from_custom_path` (254 total)

### R50 — Daemon Persisted-State Hardening
- **Fix:** `daemon_state.json` writes failed with `EROFS (Read-only file system, os error 30)` under systemd `ProtectSystem=strict` sandbox
- **Root cause:** systemd service file was missing `StateDirectory=control-ofc` and `/var/lib/control-ofc` was not in `ReadWritePaths`
- Added `StateDirectory=control-ofc` to systemd unit — systemd now creates and manages `/var/lib/control-ofc` with correct ownership
- Added `/var/lib/control-ofc` to `ReadWritePaths` for belt-and-suspenders protection
- State directory now configurable via `[state] state_dir` in `daemon.toml` (default: `/var/lib/control-ofc`)
- `daemon_state.rs` rewritten to use `OnceLock<String>` for runtime-configurable state path
- State directory initialized from config at startup before any load/save operations

### Write-Path Sanity Check — hwmon + OpenFan Audit
- **hwmon coalescing:** Added per-header write state tracking (`last_commanded_pct`, `manual_mode_set`). Identical PWM writes now skip sysfs entirely (0 ops instead of 4). `pwm_enable` written once per lease instead of every call. State reset on lease release.
- **OpenFan gui_active check:** Profile engine now skips OpenFan writes when GUI was active in the last 30s, matching the existing GPU write deferral (prevents dual-writer contention).
- **No issues found:** sysfs scalar parsing is correct for standard hwmon files; hwmon dual-writer conflict properly handled by lease mechanism; serial transport mutex prevents concurrent writes; reconnect logic is write-free.

### R53 — GPU Fan Curve EINVAL Fix
- **Fix (P0):** `set_static_speed()` now reads the device's OD_RANGE before writing PMFW curve points. Speed is clamped to the device minimum (typically 15%) instead of passing through unchecked values that the driver rejects with EINVAL.
- **Root cause:** Profile engine evaluated curves to low percentages (e.g., 5-10% at idle), but PMFW firmware rejects fan speed below `OverDriveLimitsMin` (typically 15%). Point 0 failed first, aborting the entire write. Temperature values now use the device's actual range instead of hardcoded 25-100°C.
- **Fix (P1):** Profile engine now tracks failed GPU writes and suppresses retry until the speed changes or a 60-second cooldown elapses. Previously, a failed write was retried every second with no backoff (1 WARN/sec in journal).
- **Fix (P2):** Write error messages now include the actual values written (temp°C, speed%) for diagnosability.
- 1 new test: `set_static_speed_clamps_below_od_range_minimum` (253 total)

### R52 — Syslog/Telemetry De-Scope
- **Removed:** Complete telemetry module (syslog.rs, queue.rs, aggregator.rs, exporter.rs) — ~1,133 lines
- **Removed:** `TelemetryConfig` from daemon config, `/telemetry/status` and `/telemetry/config` endpoints
- **Removed:** Telemetry types from health state, staleness computation, capabilities response
- **Removed:** 49 telemetry-specific tests (301 → 252 total)
- **Breaking:** `daemon.toml` files with `[telemetry]` section now fail to parse — remove the section
- **Removed:** `[telemetry]` section from `daemon.toml.example`

### V4 Comprehensive Audit — Safety Fixes
- **Fix (P0):** Daemon now restores `pwm_enable=2` (automatic) for all hwmon headers on shutdown. Previously only GPU fans were reset — motherboard fans could be stuck in manual mode after a daemon crash.
- **Fix (P1):** Thermal safety override now logs errors at ERROR level instead of silently discarding them. Failed writes during thermal emergency use "THERMAL SAFETY" prefix for operator visibility.
- **Fix (P2):** Pre-RDNA3 GPU fallback path now propagates `pwm1_enable` write error instead of silently discarding it. The amdgpu driver rejects `pwm1` writes when not in manual mode — previously the enable error was dropped, causing a redundant `pwm1` write that also failed.
- 4 new tests (272 unit + 29 integration = 301 total)

## [0.2.0] — 2026-03-18

### Protocol Fix — OpenFanController Response Parsing
- **Fix:** Protocol decoder now accepts responses without closing `>` bracket (real Karanovic OpenFan firmware omits it)
- Auto-detect fallback: probes `/dev/ttyACM0..9` directly when libudev enumeration fails (systemd sandbox)
- FanController and polling loop now share a single serial connection (was incorrectly opening two)
- All serial probe attempts logged at INFO level for diagnostics
- Systemd unit updated with `DeviceAllow` for serial device access

### Hardware Polling & Serial Port Support
- Added hardware polling loops (`polling.rs`) — hwmon sensors and OpenFanController fans now polled on `poll_interval_ms`
- Added real serial transport (`serial/real_transport.rs`) using `serialport` crate (115200 baud)
- Added auto-detection of OpenFanController on `/dev/ttyACM*` and `/dev/ttyUSB*` (probes with `ReadAllRpm`)
- Daemon now initializes hwmon PWM controller from sysfs discovery at startup
- Daemon now initializes OpenFanController from configured or auto-detected serial port
- Polling preserves `last_commanded_pwm` from cache when updating RPM readings
- Log suppression after 3 consecutive poll errors per subsystem

### M8 — Finalisation: GUI-Ready Daemon Contract (v1)
- Added `GET /capabilities` endpoint — device capabilities, feature flags, safety limits
  - Devices: openfan (channels, RPM, write), hwmon (headers, lease), aio_hwmon/aio_usb (unsupported)
  - Features: write support flags, lease requirement, telemetry support/enabled
  - Limits: PWM ranges, safety floors, interval bounds
- Added `GET /hwmon/lease/status` — shows held/TTL/owner for GUI lease display
- Added `POST /hwmon/lease/renew` — extend lease TTL without release/retake
- Identity contract: all sensors/fans/headers include stable `id`, `label`, `source`, `kind`
- Measured vs commanded: `rpm` (hardware) and `last_commanded_pwm` (daemon-tracked) always separate
- Added systemd unit file (`packaging/control-ofc-daemon.service`) with security hardening
- Added `docs/DEVELOPER_HANDOVER.md` and `docs/USER_GUIDE.md`
- 11 new tests (219 total, incl. 29 integration)

### M7 — Telemetry Export (TCP Syslog, RFC5424)
- Added telemetry config model with 10 fields: poll/publish intervals, queue size, TCP timeouts, health interval, local log copy
- Added config validation with bounds checking (poll 500–5000ms, publish 1000–60000ms, queue ≥1, timeouts > 0)
- Added `Aggregator` — builds telemetry payloads from cache, filters to allowlisted temperature metrics only
- Added `TelemetryQueue` — bounded queue with drop-oldest backpressure and dropped counter
- Added RFC5424 syslog message builder with `<PRI>1 TIMESTAMP HOSTNAME APP-NAME PROCID MSGID - JSON` format
- Added octet-counting TCP framing: `<len> <syslog-message>`
- Added `TelemetryHandle` — shared state for runtime config, queue, and error tracking
- Added `export_loop` — TCP connection lifecycle with exponential backoff + jitter (capped at 60s), rate-limited error logging
- Added `aggregation_loop` — async poll/publish loop with configurable intervals, health event emission
- Added API endpoints:
  - `GET /telemetry/status` — enabled flag, destination, connection state, queue depth, dropped count, error count
  - `POST /telemetry/config` — enable/disable, host/port, intervals; validates and rejects invalid configs
- Runtime config updates without daemon restart; disabling clears queue and closes connection immediately
- Missing/stale readings included as `null` with reason in telemetry payload
- 43 new tests (208 total, incl. TCP end-to-end integration test with local server)

### M6 — Motherboard (hwmon) PWM Control + Lease Model
- Added PWM header discovery (`hwmon/pwm_discovery.rs`) with stable IDs (`hwmon:<chip>:<device>:pwm<N>:<label>`)
- Added lease/token model (`hwmon/lease.rs`) for exclusive hwmon PWM write access
  - 60-second TTL with automatic expiry
  - Take/release/validate operations
- Added `HwmonPwmController` (`hwmon/pwm_control.rs`) with lease enforcement and safety floors
  - 20% minimum for chassis fans, 30% for CPU/pump headers
  - Automatic `pwmN_enable` mode switching on first write per lease
  - `SysfsWriter` trait for mocked filesystem testing
- Added API endpoints:
  - `GET /hwmon/headers` — list discovered controllable PWM outputs
  - `POST /hwmon/lease/take` — acquire exclusive write lease
  - `POST /hwmon/lease/release` — release write lease
  - `POST /hwmon/{header_id}/pwm` — set PWM (requires lease)
- Error mapping: `lease_required` (403), `lease_already_held` (409), `validation_error` (400), `hardware_unavailable` (503)
- 42 new tests (165 total)

### M5 — OpenFanController Fan Control (Write Paths)
- Added `FanController` (`serial/controller.rs`) with per-channel and all-channel PWM control
- Added target RPM support (closed-loop mode via EMC2305)
- Added POST endpoints: `/fans/openfan/{channel}/pwm`, `/fans/openfan/pwm`, `/fans/openfan/{channel}/target_rpm`
- PWM percent (0-100) converted to raw (0-255) at the protocol boundary
- Safety: 0% PWM allowed for max 8s (stop timeout), non-zero values clamped to 20% minimum
- Command coalescing: duplicate PWM commands are skipped (idempotent)
- Cache tracks `last_commanded_pwm` per channel
- Error envelope: `validation_error` (400), `hardware_unavailable` (503)
- 28 new tests (123 total)

### M4 — IPC v1 (Read-Only Endpoints)
- HTTP over Unix domain socket using axum + tokio
- GET endpoints: `/status`, `/sensors`, `/fans`
- Standard error envelope with structured error responses
- Graceful shutdown via oneshot channel
- ADR-001: IPC transport decision documented

### M3 — Cache, Staleness, and Health Model
- In-memory `StateCache` with `RwLock`, batch updates, snapshot reads
- Staleness thresholds: OK <=2x, Warn 2x-5x, Crit >5x expected interval
- Deterministic health computation with injected time

### M2 — Sensor Collection (Read-Only)
- hwmon sysfs discovery with stable sensor IDs
- Temperature reads (CPU, GPU, disk, motherboard) via hwmon
- Chip classification (k10temp, amdgpu, nvme, ite, nct)

### M1 — OpenFanController Protocol Layer
- Serial protocol encoding/decoding (ASCII hex pairs)
- Command types: ReadAllRpm, ReadRpm, SetPwm, SetAllPwm, SetTargetRpm
- Transport trait with mock support

### M0 — Repo + Scaffolding
- Rust workspace with `daemon/` crate
- Config scaffolding with TOML + validation
- Structured error types
- Module layout for all planned subsystems

[Unreleased]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.6...HEAD
[1.1.6]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.5...v1.1.6
[1.1.5]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.4...v1.1.5
[1.1.4]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.3...v1.1.4
[1.1.3]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.2...v1.1.3
[1.1.2]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.1...v1.1.2
[1.1.1]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.1.0...v1.1.1
[1.1.0]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.0.1...v1.1.0
[1.0.1]: https://github.com/Plan-B-Development/control-ofc-daemon/compare/v1.0.0...v1.0.1
