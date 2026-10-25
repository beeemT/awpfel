#!/usr/bin/env zsh
# Awpfel — Ghost completions from the on-device model.
# Smart when you need it. Silent when you don't.
#
# Requires jq (Homebrew/formula dependency). No manual JSON fallback.

# Guard BEFORE any destructive top-level state: re-sourcing after a
# successful init must preserve session/async state, never reset it.
if (( ${+_AWPFEL_INIT_DONE} )) && (( _AWPFEL_INIT_DONE )); then
    return 0 2>/dev/null || exit 0
fi

typeset -g _AWPFEL_PLUGIN_DIR="${0:h}"

typeset -g _AWPFEL_SUGGESTION=""
typeset -g _AWPFEL_HINT=""
typeset -g _AWPFEL_WARNING=""
typeset -g _AWPFEL_SOCKET=""
typeset -g _AWPFEL_BIN=""
typeset -g _AWPFEL_LAST_STDERR_FILE="${TMPDIR:-/tmp}/awpfel-stderr-$$"
typeset -g _AWPFEL_GHOST_STYLE="${AWPFEL_GHOST_STYLE:-fg=242}"
typeset -g _AWPFEL_STDERR_MAX="${AWPFEL_STDERR_MAX_CHARS:-500}"
typeset -ga _AWPFEL_HL_ENTRIES=()

# Async state: exactly one in-flight completion. Each request carries its
# kind (suggest / nl_preview / nl_apply) plus a snapshot of the BUFFER it
# was issued for; stale results (including ABA via line revision) are
# discarded.
typeset -g _AWPFEL_AI_PID=""
typeset -g _AWPFEL_AI_FD=""
typeset -g _AWPFEL_AI_FIFO=""
typeset -g _AWPFEL_AI_SNAPSHOT=""
typeset -g _AWPFEL_LAST_LINE_SEEN=""
typeset -g -i _AWPFEL_LINE_REV=0
typeset -g -i _AWPFEL_ASYNC_LINE_REV=0
typeset -g _AWPFEL_AI_SEQ=0
typeset -g _AWPFEL_AI_ACTIVE_SEQ=0
typeset -g _AWPFEL_PENDING_KIND=""
typeset -g _AWPFEL_NEED_AI=""
typeset -g _AWPFEL_AI_DELAY="${AWPFEL_AI_DELAY:-0.35}"
typeset -g _AWPFEL_AI_LOADING=0
typeset -g _AWPFEL_LOCAL_THROTTLE_MS="${AWPFEL_LOCAL_THROTTLE_MS:-20}"
typeset -g _AWPFEL_LAST_LOCAL_MS=0
typeset -g _AWPFEL_AI_RESULT_FILE=""

typeset -g _AWPFEL_LAST_COMMAND=""
typeset -g _AWPFEL_LAST_RECORDED_COMMAND=""
typeset -ga _AWPFEL_SESSION_CMDS=()
typeset -g _AWPFEL_LAST_EXIT_CODE=0
typeset -g -i _AWPFEL_INIT_DONE=0

# Source modules
source "${_AWPFEL_PLUGIN_DIR}/awpfel-utils.zsh"
source "${_AWPFEL_PLUGIN_DIR}/awpfel-render.zsh"
source "${_AWPFEL_PLUGIN_DIR}/awpfel-interact.zsh"
source "${_AWPFEL_PLUGIN_DIR}/awpfel-communicate.zsh"

_awpfel_trap_cleanup() {
    # zshexit: stop the owned async job, detach/close the readiness fd,
    # and remove private files. Never touches user traps.
    if [[ -n "${_AWPFEL_AI_PID:-}" ]]; then
        kill "$_AWPFEL_AI_PID" 2>/dev/null
        _AWPFEL_AI_PID=""
    fi
    _awpfel_ai_fd_close 2>/dev/null
    rm -f "$_AWPFEL_LAST_STDERR_FILE" "${TMPDIR:-/tmp}/.awpfel-ai-token-$$" "$_AWPFEL_AI_RESULT_FILE" 2>/dev/null
    rm -f "${TMPDIR:-/tmp}/.awpfel-ai-fifo-$$"* 2>/dev/null
}

# line-pre-redraw redraw watcher: any BUFFER/CURSOR change since the last
# check bumps the line revision and clears staged ghost state, so stale
# async (incl. ABA via Ctrl-U/undo, word jumps) can never land. Registered
# via add-zle-hook-widget (ZLE hook — add-zsh-hook cannot do this). Own
_awpfel_line_redraw() {
    local cur="${BUFFER:-}"$'\x1f'"${CURSOR:-0}"
    if [[ "$cur" != "${_AWPFEL_LAST_LINE_SEEN:-}" ]]; then
        _AWPFEL_LAST_LINE_SEEN="$cur"
        (( _AWPFEL_LINE_REV++ ))
        if [[ "$cur" != "${_AWPFEL_OWN_LINE_SEEN:-}" ]]; then
            _awpfel_clear_ghost
            _awpfel_cancel_pending_ai
        fi
        _AWPFEL_OWN_LINE_SEEN=""
    fi
}

_awpfel_line_sync() {
    _AWPFEL_LAST_LINE_SEEN="${BUFFER:-}"$'\x1f'"${CURSOR:-0}"
    _AWPFEL_OWN_LINE_SEEN=""
}

