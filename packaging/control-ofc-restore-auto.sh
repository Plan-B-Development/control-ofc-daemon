#!/bin/bash
# Give hwmon fans back and reset GPU fan curves after the daemon stops.
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
# DEC-199: the writes go through the /sys/class/hwmon and /sys/class/drm
# *symlinks*, but the service sandbox grants write access via
# ReadWritePaths=/sys/devices (the real backing path). A ReadWritePaths entry on
# the /sys/class/* symlink directories does NOT work — writes fail with EROFS.

# A no-match glob must expand to nothing, not the literal pattern, so the loops
# below simply skip on a machine with no GPU fan_curve nodes.
shopt -s nullglob

# systemd sets RUNTIME_DIRECTORY for RuntimeDirectory=control-ofc; a list is
# ':'-separated, and this unit has one entry. CONTROL_OFC_SYSFS_ROOT exists only
# so the test suite can point this at a fake tree; the unit never sets it.
runtime_dir="${RUNTIME_DIRECTORY:-/run/control-ofc}"
record="${runtime_dir%%:*}/hwmon-handback"
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

if [ -r "$record" ]; then
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
fi

# Also reset GPU fan curves to auto if the sysfs paths exist
for fan_curve in "$sysfs_root"/class/drm/card*/device/gpu_od/fan_ctrl/fan_curve; do
    if [ -w "$fan_curve" ]; then
        echo r > "$fan_curve" 2>/dev/null
        echo c > "$fan_curve" 2>/dev/null
    fi
done

# Re-enable PMFW fan zero-RPM (firmware idle fan-stop) on every GPU that
# exposes the sysfs file. The daemon disables zero-RPM before writing a
# manual curve and re-enables it on graceful shutdown / panic; this is the
# SIGKILL/OOM fallback. If we don't restore this, a fan that previously
# stopped at idle will run continuously after a daemon crash.
for zero_rpm in "$sysfs_root"/class/drm/card*/device/gpu_od/fan_ctrl/fan_zero_rpm_enable; do
    if [ -w "$zero_rpm" ]; then
        echo 1 > "$zero_rpm" 2>/dev/null
        echo c > "$zero_rpm" 2>/dev/null
    fi
done

# ExecStopPost's status is not a verdict on the fans — each failure is reported
# above — and a non-zero exit here would only mark the unit failed after a clean
# stop.
exit 0
