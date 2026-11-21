use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::model::UsageCache;
use crate::storage::{JsonStorageError, read_json_optional, write_json_atomic};

pub const MIN_REFRESH_INTERVAL_SECS: u64 = 15;
pub const MAX_REFRESH_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 60;
pub const DEFAULT_CLAUDE_STALE_AFTER_SECS: u64 = 900; // 15m (passive status-line source)
pub const DEFAULT_CODEX_STALE_AFTER_SECS: u64 = 180; // 3m (active poll source)
pub const DEFAULT_SNAPSHOT_TTL_SECS: u64 = 21_600; // 6h max cache visibility

/// Canonical `config.json` schema for MVP v1.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub refresh_interval_seconds: u64,
    pub show_claude: bool,
    pub show_codex: bool,
    pub launch_at_login: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_path: Option<String>,
    pub claude_stale_after_seconds: u64,
    pub codex_stale_after_seconds: u64,
    pub snapshot_ttl_seconds: u64,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_interval_seconds: DEFAULT_REFRESH_INTERVAL_SECS,
            show_claude: true,
            show_codex: true,
            launch_at_login: false,
            codex_path: None,
            claude_stale_after_seconds: DEFAULT_CLAUDE_STALE_AFTER_SECS,
            codex_stale_after_seconds: DEFAULT_CODEX_STALE_AFTER_SECS,
            snapshot_ttl_seconds: DEFAULT_SNAPSHOT_TTL_SECS,
        }
    }
}

impl AppConfig {
    /// Validate configuration invariants.
    pub fn validate(&self) -> Result<()> {
        if !(MIN_REFRESH_INTERVAL_SECS..=MAX_REFRESH_INTERVAL_SECS)
            .contains(&self.refresh_interval_seconds)
        {
            return Err(AppError::InvalidConfig(format!(
                "refresh_interval_seconds must be in {MIN_REFRESH_INTERVAL_SECS}..={MAX_REFRESH_INTERVAL_SECS}, got {}",
                self.refresh_interval_seconds
            )));
        }
        if self.claude_stale_after_seconds == 0 {
            return Err(AppError::InvalidConfig(
                "claude_stale_after_seconds must be > 0".into(),
            ));
        }
        if self.codex_stale_after_seconds == 0 {
            return Err(AppError::InvalidConfig(
                "codex_stale_after_seconds must be > 0".into(),
            ));
        }
        if self.snapshot_ttl_seconds == 0 {
            return Err(AppError::InvalidConfig(
                "snapshot_ttl_seconds must be > 0".into(),
            ));
        }
        if let Some(ref p) = self.codex_path
            && p.trim().is_empty()
        {
            return Err(AppError::InvalidConfig(
                "codex_path cannot be empty when set".into(),
            ));
        }
        Ok(())
    }

    /// Load `config.json` if present, otherwise return validated defaults.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        let cfg: Self = match read_json_optional(path) {
            Ok(Some(c)) => c,
            Ok(None) => Self::default(),
            Err(JsonStorageError::Io(source)) => {
                return Err(AppError::ConfigIo {
                    path: path.to_path_buf(),
                    source,
                });
            }
            Err(JsonStorageError::Json(source)) => {
                return Err(AppError::ConfigParse {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        cfg.validate()?;
        Ok(cfg)
    }

    /// Validate and atomically save `config.json`. Returns `true` if disk was written.
    pub fn save_atomic(&self, path: &Path) -> Result<bool> {
        self.validate()?;
        write_json_atomic(path, self).map_err(|err| match err {
            JsonStorageError::Io(source) => AppError::ConfigIo {
                path: path.to_path_buf(),
                source,
            },
            JsonStorageError::Json(source) => AppError::Serialize(source),
        })
    }
}

/// Application Support directory paths (`~/Library/Application Support/Ebb`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub base_dir: PathBuf,
    pub config_path: PathBuf,
    pub cache_path: PathBuf,
    pub claude_cache_path: PathBuf,
    pub logs_dir: PathBuf,
}

impl AppPaths {
    pub const APP_DIR_NAME: &'static str = "Ebb";

    /// Resolve standard macOS Application Support paths from `$HOME`.
    pub fn default_macos() -> Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            AppError::InvalidConfig("HOME environment variable is not set".into())
        })?;
        Ok(Self::from_base_dir(
            home.join("Library")
                .join("Application Support")
                .join(Self::APP_DIR_NAME),
        ))
    }

    /// Construct paths under an explicit base directory (useful for tests or custom overrides).
    pub fn from_base_dir(base_dir: PathBuf) -> Self {
        Self {
            config_path: base_dir.join("config.json"),
            cache_path: base_dir.join("usage-cache.json"),
            claude_cache_path: base_dir.join("claude-statusline-cache.json"),
            logs_dir: base_dir.join("logs"),
            base_dir,
        }
    }

    /// Load persisted `usage-cache.json`, returning an empty cache if missing.
    pub fn load_cache_or_empty(&self) -> Result<UsageCache> {
        match read_json_optional(&self.cache_path) {
            Ok(Some(cache)) => Ok(cache),
            Ok(None) => Ok(UsageCache::empty()),
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

    /// Atomically persist `usage-cache.json`. Returns `true` if content changed and was written.
    pub fn save_cache_atomic(&self, cache: &UsageCache) -> Result<bool> {
        write_json_atomic(&self.cache_path, cache).map_err(|err| match err {
            JsonStorageError::Io(source) => AppError::CacheIo {
                path: self.cache_path.clone(),
                source,
            },
            JsonStorageError::Json(source) => AppError::Serialize(source),
        })
    }
}
