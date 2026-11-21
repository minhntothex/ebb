# ebb

**ebb** is a minimal macOS Menu Bar app written in pure **Rust + AppKit** (no Electron, Tauri, WebView, or Dock icon) that displays **5-hour and 7-day (weekly) quota percentages** for **Claude Code**, **OpenAI Codex**, and **Antigravity CLI (`agy`)**.

```text
65/72  43/31  –/94
```

---

## Versioning

Current version: **`26.3`**

`ebb` uses the **`XX.Y(.Z)`** calendar versioning scheme (`Z` is optional):

- **`XX`** — last two digits of the current year (e.g. `26` for 2026)
- **`Y`** — sequential release number within that year, starting at **`0`** (e.g. `26.0`, `26.1`, `26.12`)
- **`Z`** *(optional)* — hotfix or sub-release of `Y` (e.g. `26.0.2`, `26.1.1`, `26.5.3`)

---

## Installation

### Requirements
- macOS 13 (Ventura) or later
- **Claude Code**, **OpenAI Codex**, and/or **Antigravity CLI (`agy`)** signed in locally
- *(Only when building from source)* Rust toolchain (`cargo`, edition 2024)

### Option A: Download prebuilt Universal binary (Apple Silicon & Intel)

1. Download `ebb-26.3-macos-universal.dmg` (or `.zip`) from [GitHub Releases](https://github.com/minhntothex/ebb/releases).
2. Drag **`Ebb.app`** into `/Applications`.
3. Releases are ad-hoc signed. If macOS Gatekeeper blocks the first launch, clear the quarantine attribute once:

   ```bash
   xattr -dr com.apple.quarantine /Applications/Ebb.app
   open /Applications/Ebb.app
   ```

*(Each GitHub Release also attaches a ready-to-use `ebb.rb` Homebrew Cask populated with the verified DMG `SHA256`, generated from [`packaging/homebrew/ebb.rb`](packaging/homebrew/ebb.rb).)*

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

### Enable Claude Code & Antigravity CLI (`agy`) quota feeds (opt-in)

Both Claude Code and Antigravity CLI emit live quota payloads through their `statusLine` command during active sessions. `ebb` never modifies CLI settings silently — configure them via **`Settings → Integrations`** in the Menu Bar (with a confirmation preview dialog) or via CLI:

```bash
# Preview changes (dry-run)
/Applications/Ebb.app/Contents/MacOS/ebb setup-claude-statusline
/Applications/Ebb.app/Contents/MacOS/ebb setup-agy-statusline

# Apply (creates a timestamped backup and chains any existing statusLine command;
# standalone Antigravity setup sets `stack_with_default: true` so the built-in status line stays intact)
/Applications/Ebb.app/Contents/MacOS/ebb setup-claude-statusline --apply
/Applications/Ebb.app/Contents/MacOS/ebb setup-agy-statusline --apply
```

### Scriptable CLI (`ebb status`)

Query Ebb's normalized cache in `<2 ms` for tmux, SketchyBar, Raycast, Starship, or shell scripts:

```bash
# Compact plain text respecting your config order & display mode
/Applications/Ebb.app/Contents/MacOS/ebb status

# Structured JSON (version 1)
/Applications/Ebb.app/Contents/MacOS/ebb status --json

# Include a fresh live Codex poll
/Applications/Ebb.app/Contents/MacOS/ebb status --json --live
```

### Configuration (`config.json`) — Hot-Reloaded via `kqueue`

Stored at `~/Library/Application Support/Ebb/config.json` (accessible via `Settings → Open Config Folder`). External edits apply automatically in `<5 ms` without restarting Ebb:

```json
{
  "refresh_interval_seconds": 60,
  "show_claude": true,
  "show_codex": true,
  "show_agy": false,
  "show_reset_time": true,
  "display_metric": "remaining",
  "menu_bar_windows": "both",
  "highlight_critical": true,
  "provider_order": ["claude", "codex", "agy"],
  "color_theme": "default",
  "provider_colors": {
    "agy": { "dark": "#7EB6FF", "light": "#1D5FD6" }
  },
  "alerts_enabled": true,
  "alert_warning_threshold": 20,
  "alert_critical_threshold": 5,
  "notify_on_refresh": true,
  "launch_at_login": false,
  "codex_path": "/optional/path/to/codex"
}
```

---

## Features

- **Customizable multi-provider Menu Bar:** Displays `Claude` (amber), `Codex` (mint), and `Agy` (sky blue) using system tabular numerals and dynamic Dark/Light Mode colors. Switch between **`Remaining %`** / **`Used %`**, **`5h + Weekly`** / **`5h Only (Compact)`** for notched MacBooks, custom **`Provider Order`**, **`Color Theme`** (`Default`, `High Contrast`, `Monochrome`), or `#RRGGBB` overrides in `config.json`.
- **Per-window Critical highlight:** When a 5h or weekly window drops to `≤ alert_critical_threshold`, only that number turns accessible red in the Menu Bar — staying visible even under macOS Do Not Disturb.
- **Aligned native `NSMenu` dropdown with live refresh on open:** `NSMenuDelegate::menuWillOpen:` recalculates `Updated …` and `Reset in` countdowns to the exact current second the instant you click. Provider rows stay vertically aligned (`264pt` reused `NSView`s) with detailed hover tooltips.
- **Instant Claude, Antigravity & `config.json` watching (`kqueue`):** A single macOS `kqueue` (`EVFILT_VNODE`) fd watches `claude-statusline-cache.json`, `agy-statusline-cache.json`, and `config.json` at `0%` idle CPU, updating local caches in `<5 ms` without spawning `codex app-server`.
- **Low-quota & reset recovery notifications:** Native `UNUserNotificationCenter` alerts on Warning (`≤ 20%`) and Critical (`≤ 5%`) thresholds plus an optional `Notify on Reset` recovery alert once a previously low window refreshes.
- **Safe opt-in Integrations & manual update check:** One-click `Settings → Integrations` dialogs preview exact backups & wrapped commands before modifying Claude or Antigravity settings; `Settings → Check for Updates…` runs a CalVer check against GitHub Releases via native `NSAlert` with zero background telemetry.
- **Zero-token OpenAI Codex polling & honest quota states:** Talks directly to short-lived `codex app-server --stdio` (`account/rateLimits/read`), maps windows strictly by duration (`300m` / `10080m`), displays `–` when a window is missing or past TTL, and never derives quota from token totals or fakes resets to `100`.
- **Low footprint & Universal packaging:** ~`660 KB` stripped binary, ~`17 MB` idle RSS, `0%` idle CPU; automated Universal (`arm64` + `x86_64`) `.dmg` / `.zip` / `SHA256SUMS.txt` / `ebb.rb` GitHub Releases and native `AppIcon.icns`.

---

## Backlog

Planned items deferred beyond `26.3`:

- **BL-005:** Lightweight usage history / trend visualization.

---

## Verification

```bash
cargo test
dist/Ebb.app/Contents/MacOS/ebb probe
dist/Ebb.app/Contents/MacOS/ebb status --json
dist/Ebb.app/Contents/MacOS/ebb --ui-smoke-test
```
