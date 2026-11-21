use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::model::{DisplayMetric, MenuBarWindows, Provider, UsageCache};
use crate::storage::{JsonStorageError, read_json_optional, write_json_atomic};

pub const MIN_REFRESH_INTERVAL_SECS: u64 = 15;
pub const MAX_REFRESH_INTERVAL_SECS: u64 = 3600;
pub const DEFAULT_REFRESH_INTERVAL_SECS: u64 = 60;
pub const DEFAULT_CLAUDE_STALE_AFTER_SECS: u64 = 900; // 15m (passive status-line source)
pub const DEFAULT_CODEX_STALE_AFTER_SECS: u64 = 180; // 3m (active poll source)
pub const DEFAULT_AGY_STALE_AFTER_SECS: u64 = 900; // 15m (passive status-line source)
pub const DEFAULT_SNAPSHOT_TTL_SECS: u64 = 21_600; // 6h max cache visibility
pub const DEFAULT_SHOW_AGY: bool = false;
pub const DEFAULT_SHOW_RESET_TIME: bool = true;
pub const DEFAULT_HIGHLIGHT_CRITICAL: bool = true;
pub const DEFAULT_ALERTS_ENABLED: bool = true;
pub const DEFAULT_NOTIFY_ON_REFRESH: bool = true;
pub const DEFAULT_ALERT_WARNING_THRESHOLD: u8 = 20;
pub const DEFAULT_ALERT_CRITICAL_THRESHOLD: u8 = 5;

/// Preset warning thresholds (remaining %) exposed in Settings submenu.
pub const WARNING_THRESHOLD_PRESETS: [u8; 5] = [30, 25, 20, 15, 10];

/// Preset critical thresholds (remaining %) exposed in Settings submenu.
pub const CRITICAL_THRESHOLD_PRESETS: [u8; 3] = [10, 5, 3];

/// Curated color themes for Menu Bar and dropdown provider dots.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColorTheme {
    #[default]
    Default,
    HighContrast,
    Monochrome,
}

/// Optional `#RRGGBB` Dark/Light hex overrides for a single provider in `config.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HexColorOverride {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dark: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub light: Option<String>,
}

/// Parse a strict `#RRGGBB` ASCII hex string into `(r, g, b)`.
pub fn parse_hex_rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let bytes = hex.as_bytes();
    if bytes.len() != 7 || bytes[0] != b'#' {
        return None;
    }
    let r = u8::from_str_radix(&hex[1..3], 16).ok()?;
    let g = u8::from_str_radix(&hex[3..5], 16).ok()?;
    let b = u8::from_str_radix(&hex[5..7], 16).ok()?;
    Some((r, g, b))
}

/// Normalize a user-supplied provider order by deduplicating and appending any missing providers.
pub fn normalize_provider_order(order: &[Provider]) -> Vec<Provider> {
    let mut normalized = Vec::with_capacity(Provider::ALL.len());
    for &p in order {
        if !normalized.contains(&p) {
            normalized.push(p);
        }
    }
    for p in Provider::ALL {
        if !normalized.contains(&p) {
            normalized.push(p);
        }
    }
    normalized
}

/// Canonical `config.json` schema.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub refresh_interval_seconds: u64,
    pub show_claude: bool,
    pub show_codex: bool,
    pub show_agy: bool,
    pub launch_at_login: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub codex_path: Option<String>,
    pub claude_stale_after_seconds: u64,
    pub codex_stale_after_seconds: u64,
    pub agy_stale_after_seconds: u64,
    pub snapshot_ttl_seconds: u64,
    pub show_reset_time: bool,
    pub display_metric: DisplayMetric,
    pub menu_bar_windows: MenuBarWindows,
    pub highlight_critical: bool,
    pub provider_order: Vec<Provider>,
    pub color_theme: ColorTheme,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provider_colors: BTreeMap<String, HexColorOverride>,
    pub alerts_enabled: bool,
    pub notify_on_refresh: bool,
    pub alert_warning_threshold: u8,
    pub alert_critical_threshold: u8,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            refresh_interval_seconds: DEFAULT_REFRESH_INTERVAL_SECS,
            show_claude: true,
            show_codex: true,
            show_agy: DEFAULT_SHOW_AGY,
            launch_at_login: false,
            codex_path: None,
            claude_stale_after_seconds: DEFAULT_CLAUDE_STALE_AFTER_SECS,
            codex_stale_after_seconds: DEFAULT_CODEX_STALE_AFTER_SECS,
            agy_stale_after_seconds: DEFAULT_AGY_STALE_AFTER_SECS,
            snapshot_ttl_seconds: DEFAULT_SNAPSHOT_TTL_SECS,
            show_reset_time: DEFAULT_SHOW_RESET_TIME,
            display_metric: DisplayMetric::Remaining,
            menu_bar_windows: MenuBarWindows::Both,
            highlight_critical: DEFAULT_HIGHLIGHT_CRITICAL,
            provider_order: Provider::ALL.to_vec(),
            color_theme: ColorTheme::Default,
            provider_colors: BTreeMap::new(),
            alerts_enabled: DEFAULT_ALERTS_ENABLED,
            notify_on_refresh: DEFAULT_NOTIFY_ON_REFRESH,
            alert_warning_threshold: DEFAULT_ALERT_WARNING_THRESHOLD,
            alert_critical_threshold: DEFAULT_ALERT_CRITICAL_THRESHOLD,
        }
    }
}

