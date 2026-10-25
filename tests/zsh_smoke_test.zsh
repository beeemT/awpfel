#!/usr/bin/env zsh
# Awpfel zsh plugin smoke tests — runs without a terminal or daemon.
#
# Behavioral coverage only (no source-text or stub-shape assertions):
# - full-command Tab accept incl. midword/nonprefix replacement preview
# - Enter executes BUFFER as-is, never auto-accepts ghost
# - NL success replaces BUFFER, failure/empty/stale preserves it
# - cancel of a prior async completion so it cannot overwrite a transform

set -uo pipefail

PASS=0
FAIL=0
PLUGIN_DIR="${0:A:h}/../plugin"
PLUGIN_FILE="$PLUGIN_DIR/awpfel.zsh"

fail() {
    FAIL=$((FAIL + 1))
    print -u2 "FAIL: $1"
    [[ -n "${2:-}" ]] && print -u2 "  $2"
}

pass() {
    PASS=$((PASS + 1))
}

assert_eq() {
    local label="$1" expected="$2" actual="$3"
    if [[ "$expected" == "$actual" ]]; then
        pass
    else
        fail "$label" "expected: $(printf '%q' "$expected") / actual: $(printf '%q' "$actual")"
    fi
}

assert_contains() {
    local label="$1" haystack="$2" needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        pass
    else
        fail "$label" "expected to contain: $needle / got: $haystack"
    fi
}

# ============================================================
# Load only function definitions (skip auto-init on last line)
# ============================================================

load_plugin_functions() {
    zle()          { : }
    bindkey()      { : }
    autoload()     { : }
    add-zsh-hook() { : }
    typeset -g _AWPFEL_PLUGIN_DIR="$PLUGIN_DIR"
    eval "$(sed -e '/^awpfel_init$/d' -e '/^typeset -g _AWPFEL_PLUGIN_DIR=/d' "$PLUGIN_FILE")"
}

load_plugin_functions

# Deterministic zsh harness: stub zle-driven display, drive BUFFER directly.
BUFFER=""
CURSOR=0
POSTDISPLAY=""
region_highlight=()
COLUMNS=80

_harness_zle() {
    case "${1:-}" in
        -R) : ;;
        -M) : ;;
        forward-char) (( CURSOR < ${#BUFFER} )) && (( CURSOR++ )) ;;
        forward-word)
            local rest="${BUFFER:$CURSOR}"
            if [[ "$rest" =~ ^[[:space:]]*[^[:space:]]+ ]]; then
                (( CURSOR += ${#MATCH} ))
            else
                CURSOR=${#BUFFER}
            fi
            ;;
        .self-insert) : ;;
        .backward-delete-char) : ;;
        .backward-char) (( CURSOR > 0 )) && (( CURSOR-- )) ;;
        .forward-char) (( CURSOR < ${#BUFFER} )) && (( CURSOR++ )) ;;
        .beginning-of-line) CURSOR=0 ;;
        .end-of-line) CURSOR=${#BUFFER} ;;
        .bracketed-paste) : ;;
        accept-line) _HARNESS_ACCEPTED_LINE="$BUFFER" ;;
        expand-or-complete) _HARNESS_EXPAND_CALLED=1 ;;
        *) : ;;
    esac
}
zle() { _harness_zle "$@" }

_HARNESS_ACCEPTED_LINE="<none>"
_HARNESS_EXPAND_CALLED=0

reset_harness() {
    BUFFER=""
    CURSOR=0
    POSTDISPLAY=""
    region_highlight=()
    _AWPFEL_SUGGESTION=""
    _AWPFEL_HINT=""
    _AWPFEL_WARNING=""
    _AWPFEL_PENDING_KIND=""
    _AWPFEL_NEED_AI=""
    _AWPFEL_AI_PID=""
    _AWPFEL_AI_LOADING=0
    _AWPFEL_AI_SNAPSHOT=""
    _AWPFEL_LAST_LINE_SEEN="0"
    _AWPFEL_OWN_LINE_SEEN=""
    _AWPFEL_LINE_REV=0
    _AWPFEL_ASYNC_LINE_REV=0
    _AWPFEL_AI_SEQ=0
    _AWPFEL_AI_ACTIVE_SEQ=0
    _AWPFEL_LAST_COMMAND=""
    _AWPFEL_LAST_RECORDED_COMMAND=""
    _AWPFEL_SESSION_CMDS=()
    _AWPFEL_LAST_EXIT_CODE=0
    _HARNESS_ACCEPTED_LINE="<none>"
    _HARNESS_EXPAND_CALLED=0
    _AWPFEL_HAS_JQ=1
    _awpfel_suggest_local() { : }
}

