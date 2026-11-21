use std::collections::BTreeMap;
use std::process::Command;

use ebb::config::{AppConfig, ColorTheme, HexColorOverride};
use ebb::error::AppError;
use ebb::model::{
    DisplayMetric, MenuBarWindows, Provider, ProviderState, QuotaSnapshot, QuotaWindow,
};
use ebb::ui::view_model::{
    RgbColor, UiSnapshot, format_interval_label, format_relative_age, provider_colors,
    provider_colors_for_theme, resolve_provider_colors,
};

#[test]
fn provider_palette_matches_spec_dark_and_accessible_light() {
    let claude = provider_colors(Provider::Claude);
    assert_eq!(claude.dark, RgbColor::new(0xE8, 0x9A, 0x70));
    assert_eq!(claude.light, RgbColor::new(0xB8, 0x4E, 0x1E));

    let codex = provider_colors(Provider::Codex);
    assert_eq!(codex.dark, RgbColor::new(0x74, 0xC8, 0x9A));
    assert_eq!(codex.light, RgbColor::new(0x18, 0x79, 0x4E));

    let agy = provider_colors(Provider::Agy);
    assert_eq!(agy.dark, RgbColor::new(0x7E, 0xB6, 0xFF));
    assert_eq!(agy.light, RgbColor::new(0x1D, 0x5F, 0xD6));
}

#[test]
fn ui_snapshot_formats_menu_bar_and_dropdown_per_mvp_spec() {
    let cfg = AppConfig::default();
    let mut claude = ProviderState::new(Provider::Claude);
    claude.apply_success(
        QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 65,
                resets_at: None,
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 72,
                resets_at: None,
            }),
            fetched_at: 10_000,
        },
        10_000,
    );
    let mut codex = ProviderState::new(Provider::Codex);
    codex.apply_success(
        QuotaSnapshot {
            provider: Provider::Codex,
            five_hour: Some(QuotaWindow {
                remaining_percent: 43,
                resets_at: None,
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 31,
                resets_at: None,
            }),
            fetched_at: 10_000,
        },
        10_000,
    );
    let agy = ProviderState::new(Provider::Agy);

    let ui = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, 10_005);
    assert!(!ui.menu_bar_fallback_only);
    assert_eq!(ui.menu_bar_plain_text(), "65/72 43/31");
    assert_eq!(ui.row_for(Provider::Claude).value_text, "65% / 72%");
    assert_eq!(ui.row_for(Provider::Codex).value_text, "43% / 31%");
    assert!(!ui.row_for(Provider::Claude).stale);
    assert!(!ui.row_for(Provider::Codex).stale);
    assert!(!ui.row_for(Provider::Agy).visible);
    assert_eq!(ui.status_line.as_deref(), Some("Updated just now"));

    // No `5h/weekly` or `remaining` labels leaked into dropdown row text.
    assert!(!ui.row_for(Provider::Claude).value_text.contains("5h"));
    assert!(
        !ui.row_for(Provider::Claude)
            .value_text
            .contains("remaining")
    );
    assert!(!ui.row_for(Provider::Codex).value_text.contains("weekly"));

    // Equality check supports update-on-change diffing.
    let ui_same = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, 10_005);
    assert_eq!(ui, ui_same);
}

#[test]
fn ui_snapshot_handles_stale_unauth_hidden_providers_and_refreshing() {
    let mut cfg = AppConfig::default();
    let mut claude = ProviderState::new(Provider::Claude);
    claude.apply_success(
        QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 85,
                resets_at: None,
            }),
            weekly: None,
            fetched_at: 1_000,
        },
        1_000,
    );
    let mut codex = ProviderState::new(Provider::Codex);
    codex.apply_failure(
        &AppError::Unauthenticated {
            provider: Provider::Codex,
        },
        2_000,
    );
    let agy = ProviderState::new(Provider::Agy);

    // At now = 2_000 (1000s > default 900s claude_stale_after), Claude becomes stale.
    let ui = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, 2_000);
    assert_eq!(ui.menu_bar_plain_text(), "85/– –/–");
    assert!(ui.row_for(Provider::Claude).stale);
    assert_eq!(ui.row_for(Provider::Claude).value_text, "85% / –");
    assert_eq!(ui.row_for(Provider::Codex).value_text, "– / –");
    assert!(
        ui.row_for(Provider::Codex)
            .tooltip
            .contains("Not authenticated")
    );
    assert_eq!(
        ui.status_line.as_deref(),
        Some("Updated 16m ago · Claude stale, Codex login needed")
    );

    // Refreshing status overrides footer cleanly.
    let ui_refreshing = UiSnapshot::build(&cfg, &claude, &codex, &agy, true, 2_000);
    assert_eq!(ui_refreshing.status_line.as_deref(), Some("Refreshing…"));

    // Hiding all providers activates compact fallback `–` so Menu Bar stays clickable.
    cfg.show_claude = false;
    cfg.show_codex = false;
    cfg.show_agy = false;
    let ui_hidden = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, 2_000);
    assert!(ui_hidden.menu_bar_fallback_only);
    assert_eq!(ui_hidden.menu_bar_plain_text(), "–");
    assert!(!ui_hidden.row_for(Provider::Claude).visible);
    assert!(!ui_hidden.row_for(Provider::Codex).visible);
    assert!(!ui_hidden.row_for(Provider::Agy).visible);
}

