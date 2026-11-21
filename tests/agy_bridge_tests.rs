use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use ebb::error::AppError;
use ebb::model::Provider;
use ebb::providers::agy::{
    AgyAdapter, apply_agy_statusline_install, parse_agy_statusline_json, parse_rfc3339_utc_secs,
    plan_agy_statusline_install,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ebb-agy-bridge-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

#[test]
fn parses_official_agy_weekly_and_five_hour_payloads_and_rfc3339() {
    // Exact official docs payload example (`gemini-weekly` only).
    let official = br#"{
        "product": "antigravity",
        "model": { "id": "Gemini 4 Argon (Medium)", "display_name": "Gemini 4 Argon (Medium)" },
        "context_window": { "used_percentage": 14.24, "remaining_percentage": 85.76 },
        "quota": {
            "gemini-weekly": {
                "remaining_fraction": 0.9378,
                "reset_time": "2026-07-06T07:50:32Z",
                "reset_in_seconds": 560580
            }
        }
    }"#;
    let now = 1_782_817_652;
    let snap = parse_agy_statusline_json(official, now)
        .unwrap()
        .expect("weekly snapshot");
    assert_eq!(snap.provider, Provider::Agy);
    assert!(snap.five_hour.is_none());
    let weekly = snap.weekly.expect("weekly");
    assert_eq!(weekly.remaining_percent, 94);
    assert_eq!(weekly.resets_at, Some(now + 560_580));
    assert_eq!(snap.format_menu_bar(), "–/94");
    assert_eq!(snap.format_dropdown(), "– / 94%");

    // Both 5h and weekly buckets, using RFC-3339 `reset_time` when `reset_in_seconds` is absent.
    let both = br#"{
        "model": { "id": "Gemini 4 Neon" },
        "quota": {
            "gemini-5h": {
                "remaining_fraction": 0.425,
                "reset_time": "2026-07-06T07:50:32Z"
            },
            "gemini-weekly": {
                "remaining_fraction": 0.801,
                "reset_time": "2026-07-12T00:00:00+00:00"
            }
        }
    }"#;
    let parsed_both = parse_agy_statusline_json(both, 1_783_320_000)
        .unwrap()
        .expect("both windows");
    assert_eq!(parsed_both.five_hour.unwrap().remaining_percent, 43);
    assert_eq!(parsed_both.weekly.unwrap().remaining_percent, 80);
    assert_eq!(
        parsed_both.five_hour.unwrap().resets_at,
        parse_rfc3339_utc_secs("2026-07-06T07:50:32Z")
    );
    assert_eq!(parse_rfc3339_utc_secs("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(
        parse_rfc3339_utc_secs("2026-07-06T09:50:32+02:00"),
        parse_rfc3339_utc_secs("2026-07-06T07:50:32Z")
    );
    assert_eq!(
        parse_rfc3339_utc_secs("2026-07-06T07:50:32.123456Z"),
        parse_rfc3339_utc_secs("2026-07-06T07:50:32Z")
    );
    assert_eq!(parse_rfc3339_utc_secs("not-a-timestamp"), None);
    assert_eq!(parse_rfc3339_utc_secs("2026-02-29T00:00:00Z"), None);
    assert!(parse_rfc3339_utc_secs("2024-02-29T00:00:00Z").is_some());
}

#[test]
fn selects_active_model_family_and_handles_disabled_and_honest_omissions() {
    // Active Claude model selects `claude-*` buckets over `gemini-*`.
    let multi = br#"{
        "model": { "id": "Claude Sonnet 5.5 (thinking)" },
        "quota": {
            "gemini-5h": { "remaining_fraction": 0.90, "reset_in_seconds": 7200 },
            "gemini-weekly": { "remaining_fraction": 0.85, "reset_in_seconds": 400000 },
            "claude-5h": { "remaining_fraction": 0.25, "reset_in_seconds": 3600 },
            "claude-weekly": { "remaining_fraction": 0.60, "reset_in_seconds": 300000 }
        }
    }"#;
    let snap = parse_agy_statusline_json(multi, 10_000).unwrap().unwrap();
    assert_eq!(snap.five_hour.unwrap().remaining_percent, 25);
    assert_eq!(snap.weekly.unwrap().remaining_percent, 60);

    // Without matching model family, falls back to lowest remaining % per window.
    let fallback = br#"{
        "quota": {
            "gemini-weekly": { "remaining_fraction": 0.85, "reset_in_seconds": 400000 },
            "claude-weekly": { "remaining_fraction": 0.30, "reset_in_seconds": 300000 },
            "disabled-weekly": { "remaining_fraction": 0.01, "disabled": true, "reset_in_seconds": 300000 }
        }
    }"#;
    let snap_fb = parse_agy_statusline_json(fallback, 10_000)
        .unwrap()
        .unwrap();
    assert_eq!(snap_fb.weekly.unwrap().remaining_percent, 30);

    // Bucket without window keyword classified conservatively via `reset_in_seconds`.
    let by_duration = br#"{
        "quota": {
            "gemini-pro": { "remaining_fraction": 0.55, "reset_in_seconds": 9000 }
        }
    }"#;
    let snap_dur = parse_agy_statusline_json(by_duration, 10_000)
        .unwrap()
        .unwrap();
    assert_eq!(snap_dur.five_hour.unwrap().remaining_percent, 55);
    assert!(snap_dur.weekly.is_none());

    // `remaining_amount` only or missing `quota` returns Ok(None) (never fabricates `%`).
    let credits_only = br#"{
        "context_window": { "used_percentage": 40.0 },
        "quota": {
            "ai-credits": { "remaining_amount": 420 }
        }
    }"#;
    assert_eq!(
        parse_agy_statusline_json(credits_only, 10_000).unwrap(),
        None
    );
    assert_eq!(
        parse_agy_statusline_json(br#"{"agent_state":"idle"}"#, 10_000).unwrap(),
        None
    );

    // Invalid fraction returns InvalidPercentage; malformed JSON returns Protocol error.
    let bad_frac = br#"{"quota":{"gemini-weekly":{"remaining_fraction":1.5}}}"#;
    assert!(matches!(
        parse_agy_statusline_json(bad_frac, 10_000),
        Err(AppError::InvalidPercentage {
            provider: Provider::Agy,
            ..
        })
    ));
    assert!(matches!(
        parse_agy_statusline_json(b"{broken", 10_000),
        Err(AppError::Protocol {
            provider: Provider::Agy,
            ..
        })
    ));
}

