use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::error::{AppError, Result};
use crate::model::{Provider, QuotaSnapshot, QuotaWindow, WindowKind};
pub use crate::providers::statusline_common::{
    MIN_IDENTICAL_REWRITE_SECS, StatuslineBridgeArgs, StatuslineInstallPlan,
    default_agy_settings_path, shell_single_quote,
};
use crate::providers::statusline_common::{
    apply_install, build_wrapped_command, plan_install, read_provider_cache,
    run_bridge_with_parser, write_provider_cache_atomic,
};

/// Subcommand token used to detect already-wrapped Antigravity CLI status-line configs.
pub const AGY_STATUSLINE_SUBCOMMAND: &str = "agy-statusline";

const FIVE_HOUR_MAX_SECS: u64 = 18_600; // 5h + 10m buffer
const WEEKLY_MIN_SECS: u64 = 86_400; // > 1 day
const WEEKLY_MAX_SECS: u64 = 620_000; // ~7.17 days

#[derive(Debug, Deserialize)]
struct RawAgyPayload {
    #[serde(default)]
    model: Option<RawAgyModel>,
    #[serde(default)]
    quota: Option<BTreeMap<String, RawAgyQuotaBucket>>,
}

#[derive(Debug, Deserialize)]
struct RawAgyModel {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawAgyQuotaBucket {
    #[serde(default)]
    remaining_fraction: Option<f64>,
    #[serde(default)]
    reset_time: Option<String>,
    #[serde(default)]
    reset_in_seconds: Option<f64>,
    #[serde(default)]
    disabled: bool,
}

#[derive(Debug, Clone)]
struct CandidateBucket<'a> {
    id_lower: String,
    window: QuotaWindow,
    _raw_id: &'a str,
}

/// Parse Antigravity CLI (`agy`) `statusLine` JSON stdin bytes into a normalized `QuotaSnapshot`.
///
/// Rules:
/// - Never fabricates quota from `context_window`, `cost`, or `remaining_amount` without a fraction.
/// - Ignores buckets with `disabled: true`.
/// - Prefers buckets matching the active `model` family (`gemini` / `claude` / `gpt`), falling back
///   to the lowest-remaining bucket in each window (`5h` / `weekly`) so exhausted pools are never hidden.
/// - Prefers `observed_at + reset_in_seconds`, falling back to RFC-3339 UTC parsing of `reset_time`.
pub fn parse_agy_statusline_json(bytes: &[u8], observed_at: u64) -> Result<Option<QuotaSnapshot>> {
    let payload: RawAgyPayload = serde_json::from_slice(bytes).map_err(|e| AppError::Protocol {
        provider: Provider::Agy,
        message: format!("invalid agy status-line JSON: {e}"),
    })?;

    let Some(quota_map) = payload.quota else {
        return Ok(None);
    };
    if quota_map.is_empty() {
        return Ok(None);
    }

    let preferred_family = detect_model_family(payload.model.as_ref());
    let mut five_hour_candidates: Vec<CandidateBucket<'_>> = Vec::new();
    let mut weekly_candidates: Vec<CandidateBucket<'_>> = Vec::new();

    for (bucket_id, bucket) in &quota_map {
        if bucket.disabled {
            continue;
        }
        let Some(fraction) = bucket.remaining_fraction else {
            // Honest quota rule: `remaining_amount` without denominator cannot be turned into `%`.
            continue;
        };

        let reset_in_secs = bucket
            .reset_in_seconds
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(|s| s.round() as u64);
        let resets_at = match reset_in_secs {
            Some(secs) => Some(observed_at.saturating_add(secs)),
            None => bucket
                .reset_time
                .as_deref()
                .and_then(parse_rfc3339_utc_secs),
        };
        let duration_hint_secs = reset_in_secs
            .or_else(|| resets_at.and_then(|ts| ts.checked_sub(observed_at).filter(|&d| d > 0)));

        let Some(window_kind) = classify_agy_window(bucket_id, duration_hint_secs) else {
            continue;
        };

        if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
            return Err(AppError::InvalidPercentage {
                provider: Provider::Agy,
                window: window_kind,
                value: fraction * 100.0,
            });
        }

        let window = QuotaWindow::from_remaining_percent(
            Provider::Agy,
            window_kind,
            fraction * 100.0,
            resets_at,
        )?;
        let candidate = CandidateBucket {
            id_lower: bucket_id.to_ascii_lowercase(),
            window,
            _raw_id: bucket_id.as_str(),
        };

        match window_kind {
            WindowKind::FiveHour => five_hour_candidates.push(candidate),
            WindowKind::Weekly => weekly_candidates.push(candidate),
        }
    }

    let five_hour = select_candidate(&five_hour_candidates, preferred_family);
    let weekly = select_candidate(&weekly_candidates, preferred_family);

    if five_hour.is_none() && weekly.is_none() {
        return Ok(None);
    }

    Ok(Some(QuotaSnapshot {
        provider: Provider::Agy,
        five_hour,
        weekly,
        fetched_at: observed_at,
    }))
}