# Async test helper: stage a result file with seq/kind/line-rev + JSON.
# Defined before first use (partial-accept stale test uses it).
stage_result() {
    _AWPFEL_AI_RESULT_FILE="${TMPDIR:-/tmp}/.awpfel-test-result-$$"
    : > "$_AWPFEL_AI_RESULT_FILE"
    _AWPFEL_AI_SEQ=$1; _AWPFEL_AI_ACTIVE_SEQ=$1
    _AWPFEL_PENDING_KIND="$2"
    _AWPFEL_AI_SNAPSHOT="$3"
    _AWPFEL_ASYNC_LINE_REV=$_AWPFEL_LINE_REV
    _awpfel_line_sync
    printf '%s\n%s\n%s\n%s\n' "$1" "$2" "$_AWPFEL_LINE_REV" "$4" > "$_AWPFEL_AI_RESULT_FILE"
}

# ============================================================
# Test: full-command ghost — prefix shows suffix ghost
# (Shift-Tab partial accept is covered below; Esc dismisses ghost.)
# ============================================================

reset_harness
BUFFER="git ch"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
assert_eq "prefix: suggestion stores full command" "git checkout main" "$_AWPFEL_SUGGESTION"
assert_eq "prefix: ghost shows suffix only" "eckout main" "$POSTDISPLAY"

reset_harness
BUFFER="git ch"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
_awpfel_tab
assert_eq "tab prefix: buffer becomes full command" "git checkout main" "$BUFFER"
assert_eq "tab prefix: cursor at end" "${#BUFFER}" "$CURSOR"
assert_eq "tab prefix: ghost cleared" "" "$POSTDISPLAY"
assert_eq "tab prefix: suggestion cleared" "" "$_AWPFEL_SUGGESTION"

# ============================================================
# Test: nonprefix best match — visible replacement preview, Tab replaces
# (BUFFER "gco" is not a prefix of the full command, so the preview must
# show the whole replacement explicitly, never a bare suffix.)
# ============================================================

reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
assert_eq "nonprefix: suggestion stores full command" "git checkout main" "$_AWPFEL_SUGGESTION"
assert_contains "nonprefix: preview shows full command" "$POSTDISPLAY" "git checkout main"
assert_contains "nonprefix: preview marks replacement" "$POSTDISPLAY" "→"
if [[ "$POSTDISPLAY" == "gco"* ]]; then
    fail "nonprefix: must not masquerade as suffix ghost" "POSTDISPLAY=$POSTDISPLAY"
else
    pass
fi
# Ghost body highlight covers the whole shown command, first through
# last glyph; exclusive end matches the actual POSTDISPLAY length.
_ghost_body=""
for _hl_entry in "${region_highlight[@]}"; do
    if [[ "$_hl_entry" == *"$_AWPFEL_GHOST_STYLE"* ]]; then
        _ghost_body="$_hl_entry"
    fi
