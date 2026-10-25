#!/bin/sh
set -eu

REPO="beeemT/awpfel"
INSTALL_DIR="${HOME}/.local/bin"
CONFIG_DIR="${HOME}/.config/awpfel"
PLUGIN_DIR="${CONFIG_DIR}"
DATA_DIR="${HOME}/.local/share/awpfel"

BOLD='\033[1m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
RED='\033[0;31m'
RESET='\033[0m'

info()  { printf "${GREEN}[info]${RESET} %s\n" "$1"; }
warn()  { printf "${YELLOW}[warn]${RESET} %s\n" "$1"; }
error() { printf "${RED}[error]${RESET} %s\n" "$1"; }

detect_platform() {
    # Awpfel releases ship macOS Apple Silicon only (Apfel is macOS-only).
    case "$(uname -s)" in
        Darwin) ;;
        *)      error "Unsupported OS: $(uname -s) (awpfel requires macOS)"; exit 1 ;;
    esac

    case "$(uname -m)" in
        arm64|aarch64) ;;
        *)             error "Unsupported architecture: $(uname -m) (awpfel requires Apple Silicon)"; exit 1 ;;
    esac
}

get_latest_version() {
    if [ -n "${AWPFEL_VERSION:-}" ]; then
        VERSION="${AWPFEL_VERSION}"
        return
    fi

    info "Fetching latest version..."
    if command -v curl >/dev/null 2>&1; then
        VERSION=$(curl -sSL "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' | head -1 | sed 's/.*"v\([^"]*\)".*/\1/')
    elif command -v wget >/dev/null 2>&1; then
        VERSION=$(wget -qO- "https://api.github.com/repos/${REPO}/releases/latest" \
            | grep '"tag_name"' | head -1 | sed 's/.*"v\([^"]*\)".*/\1/')
    else
        error "Neither curl nor wget found. Install one and retry."
        exit 1
    fi

    if [ -z "${VERSION}" ]; then
        error "Failed to determine latest version. Set AWPFEL_VERSION=x.y.z and retry."
        exit 1
    fi
}

download() {
    url="$1"
    output="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -sSL -o "${output}" "${url}"
    elif command -v wget >/dev/null 2>&1; then
        wget -qO "${output}" "${url}"
    fi
}

verify_checksum() {
    tarball="$1"
    checksums="$2"
    if command -v shasum >/dev/null 2>&1; then
        grep "$(basename "${tarball}")" "${checksums}" | shasum -a 256 -c --quiet -
    elif command -v sha256sum >/dev/null 2>&1; then
        grep "$(basename "${tarball}")" "${checksums}" | sha256sum -c --quiet -
    else
        warn "No sha256 tool found, skipping checksum verification."
        return 0
    fi
}

ask_yes() {
    prompt="$1"
    if [ ! -t 0 ]; then
        printf "%s [Y/n] Y (non-interactive, auto-yes)\n" "${prompt}"
        return 0
    fi
    printf "%s [Y/n] " "${prompt}"
    read -r answer
    answer="${answer:-Y}"
    case "${answer}" in
        [Yy]*) return 0 ;;
        *)     return 1 ;;
    esac
}

main() {
    detect_platform
    get_latest_version

    TARBALL="awpfel_${VERSION}_darwin_arm64.tar.gz"
    BASE_URL="https://github.com/${REPO}/releases/download/v${VERSION}"
    TMPDIR_DL="$(mktemp -d)"

    trap 'rm -rf "${TMPDIR_DL}"' EXIT

    info "Downloading Awpfel v${VERSION} (darwin arm64)..."
    download "${BASE_URL}/${TARBALL}" "${TMPDIR_DL}/${TARBALL}"
    download "${BASE_URL}/checksums.txt" "${TMPDIR_DL}/checksums.txt"

    info "Verifying checksum..."
    if ! (cd "${TMPDIR_DL}" && verify_checksum "${TARBALL}" "checksums.txt"); then
        error "Checksum verification failed!"
        exit 1
    fi

    info "Extracting..."
    tar xzf "${TMPDIR_DL}/${TARBALL}" -C "${TMPDIR_DL}"

    EXTRACTED="${TMPDIR_DL}"

    # Install binary
    info "Installing binary to ${INSTALL_DIR}/awpfel..."
    mkdir -p "${INSTALL_DIR}"
    cp "${EXTRACTED}/awpfel" "${INSTALL_DIR}/awpfel"
    chmod +x "${INSTALL_DIR}/awpfel"

    # macOS: clear quarantine + codesign
    if [ "$(uname -s)" = "Darwin" ]; then
        xattr -cr "${INSTALL_DIR}/awpfel" 2>/dev/null || true
        codesign -fs - "${INSTALL_DIR}/awpfel" 2>/dev/null || true
    fi

    # Install full plugin directory (entry + modules)
    info "Installing plugin to ${PLUGIN_DIR}/..."
    mkdir -p "${PLUGIN_DIR}"
    for module in awpfel.zsh awpfel-utils.zsh awpfel-render.zsh awpfel-interact.zsh awpfel-communicate.zsh; do
        cp "${EXTRACTED}/${module}" "${PLUGIN_DIR}/"
    done

    # Create default config if missing (defaults target the managed local Apfel)
    if [ ! -f "${CONFIG_DIR}/config.toml" ]; then
        info "Creating default config at ${CONFIG_DIR}/config.toml..."
        mkdir -p "${CONFIG_DIR}"
        "${INSTALL_DIR}/awpfel" config > "${CONFIG_DIR}/config.toml" || warn "Could not write default config."
    else
        info "Config already exists, preserving."
    fi

    # Check for the Apfel CLI (provides the on-device model server)
    if ! command -v apfel >/dev/null 2>&1; then
        warn "apfel not found. Install it for on-device AI: brew install beeemT/tap/awpfel"
    fi

    # Create data directory
    mkdir -p "${DATA_DIR}"

    printf "\n"
    info "${BOLD}Awpfel v${VERSION} installed successfully!${RESET}"
    printf "\n"

    # PATH setup
    case ":${PATH}:" in
        *":${INSTALL_DIR}:"*) ;;
        *)
            ZSHRC="${HOME}/.zshrc"
            if [ -f "${ZSHRC}" ] && grep -qF '.local/bin' "${ZSHRC}"; then
                info "${INSTALL_DIR} PATH entry already in .zshrc"
            elif ask_yes "Add ${INSTALL_DIR} to PATH in ~/.zshrc?"; then
                # shellcheck disable=SC2016
                printf '\n# Added by Awpfel installer\nexport PATH="${HOME}/.local/bin:${PATH}"\n' >> "${ZSHRC}"
                info "PATH entry added to ~/.zshrc"
            else
                warn "Skipped. Add manually: export PATH=\"\${HOME}/.local/bin:\${PATH}\""
            fi
            ;;
    esac

    # Shell integration is handled by `awpfel setup` (source line + autostart block).
    "${INSTALL_DIR}/awpfel" setup --yes

    # Required plugin helpers
    if ! command -v jq >/dev/null 2>&1; then
        warn "jq not found. Install jq — the shell plugin needs it."
    fi
    if ! command -v socat >/dev/null 2>&1; then
        warn "socat not found (falls back to zsh zsocket)."
    fi

    printf "\n"
    printf '%sQuick start:%s\n' "${BOLD}" "${RESET}"
    printf '  Open a new terminal — Awpfel starts automatically (managed local Apfel).\n'
    printf '  awpfel status   — check daemon status\n'
    printf '  awpfel stop     — stop the daemon\n'
    printf '  awpfel config   — view configuration\n'
    printf '\n'
    printf '  On first launch, Awpfel imports your zsh history automatically.\n'
    printf '  To import manually: awpfel history import\n'
    printf '\n'
    printf 'AI runs on-device via the managed Apfel backend. No API key needed.\n'
    printf '\n'
    printf '%sTo uninstall:%s\n' "${BOLD}" "${RESET}"
    printf '  awpfel uninstall --yes\n'
    printf '  rm ~/.local/bin/awpfel\n'
    printf '  rm -rf ~/.config/awpfel\n'
    printf '  rm -rf ~/.local/share/awpfel\n'
}

main
