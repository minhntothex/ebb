use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;
use serde::de::DeserializeOwned;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub enum JsonStorageError {
    Io(std::io::Error),
    Json(serde_json::Error),
}

/// Read an optional JSON file; returns `Ok(None)` when the file does not exist.
pub fn read_json_optional<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, JsonStorageError> {
    let bytes = match fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(JsonStorageError::Io(e)),
    };
    let parsed = serde_json::from_slice(&bytes).map_err(JsonStorageError::Json)?;
    Ok(Some(parsed))
}

/// Atomically write JSON (`0600` on Unix) via same-directory temp file + rename.
/// Returns `Ok(false)` without touching disk if the existing file already has identical bytes.
pub fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> Result<bool, JsonStorageError> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(JsonStorageError::Json)?;
    bytes.push(b'\n');

    if let Ok(existing) = fs::read(path)
        && existing == bytes
    {
        return Ok(false);
    }

    write_bytes_atomic(path, &bytes).map_err(JsonStorageError::Io)?;
    Ok(true)
}

fn write_bytes_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path has no parent directory",
        )
    })?;
    ensure_private_dir(parent)?;

    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("data");
    let seq = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp_path: PathBuf = parent.join(format!(".{file_name}.{}.{seq}.tmp", std::process::id()));

    let write_result = (|| -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts.open(&tmp_path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp_path, path)?;
        Ok(())
    })();

    if write_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    write_result
}

fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.exists() {
        return Ok(());
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}
