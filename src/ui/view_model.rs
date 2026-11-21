use std::collections::BTreeMap;

use crate::config::{AppConfig, ColorTheme, HexColorOverride, parse_hex_rgb};
use crate::model::{
    DisplayMetric, MISSING_VALUE, MenuBarSegment, MenuBarWindows, Provider, ProviderHealth,
    ProviderState, QuotaSnapshot, build_menu_bar_segments_ordered, format_dropdown_snapshot_with,
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

/// Dynamic accessible red used for per-window critical quota emphasis in the Menu Bar.
pub const CRITICAL_COLORS: ProviderColorPair = ProviderColorPair {
    dark: RgbColor::new(0xFF, 0x6B, 0x6B),
    light: RgbColor::new(0xC9, 0x2A, 0x2A),
};

/// Canonical default color pair for each provider (Dark matches MVP spec; Light meets WCAG readability).
pub const fn provider_colors(provider: Provider) -> ProviderColorPair {
    provider_colors_for_theme(provider, ColorTheme::Default)
}

/// Curated palette per provider and `ColorTheme`.
pub const fn provider_colors_for_theme(provider: Provider, theme: ColorTheme) -> ProviderColorPair {
    match theme {
        ColorTheme::Default => match provider {
            Provider::Claude => ProviderColorPair {
                dark: RgbColor::new(0xE8, 0x9A, 0x70),
                light: RgbColor::new(0xB8, 0x4E, 0x1E),
            },
            Provider::Codex => ProviderColorPair {
                dark: RgbColor::new(0x74, 0xC8, 0x9A),
                light: RgbColor::new(0x18, 0x79, 0x4E),
            },
            Provider::Agy => ProviderColorPair {
                dark: RgbColor::new(0x7E, 0xB6, 0xFF),
                light: RgbColor::new(0x1D, 0x5F, 0xD6),
            },
        },
        ColorTheme::HighContrast => match provider {
            Provider::Claude => ProviderColorPair {
                dark: RgbColor::new(0xFF, 0xB3, 0x8A),
                light: RgbColor::new(0x9E, 0x38, 0x08),
            },
            Provider::Codex => ProviderColorPair {
                dark: RgbColor::new(0x8C, 0xE8, 0xB5),
                light: RgbColor::new(0x0D, 0x64, 0x3D),
            },
            Provider::Agy => ProviderColorPair {
                dark: RgbColor::new(0x99, 0xC8, 0xFF),
                light: RgbColor::new(0x0E, 0x47, 0xB5),
            },
        },
        ColorTheme::Monochrome => ProviderColorPair {
            dark: RgbColor::new(0xE8, 0xE8, 0xED),
            light: RgbColor::new(0x22, 0x22, 0x26),
        },
    }
}

/// Resolve the effective Dark/Light color pair applying any valid `#RRGGBB` override from `config.json`.
pub fn resolve_provider_colors(
    provider: Provider,
    theme: ColorTheme,
    overrides: &BTreeMap<String, HexColorOverride>,
) -> ProviderColorPair {
    let mut pair = provider_colors_for_theme(provider, theme);
    if let Some(custom) = overrides.get(provider.key()) {
        if let Some((r, g, b)) = custom.dark.as_deref().and_then(parse_hex_rgb) {
            pair.dark = RgbColor::new(r, g, b);
        }
        if let Some((r, g, b)) = custom.light.as_deref().and_then(parse_hex_rgb) {
            pair.light = RgbColor::new(r, g, b);
        }
    }
    pair
}

/// View model for one provider row inside the native `NSMenu` dropdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRowViewModel {
    pub provider: Provider,
    /// Formatted value column with `%`, e.g. `65% / 72%`, `65% / –`, `– / –`.
    pub value_text: String,
    /// Optional right-aligned reset countdown value for the `Reset in` sub-row, e.g. `2h 15m / 4d 6h`.
    pub reset_value_text: Option<String>,
    pub stale: bool,
    pub visible: bool,
    pub tooltip: String,
}

/// Complete pure-data UI snapshot diffed before touching AppKit controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiSnapshot {
    pub menu_bar_segments: Vec<MenuBarSegment>,
    /// True when all providers are hidden in Settings; renders compact `–` so Menu Bar stays clickable.
    pub menu_bar_fallback_only: bool,
    pub menu_bar_tooltip: String,
    /// Provider rows ordered by `config.normalized_provider_order()`.
    pub provider_rows: Vec<ProviderRowViewModel>,
    /// Optional subtle header status line (e.g. `Refreshing…` or `Updated 2m ago · Claude stale`).
    pub status_line: Option<String>,
    pub is_refreshing: bool,
    pub refresh_interval_seconds: u64,
    pub show_claude: bool,
    pub show_codex: bool,
    pub show_agy: bool,
    pub show_reset_time: bool,
    pub display_metric: DisplayMetric,
    pub menu_bar_windows: MenuBarWindows,
    pub highlight_critical: bool,
    pub provider_order: Vec<Provider>,
    pub color_theme: ColorTheme,
    pub provider_colors: BTreeMap<String, HexColorOverride>,
    pub alerts_enabled: bool,
    pub notify_on_refresh: bool,
    pub alert_warning_threshold: u8,
    pub alert_critical_threshold: u8,
    pub launch_at_login: bool,
    pub claude_configured: bool,
    pub agy_configured: bool,
}

