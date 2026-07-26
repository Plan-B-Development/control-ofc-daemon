#!/bin/bash
# Restore all hwmon fans to automatic mode (pwm_enable=2) after daemon stops.
#
# This runs as ExecStopPost so it executes even after SIGKILL, OOM, or panic.
# Without this, a daemon crash leaves motherboard fans stuck in manual mode
# (pwm_enable=1) with no BIOS thermal management.
#
# DEC-199: the writes below go through the /sys/class/hwmon and /sys/class/drm
# *symlinks*, but the service sandbox grants write access via
# ReadWritePaths=/sys/devices (the real backing path). A ReadWritePaths entry on
# the /sys/class/* symlink directories does NOT work — writes fail with EROFS.

# A no-match glob must expand to nothing, not the literal pattern, so the loops
# below simply skip on a machine with no hwmon PWM or GPU fan_curve nodes.
shopt -s nullglob

for pwm_enable in /sys/class/hwmon/hwmon*/pwm*_enable; do
    [ -w "$pwm_enable" ] && echo 2 > "$pwm_enable" 2>/dev/null
done

# Also reset GPU fan curves to auto if the sysfs paths exist
for fan_curve in /sys/class/drm/card*/device/gpu_od/fan_ctrl/fan_curve; do
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
for zero_rpm in /sys/class/drm/card*/device/gpu_od/fan_ctrl/fan_zero_rpm_enable; do
    if [ -w "$zero_rpm" ]; then
        echo 1 > "$zero_rpm" 2>/dev/null
        echo c > "$zero_rpm" 2>/dev/null
    fi
done
