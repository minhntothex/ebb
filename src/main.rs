use std::path::PathBuf;

use ebb::APP_VERSION;
use ebb::config::{AppConfig, AppPaths};
use ebb::providers::claude::{
    CLAUDE_STATUSLINE_SUBCOMMAND, ClaudeAdapter, StatuslineBridgeArgs, apply_statusline_install,
    plan_statusline_install, run_statusline_bridge,
};
use ebb::providers::codex::CodexAdapter;

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version") | Some("-V") => {
            println!("ebb {APP_VERSION}");
        }
        Some("--ui-smoke-test") => {
            #[cfg(target_os = "macos")]
            {
                if let Err(e) = ebb::app::run_ui_smoke_test() {
                    eprintln!("UI smoke test failed: {e}");
                    std::process::exit(1);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                eprintln!("UI smoke test is only supported on macOS");
                std::process::exit(1);
            }
        }
        Some(CLAUDE_STATUSLINE_SUBCOMMAND) => {
            let parsed = match StatuslineBridgeArgs::parse(args) {
                Ok(p) => p,
                Err(_) => std::process::exit(2),
            };
            let cache_path = match parsed.cache_path_override {
                Some(p) => p,
                None => match AppPaths::default_macos() {
                    Ok(paths) => paths.claude_cache_path,
                    Err(_) => std::process::exit(0),
                },
            };
            let code = run_statusline_bridge(&cache_path, &parsed.chained_command);
            std::process::exit(code);
        }
        Some("setup-claude-statusline") => {
            let apply = args.any(|a| a == "--apply");
            let settings_path = default_claude_settings_path();
            let binary_path = match std::env::current_exe() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("failed to resolve current binary path: {e}");
                    std::process::exit(1);
                }
            };
            if apply {
                match apply_statusline_install(&settings_path, &binary_path) {
                    Ok((plan, backup)) => {
                        if plan.already_installed {
                            println!(
                                "Claude statusLine is already configured: {}",
                                plan.new_command
                            );
                        } else {
                            if let Some(b) = backup {
                                println!("Backup saved to: {}", b.display());
                            }
                            println!("Updated {}:", plan.settings_path.display());
                            println!("  statusLine.command = {}", plan.new_command);
                        }
                    }
                    Err(e) => {
                        eprintln!("failed to update Claude settings: {e}");
                        std::process::exit(1);
                    }
                }
            } else {
                match plan_statusline_install(&settings_path, &binary_path) {
                    Ok(plan) => {
                        println!(
                            "Dry run (pass --apply to write {}):",
                            plan.settings_path.display()
                        );
                        if let Some(prev) = plan.previous_command {
                            println!("  current: {prev}");
                        } else {
                            println!("  current: <none>");
                        }
                        println!("  planned: {}", plan.new_command);
                        if plan.already_installed {
                            println!("  status:  already installed");
                        }
                    }
                    Err(e) => {
                        eprintln!("failed to inspect Claude settings: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Some("probe") => {
            let paths = match AppPaths::default_macos() {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("failed to resolve paths: {e}");
                    std::process::exit(1);
                }
            };
            let cfg = AppConfig::load_or_default(&paths.config_path).unwrap_or_default();

            match ClaudeAdapter::new(paths.claude_cache_path).read_snapshot() {
                Ok(Some(snap)) => println!(
                    "Claude: {} (dropdown: {}, fetched_at={})",
                    snap.format_menu_bar(),
                    snap.format_dropdown(),
                    snap.fetched_at
                ),
                Ok(None) => println!("Claude: –/– (no status-line cache yet)"),
                Err(e) => println!("Claude: error ({e})"),
            }

            let codex = CodexAdapter::new(cfg.codex_path);
            match codex.fetch_snapshot() {
                Ok(snap) => println!(
                    "Codex:  {} (dropdown: {}, fetched_at={})",
                    snap.format_menu_bar(),
                    snap.format_dropdown(),
                    snap.fetched_at
                ),
                Err(e) => println!("Codex:  error ({e})"),
            }
        }
        Some(other) => {
            eprintln!("ebb: unknown subcommand `{other}`");
            std::process::exit(2);
        }
        None => {
            #[cfg(target_os = "macos")]
            {
                if let Err(e) = ebb::app::run_menu_bar_app() {
                    eprintln!("ebb failed to start: {e}");
                    std::process::exit(1);
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                eprintln!("ebb Menu Bar UI is only supported on macOS");
                std::process::exit(1);
            }
        }
    }
}

fn default_claude_settings_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".claude").join("settings.json")
}
