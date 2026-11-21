use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};

/// En-dash (U+2013) used when a quota window or snapshot is missing.
pub const MISSING_VALUE: &str = "\u{2013}";

/// Supported quota providers (`Claude`, `Codex`, `Agy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
    Agy,
}

impl Provider {
    /// Default display order: Claude -> Codex -> Agy.
    pub const ALL: [Provider; 3] = [Provider::Claude, Provider::Codex, Provider::Agy];

    /// Alias of `ALL` kept for backward compatibility.
    pub const FIXED_ORDER: [Provider; 3] = Self::ALL;

    pub const fn display_name(self) -> &'static str {
        match self {
            Provider::Claude => "Claude",
            Provider::Codex => "Codex",
            Provider::Agy => "Agy",
        }
    }

    pub const fn key(self) -> &'static str {
        match self {
            Provider::Claude => "claude",
            Provider::Codex => "codex",
            Provider::Agy => "agy",
        }
    }
}

/// Whether quota percentages are displayed as remaining (default) or used.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayMetric {
    #[default]
    Remaining,
    Used,
}

impl DisplayMetric {
    /// Convert a canonical `remaining_percent` (`0..=100`) to the configured display value.
    pub const fn display_percent(self, remaining_percent: u8) -> u8 {
        match self {
            DisplayMetric::Remaining => remaining_percent,
            DisplayMetric::Used => 100u8.saturating_sub(remaining_percent),
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            DisplayMetric::Remaining => "remaining",
            DisplayMetric::Used => "used",
        }
    }
}

/// Which quota windows appear in the compact Menu Bar item.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MenuBarWindows {
    #[default]
    Both,
    FiveHourOnly,
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Quota window type mapped from provider duration or field name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowKind {
    FiveHour,
    Weekly,
}

impl WindowKind {
    pub const FIVE_HOUR_MINS: i64 = 300;
    pub const WEEKLY_MINS: i64 = 10_080;

    /// Map actual window duration in minutes to canonical window kind.
    /// Never assumes `primary` is 5h or `secondary` is weekly.
    pub const fn from_duration_mins(mins: i64) -> Option<Self> {
        match mins {
            Self::FIVE_HOUR_MINS => Some(Self::FiveHour),
            Self::WEEKLY_MINS => Some(Self::Weekly),
            _ => None,
        }
    }
}

impl fmt::Display for WindowKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowKind::FiveHour => f.write_str("5h"),
            WindowKind::Weekly => f.write_str("weekly"),
        }
    }
}

/// Normalized quota window storing remaining percentage (0..=100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaWindow {
    pub remaining_percent: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

impl QuotaWindow {
    /// Convert a provider-reported `used_percentage` (0.0..=100.0) into `remaining_percent`.
    pub fn from_used_percent(
        provider: Provider,
        window: WindowKind,
        used_percent: f64,
        resets_at: Option<u64>,
    ) -> Result<Self> {
        validate_percent(provider, window, used_percent)?;
        let remaining = (100.0 - used_percent).round().clamp(0.0, 100.0) as u8;
        Ok(Self {
            remaining_percent: remaining,
            resets_at,
        })
    }

    /// Store a provider-reported `remaining_percentage` directly without subtracting again.
    pub fn from_remaining_percent(
        provider: Provider,
        window: WindowKind,
        remaining_percent: f64,
        resets_at: Option<u64>,
    ) -> Result<Self> {
        validate_percent(provider, window, remaining_percent)?;
        let remaining = remaining_percent.round().clamp(0.0, 100.0) as u8;
        Ok(Self {
            remaining_percent: remaining,
            resets_at,
        })
    }
}

fn validate_percent(provider: Provider, window: WindowKind, value: f64) -> Result<()> {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        return Err(AppError::InvalidPercentage {
            provider,
            window,
            value,
        });
    }
    Ok(())
}

/// Normalized quota snapshot for one provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaSnapshot {
    pub provider: Provider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<QuotaWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub weekly: Option<QuotaWindow>,
    /// Unix timestamp (seconds) when this snapshot was observed/fetched.
    pub fetched_at: u64,
}