fn detect_model_family(model: Option<&RawAgyModel>) -> Option<&'static str> {
    let model = model?;
    for raw in [model.id.as_deref(), model.display_name.as_deref()]
        .into_iter()
        .flatten()
    {
        let lower = raw.to_ascii_lowercase();
        if lower.contains("claude") {
            return Some("claude");
        }
        if lower.contains("gpt") || lower.contains("openai") || lower.contains("codex") {
            return Some("gpt");
        }
        if lower.contains("gemini") {
            return Some("gemini");
        }
    }
    None
}

fn classify_agy_window(bucket_id: &str, duration_hint_secs: Option<u64>) -> Option<WindowKind> {
    let lower = bucket_id.to_ascii_lowercase();
    if lower.contains("5h")
        || lower.contains("5-h")
        || lower.contains("5_h")
        || lower.contains("five_hour")
        || lower.contains("five-hour")
        || lower.contains("fivehour")
        || lower.contains("short")
        || lower.contains("session")
    {
        return Some(WindowKind::FiveHour);
    }
    if lower.contains("weekly")
        || lower.contains("7d")
        || lower.contains("7-d")
        || lower.contains("7_d")
        || lower.contains("seven_day")
        || lower.contains("seven-day")
        || lower.contains("week")
    {
        return Some(WindowKind::Weekly);
    }
    if let Some(secs) = duration_hint_secs {
        if secs > 0 && secs <= FIVE_HOUR_MAX_SECS {
            return Some(WindowKind::FiveHour);
        }
        if secs > WEEKLY_MIN_SECS && secs <= WEEKLY_MAX_SECS {
            return Some(WindowKind::Weekly);
        }
    }
    None
}

fn select_candidate(
    candidates: &[CandidateBucket<'_>],
    preferred_family: Option<&str>,
) -> Option<QuotaWindow> {
    if candidates.is_empty() {
        return None;
    }

    let family_matches: Vec<&CandidateBucket<'_>> = match preferred_family {
        Some(fam) => candidates
            .iter()
            .filter(|c| c.id_lower.contains(fam))
            .collect(),
        None => Vec::new(),
    };

    let pool: Vec<&CandidateBucket<'_>> = if family_matches.is_empty() {
        candidates.iter().collect()
    } else {
        family_matches
    };

    // Pick lowest remaining % (tie-break by earliest resets_at) so tightest active limit surfaces.
    pool.into_iter()
        .min_by_key(|c| {
            (
                c.window.remaining_percent,
                c.window.resets_at.unwrap_or(u64::MAX),
            )
        })
        .map(|c| c.window)
}

