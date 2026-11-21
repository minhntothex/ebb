use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{AppError, Result};
use crate::model::{Provider, QuotaSnapshot, QuotaWindow, WindowKind};
pub use crate::providers::statusline_common::{
    MAX_STATUSLINE_STDIN_BYTES, MIN_IDENTICAL_REWRITE_SECS, StatuslineBridgeArgs,
    StatuslineInstallPlan, default_claude_settings_path, shell_single_quote,
};
use crate::providers::statusline_common::{
    apply_install, build_wrapped_command, plan_install, read_provider_cache,
    run_bridge_with_parser, write_provider_cache_atomic,
};

/// Subcommand token used to detect already-wrapped Claude status-line configs.
pub const CLAUDE_STATUSLINE_SUBCOMMAND: &str = "claude-statusline";

#[derive(Debug, Deserialize)]
struct RawStatusLinePayload {
    #[serde(default)]
    rate_limits: Option<RawRateLimits>,
}

#[derive(Debug, Deserialize)]
struct RawRateLimits {
    #[serde(default)]
    five_hour: Option<RawQuotaWindow>,
    #[serde(default)]
    seven_day: Option<RawQuotaWindow>,
    // `spend_limit` (gateway-only) is intentionally ignored.
}

#[derive(Debug, Deserialize)]
struct RawQuotaWindow {
    #[serde(default)]
    used_percentage: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_epoch_secs")]
    resets_at: Option<u64>,
}

fn deserialize_optional_epoch_secs<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<Value>::deserialize(deserializer)?;
    match opt {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else if let Some(f) = n.as_f64()
                && f.is_finite()
                && f >= 0.0
            {
                Ok(Some(f.round() as u64))
            } else {
                Ok(None)
            }
        }
        Some(_) => Ok(None),
    }
}

/// Parse Claude Code status-line JSON bytes into a normalized `QuotaSnapshot`.
///
/// Returns `Ok(None)` when `rate_limits` or both `five_hour`/`seven_day` windows are absent
/// (for example before the first API response or on gateway-only spend limits).
/// Returns `Err` on malformed JSON or out-of-range percentages.
pub fn parse_statusline_json(bytes: &[u8], observed_at: u64) -> Result<Option<QuotaSnapshot>> {
    let payload: RawStatusLinePayload =
        serde_json::from_slice(bytes).map_err(|e| AppError::Protocol {
            provider: Provider::Claude,
            message: format!("invalid status-line JSON: {e}"),
        })?;

    let Some(limits) = payload.rate_limits else {
        return Ok(None);
    };

    let five_hour = match limits
        .five_hour
        .and_then(|w| w.used_percentage.map(|used| (used, w.resets_at)))
    {
        Some((used, resets_at)) => Some(QuotaWindow::from_used_percent(
            Provider::Claude,
            WindowKind::FiveHour,
            used,
            resets_at,
        )?),
        None => None,
    };

    let weekly = match limits
        .seven_day
        .and_then(|w| w.used_percentage.map(|used| (used, w.resets_at)))
    {
        Some((used, resets_at)) => Some(QuotaWindow::from_used_percent(
            Provider::Claude,
            WindowKind::Weekly,
            used,
            resets_at,
        )?),
        None => None,
    };

    if five_hour.is_none() && weekly.is_none() {
        return Ok(None);
    }

    Ok(Some(QuotaSnapshot {
        provider: Provider::Claude,
        five_hour,
        weekly,
        fetched_at: observed_at,
    }))
}

/// Persist a Claude snapshot into its dedicated atomic `0600` cache file.
pub fn write_claude_cache_atomic(
    cache_path: &Path,
    snapshot: &QuotaSnapshot,
    min_rewrite_secs: u64,
) -> Result<bool> {
    write_provider_cache_atomic(cache_path, Provider::Claude, snapshot, min_rewrite_secs)
}

/// Read-only adapter used by the Menu Bar refresh coordinator.
/// Never invokes Claude CLI or creates model requests.
#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    cache_path: PathBuf,
}

impl ClaudeAdapter {
    pub fn new(cache_path: PathBuf) -> Self {
        Self { cache_path }
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    /// Read the latest cached Claude snapshot, or `Ok(None)` if no cache file exists yet.
    pub fn read_snapshot(&self) -> Result<Option<QuotaSnapshot>> {
        read_provider_cache(&self.cache_path, Provider::Claude)
    }
}

/// Execute the `claude-statusline` bridge.
pub fn run_statusline_bridge(cache_path: &Path, chained_command: &[String]) -> i32 {
    run_bridge_with_parser(
        cache_path,
        Provider::Claude,
        chained_command,
        parse_statusline_json,
    )
}

/// Build the wrapped Claude `statusLine.command` string, chaining any existing command verbatim.
pub fn build_wrapped_statusline_command(
    binary_path: &Path,
    existing_command: Option<&str>,
) -> String {
    build_wrapped_command(binary_path, CLAUDE_STATUSLINE_SUBCOMMAND, existing_command)
}

/// Inspect Claude's `settings.json` without modifying it and produce an opt-in install plan.
pub fn plan_statusline_install(
    settings_path: &Path,
    binary_path: &Path,
) -> Result<StatuslineInstallPlan> {
    plan_install(settings_path, binary_path, CLAUDE_STATUSLINE_SUBCOMMAND)
}

/// Apply an opt-in Claude `settings.json` update, creating a timestamped backup first.
/// Preserves all unrelated keys in `settings.json` and is idempotent when already installed.
pub fn apply_statusline_install(
    settings_path: &Path,
    binary_path: &Path,
) -> Result<(StatuslineInstallPlan, Option<PathBuf>)> {
    apply_install(
        settings_path,
        binary_path,
        CLAUDE_STATUSLINE_SUBCOMMAND,
        |_, _| {},
    )
}