impl UiSnapshot {
    pub fn build(
        config: &AppConfig,
        claude: &ProviderState,
        codex: &ProviderState,
        agy: &ProviderState,
        is_refreshing: bool,
        now_secs: u64,
    ) -> Self {
        let order = config.normalized_provider_order();
        let state_for = |provider: Provider| match provider {
            Provider::Claude => claude,
            Provider::Codex => codex,
            Provider::Agy => agy,
        };

        let claude_health = claude.effective_health(now_secs, config.claude_stale_after_seconds);
        let codex_health = codex.effective_health(now_secs, config.codex_stale_after_seconds);
        let agy_health = agy.effective_health(now_secs, config.agy_stale_after_seconds);
        let health_for = |provider: Provider| match provider {
            Provider::Claude => &claude_health,
            Provider::Codex => &codex_health,
            Provider::Agy => &agy_health,
        };

        let claude_snap = claude.visible_snapshot(now_secs, config.snapshot_ttl_seconds);
        let codex_snap = codex.visible_snapshot(now_secs, config.snapshot_ttl_seconds);
        let agy_snap = agy.visible_snapshot(now_secs, config.snapshot_ttl_seconds);
        let snap_for = |provider: Provider| match provider {
            Provider::Claude => claude_snap,
            Provider::Codex => codex_snap,
            Provider::Agy => agy_snap,
        };

        let critical_threshold = config
            .highlight_critical
            .then_some(config.alert_critical_threshold);
        let menu_bar_segments = build_menu_bar_segments_ordered(
            &order,
            |provider| {
                if !config.is_provider_visible(provider) {
                    return None;
                }
                let snap = snap_for(provider);
                let stale = snap.is_some() && health_for(provider).is_stale();
                Some((snap, stale))
            },
            config.display_metric,
            config.menu_bar_windows,
            critical_threshold,
        );
        let menu_bar_fallback_only = menu_bar_segments.is_empty();

        let mut provider_rows = Vec::with_capacity(order.len());
        let mut tooltip_lines = vec![format!(
            "Ebb (5h / weekly {})",
            config.display_metric.label()
        )];
        for &provider in &order {
            let state = state_for(provider);
            let health = health_for(provider);
            let snap = snap_for(provider);
            let stale = snap.is_some() && health.is_stale();
            let visible = config.is_provider_visible(provider);
            let value_text = format_dropdown_snapshot_with(snap, config.display_metric);
            let reset_value_text = if config.show_reset_time {
                format_reset_values(snap, now_secs)
            } else {
                None
            };
            if visible {
                tooltip_lines.push(format!("{provider}: {value_text}"));
            }
            provider_rows.push(ProviderRowViewModel {
                provider,
                value_text,
                reset_value_text,
                stale,
                visible,
                tooltip: build_provider_tooltip(
                    state,
                    health,
                    snap,
                    config.display_metric,
                    now_secs,
                ),
            });
        }
        let menu_bar_tooltip = tooltip_lines.join("\n");

        let status_line = build_status_line(
            is_refreshing,
            &order,
            |p| {
                (
                    config.is_provider_visible(p),
                    health_for(p),
                    state_for(p).last_success_at,
                )
            },
            now_secs,
        );

        Self {
            menu_bar_segments,
            menu_bar_fallback_only,
            menu_bar_tooltip,
            provider_rows,
            status_line,
            is_refreshing,
            refresh_interval_seconds: config.refresh_interval_seconds,
            show_claude: config.show_claude,
            show_codex: config.show_codex,
            show_agy: config.show_agy,
            show_reset_time: config.show_reset_time,
            display_metric: config.display_metric,
            menu_bar_windows: config.menu_bar_windows,
            highlight_critical: config.highlight_critical,
            provider_order: order,
            color_theme: config.color_theme,
            provider_colors: config.provider_colors.clone(),
            alerts_enabled: config.alerts_enabled,
            notify_on_refresh: config.notify_on_refresh,
            alert_warning_threshold: config.alert_warning_threshold,
            alert_critical_threshold: config.alert_critical_threshold,
            launch_at_login: config.launch_at_login,
            claude_configured: false,
            agy_configured: false,
        }
    }

    /// Attach current status-line integration states for `Settings → Integrations` checkmarks.
    pub fn with_integrations(mut self, claude_configured: bool, agy_configured: bool) -> Self {
        self.claude_configured = claude_configured;
        self.agy_configured = agy_configured;
        self
    }

    /// Look up the row view model for a specific provider regardless of ordering.
    pub fn row_for(&self, provider: Provider) -> &ProviderRowViewModel {
        self.provider_rows
            .iter()
            .find(|r| r.provider == provider)
            .expect("UiSnapshot always contains all providers")
    }

