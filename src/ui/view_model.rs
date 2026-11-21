use crate::config::AppConfig;
use crate::model::{
    MISSING_VALUE, MenuBarSegment, Provider, ProviderHealth, ProviderState,
    build_menu_bar_segments, format_dropdown_snapshot,
};

/// Supported refresh interval presets exposed in the native Settings submenu.
pub const INTERVAL_PRESETS: [u64; 4] = [30, 60, 120, 300];

/// Exact sRGB palette for Dark Mode (spec defaults) and accessible Light Mode counterparts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn components_f64(self) -> (f64, f64, f64) {
        (
            f64::from(self.r) / 255.0,
            f64::from(self.g) / 255.0,
            f64::from(self.b) / 255.0,
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderColorPair {
    pub dark: RgbColor,
    pub light: RgbColor,
}

/// Canonical color pair for each provider (Dark matches MVP spec; Light meets WCAG readability).
pub const fn provider_colors(provider: Provider) -> ProviderColorPair {
    match provider {
        Provider::Claude => ProviderColorPair {
            dark: RgbColor::new(0xE8, 0x9A, 0x70),
            light: RgbColor::new(0xB8, 0x4E, 0x1E),
        },
        Provider::Codex => ProviderColorPair {
            dark: RgbColor::new(0x74, 0xC8, 0x9A),
            light: RgbColor::new(0x18, 0x79, 0x4E),
        },
    }
}

/// View model for one provider row inside the native `NSMenu` dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRowViewModel {
    pub provider: Provider,
    /// Formatted value column with `%`, e.g. `65% / 72%`, `65% / –`, `– / –`.
    pub value_text: String,
    pub stale: bool,
    pub visible: bool,
    pub tooltip: String,
}

/// Complete pure-data UI snapshot diffed before touching AppKit controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSnapshot {
    pub menu_bar_segments: Vec<MenuBarSegment>,
    /// True when both providers are hidden in Settings; renders compact `–` so Menu Bar stays clickable.
    pub menu_bar_fallback_only: bool,
    pub menu_bar_tooltip: String,
    pub claude_row: ProviderRowViewModel,
    pub codex_row: ProviderRowViewModel,
    /// Optional subtle footer line (e.g. `Refreshing…` or `Updated 2m ago · Claude stale`).
    pub status_line: Option<String>,
    pub is_refreshing: bool,
    pub refresh_interval_seconds: u64,
    pub show_claude: bool,
    pub show_codex: bool,
    pub launch_at_login: bool,
}

impl UiSnapshot {
    pub fn build(
        config: &AppConfig,
        claude: &ProviderState,
        codex: &ProviderState,
        is_refreshing: bool,
        now_secs: u64,
    ) -> Self {
        let claude_health = claude.effective_health(now_secs, config.claude_stale_after_seconds);
        let codex_health = codex.effective_health(now_secs, config.codex_stale_after_seconds);

        let claude_snap = claude.visible_snapshot(now_secs, config.snapshot_ttl_seconds);
        let codex_snap = codex.visible_snapshot(now_secs, config.snapshot_ttl_seconds);

        let claude_stale = claude_snap.is_some() && claude_health.is_stale();
        let codex_stale = codex_snap.is_some() && codex_health.is_stale();

        let menu_bar_segments = build_menu_bar_segments(
            claude_snap.map(|s| (s, claude_stale)),
            codex_snap.map(|s| (s, codex_stale)),
            config.show_claude,
            config.show_codex,
        );
        let menu_bar_fallback_only = menu_bar_segments.is_empty();

        let claude_row = ProviderRowViewModel {
            provider: Provider::Claude,
            value_text: format_dropdown_snapshot(claude_snap),
            stale: claude_stale,
            visible: config.show_claude,
            tooltip: build_provider_tooltip(
                claude,
                &claude_health,
                claude_snap.is_some(),
                now_secs,
            ),
        };
        let codex_row = ProviderRowViewModel {
            provider: Provider::Codex,
            value_text: format_dropdown_snapshot(codex_snap),
            stale: codex_stale,
            visible: config.show_codex,
            tooltip: build_provider_tooltip(codex, &codex_health, codex_snap.is_some(), now_secs),
        };

        let menu_bar_tooltip = format!(
            "Ebb (5h / weekly remaining)\nClaude: {}\nCodex: {}",
            claude_row.value_text, codex_row.value_text
        );

        let status_line = build_status_line(
            is_refreshing,
            &claude_health,
            &codex_health,
            claude.last_success_at,
            codex.last_success_at,
            config.show_claude,
            config.show_codex,
            now_secs,
        );

        Self {
            menu_bar_segments,
            menu_bar_fallback_only,
            menu_bar_tooltip,
            claude_row,
            codex_row,
            status_line,
            is_refreshing,
            refresh_interval_seconds: config.refresh_interval_seconds,
            show_claude: config.show_claude,
            show_codex: config.show_codex,
            launch_at_login: config.launch_at_login,
        }
    }

