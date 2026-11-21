use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ebb::alerts::{AlertKind, AlertLevel, AlertTracker, QuotaAlert, format_alert_copy};
use ebb::config::{
    AppConfig, DEFAULT_ALERT_CRITICAL_THRESHOLD, DEFAULT_ALERT_WARNING_THRESHOLD,
    DEFAULT_ALERTS_ENABLED, DEFAULT_NOTIFY_ON_REFRESH, DEFAULT_SHOW_RESET_TIME,
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
fn config_26_2_is_backward_compatible_with_26_0_and_26_1_and_validates_thresholds() {
    let dir = temp_dir("compat");
    let cfg_path = dir.join("config.json");

    // Simulate a 26.0/26.1 config.json without `notify_on_refresh` or 26.1 keys.
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
    assert_eq!(cfg.notify_on_refresh, DEFAULT_NOTIFY_ON_REFRESH);
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

    let agy = ProviderState::new(Provider::Agy);
    let ui = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, now);
    // Menu bar stays strictly minimal numbers only.
    assert_eq!(ui.menu_bar_plain_text(), "65/72 43/31");
    assert_eq!(
        ui.row_for(Provider::Claude).reset_value_text.as_deref(),
        Some("2h 15m / 4d 6h")
    );
    assert_eq!(
        ui.row_for(Provider::Codex).reset_value_text.as_deref(),
        Some("resetting… / 5d 11h")
    );
    assert!(
        ui.row_for(Provider::Claude)
            .tooltip
            .contains("5h resets in 2h 15m")
    );
    assert!(
        ui.row_for(Provider::Claude)
            .tooltip
            .contains("weekly resets in 4d 6h")
    );
    assert!(
        ui.row_for(Provider::Codex)
            .tooltip
            .contains("5h resetting…")
    );

    // Turning off `show_reset_time` hides dropdown sub-lines while keeping tooltip info.
    cfg.show_reset_time = false;
    let ui_hidden = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, now);
    assert!(
        ui_hidden
            .row_for(Provider::Claude)
            .reset_value_text
            .is_none()
    );
    assert!(
        ui_hidden
            .row_for(Provider::Codex)
            .reset_value_text
            .is_none()
    );
    assert!(
        ui_hidden
            .row_for(Provider::Claude)
            .tooltip
            .contains("5h resets in 2h 15m")
    );

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
    tracker.seed_baseline(&[Some(&cached_claude), None], &cfg, t0);
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
            kind: AlertKind::Low(AlertLevel::Critical),
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

    // 5. New reset cycle (`reset_cycle_2`) that is ALREADY low fires Warning (not fake Refreshed).
    let claude_new_cycle = snap(
        Provider::Claude,
        Some((18, reset_cycle_2)),
        Some((80, Some(t0 + 86_400))),
        t0 + 8000,
    );
    let next_cycle_alerts = tracker.evaluate(&claude_new_cycle, &cfg, t0 + 8000);
    assert_eq!(next_cycle_alerts.len(), 1);
    assert_eq!(
        next_cycle_alerts[0].kind,
        AlertKind::Low(AlertLevel::Warning)
    );
}

