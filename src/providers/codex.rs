use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{AppError, Result};
use crate::model::{Provider, QuotaSnapshot, QuotaWindow, WindowKind};

/// Default wall-clock timeout for one short-lived `codex app-server --stdio` fetch.
pub const DEFAULT_CODEX_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum size (bytes) of a single NDJSON line accepted from `codex app-server`.
pub const MAX_JSON_LINE_BYTES: usize = 1024 * 1024; // 1 MiB

/// Maximum stderr bytes retained for diagnostics.
const MAX_STDERR_BYTES: usize = 8 * 1024;

/// Graceful exit wait before `SIGKILL` after closing stdin.
const GRACEFUL_EXIT_WAIT: Duration = Duration::from_millis(150);

/// Canonical overall Codex rate-limit bucket key.
pub const CANONICAL_CODEX_LIMIT_ID: &str = "codex";

#[derive(Debug, Deserialize)]
struct RawGetRateLimitsResult {
    #[serde(default, rename = "rateLimits")]
    rate_limits: Option<RawRateLimitSnapshot>,
    #[serde(default, rename = "rateLimitsByLimitId")]
    rate_limits_by_limit_id: Option<BTreeMap<String, RawRateLimitSnapshot>>,
}

#[derive(Debug, Deserialize)]
struct RawRateLimitSnapshot {
    #[serde(default, rename = "limitId")]
    limit_id: Option<String>,
    #[serde(default)]
    primary: Option<RawRateLimitWindow>,
    #[serde(default)]
    secondary: Option<RawRateLimitWindow>,
}