impl QuotaSnapshot {
    pub fn has_any_window(&self) -> bool {
        self.five_hour.is_some() || self.weekly.is_some()
    }

    /// Age in seconds relative to `now_secs` (saturates to 0 on clock skew).
    pub fn age_secs(&self, now_secs: u64) -> u64 {
        now_secs.saturating_sub(self.fetched_at)
    }

    pub fn is_stale(&self, now_secs: u64, stale_after_secs: u64) -> bool {
        self.age_secs(now_secs) >= stale_after_secs
    }

    pub fn is_expired(&self, now_secs: u64, ttl_secs: u64) -> bool {
        self.age_secs(now_secs) >= ttl_secs
    }

    /// Format compact Menu Bar text (no `%`, no provider prefix): e.g. `65/72`, `65/–`, `–/72`, `–/–`.
    pub fn format_menu_bar(&self) -> String {
        self.format_menu_bar_with(DisplayMetric::Remaining, MenuBarWindows::Both)
    }

    /// Format compact Menu Bar text respecting display metric (`Remaining`/`Used`) and window mode.
    pub fn format_menu_bar_with(&self, metric: DisplayMetric, windows: MenuBarWindows) -> String {
        let (five, weekly) = format_menu_bar_parts(self.five_hour, self.weekly, metric, windows);
        match weekly {
            Some(w) => format!("{five}/{w}"),
            None => five,
        }
    }

    /// Format native dropdown value text (with `%` on present values): e.g. `65% / 72%`, `65% / –`.
    pub fn format_dropdown(&self) -> String {
        self.format_dropdown_with(DisplayMetric::Remaining)
    }

    /// Format native dropdown value text respecting display metric (`Remaining`/`Used`).
    pub fn format_dropdown_with(&self, metric: DisplayMetric) -> String {
        format_dropdown_windows(self.five_hour, self.weekly, metric)
    }
}

/// Format Menu Bar string for an optional snapshot (default `Remaining` + `Both`).
pub fn format_menu_bar_snapshot(snapshot: Option<&QuotaSnapshot>) -> String {
    format_menu_bar_snapshot_with(snapshot, DisplayMetric::Remaining, MenuBarWindows::Both)
}

/// Format Menu Bar string for an optional snapshot with explicit display options.
pub fn format_menu_bar_snapshot_with(
    snapshot: Option<&QuotaSnapshot>,
    metric: DisplayMetric,
    windows: MenuBarWindows,
) -> String {
    let (five, weekly) = match snapshot {
        Some(s) => format_menu_bar_parts(s.five_hour, s.weekly, metric, windows),
        None => format_menu_bar_parts(None, None, metric, windows),
    };
    match weekly {
        Some(w) => format!("{five}/{w}"),
        None => five,
    }
}

/// Format Dropdown string for an optional snapshot (default `Remaining`).
pub fn format_dropdown_snapshot(snapshot: Option<&QuotaSnapshot>) -> String {
    format_dropdown_snapshot_with(snapshot, DisplayMetric::Remaining)
}

/// Format Dropdown string for an optional snapshot with explicit display metric.
pub fn format_dropdown_snapshot_with(
    snapshot: Option<&QuotaSnapshot>,
    metric: DisplayMetric,
) -> String {
    match snapshot {
        Some(s) => s.format_dropdown_with(metric),
        None => format_dropdown_windows(None, None, metric),
    }
}

fn format_menu_bar_parts(
    five_hour: Option<QuotaWindow>,
    weekly: Option<QuotaWindow>,
    metric: DisplayMetric,
    windows: MenuBarWindows,
) -> (String, Option<String>) {
    let left = five_hour
        .map(|w| metric.display_percent(w.remaining_percent).to_string())
        .unwrap_or_else(|| MISSING_VALUE.to_owned());
    let right = match windows {
        MenuBarWindows::Both => Some(
            weekly
                .map(|w| metric.display_percent(w.remaining_percent).to_string())
                .unwrap_or_else(|| MISSING_VALUE.to_owned()),
        ),
        MenuBarWindows::FiveHourOnly => None,
    };
    (left, right)
}

