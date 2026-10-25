#!/usr/bin/env zsh
# Awpfel — Ghost rendering. Suggestion.text from the daemon is always the
# FULL command; the UI only derives display text. Prefix matches show a
# suffix ghost, anything else shows an explicit next-line replacement
# preview. BUFFER never mutates until accept.

_awpfel_hl_clear() {
    local entry
    if (( ${+_AWPFEL_HL_ENTRIES} )) && (( ${#_AWPFEL_HL_ENTRIES} > 0 )); then
        if (( ${+region_highlight} )); then
            for entry in "${_AWPFEL_HL_ENTRIES[@]}"; do
                region_highlight=("${(@)region_highlight:#$entry}")
            done
        fi
        _AWPFEL_HL_ENTRIES=()
    fi
}

_awpfel_hl_add() {
    region_highlight+=("$1")
    _AWPFEL_HL_ENTRIES+=("$1")
}

_awpfel_hl_set_ghost() {
    region_highlight+=("$1")
    _AWPFEL_HL_ENTRIES+=("$1")
}

_awpfel_clear_ghost() {
    _AWPFEL_SUGGESTION=""
    POSTDISPLAY=""
    _awpfel_hl_clear
}

_awpfel_clear_hint() {
    _AWPFEL_HINT=""
    _AWPFEL_WARNING=""
    zle -M "" 2>/dev/null
}

_awpfel_render_hint() {
    if [[ -n "$_AWPFEL_WARNING" ]]; then
        zle -M "  ⚠ ${_AWPFEL_WARNING}" 2>/dev/null
    elif [[ -n "$_AWPFEL_HINT" ]]; then
        zle -M "  ⓘ ${_AWPFEL_HINT}" 2>/dev/null
    fi
}

# Render a FULL command suggestion for the current BUFFER.
_awpfel_render_ghost() {
    local full="$1"

    _awpfel_hl_clear

    if [[ -z "$full" || -z "$BUFFER" || "$full" == "$BUFFER" ]]; then
        _AWPFEL_SUGGESTION=""
        POSTDISPLAY=""
        return
    fi

    _AWPFEL_SUGGESTION="$full"

    if [[ "$full" == "$BUFFER"* ]]; then
        local ghost_part="${full#$BUFFER}"
        if [[ -n "$ghost_part" ]]; then
            local cols=${COLUMNS:-80}
            local available=$(( cols - ${#BUFFER} - 2 ))
            if (( available > 2 && ${#ghost_part} > available )); then
                ghost_part="${ghost_part[1,$((available - 1))]}…"
            elif (( available <= 2 )); then
                ghost_part=""
            fi
            if [[ -n "$ghost_part" ]]; then
                POSTDISPLAY="$ghost_part"
                _awpfel_hl_set_ghost "$#BUFFER $(( $#BUFFER + $#ghost_part )) $_AWPFEL_GHOST_STYLE"
            else
                POSTDISPLAY=""
            fi
        else
            POSTDISPLAY=""
        fi
    else
        # Non-prefix best match: explicit replacement preview, never an
        # invisible substitution. Tab replaces the entire BUFFER.
        local cols=${COLUMNS:-80}
        local max=$(( cols - 6 ))
        (( max < 10 )) && max=10
        local shown="$full"
        if (( ${#shown} > max )); then
            shown="${shown[1,$max]}…"
        fi
        local prefix=$'\n  → '
        local command_start=$(( ${#BUFFER} + ${#prefix} ))
        POSTDISPLAY="$prefix$shown"
        _awpfel_hl_add "$(( $#BUFFER + 1 )) $command_start fg=240"
        _awpfel_hl_add "$command_start $(( command_start + ${#shown} )) $_AWPFEL_GHOST_STYLE"
    fi
}
