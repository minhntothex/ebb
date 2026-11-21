use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::error::{AppError, Result};
use crate::model::{Provider, QuotaSnapshot, QuotaWindow, WindowKind};
use crate::storage::{JsonStorageError, read_json_optional, write_json_atomic};

/// Maximum stdin payload size read by the status-line bridge (4 MiB).
pub const MAX_STATUSLINE_STDIN_BYTES: usize = 4 * 1024 * 1024;

/// Minimum interval (seconds) before rewriting identical quota values just to bump `fetched_at`.
pub const MIN_IDENTICAL_REWRITE_SECS: u64 = 60;

/// Subcommand token used to detect already-wrapped Claude status-line configs.
pub const CLAUDE_STATUSLINE_SUBCOMMAND: &str = "claude-statusline";

#[derive(Debug, Deserialize)]
struct RawStatusLinePayload {
    #[serde(default)]
    rate_limits: Option<RawRateLimits>,
}

#[derive(Debug, Deserialize)]
struct RawRateLimits {
    #[serde(default)]
    five_hour: Option<RawQuotaWindow>,
    #[serde(default)]
    seven_day: Option<RawQuotaWindow>,
    // `spend_limit` (gateway-only) is intentionally ignored for MVP v1.
}

#[derive(Debug, Deserialize)]
struct RawQuotaWindow {
    #[serde(default)]
    used_percentage: Option<f64>,
    #[serde(default, deserialize_with = "deserialize_optional_epoch_secs")]
    resets_at: Option<u64>,
}

fn deserialize_optional_epoch_secs<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<u64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let opt = Option::<Value>::deserialize(deserializer)?;
    match opt {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => {
            if let Some(u) = n.as_u64() {
                Ok(Some(u))
            } else if let Some(f) = n.as_f64()
                && f.is_finite()
                && f >= 0.0
            {
                Ok(Some(f.round() as u64))
            } else {
                Ok(None)
            }
        }
        Some(_) => Ok(None),
    }
}

/// Parse Claude Code status-line JSON bytes into a normalized `QuotaSnapshot`.
///
/// Returns `Ok(None)` when `rate_limits` or both `five_hour`/`seven_day` windows are absent
/// (for example before the first API response or on gateway-only spend limits).
/// Returns `Err` on malformed JSON or out-of-range percentages.
pub fn parse_statusline_json(bytes: &[u8], observed_at: u64) -> Result<Option<QuotaSnapshot>> {
    let payload: RawStatusLinePayload =
        serde_json::from_slice(bytes).map_err(|e| AppError::Protocol {
            provider: Provider::Claude,
            message: format!("invalid status-line JSON: {e}"),
        })?;

    let Some(limits) = payload.rate_limits else {
        return Ok(None);
    };

    let five_hour = match limits
        .five_hour
        .and_then(|w| w.used_percentage.map(|used| (used, w.resets_at)))
    {
        Some((used, resets_at)) => Some(QuotaWindow::from_used_percent(
            Provider::Claude,
            WindowKind::FiveHour,
            used,
            resets_at,
        )?),
        None => None,
    };

    let weekly = match limits
        .seven_day
        .and_then(|w| w.used_percentage.map(|used| (used, w.resets_at)))
    {
        Some((used, resets_at)) => Some(QuotaWindow::from_used_percent(
            Provider::Claude,
            WindowKind::Weekly,
            used,
            resets_at,
        )?),
        None => None,
    };

    if five_hour.is_none() && weekly.is_none() {
        return Ok(None);
    }

    Ok(Some(QuotaSnapshot {
        provider: Provider::Claude,
        five_hour,
        weekly,
        fetched_at: observed_at,
    }))
}

/// Persist a Claude snapshot into its dedicated atomic cache file.
///
/// Multi-session coordination rules:
/// - Never overwrite a newer snapshot (`existing.fetched_at > snapshot.fetched_at`).
/// - Avoid redundant disk writes when windows are unchanged within `min_rewrite_secs`.
/// - Uses atomic `tmp + sync_all + rename` with `0600` permissions.
pub fn write_claude_cache_atomic(
    cache_path: &Path,
    snapshot: &QuotaSnapshot,
    min_rewrite_secs: u64,
) -> Result<bool> {
    if snapshot.provider != Provider::Claude || !snapshot.has_any_window() {
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

/// Read-only adapter used by the Menu Bar refresh coordinator.
/// Never invokes Claude CLI or creates model requests.
#[derive(Debug, Clone)]
pub struct ClaudeAdapter {
    cache_path: PathBuf,
}

impl ClaudeAdapter {
    pub fn new(cache_path: PathBuf) -> Self {
        Self { cache_path }
    }

    pub fn cache_path(&self) -> &Path {
        &self.cache_path
    }

    /// Read the latest cached Claude snapshot, or `Ok(None)` if no cache file exists yet.
    pub fn read_snapshot(&self) -> Result<Option<QuotaSnapshot>> {
        match read_json_optional::<QuotaSnapshot>(&self.cache_path) {
            Ok(Some(snap)) => {
                if snap.provider != Provider::Claude {
                    return Err(AppError::Protocol {
                        provider: Provider::Claude,
                        message: format!("unexpected provider `{}` in Claude cache", snap.provider),
                    });
                }
                Ok(Some(snap))
            }
            Ok(None) => Ok(None),
            Err(JsonStorageError::Io(source)) => Err(AppError::CacheIo {
                path: self.cache_path.clone(),
                source,
            }),
            Err(JsonStorageError::Json(source)) => Err(AppError::CacheParse {
                path: self.cache_path.clone(),
                source,
            }),
        }
    }
}

/// Parsed CLI options for `ebb claude-statusline`.
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
            return Err(format!("unknown argument for claude-statusline: `{arg}`"));
        }

        Ok(Self {
            cache_path_override,
            chained_command,
        })
    }
}

