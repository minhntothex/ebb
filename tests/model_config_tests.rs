use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use ebb::config::{
    AppConfig, AppPaths, ColorTheme, DEFAULT_AGY_STALE_AFTER_SECS, DEFAULT_CLAUDE_STALE_AFTER_SECS,
    DEFAULT_CODEX_STALE_AFTER_SECS, DEFAULT_HIGHLIGHT_CRITICAL, DEFAULT_REFRESH_INTERVAL_SECS,
    DEFAULT_SHOW_AGY, DEFAULT_SNAPSHOT_TTL_SECS, HexColorOverride, normalize_provider_order,
    parse_hex_rgb,
};
use ebb::error::AppError;
use ebb::model::{
    DisplayMetric, MISSING_VALUE, MenuBarSegment, MenuBarWindows, Provider, ProviderHealth,
    ProviderState, QuotaSnapshot, QuotaWindow, UsageCache, WindowKind, build_menu_bar_segments,
    build_menu_bar_segments_ordered, format_dropdown_snapshot, format_menu_bar_snapshot,
};

static TEST_DIR_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ai-usage-monitor-test-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

#[test]
fn used_percentage_conversion_and_rounding() {
    let w0 =
        QuotaWindow::from_used_percent(Provider::Claude, WindowKind::FiveHour, 0.0, None).unwrap();
    assert_eq!(w0.remaining_percent, 100);

    let w100 = QuotaWindow::from_used_percent(Provider::Claude, WindowKind::FiveHour, 100.0, None)
        .unwrap();
    assert_eq!(w100.remaining_percent, 0);

    // Real Step 1 spike numbers: Claude 15% / 3% used -> 85% / 97% remaining
    let claude_5h = QuotaWindow::from_used_percent(
        Provider::Claude,
        WindowKind::FiveHour,
        15.0,
        Some(1_795_239_600),
    )
    .unwrap();
    let claude_7d = QuotaWindow::from_used_percent(
        Provider::Claude,
        WindowKind::Weekly,
        3.0,
        Some(1_795_788_000),
    )
    .unwrap();
    assert_eq!(claude_5h.remaining_percent, 85);
    assert_eq!(claude_7d.remaining_percent, 97);

    // Real Step 1 spike numbers: Codex 3% / 16% used -> 97% / 84% remaining
    let codex_5h =
        QuotaWindow::from_used_percent(Provider::Codex, WindowKind::FiveHour, 3.0, None).unwrap();
    let codex_7d =
        QuotaWindow::from_used_percent(Provider::Codex, WindowKind::Weekly, 16.0, None).unwrap();
    assert_eq!(codex_5h.remaining_percent, 97);
    assert_eq!(codex_7d.remaining_percent, 84);

    // Fractional rounding (100 - 23.5 = 76.5 -> 77; 100 - 23.6 = 76.4 -> 76)
    assert_eq!(
        QuotaWindow::from_used_percent(Provider::Claude, WindowKind::FiveHour, 23.5, None)
            .unwrap()
            .remaining_percent,
        77
    );
    assert_eq!(
        QuotaWindow::from_used_percent(Provider::Claude, WindowKind::FiveHour, 23.6, None)
            .unwrap()
            .remaining_percent,
        76
    );
    // Negative zero is valid 0.0
    assert_eq!(
        QuotaWindow::from_used_percent(Provider::Claude, WindowKind::FiveHour, -0.0, None)
            .unwrap()
            .remaining_percent,
        100
    );
}

#[test]
fn remaining_percentage_does_not_subtract_twice() {
    let w = QuotaWindow::from_remaining_percent(Provider::Codex, WindowKind::FiveHour, 65.0, None)
        .unwrap();
    assert_eq!(w.remaining_percent, 65);
}

#[test]
fn invalid_percentages_are_rejected() {
    for bad in [
        -0.01,
        -50.0,
        100.01,
        150.0,
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
    ] {
        let err_used =
            QuotaWindow::from_used_percent(Provider::Claude, WindowKind::FiveHour, bad, None)
                .unwrap_err();
        assert!(matches!(err_used, AppError::InvalidPercentage { .. }));

        let err_rem =
            QuotaWindow::from_remaining_percent(Provider::Codex, WindowKind::Weekly, bad, None)
                .unwrap_err();
        assert!(matches!(err_rem, AppError::InvalidPercentage { .. }));
    }
}

