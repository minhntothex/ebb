use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSWorkspace, NSWorkspaceDidWakeNotification,
    NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol};

use crate::config::{AppConfig, AppPaths};
use crate::error::{AppError, Result};
use crate::launch_agent::LaunchAgentManager;
use crate::model::{Provider, ProviderState, QuotaSnapshot, QuotaWindow};
use crate::providers::claude::ClaudeAdapter;
use crate::refresh::{
    AppState, LiveQuotaFetcher, RefreshConfig, RefreshEvent, RefreshReason, RefreshScheduler,
};
use crate::ui::menu::{DropdownMenu, MenuAction};
use crate::ui::status_bar::StatusBarView;
use crate::ui::view_model::UiSnapshot;

struct WorkspaceObserverIvars {
    scheduler: Arc<RefreshScheduler>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "EbbWorkspaceObserver"]
    #[ivars = WorkspaceObserverIvars]
    struct WorkspaceObserver;

    unsafe impl NSObjectProtocol for WorkspaceObserver {}

    impl WorkspaceObserver {
        #[unsafe(method(onWillSleep:))]
        fn on_will_sleep(&self, _notification: Option<&NSNotification>) {
            self.ivars().scheduler.set_sleeping(true);
        }

        #[unsafe(method(onDidWake:))]
        fn on_did_wake(&self, _notification: Option<&NSNotification>) {
            self.ivars().scheduler.set_sleeping(false);
        }
    }
);

impl WorkspaceObserver {
    fn register(mtm: MainThreadMarker, scheduler: Arc<RefreshScheduler>) -> Retained<Self> {
        let this = mtm
            .alloc::<Self>()
            .set_ivars(WorkspaceObserverIvars { scheduler });
        let observer: Retained<Self> = unsafe { objc2::msg_send![super(this), init] };

        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let observer_obj: &AnyObject = &observer;
        unsafe {
            center.addObserver_selector_name_object(
                observer_obj,
                sel!(onWillSleep:),
                Some(NSWorkspaceWillSleepNotification),
                None,
            );
            center.addObserver_selector_name_object(
                observer_obj,
                sel!(onDidWake:),
                Some(NSWorkspaceDidWakeNotification),
                None,
            );
        }

        observer
    }

    fn unregister(&self) {
        let center = NSWorkspace::sharedWorkspace().notificationCenter();
        let observer_obj: &AnyObject = self;
        unsafe {
            center.removeObserver(observer_obj);
        }
    }
}

struct MainThreadUi {
    paths: AppPaths,
    state: AppState,
    scheduler: Arc<RefreshScheduler>,
    ui_alive: Arc<AtomicBool>,
    workspace_observer: Retained<WorkspaceObserver>,
    status_bar: StatusBarView,
    dropdown: DropdownMenu,
}

impl MainThreadUi {
    fn render(&mut self, mtm: MainThreadMarker) {
        let snap = self.state.ui_snapshot(unix_now_secs());
        self.status_bar.apply(&snap, mtm);
        self.dropdown.apply(&snap);
    }

    fn persist_config(&self) {
        let _ = self.state.config.save_atomic(&self.paths.config_path);
    }

    fn persist_cache(&self) {
        let cache = self.state.to_usage_cache();
        let _ = self.paths.save_cache_atomic(&cache);
    }

    fn sync_scheduler_config(&self) {
        let refresh_cfg = RefreshConfig::from_app_config(
            &self.state.config,
            self.paths.claude_cache_path.clone(),
        );
        self.scheduler.update_config(refresh_cfg);
    }

    fn shutdown(&self) {
        self.ui_alive.store(false, Ordering::Release);
        self.workspace_observer.unregister();
        self.scheduler.shutdown();
        self.status_bar.remove();
    }
}

struct MainUiPtr(usize);
// SAFETY: only ever dereferenced on `DispatchQueue::main()` while `app.run()` owns the `Rc`.
unsafe impl Send for MainUiPtr {}
unsafe impl Sync for MainUiPtr {}

