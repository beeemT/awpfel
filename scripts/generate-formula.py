#!/usr/bin/env python3
"""Generate the Awpfel Homebrew formula from the dist archive and its checksum.

Mirrors scripts/generate-formula.py in claimy: the formula file is derived
from the built archive, so no committed placeholder or checked-in hash exists.
Usage: scripts/generate-formula.py 0.2.0
"""
import hashlib
import pathlib
import re
import sys

root = pathlib.Path(__file__).resolve().parent.parent
dist = root / "dist"
version = sys.argv[1] if len(sys.argv) > 1 else "0.0.0"
if version.startswith("v"):
    version = version[1:]
if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
    raise SystemExit(f"error: version must be MAJOR.MINOR.PATCH: {version}")
repo = "beeemT/awpfel"
archives = sorted(dist.glob(f"awpfel_{version}_*.tar.gz"))
expected = {f"awpfel_{version}_darwin_arm64.tar.gz"}
if {p.name for p in archives} != expected:
    raise SystemExit(f"error: expected exactly one archive for {version} in {dist}")

checksums = {}
for archive in archives:
    checksums[archive.name] = hashlib.sha256(archive.read_bytes()).hexdigest()
(dist / "checksums.txt").write_text(
    "".join(f"{checksums[name]}  {name}\n" for name in sorted(checksums)),
    encoding="utf-8",
)

sha = checksums[f"awpfel_{version}_darwin_arm64.tar.gz"]

formula = f'''class Awpfel < Formula
  desc "On-device AI ghost text for zsh, backed by a managed local Apfel"
  homepage "https://github.com/{repo}"
  version "{version}"
  license "MIT"

  depends_on "apfel"
  depends_on "jq"
  depends_on "socat"

  depends_on macos: :tahoe
  depends_on arch: :arm64
  on_macos do
    on_arm do
      url "https://github.com/{repo}/releases/download/v#{{version}}/awpfel_#{{version}}_darwin_arm64.tar.gz"
      sha256 "{sha}"
    end
  end

  def install
    bin.install "awpfel"
    pkgshare.install Dir["awpfel*.zsh"]
  end

  def caveats
    <<~EOS
      Run `awpfel setup` to configure your shell, then restart your terminal.
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{{bin}}/awpfel --version")
  end
end
'''
(dist / "awpfel.rb").write_text(formula, encoding="utf-8")
print(f"wrote {dist / 'awpfel.rb'} and {dist / 'checksums.txt'}")