#[test]
fn window_kind_maps_strictly_by_duration_minutes() {
    assert_eq!(
        WindowKind::from_duration_mins(300),
        Some(WindowKind::FiveHour)
    );
    assert_eq!(
        WindowKind::from_duration_mins(10_080),
        Some(WindowKind::Weekly)
    );
    assert_eq!(WindowKind::from_duration_mins(60), None);
    assert_eq!(WindowKind::from_duration_mins(1440), None);
    assert_eq!(WindowKind::from_duration_mins(0), None);
    assert_eq!(WindowKind::from_duration_mins(-300), None);
}

#[test]
fn formatting_matches_mvp_spec_table() {
    let w65 = Some(QuotaWindow {
        remaining_percent: 65,
        resets_at: None,
    });
    let w72 = Some(QuotaWindow {
        remaining_percent: 72,
        resets_at: None,
    });

    let both = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: w65,
        weekly: w72,
        fetched_at: 1_000,
    };
    assert_eq!(both.format_menu_bar(), "65/72");
    assert_eq!(both.format_dropdown(), "65% / 72%");

    let missing_weekly = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: w65,
        weekly: None,
        fetched_at: 1_000,
    };
    assert_eq!(
        missing_weekly.format_menu_bar(),
        format!("65/{MISSING_VALUE}")
    );
    assert_eq!(missing_weekly.format_menu_bar(), "65/–");
    assert_eq!(missing_weekly.format_dropdown(), "65% / –");

    let missing_5h = QuotaSnapshot {
        provider: Provider::Codex,
        five_hour: None,
        weekly: w72,
        fetched_at: 1_000,
    };
    assert_eq!(missing_5h.format_menu_bar(), "–/72");
    assert_eq!(missing_5h.format_dropdown(), "– / 72%");

    let missing_both = QuotaSnapshot {
        provider: Provider::Codex,
        five_hour: None,
        weekly: None,
        fetched_at: 1_000,
    };
    assert_eq!(missing_both.format_menu_bar(), "–/–");
    assert_eq!(missing_both.format_dropdown(), "– / –");

    assert_eq!(format_menu_bar_snapshot(None), "–/–");
    assert_eq!(format_dropdown_snapshot(None), "– / –");
}

#[test]
fn menu_bar_segments_follow_fixed_order_and_visibility_flags() {
    let claude = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: Some(QuotaWindow {
            remaining_percent: 85,
            resets_at: None,
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 97,
            resets_at: None,
        }),
        fetched_at: 1_000,
    };
    let codex = QuotaSnapshot {
        provider: Provider::Codex,
        five_hour: Some(QuotaWindow {
            remaining_percent: 97,
            resets_at: None,
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 84,
            resets_at: None,
        }),
        fetched_at: 1_000,
    };

    let both = build_menu_bar_segments(Some((&claude, false)), Some((&codex, true)), true, true);
    assert_eq!(
        both,
        vec![
            MenuBarSegment {
                provider: Provider::Claude,
                five_hour_text: "85".into(),
                weekly_text: Some("97".into()),
                five_hour_critical: false,
                weekly_critical: false,
                stale: false,
            },
            MenuBarSegment {
                provider: Provider::Codex,
                five_hour_text: "97".into(),
                weekly_text: Some("84".into()),
                five_hour_critical: false,
                weekly_critical: false,
                stale: true,
            },
        ]
    );
    assert_eq!(both[0].plain_text(), "85/97");
    assert_eq!(both[1].plain_text(), "97/84");

    let only_codex =
        build_menu_bar_segments(Some((&claude, false)), Some((&codex, false)), false, true);
    assert_eq!(only_codex.len(), 1);
    assert_eq!(only_codex[0].provider, Provider::Codex);

    let missing_claude = build_menu_bar_segments(None, Some((&codex, false)), true, true);
    assert_eq!(missing_claude[0].plain_text(), "–/–");
    assert_eq!(missing_claude[1].plain_text(), "97/84");
}

