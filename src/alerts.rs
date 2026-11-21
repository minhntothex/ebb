use std::collections::HashMap;

use crate::config::AppConfig;
use crate::model::{Provider, QuotaSnapshot, QuotaWindow, WindowKind};
use crate::ui::view_model::format_reset_countdown;

/// Recovery buffer (percentage points above warning threshold) required to clear an alert
/// when `resets_at` is unavailable, preventing boundary flapping.
pub const ALERT_RECOVERY_HYSTERESIS: u8 = 5;

/// Minimum remaining percentage required to emit a `Refreshed` recovery notification
/// when `resets_at` did not change (for example, snapshots without `resets_at`).
pub const REFRESH_RECOVERY_MIN_PERCENT: u8 = 80;

/// Severity level for a low-quota alert (`Warning < Critical` for escalation checks).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AlertLevel {
    Warning,
    Critical,
}

impl AlertLevel {
    pub const fn as_str(self) -> &'static str {
        match self {
            AlertLevel::Warning => "warning",
            AlertLevel::Critical => "critical",
        }
    }
}

/// Kind of quota notification emitted by `AlertTracker`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AlertKind {
    Low(AlertLevel),
    Refreshed,
}

impl AlertKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            AlertKind::Low(level) => level.as_str(),
            AlertKind::Refreshed => "refreshed",
        }
    }
}

/// Actionable quota notification emitted by `AlertTracker`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaAlert {
    pub provider: Provider,
    pub window: WindowKind,
    pub kind: AlertKind,
    pub remaining_percent: u8,
    pub resets_at: Option<u64>,
}