fn format_dropdown_windows(
    five_hour: Option<QuotaWindow>,
    weekly: Option<QuotaWindow>,
    metric: DisplayMetric,
) -> String {
    let left = five_hour
        .map(|w| format!("{}%", metric.display_percent(w.remaining_percent)))
        .unwrap_or_else(|| MISSING_VALUE.to_owned());
    let right = weekly
        .map(|w| format!("{}%", metric.display_percent(w.remaining_percent)))
        .unwrap_or_else(|| MISSING_VALUE.to_owned());
    format!("{left} / {right}")
}

/// One colored segment to render in the single `NSStatusItem`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MenuBarSegment {
    pub provider: Provider,
    pub five_hour_text: String,
    /// `None` when `MenuBarWindows::FiveHourOnly` is active.
    pub weekly_text: Option<String>,
    pub five_hour_critical: bool,
    pub weekly_critical: bool,
    pub stale: bool,
}

impl MenuBarSegment {
    /// Plain text representation of this segment (`65/72` or `65`).
    pub fn plain_text(&self) -> String {
        match &self.weekly_text {
            Some(w) => format!("{}/{w}", self.five_hour_text),
            None => self.five_hour_text.clone(),
        }
    }
}

/// Build ordered Menu Bar segments respecting custom `provider_order`, metric, window mode,
/// and optional per-window critical threshold (always evaluated against `remaining_percent`).
pub fn build_menu_bar_segments_ordered<'a>(
    order: &[Provider],
    providers: impl Fn(Provider) -> Option<(Option<&'a QuotaSnapshot>, bool)>,
    metric: DisplayMetric,
    windows: MenuBarWindows,
    critical_threshold: Option<u8>,
) -> Vec<MenuBarSegment> {
    let mut segments = Vec::with_capacity(order.len());
    for &provider in order {
        let Some((snap, stale)) = providers(provider) else {
            continue;
        };
        let (five_win, weekly_win) = match snap {
            Some(s) => (s.five_hour, s.weekly),
            None => (None, None),
        };
        let (five_hour_text, weekly_text) =
            format_menu_bar_parts(five_win, weekly_win, metric, windows);
        let five_hour_critical =
            critical_threshold.is_some_and(|t| five_win.is_some_and(|w| w.remaining_percent <= t));
        let weekly_critical = windows == MenuBarWindows::Both
            && critical_threshold
                .is_some_and(|t| weekly_win.is_some_and(|w| w.remaining_percent <= t));
        segments.push(MenuBarSegment {
            provider,
            five_hour_text,
            weekly_text,
            five_hour_critical,
            weekly_critical,
            stale,
        });
    }
    segments
}

/// Convenience helper building default-ordered (`Claude` -> `Codex`) Menu Bar segments.
pub fn build_menu_bar_segments(
    claude: Option<(&QuotaSnapshot, bool)>,
    codex: Option<(&QuotaSnapshot, bool)>,
    show_claude: bool,
    show_codex: bool,
) -> Vec<MenuBarSegment> {
    build_menu_bar_segments_ordered(
        &[Provider::Claude, Provider::Codex],
        |p| match p {
            Provider::Claude if show_claude => {
                Some(claude.map_or((None, false), |(s, st)| (Some(s), st)))
            }
            Provider::Codex if show_codex => {
                Some(codex.map_or((None, false), |(s, st)| (Some(s), st)))
            }
            _ => None,
        },
        DisplayMetric::Remaining,
        MenuBarWindows::Both,
        None,
    )
}

/// Health / freshness state tracked separately from `QuotaSnapshot`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealth {
    Fresh,
    Stale,
    Unavailable,
    Unauthenticated,
    Error(String),
}

impl ProviderHealth {
    pub fn is_stale(&self) -> bool {
        matches!(self, ProviderHealth::Stale)
    }
}

/// Runtime state for a single provider preserving last-known-good snapshot across transient failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderState {
    pub provider: Provider,
    pub snapshot: Option<QuotaSnapshot>,
    pub health: ProviderHealth,
    pub last_attempt_at: Option<u64>,
    pub last_success_at: Option<u64>,
    pub last_error: Option<String>,
}