#[test]
fn display_modes_critical_flags_provider_order_and_hex_color_validation() {
    let claude = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: Some(QuotaWindow {
            remaining_percent: 4,
            resets_at: None,
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 72,
            resets_at: None,
        }),
        fetched_at: 1_000,
    };
    let agy = QuotaSnapshot {
        provider: Provider::Agy,
        five_hour: None,
        weekly: Some(QuotaWindow {
            remaining_percent: 94,
            resets_at: None,
        }),
        fetched_at: 1_000,
    };

    // Used metric + Both windows + per-window critical threshold evaluated on remaining %.
    assert_eq!(
        claude.format_menu_bar_with(DisplayMetric::Used, MenuBarWindows::Both),
        "96/28"
    );
    assert_eq!(
        claude.format_dropdown_with(DisplayMetric::Used),
        "96% / 28%"
    );
    assert_eq!(
        agy.format_menu_bar_with(DisplayMetric::Used, MenuBarWindows::Both),
        "–/6"
    );
    assert_eq!(
        claude.format_menu_bar_with(DisplayMetric::Remaining, MenuBarWindows::FiveHourOnly),
        "4"
    );

    let ordered = build_menu_bar_segments_ordered(
        &[Provider::Agy, Provider::Claude],
        |p| match p {
            Provider::Agy => Some((Some(&agy), false)),
            Provider::Claude => Some((Some(&claude), false)),
            Provider::Codex => None,
        },
        DisplayMetric::Used,
        MenuBarWindows::Both,
        Some(5),
    );
    assert_eq!(ordered.len(), 2);
    assert_eq!(ordered[0].provider, Provider::Agy);
    assert_eq!(ordered[0].plain_text(), "–/6");
    assert!(!ordered[0].five_hour_critical);
    assert!(!ordered[0].weekly_critical);
    assert_eq!(ordered[1].provider, Provider::Claude);
    assert_eq!(ordered[1].plain_text(), "96/28");
    assert!(ordered[1].five_hour_critical);
    assert!(!ordered[1].weekly_critical);

    // Compact 5h-only mode suppresses weekly text & weekly critical flag.
    let compact = build_menu_bar_segments_ordered(
        &[Provider::Claude],
        |_| Some((Some(&claude), false)),
        DisplayMetric::Remaining,
        MenuBarWindows::FiveHourOnly,
        Some(5),
    );
    assert_eq!(compact[0].plain_text(), "4");
    assert_eq!(compact[0].weekly_text, None);
    assert!(compact[0].five_hour_critical);
    assert!(!compact[0].weekly_critical);

    // Provider order normalization & move_provider_first.
    assert_eq!(
        normalize_provider_order(&[Provider::Codex, Provider::Codex]),
        vec![Provider::Codex, Provider::Claude, Provider::Agy]
    );
    let mut cfg = AppConfig::default();
    assert_eq!(cfg.show_agy, DEFAULT_SHOW_AGY);
    assert_eq!(cfg.agy_stale_after_seconds, DEFAULT_AGY_STALE_AFTER_SECS);
    assert_eq!(cfg.highlight_critical, DEFAULT_HIGHLIGHT_CRITICAL);
    assert_eq!(cfg.color_theme, ColorTheme::Default);
    cfg.move_provider_first(Provider::Agy);
    assert_eq!(
        cfg.provider_order,
        vec![Provider::Agy, Provider::Claude, Provider::Codex]
    );
    cfg.move_provider_first(Provider::Codex);
    assert_eq!(
        cfg.provider_order,
        vec![Provider::Codex, Provider::Agy, Provider::Claude]
    );

    // Hex RGB parser & config validation.
    assert_eq!(parse_hex_rgb("#7EB6FF"), Some((0x7E, 0xB6, 0xFF)));
    assert_eq!(parse_hex_rgb("#00ff0a"), Some((0x00, 0xFF, 0x0A)));
    assert_eq!(parse_hex_rgb("7EB6FF"), None);
    assert_eq!(parse_hex_rgb("#12345"), None);
    assert_eq!(parse_hex_rgb("#GGGGGG"), None);

    let mut valid_colors = BTreeMap::new();
    valid_colors.insert(
        "agy".to_owned(),
        HexColorOverride {
            dark: Some("#88CCFF".to_owned()),
            light: Some("#1144AA".to_owned()),
        },
    );
    cfg.provider_colors = valid_colors;
    assert!(cfg.validate().is_ok());

    let mut bad_key = AppConfig::default();
    bad_key.provider_colors.insert(
        "unknown".to_owned(),
        HexColorOverride {
            dark: Some("#FFFFFF".to_owned()),
            light: None,
        },
    );
    assert!(matches!(
        bad_key.validate(),
        Err(AppError::InvalidConfig(_))
    ));

    let mut bad_hex = AppConfig::default();
    bad_hex.provider_colors.insert(
        "claude".to_owned(),
        HexColorOverride {
            dark: Some("#12345".to_owned()),
            light: None,
        },
    );
    assert!(matches!(
        bad_hex.validate(),
        Err(AppError::InvalidConfig(_))
    ));

    // UsageCache roundtrip with Agy & backward-compatible deserialization without `agy`.
    let old_cache: UsageCache = serde_json::from_str(r#"{"version":1}"#).unwrap();
    assert_eq!(old_cache.agy, None);
    let mut cache = UsageCache::empty();
    cache.set_snapshot(agy.clone());
    assert_eq!(cache.snapshot_for(Provider::Agy), Some(&agy));
}