#[derive(Debug, Deserialize)]
struct RawRateLimitWindow {
    #[serde(rename = "usedPercent")]
    used_percent: f64,
    #[serde(default, rename = "windowDurationMins")]
    window_duration_mins: Option<i64>,
    #[serde(default, rename = "resetsAt")]
    resets_at: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawJsonRpcError {
    #[serde(default)]
    code: i64,
    #[serde(default)]
    message: String,
}

/// Parse the `result` object of `account/rateLimits/read` (or a full JSON-RPC envelope
/// containing `"result"` or `"error"`) into a normalized `QuotaSnapshot`.
///
/// Bucket selection rule (never merges unrelated buckets):
/// 1. `rateLimitsByLimitId["codex"]` if present.
/// 2. Any entry in `rateLimitsByLimitId` whose `limitId == Some("codex")`.
/// 3. Top-level `rateLimits` (official backward-compatible single-bucket view).
///
/// Window mapping rule (never assumes `primary == 5h` or `secondary == weekly`):
/// - `windowDurationMins == 300`   -> `five_hour`
/// - `windowDurationMins == 10080` -> `weekly`
/// - any other or missing duration -> ignored (displayed as `–`, never mislabeled)
pub fn parse_rate_limits_response(json_bytes: &[u8], fetched_at: u64) -> Result<QuotaSnapshot> {
    let envelope: Value = serde_json::from_slice(json_bytes).map_err(|e| AppError::Protocol {
        provider: Provider::Codex,
        message: format!("invalid JSON from codex app-server: {e}"),
    })?;

    if let Some(err_val) = envelope.get("error").filter(|v| !v.is_null()) {
        return Err(classify_rpc_error(err_val));
    }

    let result_val = envelope.get("result").unwrap_or(&envelope);
    let parsed: RawGetRateLimitsResult =
        serde_json::from_value(result_val.clone()).map_err(|e| AppError::Protocol {
            provider: Provider::Codex,
            message: format!("unexpected account/rateLimits/read schema: {e}"),
        })?;

    let bucket = select_codex_bucket(&parsed);
    let Some(bucket) = bucket else {
        return Ok(QuotaSnapshot {
            provider: Provider::Codex,
            five_hour: None,
            weekly: None,
            fetched_at,
        });
    };

    let mut five_hour = None;
    let mut weekly = None;

    for raw_window in [bucket.primary.as_ref(), bucket.secondary.as_ref()]
        .into_iter()
        .flatten()
    {
        let Some(duration_mins) = raw_window.window_duration_mins else {
            continue;
        };
        let Some(kind) = WindowKind::from_duration_mins(duration_mins) else {
            continue;
        };
        let resets_at = raw_window.resets_at.and_then(|ts| u64::try_from(ts).ok());
        let normalized = QuotaWindow::from_used_percent(
            Provider::Codex,
            kind,
            raw_window.used_percent,
            resets_at,
        )?;
        match kind {
            WindowKind::FiveHour => {
                if five_hour.is_none() {
                    five_hour = Some(normalized);
                }
            }
            WindowKind::Weekly => {
                if weekly.is_none() {
                    weekly = Some(normalized);
                }
            }
        }
    }

    Ok(QuotaSnapshot {
        provider: Provider::Codex,
        five_hour,
        weekly,
        fetched_at,
    })
}

fn select_codex_bucket(res: &RawGetRateLimitsResult) -> Option<&RawRateLimitSnapshot> {
    if let Some(ref map) = res.rate_limits_by_limit_id {
        if let Some(exact) = map.get(CANONICAL_CODEX_LIMIT_ID) {
            return Some(exact);
        }
        if let Some(by_field) = map
            .values()
            .find(|snap| snap.limit_id.as_deref() == Some(CANONICAL_CODEX_LIMIT_ID))
        {
            return Some(by_field);
        }
    }
    res.rate_limits.as_ref()
}

fn classify_rpc_error(err_val: &Value) -> AppError {
    let parsed =
        serde_json::from_value::<RawJsonRpcError>(err_val.clone()).unwrap_or(RawJsonRpcError {
            code: 0,
            message: err_val.to_string(),
        });
    let msg_lower = parsed.message.to_lowercase();

    if msg_lower.contains("unauthenticated")
        || msg_lower.contains("not authenticated")
        || msg_lower.contains("not logged in")
        || msg_lower.contains("unauthorized")
        || msg_lower.contains("login required")
        || msg_lower.contains("sign in")
        || msg_lower.contains("401")
    {
        return AppError::Unauthenticated {
            provider: Provider::Codex,
        };
    }

    if parsed.code == -32601 || msg_lower.contains("method not found") {
        return AppError::ProviderUnavailable {
            provider: Provider::Codex,
            reason: format!(
                "codex app-server does not support account/rateLimits/read: {}",
                parsed.message
            ),
        };
    }

    AppError::Protocol {
        provider: Provider::Codex,
        message: format!("JSON-RPC error {}: {}", parsed.code, parsed.message),
    }
}

/// Resolve the `codex` binary path.
///
/// Order:
/// 1. Explicit config override (`codex_path`).
/// 2. `$PATH` lookup.
/// 3. Standard macOS fallback locations (managed daemon, Homebrew, `/usr/local/bin`,
///    `~/.local/bin`, `~/.bun/bin`, `~/.volta/bin`, `~/.nvm/versions/node/*/bin/codex`)
///    so `.app` bundles launched without shell `$PATH` still find `codex`.
pub fn resolve_codex_binary(
    override_path: Option<&str>,
    path_env: Option<&OsStr>,
    home_dir: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(raw) = override_path.map(str::trim).filter(|s| !s.is_empty()) {
        let p = expand_tilde(raw, home_dir);
        if is_executable_file(&p) {
            return Ok(p);
        }
        return Err(AppError::ProviderUnavailable {
            provider: Provider::Codex,
            reason: format!("configured codex_path is not executable: {}", p.display()),
        });
    }

    if let Some(paths) = path_env {
        for dir in std::env::split_paths(paths) {
            let candidate = dir.join("codex");
            if is_executable_file(&candidate) {
                return Ok(candidate);
            }
        }
    }

    let mut fallbacks = Vec::new();
    if let Some(home) = home_dir {
        fallbacks.push(
            home.join(".codex")
                .join("packages")
                .join("app-server-daemon")
                .join("current")
                .join("bin")
                .join("codex"),
        );
        fallbacks.push(home.join(".local").join("bin").join("codex"));
        fallbacks.push(home.join(".bun").join("bin").join("codex"));
        fallbacks.push(home.join(".volta").join("bin").join("codex"));
    }
    fallbacks.push(PathBuf::from("/opt/homebrew/bin/codex"));
    fallbacks.push(PathBuf::from("/usr/local/bin/codex"));

    for candidate in fallbacks {
        if is_executable_file(&candidate) {
            return Ok(candidate);
        }
    }

    if let Some(home) = home_dir
        && let Some(nvm_codex) = find_latest_nvm_codex(home)
    {
        return Ok(nvm_codex);
    }

    Err(AppError::ProviderUnavailable {
        provider: Provider::Codex,
        reason: "codex executable not found in PATH or standard macOS locations".into(),
    })
}

fn expand_tilde(raw: &str, home_dir: Option<&Path>) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = home_dir
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

fn find_latest_nvm_codex(home_dir: &Path) -> Option<PathBuf> {
    let nvm_root = home_dir.join(".nvm").join("versions").join("node");
    let entries = fs::read_dir(nvm_root).ok()?;
    let mut versions: Vec<(Vec<u64>, PathBuf)> = Vec::new();

    for entry in entries.flatten() {
        let dir_name = entry.file_name();
        let name_str = dir_name.to_string_lossy();
        let candidate = entry.path().join("bin").join("codex");
        if is_executable_file(&candidate) {
            let key = parse_semver_key(&name_str);
            versions.push((key, candidate));
        }
    }

    versions.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    versions.pop().map(|(_, path)| path)
}

fn parse_semver_key(name: &str) -> Vec<u64> {
    name.trim_start_matches('v')
        .split('.')
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

fn is_executable_file(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Short-lived `codex app-server --stdio` client.
#[derive(Debug, Clone)]
pub struct CodexAdapter {
    binary_override: Option<String>,
    timeout: Duration,
    max_line_bytes: usize,
}

impl CodexAdapter {
    pub fn new(binary_override: Option<String>) -> Self {
        Self {
            binary_override,
            timeout: DEFAULT_CODEX_TIMEOUT,
            max_line_bytes: MAX_JSON_LINE_BYTES,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn with_max_line_bytes(mut self, max_line_bytes: usize) -> Self {
        self.max_line_bytes = max_line_bytes;
        self
    }

    /// Resolve binary using current process environment.
    pub fn resolve_binary(&self) -> Result<PathBuf> {
        let path_env = std::env::var_os("PATH");
        let home_dir = std::env::var_os("HOME").map(PathBuf::from);
        resolve_codex_binary(
            self.binary_override.as_deref(),
            path_env.as_deref(),
            home_dir.as_deref(),
        )
    }

    /// Spawn a short-lived `codex app-server --stdio`, perform the JSON-RPC handshake,
    /// query `account/rateLimits/read`, and always terminate & reap the child process.
    pub fn fetch_snapshot(&self) -> Result<QuotaSnapshot> {
        let binary = self.resolve_binary()?;
        self.fetch_from_binary(&binary, &["app-server", "--stdio"])
    }

    /// Lower-level entrypoint allowing tests to pass a custom executable and args.
    pub fn fetch_from_binary(&self, binary: &Path, args: &[&str]) -> Result<QuotaSnapshot> {
        let mut child = Command::new(binary)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| AppError::Subprocess {
                provider: Provider::Codex,
                message: format!("failed to spawn {}: {e}", binary.display()),
            })?;

        let mut child_stdin = child.stdin.take().ok_or_else(|| AppError::Subprocess {
            provider: Provider::Codex,
            message: "failed to capture codex stdin pipe".into(),
        })?;
        let child_stdout = child.stdout.take().ok_or_else(|| AppError::Subprocess {
            provider: Provider::Codex,
            message: "failed to capture codex stdout pipe".into(),
        })?;
        let child_stderr = child.stderr.take().ok_or_else(|| AppError::Subprocess {
            provider: Provider::Codex,
            message: "failed to capture codex stderr pipe".into(),
        })?;

        let mut guard = ChildReaper::new(child);
        let max_line_bytes = self.max_line_bytes;

        // Drain stderr concurrently (bounded) so the child can never block on a full stderr pipe.
        let stderr_handle =
            thread::spawn(move || drain_bounded_stderr(child_stderr, MAX_STDERR_BYTES));

        // Read bounded NDJSON lines concurrently so the main worker can enforce a strict wall-clock deadline.
        let (tx, rx) = mpsc::channel::<LineEvent>();
        let stdout_handle = thread::spawn(move || {
            let mut reader = BufReader::new(child_stdout);
            loop {
                match read_bounded_line(&mut reader, max_line_bytes) {
                    Ok(Some(line)) => {
                        if tx.send(LineEvent::Line(line)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(LineEvent::Eof);
                        break;
                    }
                    Err(LineReadError::TooLong) => {
                        let _ = tx.send(LineEvent::TooLong);
                        break;
                    }
                    Err(LineReadError::Io(e)) => {
                        let _ = tx.send(LineEvent::Io(e));
                        break;
                    }
                }
            }
        });

        let deadline = Instant::now() + self.timeout;
        let timeout_secs = self.timeout.as_secs().max(1);

        let protocol_result = (|| -> Result<QuotaSnapshot> {
            // 1. Send `initialize` request (id = 1)
            let init_req = json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "clientInfo": {
                        "name": "ebb",
                        "version": crate::APP_VERSION,
                    },
                    "capabilities": {
                        "experimentalApi": false,
                    }
                }
            });
            write_ndjson(&mut child_stdin, &init_req)?;

            // Wait for initialize response (id = 1), ignoring interleaved notifications.
            let init_resp = read_response_for_id(&rx, 1, deadline, timeout_secs)?;
            if let Some(err_val) = init_resp.get("error").filter(|v| !v.is_null()) {
                return Err(classify_rpc_error(err_val));
            }

            // 2. Send `initialized` notification
            let initialized_notif = json!({ "method": "initialized" });
            write_ndjson(&mut child_stdin, &initialized_notif)?;

            // 3. Send `account/rateLimits/read` request (id = 2)
            let read_req = json!({
                "id": 2,
                "method": "account/rateLimits/read",
                "params": {
                    "excludeResetCreditDetails": true,
                }
            });
            write_ndjson(&mut child_stdin, &read_req)?;

            // Wait for response (id = 2), skipping notifications like `configWarning`.
            let rl_resp = read_response_for_id(&rx, 2, deadline, timeout_secs)?;
            let raw = serde_json::to_vec(&rl_resp)?;
            parse_rate_limits_response(&raw, unix_now_secs())
        })();

        // Always close stdin first, then terminate & reap child BEFORE joining reader threads.
        drop(child_stdin);
        let graceful = protocol_result.is_ok();
        guard.cleanup(graceful);

        let _ = stdout_handle.join();
        let stderr_snippet = stderr_handle.join().unwrap_or_default();

        // Enrich unexpected EOF/subprocess errors with concise stderr diagnostics (never secrets).
        match protocol_result {
            Ok(snap) => Ok(snap),
            Err(AppError::Subprocess { provider, message }) if !stderr_snippet.is_empty() => {
                Err(AppError::Subprocess {
                    provider,
                    message: format!("{message}; stderr: {}", summarize_stderr(&stderr_snippet)),
                })
            }
            Err(other) => Err(other),
        }
    }
}

enum LineEvent {
    Line(Vec<u8>),
    Eof,
    TooLong,
    Io(io::Error),
}

enum LineReadError {
    TooLong,
    Io(io::Error),
}

fn read_bounded_line<R: BufRead>(
    reader: &mut R,
    max_bytes: usize,
) -> std::result::Result<Option<Vec<u8>>, LineReadError> {
    let mut out = Vec::new();
    loop {
        let available = reader.fill_buf().map_err(LineReadError::Io)?;
        if available.is_empty() {
            return if out.is_empty() {
                Ok(None)
            } else {
                Ok(Some(out))
            };
        }

        if let Some(nl_pos) = available.iter().position(|&b| b == b'\n') {
            let take = nl_pos + 1;
            if out.len() + take > max_bytes {
                reader.consume(take);
                return Err(LineReadError::TooLong);
            }
            out.extend_from_slice(&available[..nl_pos]);
            if out.last() == Some(&b'\r') {
                out.pop();
            }
            reader.consume(take);
            return Ok(Some(out));
        }

        let len = available.len();
        if out.len() + len > max_bytes {
            reader.consume(len);
            return Err(LineReadError::TooLong);
        }
        out.extend_from_slice(available);
        reader.consume(len);
    }
}

fn write_ndjson<W: Write>(writer: &mut W, payload: &Value) -> Result<()> {
    let mut line = serde_json::to_vec(payload)?;
    line.push(b'\n');
    writer.write_all(&line).map_err(|e| AppError::Subprocess {
        provider: Provider::Codex,
        message: format!("failed writing JSON-RPC request to codex stdin: {e}"),
    })?;
    writer.flush().map_err(|e| AppError::Subprocess {
        provider: Provider::Codex,
        message: format!("failed flushing codex stdin: {e}"),
    })
}

fn read_response_for_id(
    rx: &mpsc::Receiver<LineEvent>,
    target_id: u64,
    deadline: Instant,
    timeout_secs: u64,
) -> Result<Value> {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return Err(AppError::Timeout {
                provider: Provider::Codex,
                timeout_secs,
            });
        }
        let remaining = deadline.saturating_duration_since(now);
        match rx.recv_timeout(remaining) {
            Ok(LineEvent::Line(bytes)) => {
                if bytes.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                let msg: Value =
                    serde_json::from_slice(&bytes).map_err(|e| AppError::Protocol {
                        provider: Provider::Codex,
                        message: format!("non-JSON line from codex app-server: {e}"),
                    })?;

                if msg.get("id").and_then(Value::as_u64) == Some(target_id) {
                    return Ok(msg);
                }
                // Ignore server notifications (`method` without matching `id`) and other IDs.
            }
            Ok(LineEvent::Eof) => {
                return Err(AppError::Subprocess {
                    provider: Provider::Codex,
                    message: format!(
                        "codex app-server exited before replying to request id={target_id}"
                    ),
                });
            }
            Ok(LineEvent::TooLong) => {
                return Err(AppError::Protocol {
                    provider: Provider::Codex,
                    message: "codex app-server response exceeded maximum JSON line size".into(),
                });
            }
            Ok(LineEvent::Io(e)) => {
                return Err(AppError::Subprocess {
                    provider: Provider::Codex,
                    message: format!("error reading codex stdout: {e}"),
                });
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(AppError::Timeout {
                    provider: Provider::Codex,
                    timeout_secs,
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(AppError::Subprocess {
                    provider: Provider::Codex,
                    message: "codex stdout reader disconnected unexpectedly".into(),
                });
            }
        }
    }
}

fn drain_bounded_stderr<R: Read>(mut reader: R, max_bytes: usize) -> String {
    let mut kept = Vec::new();
    let mut buf = [0u8; 1024];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if kept.len() < max_bytes {
                    let take = n.min(max_bytes - kept.len());
                    kept.extend_from_slice(&buf[..take]);
                }
            }
        }
    }
    String::from_utf8_lossy(&kept).trim().to_owned()
}

fn summarize_stderr(stderr: &str) -> String {
    let compact: String = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(3)
        .collect::<Vec<_>>()
        .join(" | ");
    if compact.chars().count() > 240 {
        let prefix: String = compact.chars().take(240).collect();
        format!("{prefix}...")
    } else {
        compact
    }
}

/// RAII child-process reaper guaranteeing no leaked subprocesses or zombies on any exit path.
struct ChildReaper {
    child: Option<Child>,
}

impl ChildReaper {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn cleanup(&mut self, allow_graceful: bool) {
        let Some(mut child) = self.child.take() else {
            return;
        };

        if allow_graceful {
            let start = Instant::now();
            while start.elapsed() < GRACEFUL_EXIT_WAIT {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => thread::sleep(Duration::from_millis(10)),
                    Err(_) => break,
                }
            }
        }

        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for ChildReaper {
    fn drop(&mut self) {
        self.cleanup(false);
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
