#!/bin/sh
set -eu

# Build the single awpfel release archive (macOS Apple Silicon).
# Usage: scripts/build-archives.sh 0.2.0
# Mirrors the claimy build-archives.sh convention: strict semver, dist/ output.
# Apfel (the model backend) is macOS-only, so only darwin/arm64 ships.

version=${1:-0.0.0}
case "$version" in
  v*) version=${version#v} ;;
esac
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || {
  echo 'error: version must be MAJOR.MINOR.PATCH' >&2
  exit 2
}

root=$(CDPATH=; cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
dist="$root/dist"
mkdir -p "$dist"
command -v cargo >/dev/null 2>&1 || { echo 'error: cargo is required' >&2; exit 1; }
[ -f "$root/LICENSE" ] || { echo 'error: LICENSE is missing' >&2; exit 1; }

os=darwin
arch=arm64
target=aarch64-apple-darwin
name="awpfel_${version}_${os}_${arch}"
stage=$(mktemp -d "${TMPDIR:-/tmp}/awpfel-package.XXXXXX")
# shellcheck disable=SC2064
trap "rm -rf '$stage'" EXIT HUP INT TERM
echo "==> building $name"
cargo build --release --locked --target "$target"
cp "$root/target/$target/release/awpfel" "$stage/awpfel"
# Ship the full plugin directory: entry plus every module (explicit list).
for module in awpfel.zsh awpfel-utils.zsh awpfel-render.zsh awpfel-interact.zsh awpfel-communicate.zsh; do
    [ -f "$root/plugin/$module" ] || { echo "error: plugin/$module is missing" >&2; exit 1; }
    cp "$root/plugin/$module" "$stage/"
done
cp "$root/LICENSE" "$stage/LICENSE"
COPYFILE_DISABLE=1 tar -C "$stage" -czf "$dist/$name.tar.gz" awpfel awpfel.zsh awpfel-utils.zsh awpfel-render.zsh awpfel-interact.zsh awpfel-communicate.zsh LICENSE
rm -rf "$stage"
trap - EXIT HUP INT TERM

printf 'archive written to %s\n' "$dist"
