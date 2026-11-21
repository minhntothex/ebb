use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ebb::config::{AppConfig, AppPaths};
use ebb::model::{DisplayMetric, MenuBarWindows, Provider, QuotaSnapshot, QuotaWindow, UsageCache};
use ebb::providers::agy::write_agy_cache_atomic;
use ebb::providers::claude::write_claude_cache_atomic;
use ebb::status::{collect_status_report, format_status_json, format_status_plain};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ebb-status-cli-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

#[test]
fn collect_status_report_respects_order_display_modes_and_json_schema() {
    let dir = temp_dir("report");
    let paths = AppPaths::from_base_dir(dir.clone());
    let now = 50_000;

    let mut cfg = AppConfig {
        show_claude: true,
        show_codex: true,
        show_agy: true,
        display_metric: DisplayMetric::Used,
        menu_bar_windows: MenuBarWindows::Both,
        ..AppConfig::default()
    };
    cfg.move_provider_first(Provider::Agy);
    cfg.save_atomic(&paths.config_path).unwrap();

    write_claude_cache_atomic(
        &paths.claude_cache_path,
        &QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 65,
                resets_at: Some(now + 3600),
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 72,
                resets_at: Some(now + 86_400),
            }),
            fetched_at: now - 10,
        },
        0,
    )
    .unwrap();

    write_agy_cache_atomic(
        &paths.agy_cache_path,
        &QuotaSnapshot {
            provider: Provider::Agy,
            five_hour: None,
            weekly: Some(QuotaWindow {
                remaining_percent: 94,
                resets_at: Some(now + 200_000),
            }),
            fetched_at: now - 5,
        },
        0,
    )
    .unwrap();

    let mut cache = UsageCache::empty();
    cache.set_snapshot(QuotaSnapshot {
        provider: Provider::Codex,
        five_hour: Some(QuotaWindow {
            remaining_percent: 43,
            resets_at: Some(now + 1800),
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 31,
            resets_at: Some(now + 300_000),
        }),
        fetched_at: now - 20,
    });
    paths.save_cache_atomic(&cache).unwrap();

    let report = collect_status_report(&paths, false, now).unwrap();
    assert_eq!(report.version, 1);
    assert_eq!(
        report.provider_order,
        vec![Provider::Agy, Provider::Claude, Provider::Codex]
    );
    assert_eq!(
        format_status_plain(&report),
        "agy –/6  claude 35/28  codex 57/69"
    );

    let json_str = format_status_json(&report).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
    assert_eq!(parsed["version"], 1);
    assert_eq!(parsed["display_metric"], "used");
    assert_eq!(
        parsed["providers"]["claude"]["five_hour"]["remaining_percent"],
        65
    );
    assert_eq!(
        parsed["providers"]["claude"]["five_hour"]["used_percent"],
        35
    );
    assert_eq!(
        parsed["providers"]["agy"]["weekly"]["remaining_percent"],
        94
    );
    assert_eq!(parsed["providers"]["codex"]["stale"], true);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_ebb_status_plain_and_json_end_to_end() {
    let dir = temp_dir("e2e");
    let paths = AppPaths::from_base_dir(dir.clone());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let cfg = AppConfig {
        show_claude: true,
        show_codex: false,
        show_agy: true,
        ..AppConfig::default()
    };
    cfg.save_atomic(&paths.config_path).unwrap();

    write_claude_cache_atomic(
        &paths.claude_cache_path,
        &QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 80,
                resets_at: Some(now + 7200),
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 90,
                resets_at: Some(now + 300_000),
            }),
            fetched_at: now,
        },
        0,
    )
    .unwrap();
    write_agy_cache_atomic(
        &paths.agy_cache_path,
        &QuotaSnapshot {
            provider: Provider::Agy,
            five_hour: None,
            weekly: Some(QuotaWindow {
                remaining_percent: 95,
                resets_at: Some(now + 400_000),
            }),
            fetched_at: now,
        },
        0,
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_ebb");

    let plain = Command::new(bin)
        .args(["status", "--base-dir"])
        .arg(&dir)
        .output()
        .expect("run ebb status");
    assert!(plain.status.success());
    assert_eq!(
        String::from_utf8_lossy(&plain.stdout).trim(),
        "claude 80/90  agy –/95"
    );

    let json = Command::new(bin)
        .args(["status", "--json", "--base-dir"])
        .arg(&dir)
        .output()
        .expect("run ebb status --json");
    assert!(json.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(parsed["version"], 1);
    assert_eq!(parsed["providers"]["claude"]["menu_bar_text"], "80/90");
    assert_eq!(parsed["providers"]["codex"]["visible"], false);
    assert_eq!(parsed["providers"]["agy"]["menu_bar_text"], "–/95");

    let _ = fs::remove_dir_all(&dir);
}
