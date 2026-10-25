<div align="center">

# Awpfel

**On-device AI ghost text for zsh. Type, see the full command, press Tab.**

<a href="https://github.com/beeemT/awpfel/releases"><img src="https://img.shields.io/github/v/tag/beeemT/awpfel?label=version&style=flat-square" alt="Version"></a>
<a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-blue.svg?style=flat-square" alt="License"></a>
<br/>
<a href="README_CN.md">中文</a>
</div>

## What it does

Awpfel shows ghost text as you type in zsh. The ghost text is a full
command. It comes from on-device AI with your relevant shell history as
context. You accept it or you ignore it. Awpfel never runs a command.

- Type part of a command. Ghost text shows the full command.
- Press **Tab** to accept the ghost text into the line for editing.
- Press **Enter** to run what you typed. Enter never accepts ghost text.
- If no ghost text shows, keep typing. Nothing is forced on you.

## Keys

| Key | Action |
| :-- | :----- |
| `Tab` / `→` | Accept all ghost text into the line for editing |
| `Shift-Tab` / `Ctrl-→` or `Alt-→` | Accept the next shell-word chunk of ghost text; press again for the next |
| `Enter` | Run the line as typed (never accepts ghost text) |
| `Ctrl-X` then `A` | Replace the line with an AI-built command, in place, without running it |

Note: `Ctrl-X A` means press `Ctrl-X`, release, then press `A`.
It is not `Ctrl-X Ctrl-A`. It never runs the result.
Enter alone only runs what is in the line.

## Completion display

- **Suffix ghost.** The shown text starts where your input ends.
  Accept it all at once, or take only the next shell-word chunk
  instead, for example one quoted argument at a time.
- **Replacement preview.** The best match does not start with your
  input. Awpfel shows the full command for review. Accepting
  replaces the whole line, or takes the first proposed chunk.

Quoted text and escaped spaces stay one chunk. If the cursor sits
mid-word, chunk accept completes that word first. Acceptance only
edits the line; it never runs it. Only `Ctrl-X A` replaces the line
outside the ghost-text flow, and it never runs it.

## Natural language

Type `#` plus a description. The built command appears as a
next-line ghost preview (`→ full-command`). The line itself is
unchanged until you press Tab to accept the preview. It never runs.

```
# find all go files changed today
```

Press `Ctrl-X` then `A` to replace the current line with the built
command. The result lands in the line for editing, never in the shell.

## Install

Awpfel needs macOS 26 (Tahoe) or later on Apple Silicon. It uses the Apfel
CLI for on-device AI.

### Homebrew

```bash
brew install beeemT/tap/awpfel
awpfel setup
```

`awpfel setup` installs config and shell integration and starts the
background daemon. The daemon manages a local Apfel on its own port.
You do not run `brew services start apfel`.

### Shell script

Runs on this machine only (macOS Apple Silicon). It downloads the
macOS Apple Silicon archive from the latest release.

```bash
curl -sSL https://raw.githubusercontent.com/beeemT/awpfel/main/install-remote.sh | sh
```

### Build from source

Runs on this machine only (macOS Apple Silicon). `install.sh`
refuses other hosts and builds with the local Rust toolchain.

**Prerequisites:** current stable Rust, zsh, apfel, jq, socat.

```bash
git clone https://github.com/beeemT/awpfel.git
cd awpfel
./install.sh
```

---

Restart your shell. Awpfel starts with the shell and imports your
zsh history on first launch.

## Update

### Homebrew

```bash
brew upgrade beeemT/tap/awpfel
awpfel stop && awpfel start
```

### Shell script

```bash
curl -sSL https://raw.githubusercontent.com/beeemT/awpfel/main/install-remote.sh | sh
```

### From source

```bash
cd awpfel
git pull
./install.sh
```

Restart your shell after updating.

## Uninstall

```bash
awpfel uninstall --yes
```

This stops the daemon, removes the shell integration from
`~/.zshrc`, and deletes `~/.config/awpfel/` and
`~/.local/share/awpfel/`. Without `--yes` it asks once
for confirmation.

**Homebrew:**

```bash
brew uninstall awpfel
```

**Shell script / source install:**

```bash
rm ~/.local/bin/awpfel
rm -rf ~/.config/awpfel
rm -rf ~/.local/share/awpfel
# Remove the awpfel source lines from ~/.zshrc
```

## CLI

```bash
awpfel setup              # Configure shell integration, start daemon
awpfel start              # Start daemon
awpfel stop               # Stop daemon
awpfel status             # Show status (pid, uptime, history count)
awpfel logs               # Show recent logs
awpfel config             # Show configuration
awpfel context            # Show current context state
awpfel history import     # Import zsh history
awpfel uninstall          # Remove all data, config, and shell integration
```

## Configuration

Config lives at `~/.config/awpfel/config.toml`. All fields have
defaults. The defaults point at the managed local Apfel backend.

```toml
[ai]
enabled = true
base_url = "http://127.0.0.1:11435/v1"
model = "apple-foundationmodel"
```

Set `ai.enabled = false` in the config file to turn AI off.

## Safety

- **Never runs on its own.** Accept ghost text before Enter can run
  it. `Ctrl-X A` replaces the line directly but never runs it.
- **History is context.** History entries feed the model prompt as
  untrusted input. Awpfel sanitizes them before sending.
- **Never reads sensitive files.** No `.env`, `.ssh`, or key files.
- **Privacy filter.** Values that look like keys or tokens are
  redacted before they leave the machine. Filtering is best effort.
- **On-device AI.** The default backend is the managed local Apfel.
  No API key and no cloud account are needed.

## Development

```bash
make dev              # Debug build + sync + restart daemon
make release          # Release build + sync + restart
make test             # cargo test + shellcheck + zsh smoke
make lint             # clippy + fmt check + shellcheck
make sync             # Copy plugin only (no rebuild)
make status           # Check daemon status
make logs             # Show daemon logs
```

## Credits

Awpfel builds on two open projects:

- [Awen](https://github.com/zzf2333/Awen) by SaoNian (zzf2333),
  MIT licensed. Awpfel is a fork of Awen with the product and
  backend changed to on-device AI.
- [Apfel](https://github.com/Arthur-Ficial/apfel), MIT licensed,
  which serves the on-device model Awpfel queries.

The original MIT copyright from Awen is preserved in LICENSE.

Awpfel is an independent project. It is not affiliated with,
endorsed by, or sponsored by the Awen authors, Apfel, or Apple.

## License

MIT