impl QuotaAlert {
    /// Deterministic notification identifier for macOS Notification Center deduplication.
    pub fn notification_id(&self) -> String {
        let provider = self.provider.key();
        let window = match self.window {
            WindowKind::FiveHour => "5h",
            WindowKind::Weekly => "weekly",
        };
        format!("ebb.{provider}.{window}.{}", self.kind.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FiredWindowState {
    highest_level: AlertLevel,
    resets_at: Option<u64>,
    fired_at: u64,
}

/// Pure-Rust per-window quota alert state machine with anti-spam deduplication and recovery alerts.
///
/// Rules:
/// - Baseline seeding on startup suppresses stale replay notifications while tracking already-low
///   windows so they can still emit a `Refreshed` notification once their cycle resets.
/// - Low alerts fire at most once per `(Provider, WindowKind, AlertLevel)` per `resets_at` cycle.
/// - Allows upward severity escalation (`Warning -> Critical`) inside the same cycle.
/// - When a previously low/critical window recovers into a new reset cycle (or jumps to `>= 80%`
///   without `resets_at`), clears the low state and emits at most one `Refreshed` notification
///   when `cfg.notify_on_refresh` is enabled.
#[derive(Debug, Default, Clone)]
pub struct AlertTracker {
    fired: HashMap<(Provider, WindowKind), FiredWindowState>,
}

impl AlertTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record already-low cached windows on startup so launching the app never replays old alerts.
    pub fn seed_baseline(
        &mut self,
        snapshots: &[Option<&QuotaSnapshot>],
        cfg: &AppConfig,
        now_secs: u64,
    ) {
        for snap in snapshots.iter().copied().flatten() {
            for (kind, win) in [
                (WindowKind::FiveHour, snap.five_hour),
                (WindowKind::Weekly, snap.weekly),
            ] {
                if let Some(w) = win
                    && let Some(level) = classify_level(w.remaining_percent, cfg)
                {
                    self.fired.insert(
                        (snap.provider, kind),
                        FiredWindowState {
                            highest_level: level,
                            resets_at: w.resets_at,
                            fired_at: now_secs,
                        },
                    );
                }
            }
        }
    }

    /// Evaluate one freshly fetched snapshot and return any new alerts to deliver.
    pub fn evaluate(
        &mut self,
        snapshot: &QuotaSnapshot,
        cfg: &AppConfig,
        now_secs: u64,
    ) -> Vec<QuotaAlert> {
        if !cfg.alerts_enabled || !provider_visible(snapshot.provider, cfg) {
            return Vec::new();
        }

        let mut alerts = Vec::new();
        for (kind, win) in [
            (WindowKind::FiveHour, snapshot.five_hour),
            (WindowKind::Weekly, snapshot.weekly),
        ] {
            if let Some(window) = win
                && let Some(alert) =
                    self.evaluate_window(snapshot.provider, kind, window, cfg, now_secs)
            {
                alerts.push(alert);
            }
        }
        alerts
    }

    fn evaluate_window(
        &mut self,
        provider: Provider,
        window_kind: WindowKind,
        window: QuotaWindow,
        cfg: &AppConfig,
        now_secs: u64,
    ) -> Option<QuotaAlert> {
        let key = (provider, window_kind);
        let target_level = classify_level(window.remaining_percent, cfg);
        let recovery_cutoff = cfg
            .alert_warning_threshold
            .saturating_add(ALERT_RECOVERY_HYSTERESIS)
            .min(100);

        if let Some(&prev) = self.fired.get(&key) {
            let new_reset_cycle = matches!(
                (prev.resets_at, window.resets_at),
                (Some(old_ts), Some(new_ts)) if old_ts != new_ts
            );
            let recovered = target_level.is_none() && window.remaining_percent >= recovery_cutoff;

            if new_reset_cycle || recovered {
                self.fired.remove(&key);

                // Emit one calm recovery notification if the window is now genuinely healthy
                // (new reset cycle above recovery cutoff, OR strong recovery >= 80% without `resets_at`).
                let strong_recovery = window.remaining_percent >= REFRESH_RECOVERY_MIN_PERCENT;
                if target_level.is_none()
                    && cfg.notify_on_refresh
                    && ((new_reset_cycle && recovered) || (!new_reset_cycle && strong_recovery))
                {
                    return Some(QuotaAlert {
                        provider,
                        window: window_kind,
                        kind: AlertKind::Refreshed,
                        remaining_percent: window.remaining_percent,
                        resets_at: window.resets_at,
                    });
                }
            }
        }

        let level = target_level?;
        let should_fire = match self.fired.get(&key) {
            None => true,
            Some(prev) => level > prev.highest_level,
        };

        if !should_fire {
            return None;
        }

        self.fired.insert(
            key,
            FiredWindowState {
                highest_level: level,
                resets_at: window.resets_at,
                fired_at: now_secs,
            },
        );

        Some(QuotaAlert {
            provider,
            window: window_kind,
            kind: AlertKind::Low(level),
            remaining_percent: window.remaining_percent,
            resets_at: window.resets_at,
        })
    }
}

fn classify_level(remaining_percent: u8, cfg: &AppConfig) -> Option<AlertLevel> {
    if remaining_percent <= cfg.alert_critical_threshold {
        Some(AlertLevel::Critical)
    } else if remaining_percent <= cfg.alert_warning_threshold {
        Some(AlertLevel::Warning)
    } else {
        None
    }
}

fn provider_visible(provider: Provider, cfg: &AppConfig) -> bool {
    cfg.is_provider_visible(provider)
}

/// Format crisp `(title, body)` copy for a macOS notification.
pub fn format_alert_copy(alert: &QuotaAlert, now_secs: u64) -> (String, String) {
    let title = match alert.kind {
        AlertKind::Low(AlertLevel::Warning) => {
            format!("Ebb — {} {} quota low", alert.provider, alert.window)
        }
        AlertKind::Low(AlertLevel::Critical) => {
            format!("Ebb — {} {} quota critical", alert.provider, alert.window)
        }
        AlertKind::Refreshed => {
            format!("Ebb — {} {} quota refreshed", alert.provider, alert.window)
        }
    };

    let body = match alert.resets_at {
        Some(ts) => {
            let countdown = format_reset_countdown(now_secs, ts);
            if countdown == "resetting…" {
                format!("{}% remaining · resetting…", alert.remaining_percent)
            } else {
                format!(
                    "{}% remaining · resets in {countdown}",
                    alert.remaining_percent
                )
            }
        }
        None => format!("{}% remaining", alert.remaining_percent),
    };
    (title, body)
}
