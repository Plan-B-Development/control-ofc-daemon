#!/bin/bash
# Restore all hwmon fans to automatic mode (pwm_enable=2) after daemon stops.
#
# This runs as ExecStopPost so it executes even after SIGKILL, OOM, or panic.
# Without this, a daemon crash leaves motherboard fans stuck in manual mode
# (pwm_enable=1) with no BIOS thermal management.

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
