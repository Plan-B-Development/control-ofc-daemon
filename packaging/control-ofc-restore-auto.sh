#!/bin/bash
# Give hwmon fans back and reset the GPU fan curves the daemon drove after it
# stops.
#
# This runs as ExecStopPost, so it runs after a clean stop and also after a
# crash, SIGKILL, an OOM kill or a panic: systemd.service(5) runs ExecStopPost=
# when the service "exited unexpectedly" as well.
#
# DEC-382: hwmon headers are given back EXACTLY as the daemon found them. Before
# it switches a header to manual mode, the daemon adds it to a record in its
# runtime directory, and this script replays that record: each header gets its
# recorded pwmN_enable back (and its duty, if it was already in manual mode),
# confirmed by reading it back, with lm-sensors fancontrol's fallback —
# pwmN_enable=0 (full speed), else manual at 255 — where that fails. A switch the
# driver makes write-only (dell_smm's, DEC-398) cannot be read back, so there the
# write succeeding is the confirmation. A header the daemon never took is not
# touched. This replaces a loop that wrote
# pwm_enable=2 to EVERY header on the machine: 2 is automatic only on it87. It is
# Thermal Cruise on nct6775, and on an NZXT Kraken (nzxt-kraken3) it uploads a
# curve buffer that is all zero unless something wrote it — a 0% pump curve.
#
# The hand-back rules below restate daemon/src/hwmon/handback.rs, which is the
# canonical copy; daemon/tests/restore_auto_script.rs holds the two together.
#
# DEC-414 (TS-aa): a second record, gpu-handback, in the same line format. The
# legacy (pre-RDNA3) GPU verify is the one path that puts an amdgpu fan in
# manual mode (pwm1_enable=1), and amdgpu is outside the hwmon ledger by design,
# so the verify records the card's original there before its write -- its mode,
# or "manual <raw>" with its duty when another tool had it in manual (DEC-447) --
# and drops the line once it has restored the fan. A line still present here is
# a verify that did not finish: its card gets its original back. A card no line
# names -- including one another tool put in manual mode -- is not touched.
#
# DEC-435 (DC-aa): a third record, gpu-pmfw-handback, for RDNA3+ cards driven
# through the PMFW fan_curve interface; see the loop at the end.
#
# DEC-199: the writes go through the /sys/class/hwmon and /sys/class/drm
# *symlinks*, but the service sandbox grants write access via
# ReadWritePaths=/sys/devices (the real backing path). A ReadWritePaths entry on
# the /sys/class/* symlink directories does NOT work — writes fail with EROFS.

# systemd sets RUNTIME_DIRECTORY for RuntimeDirectory=control-ofc; a list is
# ':'-separated, and this unit has one entry. CONTROL_OFC_SYSFS_ROOT exists only
# so the test suite can point this at a fake tree; the unit never sets it.
runtime_dir="${RUNTIME_DIRECTORY:-/run/control-ofc}"

# LIFE-a (DEC-487): a daemon refused because another one is running -- one
# started by hand holds the lock or serves on the socket -- exits 75
# (single_instance::ANOTHER_INSTANCE_EXIT_CODE) before it writes anything here.
# The records in this directory are the running daemon's, so replaying them
# would hand back the headers it drives. systemd passes how the main process
# ended as EXIT_CODE and EXIT_STATUS (systemd.exec(5)).
if [ "${EXIT_CODE:-}" = exited ] && [ "${EXIT_STATUS:-}" = 75 ]; then
    exit 0
fi

records=("${runtime_dir%%:*}/hwmon-handback" "${runtime_dir%%:*}/gpu-handback")
sysfs_root="${CONTROL_OFC_SYSFS_ROOT:-/sys}"

# The value of a sysfs attribute, whitespace stripped; fails if unreadable.
read_value() {
    local v
    v=$(cat "$1" 2>/dev/null) || return 1
    printf '%s' "${v//[[:space:]]/}"
}

# fancontrol's pwmdisable: no fan control (full speed), else manual at 255.
# 190 is fancontrol's own threshold — some chips cap the register below 255.
full_speed() {
    local enable=$1 pwm=$2 duty
    if echo 0 > "$enable" 2>/dev/null && [ "$(read_value "$enable")" = 0 ]; then
        return 0
    fi
    if echo 1 > "$enable" 2>/dev/null && echo 255 > "$pwm" 2>/dev/null; then
        duty=$(read_value "$pwm") && [ "$duty" -ge 190 ] 2>/dev/null && return 0
    fi
    return 1
}

