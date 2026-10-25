#!/usr/bin/env bash
set -euo pipefail

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
RESET='\033[0m'

info() { echo -e "${GREEN}[info]${RESET} $1"; }
warn() { echo -e "${YELLOW}[warn]${RESET} $1"; }
error() { echo -e "${RED}[error]${RESET} $1"; }

INSTALL_DIR="${HOME}/.local/bin"
CONFIG_DIR="${HOME}/.config/awpfel"
PLUGIN_DIR="${CONFIG_DIR}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Source install runs on this machine only (macOS Apple Silicon, like releases).
if [[ "$(uname -s)" != "Darwin" ]]; then
    error "Unsupported OS: $(uname -s) (awpfel requires macOS)"
    exit 1
fi
if [[ "$(uname -m)" != "arm64" && "$(uname -m)" != "aarch64" ]]; then
    error "Unsupported architecture: $(uname -m) (awpfel requires Apple Silicon)"
    exit 1
fi
if ! command -v cargo &>/dev/null; then
    error "Rust toolchain not found. Install via: curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi

# Check for zsh
if ! command -v zsh &>/dev/null; then
    warn "zsh not found. Awpfel requires zsh for the shell plugin."
fi

# Check for required plugin helpers
if ! command -v jq &>/dev/null; then
    warn "jq not found. Install jq — the shell plugin needs it."
fi
if ! command -v socat &>/dev/null; then
    warn "socat not found. The plugin will fall back to zsh's built-in zsocket."
fi

info "Building awpfel (release mode)..."
cargo build --release --manifest-path "${SCRIPT_DIR}/Cargo.toml"

info "Installing binary to ${INSTALL_DIR}..."
mkdir -p "${INSTALL_DIR}"
cp "${SCRIPT_DIR}/target/release/awpfel" "${INSTALL_DIR}/awpfel"
chmod +x "${INSTALL_DIR}/awpfel"

# macOS: clear quarantine/provenance attrs and re-sign so Gatekeeper won't SIGKILL
if [[ "$(uname)" == "Darwin" ]]; then
    xattr -cr "${INSTALL_DIR}/awpfel" 2>/dev/null
    codesign -fs - "${INSTALL_DIR}/awpfel" 2>/dev/null
fi

info "Installing zsh plugin to ${PLUGIN_DIR}/..."
mkdir -p "${PLUGIN_DIR}"
for module in awpfel.zsh awpfel-utils.zsh awpfel-render.zsh awpfel-interact.zsh awpfel-communicate.zsh; do
    cp "${SCRIPT_DIR}/plugin/${module}" "${PLUGIN_DIR}/"
done

# Create default config if not exists (defaults target the managed local Apfel)
if [[ ! -f "${CONFIG_DIR}/config.toml" ]]; then
    info "Creating default config at ${CONFIG_DIR}/config.toml..."
    mkdir -p "${CONFIG_DIR}"
    "${INSTALL_DIR}/awpfel" config > "${CONFIG_DIR}/config.toml" || warn "Could not write default config."
    info "Defaults target the managed local Apfel (no API key needed)."
else
    info "Config file already exists, skipping."
fi

# Create data directory
mkdir -p "${HOME}/.local/share/awpfel"

echo ""
info "${BOLD}Awpfel installed successfully!${RESET}"
echo ""

# Interactive prompt helper: defaults to yes in non-interactive mode (pipe)
ask_yes() {
    local prompt="$1"
    if [[ ! -t 0 ]]; then
        echo -e "${prompt} [Y/n] Y (non-interactive, auto-yes)"
        return 0
    fi
    local answer
    read -rp "$(echo -e "${prompt} [Y/n] ")" answer
    answer="${answer:-Y}"
    [[ "$answer" =~ ^[Yy]$ ]]
}

# Check PATH and offer to fix
if [[ ":${PATH}:" != *":${INSTALL_DIR}:"* ]]; then
    ZSHRC="${HOME}/.zshrc"
    if [[ -f "$ZSHRC" ]] && grep -qF '.local/bin' "$ZSHRC"; then
        info "${INSTALL_DIR} PATH entry already in .zshrc"
    elif ask_yes "${BOLD}${INSTALL_DIR} is not in PATH. Add to ~/.zshrc?"; then
        {
            echo ""
            echo "# Added by Awpfel installer"
            # shellcheck disable=SC2016
            echo 'export PATH="${HOME}/.local/bin:${PATH}"'
        } >> "${HOME}/.zshrc"
        info "PATH entry added to ~/.zshrc"
        export PATH="${INSTALL_DIR}:${PATH}"
    else
        # shellcheck disable=SC2016
        warn 'Skipped. Add manually: export PATH="${HOME}/.local/bin:${PATH}"'
    fi
fi

# Shell integration is handled by `awpfel setup` (source line + autostart block).
# It runs next so a fresh install works without manual .zshrc edits.
"${INSTALL_DIR}/awpfel" setup --yes

echo ""
echo -e "${BOLD}Quick start:${RESET}"
echo "  Open a new terminal — Awpfel starts automatically (managed local Apfel)."
echo "  awpfel status   — check daemon status"
echo "  awpfel stop     — stop the daemon"
echo "  awpfel config   — view configuration"
echo ""
echo "  On first launch, Awpfel imports your zsh history automatically."
echo "  To import manually: awpfel history import"
echo ""
echo "AI runs on-device via the managed Apfel backend. No API key needed."
echo "To disable AI completions: set ai.enabled = false in ${CONFIG_DIR}/config.toml"
echo ""
echo -e "${BOLD}To uninstall:${RESET}"
echo "  awpfel uninstall --yes"
echo "  rm ~/.local/bin/awpfel"
echo "  rm -rf ~/.config/awpfel"
echo "  rm -rf ~/.local/share/awpfel"
