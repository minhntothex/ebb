use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use dispatch2::DispatchQueue;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSWorkspace, NSWorkspaceDidWakeNotification, NSWorkspaceWillSleepNotification,
};
use objc2_foundation::{NSNotification, NSObject, NSObjectProtocol, NSString};

use crate::APP_VERSION;
use crate::alerts::{AlertKind, AlertLevel, AlertTracker, QuotaAlert};
use crate::config::{AppConfig, AppPaths};
use crate::error::{AppError, Result};
use crate::launch_agent::LaunchAgentManager;
use crate::model::{Provider, ProviderState, QuotaSnapshot, QuotaWindow, WindowKind};
use crate::providers::claude::ClaudeAdapter;
use crate::refresh::{
    AppState, LiveQuotaFetcher, RefreshConfig, RefreshEvent, RefreshReason, RefreshScheduler,
};
use crate::ui::menu::{DropdownMenu, MenuAction};
use crate::ui::notifications::NotificationSender;
use crate::ui::status_bar::StatusBarView;
use crate::ui::view_model::UiSnapshot;
use crate::updates::{UpdateCheckResult, check_latest_release};
use crate::watcher::ClaudeCacheWatcher;

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
    alert_tracker: AlertTracker,
    notifier: NotificationSender,
    scheduler: Arc<RefreshScheduler>,
    claude_watcher: Option<ClaudeCacheWatcher>,
    update_check_in_flight: Arc<AtomicBool>,
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

    fn evaluate_and_notify(
        &mut self,
        fresh_claude: Option<&QuotaSnapshot>,
        fresh_codex: Option<&QuotaSnapshot>,
        now_secs: u64,
    ) {
        let mut alerts = Vec::new();
        if let Some(snap) = fresh_claude
            && !snap.is_stale(now_secs, self.state.config.claude_stale_after_seconds)
        {
            alerts.extend(
                self.alert_tracker
                    .evaluate(snap, &self.state.config, now_secs),
            );
        }
        if let Some(snap) = fresh_codex {
            alerts.extend(
                self.alert_tracker
                    .evaluate(snap, &self.state.config, now_secs),
            );
        }
        for alert in alerts {
            self.notifier.send(&alert, now_secs);
        }
    }

    fn shutdown(&self) {
        self.ui_alive.store(false, Ordering::Release);
        self.workspace_observer.unregister();
        if let Some(ref watcher) = self.claude_watcher {
            watcher.shutdown();
        }
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

    let now = unix_now_secs();
    let persisted_cache = paths.load_cache_or_empty().unwrap_or_default();
    let claude_snapshot = ClaudeAdapter::new(paths.claude_cache_path.clone())
        .read_snapshot()
        .ok()
        .flatten();
    let state = AppState::from_persisted(config, persisted_cache, claude_snapshot, now);

    // Seed alert baseline from cached snapshots so launching Ebb never replays old alerts.
    let mut alert_tracker = AlertTracker::new();
    alert_tracker.seed_baseline(
        state.claude.snapshot.as_ref(),
        state.codex.snapshot.as_ref(),
        &state.config,
        now,
    );
    let mut notifier = NotificationSender::new();
    if state.config.alerts_enabled {
        notifier.ensure_authorized();
    }

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
                        RefreshEvent::Started(reason) => {
                            if reason.fetches_codex() {
                                ui.state.mark_refresh_started();
                                ui.render(mtm);
                            }
                        }
                        RefreshEvent::Completed(outcome) => {
                            let completed_at = outcome.completed_at;
                            let fresh_claude = outcome
                                .claude
                                .as_ref()
                                .and_then(|res| res.as_ref().ok())
                                .and_then(|opt| opt.clone());
                            let fresh_codex = outcome
                                .codex
                                .as_ref()
                                .and_then(|res| res.as_ref().ok())
                                .cloned();

                            let cache_changed = ui.state.apply_refresh_outcome(outcome);
                            if cache_changed {
                                ui.persist_cache();
                            }
                            ui.evaluate_and_notify(
                                fresh_claude.as_ref(),
                                fresh_codex.as_ref(),
                                completed_at,
                            );
                            ui.render(mtm);
                        }
                    }
                }
            });
        },
    ));

    let workspace_observer = WorkspaceObserver::register(mtm, Arc::clone(&scheduler));

    // Zero-CPU macOS kqueue watcher for instant Claude status-line updates.
    let watch_sched = Arc::clone(&scheduler);
    let claude_watcher = ClaudeCacheWatcher::start(paths.claude_cache_path.clone(), move || {
        watch_sched.request_refresh(RefreshReason::ClaudeCacheChanged);
    })
    .ok();

    let action_cell = Rc::clone(&ui_cell);
    let status_bar = StatusBarView::new(mtm);
    let dropdown = DropdownMenu::new(mtm, move |action| {
        handle_menu_action(&action_cell, action);
    });
    status_bar.set_menu(dropdown.ns_menu());

    let mut ui = MainThreadUi {
        paths,
        state,
        alert_tracker,
        notifier,
        scheduler: Arc::clone(&scheduler),
        claude_watcher,
        update_check_in_flight: Arc::new(AtomicBool::new(false)),
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
        MenuAction::ToggleShowResetTime => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.show_reset_time = !ui.state.config.show_reset_time;
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::ToggleAlerts => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.alerts_enabled = !ui.state.config.alerts_enabled;
                if ui.state.config.alerts_enabled {
                    ui.notifier.ensure_authorized();
                    ui.alert_tracker.seed_baseline(
                        ui.state.claude.snapshot.as_ref(),
                        ui.state.codex.snapshot.as_ref(),
                        &ui.state.config,
                        unix_now_secs(),
                    );
                }
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::ToggleNotifyOnRefresh => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.notify_on_refresh = !ui.state.config.notify_on_refresh;
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::SetWarningThreshold(pct) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut()
                && ui.state.config.set_warning_threshold(pct).is_ok()
            {
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::SetCriticalThreshold(pct) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut()
                && ui.state.config.set_critical_threshold(pct).is_ok()
            {
                ui.persist_config();
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
        MenuAction::CheckForUpdates => {
            let in_flight = ui_cell
                .borrow()
                .as_ref()
                .map(|ui| Arc::clone(&ui.update_check_in_flight));
            if let Some(flag) = in_flight
                && flag
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
            {
                let done_flag = Arc::clone(&flag);
                thread::spawn(move || {
                    let result = check_latest_release(APP_VERSION);
                    DispatchQueue::main().exec_async(move || {
                        done_flag.store(false, Ordering::Release);
                        if let Some(mtm) = MainThreadMarker::new() {
                            present_update_dialog(mtm, result);
                        }
                    });
                });
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

/// Non-blocking AppKit smoke test verifying real `NSStatusItem`, `NSMenu`, reset sub-lines,
/// `WorkspaceObserver`, and bundle-guarded `NotificationSender` on macOS.
pub fn run_ui_smoke_test() -> Result<()> {
    let mtm = MainThreadMarker::new()
        .ok_or_else(|| AppError::InvalidConfig("UI smoke test must run on main thread".into()))?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let now = 1_005;
    let mut config = AppConfig::default();
    let mut claude = ProviderState::new(Provider::Claude);
    claude.apply_success(
        QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 65,
                resets_at: Some(now + 2 * 3600 + 15 * 60),
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 72,
                resets_at: Some(now + 4 * 86_400 + 6 * 3600),
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
                resets_at: Some(now + 48 * 60),
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 31,
                resets_at: Some(now + 5 * 86_400 + 11 * 3600),
            }),
            fetched_at: 1_000,
        },
        1_000,
    );

    let snap = UiSnapshot::build(&config, &claude, &codex, false, now);

    let mut status_bar = StatusBarView::new(mtm);
    let mut dropdown = DropdownMenu::new(mtm, |_| {});
    status_bar.set_menu(dropdown.ns_menu());

    // Verify WorkspaceObserver registers & unregisters cleanly without panicking.
    let dummy_cfg = RefreshConfig::from_app_config(&config, std::env::temp_dir().join("none.json"));
    let scheduler = Arc::new(RefreshScheduler::start(dummy_cfg, LiveQuotaFetcher, |_| {}));
    let observer = WorkspaceObserver::register(mtm, Arc::clone(&scheduler));
    observer.unregister();
    scheduler.shutdown();

    // Verify NotificationSender safely no-ops when invoked outside a `.app` bundle.
    let mut notifier = NotificationSender::new();
    if !notifier.is_bundle_available() {
        notifier.send(
            &QuotaAlert {
                provider: Provider::Claude,
                window: WindowKind::FiveHour,
                kind: AlertKind::Low(AlertLevel::Warning),
                remaining_percent: 15,
                resets_at: Some(now + 3600),
            },
            now,
        );
        notifier.send(
            &QuotaAlert {
                provider: Provider::Claude,
                window: WindowKind::FiveHour,
                kind: AlertKind::Refreshed,
                remaining_percent: 100,
                resets_at: Some(now + 18_000),
            },
            now,
        );
    }

    // Verify ClaudeCacheWatcher starts and shuts down cleanly without leaks.
    let smoke_watch_path = std::env::temp_dir().join("ebb-smoke-claude-cache.json");
    if let Ok(watcher) = ClaudeCacheWatcher::start(smoke_watch_path, || {}) {
        watcher.shutdown();
    }

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
    assert!(menu_info.top_level_items >= 10);
    assert_eq!(menu_info.header_right_text, "Updated just now");
    assert_eq!(menu_info.claude_row_text, "● Claude\t65% / 72%");
    assert_eq!(
        menu_info.claude_reset_text.as_deref(),
        Some("Reset in\t2h 15m / 4d 6h")
    );
    assert_eq!(menu_info.codex_row_text, "● Codex\t43% / 31%");
    assert_eq!(
        menu_info.codex_reset_text.as_deref(),
        Some("Reset in\t48m / 5d 11h")
    );
    // Verify exact vertical alignment of provider name == `Reset in` and flush-right values.
    assert!((menu_info.provider_name_x - menu_info.reset_label_x).abs() < 0.01);
    assert!((menu_info.provider_value_right_x - menu_info.reset_value_right_x).abs() < 0.01);

    // Verify hiding reset times hides sub-lines cleanly.
    config.show_reset_time = false;
    let hidden_reset_snap = UiSnapshot::build(&config, &claude, &codex, false, now);
    assert!(dropdown.apply(&hidden_reset_snap));
    let hidden_info = dropdown.debug_inspect();
    assert!(hidden_info.claude_reset_text.is_none());
    assert!(hidden_info.codex_reset_text.is_none());

    // Verify stale/partial transition mutates cleanly.
    let partial_snap = UiSnapshot::build(&config, &claude, &codex, true, 2_500);
    assert!(status_bar.apply(&partial_snap, mtm));
    assert!(dropdown.apply(&partial_snap));

    println!(
        "UI smoke test OK: status_bar={}ptx{}pt (len={}), menu_items={}, header_right={:?}, claude={:?}, claude_reset={:?}, codex={:?}",
        sb_info.width_pt,
        sb_info.height_pt,
        sb_info.title_len,
        menu_info.top_level_items,
        menu_info.header_right_text,
        menu_info.claude_row_text,
        menu_info.claude_reset_text,
        menu_info.codex_row_text
    );

    status_bar.remove();
    Ok(())
}

fn present_update_dialog(
    mtm: MainThreadMarker,
    result: std::result::Result<UpdateCheckResult, String>,
) {
    let app = NSApplication::sharedApplication(mtm);
    app.activate();

    let alert = NSAlert::new(mtm);
    match result {
        Ok(UpdateCheckResult::UpdateAvailable {
            current,
            latest,
            release_url,
        }) => {
            alert.setAlertStyle(NSAlertStyle::Informational);
            alert.setMessageText(&NSString::from_str(&format!("Ebb {latest} is available")));
            alert.setInformativeText(&NSString::from_str(&format!(
                "You are running Ebb {current}. Download the latest Universal build from GitHub Releases."
            )));
            alert.addButtonWithTitle(&NSString::from_str("Open Release Page"));
            alert.addButtonWithTitle(&NSString::from_str("Later"));
            if alert.runModal() == NSAlertFirstButtonReturn {
                let _ = Command::new("/usr/bin/open").arg(&release_url).spawn();
            }
        }
        Ok(UpdateCheckResult::UpToDate { current, .. }) => {
            alert.setAlertStyle(NSAlertStyle::Informational);
            alert.setMessageText(&NSString::from_str("Ebb is up to date"));
            alert.setInformativeText(&NSString::from_str(&format!(
                "Ebb {current} is currently the newest version available."
            )));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            let _ = alert.runModal();
        }
        Err(err) => {
            alert.setAlertStyle(NSAlertStyle::Warning);
            alert.setMessageText(&NSString::from_str("Unable to check for updates"));
            alert.setInformativeText(&NSString::from_str(&err));
            alert.addButtonWithTitle(&NSString::from_str("OK"));
            let _ = alert.runModal();
        }
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