# Give one header back; 0 = restored or at full speed, 1 = nothing could be written.
hand_back() {
    local enable=$1 pwm=$2 kind=$3 value=$4 mode
    case "$kind" in
        mode)
            if echo "$value" > "$enable" 2>/dev/null \
                && [ "$(read_value "$enable")" = "$value" ]; then
                return 0
            fi
            ;;
        write-only)
            # Nothing can read this switch back, so the write succeeding is the
            # only confirmation there is. Falling back after it would take back
            # the mode just given: the fallback's 1 is manual mode on dell_smm.
            if echo "$value" > "$enable" 2>/dev/null; then
                return 0
            fi
            ;;
        manual)
            if echo 1 > "$enable" 2>/dev/null && echo "$value" > "$pwm" 2>/dev/null; then
                mode=$(read_value "$enable")
                # Some drivers report mode 0 for manual at full scale (DEC-326).
                if [ "$mode" = 1 ] || { [ "$mode" = 0 ] && [ "$value" -ge 254 ] \
                    && [ "$(read_value "$pwm")" -ge 254 ] 2>/dev/null; }; then
                    return 0
                fi
            fi
            ;;
    esac
    full_speed "$enable" "$pwm"
}

for record in "${records[@]}"; do
    [ -r "$record" ] || continue
    while IFS=$'\t' read -r enable pwm kind value; do
        case "$enable" in '' | '#'*) continue ;; esac
        # The record is root's, in a root-owned directory, but a line is still
        # checked before anything is written to the path it names: both paths
        # under the sysfs root, the enable file belonging to that very pwm file.
        case "$pwm" in "$sysfs_root"/*) ;; *) continue ;; esac
        [ "$enable" = "${pwm}_enable" ] || continue
        [[ "$pwm" =~ /pwm[0-9]+$ ]] || continue
        case "$kind" in
            mode | write-only | manual)
                # Anything but a small integer is unrecorded: fall back.
                [[ "$value" =~ ^[0-9]{1,3}$ ]] && [ "$value" -le 255 ] || kind=full
                ;;
            full) ;;
            *) continue ;;
        esac
        [ -e "$enable" ] || continue
        hand_back "$enable" "$pwm" "$kind" "$value" \
            || echo "control-ofc-restore-auto: could not give $enable back" >&2
    done < "$record"
    # Consumed, as systemd used to consume it by emptying this directory: the
    # unit keeps the directory now (RuntimeDirectoryPreserve=yes), and a record
    # left here would be replayed again by a later start that fails before its
    # daemon writes its own -- over whatever has the headers by then.
    rm -f -- "$record"
done

# DEC-435 (DC-aa): a GPU fan curve is reset only on a card the daemon drove.
# Before its first PMFW fan_curve write to a card -- a profile's curve or a
# hardware verify -- the daemon names the card in gpu-pmfw-handback, and drops
# it once it has put the card back on firmware auto itself (POST
# /gpu/{id}/fan/reset). Each line is "<fan_curve>\t<fan_zero_rpm_enable or ->";
# its card gets the curve reset ("r", then "c") and firmware zero-RPM idle
# fan-stop back ("1", then "c"), which the daemon turns off before it writes a
# curve. A card no line names -- one LACT or CoreCtrl manages -- is not touched.
# This used to reset every card on the machine, replacing such a tool's curve on
# every stop and restart.
pmfw_record="${runtime_dir%%:*}/gpu-pmfw-handback"
if [ -r "$pmfw_record" ]; then
    while IFS=$'\t' read -r fan_curve zero_rpm; do
        case "$fan_curve" in '' | '#'*) continue ;; esac
        # Checked before anything is written, as a hwmon line is: a PMFW
        # fan_curve under the sysfs root, and a zero-RPM file only if it is that
        # card's own.
        case "$fan_curve" in
            *..*) continue ;;
            "$sysfs_root"/*/gpu_od/fan_ctrl/fan_curve) ;;
            *) continue ;;
        esac
        if [ -w "$fan_curve" ]; then
            { echo r > "$fan_curve" && echo c > "$fan_curve"; } 2>/dev/null \
                || echo "control-ofc-restore-auto: could not reset $fan_curve" >&2
        fi
        # Re-enabled even when the curve reset failed, as the daemon does: left
        # off, a fan that stopped at idle would run continuously.
        if [ "$zero_rpm" = "${fan_curve%/fan_curve}/fan_zero_rpm_enable" ] && [ -w "$zero_rpm" ]; then
            { echo 1 > "$zero_rpm" && echo c > "$zero_rpm"; } 2>/dev/null \
                || echo "control-ofc-restore-auto: could not re-enable $zero_rpm" >&2
        fi
    done < "$pmfw_record"
    rm -f -- "$pmfw_record" # consumed, as the hwmon records are above
fi

# ExecStopPost's status is not a verdict on the fans — each failure is reported
# above — and a non-zero exit here would only mark the unit failed after a clean
# stop.
exit 0