/// Launch the native macOS Menu Bar application (no Dock icon).
pub fn run_menu_bar_app() -> Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| AppError::InvalidConfig("AppKit UI must run on the main thread".into()))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let paths = AppPaths::default_macos()?;
    let mut config = AppConfig::load_or_default(&paths.config_path).unwrap_or_default();

    // Reflect actual LaunchAgent state on disk so Settings checkmark is always truthful.
    if let Ok(agent) = LaunchAgentManager::default_macos() {
        config.launch_at_login = agent.is_enabled();
    }

    let persisted_cache = paths.load_cache_or_empty().unwrap_or_default();
    let claude_snapshot = ClaudeAdapter::new(paths.claude_cache_path.clone())
        .read_snapshot()
        .ok()
        .flatten();
    let state = AppState::from_persisted(config, persisted_cache, claude_snapshot, unix_now_secs());
    let refresh_cfg =
        RefreshConfig::from_app_config(&state.config, paths.claude_cache_path.clone());

    let ui_cell: Rc<RefCell<Option<MainThreadUi>>> = Rc::new(RefCell::new(None));
    let ui_alive = Arc::new(AtomicBool::new(true));
    let ui_ptr = Arc::new(MainUiPtr(Rc::as_ptr(&ui_cell) as usize));

    // Single background worker thread (Condvar-based, coalesced, zero busy-looping).
    let event_ptr = Arc::clone(&ui_ptr);
    let event_alive = Arc::clone(&ui_alive);
    let scheduler = Arc::new(RefreshScheduler::start(
        refresh_cfg,
        LiveQuotaFetcher,
        move |event| {
            if !event_alive.load(Ordering::Acquire) {
                return;
            }
            let target_ptr = Arc::clone(&event_ptr);
            let alive = Arc::clone(&event_alive);
            DispatchQueue::main().exec_async(move || {
                if !alive.load(Ordering::Acquire) {
                    return;
                }
                let Some(mtm) = MainThreadMarker::new() else {
                    return;
                };
                // SAFETY: `alive` is flipped to `false` on the main thread in `shutdown()`
                // before `ui_cell` can ever be dropped, and this closure runs on the main thread.
                let cell = unsafe { &*(target_ptr.0 as *const RefCell<Option<MainThreadUi>>) };
                if let Some(ui) = cell.borrow_mut().as_mut() {
                    match event {
                        RefreshEvent::Started(_reason) => {
                            ui.state.mark_refresh_started();
                            ui.render(mtm);
                        }
                        RefreshEvent::Completed(outcome) => {
                            let cache_changed = ui.state.apply_refresh_outcome(outcome);
                            if cache_changed {
                                ui.persist_cache();
                            }
                            ui.render(mtm);
                        }
                    }
                }
            });
        },
    ));

    let workspace_observer = WorkspaceObserver::register(mtm, Arc::clone(&scheduler));

    let action_cell = Rc::clone(&ui_cell);
    let status_bar = StatusBarView::new(mtm);
    let dropdown = DropdownMenu::new(mtm, move |action| {
        handle_menu_action(&action_cell, action);
    });
    status_bar.set_menu(dropdown.ns_menu());

    let mut ui = MainThreadUi {
        paths,
        state,
        scheduler: Arc::clone(&scheduler),
        ui_alive,
        workspace_observer,
        status_bar,
        dropdown,
    };
    ui.render(mtm);
    *ui_cell.borrow_mut() = Some(ui);

    // Kick off an immediate non-blocking startup refresh on the single worker thread.
    scheduler.request_refresh(RefreshReason::Startup);

    app.run();

    // Ensure clean teardown if `app.run()` returns normally.
    if let Some(ui) = ui_cell.borrow().as_ref() {
        ui.shutdown();
    }
    Ok(())
}

