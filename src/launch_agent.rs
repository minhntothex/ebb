use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{AppError, Result};

/// macOS LaunchAgent label used for `Launch at Login`.
pub const LAUNCH_AGENT_LABEL: &str = "com.minhntothex.ebb";

static PLIST_TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Manages the user's macOS LaunchAgent plist (`~/Library/LaunchAgents/com.aiusagemonitor.app.plist`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchAgentManager {
    plist_path: PathBuf,
}

impl LaunchAgentManager {
    pub fn default_macos() -> Result<Self> {
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            AppError::InvalidConfig("HOME environment variable is not set".into())
        })?;
        Ok(Self::in_agents_dir(
            home.join("Library").join("LaunchAgents"),
        ))
    }

    pub fn in_agents_dir(agents_dir: PathBuf) -> Self {
        Self {
            plist_path: agents_dir.join(format!("{LAUNCH_AGENT_LABEL}.plist")),
        }
    }

    pub fn plist_path(&self) -> &Path {
        &self.plist_path
    }

    pub fn is_enabled(&self) -> bool {
        self.plist_path.is_file()
    }

    /// Enable or disable Launch at Login by atomically creating or removing the LaunchAgent plist.
    pub fn set_enabled(&self, enabled: bool, executable_path: &Path) -> Result<()> {
        if enabled {
            self.install(executable_path)
        } else {
            self.uninstall()
        }
    }

    pub fn install(&self, executable_path: &Path) -> Result<()> {
        if executable_path.as_os_str().is_empty() {
            return Err(AppError::InvalidConfig(
                "executable path for LaunchAgent cannot be empty".into(),
            ));
        }

        let content = render_launch_agent_plist(LAUNCH_AGENT_LABEL, executable_path);
        if let Ok(existing) = fs::read_to_string(&self.plist_path)
            && existing == content
        {
            return Ok(());
        }

        let parent = self.plist_path.parent().ok_or_else(|| {
            AppError::InvalidConfig("LaunchAgent plist path has no parent directory".into())
        })?;
        fs::create_dir_all(parent).map_err(|source| AppError::ConfigIo {
            path: parent.to_path_buf(),
            source,
        })?;

        let seq = PLIST_TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp_path = parent.join(format!(
            ".{LAUNCH_AGENT_LABEL}.{}.{seq}.tmp",
            std::process::id()
        ));

        let res = (|| -> std::io::Result<()> {
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o644);
            }
            let mut file = opts.open(&tmp_path)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            drop(file);
            fs::rename(&tmp_path, &self.plist_path)?;
            Ok(())
        })();

        if let Err(source) = res {
            let _ = fs::remove_file(&tmp_path);
            return Err(AppError::ConfigIo {
                path: self.plist_path.clone(),
                source,
            });
        }

        Ok(())
    }

    pub fn uninstall(&self) -> Result<()> {
        match fs::remove_file(&self.plist_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(AppError::ConfigIo {
                path: self.plist_path.clone(),
                source,
            }),
        }
    }
}

/// Render a minimal, valid macOS LaunchAgent XML property list.
pub fn render_launch_agent_plist(label: &str, executable_path: &Path) -> String {
    let escaped_label = xml_escape(label);
    let escaped_exec = xml_escape(&executable_path.to_string_lossy());
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{escaped_label}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{escaped_exec}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <false/>
    <key>ProcessType</key>
    <string>Interactive</string>
</dict>
</plist>
"#
    )
}

fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}