#[test]
fn provider_state_preserves_last_known_good_and_never_fakes_reset() {
    let mut state = ProviderState::new(Provider::Codex);
    assert_eq!(state.health, ProviderHealth::Unavailable);
    assert!(state.visible_snapshot(1_000, 3_600).is_none());

    let snap = QuotaSnapshot {
        provider: Provider::Codex,
        five_hour: Some(QuotaWindow {
            remaining_percent: 43,
            resets_at: Some(1_100), // already passed at now = 1_500
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 31,
            resets_at: Some(2_000),
        }),
        fetched_at: 1_000,
    };

    state.apply_success(snap.clone(), 1_000);
    assert_eq!(state.effective_health(1_050, 180), ProviderHealth::Fresh);
    assert_eq!(state.effective_health(1_200, 180), ProviderHealth::Stale);

    // Passing `resets_at` MUST NOT artificially flip remaining_percent to 100.
    let after_reset = state.visible_snapshot(1_500, 3_600).unwrap();
    assert_eq!(after_reset.five_hour.unwrap().remaining_percent, 43);
    assert_eq!(after_reset.weekly.unwrap().remaining_percent, 31);

    // Transient error preserves last known good value and marks Stale.
    let timeout = AppError::Timeout {
        provider: Provider::Codex,
        timeout_secs: 5,
    };
    state.apply_failure(&timeout, 1_500);
    assert_eq!(state.health, ProviderHealth::Stale);
    assert!(state.last_error.is_some());
    assert_eq!(state.visible_snapshot(1_500, 3_600), Some(&snap));

    // Past TTL hides ancient cache as None -> displays `–/–`.
    assert!(state.visible_snapshot(1_000 + 3_600, 3_600).is_none());

    // Unauthenticated keeps distinct health state.
    let unauth = AppError::Unauthenticated {
        provider: Provider::Codex,
    };
    state.apply_failure(&unauth, 1_600);
    assert_eq!(state.health, ProviderHealth::Unauthenticated);
}

#[test]
fn cache_seeding_clock_skew_and_empty_windows_handling() {
    let mut state = ProviderState::new(Provider::Claude);

    // Mismatched provider snapshot is ignored.
    state.seed_from_cache(QuotaSnapshot {
        provider: Provider::Codex,
        five_hour: Some(QuotaWindow {
            remaining_percent: 50,
            resets_at: None,
        }),
        weekly: None,
        fetched_at: 2_000,
    });
    assert!(state.snapshot.is_none());

    // Valid cached snapshot starts Stale until verified.
    let cached = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: Some(QuotaWindow {
            remaining_percent: 85,
            resets_at: None,
        }),
        weekly: None,
        fetched_at: 2_000,
    };
    state.seed_from_cache(cached.clone());
    assert_eq!(state.health, ProviderHealth::Stale);
    assert_eq!(state.visible_snapshot(1_900, 3_600), Some(&cached)); // clock skew safe

    // Clock skew on fresh snapshot does not underflow or mark stale.
    state.apply_success(cached, 1_900);
    assert_eq!(state.effective_health(1_900, 900), ProviderHealth::Fresh);

    // Empty-window success marks Unavailable and renders `–/–`, never 0 or 100.
    state.apply_success(
        QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: None,
            weekly: None,
            fetched_at: 2_100,
        },
        2_100,
    );
    assert_eq!(state.health, ProviderHealth::Unavailable);
    assert_eq!(
        format_menu_bar_snapshot(state.visible_snapshot(2_100, 3_600)),
        "–/–"
    );
}