# Record an intentional plugin-side BUFFER change (accept paths): bumps
# the revision but stamps the async snapshot to match, so the selected
# ghost survives our own edit while foreign edits invalidate it.
_awpfel_line_note_own_edit() {
    _AWPFEL_OWN_LINE_SEEN="${BUFFER:-}"$'\x1f'"${CURSOR:-0}"
    _AWPFEL_LAST_LINE_SEEN="$_AWPFEL_OWN_LINE_SEEN"
    (( _AWPFEL_LINE_REV++ ))
    _AWPFEL_ASYNC_LINE_REV=$_AWPFEL_LINE_REV
    _AWPFEL_AI_SNAPSHOT="$BUFFER"
}

awpfel_init() {
    # Repeated sourcing must never duplicate widgets, hooks, or traps.
    if (( ${+_AWPFEL_INIT_DONE} )) && (( _AWPFEL_INIT_DONE )); then
        return 0
    fi

    if ! command -v jq &>/dev/null; then
        echo "awpfel: jq is required (brew install jq). Refusing to start without it." >&2
        return 1
    fi
    typeset -g _AWPFEL_HAS_JQ=1

    _awpfel_find_binary
    _awpfel_find_socket

    if [[ -z "$_AWPFEL_BIN" ]]; then
        echo "awpfel: binary not found. Install with: cargo install --path ."
        return 1
    fi

    # Millisecond clock for the local throttle: zsh/datetime gives real
    # EPOCHREALTIME; without it every keystroke in a wall-clock second
    # would collide and bursts would starve the async schedule.
    zmodload zsh/datetime 2>/dev/null

    typeset -g _AWPFEL_AI_RESULT_FILE="${TMPDIR:-/tmp}/.awpfel-ai-result-$$"
    : > "$_AWPFEL_AI_RESULT_FILE"

    autoload -Uz add-zsh-hook add-zle-hook-widget
    add-zsh-hook zshexit _awpfel_trap_cleanup
    add-zle-hook-widget line-pre-redraw _awpfel_line_redraw
    add-zsh-hook precmd _awpfel_precmd
    add-zsh-hook preexec _awpfel_preexec

    # Async arrival uses ZLE fd notification (a private per-request FIFO
    # watched with `zle -F -w`); no signal traps are ever installed, so
    # user-defined traps are never observed, let alone overwritten.
    # Resize redraw stays with ZLE's own POSTDISPLAY/highlight handling.

    _awpfel_ensure_daemon

    if [[ -S "$_AWPFEL_SOCKET" ]]; then
        local _cfg_resp
        _cfg_resp=$(_awpfel_send_nc '{"type":"status"}')
        if [[ -n "$_cfg_resp" ]]; then
            local _cv=""
            _cv=$(printf '%s\n' "$_cfg_resp" | jq -r '.config.ghost_text_color // empty' 2>/dev/null)
            [[ -z "${AWPFEL_GHOST_STYLE:-}" && -n "$_cv" ]] && _AWPFEL_GHOST_STYLE="fg=$_cv"
            _cv=$(printf '%s\n' "$_cfg_resp" | jq -r '.config.stderr_max_chars // empty' 2>/dev/null)
            [[ -z "${AWPFEL_STDERR_MAX_CHARS:-}" && -n "$_cv" ]] && _AWPFEL_STDERR_MAX="$_cv"
        fi
    fi

    zle -N _awpfel_self_insert
    zle -N _awpfel_backward_delete_char
    zle -N _awpfel_accept
    zle -N _awpfel_accept_chunk
    zle -N _awpfel_accept_word
    zle -N _awpfel_dismiss
    zle -N _awpfel_suggest_local
    zle -N _awpfel_tab
    zle -N _awpfel_on_ai_ready
    zle -N _awpfel_accept_line
    zle -N _awpfel_nl_transform
    zle -N _awpfel_backward_char

    if [[ "${AWPFEL_ENABLE_KEYBIND_OVERRIDE:-1}" == "1" ]]; then
        zle -N bracketed-paste _awpfel_bracketed_paste
        bindkey -M main '\e[C' _awpfel_accept
        bindkey -M main '\eOC' _awpfel_accept
        bindkey -M main '\e[1;5C' _awpfel_accept_word
        bindkey -M main '\e[27;5;67~' _awpfel_accept_word
        bindkey -M main '\e\e[C' _awpfel_accept_word
        # Shift-Tab: accept the next shell-word chunk of the ghost.
        bindkey -M main '\e[Z' _awpfel_accept_chunk
        # Esc dismisses the ghost.
        bindkey -M main '\e' _awpfel_dismiss
        # Enter executes BUFFER as-is; never auto-accepts ghost state.
        bindkey -M main '^M' _awpfel_accept_line

        # Ctrl-X then A: natural-language transform of the current BUFFER.
        bindkey -M main '^Xa' _awpfel_nl_transform

        local -a printable
        printable=({a..z} {A..Z} {0..9} ' ' '-' '_' '.' '/' '~' ':' '=' '+' '@' ',' ';' '!' '?' '#' '$' '%' '^' '&' '*' '(' ')' '[' ']' '{' '}' '<' '>' '|' "'" '"' '`' '\\')
        local key
        for key in "${printable[@]}"; do
            bindkey -M main -- "$key" _awpfel_self_insert
        done

        bindkey -M main '^?' _awpfel_backward_delete_char
        bindkey -M main '^H' _awpfel_backward_delete_char

        bindkey -M main '\e[D' _awpfel_backward_char
        bindkey -M main '\eOD' _awpfel_backward_char

        bindkey -M main '\t' _awpfel_tab
    fi

    _awpfel_line_sync
    _AWPFEL_INIT_DONE=1
}

awpfel_init