impl AppConfig {
    pub fn is_provider_visible(&self, provider: Provider) -> bool {
        match provider {
            Provider::Claude => self.show_claude,
            Provider::Codex => self.show_codex,
            Provider::Agy => self.show_agy,
        }
    }

    pub fn stale_after_seconds(&self, provider: Provider) -> u64 {
        match provider {
            Provider::Claude => self.claude_stale_after_seconds,
            Provider::Codex => self.codex_stale_after_seconds,
            Provider::Agy => self.agy_stale_after_seconds,
        }
    }

    /// Return a deduplicated, complete ordering covering all providers.
    pub fn normalized_provider_order(&self) -> Vec<Provider> {
        normalize_provider_order(&self.provider_order)
    }

    /// Move `provider` to the front of `provider_order`, preserving the relative order of others.
    pub fn move_provider_first(&mut self, provider: Provider) {
        let mut next = Vec::with_capacity(Provider::ALL.len());
        next.push(provider);
        for p in self.normalized_provider_order() {
            if p != provider {
                next.push(p);
            }
        }
        self.provider_order = next;
    }

    /// Update warning threshold while preserving `1 <= critical < warning <= 100`.
    pub fn set_warning_threshold(&mut self, warning: u8) -> Result<()> {
        if !(2..=100).contains(&warning) {
            return Err(AppError::InvalidConfig(format!(
                "alert_warning_threshold must be in 2..=100, got {warning}"
            )));
        }
        self.alert_warning_threshold = warning;
        if self.alert_critical_threshold >= warning {
            self.alert_critical_threshold = CRITICAL_THRESHOLD_PRESETS
                .iter()
                .copied()
                .find(|&c| c < warning)
                .unwrap_or_else(|| warning.saturating_sub(1).max(1));
        }
        self.validate()
    }

    /// Update critical threshold while preserving `1 <= critical < warning`.
    pub fn set_critical_threshold(&mut self, critical: u8) -> Result<()> {
        if critical == 0 || critical >= self.alert_warning_threshold {
            return Err(AppError::InvalidConfig(format!(
                "alert_critical_threshold must be in 1..{}, got {critical}",
                self.alert_warning_threshold
            )));
        }
        self.alert_critical_threshold = critical;
        self.validate()
    }

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
        if self.agy_stale_after_seconds == 0 {
            return Err(AppError::InvalidConfig(
                "agy_stale_after_seconds must be > 0".into(),
            ));
        }
        if self.snapshot_ttl_seconds == 0 {
            return Err(AppError::InvalidConfig(
                "snapshot_ttl_seconds must be > 0".into(),
            ));
        }
        for (key, override_pair) in &self.provider_colors {
            if !Provider::ALL.iter().any(|p| p.key() == key) {
                return Err(AppError::InvalidConfig(format!(
                    "unknown provider in provider_colors: `{key}`"
                )));
            }
            if let Some(ref hex) = override_pair.dark
                && parse_hex_rgb(hex).is_none()
            {
                return Err(AppError::InvalidConfig(format!(
                    "invalid dark hex color for `{key}`: `{hex}` (expected #RRGGBB)"
                )));
            }
            if let Some(ref hex) = override_pair.light
                && parse_hex_rgb(hex).is_none()
            {
                return Err(AppError::InvalidConfig(format!(
                    "invalid light hex color for `{key}`: `{hex}` (expected #RRGGBB)"
                )));
            }
        }
        if self.alert_critical_threshold == 0 || self.alert_critical_threshold > 99 {
            return Err(AppError::InvalidConfig(format!(
                "alert_critical_threshold must be in 1..=99, got {}",
                self.alert_critical_threshold
            )));
        }
        if self.alert_warning_threshold < 2 || self.alert_warning_threshold > 100 {
            return Err(AppError::InvalidConfig(format!(
                "alert_warning_threshold must be in 2..=100, got {}",
                self.alert_warning_threshold
            )));
        }
        if self.alert_critical_threshold >= self.alert_warning_threshold {
            return Err(AppError::InvalidConfig(format!(
                "alert_critical_threshold ({}) must be less than alert_warning_threshold ({})",
                self.alert_critical_threshold, self.alert_warning_threshold
            )));
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
        let mut cfg: Self = match read_json_optional(path) {
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
        cfg.provider_order = cfg.normalized_provider_order();
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
    pub agy_cache_path: PathBuf,
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
            agy_cache_path: base_dir.join("agy-statusline-cache.json"),
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