    /// Plain text summary of Menu Bar content for tests and accessibility.
    pub fn menu_bar_plain_text(&self) -> String {
        if self.menu_bar_fallback_only {
            return MISSING_VALUE.to_owned();
        }
        self.menu_bar_segments
            .iter()
            .map(MenuBarSegment::plain_text)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn build_provider_tooltip(
    state: &ProviderState,
    effective_health: &ProviderHealth,
    visible_snapshot: Option<&QuotaSnapshot>,
    metric: DisplayMetric,
    now_secs: u64,
) -> String {
    let provider = state.provider;
    let has_visible_snapshot = visible_snapshot.is_some();
    let source_hint = match provider {
        Provider::Claude => "status-line cache (updates during active Claude Code sessions)",
        Provider::Codex => "Codex app-server poll",
        Provider::Agy => "status-line cache (updates during active Antigravity CLI sessions)",
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
            } else if !has_visible_snapshot && provider == Provider::Agy {
                "Waiting for Antigravity CLI status-line data".to_owned()
            } else if let Some(ref err) = state.last_error {
                format!("Unavailable ({err})")
            } else {
                "Unavailable".to_owned()
            }
        }
        ProviderHealth::Error(msg) => format!("Error: {msg}"),
    };

    let mut out = format!(
        "{provider}: {status_desc}\n5h / weekly {} · {source_hint}",
        metric.label()
    );
    if let Some(reset_desc) = format_reset_tooltip(visible_snapshot, now_secs) {
        out.push('\n');
        out.push_str(&reset_desc);
    }
    out
}

fn build_status_line<'a>(
    is_refreshing: bool,
    order: &[Provider],
    info_for: impl Fn(Provider) -> (bool, &'a ProviderHealth, Option<u64>),
    now_secs: u64,
) -> Option<String> {
    if is_refreshing {
        return Some("Refreshing…".to_owned());
    }

    let mut notes = Vec::new();
    let mut latest_ts: Option<u64> = None;

    for &provider in order {
        let (visible, health, ts_opt) = info_for(provider);
        if !visible {
            continue;
        }
        if let Some(ts) = ts_opt {
            latest_ts = Some(latest_ts.map_or(ts, |prev| prev.max(ts)));
        }
        if let Some(note) = short_health_note(provider, health) {
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

/// Format remaining duration until `resets_at` (minute granularity; never fakes `100%` when passed).
pub fn format_reset_countdown(now_secs: u64, resets_at: u64) -> String {
    if resets_at <= now_secs {
        return "resetting…".to_owned();
    }
    let remaining = resets_at - now_secs;
    if remaining < 60 {
        return "<1m".to_owned();
    }
    if remaining < 3600 {
        return format!("{}m", remaining / 60);
    }
    if remaining < 86_400 {
        let hours = remaining / 3600;
        let mins = (remaining % 3600) / 60;
        return if mins == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h {mins}m")
        };
    }
    let days = remaining / 86_400;
    let hours = (remaining % 86_400) / 3600;
    if hours == 0 {
        format!("{days}d")
    } else {
        format!("{days}d {hours}h")
    }
}

/// Format right-aligned reset countdown values for the `Reset in` sub-row (`2h 15m / 4d 6h`).
/// Returns `None` when neither window reports a `resets_at` timestamp.
pub fn format_reset_values(snapshot: Option<&QuotaSnapshot>, now_secs: u64) -> Option<String> {
    let snap = snapshot?;
    let five_ts = snap.five_hour.and_then(|w| w.resets_at);
    let weekly_ts = snap.weekly.and_then(|w| w.resets_at);
    if five_ts.is_none() && weekly_ts.is_none() {
        return None;
    }

    let left = five_ts
        .map(|ts| format_reset_countdown(now_secs, ts))
        .unwrap_or_else(|| MISSING_VALUE.to_owned());
    let right = weekly_ts
        .map(|ts| format_reset_countdown(now_secs, ts))
        .unwrap_or_else(|| MISSING_VALUE.to_owned());

    if left == "resetting…" && right == "resetting…" {
        Some("resetting…".to_owned())
    } else {
        Some(format!("{left} / {right}"))
    }
}

fn format_reset_tooltip(snapshot: Option<&QuotaSnapshot>, now_secs: u64) -> Option<String> {
    let snap = snapshot?;
    let mut parts = Vec::new();
    if let Some(ts) = snap.five_hour.and_then(|w| w.resets_at) {
        let c = format_reset_countdown(now_secs, ts);
        parts.push(if c == "resetting…" {
            "5h resetting…".to_owned()
        } else {
            format!("5h resets in {c}")
        });
    }
    if let Some(ts) = snap.weekly.and_then(|w| w.resets_at) {
        let c = format_reset_countdown(now_secs, ts);
        parts.push(if c == "resetting…" {
            "weekly resetting…".to_owned()
        } else {
            format!("weekly resets in {c}")
        });
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" · "))
    }
}