#[test]
fn config_defaults_partial_json_and_validation() {
    let dir = temp_dir("config");
    let cfg_path = dir.join("config.json");

    // Missing file returns defaults.
    let default_cfg = AppConfig::load_or_default(&cfg_path).unwrap();
    assert_eq!(
        default_cfg.refresh_interval_seconds,
        DEFAULT_REFRESH_INTERVAL_SECS
    );
    assert!(default_cfg.show_claude);
    assert!(default_cfg.show_codex);
    assert!(!default_cfg.launch_at_login);
    assert_eq!(default_cfg.codex_path, None);
    assert_eq!(
        default_cfg.claude_stale_after_seconds,
        DEFAULT_CLAUDE_STALE_AFTER_SECS
    );
    assert_eq!(
        default_cfg.codex_stale_after_seconds,
        DEFAULT_CODEX_STALE_AFTER_SECS
    );
    assert_eq!(default_cfg.snapshot_ttl_seconds, DEFAULT_SNAPSHOT_TTL_SECS);

    // Partial JSON fills remaining defaults.
    fs::write(
        &cfg_path,
        r#"{"refresh_interval_seconds": 120, "codex_path": "/opt/homebrew/bin/codex"}"#,
    )
    .unwrap();
    let loaded = AppConfig::load_or_default(&cfg_path).unwrap();
    assert_eq!(loaded.refresh_interval_seconds, 120);
    assert_eq!(
        loaded.codex_path.as_deref(),
        Some("/opt/homebrew/bin/codex")
    );
    assert!(loaded.show_claude);

    // Invalid interval rejected.
    let bad_interval = AppConfig {
        refresh_interval_seconds: 5,
        ..AppConfig::default()
    };
    assert!(matches!(
        bad_interval.validate(),
        Err(AppError::InvalidConfig(_))
    ));

    let bad_codex_path = AppConfig {
        codex_path: Some("   ".into()),
        ..AppConfig::default()
    };
    assert!(matches!(
        bad_codex_path.validate(),
        Err(AppError::InvalidConfig(_))
    ));

    // Malformed JSON returns ConfigParse.
    fs::write(&cfg_path, "{ not valid json").unwrap();
    assert!(matches!(
        AppConfig::load_or_default(&cfg_path),
        Err(AppError::ConfigParse { .. })
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn atomic_config_and_cache_persistence_and_permissions() {
    let dir = temp_dir("atomic");
    let paths = AppPaths::from_base_dir(dir.join("AIUsageMonitor"));

    // Save config first time -> writes disk.
    let cfg = AppConfig {
        refresh_interval_seconds: 90,
        show_claude: true,
        show_codex: true,
        launch_at_login: false,
        codex_path: Some("/usr/local/bin/codex".into()),
        ..AppConfig::default()
    };
    assert!(cfg.save_atomic(&paths.config_path).unwrap());
    // Saving identical content skips disk write.
    assert!(!cfg.save_atomic(&paths.config_path).unwrap());
    assert_eq!(AppConfig::load_or_default(&paths.config_path).unwrap(), cfg);

    // Cache empty when missing, then atomic roundtrip.
    assert_eq!(paths.load_cache_or_empty().unwrap(), UsageCache::empty());
    let mut cache = UsageCache::empty();
    cache.set_snapshot(QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: Some(QuotaWindow {
            remaining_percent: 85,
            resets_at: Some(1_795_239_600),
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 97,
            resets_at: Some(1_795_788_000),
        }),
        fetched_at: 1_795_222_336,
    });
    assert!(paths.save_cache_atomic(&cache).unwrap());
    assert!(!paths.save_cache_atomic(&cache).unwrap());
    assert_eq!(paths.load_cache_or_empty().unwrap(), cache);

    // Verify no leftover .tmp files and Unix 0600 permissions.
    let entries: Vec<_> = fs::read_dir(&paths.base_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(!entries.iter().any(|name| name.ends_with(".tmp")));

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(&paths.cache_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&paths.base_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
    }

    let _ = fs::remove_dir_all(&dir);
}
