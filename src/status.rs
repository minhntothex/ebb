use std::collections::BTreeMap;

use serde::Serialize;

use crate::APP_VERSION;
use crate::config::{AppConfig, AppPaths};
use crate::error::Result;
use crate::model::{
    DisplayMetric, MenuBarWindows, Provider, ProviderHealth, ProviderState, QuotaWindow,
};
use crate::providers::agy::AgyAdapter;
use crate::providers::claude::ClaudeAdapter;
use crate::providers::codex::CodexAdapter;
use crate::refresh::AppState;

/// Versioned JSON schema emitted by `ebb status --json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusReport {
    pub version: u32,
    pub app_version: &'static str,
    pub generated_at: u64,
    pub display_metric: DisplayMetric,
    pub menu_bar_windows: MenuBarWindows,
    pub provider_order: Vec<Provider>,
    pub providers: BTreeMap<String, ProviderStatusReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderStatusReport {
    pub visible: bool,
    pub health: ProviderHealth,
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub five_hour: Option<WindowStatusReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weekly: Option<WindowStatusReport>,
    pub menu_bar_text: String,
    pub dropdown_text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WindowStatusReport {
    pub remaining_percent: u8,
    pub used_percent: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
}

impl WindowStatusReport {
    fn from_window(w: QuotaWindow) -> Self {
        Self {
            remaining_percent: w.remaining_percent,
            used_percent: 100u8.saturating_sub(w.remaining_percent),
            resets_at: w.resets_at,
        }
    }
}

/// Collect a normalized status report from local cache files (`<2 ms`, zero subprocesses by default).
/// When `live == true`, also queries `CodexAdapter` for a fresh Codex snapshot.
pub fn collect_status_report(paths: &AppPaths, live: bool, now_secs: u64) -> Result<StatusReport> {
    let config = AppConfig::load_or_default(&paths.config_path).unwrap_or_default();
    let persisted_cache = paths.load_cache_or_empty().unwrap_or_default();
    let claude_snapshot = ClaudeAdapter::new(paths.claude_cache_path.clone())
        .read_snapshot()
        .ok()
        .flatten();
    let agy_snapshot = AgyAdapter::new(paths.agy_cache_path.clone())
        .read_snapshot()
        .ok()
        .flatten();

    let mut state = AppState::from_persisted(
        config,
        persisted_cache,
        claude_snapshot,
        agy_snapshot,
        now_secs,
    );

    if live {
        let codex = CodexAdapter::new(state.config.codex_path.clone());
        match codex.fetch_snapshot() {
            Ok(snap) => state.codex.apply_success(snap, now_secs),
            Err(err) => state.codex.apply_failure(&err, now_secs),
        }
    }

    Ok(build_status_report(&state, now_secs))
}

/// Build a `StatusReport` directly from an in-memory `AppState`.
pub fn build_status_report(state: &AppState, now_secs: u64) -> StatusReport {
    let cfg = &state.config;
    let order = cfg.normalized_provider_order();
    let mut providers = BTreeMap::new();

    for &provider in &order {
        let p_state = match provider {
            Provider::Claude => &state.claude,
            Provider::Codex => &state.codex,
            Provider::Agy => &state.agy,
        };
        providers.insert(
            provider.key().to_owned(),
            build_provider_report(p_state, cfg, now_secs),
        );
    }

    StatusReport {
        version: 1,
        app_version: APP_VERSION,
        generated_at: now_secs,
        display_metric: cfg.display_metric,
        menu_bar_windows: cfg.menu_bar_windows,
        provider_order: order,
        providers,
    }
}

fn build_provider_report(
    state: &ProviderState,
    cfg: &AppConfig,
    now_secs: u64,
) -> ProviderStatusReport {
    let health = state.effective_health(now_secs, cfg.stale_after_seconds(state.provider));
    let snap = state.visible_snapshot(now_secs, cfg.snapshot_ttl_seconds);
    let stale = snap.is_some() && health.is_stale();

    ProviderStatusReport {
        visible: cfg.is_provider_visible(state.provider),
        health,
        stale,
        fetched_at: snap.map(|s| s.fetched_at),
        five_hour: snap
            .and_then(|s| s.five_hour)
            .map(WindowStatusReport::from_window),
        weekly: snap
            .and_then(|s| s.weekly)
            .map(WindowStatusReport::from_window),
        menu_bar_text: crate::model::format_menu_bar_snapshot_with(
            snap,
            cfg.display_metric,
            cfg.menu_bar_windows,
        ),
        dropdown_text: crate::model::format_dropdown_snapshot_with(snap, cfg.display_metric),
    }
}

/// Format compact single-line plain text suitable for tmux, SketchyBar, or shell prompts.
pub fn format_status_plain(report: &StatusReport) -> String {
    let mut parts = Vec::new();
    for &provider in &report.provider_order {
        if let Some(info) = report.providers.get(provider.key())
            && info.visible
        {
            parts.push(format!("{} {}", provider.key(), info.menu_bar_text));
        }
    }
    if parts.is_empty() {
        crate::model::MISSING_VALUE.to_owned()
    } else {
        parts.join("  ")
    }
}

/// Serialize `StatusReport` as compact single-line JSON for scripting.
pub fn format_status_json(report: &StatusReport) -> Result<String> {
    serde_json::to_string(report).map_err(Into::into)
}
