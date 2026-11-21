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
use crate::config::{AppConfig, AppPaths, ColorTheme};
use crate::error::{AppError, Result};
use crate::launch_agent::LaunchAgentManager;
use crate::model::{
    DisplayMetric, MenuBarWindows, Provider, ProviderState, QuotaSnapshot, QuotaWindow, WindowKind,
};
use crate::providers::agy::{
    AgyAdapter, apply_agy_statusline_install, default_agy_settings_path,
    plan_agy_statusline_install,
};
use crate::providers::claude::{
    ClaudeAdapter, apply_statusline_install, default_claude_settings_path, plan_statusline_install,
};
use crate::providers::statusline_common::StatuslineInstallPlan;
use crate::refresh::{
    AppState, LiveQuotaFetcher, RefreshConfig, RefreshEvent, RefreshReason, RefreshScheduler,
};
use crate::ui::menu::{DropdownMenu, MenuAction};
use crate::ui::notifications::NotificationSender;
use crate::ui::status_bar::StatusBarView;
use crate::ui::view_model::UiSnapshot;
use crate::updates::{UpdateCheckResult, check_latest_release};
use crate::watcher::AppDirWatcher;

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
    dir_watcher: Option<AppDirWatcher>,
    update_check_in_flight: Arc<AtomicBool>,
    ui_alive: Arc<AtomicBool>,
    workspace_observer: Retained<WorkspaceObserver>,
    status_bar: StatusBarView,
    dropdown: DropdownMenu,
    claude_configured: bool,
    agy_configured: bool,
}

impl MainThreadUi {
    fn refresh_integration_flags(&mut self) {
        if let Ok(exe) = std::env::current_exe() {
            self.claude_configured = plan_statusline_install(&default_claude_settings_path(), &exe)
                .map(|p| p.already_installed)
                .unwrap_or(false);
            self.agy_configured = plan_agy_statusline_install(&default_agy_settings_path(), &exe)
                .map(|p| p.already_installed)
                .unwrap_or(false);
        }
    }

    fn render(&mut self, mtm: MainThreadMarker) {
        let snap = self
            .state
            .ui_snapshot(unix_now_secs())
            .with_integrations(self.claude_configured, self.agy_configured);
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
        let refresh_cfg = RefreshConfig::from_app_config(&self.state.config, &self.paths);
        self.scheduler.update_config(refresh_cfg);
    }

    fn evaluate_and_notify(
        &mut self,
        fresh_claude: Option<&QuotaSnapshot>,
        fresh_codex: Option<&QuotaSnapshot>,
        fresh_agy: Option<&QuotaSnapshot>,
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
        if let Some(snap) = fresh_agy
            && !snap.is_stale(now_secs, self.state.config.agy_stale_after_seconds)
        {
            alerts.extend(
                self.alert_tracker
                    .evaluate(snap, &self.state.config, now_secs),
            );
        }
        for alert in alerts {
            self.notifier.send(&alert, now_secs);
        }
    }

    fn reload_external_config(&mut self, mtm: MainThreadMarker) {
        let Ok(mut loaded) = AppConfig::load_or_default(&self.paths.config_path) else {
            return;
        };
        // Keep LaunchAgent checkmark truthful to disk state.
        if let Ok(agent) = LaunchAgentManager::default_macos() {
            loaded.launch_at_login = agent.is_enabled();
        }
        if loaded == self.state.config {
            return;
        }
        let alerts_turned_on = !self.state.config.alerts_enabled && loaded.alerts_enabled;
        self.state.config = loaded;
        if alerts_turned_on {
            self.notifier.ensure_authorized();
            self.alert_tracker.seed_baseline(
                &[
                    self.state.claude.snapshot.as_ref(),
                    self.state.codex.snapshot.as_ref(),
                    self.state.agy.snapshot.as_ref(),
                ],
                &self.state.config,
                unix_now_secs(),
            );
        }
        self.sync_scheduler_config();
        self.render(mtm);
    }

