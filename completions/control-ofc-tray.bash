# Bash completions for control-ofc-tray
_control_ofc_tray() {
    local cur prev opts
    COMPREPLY=()
    cur="${COMP_WORDS[COMP_CWORD]}"
    prev="${COMP_WORDS[COMP_CWORD-1]}"
    opts="--socket --version -h --help"

    if [[ ${prev} == "--socket" ]]; then
        COMPREPLY=( $(compgen -f -- "${cur}") )
        return
    fi

    if [[ ${cur} == -* ]]; then
        COMPREPLY=( $(compgen -W "${opts}" -- "${cur}") )
    fi
}
complete -F _control_ofc_tray control-ofc-tray
