use std::path::PathBuf;

use thiserror::Error;

use crate::model::{Provider, WindowKind};

/// Typed application errors across config, cache, parsing, and provider adapters.
#[derive(Debug, Error)]
pub enum AppError {
    #[error("failed to read or write config at {path}: {source}")]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse config JSON at {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("failed to read or write cache at {path}: {source}")]
    CacheIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse cache JSON at {path}: {source}")]
    CacheParse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("failed to serialize JSON: {0}")]
    Serialize(#[from] serde_json::Error),

    #[error(
        "invalid {window} percentage for {provider}: {value} (expected finite number in 0..=100)"
    )]
    InvalidPercentage {
        provider: Provider,
        window: WindowKind,
        value: f64,
    },

    #[error("{provider} is not authenticated")]
    Unauthenticated { provider: Provider },

    #[error("{provider} is unavailable: {reason}")]
    ProviderUnavailable { provider: Provider, reason: String },

    #[error("{provider} timed out after {timeout_secs}s")]
    Timeout {
        provider: Provider,
        timeout_secs: u64,
    },

    #[error("{provider} subprocess error: {message}")]
    Subprocess { provider: Provider, message: String },

    #[error("{provider} protocol error: {message}")]
    Protocol { provider: Provider, message: String },
}

pub type Result<T> = std::result::Result<T, AppError>;