    fn shutdown(&self) {
        self.ui_alive.store(false, Ordering::Release);
        self.workspace_observer.unregister();
        if let Some(ref watcher) = self.dir_watcher {
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
    let agy_snapshot = AgyAdapter::new(paths.agy_cache_path.clone())
        .read_snapshot()
        .ok()
        .flatten();
    let state =
        AppState::from_persisted(config, persisted_cache, claude_snapshot, agy_snapshot, now);

    // Seed alert baseline from cached snapshots so launching Ebb never replays old alerts.
    let mut alert_tracker = AlertTracker::new();
    alert_tracker.seed_baseline(
        &[
            state.claude.snapshot.as_ref(),
            state.codex.snapshot.as_ref(),
            state.agy.snapshot.as_ref(),
        ],
        &state.config,
        now,
    );
    let mut notifier = NotificationSender::new();
    if state.config.alerts_enabled {
        notifier.ensure_authorized();
    }

    let refresh_cfg = RefreshConfig::from_app_config(&state.config, &paths);

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
                            let fresh_agy = outcome
                                .agy
                                .as_ref()
                                .and_then(|res| res.as_ref().ok())
                                .and_then(|opt| opt.clone());

                            let cache_changed = ui.state.apply_refresh_outcome(*outcome);
                            if cache_changed {
                                ui.persist_cache();
                            }
                            ui.evaluate_and_notify(
                                fresh_claude.as_ref(),
                                fresh_codex.as_ref(),
                                fresh_agy.as_ref(),
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

    // Zero-CPU macOS kqueue watcher for instant Claude/Agy cache updates + `config.json` hot-reload.
    let watch_sched = Arc::clone(&scheduler);
    let watch_ptr = Arc::clone(&ui_ptr);
    let watch_alive = Arc::clone(&ui_alive);
    let dir_watcher = AppDirWatcher::start(
        paths.base_dir.clone(),
        paths.claude_cache_path.clone(),
        paths.agy_cache_path.clone(),
        paths.config_path.clone(),
        move |ev| {
            if ev.any_cache_changed() {
                watch_sched.request_refresh(RefreshReason::LocalCacheChanged);
            }
            if ev.config_changed && watch_alive.load(Ordering::Acquire) {
                let target_ptr = Arc::clone(&watch_ptr);
                let alive = Arc::clone(&watch_alive);
                DispatchQueue::main().exec_async(move || {
                    if !alive.load(Ordering::Acquire) {
                        return;
                    }
                    let Some(mtm) = MainThreadMarker::new() else {
                        return;
                    };
                    let cell = unsafe { &*(target_ptr.0 as *const RefCell<Option<MainThreadUi>>) };
                    if let Some(ui) = cell.borrow_mut().as_mut() {
                        ui.reload_external_config(mtm);
                    }
                });
            }
        },
    )
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
        dir_watcher,
        update_check_in_flight: Arc::new(AtomicBool::new(false)),
        ui_alive,
        workspace_observer,
        status_bar,
        dropdown,
        claude_configured: false,
        agy_configured: false,
    };
    ui.refresh_integration_flags();
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
        MenuAction::WillOpen => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.refresh_integration_flags();
                ui.render(mtm);
            }
        }
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
        MenuAction::ToggleShowAgy => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.show_agy = !ui.state.config.show_agy;
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
        MenuAction::SetDisplayMetric(metric) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.display_metric = metric;
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::SetMenuBarWindows(windows) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.menu_bar_windows = windows;
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::ToggleHighlightCritical => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.highlight_critical = !ui.state.config.highlight_critical;
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::MoveProviderFirst(provider) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.move_provider_first(provider);
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::SetColorTheme(theme) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.color_theme = theme;
                ui.persist_config();
                ui.render(mtm);
            }
        }
        MenuAction::SetupClaudeStatusline => {
            run_integration_setup_dialog(mtm, ui_cell, Provider::Claude);
        }
        MenuAction::SetupAgyStatusline => {
            run_integration_setup_dialog(mtm, ui_cell, Provider::Agy);
        }
        MenuAction::ToggleAlerts => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                ui.state.config.alerts_enabled = !ui.state.config.alerts_enabled;
                if ui.state.config.alerts_enabled {
                    ui.notifier.ensure_authorized();
                    ui.alert_tracker.seed_baseline(
                        &[
                            ui.state.claude.snapshot.as_ref(),
                            ui.state.codex.snapshot.as_ref(),
                            ui.state.agy.snapshot.as_ref(),
                        ],
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

    let mut agy = ProviderState::new(Provider::Agy);
    agy.apply_success(
        QuotaSnapshot {
            provider: Provider::Agy,
            five_hour: None,
            weekly: Some(QuotaWindow {
                remaining_percent: 94,
                resets_at: Some(now + 6 * 86_400 + 11 * 3600),
            }),
            fetched_at: 1_000,
        },
        1_000,
    );
    let snap = UiSnapshot::build(&config, &claude, &codex, &agy, false, now);

    let will_open_fired = Rc::new(AtomicBool::new(false));
    let will_open_cb = Rc::clone(&will_open_fired);
    let mut status_bar = StatusBarView::new(mtm);
    let mut dropdown = DropdownMenu::new(mtm, move |act| {
        if act == MenuAction::WillOpen {
            will_open_cb.store(true, Ordering::SeqCst);
        }
    });
    status_bar.set_menu(dropdown.ns_menu());
    dropdown.trigger_will_open_for_test();
    assert!(
        will_open_fired.load(Ordering::SeqCst),
        "NSMenuDelegate::menuWillOpen: callback must fire"
    );

    // Verify WorkspaceObserver registers & unregisters cleanly without panicking.
    let smoke_paths = AppPaths::from_base_dir(std::env::temp_dir().join("ebb-smoke-app"));
    let dummy_cfg = RefreshConfig::from_app_config(&config, &smoke_paths);
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

    // Verify AppDirWatcher starts and shuts down cleanly without leaks.
    if let Ok(watcher) = AppDirWatcher::start(
        smoke_paths.base_dir.clone(),
        smoke_paths.claude_cache_path.clone(),
        smoke_paths.agy_cache_path.clone(),
        smoke_paths.config_path.clone(),
        |_| {},
    ) {
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
    let hidden_reset_snap = UiSnapshot::build(&config, &claude, &codex, &agy, false, now);
    assert!(dropdown.apply(&hidden_reset_snap));
    let hidden_info = dropdown.debug_inspect();
    assert!(hidden_info.claude_reset_text.is_none());
    assert!(hidden_info.codex_reset_text.is_none());

    // Verify 3-provider ordering (`Agy` first), Used %, 5h-only compact mode, Critical highlight, and themes.
    config.show_agy = true;
    config.show_reset_time = true;
    config.move_provider_first(Provider::Agy);
    config.display_metric = DisplayMetric::Used;
    config.color_theme = ColorTheme::HighContrast;
    let reordered_snap = UiSnapshot::build(&config, &claude, &codex, &agy, false, now);
    assert!(status_bar.apply(&reordered_snap, mtm));
    assert!(dropdown.apply(&reordered_snap));
    let reordered_info = dropdown.debug_inspect();
    assert_eq!(reordered_info.visible_rows.len(), 3);
    assert_eq!(reordered_info.visible_rows[0].0, "● Agy\t– / 6%");
    assert_eq!(reordered_info.visible_rows[1].0, "● Claude\t35% / 28%");
    assert_eq!(reordered_info.visible_rows[2].0, "● Codex\t57% / 69%");

    config.menu_bar_windows = MenuBarWindows::FiveHourOnly;
    config.color_theme = ColorTheme::Monochrome;
    let compact_snap = UiSnapshot::build(&config, &claude, &codex, &agy, false, now);
    assert!(status_bar.apply(&compact_snap, mtm));

    // Verify stale/partial transition mutates cleanly.
    let partial_snap = UiSnapshot::build(&config, &claude, &codex, &agy, true, 2_500);
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

fn run_integration_setup_dialog(
    mtm: MainThreadMarker,
    ui_cell: &Rc<RefCell<Option<MainThreadUi>>>,
    provider: Provider,
) {
    let app = NSApplication::sharedApplication(mtm);
    app.activate();

    let (title, settings_path, plan_res) = match std::env::current_exe() {
        Ok(exe) => match provider {
            Provider::Claude => {
                let p = default_claude_settings_path();
                (
                    "Claude Code Status-Line",
                    p.clone(),
                    plan_statusline_install(&p, &exe).map(|plan| (exe, plan)),
                )
            }
            Provider::Agy => {
                let p = default_agy_settings_path();
                (
                    "Antigravity CLI Status-Line",
                    p.clone(),
                    plan_agy_statusline_install(&p, &exe).map(|plan| (exe, plan)),
                )
            }
            Provider::Codex => return,
        },
        Err(e) => {
            show_simple_alert(
                mtm,
                NSAlertStyle::Warning,
                "Unable to locate Ebb binary",
                &e.to_string(),
            );
            return;
        }
    };

    let (exe, plan): (std::path::PathBuf, StatuslineInstallPlan) = match plan_res {
        Ok(v) => v,
        Err(e) => {
            show_simple_alert(
                mtm,
                NSAlertStyle::Warning,
                &format!("Unable to inspect {title}"),
                &e.to_string(),
            );
            return;
        }
    };

    if plan.already_installed {
        show_simple_alert(
            mtm,
            NSAlertStyle::Informational,
            &format!("{title} is already configured"),
            &format!(
                "Settings file:\n{}\n\nActive command:\n{}",
                plan.settings_path.display(),
                plan.new_command
            ),
        );
        return;
    }

    let prev_desc = plan
        .previous_command
        .as_deref()
        .unwrap_or("<none — built-in status line will be preserved>");
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(&format!("Configure {title}?")));
    alert.setInformativeText(&NSString::from_str(&format!(
        "Ebb will create a timestamped backup and update:\n{}\n\nCurrent command:\n{prev_desc}\n\nPlanned command:\n{}",
        plan.settings_path.display(),
        plan.new_command
    )));
    alert.addButtonWithTitle(&NSString::from_str("Apply"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));

    if alert.runModal() != NSAlertFirstButtonReturn {
        return;
    }

    let apply_res = match provider {
        Provider::Claude => apply_statusline_install(&settings_path, &exe),
        Provider::Agy => apply_agy_statusline_install(&settings_path, &exe),
        Provider::Codex => return,
    };

    match apply_res {
        Ok((_, backup)) => {
            if let Some(ui) = ui_cell.borrow_mut().as_mut() {
                match provider {
                    Provider::Claude => ui.state.config.show_claude = true,
                    Provider::Agy => ui.state.config.show_agy = true,
                    Provider::Codex => {}
                }
                ui.persist_config();
                ui.sync_scheduler_config();
                ui.refresh_integration_flags();
                ui.render(mtm);
            }
            let backup_note = backup
                .map(|b| format!("\n\nBackup saved to:\n{}", b.display()))
                .unwrap_or_default();
            show_simple_alert(
                mtm,
                NSAlertStyle::Informational,
                &format!("{title} configured"),
                &format!("Updated {} cleanly.{backup_note}", settings_path.display()),
            );
        }
        Err(e) => {
            show_simple_alert(
                mtm,
                NSAlertStyle::Warning,
                &format!("Failed to configure {title}"),
                &e.to_string(),
            );
        }
    }
}

fn show_simple_alert(mtm: MainThreadMarker, style: NSAlertStyle, title: &str, body: &str) {
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(style);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(body));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    let _ = alert.runModal();
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
