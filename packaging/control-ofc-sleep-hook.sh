#!/bin/bash
# systemd-sleep hook: widen the daemon's watchdog across a system sleep
# (TS-ao, DEC-396). Installed as /usr/lib/systemd/system-sleep/control-ofc-daemon.
#
# Why. The unit's watchdog (WatchdogSec=15, DEC-387) runs on CLOCK_MONOTONIC.
# That clock stops for the sleep itself, but user space - the daemon and PID 1
# alike - is frozen while devices suspend and resume, and that stretch counts.
# On a machine whose device suspend + resume takes more than ~10 s, PID 1 could
# thaw to find the deadline already passed and restart the daemon as it resumes.
#
# What. systemd-sleep runs this with "pre" just before the sleep and "post" just
# after, and holds the sleep until it exits. "pre" sends the daemon SIGUSR1 and
# waits (at most 2 s) for its acknowledgement, which the daemon writes only after
# it has sent systemd a widened WATCHDOG_USEC= - so the wide value is queued
# before anything freezes. "post" sends SIGUSR2 and the daemon puts its
# configured watchdog back. The daemon also narrows it by itself if "post" never
# comes, so a failure here can never leave detection wide for good.
#
# Safety of the signal. SIGUSR1's default action terminates a process, so this
# signals only a PID that BOTH the daemon's own PID file names (written once its
# handlers are live - an older daemon still running after an upgrade never writes
# it) AND systemd reports as the unit's MainPID. Anything else is a no-op. The
# hook always exits 0: nothing it does may stop the machine from sleeping.
#
# systemd documents hooks in this directory as a local mechanism ("should be
# considered hacks"); the alternative is a logind inhibitor over D-Bus, which
# the daemon does not link. This is the deliberate, dependency-free choice.

run_dir="${CONTROL_OFC_RUN_DIR:-/run/control-ofc}"
# Overridable only so daemon/tests/sleep_hook_script.rs can stand in for PID 1.
systemctl="${CONTROL_OFC_SYSTEMCTL:-systemctl}"
pid_file="$run_dir/sleep-hook.pid"
ack_file="$run_dir/sleep-hook.ack"

case "$1" in
    pre) signal=USR1 ;;
    post) signal=USR2 ;;
    *) exit 0 ;;
esac

[[ -r "$pid_file" ]] || exit 0
read -r pid <"$pid_file" || exit 0
[[ "$pid" =~ ^[1-9][0-9]*$ ]] || exit 0
main_pid="$("$systemctl" show -p MainPID --value control-ofc-daemon.service 2>/dev/null)" || exit 0
[[ "$pid" == "$main_pid" ]] || exit 0

rm -f "$ack_file"
kill -s "$signal" "$pid" 2>/dev/null || exit 0

if [[ "$1" == pre ]]; then
    for _ in {1..40}; do # 40 x 50 ms = 2 s
        # "pre <outcome>" only: an acknowledgement of anything else is not this one.
        if [[ -e "$ack_file" ]] && read -r phase outcome <"$ack_file" && [[ "$phase" == pre ]]; then
            if [[ "$outcome" == failed ]]; then
                echo "control-ofc: the daemon could not widen its watchdog for this sleep;" \
                    "it may be restarted on a slow resume" >&2
            fi
            exit 0
        fi
        sleep 0.05
    done
    echo "control-ofc: the daemon did not acknowledge the sleep within 2 s;" \
        "its watchdog may restart it on a slow resume" >&2
fi
exit 0
