#!/usr/bin/env zsh
# Awpfel — ZLE widget handlers.
#
# Product contract:
# - Tab with a suggestion: accept the FULL ghost command into BUFFER.
#   Tab without a suggestion: normal expand-or-complete (empty or not).
# - Shift-Tab with a suggestion: accept the next shell-word chunk only.
#   Repeated presses accept chunk by chunk; remainder stays ghost.
#   Quoted arguments and escaped spaces stay together; original
#   spacing/quotes preserved; model text is never evaluated.
#   Prefix/midword accepts the remainder of the current word plus
#   following whitespace; nonprefix replaces with the first chunk of
#   the proposed command. Never executes; never starts a new request.
# - Enter executes the current BUFFER as-is; it never auto-accepts a
#   ghost or opens/accepts anything by itself. Esc dismisses ghost.
# - Ctrl-X then A (^Xa): transform the current BUFFER via the NlGenerate
#   route. Success replaces BUFFER (cursor at end, never executes);
#   failure/empty/stale preserves the original line.

_awpfel_accept_full() {
    local full="$_AWPFEL_SUGGESTION"
    [[ -n "$full" ]] || return 0
    local had_multiline=0
    [[ "${POSTDISPLAY:-}" == *$'\n'* ]] && had_multiline=1
    _awpfel_cancel_pending_ai
    _awpfel_hl_clear
    BUFFER="$full"
    CURSOR=${#BUFFER}
    _awpfel_line_note_own_edit
    _AWPFEL_SUGGESTION=""
    POSTDISPLAY=""
    if (( had_multiline )); then
        # Full redraw FIRST: _suggest_local below can `zle -M` (hint) and
        # `zle -R`; reset flag must be set so those flush full, not incremental.
        zle .redisplay
    fi
    _awpfel_clear_hint
    _awpfel_suggest_local
    zle -R
}

_awpfel_accept() {
    if [[ -n "$_AWPFEL_SUGGESTION" ]]; then
        _awpfel_accept_full
    else
        zle forward-char
    fi
}

# Echo the accepted length (in chars) of the prefix of $1 to take next,
# starting from accepted length $2. Shell-word aware via ${(z)} (lex
# only, never evaluated): quoted strings, escaped spaces and $(...)
# stay together as single chunks; operators split separately. Original
# spacing is preserved by mapping tokens back onto the raw source.
# Falls back to blank-delimited words when lexing normalizes input.
_awpfel_next_chunk_len() {
    local full="$1" cur="$2"
    local flen=${#full}
    (( cur < 0 )) && cur=0
    (( cur > flen )) && cur=$flen
    local -a toks
    toks=("${(@z)full}")
    if (( ${#toks[@]} > 0 )); then
        local ok=1 pos=0
        local tok tlen end e
        for tok in "${toks[@]}"; do
            while (( pos < flen )) && [[ "${full:$pos:1}" == [[:blank:]] ]]; do (( pos++ )); done
            (( pos >= flen )) && break
            tlen=${#tok}
            if [[ "${full:$pos:$tlen}" != "$tok" ]]; then
                ok=0
                break
            fi
            end=$((pos+tlen))
            if (( end > cur )); then
                e=$end
                while (( e < flen )) && [[ "${full:$e:1}" == [[:blank:]] ]]; do (( e++ )); done
                (( e <= cur )) && { print -r -- $flen; return }
                print -r -- $e
                return
            fi
            pos=$end
        done
        if (( ok )); then
            print -r -- $flen
            return
        fi
    fi
    local p=$cur q=$cur
    (( p >= flen )) && { print -r -- $flen; return }
    while (( q < flen )) && [[ "${full:$q:1}" == [[:blank:]] ]]; do (( q++ )); done
    (( q >= flen )) && { print -r -- $flen; return }
    while (( q < flen )) && [[ "${full:$q:1}" != [[:blank:]] ]]; do (( q++ )); done
    while (( q < flen )) && [[ "${full:$q:1}" == [[:blank:]] ]]; do (( q++ )); done
    (( q <= cur )) && { print -r -- $flen; return }
    print -r -- $q
}

# Shift-Tab: accept one shell-word chunk of the current suggestion.
# Keeps the suggestion for the remainder; cancels in-flight async so a
# stale generation cannot overwrite the selection; never starts a new
# request and never executes.
_awpfel_accept_chunk() {
    [[ -n "${_AWPFEL_SUGGESTION:-}" ]] || return 0
    local full="$_AWPFEL_SUGGESTION"
    local input="$BUFFER"
    [[ -n "$full" ]] || return 0
    local had_multiline=0
    [[ "${POSTDISPLAY:-}" == *$'\n'* ]] && had_multiline=1
    _awpfel_cancel_pending_ai
    # Re-stage the accepted full command (cancel clears the slot), so
    # later chunks and Tab keep working off the same ghost.
    _AWPFEL_SUGGESTION="$full"
    local cur e newbuf
    if [[ -n "$input" && "$full" == "$input"* ]]; then
        cur=${#input}
    else
        cur=0
    fi
    e=$(_awpfel_next_chunk_len "$full" "$cur")
    (( e <= cur )) && e=${#full}
    (( e > ${#full} )) && e=${#full}
    newbuf="${full:0:$e}"
    BUFFER="$newbuf"
    CURSOR=${#BUFFER}
    _awpfel_line_note_own_edit
    _AWPFEL_SUGGESTION="$full"
    if [[ "$BUFFER" == "$full" ]]; then
        _AWPFEL_SUGGESTION=""
        POSTDISPLAY=""
        _awpfel_hl_clear
    else
        _awpfel_render_ghost "$full"
    fi
    if (( had_multiline )); then
        zle .redisplay
    fi
    zle -R
}

_awpfel_accept_word() {
    if [[ -n "$_AWPFEL_SUGGESTION" ]]; then
        _awpfel_accept_chunk
    else
        zle forward-word
    fi
}
_awpfel_dismiss() {
    _awpfel_cancel_pending_ai
    if [[ -n "$_AWPFEL_SUGGESTION" || -n "$POSTDISPLAY" || -n "$_AWPFEL_HINT" || -n "$_AWPFEL_WARNING" ]]; then
        _awpfel_hl_clear
        _awpfel_clear_ghost
        _awpfel_clear_hint
        zle -R
    fi
}

_awpfel_tab() {
    if [[ -n "$_AWPFEL_SUGGESTION" ]]; then
        _awpfel_accept_full
    else
        zle expand-or-complete
    fi
}

_awpfel_self_insert() {
    _awpfel_line_redraw
    zle .self-insert
    _awpfel_line_sync
    _awpfel_suggest_local
}

_awpfel_bracketed_paste() {
    _awpfel_line_redraw
    zle .bracketed-paste
    _awpfel_line_sync
    _awpfel_suggest_local
}

_awpfel_backward_delete_char() {
    _awpfel_line_redraw
    zle .backward-delete-char
    _awpfel_line_sync
    if [[ -z "$BUFFER" ]]; then
        _awpfel_clear_hint
        return
    fi
    _awpfel_suggest_local
}

_awpfel_move() {
    local widget="$1"
    _awpfel_line_redraw
    zle "$widget"
    _awpfel_line_redraw
}

_awpfel_backward_char() { _awpfel_move .backward-char }

_awpfel_nl_transform() {
    _awpfel_request_nl_apply
}

# Enter: execute exactly what is in BUFFER. Ghost state is cleared, never
# accepted — an unaccepted suggestion must not leak into the next line.
_awpfel_accept_line() {
    _awpfel_cancel_pending_ai
    _awpfel_hl_clear
    _AWPFEL_SUGGESTION=""
    POSTDISPLAY=""
    _awpfel_clear_hint
    zle accept-line
}
