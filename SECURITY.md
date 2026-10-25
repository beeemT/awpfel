# Security Policy

## Core Invariant

**Awpfel never runs a command on its own.** Ghost text runs only
after you press Tab to accept it into the line and then press Enter.
`Ctrl-X A` replaces the line directly but never runs it. Enter
alone only runs what is already in the line.

## What Awpfel Collects

The daemon collects the following from your terminal session:

- **Current working directory** — to weight suggestions by context
- **Recent commands** — stored in a local SQLite database and used as
  model context for completions
- **Exit codes** — to track command results
- **stderr output** — only when stderr capture is on (off by default)
- **Git context** — branch name and status
- **Project type** — detected from files like `Cargo.toml`,
  `package.json`, etc.

All data stays on your machine. The model backend is the managed
local Apfel on `127.0.0.1:11435`. Nothing is sent to a cloud service.

## What Gets Sent to the Model

When AI completion is on (`ai.enabled = true`), sanitized context
goes to the local Apfel backend:

- Working directory (sensitive paths replaced with `[SENSITIVE_PATH]`)
- Git branch and status (sensitive tokens redacted)
- Relevant history entries as untrusted prompt context (redacted)
- Current input being typed

### What Gets Filtered

Before sending, Awpfel redacts:

- API keys (`sk-*`, `ghp_*`, `gho_*`, `AKIA*`)
- Bearer tokens
- Database URLs with embedded passwords
- Export statements with sensitive values

**Important:** Sanitization is best-effort pattern matching. It does
not guarantee that all sensitive data is caught. Do not type raw
secrets, private keys, or passwords into the command line.

## What Awpfel Does NOT Do

- Does not read file contents (no `.env`, `.ssh/id_rsa`, etc.)
- Does not auto-execute any command
- Does not modify any file outside its own config, data, and shell
  integration paths
- Does not transmit data to any service other than the managed local
  Apfel backend

## History Safety

History entries feed model prompts as untrusted input. Commands with
detected sensitive patterns are **not recorded** in the database.

## How to Disable Features

### Disable AI completely

```toml
# ~/.config/awpfel/config.toml
[ai]
enabled = false
```

### Disable stderr capture

Stderr capture is **off by default**. Enable it only by explicit
configuration.

### Disable the plugin entirely

Remove the awpfel source lines and autostart block from `~/.zshrc`,
or run `awpfel uninstall --yes`.

## Reporting Security Issues

If you discover a security vulnerability, report it via:

- GitHub Issues: https://github.com/beeemT/awpfel/issues

Include steps to reproduce when possible.
