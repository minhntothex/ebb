use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ebb::alerts::{AlertLevel, AlertTracker, QuotaAlert, format_alert_copy};
use ebb::config::{
    AppConfig, DEFAULT_ALERT_CRITICAL_THRESHOLD, DEFAULT_ALERT_WARNING_THRESHOLD,
    DEFAULT_ALERTS_ENABLED, DEFAULT_SHOW_RESET_TIME,
};
use ebb::error::AppError;
use ebb::model::{Provider, ProviderState, QuotaSnapshot, QuotaWindow, WindowKind};
use ebb::ui::view_model::{UiSnapshot, format_reset_countdown, format_reset_values};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ebb-alerts-reset-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

fn snap(
    provider: Provider,
    five: Option<(u8, Option<u64>)>,
    weekly: Option<(u8, Option<u64>)>,
    fetched_at: u64,
) -> QuotaSnapshot {
    QuotaSnapshot {
        provider,
        five_hour: five.map(|(remaining_percent, resets_at)| QuotaWindow {
            remaining_percent,
            resets_at,
        }),
        weekly: weekly.map(|(remaining_percent, resets_at)| QuotaWindow {
            remaining_percent,
            resets_at,
        }),
        fetched_at,
    }
}

#[test]
fn config_26_1_is_backward_compatible_with_26_0_and_validates_thresholds() {
    let dir = temp_dir("compat");
    let cfg_path = dir.join("config.json");

    // Simulate a 26.0 config.json without any 26.1 alert/reset keys.
    fs::write(
        &cfg_path,
        r#"{
            "refresh_interval_seconds": 120,
            "show_claude": true,
            "show_codex": true,
            "launch_at_login": false
        }"#,
    )
    .unwrap();

    let mut cfg = AppConfig::load_or_default(&cfg_path).unwrap();
    assert_eq!(cfg.show_reset_time, DEFAULT_SHOW_RESET_TIME);
    assert_eq!(cfg.alerts_enabled, DEFAULT_ALERTS_ENABLED);
    assert_eq!(cfg.alert_warning_threshold, DEFAULT_ALERT_WARNING_THRESHOLD);
    assert_eq!(
        cfg.alert_critical_threshold,
        DEFAULT_ALERT_CRITICAL_THRESHOLD
    );

    // Lowering warning below current critical auto-adjusts critical to preserve invariant.
    cfg.set_critical_threshold(10).unwrap();
    assert_eq!(cfg.alert_critical_threshold, 10);
    cfg.set_warning_threshold(10).unwrap();
    assert_eq!(cfg.alert_warning_threshold, 10);
    assert_eq!(cfg.alert_critical_threshold, 5);

    // Invalid threshold combinations rejected.
    assert!(matches!(
        cfg.set_critical_threshold(0),
        Err(AppError::InvalidConfig(_))
    ));
    assert!(matches!(
        cfg.set_critical_threshold(10),
        Err(AppError::InvalidConfig(_))
    ));

    let bad = AppConfig {
        alert_warning_threshold: 15,
        alert_critical_threshold: 20,
        ..AppConfig::default()
    };
    assert!(matches!(bad.validate(), Err(AppError::InvalidConfig(_))));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn reset_countdown_formats_all_time_ranges_and_expired_windows() {
    let now = 10_000;
    assert_eq!(format_reset_countdown(now, now), "resetting…");
    assert_eq!(format_reset_countdown(now, now - 50), "resetting…");
    assert_eq!(format_reset_countdown(now, now + 1), "<1m");
    assert_eq!(format_reset_countdown(now, now + 59), "<1m");
    assert_eq!(format_reset_countdown(now, now + 60), "1m");
    assert_eq!(format_reset_countdown(now, now + 48 * 60), "48m");
    assert_eq!(format_reset_countdown(now, now + 3 * 3600), "3h");
    assert_eq!(
        format_reset_countdown(now, now + 2 * 3600 + 15 * 60),
        "2h 15m"
    );
    assert_eq!(format_reset_countdown(now, now + 24 * 3600), "1d");
    assert_eq!(
        format_reset_countdown(now, now + 4 * 86_400 + 6 * 3600),
        "4d 6h"
    );
}