/// Execute the `claude-statusline` bridge:
/// 1. Read stdin once (up to `MAX_STATUSLINE_STDIN_BYTES`).
/// 2. If a chained command was supplied after `--`, spawn it immediately with full stdin
///    and inherited stdout/stderr so original status-line behavior is preserved verbatim.
/// 3. Parse `rate_limits` and update the atomic cache without ever writing to stdout.
/// 4. Return the chained command's exit code (or `0` when running standalone).
pub fn run_statusline_bridge(cache_path: &Path, chained_command: &[String]) -> i32 {
    let stdin_bytes = read_bounded_stdin(MAX_STATUSLINE_STDIN_BYTES);
    let now = unix_now_secs();

    if chained_command.is_empty() {
        tap_bytes_silently(&stdin_bytes, cache_path, now);
        return 0;
    }

    let mut child = match spawn_chained_command(chained_command) {
        Ok(c) => c,
        Err(_) => {
            tap_bytes_silently(&stdin_bytes, cache_path, now);
            return 127;
        }
    };

    if let Some(mut child_stdin) = child.stdin.take() {
        // Ignore BrokenPipe if the chained command exits without consuming stdin.
        let _ = child_stdin.write_all(&stdin_bytes);
        let _ = child_stdin.flush();
    }

    // Perform cache update while the chained command runs for minimal extra latency.
    tap_bytes_silently(&stdin_bytes, cache_path, now);

    match child.wait() {
        Ok(status) => exit_code_from_status(status),
        Err(_) => 1,
    }
}

fn tap_bytes_silently(stdin_bytes: &[u8], cache_path: &Path, observed_at: u64) {
    if stdin_bytes.is_empty() {
        return;
    }
    if let Ok(Some(snapshot)) = parse_statusline_json(stdin_bytes, observed_at) {
        let _ = write_claude_cache_atomic(cache_path, &snapshot, MIN_IDENTICAL_REWRITE_SECS);
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

/// Build the wrapped Claude `statusLine.command` string, chaining any existing command verbatim.
pub fn build_wrapped_statusline_command(
    binary_path: &Path,
    existing_command: Option<&str>,
) -> String {
    let bin = shell_single_quote(&binary_path.to_string_lossy());
    match existing_command.map(str::trim).filter(|s| !s.is_empty()) {
        Some(existing) => format!(
            "{bin} {CLAUDE_STATUSLINE_SUBCOMMAND} -- {}",
            shell_single_quote(existing)
        ),
        None => format!("{bin} {CLAUDE_STATUSLINE_SUBCOMMAND}"),
    }
}

/// Result of planning an opt-in Claude `settings.json` statusLine update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatuslineInstallPlan {
    pub settings_path: PathBuf,
    pub previous_command: Option<String>,
    pub new_command: String,
    pub already_installed: bool,
}

/// Inspect Claude's `settings.json` without modifying it and produce an opt-in install plan.
pub fn plan_statusline_install(
    settings_path: &Path,
    binary_path: &Path,
) -> Result<StatuslineInstallPlan> {
    let root = read_settings_object(settings_path)?;
    let previous_command = extract_existing_statusline_command(&root);

    if let Some(ref prev) = previous_command
        && is_already_wrapped(prev)
    {
        return Ok(StatuslineInstallPlan {
            settings_path: settings_path.to_path_buf(),
            previous_command: Some(prev.clone()),
            new_command: prev.clone(),
            already_installed: true,
        });
    }

    let new_command = build_wrapped_statusline_command(binary_path, previous_command.as_deref());
    Ok(StatuslineInstallPlan {
        settings_path: settings_path.to_path_buf(),
        previous_command,
        new_command,
        already_installed: false,
    })
}

/// Apply an opt-in Claude `settings.json` update, creating a timestamped backup first.
/// Preserves all unrelated keys in `settings.json` and is idempotent when already installed.
pub fn apply_statusline_install(
    settings_path: &Path,
    binary_path: &Path,
) -> Result<(StatuslineInstallPlan, Option<PathBuf>)> {
    let plan = plan_statusline_install(settings_path, binary_path)?;
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

    let mut status_line = match root.remove("statusLine") {
        Some(Value::Object(map)) => map,
        _ => Map::new(),
    };
    status_line.insert("type".into(), Value::String("command".into()));
    status_line.insert("command".into(), Value::String(plan.new_command.clone()));
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

fn is_already_wrapped(command: &str) -> bool {
    // Detect `... claude-statusline` either standalone or followed by ` -- ...`
    command
        .split_whitespace()
        .any(|tok| tok == CLAUDE_STATUSLINE_SUBCOMMAND)
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
