# Fish completions for control-ofc-daemon
complete -c control-ofc-daemon -l config -d 'Path to daemon.toml' -r -F
complete -c control-ofc-daemon -l profile -d 'Load a named profile from search paths' -r -x
complete -c control-ofc-daemon -l profile-file -d 'Load a profile from an absolute file path' -r -F
complete -c control-ofc-daemon -l allow-non-root -d 'Skip root privilege check (dev only)'
