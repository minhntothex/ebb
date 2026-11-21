# ebb

**ebb** is a minimal macOS Menu Bar app written in pure **Rust + AppKit** (no Electron, Tauri, WebView, or Dock icon) that displays **remaining quota percentages** for **Claude Code** and **OpenAI Codex** across their **5-hour and 7-day (weekly)** windows.

```text
65/72  43/31
```

---

## Versioning

Current version: **`26.0`**

`ebb` uses the **`XX.Y(.Z)`** calendar versioning scheme (`Z` is optional):

- **`XX`** — last two digits of the current year (e.g. `26` for 2026)
- **`Y`** — sequential release number within that year, starting at **`0`** (e.g. `26.0`, `26.5`, `26.12`)
- **`Z`** *(optional)* — hotfix or sub-release of `Y` (e.g. `26.0.2`, `26.0.12`, `26.5.3`, `26.12.3`)

---

## Installation

### Requirements
- macOS 13 or later
- Rust toolchain (`cargo`, edition 2024)
- **Claude Code** CLI and/or **OpenAI Codex** CLI signed in locally

### 1. Build and package `Ebb.app`

```bash
git clone https://github.com/minhntothex/ebb.git
cd ebb

# Build stripped LTO release binary and package dist/Ebb.app
cargo build --release
./scripts/package-app.sh --no-build

# Install into /Applications and launch
cp -R dist/Ebb.app /Applications/
open /Applications/Ebb.app
```

### 2. Enable Claude Code quota feed (opt-in)

Claude Code emits `rate_limits` through its `statusLine` command during active sessions. `ebb` never modifies `~/.claude/settings.json` silently:

```bash
# Preview changes (dry-run)
/Applications/Ebb.app/Contents/MacOS/ebb setup-claude-statusline

# Apply (creates a timestamped backup and chains your existing statusLine command if present)
/Applications/Ebb.app/Contents/MacOS/ebb setup-claude-statusline --apply
```

### 3. Optional configuration (`config.json`)

Stored at `~/Library/Application Support/Ebb/config.json` (accessible via `Settings → Open Config Folder`):

```json
{
  "refresh_interval_seconds": 60,
  "show_claude": true,
  "show_codex": true,
  "launch_at_login": false,
  "codex_path": "/optional/path/to/codex"
}
```

---

## Features

- **Compact two-color Menu Bar display:** Shows only remaining `5h/weekly` numbers (`Claude` in orange, `Codex` in green) using system tabular numerals and dynamic Dark/Light Mode colors.
- **Native `NSMenu` dropdown:** Displays `● Claude  65% / 72%` and `● Codex  43% / 31%` with per-provider freshness/error tooltips, `Refresh Now`, `Settings`, and `Quit`.
- **In-menu Settings:** Switch refresh interval (`30s` / `1m` / `2m` / `5m`), toggle individual providers, enable `Launch at Login` (macOS LaunchAgent), or open the config directory.
- **Safe Claude Code integration:** `ebb claude-statusline` writes an atomic `0600` cache and transparently chains any existing `statusLine` command (preserving `stdin`, `stdout`, `stderr`, and exit code) without sending model prompts.
- **Zero-token OpenAI Codex polling:** Talks directly to short-lived `codex app-server --stdio` (`account/rateLimits/read`), auto-discovers `codex` across `$PATH`/Homebrew/NVM/daemon paths, maps windows strictly by actual duration (`300m` / `10080m`), enforces a 5-second timeout, and reaps child processes cleanly.
- **Honest quota states:** Displays `–` when a window is missing, unauthenticated, or past TTL; preserves last-known-good values with a stale indicator on transient errors; never derives quota from token totals or fakes resets to `100`.
- **Low footprint:** ~`591 KB` stripped release binary, ~`17 MB` idle RSS, `0%` idle CPU via a single `Condvar` worker thread that pauses on sleep and refreshes on wake.

---

## Verification

```bash
cargo test
dist/Ebb.app/Contents/MacOS/ebb probe
dist/Ebb.app/Contents/MacOS/ebb --ui-smoke-test
```