    /// Plain text summary of Menu Bar content for tests and accessibility.
    pub fn menu_bar_plain_text(&self) -> String {
        if self.menu_bar_fallback_only {
            return MISSING_VALUE.to_owned();
        }
        self.menu_bar_segments
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn build_provider_tooltip(
    state: &ProviderState,
    effective_health: &ProviderHealth,
    has_visible_snapshot: bool,
    now_secs: u64,
) -> String {
    let provider = state.provider;
    let source_hint = match provider {
        Provider::Claude => "status-line cache (updates during active Claude Code sessions)",
        Provider::Codex => "Codex app-server poll",
    };

    let status_desc = match effective_health {
        ProviderHealth::Fresh => match state.last_success_at {
            Some(ts) => format!("Fresh (updated {})", format_relative_age(now_secs, ts)),
            None => "Fresh".to_owned(),
        },
        ProviderHealth::Stale => match (state.last_success_at, state.last_error.as_deref()) {
            (Some(ts), Some(err)) => format!(
                "Stale (last updated {}; {})",
                format_relative_age(now_secs, ts),
                err
            ),
            (Some(ts), None) => {
                format!("Stale (last updated {})", format_relative_age(now_secs, ts))
            }
            (None, Some(err)) => format!("Stale ({err})"),
            (None, None) => "Stale".to_owned(),
        },
        ProviderHealth::Unauthenticated => {
            format!("Not authenticated — sign in to {provider} CLI")
        }
        ProviderHealth::Unavailable => {
            if !has_visible_snapshot && provider == Provider::Claude {
                "Waiting for Claude Code status-line data".to_owned()
            } else if let Some(ref err) = state.last_error {
                format!("Unavailable ({err})")
            } else {
                "Unavailable".to_owned()
            }
        }
        ProviderHealth::Error(msg) => format!("Error: {msg}"),
    };

    format!("{provider}: {status_desc}\n5h / weekly remaining · {source_hint}")
}

#[allow(clippy::too_many_arguments)]
fn build_status_line(
    is_refreshing: bool,
    claude_health: &ProviderHealth,
    codex_health: &ProviderHealth,
    claude_ts: Option<u64>,
    codex_ts: Option<u64>,
    show_claude: bool,
    show_codex: bool,
    now_secs: u64,
) -> Option<String> {
    if is_refreshing {
        return Some("Refreshing…".to_owned());
    }

    let mut notes = Vec::new();
    let mut latest_ts: Option<u64> = None;

    if show_claude {
        if let Some(ts) = claude_ts {
            latest_ts = Some(latest_ts.map_or(ts, |prev| prev.max(ts)));
        }
        if let Some(note) = short_health_note(Provider::Claude, claude_health) {
            notes.push(note);
        }
    }
    if show_codex {
        if let Some(ts) = codex_ts {
            latest_ts = Some(latest_ts.map_or(ts, |prev| prev.max(ts)));
        }
        if let Some(note) = short_health_note(Provider::Codex, codex_health) {
            notes.push(note);
        }
    }

    let updated = latest_ts.map(|ts| format!("Updated {}", format_relative_age(now_secs, ts)));

    match (updated, notes.is_empty()) {
        (Some(u), true) => Some(u),
        (Some(u), false) => Some(format!("{u} · {}", notes.join(", "))),
        (None, false) => Some(notes.join(", ")),
        (None, true) => None,
    }
}

fn short_health_note(provider: Provider, health: &ProviderHealth) -> Option<String> {
    match health {
        ProviderHealth::Fresh => None,
        ProviderHealth::Stale => Some(format!("{provider} stale")),
        ProviderHealth::Unauthenticated => Some(format!("{provider} login needed")),
        ProviderHealth::Unavailable => None,
        ProviderHealth::Error(_) => Some(format!("{provider} error")),
    }
}

/// Format compact relative age string (e.g. `just now`, `45s ago`, `3m ago`, `2h ago`).
pub fn format_relative_age(now_secs: u64, timestamp_secs: u64) -> String {
    let age = now_secs.saturating_sub(timestamp_secs);
    if age < 10 {
        "just now".to_owned()
    } else if age < 60 {
        format!("{age}s ago")
    } else if age < 3600 {
        format!("{}m ago", age / 60)
    } else {
        format!("{}h ago", age / 3600)
    }
}

/// Format human label for a refresh interval preset.
pub fn format_interval_label(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs.is_multiple_of(60) {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}