impl ProviderState {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            snapshot: None,
            health: ProviderHealth::Unavailable,
            last_attempt_at: None,
            last_success_at: None,
            last_error: None,
        }
    }

    /// Seed state from persisted cache on startup (starts `Stale` until verified/refreshed).
    pub fn seed_from_cache(&mut self, snapshot: QuotaSnapshot) {
        if snapshot.provider != self.provider {
            return;
        }
        self.last_success_at = Some(snapshot.fetched_at);
        self.health = if snapshot.has_any_window() {
            ProviderHealth::Stale
        } else {
            ProviderHealth::Unavailable
        };
        self.snapshot = Some(snapshot);
    }

    /// Record a successful read from a provider adapter.
    pub fn apply_success(&mut self, snapshot: QuotaSnapshot, now_secs: u64) {
        self.last_attempt_at = Some(now_secs);
        self.last_error = None;
        if snapshot.has_any_window() {
            self.last_success_at = Some(snapshot.fetched_at);
            self.snapshot = Some(snapshot);
            self.health = ProviderHealth::Fresh;
        } else {
            // Provider responded honestly with no active windows; do not fabricate 0 or 100.
            self.snapshot = Some(snapshot);
            self.health = ProviderHealth::Unavailable;
        }
    }

    /// Record an adapter failure while preserving last-known-good snapshot if present.
    pub fn apply_failure(&mut self, err: &AppError, now_secs: u64) {
        self.last_attempt_at = Some(now_secs);
        let msg = err.to_string();
        self.last_error = Some(msg.clone());
        self.health = match err {
            AppError::Unauthenticated { .. } => ProviderHealth::Unauthenticated,
            AppError::ProviderUnavailable { .. } if self.snapshot.is_none() => {
                ProviderHealth::Unavailable
            }
            _ if self
                .snapshot
                .as_ref()
                .is_some_and(QuotaSnapshot::has_any_window) =>
            {
                ProviderHealth::Stale
            }
            _ => ProviderHealth::Error(msg),
        };
    }

    /// Compute effective health taking age into account.
    pub fn effective_health(&self, now_secs: u64, stale_after_secs: u64) -> ProviderHealth {
        if self.health == ProviderHealth::Fresh
            && let Some(ref snap) = self.snapshot
            && snap.is_stale(now_secs, stale_after_secs)
        {
            return ProviderHealth::Stale;
        }
        self.health.clone()
    }

    /// Return the snapshot visible in UI, hiding values only when past TTL.
    /// Deliberately NEVER resets quota to 100 after `resets_at`.
    pub fn visible_snapshot(&self, now_secs: u64, max_ttl_secs: u64) -> Option<&QuotaSnapshot> {
        let snap = self.snapshot.as_ref()?;
        if !snap.has_any_window() || snap.is_expired(now_secs, max_ttl_secs) {
            return None;
        }
        Some(snap)
    }
}

/// On-disk cache file (`usage-cache.json`) storing latest snapshots per provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageCache {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<QuotaSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<QuotaSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agy: Option<QuotaSnapshot>,
}

impl UsageCache {
    pub const CURRENT_VERSION: u32 = 1;

    pub fn empty() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            claude: None,
            codex: None,
            agy: None,
        }
    }

    pub fn snapshot_for(&self, provider: Provider) -> Option<&QuotaSnapshot> {
        match provider {
            Provider::Claude => self.claude.as_ref(),
            Provider::Codex => self.codex.as_ref(),
            Provider::Agy => self.agy.as_ref(),
        }
    }

    pub fn set_snapshot(&mut self, snapshot: QuotaSnapshot) {
        match snapshot.provider {
            Provider::Claude => self.claude = Some(snapshot),
            Provider::Codex => self.codex = Some(snapshot),
            Provider::Agy => self.agy = Some(snapshot),
        }
    }
}

impl Default for UsageCache {
    fn default() -> Self {
        Self::empty()
    }
}