#[test]
fn ui_snapshot_populates_reset_line_and_tooltip_without_altering_menu_bar() {
    let now = 10_000;
    let mut cfg = AppConfig::default();

    let mut claude = ProviderState::new(Provider::Claude);
    claude.apply_success(
        snap(
            Provider::Claude,
            Some((65, Some(now + 2 * 3600 + 15 * 60))),
            Some((72, Some(now + 4 * 86_400 + 6 * 3600))),
            now,
        ),
        now,
    );

    let mut codex = ProviderState::new(Provider::Codex);
    codex.apply_success(
        snap(
            Provider::Codex,
            Some((43, Some(now - 5))), // expired reset window
            Some((31, Some(now + 5 * 86_400 + 11 * 3600))),
            now,
        ),
        now,
    );

    let ui = UiSnapshot::build(&cfg, &claude, &codex, false, now);
    // Menu bar stays strictly minimal numbers only.
    assert_eq!(ui.menu_bar_plain_text(), "65/72 43/31");
    assert_eq!(
        ui.claude_row.reset_value_text.as_deref(),
        Some("2h 15m / 4d 6h")
    );
    assert_eq!(
        ui.codex_row.reset_value_text.as_deref(),
        Some("resetting… / 5d 11h")
    );
    assert!(ui.claude_row.tooltip.contains("5h resets in 2h 15m"));
    assert!(ui.claude_row.tooltip.contains("weekly resets in 4d 6h"));
    assert!(ui.codex_row.tooltip.contains("5h resetting…"));

    // Turning off `show_reset_time` hides dropdown sub-lines while keeping tooltip info.
    cfg.show_reset_time = false;
    let ui_hidden = UiSnapshot::build(&cfg, &claude, &codex, false, now);
    assert!(ui_hidden.claude_row.reset_value_text.is_none());
    assert!(ui_hidden.codex_row.reset_value_text.is_none());
    assert!(ui_hidden.claude_row.tooltip.contains("5h resets in 2h 15m"));

    // Snapshot with no `resets_at` returns None.
    let no_reset = snap(Provider::Claude, Some((80, None)), Some((90, None)), now);
    assert_eq!(format_reset_values(Some(&no_reset), now), None);

    // Partial `resets_at` uses en-dash for missing window.
    let partial = snap(
        Provider::Claude,
        Some((80, Some(now + 30 * 60))),
        Some((90, None)),
        now,
    );
    assert_eq!(
        format_reset_values(Some(&partial), now).as_deref(),
        Some("30m / –")
    );
}