done
assert_contains "nonprefix: ghost body highlight present" "$_ghost_body" "$_AWPFEL_GHOST_STYLE"
_np_shown="${POSTDISPLAY#*→ }"
_np_prefix_len=$(( ${#POSTDISPLAY} - ${#_np_shown} ))
_np_exp_start=$(( ${#BUFFER} + _np_prefix_len ))
_np_exp_end=$(( _np_exp_start + ${#_np_shown} ))
_np_gb_start="${_ghost_body%% *}"
_np_gb_rest="${_ghost_body#* }"
_np_gb_end="${_np_gb_rest%% *}"
assert_eq "nonprefix: ghost body starts at command text" "$_np_exp_start" "$_np_gb_start"
assert_eq "nonprefix: ghost body ends at preview end" "$_np_exp_end" "$_np_gb_end"
assert_eq "nonprefix: ghost body covers shown text" "${#_np_shown}" "$(( _np_gb_end - _np_gb_start ))"


reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
_awpfel_tab
assert_eq "tab nonprefix: buffer replaced entirely" "git checkout main" "$BUFFER"
assert_eq "tab nonprefix: cursor at end" "${#BUFFER}" "$CURSOR"

# ============================================================
# Test: midword completion ("git swi" -> "git switch main") — Tab fills
# the FULL command, fixing the old suffix-splice bug
# ============================================================

reset_harness
BUFFER="git swi"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git switch main"
assert_eq "midword: suggestion stores full command" "git switch main" "$_AWPFEL_SUGGESTION"
_awpfel_tab
assert_eq "tab midword: buffer becomes full command" "git switch main" "$BUFFER"
assert_eq "tab midword: cursor at end" "${#BUFFER}" "$CURSOR"

# ============================================================
# Test: empty / identical input shows no ghost
# ============================================================

reset_harness
BUFFER="git status"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git status"
assert_eq "identical: no suggestion" "" "$_AWPFEL_SUGGESTION"
assert_eq "identical: no ghost" "" "$POSTDISPLAY"

reset_harness
BUFFER="git st"
CURSOR=${#BUFFER}
_awpfel_render_ghost ""
assert_eq "empty: no suggestion" "" "$_AWPFEL_SUGGESTION"
assert_eq "empty: no ghost" "" "$POSTDISPLAY"

# ============================================================
# Test: Enter never auto-accepts ghost, executes BUFFER as-is
# ============================================================

reset_harness
BUFFER="git swi"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git switch main"
_awpfel_accept_line
assert_eq "enter: executes typed buffer, not ghost" "git swi" "$_HARNESS_ACCEPTED_LINE"
assert_eq "enter: clears suggestion" "" "$_AWPFEL_SUGGESTION"
assert_eq "enter: clears ghost" "" "$POSTDISPLAY"

reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
# Enter must still execute exactly what is typed — no substitution.
_awpfel_accept_line
assert_eq "enter nonprefix: no invisible substitution" "gco" "$_HARNESS_ACCEPTED_LINE"

# ============================================================
# Test: Shift-Tab accepts one shell-word chunk, repeated presses
# accept chunk by chunk, remainder stays ghost
# ============================================================

reset_harness
BUFFER="git "
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
_awpfel_accept_chunk
assert_eq "chunk1: first shell word accepted" "git checkout " "$BUFFER"
assert_eq "chunk1: full suggestion kept" "git checkout main" "$_AWPFEL_SUGGESTION"
assert_eq "chunk1: remainder ghost" "main" "$POSTDISPLAY"
_awpfel_accept_chunk
assert_eq "chunk2: second press finishes" "git checkout main" "$BUFFER"
assert_eq "chunk2: suggestion cleared" "" "$_AWPFEL_SUGGESTION"
assert_eq "chunk2: ghost cleared" "" "$POSTDISPLAY"

# ============================================================
# Test: midword Shift-Tab accepts remainder of current word
# ============================================================

reset_harness
BUFFER="git swi"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git switch main"
_awpfel_accept_chunk
assert_eq "midword chunk: remainder of word plus space" "git switch " "$BUFFER"
assert_eq "midword chunk: full kept" "git switch main" "$_AWPFEL_SUGGESTION"
assert_eq "midword chunk: remainder ghost" "main" "$POSTDISPLAY"
_awpfel_accept_chunk
assert_eq "midword chunk2: finishes full command" "git switch main" "$BUFFER"

# ============================================================
# Test: nonprefix Shift-Tab accepts first chunk of proposal
# ============================================================

reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
_awpfel_accept_chunk
assert_eq "nonprefix chunk1: first proposed word" "git " "$BUFFER"
assert_eq "nonprefix chunk1: full kept" "git checkout main" "$_AWPFEL_SUGGESTION"
assert_eq "nonprefix chunk1: remainder ghost" "checkout main" "$POSTDISPLAY"

# ============================================================
# Test: quoted argument stays together as one chunk
# ============================================================
reset_harness
BUFFER="git commit -m "
CURSOR=${#BUFFER}
_awpfel_render_ghost 'git commit -m "fix the cache"'
_awpfel_accept_chunk
assert_eq "quoted chunk: whole quoted arg accepted" 'git commit -m "fix the cache"' "$BUFFER"
assert_eq "quoted chunk: suggestion consumed" "" "$_AWPFEL_SUGGESTION"
assert_eq "quoted chunk: remainder ghost cleared" "" "$POSTDISPLAY"

# ============================================================
# Test: Tab full accept still works alongside Shift-Tab chunks
# ============================================================

reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
_awpfel_accept_chunk
assert_eq "tab-after-chunk setup" "git " "$BUFFER"
_awpfel_tab
assert_eq "tab still accepts full command" "git checkout main" "$BUFFER"
assert_eq "tab still clears suggestion" "" "$_AWPFEL_SUGGESTION"

# ============================================================
# Test: partial accept cancels stale async, never executes
# ============================================================
reset_harness
BUFFER="git "
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
_awpfel_line_sync
_AWPFEL_LINE_REV=4
_AWPFEL_ASYNC_LINE_REV=4
stage_result 60 suggest "git " '{"type":"suggest","suggestions":[{"text":"git stale overwrite"}],"need_ai":false}'
_awpfel_accept_chunk
assert_eq "chunk: in-flight cancelled keeps selection" "git checkout " "$BUFFER"
_awpfel_check_ai_result
assert_eq "chunk: stale async cannot overwrite" "git checkout " "$BUFFER"
rm -f "$_AWPFEL_AI_RESULT_FILE"

# ============================================================
# Test: dismiss clears ghost
# ============================================================
reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
_awpfel_dismiss
assert_eq "dismiss: clears suggestion" "" "$_AWPFEL_SUGGESTION"
assert_eq "dismiss: clears ghost" "" "$POSTDISPLAY"

# ============================================================
# Test: complex lex — operators split, command substitution one chunk
# ============================================================
reset_harness
BUFFER="git commit -m "
CURSOR=${#BUFFER}
_marker="${TMPDIR:-/tmp}/.awpfel-chunk-noexec-$$"
rm -f "$_marker"
_full='git commit -m "fix the cache" && printf "pct" "$(touch __MARKER__; printf hi)"'
_full=${_full/__MARKER__/$_marker}
_awpfel_render_ghost "$_full"
_awpfel_accept_chunk
assert_eq "complex: quoted arg one chunk" 'git commit -m "fix the cache" ' "$BUFFER"
_awpfel_accept_chunk
assert_eq "complex: operator own chunk" 'git commit -m "fix the cache" && ' "$BUFFER"
_awpfel_accept_chunk
assert_eq "complex: word after operator" 'git commit -m "fix the cache" && printf ' "$BUFFER"
_awpfel_accept_chunk
assert_eq "complex: quoted word one chunk" 'git commit -m "fix the cache" && printf "pct" ' "$BUFFER"
_awpfel_accept_chunk
assert_eq "complex: command substitution one chunk" "$_full" "$BUFFER"
assert_eq "complex: suggestion consumed" "" "$_AWPFEL_SUGGESTION"
assert_eq "complex: ghost cleared" "" "$POSTDISPLAY"
if [[ -e "$_marker" ]]; then
    fail "complex: chunk accept must never evaluate model syntax" "marker $_marker was created"
else
    pass
fi
rm -f "$_marker"

# ============================================================
# Test: suggest apply — extract full text, apply ghost
# ============================================================

reset_harness
BUFFER="git ch"
CURSOR=${#BUFFER}
_resp='{"type":"suggest","suggestions":[{"text":"git checkout main"}],"need_ai":false}'
_full=$(_awpfel_extract_first_suggestion "$_resp")
assert_eq "extract: full command preserved" "git checkout main" "$_full"
_awpfel_apply_suggest_response "$_resp"
assert_eq "apply: ghost is suffix" "eckout main" "$POSTDISPLAY"

reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_resp='{"type":"suggest","suggestions":[{"text":"git checkout main"}],"need_ai":false}'
_awpfel_apply_suggest_response "$_resp"
assert_eq "apply nonprefix: keeps full command" "git checkout main" "$_AWPFEL_SUGGESTION"
assert_contains "apply nonprefix: visible preview" "$POSTDISPLAY" "git checkout main"
# Truncation ellipsis is the last glyph and stays inside the ghost
# body highlight; exclusive end matches the actual POSTDISPLAY.
COLUMNS=20
reset_harness
BUFFER="gco"
CURSOR=${#BUFFER}
_long_cmd="git checkout some-very-long-branch-name-that-overflows"
_awpfel_render_ghost "$_long_cmd"
assert_contains "truncated: ellipsis shown" "$POSTDISPLAY" "…"
_tr_ghost_body=""
for _hl_entry in "${region_highlight[@]}"; do
    if [[ "$_hl_entry" == *"$_AWPFEL_GHOST_STYLE"* ]]; then
        _tr_ghost_body="$_hl_entry"
    fi
done
_tr_shown="${POSTDISPLAY#*→ }"
_tr_prefix_len=$(( ${#POSTDISPLAY} - ${#_tr_shown} ))
_tr_exp_start=$(( ${#BUFFER} + _tr_prefix_len ))
_tr_exp_end=$(( _tr_exp_start + ${#_tr_shown} ))
_tr_gb_start="${_tr_ghost_body%% *}"
_tr_gb_rest="${_tr_ghost_body#* }"
_tr_gb_end="${_tr_gb_rest%% *}"
assert_eq "truncated: body covers ellipsis end" "$_tr_exp_end" "$_tr_gb_end"
assert_eq "truncated: body starts at command text" "$_tr_exp_start" "$_tr_gb_start"
COLUMNS=80


reset_harness
BUFFER="git ch"
CURSOR=${#BUFFER}
_awpfel_apply_suggest_response '{"type":"suggest","suggestions":[],"need_ai":false}'
assert_eq "apply empty: no fabricated suggestion" "" "$_AWPFEL_SUGGESTION"
assert_eq "apply empty: no ghost" "" "$POSTDISPLAY"

# ============================================================
# Test: NL success replaces BUFFER, failure/empty/stale preserves
# Result file carries seq/kind/line-rev + JSON; staged pending state
# must match or the landing path discards it untouched.
# ============================================================
reset_harness
BUFFER="list pods"
CURSOR=${#BUFFER}
_awpfel_line_sync
stage_result 7 nl_apply "list pods" '{"type":"nl_generate","command":"kubectl get pods"}'
_awpfel_check_ai_result
assert_eq "nl success: buffer replaced" "kubectl get pods" "$BUFFER"
assert_eq "nl success: cursor at end" "${#BUFFER}" "$CURSOR"
rm -f "$_AWPFEL_AI_RESULT_FILE"

reset_harness
BUFFER="list pods"
CURSOR=${#BUFFER}
_awpfel_line_sync
_awpfel_check_ai_result
assert_eq "nl empty: buffer preserved" "list pods" "$BUFFER"
rm -f "$_AWPFEL_AI_RESULT_FILE"

reset_harness
BUFFER="list pods"
CURSOR=${#BUFFER}
_awpfel_line_sync
stage_result 9 nl_apply "list pods" ''
_awpfel_check_ai_result
assert_eq "nl failure: buffer preserved" "list pods" "$BUFFER"
rm -f "$_AWPFEL_AI_RESULT_FILE"

reset_harness
BUFFER="list pods edited"
CURSOR=${#BUFFER}
_awpfel_line_sync
_AWPFEL_LINE_REV=5
_AWPFEL_ASYNC_LINE_REV=5
stage_result 10 nl_apply "list pods" '{"type":"nl_generate","command":"kubectl get pods"}'
_AWPFEL_LINE_REV=6
_awpfel_check_ai_result
assert_eq "nl stale (ABA): edited buffer preserved" "list pods edited" "$BUFFER"

# ============================================================
# Test: nl stale after undo restore — late result must not replace
# ============================================================
reset_harness
BUFFER="list pods"
CURSOR=${#BUFFER}
_awpfel_line_sync
stage_result 11 nl_apply "list pods" '{"type":"nl_generate","command":"kubectl get pods"}'
_old_rev=$_AWPFEL_LINE_REV
_old_seq=$_AWPFEL_AI_SEQ
# generic edit: line emptied, redraw observes the foreign change
BUFFER=""
CURSOR=0
_awpfel_line_redraw
# undo restores the original text, redraw observes again
BUFFER="list pods"
CURSOR=${#BUFFER}
_awpfel_line_redraw
# late arrival of the original request must be ignored even though
# BUFFER again matches the snapshot (ABA via revision + seq guards)
printf '%s\n%s\n%s\n%s\n' "$_old_seq" "nl_apply" "$_old_rev" '{"type":"nl_generate","command":"kubectl get pods"}' > "$_AWPFEL_AI_RESULT_FILE"
_awpfel_check_ai_result
assert_eq "nl undo-restore: stale apply must not replace restored line" "list pods" "$BUFFER"
rm -f "$_AWPFEL_AI_RESULT_FILE"

# ============================================================
# Test: cancel prior async — a superseded completion cannot overwrite
# ============================================================

reset_harness
BUFFER="git ch"
CURSOR=${#BUFFER}
_awpfel_line_sync
stage_result 20 suggest "git ch" '{"type":"suggest","suggestions":[{"text":"git checkout stale"}],"need_ai":false}'
_awpfel_cancel_pending_ai
# Late arrival carries the old seq/kind: must be ignored.
printf '%s\n%s\n%s\n%s\n' "20" "suggest" "0" '{"type":"suggest","suggestions":[{"text":"git checkout stale"}],"need_ai":false}' > "$_AWPFEL_AI_RESULT_FILE"
_awpfel_check_ai_result
assert_eq "cancel: stale suggest ignored" "" "$_AWPFEL_SUGGESTION"
assert_eq "cancel: no ghost from stale result" "" "$POSTDISPLAY"
rm -f "$_AWPFEL_AI_RESULT_FILE"

reset_harness
BUFFER="transform me"
CURSOR=${#BUFFER}
_awpfel_line_sync
stage_result 30 suggest "transform me" '{"type":"suggest","suggestions":[{"text":"transform me stale"}],"need_ai":false}'
# NL transform cancels the pending suggest, then its own success lands.
_awpfel_cancel_pending_ai
_awpfel_check_ai_result
assert_eq "cancel vs transform: suggest cannot overwrite" "transform me" "$BUFFER"
rm -f "$_AWPFEL_AI_RESULT_FILE"

# ============================================================
# Test: suggest stale snapshot (plain edits) ignored incl. ABA
# ============================================================

reset_harness
BUFFER="git checkout"
CURSOR=${#BUFFER}
_awpfel_line_sync
_AWPFEL_LINE_REV=8
_AWPFEL_ASYNC_LINE_REV=8
stage_result 40 suggest "git ch" '{"type":"suggest","suggestions":[{"text":"git checkout stale"}],"need_ai":false}'
_AWPFEL_LINE_REV=9
_awpfel_check_ai_result
assert_eq "suggest stale: ignored after edits" "" "$_AWPFEL_SUGGESTION"
rm -f "$_AWPFEL_AI_RESULT_FILE"

# ============================================================
# Test: cursor movement clears ghost (last-write-wins incl ABA)
# ============================================================

reset_harness
BUFFER="git ch"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
assert_eq "move setup: ghost present" "eckout main" "$POSTDISPLAY"
_awpfel_backward_char
assert_eq "move left: ghost cleared" "" "$POSTDISPLAY"
assert_eq "move left: suggestion cleared" "" "$_AWPFEL_SUGGESTION"
assert_eq "move left: cursor moved" "5" "$CURSOR"

# ============================================================
# Test: NL preview (# mode) stays preview-only until Tab
# ============================================================

reset_harness
BUFFER="# list pods"
CURSOR=${#BUFFER}
_awpfel_line_sync
stage_result 50 nl_preview "# list pods" '{"type":"nl_generate","command":"kubectl get pods"}'
_awpfel_check_ai_result
assert_eq "nl preview: buffer untouched" "# list pods" "$BUFFER"
assert_eq "nl preview: full command staged" "kubectl get pods" "$_AWPFEL_SUGGESTION"
assert_contains "nl preview: visible" "$POSTDISPLAY" "kubectl get pods"
_awpfel_tab
assert_eq "nl preview tab: buffer takes built command" "kubectl get pods" "$BUFFER"
rm -f "$_AWPFEL_AI_RESULT_FILE"

# ============================================================
# Test: per-shell session commands feed session_commands (bounded)
# ============================================================

reset_harness
_AWPFEL_LAST_COMMAND="git status"
_AWPFEL_SESSION_CMDS=()
_AWPFEL_LAST_EXIT_CODE=0
_AWPFEL_SOCKET="/nonexistent-awpfel-test.sock"
_AWPFEL_HAS_JQ=1
_awpfel_precmd
assert_eq "session: one command recorded" "1" "${#_AWPFEL_SESSION_CMDS[@]}"
assert_eq "session: recorded command" "git status" "${_AWPFEL_SESSION_CMDS[1]}"
_AWPFEL_LAST_COMMAND="git diff"
_awpfel_precmd
assert_eq "session: two commands recorded" "2" "${#_AWPFEL_SESSION_CMDS[@]}"
_AWPFEL_LAST_COMMAND="extra"
_awpfel_precmd
assert_eq "session: newest last" "extra" "${_AWPFEL_SESSION_CMDS[3]}"
assert_eq "session: context keeps last executed" "extra" "$_AWPFEL_LAST_RECORDED_COMMAND"
_ctx_json=$(_awpfel_build_context_json)
assert_contains "session: ctx carries commands" "$_ctx_json" "git status"
assert_contains "session: ctx carries latest" "$_ctx_json" "extra"
i=0
while (( i < 25 )); do _AWPFEL_LAST_COMMAND="cmd$i"; _awpfel_precmd; (( i++ )); done
assert_eq "session: bounded at 20" "20" "${#_AWPFEL_SESSION_CMDS[@]}"

# ============================================================
# Test: edits clear ghost (typed changes invalidate the preview)
# ============================================================

reset_harness
BUFFER="git ch"
CURSOR=${#BUFFER}
_awpfel_render_ghost "git checkout main"
BUFFER="git che"
CURSOR=${#BUFFER}
_awpfel_clear_ghost
assert_eq "edit clears ghost" "" "$POSTDISPLAY"
assert_eq "edit clears suggestion" "" "$_AWPFEL_SUGGESTION"

# ============================================================
# Test: fd helper never steals shell stderr (regression: bare
# `exec {fd}>&- 2>/dev/null` permanently redirected stderr to null)
# ============================================================

reset_harness
_AWPFEL_AI_FD=""
_AWPFEL_AI_FIFO="${TMPDIR:-/tmp}/.awpfel-test-fifo-$$"
rm -f "$_AWPFEL_AI_FIFO"
mkfifo -m 600 "$_AWPFEL_AI_FIFO"
( exec 9> "$_AWPFEL_AI_FIFO"; exit 0 ) &!
wait $! 2>/dev/null
exec {_awpfel_test_fd}< "$_AWPFEL_AI_FIFO"
_AWPFEL_AI_FD=$_awpfel_test_fd
_awpfel_ai_fd_close
_awpfel_stderr_probe=$(print -ru2 $((17 * 19)) 2>&1)
assert_eq "fd close preserves shell stderr" "323" "$_awpfel_stderr_probe"
rm -f "$_AWPFEL_AI_FIFO"

# Summary
# ============================================================

echo ""
echo "=== Awpfel zsh smoke tests ==="
echo "Passed: $PASS"
echo "Failed: $FAIL"
echo ""

if [[ $FAIL -gt 0 ]]; then
    exit 1
fi
exit 0