#[test]
fn ui_snapshot_supports_agy_custom_order_display_modes_critical_and_themes() {
    let mut cfg = AppConfig {
        show_agy: true,
        display_metric: DisplayMetric::Used,
        menu_bar_windows: MenuBarWindows::Both,
        highlight_critical: true,
        color_theme: ColorTheme::HighContrast,
        ..AppConfig::default()
    };
    cfg.move_provider_first(Provider::Agy);

    let mut claude = ProviderState::new(Provider::Claude);
    claude.apply_success(
        QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 4,
                resets_at: None,
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 70,
                resets_at: None,
            }),
            fetched_at: 10_000,
        },
        10_000,
    );
    let mut codex = ProviderState::new(Provider::Codex);
    codex.apply_success(
        QuotaSnapshot {
            provider: Provider::Codex,
            five_hour: Some(QuotaWindow {
                remaining_percent: 50,
                resets_at: None,
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 80,
                resets_at: None,
            }),
            fetched_at: 10_000,
        },
        10_000,
    );
    let mut agy = ProviderState::new(Provider::Agy);
    agy.apply_success(
        QuotaSnapshot {
            provider: Provider::Agy,
            five_hour: None,
            weekly: Some(QuotaWindow {
                remaining_percent: 94,
                resets_at: None,
            }),
            fetched_at: 10_000,
        },
        10_000,
    );

    let ui = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, 10_002);
    assert_eq!(
        ui.provider_rows
            .iter()
            .map(|r| r.provider)
            .collect::<Vec<_>>(),
        vec![Provider::Agy, Provider::Claude, Provider::Codex]
    );
    // Used % in order [Agy, Claude, Codex]
    assert_eq!(ui.menu_bar_plain_text(), "–/6 96/30 50/20");
    assert!(ui.menu_bar_segments[1].five_hour_critical);
    assert!(!ui.menu_bar_segments[1].weekly_critical);
    assert_eq!(ui.row_for(Provider::Agy).value_text, "– / 6%");
    assert!(ui.menu_bar_tooltip.contains("5h / weekly used"));

    // Compact 5h-only mode
    cfg.menu_bar_windows = MenuBarWindows::FiveHourOnly;
    cfg.display_metric = DisplayMetric::Remaining;
    let compact = UiSnapshot::build(&cfg, &claude, &codex, &agy, false, 10_002);
    assert_eq!(compact.menu_bar_plain_text(), "– 4 50");

    // Color themes & hex overrides
    let mono_a = provider_colors_for_theme(Provider::Claude, ColorTheme::Monochrome);
    let mono_b = provider_colors_for_theme(Provider::Agy, ColorTheme::Monochrome);
    assert_eq!(mono_a, mono_b);

    let mut overrides = BTreeMap::new();
    overrides.insert(
        "agy".to_owned(),
        HexColorOverride {
            dark: Some("#112233".to_owned()),
            light: None,
        },
    );
    let resolved = resolve_provider_colors(Provider::Agy, ColorTheme::HighContrast, &overrides);
    assert_eq!(resolved.dark, RgbColor::new(0x11, 0x22, 0x33));
    assert_eq!(
        resolved.light,
        provider_colors_for_theme(Provider::Agy, ColorTheme::HighContrast).light
    );
}

#[test]
fn helper_formatters_for_relative_age_and_intervals() {
    assert_eq!(format_relative_age(100, 95), "just now");
    assert_eq!(format_relative_age(100, 60), "40s ago");
    assert_eq!(format_relative_age(1000, 700), "5m ago");
    assert_eq!(format_relative_age(10_000, 2_000), "2h ago");
    assert_eq!(format_relative_age(50, 100), "just now"); // clock skew safe

    assert_eq!(format_interval_label(30), "30s");
    assert_eq!(format_interval_label(60), "1m");
    assert_eq!(format_interval_label(120), "2m");
    assert_eq!(format_interval_label(300), "5m");
}

#[cfg(target_os = "macos")]
#[test]
fn real_appkit_menu_bar_and_dropdown_smoke_test_passes() {
    if std::env::var_os("CI").is_some() {
        eprintln!("skipping WindowServer AppKit smoke test in headless CI environment");
        return;
    }

    let bin = env!("CARGO_BIN_EXE_ebb");
    let out = Command::new(bin)
        .arg("--ui-smoke-test")
        .output()
        .expect("run --ui-smoke-test");

    assert!(
        out.status.success(),
        "ui smoke test failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("UI smoke test OK"), "stdout={stdout}");
    assert!(stdout.contains("Updated just now"), "stdout={stdout}");
    assert!(stdout.contains("● Claude\\t65% / 72%"), "stdout={stdout}");
    assert!(
        stdout.contains("Reset in\\t2h 15m / 4d 6h"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("● Codex\\t43% / 31%"), "stdout={stdout}");
}