#[test]
fn alert_tracker_deduplicates_escalates_resets_and_respects_baseline() {
    let cfg = AppConfig::default(); // warning <= 20, critical <= 5
    let mut tracker = AlertTracker::new();
    let t0 = 10_000;
    let reset_cycle_1 = Some(t0 + 7200);
    let reset_cycle_2 = Some(t0 + 25_200);

    // 1. Baseline seeding suppresses startup replay for already-low Claude 5h (15%).
    let cached_claude = snap(
        Provider::Claude,
        Some((15, reset_cycle_1)),
        Some((80, Some(t0 + 86_400))),
        t0,
    );
    tracker.seed_baseline(Some(&cached_claude), None, &cfg, t0);
    assert!(tracker.evaluate(&cached_claude, &cfg, t0 + 10).is_empty());

    // 2. Dropping from 15% -> 12% in same cycle still does NOT re-fire Warning.
    let claude_12 = snap(
        Provider::Claude,
        Some((12, reset_cycle_1)),
        Some((80, Some(t0 + 86_400))),
        t0 + 60,
    );
    assert!(tracker.evaluate(&claude_12, &cfg, t0 + 60).is_empty());

    // 3. Escalating from Warning -> Critical (<= 5%) fires Critical ONCE.
    let claude_5 = snap(
        Provider::Claude,
        Some((5, reset_cycle_1)),
        Some((80, Some(t0 + 86_400))),
        t0 + 120,
    );
    let escalated = tracker.evaluate(&claude_5, &cfg, t0 + 120);
    assert_eq!(
        escalated,
        vec![QuotaAlert {
            provider: Provider::Claude,
            window: WindowKind::FiveHour,
            level: AlertLevel::Critical,
            remaining_percent: 5,
            resets_at: reset_cycle_1,
        }]
    );
    assert_eq!(escalated[0].notification_id(), "ebb.claude.5h.critical");

    // 4. Further drop to 2% in same cycle does NOT re-fire Critical.
    let claude_2 = snap(
        Provider::Claude,
        Some((2, reset_cycle_1)),
        Some((80, Some(t0 + 86_400))),
        t0 + 180,
    );
    assert!(tracker.evaluate(&claude_2, &cfg, t0 + 180).is_empty());

    // 5. New reset cycle (`reset_cycle_2`) resets deduplication and fires Warning again.
    let claude_new_cycle = snap(
        Provider::Claude,
        Some((18, reset_cycle_2)),
        Some((80, Some(t0 + 86_400))),
        t0 + 8000,
    );
    let next_cycle_alerts = tracker.evaluate(&claude_new_cycle, &cfg, t0 + 8000);
    assert_eq!(next_cycle_alerts.len(), 1);
    assert_eq!(next_cycle_alerts[0].level, AlertLevel::Warning);
}

#[test]
fn alert_tracker_hysteresis_and_config_guards() {
    let mut cfg = AppConfig::default(); // warning <= 20, hysteresis +5 => recovery at >= 25
    let mut tracker = AlertTracker::new();
    let now = 20_000;

    // Fires Warning without `resets_at`.
    let low = snap(Provider::Codex, Some((19, None)), None, now);
    assert_eq!(tracker.evaluate(&low, &cfg, now).len(), 1);

    // Hovering at 22% (below 25% recovery cutoff) then back to 19% MUST NOT flap.
    let hover = snap(Provider::Codex, Some((22, None)), None, now + 60);
    assert!(tracker.evaluate(&hover, &cfg, now + 60).is_empty());
    assert!(tracker.evaluate(&low, &cfg, now + 120).is_empty());

    // Recovering to >= 25% clears state so next drop to <= 20% alerts again.
    let recovered = snap(Provider::Codex, Some((25, None)), None, now + 180);
    assert!(tracker.evaluate(&recovered, &cfg, now + 180).is_empty());
    assert_eq!(tracker.evaluate(&low, &cfg, now + 240).len(), 1);

    // Disabled alerts or hidden provider emit nothing.
    cfg.alerts_enabled = false;
    let crit = snap(Provider::Codex, Some((3, None)), None, now + 300);
    assert!(tracker.evaluate(&crit, &cfg, now + 300).is_empty());

    cfg.alerts_enabled = true;
    cfg.show_codex = false;
    assert!(tracker.evaluate(&crit, &cfg, now + 360).is_empty());

    // Copy formatter produces clean title & body.
    let (title, body) = format_alert_copy(
        &QuotaAlert {
            provider: Provider::Claude,
            window: WindowKind::FiveHour,
            level: AlertLevel::Warning,
            remaining_percent: 15,
            resets_at: Some(now + 2 * 3600 + 15 * 60),
        },
        now,
    );
    assert_eq!(title, "Ebb — Claude 5h quota low");
    assert_eq!(body, "15% remaining · resets in 2h 15m");

    let (title_c, body_c) = format_alert_copy(
        &QuotaAlert {
            provider: Provider::Codex,
            window: WindowKind::Weekly,
            level: AlertLevel::Critical,
            remaining_percent: 4,
            resets_at: None,
        },
        now,
    );
    assert_eq!(title_c, "Ebb — Codex weekly quota critical");
    assert_eq!(body_c, "4% remaining");
}
