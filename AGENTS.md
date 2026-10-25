# AGENTS.md

This file gives repository guidance to coding agents.

## Language Conventions

- **对话语言：** 与用户交流一律使用中文
- **Git commits：** 英文（格式 `<type>: <description>`，type = feat/fix/refactor/docs/test/chore）
- **代码：** 变量名、注释、日志等一律英文
- **README.md：** 维护两个文件 — `README.md`（英文）和 `README_CN.md`（中文）
- **其他所有文档（docs/、代码注释、提交到 git 的内容）：** 一律英文

## What is Awen

Awen is a Terminal Intelligence Layer — a Rust daemon + zsh plugin that adds ghost text completions, failure recovery suggestions, and risk warnings to any terminal. **Core invariant: Awen always suggests, never executes.** It is not a terminal emulator, not a shell agent, not an automation tool.

## Build & Test Commands

```bash
cargo build                          # dev build
cargo build --release                # release build
cargo test                           # all tests (192 unit + 37 E2E)
cargo test --test e2e                # E2E integration tests only
cargo test --lib                     # unit tests only
cargo test <test_name>               # single test by name
cargo test layer::failure            # tests in a module
cargo clippy                         # lint
cargo fmt --check                    # format check
cargo fmt                            # auto-format
```

## Architecture

```
zsh plugin (plugin/awen.zsh)
    ↕  Unix socket + JSON (newline-delimited)
Daemon (src/daemon.rs) — IPC, request dispatch, state
    ↓
Context Engine (src/context/) → gathers cwd, session history, repo type, git state
    ↓
Layer 1: Deterministic (<20ms)
  ├── History (src/layer/history.rs)    — SQLite + nucleo fuzzy match + bigram sequence prediction, <5ms
  ├── Specs (src/layer/specs.rs)       — TOML command specs, <20ms
  └── Filesystem (src/layer/filesystem.rs) — directory/file completion, cached, <10ms
Layer 2: Predictive (async)
  ├── AI (src/layer/ai.rs)           — DeepSeek / Ollama, optional
  ├── Failure (src/layer/failure.rs) — regex stderr → fix suggestion
  └── Risk (src/layer/risk.rs)       — regex input → warning
    ↓
Pipeline (src/pipeline.rs) — AI trigger policy, AI execution, merges AI into local results
    ↓                          All layers (including Filesystem) feed into Arbitrator
Arbitrator (src/arbitrator.rs) → dedup (Levenshtein), context-weight, group-by-source, rank, top-8
    ↓
Response (suggestions + ui_mode) → zsh renders based on interaction mode
```

**Interaction modes:** The plugin supports two modes via `config.ui.mode` (or `AWEN_UI_MODE` env override):
- **Minimal (default):** Ghost text only. No dropdown menu, no risk/failure panels. Warnings shown inline via `zle -M`. Up/Down/Enter pass through to shell. Keybinding conflict detection auto-downgrades Full→Minimal when zsh-autosuggestions or fzf are detected.
- **Full:** Dropdown menu with source labels, risk panels, failure recovery panels. Full keyboard navigation (Up/Down/Tab/Enter).

The daemon includes `ui_mode` in every `SuggestResponse`; the plugin reads it and branches rendering logic accordingly. Ghost text is truncated with `…` when it would overflow terminal width. A SIGWINCH trap cleans up active UI on terminal resize.

**Request flow (dual-channel):** The zsh plugin uses a two-phase suggest flow:
- **Phase 1 (sync, every keystroke):** Sends `Suggest` with `skip_ai: true`. The daemon runs local layers only (history + specs + failure + risk), returns in <20ms. Ghost text renders immediately. Filesystem results are returned separately in `path_completion` (ghost-only, never in the dropdown menu). The response includes a `need_ai` signal.
- **Phase 2 (async, conditional):** Only scheduled when Phase 1's `need_ai` is true (local candidates insufficient). After `AWEN_AI_DELAY` seconds of no typing, sends `Suggest` with `skip_ai: false`. The daemon runs AI as a fallback. AI triggers when: (a) local candidates < `min_local_candidates` AND max confidence < `min_local_confidence`, or (b) last command failed and no local failure pattern matched (AI error recovery).

**NL Generation (separate channel):** When the user types `# <query>`, the plugin sends `NlGenerate` (not `Suggest`) with the natural-language query. The daemon calls AI to translate the query into a shell command and returns `NlGenerateResponse { command, explanation, warning }`.

**Record:** The zsh `precmd` hook sends the previous command, exit code, stderr, and cwd so the daemon can update session context and history DB.

## Key Design Constraints

- **Latency tiers:** history <5ms, specs <20ms, filesystem <10ms (cached), AI is async and never blocks the response
- **All AI features must be disableable** (`ai.enabled = false`) — local features work offline
- **Context sanitization:** env vars with key/token/secret/password in the name are filtered; stderr has token patterns redacted (see `src/sanitize.rs`)
- **Never read sensitive files:** .env, .ssh, kubeconfig, AWS credentials, wallets, private keys
- **Specs are TOML, not TypeScript** — custom format in `specs/*.toml`, loaded at startup via `include_str!()`

## Protocol

Defined in `src/protocol.rs`. JSON messages tagged with `#[serde(tag = "type")]`:
- **Requests:** `Suggest`, `NlGenerate`, `Record`, `Status`, `Context`, `Shutdown`
- **Responses:** `Suggest` (suggestions + path_completion + hint + warning + need_ai + ui_mode), `NlGenerate` (command + explanation + warning), `Status`, `Context`, `Ok`, `Error`

## Config

`src/config.rs` — all config structs use `#[serde(default)]` so partial TOML works. User config at `~/.config/awen/config.toml`. Key paths:
- Socket: `$XDG_RUNTIME_DIR/awen-{uid}.sock`
- History DB: `~/.local/share/awen/history.db` (tables: `commands`, `command_sequences`)
- Logs: `~/.local/share/awen/awen.log`

AI fallback thresholds (in `[ai]` section):
- `min_local_candidates` (default 2) — AI triggers only when local candidate count is below this AND confidence is below threshold
- `min_local_confidence` (default 0.6) — AI triggers only when max local confidence is below this AND count is below threshold

## Testing

- **Unit tests** live in `#[cfg(test)]` modules inside each source file
- **E2E tests** in `tests/e2e.rs` — spin up a real daemon on a temp socket (via `DaemonPaths`), send requests, verify responses. Uses `tempfile` for isolation
- `src/lib.rs` re-exports all modules so integration tests can import them

## Specs Format

Built-in specs in `specs/*.toml` (git, docker, npm, cargo, brew, curl, ssh). Structure:
```toml
[command]
name = "git"
[[command.subcommands]]
name = "checkout"
[[command.subcommands.flags]]
name = "--branch"
short = "-b"
arg = "BRANCH"
```
User specs in `~/.config/awen/specs/` override built-ins.

## Failure & Risk Patterns

- `src/layer/failure.rs` — regex patterns with capture groups, template substitution (`{1}`, `{2}`). User patterns in `~/.config/awen/failure_patterns.toml`
- `src/layer/risk.rs` — regex patterns returning warning text. User patterns in `~/.config/awen/risk_patterns.toml`
