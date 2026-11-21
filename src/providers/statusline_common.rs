use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value};

use crate::error::{AppError, Result};
use crate::model::{Provider, QuotaSnapshot};
use crate::storage::{JsonStorageError, read_json_optional, write_json_atomic};

/// Maximum stdin payload size read by a status-line bridge (4 MiB).
pub const MAX_STATUSLINE_STDIN_BYTES: usize = 4 * 1024 * 1024;

/// Minimum interval (seconds) before rewriting identical quota values just to bump `fetched_at`.
pub const MIN_IDENTICAL_REWRITE_SECS: u64 = 60;

/// Parsed CLI options shared by `ebb claude-statusline` and `ebb agy-statusline`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineBridgeArgs {
    pub cache_path_override: Option<PathBuf>,
    pub chained_command: Vec<String>,
}

impl StatuslineBridgeArgs {
    pub fn parse<I, S>(args: I) -> std::result::Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut iter = args.into_iter().map(Into::into);
        let mut cache_path_override = None;
        let mut chained_command = Vec::new();

        while let Some(arg) = iter.next() {
            if arg == "--" {
                chained_command.extend(iter);
                break;
            }
            if arg == "--cache-path" {
                let value = iter
                    .next()
                    .ok_or_else(|| "--cache-path requires a file path".to_owned())?;
                cache_path_override = Some(PathBuf::from(value));
                continue;
            }
            return Err(format!("unknown argument for status-line bridge: `{arg}`"));
        }

        Ok(Self {
            cache_path_override,
            chained_command,
        })
    }
}

/// Persist a provider snapshot into its dedicated atomic `0600` cache file.
///
/// Multi-session coordination rules:
/// - Never overwrite a newer snapshot (`existing.fetched_at > snapshot.fetched_at`).
/// - Avoid redundant disk writes when windows are unchanged within `min_rewrite_secs`.
/// - Uses atomic `tmp + sync_all + rename` with `0600` permissions.
pub fn write_provider_cache_atomic(
    cache_path: &Path,
    expected_provider: Provider,
    snapshot: &QuotaSnapshot,
    min_rewrite_secs: u64,
) -> Result<bool> {
    if snapshot.provider != expected_provider || !snapshot.has_any_window() {
        return Ok(false);
    }

    if let Ok(Some(existing)) = read_json_optional::<QuotaSnapshot>(cache_path) {
        if existing.fetched_at > snapshot.fetched_at {
            return Ok(false);
        }
        let same_windows =
            existing.five_hour == snapshot.five_hour && existing.weekly == snapshot.weekly;
        if same_windows
            && snapshot.fetched_at.saturating_sub(existing.fetched_at) < min_rewrite_secs
        {
            return Ok(false);
        }
    }

    write_json_atomic(cache_path, snapshot).map_err(|err| match err {
        JsonStorageError::Io(source) => AppError::CacheIo {
            path: cache_path.to_path_buf(),
            source,
        },
        JsonStorageError::Json(source) => AppError::Serialize(source),
    })
}

/// Read the latest cached snapshot for `expected_provider`, or `Ok(None)` if missing.
pub fn read_provider_cache(
    cache_path: &Path,
    expected_provider: Provider,
) -> Result<Option<QuotaSnapshot>> {
    match read_json_optional::<QuotaSnapshot>(cache_path) {
        Ok(Some(snap)) => {
            if snap.provider != expected_provider {
                return Err(AppError::Protocol {
                    provider: expected_provider,
                    message: format!(
                        "unexpected provider `{}` in {} cache",
                        snap.provider, expected_provider
                    ),
                });
            }
            Ok(Some(snap))
        }
        Ok(None) => Ok(None),
        Err(JsonStorageError::Io(source)) => Err(AppError::CacheIo {
            path: cache_path.to_path_buf(),
            source,
        }),
        Err(JsonStorageError::Json(source)) => Err(AppError::CacheParse {
            path: cache_path.to_path_buf(),
            source,
        }),
    }
}

