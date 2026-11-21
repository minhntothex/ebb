# ebb

**ebb** is a minimal macOS Menu Bar app written in pure **Rust + AppKit** (no Electron, Tauri, WebView, or Dock icon) that displays **remaining quota percentages** for **Claude Code** and **OpenAI Codex** across their **5-hour and 7-day (weekly)** windows.

```text
65/72  43/31
```

---

## Versioning

Current version: **`26.1`**

`ebb` uses the **`XX.Y(.Z)`** calendar versioning scheme (`Z` is optional):

- **`XX`** — last two digits of the current year (e.g. `26` for 2026)
- **`Y`** — sequential release number within that year, starting at **`0`** (e.g. `26.0`, `26.1`, `26.12`)
- **`Z`** *(optional)* — hotfix or sub-release of `Y` (e.g. `26.0.2`, `26.1.1`, `26.5.3`)

---

## Installation

### Requirements
- macOS 13 (Ventura) or later
- **Claude Code** CLI and/or **OpenAI Codex** CLI signed in locally
- *(Only when building from source)* Rust toolchain (`cargo`, edition 2024)

### Option A: Download prebuilt Universal binary (Apple Silicon & Intel)

1. Download `ebb-26.1-macos-universal.dmg` (or `.zip`) from [GitHub Releases](https://github.com/minhntothex/ebb/releases).
2. Drag **`Ebb.app`** into `/Applications`.
3. Releases are ad-hoc signed. If macOS Gatekeeper blocks the first launch, clear the quarantine attribute once:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Ebb.app
   open /Applications/Ebb.app
   ```

*(A ready-to-use Homebrew Cask template is also included at [`packaging/homebrew/ebb.rb`](packaging/homebrew/ebb.rb).)*

### Option B: Build from source

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

### Enable Claude Code quota feed (opt-in)

Claude Code emits `rate_limits` through its `statusLine` command during active sessions. `ebb` never modifies `~/.claude/settings.json` silently:

```bash
# Preview changes (dry-run)
/Applications/Ebb.app/Contents/MacOS/ebb setup-claude-statusline

# Apply (creates a timestamped backup and chains your existing statusLine command if present)
/Applications/Ebb.app/Contents/MacOS/ebb setup-claude-statusline --apply
```

### Optional configuration (`config.json`)

Stored at `~/Library/Application Support/Ebb/config.json` (accessible via `Settings → Open Config Folder`):

```json
{
  "refresh_interval_seconds": 60,
  "show_claude": true,
  "show_codex": true,
  "show_reset_time": true,
  "alerts_enabled": true,
  "alert_warning_threshold": 20,
  "alert_critical_threshold": 5,
  "launch_at_login": false,
  "codex_path": "/optional/path/to/codex"
}
```

---

## Features

- **Compact two-color Menu Bar display:** Shows only remaining `5h/weekly` numbers (`Claude` in amber, `Codex` in green) using system tabular numerals and dynamic Dark/Light Mode colors, with no Dock or Menu Bar icon clutter.
- **Aligned native `NSMenu` dropdown:** Header shows `Ebb` + flush-right last-updated status; each provider row displays `● Claude` / `● Codex` with flush-right `5h% / weekly%` values and an aligned `Reset in` countdown sub-row (`2h 15m / 4d 6h`) plus detailed hover tooltips.
- **Low-quota macOS notifications:** Native `UNUserNotificationCenter` alerts when 5h or weekly remaining quota drops to Warning (default `≤ 20%`) or Critical (default `≤ 5%`), with per-reset-window anti-spam deduplication, severity escalation, and configurable thresholds right inside `Settings → Low Quota Alerts`.
- **In-menu Settings:** Switch refresh interval (`30s` / `1m` / `2m` / `5m`), toggle individual providers, toggle `Show Reset Times`, configure `Low Quota Alerts`, enable `Launch at Login` (macOS LaunchAgent), or open the config directory.
- **Safe Claude Code integration:** `ebb claude-statusline` writes an atomic `0600` cache and transparently chains any existing `statusLine` command (preserving `stdin`, `stdout`, `stderr`, and exit code) without sending model prompts.
- **Zero-token OpenAI Codex polling:** Talks directly to short-lived `codex app-server --stdio` (`account/rateLimits/read`), auto-discovers `codex` across `$PATH`/Homebrew/NVM/daemon paths, maps windows strictly by actual duration (`300m` / `10080m`), enforces a 5-second timeout, and reaps child processes cleanly.
- **Honest quota states:** Displays `–` when a window is missing, unauthenticated, or past TTL; preserves last-known-good values with a stale indicator on transient errors; never derives quota from token totals or fakes resets to `100`.
- **Low footprint & Universal packaging:** ~`615 KB` stripped binary, ~`17 MB` idle RSS, `0%` idle CPU via a single `Condvar` worker thread that pauses on sleep and refreshes on wake; automated Universal (`arm64` + `x86_64`) `.dmg` / `.zip` GitHub Releases and native `AppIcon.icns`.

---

## Backlog

Planned items deferred beyond `26.1`:

- **BL-001:** OpenCode quota adapter (provider/model-aware quota normalized to the same Menu Bar & dropdown format).
- **BL-003:** Custom provider colors and display ordering.
- **BL-005:** Lightweight usage history / trend visualization.

---

## Verification

```bash
cargo test
dist/Ebb.app/Contents/MacOS/ebb probe
dist/Ebb.app/Contents/MacOS/ebb --ui-smoke-test
```