#[test]
fn alert_tracker_emits_single_refreshed_notification_on_recovery() {
    let mut cfg = AppConfig::default();
    let mut tracker = AlertTracker::new();
    let t0 = 50_000;
    let cycle1 = Some(t0 + 3600);
    let cycle2 = Some(t0 + 21_600);

    // Normal high usage NEVER emits a Refreshed notification.
    let healthy1 = snap(Provider::Claude, Some((95, cycle1)), None, t0);
    let healthy2 = snap(Provider::Claude, Some((100, cycle2)), None, t0 + 4000);
    assert!(tracker.evaluate(&healthy1, &cfg, t0).is_empty());
    assert!(tracker.evaluate(&healthy2, &cfg, t0 + 4000).is_empty());

    // Drop into Critical -> fires Low(Critical).
    let critical = snap(Provider::Claude, Some((4, cycle2)), None, t0 + 5000);
    let a1 = tracker.evaluate(&critical, &cfg, t0 + 5000);
    assert_eq!(a1.len(), 1);
    assert_eq!(a1[0].kind, AlertKind::Low(AlertLevel::Critical));

    // Next reset cycle recovers to 100% -> emits Refreshed ONCE.
    let cycle3 = Some(t0 + 39_600);
    let refreshed_snap = snap(Provider::Claude, Some((100, cycle3)), None, t0 + 22_000);
    let a2 = tracker.evaluate(&refreshed_snap, &cfg, t0 + 22_000);
    assert_eq!(
        a2,
        vec![QuotaAlert {
            provider: Provider::Claude,
            window: WindowKind::FiveHour,
            kind: AlertKind::Refreshed,
            remaining_percent: 100,
            resets_at: cycle3,
        }]
    );
    assert_eq!(a2[0].notification_id(), "ebb.claude.5h.refreshed");
    let (title, body) = format_alert_copy(&a2[0], t0 + 22_000);
    assert_eq!(title, "Ebb — Claude 5h quota refreshed");
    assert_eq!(body, "100% remaining · resets in 4h 53m");

    // Repeated polls at 100% do NOT spam Refreshed again.
    assert!(
        tracker
            .evaluate(&refreshed_snap, &cfg, t0 + 22_060)
            .is_empty()
    );

    // Respects `notify_on_refresh = false` (still clears state, emits no Refreshed alert).
    cfg.notify_on_refresh = false;
    let low_again = snap(Provider::Claude, Some((15, cycle3)), None, t0 + 23_000);
    assert_eq!(tracker.evaluate(&low_again, &cfg, t0 + 23_000).len(), 1);
    let cycle4 = Some(t0 + 57_600);
    let full_again = snap(Provider::Claude, Some((100, cycle4)), None, t0 + 40_000);
    assert!(tracker.evaluate(&full_again, &cfg, t0 + 40_000).is_empty());
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

    // Recovering to 25% clears hysteresis without firing Refreshed (< 80% when no resets_at change).
    let recovered = snap(Provider::Codex, Some((25, None)), None, now + 180);
    assert!(tracker.evaluate(&recovered, &cfg, now + 180).is_empty());
    assert_eq!(tracker.evaluate(&low, &cfg, now + 240).len(), 1);

    // Strong recovery to 95% without `resets_at` DOES emit Refreshed once.
    let full = snap(Provider::Codex, Some((95, None)), None, now + 260);
    let ref_alerts = tracker.evaluate(&full, &cfg, now + 260);
    assert_eq!(ref_alerts.len(), 1);
    assert_eq!(ref_alerts[0].kind, AlertKind::Refreshed);

    // Disabled alerts or hidden provider emit nothing.
    cfg.alerts_enabled = false;
    let crit = snap(Provider::Codex, Some((3, None)), None, now + 300);
    assert!(tracker.evaluate(&crit, &cfg, now + 300).is_empty());

    cfg.alerts_enabled = true;
    cfg.show_codex = false;
    assert!(tracker.evaluate(&crit, &cfg, now + 360).is_empty());

    // Agy alerts respect `show_agy` and produce deterministic `ebb.agy.*` notification IDs.
    let agy_low = snap(
        Provider::Agy,
        None,
        Some((15, Some(now + 86_400))),
        now + 400,
    );
    assert!(tracker.evaluate(&agy_low, &cfg, now + 400).is_empty());
    cfg.show_agy = true;
    let agy_alerts = tracker.evaluate(&agy_low, &cfg, now + 420);
    assert_eq!(agy_alerts.len(), 1);
    assert_eq!(agy_alerts[0].notification_id(), "ebb.agy.weekly.warning");
    let agy_reset = snap(
        Provider::Agy,
        None,
        Some((100, Some(now + 7 * 86_400))),
        now + 90_000,
    );
    let agy_refreshed = tracker.evaluate(&agy_reset, &cfg, now + 90_000);
    assert_eq!(agy_refreshed.len(), 1);
    assert_eq!(
        agy_refreshed[0].notification_id(),
        "ebb.agy.weekly.refreshed"
    );

    // Copy formatter produces clean title & body.
    let (title, body) = format_alert_copy(
        &QuotaAlert {
            provider: Provider::Claude,
            window: WindowKind::FiveHour,
            kind: AlertKind::Low(AlertLevel::Warning),
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
            kind: AlertKind::Low(AlertLevel::Critical),
            remaining_percent: 4,
            resets_at: None,
        },
        now,
    );
    assert_eq!(title_c, "Ebb — Codex weekly quota critical");
    assert_eq!(body_c, "4% remaining");
}