/// Execute a transparent status-line bridge:
/// 1. Read stdin once (up to `MAX_STATUSLINE_STDIN_BYTES`).
/// 2. If a chained command was supplied after `--`, spawn it immediately with full stdin
///    and inherited stdout/stderr so original status-line behavior is preserved verbatim.
/// 3. Parse quota data via `parse_fn` and update the atomic cache without writing to stdout.
/// 4. Return the chained command's exit code (or `0` when running standalone).
pub fn run_bridge_with_parser<F>(
    cache_path: &Path,
    expected_provider: Provider,
    chained_command: &[String],
    parse_fn: F,
) -> i32
where
    F: Fn(&[u8], u64) -> Result<Option<QuotaSnapshot>>,
{
    let stdin_bytes = read_bounded_stdin(MAX_STATUSLINE_STDIN_BYTES);
    let now = unix_now_secs();

    if chained_command.is_empty() {
        tap_bytes_silently(&stdin_bytes, cache_path, expected_provider, now, &parse_fn);
        return 0;
    }

    let mut child = match spawn_chained_command(chained_command) {
        Ok(c) => c,
        Err(_) => {
            tap_bytes_silently(&stdin_bytes, cache_path, expected_provider, now, &parse_fn);
            return 127;
        }
    };

    if let Some(mut child_stdin) = child.stdin.take() {
        // Ignore BrokenPipe if the chained command exits without consuming stdin.
        let _ = child_stdin.write_all(&stdin_bytes);
        let _ = child_stdin.flush();
    }

    // Perform cache update while the chained command runs for minimal extra latency.
    tap_bytes_silently(&stdin_bytes, cache_path, expected_provider, now, &parse_fn);

    match child.wait() {
        Ok(status) => exit_code_from_status(status),
        Err(_) => 1,
    }
}

fn tap_bytes_silently<F>(
    stdin_bytes: &[u8],
    cache_path: &Path,
    expected_provider: Provider,
    observed_at: u64,
    parse_fn: &F,
) where
    F: Fn(&[u8], u64) -> Result<Option<QuotaSnapshot>>,
{
    if stdin_bytes.is_empty() {
        return;
    }
    if let Ok(Some(snapshot)) = parse_fn(stdin_bytes, observed_at) {
        let _ = write_provider_cache_atomic(
            cache_path,
            expected_provider,
            &snapshot,
            MIN_IDENTICAL_REWRITE_SECS,
        );
    }
}

fn read_bounded_stdin(max_bytes: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    let stdin = io::stdin();
    let mut handle = stdin.lock().take(max_bytes as u64);
    let _ = handle.read_to_end(&mut buf);
    buf
}

fn spawn_chained_command(chained_command: &[String]) -> io::Result<std::process::Child> {
    let mut cmd = if chained_command.len() == 1 {
        let mut sh = Command::new("/bin/sh");
        sh.arg("-c").arg(&chained_command[0]);
        sh
    } else {
        let mut direct = Command::new(&chained_command[0]);
        direct.args(&chained_command[1..]);
        direct
    };
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
}

fn exit_code_from_status(status: std::process::ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            return 128 + sig;
        }
    }
    1
}

/// Safely quote an argument for `/bin/sh` using POSIX single quotes.
pub fn shell_single_quote(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('\'');
    for ch in input.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Build a wrapped `<ebb> <subcommand> [-- '<existing>']` command string.
pub fn build_wrapped_command(
    binary_path: &Path,
    subcommand: &str,
    existing_command: Option<&str>,
) -> String {
    let bin = shell_single_quote(&binary_path.to_string_lossy());
    match existing_command.map(str::trim).filter(|s| !s.is_empty()) {
        Some(existing) => format!("{bin} {subcommand} -- {}", shell_single_quote(existing)),
        None => format!("{bin} {subcommand}"),
    }
}

/// Result of planning an opt-in `settings.json` statusLine update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineInstallPlan {
    pub settings_path: PathBuf,
    pub previous_command: Option<String>,
    pub new_command: String,
    pub already_installed: bool,
}