fn handle_menu_action(ui_cell: &Rc<RefCell<Option<MainThreadUi>>>, action: MenuAction) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };

    match action {
        MenuAction::RefreshNow => {
            if let Some(ui) = ui_cell.borrow().as_ref() {
                ui.scheduler.request_refresh(RefreshReason::Manual);
            }
        }
        MenuAction::SetIntervalSeconds(secs) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.refresh_interval_seconds = secs;
                ui.persist_config();
                ui.sync_scheduler_config();
                ui.render(mtm);
            }
        }
        MenuAction::ToggleShowClaude => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.show_claude = !ui.state.config.show_claude;
                ui.persist_config();
                ui.sync_scheduler_config();
                ui.render(mtm);
            }
        }
        MenuAction::ToggleShowCodex => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.show_codex = !ui.state.config.show_codex;
                ui.persist_config();
                ui.sync_scheduler_config();
                ui.render(mtm);
            }
        }
        MenuAction::ToggleLaunchAtLogin => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                let desired = !ui.state.config.launch_at_login;
                let applied = (|| -> Result<()> {
                    let exe = std::env::current_exe().map_err(|source| AppError::ConfigIo {
                        path: ui.paths.config_path.clone(),
                        source,
                    })?;
                    LaunchAgentManager::default_macos()?.set_enabled(desired, &exe)
                })();
                if applied.is_ok() {
                    ui.state.config.launch_at_login = desired;
                    ui.persist_config();
                    ui.render(mtm);
                }
            }
        }
        MenuAction::OpenConfigFolder => {
            if let Some(ui) = ui_cell.borrow().as_ref() {
                let _ = std::fs::create_dir_all(&ui.paths.base_dir);
                let _ = Command::new("/usr/bin/open")
                    .arg(&ui.paths.base_dir)
                    .spawn();
            }
        }
        MenuAction::Quit => {
            if let Some(ui) = ui_cell.borrow().as_ref() {
                ui.shutdown();
            }
            let app = NSApplication::sharedApplication(mtm);
            app.terminate(None);
        }
    }
}

/// Non-blocking AppKit smoke test verifying real `NSStatusItem`, `NSMenu`, and `WorkspaceObserver` on macOS.
pub fn run_ui_smoke_test() -> Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| AppError::InvalidConfig("UI smoke test must run on main thread".into()))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let config = AppConfig::default();
    let mut claude = ProviderState::new(Provider::Claude);
    claude.apply_success(
        QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 65,
                resets_at: Some(1_795_239_600),
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 72,
                resets_at: Some(1_795_788_000),
            }),
            fetched_at: 1_000,
        },
        1_000,
    );
    let mut codex = ProviderState::new(Provider::Codex);
    codex.apply_success(
        QuotaSnapshot {
            provider: Provider::Codex,
            five_hour: Some(QuotaWindow {
                remaining_percent: 43,
                resets_at: Some(1_795_238_616),
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 31,
                resets_at: Some(1_795_767_342),
            }),
            fetched_at: 1_000,
        },
        1_000,
    );

    let snap = UiSnapshot::build(&config, &claude, &codex, false, 1_005);

    let mut status_bar = StatusBarView::new(mtm);
    let mut dropdown = DropdownMenu::new(mtm, |_| {});
    status_bar.set_menu(dropdown.ns_menu());

    // Verify WorkspaceObserver registers & unregisters cleanly without panicking.
    let dummy_cfg = RefreshConfig::from_app_config(&config, std::env::temp_dir().join("none.json"));
    let scheduler = Arc::new(RefreshScheduler::start(dummy_cfg, LiveQuotaFetcher, |_| {}));
    let observer = WorkspaceObserver::register(mtm, Arc::clone(&scheduler));
    observer.unregister();
    scheduler.shutdown();

    // First apply mutates AppKit; identical second apply is a no-op (update-on-change).
    assert!(status_bar.apply(&snap, mtm));
    assert!(!status_bar.apply(&snap, mtm));
    assert!(dropdown.apply(&snap));
    assert!(!dropdown.apply(&snap));

    let sb_info = status_bar
        .debug_inspect(mtm)
        .ok_or_else(|| AppError::InvalidConfig("NSStatusBarButton was nil".into()))?;
    let menu_info = dropdown.debug_inspect();

    assert_eq!(sb_info.title_len, 12, "expected 5 + 2(spaces) + 5 chars");
    assert!(sb_info.width_pt > 40.0 && sb_info.width_pt < 120.0);
    assert!(menu_info.top_level_items >= 8);
    assert_eq!(menu_info.claude_row_text, "● Claude\t65% / 72%");
    assert_eq!(menu_info.codex_row_text, "● Codex\t43% / 31%");

    // Verify stale/partial transition mutates cleanly.
    let partial_snap = UiSnapshot::build(&config, &claude, &codex, true, 2_500);
    assert!(status_bar.apply(&partial_snap, mtm));
    assert!(dropdown.apply(&partial_snap));

    println!(
        "UI smoke test OK: status_bar={}ptx{}pt (len={}), menu_items={}, claude={:?}, codex={:?}",
        sb_info.width_pt,
        sb_info.height_pt,
        sb_info.title_len,
        menu_info.top_level_items,
        menu_info.claude_row_text,
        menu_info.codex_row_text
    );

    status_bar.remove();
    Ok(())
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
