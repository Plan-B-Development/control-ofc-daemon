# Bash completions for control-ofc-daemon
_control_ofc_daemon() {
    local cur opts
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    opts="--config --profile --profile-file --allow-non-root"

    if [[ ${cur} == -* ]]; then
        COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
    fi
}
complete -F _control_ofc_daemon control-ofc-daemon