#[test]
fn agy_installer_sets_stack_with_default_standalone_and_chains_existing_with_backup() {
    let dir = temp_dir("install");
    let bin = PathBuf::from("/Applications/Ebb.app/Contents/MacOS/ebb");

    // 1. Standalone install on fresh settings.json sets `stack_with_default: true` & `enabled: true`.
    let fresh_settings = dir.join("fresh-settings.json");
    let plan1 = plan_agy_statusline_install(&fresh_settings, &bin).unwrap();
    assert!(!plan1.already_installed);
    assert!(plan1.previous_command.is_none());
    assert_eq!(
        plan1.new_command,
        "'/Applications/Ebb.app/Contents/MacOS/ebb' agy-statusline"
    );
    let (_, backup1) = apply_agy_statusline_install(&fresh_settings, &bin).unwrap();
    assert!(backup1.is_none());
    let written1: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&fresh_settings).unwrap()).unwrap();
    assert_eq!(written1["statusLine"]["type"], "command");
    assert_eq!(written1["statusLine"]["enabled"], true);
    assert_eq!(written1["statusLine"]["stack_with_default"], true);

    // Idempotent second call changes nothing.
    let plan1_again = plan_agy_statusline_install(&fresh_settings, &bin).unwrap();
    assert!(plan1_again.already_installed);

    // 2. Existing custom command gets backed up, wrapped, and preserves user settings.
    let custom_settings = dir.join("custom-settings.json");
    fs::write(
        &custom_settings,
        r#"{
  "theme": "dark",
  "statusLine": {
    "type": "command",
    "command": "~/.gemini/antigravity-cli/statusline.sh",
    "padding": 1,
    "stack_with_default": false
  }
}"#,
    )
    .unwrap();
    let (plan2, backup2) = apply_agy_statusline_install(&custom_settings, &bin).unwrap();
    assert!(!plan2.already_installed);
    assert!(backup2.expect("backup created").exists());
    let written2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&custom_settings).unwrap()).unwrap();
    assert_eq!(written2["theme"], "dark");
    assert_eq!(written2["statusLine"]["padding"], 1);
    assert_eq!(written2["statusLine"]["stack_with_default"], false);
    assert_eq!(
        written2["statusLine"]["command"],
        "'/Applications/Ebb.app/Contents/MacOS/ebb' agy-statusline -- '~/.gemini/antigravity-cli/statusline.sh'"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_agy_bridge_standalone_and_chained_end_to_end() {
    let dir = temp_dir("cli");
    let cache_path = dir.join("agy-statusline-cache.json");
    let bin = env!("CARGO_BIN_EXE_ebb");

    let payload = br#"{"model":{"id":"Gemini 4 Argon"},"quota":{"gemini-weekly":{"remaining_fraction":0.73,"reset_in_seconds":400000}}}"#;

    // 1. Standalone bridge writes cache with empty stdout and exit 0.
    let mut child = Command::new(bin)
        .args(["agy-statusline", "--cache-path"])
        .arg(&cache_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ebb agy-statusline");
    child.stdin.take().unwrap().write_all(payload).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    assert!(out.stdout.is_empty());

    let adapter = AgyAdapter::new(cache_path.clone());
    let cached = adapter.read_snapshot().unwrap().expect("cached agy snap");
    assert_eq!(cached.provider, Provider::Agy);
    assert_eq!(cached.weekly.unwrap().remaining_percent, 73);

    // 2. Chained bridge preserves stdout, stderr, and exit code while updating cache.
    let payload2 = br#"{"model":{"id":"Gemini 4 Neon"},"quota":{"gemini-5h":{"remaining_fraction":0.61,"reset_in_seconds":10000},"gemini-weekly":{"remaining_fraction":0.70,"reset_in_seconds":399000}}}"#;
    let mut chained = Command::new(bin)
        .args(["agy-statusline", "--cache-path"])
        .arg(&cache_path)
        .args([
            "--",
            "cat >/dev/null; echo AGY_OUT; echo AGY_ERR >&2; exit 7",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chained agy-statusline");
    chained.stdin.take().unwrap().write_all(payload2).unwrap();
    let chained_out = chained.wait_with_output().unwrap();
    assert_eq!(chained_out.status.code(), Some(7));
    assert_eq!(
        String::from_utf8_lossy(&chained_out.stdout).trim(),
        "AGY_OUT"
    );
    assert_eq!(
        String::from_utf8_lossy(&chained_out.stderr).trim(),
        "AGY_ERR"
    );

    let updated = adapter.read_snapshot().unwrap().expect("updated agy snap");
    assert_eq!(updated.five_hour.unwrap().remaining_percent, 61);
    assert_eq!(updated.weekly.unwrap().remaining_percent, 70);

    let _ = fs::remove_dir_all(&dir);
}