/// Pure-Rust RFC-3339 timestamp parser (`YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)` -> Unix seconds).
pub fn parse_rfc3339_utc_secs(input: &str) -> Option<u64> {
    let s = input.trim();
    if s.len() < 20 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-'
        || b[7] != b'-'
        || (b[10] != b'T' && b[10] != b't')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }

    let year: i64 = s[0..4].parse().ok()?;
    let month: u32 = s[5..7].parse().ok()?;
    let day: u32 = s[8..10].parse().ok()?;
    let hour: u32 = s[11..13].parse().ok()?;
    let minute: u32 = s[14..16].parse().ok()?;
    let second_raw: u32 = s[17..19].parse().ok()?;

    if year < 1970 || !(1..=12).contains(&month) || hour > 23 || minute > 59 || second_raw > 60 {
        return None;
    }
    if day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let second = second_raw.min(59);

    let mut rest = &s[19..];
    if let Some(after_dot) = rest.strip_prefix('.') {
        let frac_len = after_dot.bytes().take_while(|c| c.is_ascii_digit()).count();
        if frac_len == 0 {
            return None;
        }
        rest = &after_dot[frac_len..];
    }

    let tz_offset_secs: i64 = if rest == "Z" || rest == "z" {
        0
    } else if rest.len() == 6
        && (rest.starts_with('+') || rest.starts_with('-'))
        && rest.as_bytes()[3] == b':'
    {
        let sign: i64 = if rest.starts_with('+') { 1 } else { -1 };
        let off_h: i64 = rest[1..3].parse().ok()?;
        let off_m: i64 = rest[4..6].parse().ok()?;
        if off_h > 23 || off_m > 59 {
            return None;
        }
        sign * (off_h * 3600 + off_m * 60)
    } else {
        return None;
    };

    let days = days_from_civil(year, month, day);
    let utc_secs = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3600 + i64::from(minute) * 60 + i64::from(second))?
        .checked_sub(tz_offset_secs)?;
    u64::try_from(utc_secs).ok()
}

fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant's civil-to-days algorithm returning days since 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let m = u64::from(month);
    let d = u64::from(day);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + (doe as i64) - 719_468
}

/// Persist an Antigravity CLI snapshot into its dedicated atomic `0600` cache file.
pub fn write_agy_cache_atomic(
    cache_path: &Path,
    snapshot: &QuotaSnapshot,
    min_rewrite_secs: u64,
) -> Result<bool> {
    write_provider_cache_atomic(cache_path, Provider::Agy, snapshot, min_rewrite_secs)
}

/// Read-only adapter used by the Menu Bar refresh coordinator.
/// Never spawns `agy` CLI or creates model requests.
#[derive(Debug, Clone)]
pub struct AgyAdapter {
    cache_path: PathBuf,
}

impl AgyAdapter {
    pub fn new(cache_path: PathBuf) -> Self {
        Self { cache_path }
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    pub fn read_snapshot(&self) -> Result<Option<QuotaSnapshot>> {
        read_provider_cache(&self.cache_path, Provider::Agy)
    }
}

/// Execute the `agy-statusline` bridge.
pub fn run_agy_statusline_bridge(cache_path: &Path, chained_command: &[String]) -> i32 {
    run_bridge_with_parser(
        cache_path,
        Provider::Agy,
        chained_command,
        parse_agy_statusline_json,
    )
}

/// Build the wrapped Antigravity CLI `statusLine.command` string.
pub fn build_wrapped_agy_statusline_command(
    binary_path: &Path,
    existing_command: Option<&str>,
) -> String {
    build_wrapped_command(binary_path, AGY_STATUSLINE_SUBCOMMAND, existing_command)
}

/// Inspect `~/.gemini/antigravity-cli/settings.json` without modifying it.
pub fn plan_agy_statusline_install(
    settings_path: &Path,
    binary_path: &Path,
) -> Result<StatuslineInstallPlan> {
    plan_install(settings_path, binary_path, AGY_STATUSLINE_SUBCOMMAND)
}

/// Apply an opt-in Antigravity CLI `settings.json` update with a timestamped backup.
///
/// When no custom `statusLine.command` existed previously, sets `"stack_with_default": true`
/// and `"enabled": true` so Antigravity's built-in status line remains 100% visible while
/// `ebb agy-statusline` taps quota silently with empty stdout.
/// When chaining an existing custom command, preserves user `stack_with_default`/`padding`.
pub fn apply_agy_statusline_install(
    settings_path: &Path,
    binary_path: &Path,
) -> Result<(StatuslineInstallPlan, Option<PathBuf>)> {
    apply_install(
        settings_path,
        binary_path,
        AGY_STATUSLINE_SUBCOMMAND,
        |status_line, had_existing| {
            status_line
                .entry("enabled".to_owned())
                .or_insert(Value::Bool(true));
            if !had_existing {
                status_line.insert("enabled".into(), Value::Bool(true));
                status_line.insert("stack_with_default".into(), Value::Bool(true));
            }
        },
    )
}