/// Inspect a CLI `settings.json` file without modifying it and produce an opt-in install plan.
pub fn plan_install(
    settings_path: &Path,
    binary_path: &Path,
    subcommand: &str,
) -> Result<StatuslineInstallPlan> {
    let root = read_settings_object(settings_path)?;
    let previous_command = extract_existing_statusline_command(&root);

    if let Some(ref prev) = previous_command
        && is_already_wrapped(prev, subcommand)
    {
        return Ok(StatuslineInstallPlan {
            settings_path: settings_path.to_path_buf(),
            previous_command: Some(prev.clone()),
            new_command: prev.clone(),
            already_installed: true,
        });
    }

    let new_command = build_wrapped_command(binary_path, subcommand, previous_command.as_deref());
    Ok(StatuslineInstallPlan {
        settings_path: settings_path.to_path_buf(),
        previous_command,
        new_command,
        already_installed: false,
    })
}

/// Apply an opt-in `settings.json` statusLine update with a timestamped `0600` backup.
/// Preserves all unrelated keys in `settings.json` and is idempotent when already installed.
pub fn apply_install<F>(
    settings_path: &Path,
    binary_path: &Path,
    subcommand: &str,
    customize_statusline: F,
) -> Result<(StatuslineInstallPlan, Option<PathBuf>)>
where
    F: FnOnce(&mut Map<String, Value>, bool),
{
    let plan = plan_install(settings_path, binary_path, subcommand)?;
    if plan.already_installed {
        return Ok((plan, None));
    }

    let mut root = read_settings_object(settings_path)?;
    let backup_path = if settings_path.exists() {
        let backup = backup_path_for(settings_path, unix_now_secs());
        fs::copy(settings_path, &backup).map_err(|source| AppError::ConfigIo {
            path: backup.clone(),
            source,
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&backup, fs::Permissions::from_mode(0o600));
        }
        Some(backup)
    } else {
        None
    };

    let had_existing_command = plan.previous_command.is_some();
    let mut status_line = match root.remove("statusLine") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    status_line.insert("type".into(), Value::String("command".into()));
    status_line.insert("command".into(), Value::String(plan.new_command.clone()));
    customize_statusline(&mut status_line, had_existing_command);
    root.insert("statusLine".into(), Value::Object(status_line));

    write_json_atomic(settings_path, &Value::Object(root)).map_err(|err| match err {
        JsonStorageError::Io(source) => AppError::ConfigIo {
            path: settings_path.to_path_buf(),
            source,
        },
        JsonStorageError::Json(source) => AppError::Serialize(source),
    })?;

    Ok((plan, backup_path))
}

/// Default Claude Code settings path (`~/.claude/settings.json`).
pub fn default_claude_settings_path() -> PathBuf {
    home_dir().join(".claude").join("settings.json")
}

/// Default Antigravity CLI settings path (`~/.gemini/antigravity-cli/settings.json`).
pub fn default_agy_settings_path() -> PathBuf {
    home_dir()
        .join(".gemini")
        .join("antigravity-cli")
        .join("settings.json")
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn read_settings_object(settings_path: &Path) -> Result<Map<String, Value>> {
    match read_json_optional::<Value>(settings_path) {
        Ok(None) => Ok(Map::new()),
        Ok(Some(Value::Object(map))) => Ok(map),
        Ok(Some(_)) => Err(AppError::InvalidConfig(format!(
            "expected JSON object in {}",
            settings_path.display()
        ))),
        Err(JsonStorageError::Io(source)) => Err(AppError::ConfigIo {
            path: settings_path.to_path_buf(),
            source,
        }),
        Err(JsonStorageError::Json(source)) => Err(AppError::ConfigParse {
            path: settings_path.to_path_buf(),
            source,
        }),
    }
}

fn extract_existing_statusline_command(root: &Map<String, Value>) -> Option<String> {
    let status_line = root.get("statusLine")?.as_object()?;
    status_line
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn is_already_wrapped(command: &str, subcommand: &str) -> bool {
    command.split_whitespace().any(|tok| tok == subcommand)
}

fn backup_path_for(settings_path: &Path, now_secs: u64) -> PathBuf {
    let file_name = settings_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("settings.json");
    settings_path.with_file_name(format!("{file_name}.ebb-backup-{now_secs}"))
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
