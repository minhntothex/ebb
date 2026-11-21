use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ebb::error::AppError;
use ebb::model::Provider;
use ebb::providers::codex::{CodexAdapter, parse_rate_limits_response, resolve_codex_binary};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ai-usage-monitor-codex-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

fn fixture(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    fs::read(path).expect("read fixture")
}

fn write_executable_script(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[test]
fn parses_real_step1_fixture_and_swapped_window_durations() {
    // Full real sanitized payload from Step 1 spike (3% 5h, 16% weekly -> 97/84 remaining).
    let snap =
        parse_rate_limits_response(&fixture("codex_rate_limits_full.json"), 1_795_222_500).unwrap();
    assert_eq!(snap.provider, Provider::Codex);
    assert_eq!(snap.five_hour.unwrap().remaining_percent, 97);
    assert_eq!(snap.five_hour.unwrap().resets_at, Some(1_795_238_616));
    assert_eq!(snap.weekly.unwrap().remaining_percent, 84);
    assert_eq!(snap.weekly.unwrap().resets_at, Some(1_795_767_342));
    assert_eq!(snap.format_menu_bar(), "97/84");
    assert_eq!(snap.format_dropdown(), "97% / 84%");

    // Swapped slots: primary is weekly (10080m, 69% used -> 31%), secondary is 5h (300m, 57% used -> 43%).
    let swapped =
        parse_rate_limits_response(&fixture("codex_rate_limits_swapped_windows.json"), 1_000)
            .unwrap();
    assert_eq!(swapped.five_hour.unwrap().remaining_percent, 43);
    assert_eq!(swapped.weekly.unwrap().remaining_percent, 31);
    assert_eq!(swapped.format_menu_bar(), "43/31");
}

#[test]
fn selects_canonical_codex_bucket_from_multi_bucket_response() {
    let snap =
        parse_rate_limits_response(&fixture("codex_rate_limits_multi_bucket.json"), 1_000).unwrap();
    // Must pick `rateLimitsByLimitId["codex"]` (25%/40% used -> 75/60), NOT `codex_model_alias` (90%/80%).
    assert_eq!(snap.five_hour.unwrap().remaining_percent, 75);
    assert_eq!(snap.weekly.unwrap().remaining_percent, 60);
    assert_eq!(snap.format_menu_bar(), "75/60");
}

#[test]
fn handles_partial_unknown_durations_and_rpc_error_classification() {
    // Unknown durations (60m, 1440m) must NOT be mislabeled as 5h or weekly.
    let unknown_durations = br#"{
        "id": 2,
        "result": {
            "rateLimits": {
                "limitId": "codex",
                "primary": { "usedPercent": 10, "windowDurationMins": 60 },
                "secondary": { "usedPercent": 20, "windowDurationMins": 1440 }
            }
        }
    }"#;
    let snap = parse_rate_limits_response(unknown_durations, 1_000).unwrap();
    assert!(snap.five_hour.is_none());
    assert!(snap.weekly.is_none());
    assert_eq!(snap.format_menu_bar(), "–/–");

    // Partial: only weekly present.
    let weekly_only = br#"{
        "id": 2,
        "result": {
            "rateLimits": {
                "primary": { "usedPercent": 28, "windowDurationMins": 10080 },
                "secondary": null
            }
        }
    }"#;
    let snap_w = parse_rate_limits_response(weekly_only, 1_000).unwrap();
    assert_eq!(snap_w.format_menu_bar(), "–/72");

    // Out-of-range usedPercent rejected.
    let bad_pct = br#"{
        "id": 2,
        "result": {
            "rateLimits": {
                "primary": { "usedPercent": 120, "windowDurationMins": 300 }
            }
        }
    }"#;
    assert!(matches!(
        parse_rate_limits_response(bad_pct, 1_000),
        Err(AppError::InvalidPercentage {
            provider: Provider::Codex,
            ..
        })
    ));

    // Auth RPC error -> Unauthenticated.
    let auth_err = br#"{"id":2,"error":{"code":-32000,"message":"User is not logged in (401)"}}"#;
    assert!(matches!(
        parse_rate_limits_response(auth_err, 1_000),
        Err(AppError::Unauthenticated {
            provider: Provider::Codex
        })
    ));

    // Method not found -> ProviderUnavailable.
    let unsupported = br#"{"id":2,"error":{"code":-32601,"message":"Method not found"}}"#;
    assert!(matches!(
        parse_rate_limits_response(unsupported, 1_000),
        Err(AppError::ProviderUnavailable {
            provider: Provider::Codex,
            ..
        })
    ));
}

