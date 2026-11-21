/// Human-facing CalVer release string (`XX.Y[.Z]`, `Y` starts at `0`).
pub const APP_VERSION: &str = "26.2";

/// Display name used in macOS UI and Application Support directory.
pub const APP_NAME: &str = "Ebb";

pub mod alerts;
#[cfg(target_os = "macos")]
pub mod app;
pub mod config;
pub mod error;
pub mod launch_agent;
pub mod model;
pub mod providers;
pub mod refresh;
pub mod storage;
pub mod ui;
pub mod updates;
#[cfg(target_os = "macos")]
pub mod watcher;
