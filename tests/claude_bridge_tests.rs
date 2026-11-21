use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use ebb::error::AppError;
use ebb::model::{Provider, QuotaSnapshot, QuotaWindow};
use ebb::providers::claude::{
    ClaudeAdapter, MIN_IDENTICAL_REWRITE_SECS, StatuslineBridgeArgs, apply_statusline_install,
    build_wrapped_statusline_command, parse_statusline_json, plan_statusline_install,
    shell_single_quote, write_claude_cache_atomic,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ai-usage-monitor-claude-{label}-{}-{seq}",
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

#[test]
fn parses_full_partial_missing_and_gateway_fixtures() {
    let full = parse_statusline_json(&fixture("claude_statusline_full.json"), 1_795_222_336)
        .unwrap()
        .expect("full snapshot");
    assert_eq!(full.provider, Provider::Claude);
    assert_eq!(full.five_hour.unwrap().remaining_percent, 85);
    assert_eq!(full.five_hour.unwrap().resets_at, Some(1_795_239_600));
    assert_eq!(full.weekly.unwrap().remaining_percent, 97);
    assert_eq!(full.weekly.unwrap().resets_at, Some(1_795_788_000));
    assert_eq!(full.format_menu_bar(), "85/97");
    assert_eq!(full.format_dropdown(), "85% / 97%");

    let partial =
        parse_statusline_json(&fixture("claude_statusline_partial_5h.json"), 1_795_222_400)
            .unwrap()
            .expect("partial snapshot");
    assert_eq!(partial.five_hour.unwrap().remaining_percent, 65); // round(100 - 34.6) = 65
    assert!(partial.weekly.is_none());
    assert_eq!(partial.format_menu_bar(), "65/–");
    assert_eq!(partial.format_dropdown(), "65% / –");

    // Must NOT infer quota from context_window.used_percentage.
    let no_rl = parse_statusline_json(
        &fixture("claude_statusline_no_rate_limits.json"),
        1_795_222_400,
    )
    .unwrap();
    assert!(no_rl.is_none());

    // Gateway-only spend_limit is ignored for MVP v1.
    let spend_only = parse_statusline_json(
        &fixture("claude_statusline_gateway_spend_only.json"),
        1_795_222_400,
    )
    .unwrap();
    assert!(spend_only.is_none());

    // Out-of-range percentage fails with InvalidPercentage.
    let bad = br#"{"rate_limits":{"five_hour":{"used_percentage":105.0}}}"#;
    assert!(matches!(
        parse_statusline_json(bad, 1_000),
        Err(AppError::InvalidPercentage { .. })
    ));

    // Malformed JSON fails with Protocol error.
    assert!(matches!(
        parse_statusline_json(b"{broken", 1_000),
        Err(AppError::Protocol {
            provider: Provider::Claude,
            ..
        })
    ));
}

#[test]
fn cache_writer_handles_throttling_ordering_and_adapter_reads() {
    let dir = temp_dir("cache");
    let cache_path = dir.join("claude-statusline-cache.json");
    let adapter = ClaudeAdapter::new(cache_path.clone());

    assert_eq!(adapter.read_snapshot().unwrap(), None);

    let snap1 = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: Some(QuotaWindow {
            remaining_percent: 85,
            resets_at: Some(1_795_239_600),
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 97,
            resets_at: Some(1_795_788_000),
        }),
        fetched_at: 10_000,
    };
    assert!(write_claude_cache_atomic(&cache_path, &snap1, MIN_IDENTICAL_REWRITE_SECS).unwrap());
    assert_eq!(adapter.read_snapshot().unwrap(), Some(snap1.clone()));

    // Identical data within throttle window skips write.
    let snap_same_soon = QuotaSnapshot {
        fetched_at: 10_020,
        ..snap1.clone()
    };
    assert!(
        !write_claude_cache_atomic(&cache_path, &snap_same_soon, MIN_IDENTICAL_REWRITE_SECS)
            .unwrap()
    );

    // Identical data after throttle window refreshes fetched_at.
    let snap_same_later = QuotaSnapshot {
        fetched_at: 10_000 + MIN_IDENTICAL_REWRITE_SECS,
        ..snap1.clone()
    };
    assert!(
        write_claude_cache_atomic(&cache_path, &snap_same_later, MIN_IDENTICAL_REWRITE_SECS)
            .unwrap()
    );
    assert_eq!(
        adapter.read_snapshot().unwrap().unwrap().fetched_at,
        10_000 + MIN_IDENTICAL_REWRITE_SECS
    );

    // Older out-of-order session MUST NOT overwrite newer cache.
    let snap_older = QuotaSnapshot {
        five_hour: Some(QuotaWindow {
            remaining_percent: 50,
            resets_at: None,
        }),
        fetched_at: 10_010,
        ..snap1.clone()
    };
    assert!(
        !write_claude_cache_atomic(&cache_path, &snap_older, MIN_IDENTICAL_REWRITE_SECS).unwrap()
    );
    assert_eq!(
        adapter
            .read_snapshot()
            .unwrap()
            .unwrap()
            .five_hour
            .unwrap()
            .remaining_percent,
        85
    );

    // Changed quota updates immediately even within throttle window.
    let snap_changed = QuotaSnapshot {
        five_hour: Some(QuotaWindow {
            remaining_percent: 82,
            resets_at: Some(1_795_239_600),
        }),
        fetched_at: 10_000 + MIN_IDENTICAL_REWRITE_SECS + 5,
        ..snap1
    };
    assert!(
        write_claude_cache_atomic(&cache_path, &snap_changed, MIN_IDENTICAL_REWRITE_SECS).unwrap()
    );
    assert_eq!(
        adapter
            .read_snapshot()
            .unwrap()
            .unwrap()
            .five_hour
            .unwrap()
            .remaining_percent,
        82
    );

    // Corrupted cache file yields typed CacheParse error.
    fs::write(&cache_path, "{corrupt").unwrap();
    assert!(matches!(
        adapter.read_snapshot(),
        Err(AppError::CacheParse { .. })
    ));

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_bridge_standalone_writes_cache_with_empty_stdout() {
    let dir = temp_dir("standalone");
    let cache_path = dir.join("claude-statusline-cache.json");
    let bin = env!("CARGO_BIN_EXE_ebb");

    let mut child = Command::new(bin)
        .args(["claude-statusline", "--cache-path"])
        .arg(&cache_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bridge");

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&fixture("claude_statusline_full.json"))
        .unwrap();
    let out = child.wait_with_output().unwrap();

    assert_eq!(out.status.code(), Some(0));
    assert!(
        out.stdout.is_empty(),
        "standalone bridge must not write stdout"
    );
    assert!(out.stderr.is_empty());

    let snap = ClaudeAdapter::new(cache_path)
        .read_snapshot()
        .unwrap()
        .expect("cached snapshot");
    assert_eq!(snap.format_menu_bar(), "85/97");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_bridge_chains_existing_command_preserving_stdin_stdout_stderr_and_exit_code() {
    let dir = temp_dir("chain");
    let cache_path = dir.join("claude-statusline-cache.json");
    let captured_stdin_path = dir.join("captured-stdin.json");
    let mock_script = dir.join("mock statusline.sh");

    fs::write(
        &mock_script,
        format!(
            "#!/bin/sh\ncat > '{}'\nprintf 'custom-status-ok'\nprintf 'minor-warn' >&2\nexit 42\n",
            captured_stdin_path.display()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&mock_script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let original_cmd = format!("\"{}\"", mock_script.display());
    let payload = fixture("claude_statusline_full.json");
    let bin = env!("CARGO_BIN_EXE_ebb");

    let mut child = Command::new(bin)
        .args(["claude-statusline", "--cache-path"])
        .arg(&cache_path)
        .arg("--")
        .arg(&original_cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn chained bridge");

    child.stdin.as_mut().unwrap().write_all(&payload).unwrap();
    let out = child.wait_with_output().unwrap();

    assert_eq!(out.status.code(), Some(42));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "custom-status-ok");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "minor-warn");
    assert_eq!(fs::read(&captured_stdin_path).unwrap(), payload);

    let snap = ClaudeAdapter::new(cache_path)
        .read_snapshot()
        .unwrap()
        .expect("cached snapshot");
    assert_eq!(snap.format_menu_bar(), "85/97");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_bridge_handles_broken_pipe_and_invalid_json_without_breaking_chained_command() {
    let dir = temp_dir("broken-pipe");
    let cache_path = dir.join("claude-statusline-cache.json");
    let bin = env!("CARGO_BIN_EXE_ebb");

    // 1. Chained command exits immediately without reading stdin (triggers EPIPE on write).
    let mut child = Command::new(bin)
        .args(["claude-statusline", "--cache-path"])
        .arg(&cache_path)
        .args(["--", "printf 'fast-exit'"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&fixture("claude_statusline_full.json"))
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "fast-exit");
    assert_eq!(
        ClaudeAdapter::new(cache_path.clone())
            .read_snapshot()
            .unwrap()
            .unwrap()
            .format_menu_bar(),
        "85/97"
    );

    // 2. Malformed stdin must NOT break chained command or overwrite good cache.
    let mut child2 = Command::new(bin)
        .args(["claude-statusline", "--cache-path"])
        .arg(&cache_path)
        .args(["--", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child2
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"not-json")
        .unwrap();
    let out2 = child2.wait_with_output().unwrap();
    assert_eq!(out2.status.code(), Some(0));
    assert_eq!(out2.stdout, b"not-json");
    assert_eq!(
        ClaudeAdapter::new(cache_path)
            .read_snapshot()
            .unwrap()
            .unwrap()
            .format_menu_bar(),
        "85/97"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn opt_in_installer_chains_existing_config_with_backup_and_idempotency() {
    let dir = temp_dir("install");
    let settings_path = dir.join("settings.json");
    let binary_path = PathBuf::from("/Applications/Ebb.app/Contents/MacOS/ebb");

    let initial = serde_json::json!({
        "model": "claude-opus-4-6",
        "theme": "dark",
        "statusLine": {
            "type": "command",
            "command": "node \"/Users/example/scripts/custom-statusline.mjs\""
        }
    });
    fs::write(
        &settings_path,
        serde_json::to_string_pretty(&initial).unwrap(),
    )
    .unwrap();

    // Dry-run plan does not modify disk.
    let dry = plan_statusline_install(&settings_path, &binary_path).unwrap();
    assert!(!dry.already_installed);
    assert_eq!(
        dry.previous_command.as_deref(),
        Some("node \"/Users/example/scripts/custom-statusline.mjs\"")
    );
    assert_eq!(
        dry.new_command,
        build_wrapped_statusline_command(&binary_path, dry.previous_command.as_deref())
    );
    assert_eq!(
        fs::read_to_string(&settings_path).unwrap(),
        serde_json::to_string_pretty(&initial).unwrap()
    );

    // Apply creates backup, preserves unrelated keys, and wraps command cleanly.
    let (applied, backup) = apply_statusline_install(&settings_path, &binary_path).unwrap();
    assert!(!applied.already_installed);
    let backup_path = backup.expect("backup created");
    assert!(backup_path.exists());

    let updated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&settings_path).unwrap()).unwrap();
    assert_eq!(updated["model"], "claude-opus-4-6");
    assert_eq!(updated["theme"], "dark");
    assert_eq!(updated["statusLine"]["type"], "command");
    assert_eq!(updated["statusLine"]["command"], applied.new_command);

    // Second apply is idempotent: no double-wrapping and no extra backup.
    let (second, second_backup) = apply_statusline_install(&settings_path, &binary_path).unwrap();
    assert!(second.already_installed);
    assert!(second_backup.is_none());
    assert_eq!(second.new_command, applied.new_command);

    // Shell quoting handles embedded single quotes cleanly.
    assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");

    // CLI arg parser handles --cache-path and -- cleanly.
    let parsed =
        StatuslineBridgeArgs::parse(["--cache-path", "/tmp/c.json", "--", "node \"/tmp/tap.mjs\""])
            .unwrap();
    assert_eq!(
        parsed.cache_path_override,
        Some(PathBuf::from("/tmp/c.json"))
    );
    assert_eq!(parsed.chained_command, vec!["node \"/tmp/tap.mjs\""]);

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn wrapped_command_executes_cleanly_through_bin_sh_end_to_end() {
    let dir = temp_dir("sh-e2e");
    let cache_path = dir.join("claude-statusline-cache.json");
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_ebb"));

    // Simulate how Claude Code executes `statusLine.command` via `/bin/sh -c`.
    let existing = "printf 'orig: '; tr 'a-z' 'A-Z' | head -c 10";
    let wrapped = format!(
        "{} claude-statusline --cache-path {} -- {}",
        shell_single_quote(&bin.to_string_lossy()),
        shell_single_quote(&cache_path.to_string_lossy()),
        shell_single_quote(existing),
    );

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&wrapped)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(&fixture("claude_statusline_full.json"))
        .unwrap();
    let out = child.wait_with_output().unwrap();

    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&out.stdout).starts_with("orig: "),
        "stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert_eq!(
        ClaudeAdapter::new(cache_path)
            .read_snapshot()
            .unwrap()
            .unwrap()
            .format_menu_bar(),
        "85/97"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_claude_sessions_never_corrupt_atomic_cache() {
    let dir = temp_dir("concurrent");
    let cache_path = dir.join("claude-statusline-cache.json");

    let handles: Vec<_> = (0..8)
        .map(|i| {
            let path = cache_path.clone();
            std::thread::spawn(move || {
                for step in 0..10u64 {
                    let snap = QuotaSnapshot {
                        provider: Provider::Claude,
                        five_hour: Some(QuotaWindow {
                            remaining_percent: (50 + (i * 3 + step as usize) % 45) as u8,
                            resets_at: Some(1_795_239_600),
                        }),
                        weekly: Some(QuotaWindow {
                            remaining_percent: 90,
                            resets_at: Some(1_795_788_000),
                        }),
                        fetched_at: 20_000 + step * 10 + i as u64,
                    };
                    write_claude_cache_atomic(&path, &snap, 0).unwrap();
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Cache must parse cleanly with no leftover temp files.
    let final_snap = ClaudeAdapter::new(cache_path.clone())
        .read_snapshot()
        .unwrap()
        .expect("valid snapshot after concurrent writes");
    assert_eq!(final_snap.provider, Provider::Claude);
    assert!(final_snap.five_hour.is_some());

    let leftover_tmp = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(!leftover_tmp, "no .tmp files may remain");

    let _ = fs::remove_dir_all(&dir);
}
