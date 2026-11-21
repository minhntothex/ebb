use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};

use serde::Deserialize;

/// GitHub Releases API endpoint for manual update checks.
pub const GITHUB_RELEASES_API_URL: &str =
    "https://api.github.com/repos/minhntothex/ebb/releases/latest";

/// Canonical GitHub Releases web page fallback URL.
pub const GITHUB_RELEASES_PAGE_URL: &str = "https://github.com/minhntothex/ebb/releases";

/// Maximum response body read from GitHub API (256 KiB).
const MAX_RELEASE_RESPONSE_BYTES: usize = 256 * 1024;

/// Calendar version `XX.Y(.Z)` where `XX` is 2-digit year, `Y` starts at 0, and `Z` is optional.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CalVer {
    pub year: u32,
    pub release: u32,
    pub patch: u32,
}

impl CalVer {
    /// Parse `XX.Y` or `XX.Y.Z` (accepts optional leading `v`). Rejects prerelease suffixes.
    pub fn parse(raw: &str) -> Option<Self> {
        let trimmed = raw.trim();
        let stripped = trimmed
            .strip_prefix('v')
            .or_else(|| trimmed.strip_prefix('V'))
            .unwrap_or(trimmed);
        if stripped.is_empty() {
            return None;
        }

        let parts: Vec<&str> = stripped.split('.').collect();
        if !(2..=3).contains(&parts.len()) {
            return None;
        }
        if parts
            .iter()
            .any(|p| p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }

        let year = parts[0].parse::<u32>().ok()?;
        let release = parts[1].parse::<u32>().ok()?;
        let patch = if parts.len() == 3 {
            parts[2].parse::<u32>().ok()?
        } else {
            0
        };

        Some(Self {
            year,
            release,
            patch,
        })
    }
}

impl fmt::Display for CalVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.patch == 0 {
            write!(f, "{}.{}", self.year, self.release)
        } else {
            write!(f, "{}.{}.{}", self.year, self.release, self.patch)
        }
    }
}

/// Outcome of a manual `Check for Updates…` lookup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheckResult {
    UpToDate {
        current: String,
        latest: String,
    },
    UpdateAvailable {
        current: String,
        latest: String,
        release_url: String,
    },
}

#[derive(Debug, Deserialize)]
struct GithubLatestRelease {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Parse GitHub `/releases/latest` JSON and compare against `current_version`.
pub fn parse_github_release_json(
    json_bytes: &[u8],
    current_version: &str,
) -> Result<UpdateCheckResult, String> {
    let current = CalVer::parse(current_version)
        .ok_or_else(|| format!("invalid current version `{current_version}`"))?;

    let payload: GithubLatestRelease = serde_json::from_slice(json_bytes)
        .map_err(|e| format!("invalid GitHub release JSON: {e}"))?;

    if payload.draft || payload.prerelease {
        return Ok(UpdateCheckResult::UpToDate {
            current: current.to_string(),
            latest: current.to_string(),
        });
    }

    let latest = CalVer::parse(&payload.tag_name)
        .ok_or_else(|| format!("unrecognized release tag `{}`", payload.tag_name))?;

    if latest > current {
        let release_url = payload
            .html_url
            .filter(|u| u.starts_with("https://github.com/"))
            .unwrap_or_else(|| GITHUB_RELEASES_PAGE_URL.to_owned());
        Ok(UpdateCheckResult::UpdateAvailable {
            current: current.to_string(),
            latest: latest.to_string(),
            release_url,
        })
    } else {
        Ok(UpdateCheckResult::UpToDate {
            current: current.to_string(),
            latest: latest.to_string(),
        })
    }
}

/// Perform a manual, bounded HTTPS check against GitHub Releases using macOS `/usr/bin/curl`.
pub fn check_latest_release(current_version: &str) -> Result<UpdateCheckResult, String> {
    let mut child = Command::new("/usr/bin/curl")
        .args([
            "-fsSL",
            "--max-time",
            "5",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            &format!("User-Agent: ebb/{current_version}"),
            GITHUB_RELEASES_API_URL,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch curl: {e}"))?;

    let mut body = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        let mut limited = stdout.take((MAX_RELEASE_RESPONSE_BYTES + 1) as u64);
        limited
            .read_to_end(&mut body)
            .map_err(|e| format!("failed reading update response: {e}"))?;
    }
    let mut err_buf = Vec::new();
    if let Some(stderr) = child.stderr.take() {
        let _ = stderr.take(4096).read_to_end(&mut err_buf);
    }

    let status = child
        .wait()
        .map_err(|e| format!("failed waiting for curl: {e}"))?;
    if !status.success() {
        let msg = String::from_utf8_lossy(&err_buf).trim().to_owned();
        return Err(if msg.is_empty() {
            format!("GitHub update check failed (exit {})", status)
        } else {
            format!("GitHub update check failed: {msg}")
        });
    }
    if body.len() > MAX_RELEASE_RESPONSE_BYTES {
        return Err("update response exceeded size limit".to_owned());
    }

    parse_github_release_json(&body, current_version)
}
