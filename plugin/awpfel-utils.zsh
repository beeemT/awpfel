#!/usr/bin/env zsh
# Awpfel — Utility functions (JSON via jq, socket, daemon lifecycle, clock)

_awpfel_json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    printf '%s' "$s"
}

_awpfel_find_binary() {
    if [[ -x "${HOME}/.local/bin/awpfel" ]]; then
        _AWPFEL_BIN="${HOME}/.local/bin/awpfel"
    elif command -v awpfel &>/dev/null; then
        _AWPFEL_BIN="$(command -v awpfel)"
    elif [[ -x "${0:A:h}/../target/release/awpfel" ]]; then
        _AWPFEL_BIN="${0:A:h}/../target/release/awpfel"
    elif [[ -x "${0:A:h}/../target/debug/awpfel" ]]; then
        _AWPFEL_BIN="${0:A:h}/../target/debug/awpfel"
    fi
}

_awpfel_find_socket() {
    local uid=$(id -u)
    local xdg="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}"
    _AWPFEL_SOCKET="${xdg}/awpfel-${uid}.sock"
}

_awpfel_daemon_available() {
    [[ -n "${_AWPFEL_SOCKET:-}" && -S "$_AWPFEL_SOCKET" ]]
}

# Never start the model host directly: `awpfel start` owns a healthy local
# backend at its dedicated socket and leaves foreign processes alone.
_awpfel_ensure_daemon() {
    if [[ -z "$_AWPFEL_BIN" ]]; then
        return 1
    fi
    if [[ ! -S "$_AWPFEL_SOCKET" ]]; then
        "$_AWPFEL_BIN" start &!
        sleep 0.3
    fi
}

_awpfel_send_nc() {
    if [[ ! -S "$_AWPFEL_SOCKET" ]]; then
        return 1
    fi
    local request="$1"
    if command -v socat &>/dev/null; then
        printf '%s\n' "$request" | socat -T 0.1 -t 0.5 - UNIX-CONNECT:"$_AWPFEL_SOCKET" 2>/dev/null
    else
        if zmodload zsh/net/socket 2>/dev/null; then
            local fd
            zsocket "$_AWPFEL_SOCKET" && fd=$REPLY
            if [[ -n "$fd" ]]; then
                printf '%s\n' "$request" >&$fd
                local response
                read -r response <&$fd
                exec {fd}>&-
                printf '%s\n' "$response"
            fi
        fi
    fi
}

# Millisecond clock backing the local throttle. EPOCHREALTIME is only
# present with zsh/datetime loaded (awpfel_init loads it); fall back to
# $SECONDS-based milliseconds so throttling still works without it.
_awpfel_now_ms() {
    if (( ${+EPOCHREALTIME} )); then
        local secs="${EPOCHREALTIME%.*}"
        local frac="${EPOCHREALTIME#*.}"
        printf '%s' "${secs}${frac:0:3}"
    else
        printf '%d' $(( ${SECONDS:-0} * 1000 ))
    fi
}
