#!/usr/bin/env zsh
# Awpfel — Request building, response parsing, async completions, hooks.
# Suggestion.text from the daemon is always the FULL command. The async
# phase carries its own kind (suggest/nl_preview/nl_apply), sequence, and
# line-revision stamp so a stale completion can never overwrite a newer
# BUFFER — including ABA restores via kill/undo or cursor jumps the
# plugin's own widgets never see. jq is required; no manual JSON fallback.

_awpfel_build_context_json() {
    local cwd="$(pwd)"
    local _last_cmd_ctx="${_AWPFEL_LAST_COMMAND:-}"
    if [[ -z "$_last_cmd_ctx" && -n "${_AWPFEL_LAST_RECORDED_COMMAND:-}" ]]; then
        _last_cmd_ctx="$_AWPFEL_LAST_RECORDED_COMMAND"
    fi
    # Bounded per-shell recent list (this shell only, newest last, capped
    # at 20). Passed as session_commands so the daemon can use live
    # shell context without any cross-terminal fallback.
    local sess_json="[]"
    if (( ${#_AWPFEL_SESSION_CMDS[@]} > 0 )); then
        local -a _sess_esc=()
        local _c
        for _c in "${_AWPFEL_SESSION_CMDS[@]}"; do
            _sess_esc+=("\"$(_awpfel_json_escape "$_c")\"")
        done
        sess_json="[${(j:, :)_sess_esc}]"
    fi
    jq -cn \
        --arg cwd "$cwd" \
        --arg last_cmd "$_last_cmd_ctx" \
        --argjson exit_code "${_AWPFEL_LAST_EXIT_CODE:-0}" \
        --argjson sess "$sess_json" \
        '{cwd: $cwd,
          last_command: (if $last_cmd == "" then null else $last_cmd end),
          last_exit_code: $exit_code,
          last_stderr: null,
          git_branch: null,
          git_status: null, session_commands: $sess, env_hints: []}' 2>/dev/null
}

_awpfel_build_request() {
    local input="$1" cursor="$2" skip_ai="$3"
    local ctx=$(_awpfel_build_context_json)
    jq -cn \
        --arg input "$input" \
        --argjson cursor "$cursor" \
        --argjson skip_ai "$skip_ai" \
        --argjson ctx "$ctx" \
        '{type: "suggest", input: $input, cursor_pos: $cursor, skip_ai: $skip_ai, context: $ctx}' 2>/dev/null
}

_awpfel_build_nl_request() {
    local query="$1"
    local ctx=$(_awpfel_build_context_json)
    jq -cn \
        --arg query "$query" \
        --argjson ctx "$ctx" \
        '{type: "nl_generate", query: $query, context: $ctx}' 2>/dev/null
}

# First suggestion text — always the FULL command, never a suffix.
# jq is required: no manual fallback (a warning-text ghost would be a
# wrong, executable-looking command).
_awpfel_extract_first_suggestion() {
    printf '%s\n' "$1" | jq -r '.suggestions[0].text // empty' 2>/dev/null
}

_awpfel_extract_nl_command() {
    printf '%s\n' "$1" | jq -r '.command // empty' 2>/dev/null
}

_awpfel_extract_hint_text() {
    printf '%s\n' "$1" | jq -r '.hint.text // empty' 2>/dev/null
}

_awpfel_extract_warning_text() {
    printf '%s\n' "$1" | jq -r '.warning.text // empty' 2>/dev/null
}

_awpfel_extract_need_ai() {
    printf '%s\n' "$1" | jq -r '.need_ai // "false"' 2>/dev/null
}

_awpfel_apply_suggest_response() {
    local response="$1"

    if [[ -z "$response" ]]; then
        _AWPFEL_NEED_AI=""
        _awpfel_clear_ghost
        return
    fi

    _AWPFEL_HINT="$(_awpfel_extract_hint_text "$response")"
    _AWPFEL_WARNING="$(_awpfel_extract_warning_text "$response")"
    _AWPFEL_NEED_AI="$(_awpfel_extract_need_ai "$response")"

    local full=$(_awpfel_extract_first_suggestion "$response")
    if [[ -n "$full" ]]; then
        _awpfel_render_ghost "$full"
    else
        _awpfel_clear_ghost
    fi

    _awpfel_render_hint
}

_awpfel_suggest_local() {
    if [[ -z "$BUFFER" || ! -S "$_AWPFEL_SOCKET" ]]; then
        _awpfel_clear_ghost
        _awpfel_clear_hint
        _awpfel_cancel_pending_ai
        return
    fi

    # Ghost only tracks the end of the line; mid-line cursors show nothing.
    if (( CURSOR != ${#BUFFER} )); then
        _awpfel_clear_ghost
        _awpfel_cancel_pending_ai
        return
    fi

    # "# description" stays preview-only: BUFFER is untouched until Tab.
    if [[ "$BUFFER" == "# "* ]]; then
        _awpfel_check_ai_result
        if (( ${#BUFFER} >= 4 )); then
            _awpfel_schedule_nl_preview
        else
            _awpfel_clear_ghost
            _awpfel_cancel_pending_ai
        fi
        return
    fi

    # Throttle the synchronous local lookup only. The async schedule
    # always runs for the current BUFFER, so a burst's final keystroke is
    # never starved — eligibility is derived from the live line (server
    # decides risk/features), never from a stale local response.
    _awpfel_check_ai_result
    _awpfel_line_sync
    local now_ms=$(_awpfel_now_ms)
    local elapsed=$(( now_ms - _AWPFEL_LAST_LOCAL_MS ))
    if (( elapsed >= _AWPFEL_LOCAL_THROTTLE_MS )); then
        _AWPFEL_LAST_LOCAL_MS=$now_ms
        local request=$(_awpfel_build_request "$BUFFER" "$CURSOR" true)
        local response
        response=$(_awpfel_send_nc "$request")
        _awpfel_apply_suggest_response "$response"
    fi

    _awpfel_schedule_suggest
}

_awpfel_ai_fd_close() {
    if [[ -z "${_AWPFEL_AI_FD:-}" ]]; then
        # No fd tracked: still drop a lingering fifo path, if any.
        if [[ -n "${_AWPFEL_AI_FIFO:-}" ]]; then
            rm -f "$_AWPFEL_AI_FIFO" 2>/dev/null
            _AWPFEL_AI_FIFO=""
        fi
        return 0
    fi
    # Detach first so a concurrent readiness event can never touch the
    # fd we may open next; clearing the var is what makes an old
    # callback a no-op (fd-mismatch), and the seq/rev/snapshot guards
    # inside _awpfel_check_ai_result make it a no-op for content.
    local _fd="$_AWPFEL_AI_FD"
    _AWPFEL_AI_FD=""
    { zle -F "$_fd" 2>/dev/null; } 2>/dev/null
    { exec {_fd}>&-; } 2>/dev/null
    if [[ -n "${_AWPFEL_AI_FIFO:-}" ]]; then
        rm -f "$_AWPFEL_AI_FIFO" 2>/dev/null
        _AWPFEL_AI_FIFO=""
    fi
    return 0
}

_awpfel_cancel_pending_ai() {
    if [[ -n "${_AWPFEL_AI_PID:-}" ]]; then
        kill "$_AWPFEL_AI_PID" 2>/dev/null
        _AWPFEL_AI_PID=""
    fi
    _awpfel_ai_fd_close
    _AWPFEL_AI_LOADING=0
    # Invalidate any in-flight completion so it cannot overwrite newer state.
    (( _AWPFEL_AI_SEQ++ ))
    _AWPFEL_AI_ACTIVE_SEQ=$_AWPFEL_AI_SEQ
    _AWPFEL_PENDING_KIND=""
    if [[ -n "${_AWPFEL_AI_RESULT_FILE:-}" ]]; then
        : > "$_AWPFEL_AI_RESULT_FILE" 2>/dev/null
    fi
}

_awpfel_on_ai_ready() {
    # Widget-context readiness handler (registered via `zle -F -w` so
    # BUFFER/CURSOR assignment and redisplay are legal). $1 is the
    # ready fd. Validates it is still the current fd so an old
    # callback can never close or consume the new request's fd;
    # content staleness (seq/rev/snapshot) is enforced unchanged by
    # _awpfel_check_ai_result.
    local _ready_fd="${1:-}"
    # Stale callback (fd already superseded): return before touching
    # anything, so the current request's fd and PID are never cleared.
    [[ -n "$_ready_fd" && "$_ready_fd" == "${_AWPFEL_AI_FD:-}" ]] || return 0
    # The writer owned by this fd is finished: the marker is written
    # only after all request work, and EOF means the writer (and its
    # descendants) are gone — only subshell exit remains. Disown the
    # PID unconditionally so a later cancel can never signal a
    # recycled PID; no kill is needed for a completed writer.
    _AWPFEL_AI_PID=""
    local _discard
    # Drain exactly one readiness marker. The marker is written only
    # after the result file is committed, so a ready fd always has a
    # byte to consume. No blocking read: fd readiness is the signal,
    # the marker only resynchronizes the pipe. EOF (writer died before
    # writing) means no result will ever arrive — drop to idle without
    # applying anything.
    if IFS= read -r -u "$_ready_fd" _discard 2>/dev/null; then
        _awpfel_ai_fd_close
        _awpfel_check_ai_result
    else
        _awpfel_ai_fd_close
        _AWPFEL_AI_LOADING=0
    fi
    zle -R 2>/dev/null
    return 0
}

_awpfel_schedule_async() {
    local kind="$1" request="$2" snapshot="$3"
    _awpfel_cancel_pending_ai
    _awpfel_line_sync

    [[ -S "$_AWPFEL_SOCKET" ]] || return 1
    command -v socat &>/dev/null || return 1

    local delay="$_AWPFEL_AI_DELAY"
    if [[ "$kind" == "nl_preview" ]]; then
        delay=0.3
    elif [[ "$kind" == "nl_apply" ]]; then
        delay=0.1
    fi

    (( _AWPFEL_AI_SEQ++ ))
    _AWPFEL_AI_ACTIVE_SEQ=$_AWPFEL_AI_SEQ
    _AWPFEL_PENDING_KIND="$kind"
    _AWPFEL_AI_SNAPSHOT="$snapshot"
    _AWPFEL_ASYNC_LINE_REV=$_AWPFEL_LINE_REV
    local socket="$_AWPFEL_SOCKET"
    local seq=$_AWPFEL_AI_SEQ
    local rev=$_AWPFEL_LINE_REV
    local result_file="$_AWPFEL_AI_RESULT_FILE"
    local token_file="${TMPDIR:-/tmp}/.awpfel-ai-token-$$"

    echo "$seq" > "$token_file"

    # Readiness pipe: per-request private FIFO. The background job
    # opens the write end first and holds it for its whole lifetime;
    # the parent opens the read end (plain `<`, never RDWR). The job
    # writes one marker byte AFTER committing the result file, then
    # closes its end. No signals involved. Because the parent holds
    # no write end, a dead writer surfaces as EOF on the read end —
    # the readiness handler treats that as "no result will arrive"
    # and drops to idle without applying anything.
    local fifo="${TMPDIR:-/tmp}/.awpfel-ai-fifo-$$-${seq}"
    rm -f "$fifo" 2>/dev/null
    mkfifo -m 600 "$fifo" 2>/dev/null || { _AWPFEL_PENDING_KIND=""; return 1 }
    (
        # Child keeps the write end open from birth so the parent's
        # blocking `exec < fifo` below always has a writer to meet.
        exec 9> "$fifo"
        sleep "$delay" 2>/dev/null
        [[ "$(cat "$token_file" 2>/dev/null)" != "$seq" ]] && exit 0
        local result
        result=$(printf '%s\n' "$request" | socat -t 35 - UNIX-CONNECT:"$socket" 2>/dev/null)
        [[ "$(cat "$token_file" 2>/dev/null)" != "$seq" ]] && exit 0
        {
            printf '%s\n' "$seq"
            printf '%s\n' "$kind"
            printf '%s\n' "$rev"
            printf '%s\n' "$result"
        } > "$result_file" 2>/dev/null
        # Signal readiness only after the result file is committed, on
        # the already-held write end (never a path reopen: no block,
        # no recreating an unlinked fifo). Closing fd 9 at subshell
        # exit delivers EOF after the byte.
        printf '.\n' >&9 2>/dev/null || true
    ) &!
    _AWPFEL_AI_PID=$!
    local _fd=""
    { exec {_fd}< "$fifo"; } 2>/dev/null || { kill "$_AWPFEL_AI_PID" 2>/dev/null; _AWPFEL_AI_PID=""; _AWPFEL_PENDING_KIND=""; rm -f "$fifo" 2>/dev/null; return 1 }
    _AWPFEL_AI_FD="$_fd"
    _AWPFEL_AI_FIFO="$fifo"
    zle -F -w "$_fd" _awpfel_on_ai_ready 2>/dev/null
    _AWPFEL_AI_LOADING=1
}

_awpfel_schedule_suggest() {
    [[ ${#BUFFER} -ge 1 ]] || return 1
    _awpfel_line_sync
    local request=$(_awpfel_build_request "$BUFFER" "$CURSOR" false)
    _awpfel_schedule_async suggest "$request" "$BUFFER"
}

_awpfel_schedule_nl_preview() {
    local query="${BUFFER#\# }"
    [[ -n "$query" ]] || return 1
    _awpfel_line_sync
    local request=$(_awpfel_build_nl_request "$query")
    _awpfel_schedule_async nl_preview "$request" "$BUFFER"
}

# Ctrl-X A entry: transform the current BUFFER via NlGenerate. Success
# replaces BUFFER (cursor at end, never executes); failure/empty/stale
# preserves the original line untouched.
_awpfel_request_nl_apply() {
    [[ -n "$BUFFER" ]] || return 1
    local query="$BUFFER"
    if [[ "$query" == "# "* ]]; then
        query="${query#\# }"
    fi
    [[ -n "$query" ]] || return 1
    _awpfel_line_sync
    local request=$(_awpfel_build_nl_request "$query")
    _awpfel_schedule_async nl_apply "$request" "$BUFFER"
}

_awpfel_check_ai_result() {
    [[ -n "${_AWPFEL_AI_RESULT_FILE:-}" && -s "$_AWPFEL_AI_RESULT_FILE" ]] || return 0
    local line1 line2 line3 rest kind rev response
    { read -r line1 && read -r line2 && read -r line3 && rest=$(cat); } < "$_AWPFEL_AI_RESULT_FILE" 2>/dev/null || return 0
    : > "$_AWPFEL_AI_RESULT_FILE"
    _AWPFEL_AI_LOADING=0
    [[ -n "${line1:-}" && -n "${line2:-}" && -n "${line3:-}" ]] || return 0
    [[ "$line1" == "$_AWPFEL_AI_ACTIVE_SEQ" ]] || return 0
    [[ "$line2" == "$_AWPFEL_PENDING_KIND" ]] || return 0
    # Line-revision guard: any BUFFER/CURSOR change since scheduling
    # (kill/undo restores, word jumps, plain typing) invalidates the
    # result — including ABA restores detected via the redraw revision.
    _awpfel_line_redraw
    [[ "$line3" == "$_AWPFEL_LINE_REV" && "$line3" == "$_AWPFEL_ASYNC_LINE_REV" ]] || return 0
    kind="$line2"
    response="${rest:-}"
    case "$kind" in
        suggest)
            # Stale input (including ABA via seq+revision above): never render.
            [[ "$BUFFER" == "$_AWPFEL_AI_SNAPSHOT" ]] || return 0
            if (( CURSOR != ${#BUFFER} )); then
                _awpfel_clear_ghost
                return 0
            fi
            _AWPFEL_PENDING_KIND=""
            _awpfel_apply_suggest_response "$response"
            zle -R 2>/dev/null
            ;;
        nl_preview)
            [[ "$BUFFER" == "$_AWPFEL_AI_SNAPSHOT" ]] || return 0
            _AWPFEL_PENDING_KIND=""
            if [[ -z "$response" ]]; then
                _awpfel_clear_ghost
            else
                local nl_cmd_preview
                nl_cmd_preview=$(_awpfel_extract_nl_command "$response")
                if [[ -n "$nl_cmd_preview" ]]; then
                    _awpfel_render_ghost "$nl_cmd_preview"
                else
                    _awpfel_clear_ghost
                fi
            fi
            zle -R 2>/dev/null
            ;;
        nl_apply)
            # Stale or already-edited line: preserve the user's BUFFER.
            [[ "$BUFFER" == "$_AWPFEL_AI_SNAPSHOT" ]] || return 0
            _AWPFEL_PENDING_KIND=""
            [[ -n "$response" ]] || return 0
            local nl_cmd_apply
            nl_cmd_apply=$(_awpfel_extract_nl_command "$response")
            [[ -n "$nl_cmd_apply" ]] || return 0
            _awpfel_clear_ghost
            _awpfel_clear_hint
            BUFFER="$nl_cmd_apply"
            CURSOR=${#BUFFER}
            _awpfel_line_note_own_edit
            zle -R 2>/dev/null
            ;;
    esac
}

_awpfel_precmd() {
    _AWPFEL_LAST_EXIT_CODE=$?
    # Bounded per-shell recent list: newest last, capped at 20 entries.
    # _AWPFEL_LAST_COMMAND is the pending marker; the executed command is
    # preserved in _AWPFEL_LAST_RECORDED_COMMAND so context keeps sending
    # last_command instead of null after recording.
    if [[ -n "${_AWPFEL_LAST_COMMAND:-}" ]]; then
        _AWPFEL_LAST_RECORDED_COMMAND="$_AWPFEL_LAST_COMMAND"
        _AWPFEL_SESSION_CMDS+=("$_AWPFEL_LAST_COMMAND")
        if (( ${#_AWPFEL_SESSION_CMDS[@]} > 20 )); then
            _AWPFEL_SESSION_CMDS=("${_AWPFEL_SESSION_CMDS[@]: -20}")
        fi
        _AWPFEL_LAST_COMMAND=""
    fi
    if [[ -S "${_AWPFEL_SOCKET:-}" && ${#_AWPFEL_SESSION_CMDS[@]} -gt 0 ]]; then
        local record_request
        local _last_idx=${#_AWPFEL_SESSION_CMDS[@]}
        local _last_cmd="${_AWPFEL_SESSION_CMDS[$_last_idx]}"
        record_request=$(jq -cn \
            --arg cmd "$_last_cmd" \
            --argjson exit "${_AWPFEL_LAST_EXIT_CODE:-0}" \
            --arg cwd "$(pwd)" \
            '{type: "record", command: $cmd, exit_code: $exit,
              stderr: null, cwd: $cwd}' 2>/dev/null)

        _awpfel_send_nc "$record_request" &>/dev/null &!
    fi
}

_awpfel_preexec() {
    _AWPFEL_LAST_COMMAND="$1"
}