#[test]
fn resolves_codex_binary_from_override_path_and_nvm_fallback() {
    let dir = temp_dir("resolve");
    let fake_home = dir.join("home");

    // Create two nvm versions; resolver should prefer the newer semver (v22.21.1 over v20.19.0).
    let nvm_old = fake_home.join(".nvm/versions/node/v20.19.0/bin/codex");
    let nvm_new = fake_home.join(".nvm/versions/node/v22.21.1/bin/codex");
    write_executable_script(&nvm_old, "#!/bin/sh\nexit 0\n");
    write_executable_script(&nvm_new, "#!/bin/sh\nexit 0\n");

    // Empty PATH (simulates macOS `.app` bundle launch) -> finds latest NVM binary.
    let empty_path = OsString::from("");
    let found_nvm =
        resolve_codex_binary(None, Some(empty_path.as_os_str()), Some(&fake_home)).unwrap();
    assert_eq!(found_nvm, nvm_new);

    // PATH takes priority over fallback.
    let path_bin_dir = dir.join("custom-bin");
    let path_codex = path_bin_dir.join("codex");
    write_executable_script(&path_codex, "#!/bin/sh\nexit 0\n");
    let found_path =
        resolve_codex_binary(None, Some(path_bin_dir.as_os_str()), Some(&fake_home)).unwrap();
    assert_eq!(found_path, path_codex);

    // Explicit override takes highest priority.
    let override_codex = dir.join("override/codex");
    write_executable_script(&override_codex, "#!/bin/sh\nexit 0\n");
    let found_override = resolve_codex_binary(
        Some(override_codex.to_str().unwrap()),
        Some(path_bin_dir.as_os_str()),
        Some(&fake_home),
    )
    .unwrap();
    assert_eq!(found_override, override_codex);

    // Missing explicit override errors clearly instead of silently falling back.
    assert!(matches!(
        resolve_codex_binary(
            Some("/nonexistent/path/to/codex"),
            Some(path_bin_dir.as_os_str()),
            Some(&fake_home),
        ),
        Err(AppError::ProviderUnavailable {
            provider: Provider::Codex,
            ..
        })
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_handshake_skips_notifications_and_returns_normalized_quota() {
    let dir = temp_dir("handshake");
    let mock_server = dir.join("mock_codex.py");
    let log_path = dir.join("requests.jsonl");

    let script = format!(
        r#"#!/usr/bin/env python3
import json, sys

log_path = {log_path:?}
requests = []

for raw in sys.stdin:
    line = raw.strip()
    if not line:
        continue
    msg = json.loads(line)
    requests.append(msg)
    with open(log_path, "w") as f:
        for r in requests:
            f.write(json.dumps(r) + "\n")
    method = msg.get("method")
    if method == "initialize":
        # Emit an interleaved notification before replying
        print(json.dumps({{"method": "configWarning", "params": {{"summary": "test"}}}}), flush=True)
        print(json.dumps({{"id": msg["id"], "result": {{"userAgent": "mock/1.0"}}}}), flush=True)
    elif method == "initialized":
        continue
    elif method == "account/rateLimits/read":
        assert msg.get("params", {{}}).get("excludeResetCreditDetails") is True
        print(json.dumps({{"method": "remoteControl/status/changed", "params": {{}}}}), flush=True)
        print(json.dumps({{
            "id": msg["id"],
            "result": {{
                "rateLimits": {{
                    "limitId": "codex",
                    "primary": {{"usedPercent": 3, "windowDurationMins": 300, "resetsAt": 1795238616}},
                    "secondary": {{"usedPercent": 16, "windowDurationMins": 10080, "resetsAt": 1795767342}}
                }}
            }}
        }}), flush=True)
        break
"#,
        log_path = log_path.to_string_lossy()
    );
    write_executable_script(&mock_server, &script);

    let adapter = CodexAdapter::new(None).with_timeout(Duration::from_secs(3));
    let snap = adapter.fetch_from_binary(&mock_server, &[]).unwrap();
    assert_eq!(snap.format_menu_bar(), "97/84");
    assert_eq!(snap.format_dropdown(), "97% / 84%");

    // Verify exact 3-step handshake sequence sent over stdin.
    let logged = fs::read_to_string(&log_path).unwrap();
    let lines: Vec<serde_json::Value> = logged
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["method"], "initialize");
    assert_eq!(lines[1]["method"], "initialized");
    assert_eq!(lines[2]["method"], "account/rateLimits/read");
    assert_eq!(lines[2]["params"]["excludeResetCreditDetails"], true);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_timeout_kills_and_reaps_hung_child() {
    let dir = temp_dir("timeout");
    let pid_file = dir.join("child.pid");
    let hung_script = dir.join("hung_codex.sh");

    write_executable_script(
        &hung_script,
        &format!(
            "#!/bin/sh\necho $$ > '{}'\nexec sleep 30\n",
            pid_file.display()
        ),
    );

    let adapter = CodexAdapter::new(None).with_timeout(Duration::from_millis(250));
    let started = Instant::now();
    let err = adapter.fetch_from_binary(&hung_script, &[]).unwrap_err();
    let elapsed = started.elapsed();

    assert!(matches!(
        err,
        AppError::Timeout {
            provider: Provider::Codex,
            ..
        }
    ));
    assert!(
        elapsed < Duration::from_secs(2),
        "timeout cleanup took too long: {elapsed:?}"
    );

    // Give kernel a brief moment if needed and verify PID is no longer alive.
    if let Ok(pid_str) = fs::read_to_string(&pid_file) {
        let pid = pid_str.trim();
        let alive = std::process::Command::new("/bin/kill")
            .args(["-0", pid])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(!alive, "hung child pid {pid} leaked after timeout");
    }

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn subprocess_rejects_oversized_line_and_early_crash_with_stderr() {
    let dir = temp_dir("errors");

    // 1. Oversized line exceeds configured limit without OOM.
    let huge_script = dir.join("huge_line.py");
    write_executable_script(
        &huge_script,
        "#!/usr/bin/env python3\nimport sys\nsys.stdout.write('x' * 4096 + '\\n')\nsys.stdout.flush()\n",
    );
    let adapter = CodexAdapter::new(None)
        .with_timeout(Duration::from_secs(2))
        .with_max_line_bytes(512);
    assert!(matches!(
        adapter.fetch_from_binary(&huge_script, &[]),
        Err(AppError::Protocol {
            provider: Provider::Codex,
            ..
        })
    ));

    // 2. Immediate crash includes stderr snippet.
    let crash_script = dir.join("crash.sh");
    write_executable_script(
        &crash_script,
        "#!/bin/sh\necho 'fatal: state db unavailable' >&2\nexit 1\n",
    );
    let err = CodexAdapter::new(None)
        .with_timeout(Duration::from_secs(2))
        .fetch_from_binary(&crash_script, &[])
        .unwrap_err();
    match err {
        AppError::Subprocess {
            provider: Provider::Codex,
            message,
        } => {
            assert!(
                message.contains("state db unavailable"),
                "expected stderr snippet in: {message}"
            );
        }
        other => panic!("unexpected error variant: {other:?}"),
    }

    let _ = fs::remove_dir_all(&dir);
}
