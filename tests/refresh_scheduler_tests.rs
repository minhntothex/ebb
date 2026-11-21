use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use ebb::config::AppConfig;
use ebb::error::{AppError, Result};
use ebb::launch_agent::{LAUNCH_AGENT_LABEL, LaunchAgentManager, render_launch_agent_plist};
use ebb::model::{Provider, ProviderHealth, QuotaSnapshot, QuotaWindow, UsageCache};
use ebb::refresh::{
    AppState, QuotaFetcher, RefreshConfig, RefreshEvent, RefreshOutcome, RefreshReason,
    RefreshScheduler,
};

static TEST_SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_dir(label: &str) -> PathBuf {
    let seq = TEST_SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ai-usage-monitor-sched-{label}-{}-{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("create temp test dir");
    dir
}

struct MockFetcher {
    delay: Duration,
    active: Arc<AtomicUsize>,
    max_concurrent: Arc<AtomicUsize>,
    calls: Arc<AtomicUsize>,
    fail_codex: Arc<Mutex<bool>>,
}

impl MockFetcher {
    fn new(delay: Duration) -> (Self, Arc<AtomicUsize>, Arc<AtomicUsize>, Arc<Mutex<bool>>) {
        let active = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let fail_codex = Arc::new(Mutex::new(false));
        let fetcher = Self {
            delay,
            active,
            max_concurrent: Arc::clone(&max_concurrent),
            calls: Arc::clone(&calls),
            fail_codex: Arc::clone(&fail_codex),
        };
        (fetcher, calls, max_concurrent, fail_codex)
    }
}

impl QuotaFetcher for MockFetcher {
    fn fetch_claude(&self, _cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>> {
        Ok(Some(QuotaSnapshot {
            provider: Provider::Claude,
            five_hour: Some(QuotaWindow {
                remaining_percent: 85,
                resets_at: None,
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 97,
                resets_at: None,
            }),
            fetched_at: 10_000,
        }))
    }

    fn fetch_codex(&self, _cfg: &RefreshConfig) -> Result<QuotaSnapshot> {
        let now_active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_concurrent.fetch_max(now_active, Ordering::SeqCst);
        self.calls.fetch_add(1, Ordering::SeqCst);

        if !self.delay.is_zero() {
            thread::sleep(self.delay);
        }

        self.active.fetch_sub(1, Ordering::SeqCst);

        if *self.fail_codex.lock().unwrap() {
            return Err(AppError::Timeout {
                provider: Provider::Codex,
                timeout_secs: 5,
            });
        }

        Ok(QuotaSnapshot {
            provider: Provider::Codex,
            five_hour: Some(QuotaWindow {
                remaining_percent: 78,
                resets_at: None,
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 81,
                resets_at: None,
            }),
            fetched_at: 10_000,
        })
    }
}

fn test_refresh_config(interval: Duration) -> RefreshConfig {
    RefreshConfig {
        interval,
        show_claude: true,
        show_codex: true,
        claude_cache_path: PathBuf::from("/tmp/unused-claude-cache.json"),
        codex_path: None,
    }
}

#[test]
fn scheduler_coalesces_burst_requests_and_never_overlaps() {
    let (fetcher, calls, max_concurrent, _) = MockFetcher::new(Duration::from_millis(80));
    let (tx, rx) = mpsc::channel();

    let sched = RefreshScheduler::start(
        test_refresh_config(Duration::from_secs(60)),
        fetcher,
        move |ev| {
            if let RefreshEvent::Completed(outcome) = ev {
                let _ = tx.send(outcome.reason);
            }
        },
    );

    // Fire 10 bursts back-to-back while the first 80ms fetch is in-flight.
    sched.request_refresh(RefreshReason::Startup);
    thread::sleep(Duration::from_millis(15));
    for _ in 0..9 {
        sched.request_refresh(RefreshReason::Manual);
    }

    let first = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    let second = rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(first, RefreshReason::Startup);
    assert_eq!(second, RefreshReason::Manual);

    // No 3rd spurious run should occur.
    assert!(rx.recv_timeout(Duration::from_millis(150)).is_err());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        max_concurrent.load(Ordering::SeqCst),
        1,
        "must never run overlapping refreshes"
    );

    sched.shutdown();
}

#[test]
fn scheduler_periodic_and_dynamic_interval_rescheduling() {
    let (fetcher, _, _, _) = MockFetcher::new(Duration::from_millis(5));
    let (tx, rx) = mpsc::channel();

    // Start with a very long interval (60s), then shorten dynamically to 40ms.
    let sched = RefreshScheduler::start(
        test_refresh_config(Duration::from_secs(60)),
        fetcher,
        move |ev| {
            if let RefreshEvent::Completed(outcome) = ev {
                let _ = tx.send(outcome.reason);
            }
        },
    );

    // No periodic tick should fire immediately.
    assert!(rx.recv_timeout(Duration::from_millis(60)).is_err());

    // Shorten interval to 40ms -> Condvar wakes immediately and fires periodic refreshes.
    sched.update_config(test_refresh_config(Duration::from_millis(40)));
    let r1 = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    let r2 = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(r1, RefreshReason::Periodic);
    assert_eq!(r2, RefreshReason::Periodic);

    sched.shutdown();
}

#[test]
fn scheduler_pauses_on_sleep_and_triggers_wake_refresh() {
    let (fetcher, _, _, _) = MockFetcher::new(Duration::from_millis(5));
    let (tx, rx) = mpsc::channel();

    let sched = RefreshScheduler::start(
        test_refresh_config(Duration::from_millis(40)),
        fetcher,
        move |ev| {
            if let RefreshEvent::Completed(outcome) = ev {
                let _ = tx.send(outcome.reason);
            }
        },
    );

    // Put system to sleep immediately; drain any in-flight event first, then verify silence.
    sched.set_sleeping(true);
    thread::sleep(Duration::from_millis(30));
    while rx.try_recv().is_ok() {}
    assert!(
        rx.recv_timeout(Duration::from_millis(120)).is_err(),
        "no periodic refreshes may run while sleeping"
    );

    // Wake up -> must immediately emit a RefreshReason::Wake cycle.
    sched.set_sleeping(false);
    let reason = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(reason, RefreshReason::Wake);

    sched.shutdown();
}

#[test]
fn app_state_seeds_cache_merges_outcomes_and_tracks_dirty_flag() {
    let cfg = AppConfig::default();
    let mut cache = UsageCache::empty();
    cache.set_snapshot(QuotaSnapshot {
        provider: Provider::Codex,
        five_hour: Some(QuotaWindow {
            remaining_percent: 78,
            resets_at: None,
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 81,
            resets_at: None,
        }),
        fetched_at: 1_000,
    });

    let live_claude = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: Some(QuotaWindow {
            remaining_percent: 85,
            resets_at: None,
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 97,
            resets_at: None,
        }),
        fetched_at: 1_050,
    };

    let mut state = AppState::from_persisted(cfg, cache, Some(live_claude.clone()), 1_060);
    assert_eq!(state.claude.health, ProviderHealth::Fresh);
    assert_eq!(state.codex.health, ProviderHealth::Stale);

    // Identical outcome -> dirty flag false (no redundant disk write).
    let same_outcome = RefreshOutcome {
        reason: RefreshReason::Periodic,
        claude: Some(Ok(Some(live_claude.clone()))),
        codex: Some(Err(AppError::Timeout {
            provider: Provider::Codex,
            timeout_secs: 5,
        })),
        completed_at: 1_100,
    };
    assert!(!state.apply_refresh_outcome(same_outcome));
    assert_eq!(state.claude.health, ProviderHealth::Fresh);
    assert_eq!(state.codex.health, ProviderHealth::Stale);
    assert_eq!(
        state.ui_snapshot(1_100).menu_bar_plain_text(),
        "85/97 78/81"
    );

    // Updated Codex snapshot -> dirty flag true, Codex becomes Fresh.
    let updated_outcome = RefreshOutcome {
        reason: RefreshReason::Manual,
        claude: Some(Ok(Some(live_claude))),
        codex: Some(Ok(QuotaSnapshot {
            provider: Provider::Codex,
            five_hour: Some(QuotaWindow {
                remaining_percent: 70,
                resets_at: None,
            }),
            weekly: Some(QuotaWindow {
                remaining_percent: 80,
                resets_at: None,
            }),
            fetched_at: 1_160,
        })),
        completed_at: 1_160,
    };
    assert!(state.apply_refresh_outcome(updated_outcome));
    assert_eq!(state.codex.health, ProviderHealth::Fresh);
    assert_eq!(
        state.ui_snapshot(1_160).menu_bar_plain_text(),
        "85/97 70/80"
    );
}

#[test]
fn launch_agent_plist_install_uninstall_and_xml_escaping() {
    let dir = temp_dir("launch-agent");
    let mgr = LaunchAgentManager::in_agents_dir(dir.clone());
    assert!(!mgr.is_enabled());

    let exe =
        PathBuf::from("/Applications/AI & Usage's <Monitor>.app/Contents/MacOS/ai-usage-monitor");
    mgr.set_enabled(true, &exe).unwrap();
    assert!(mgr.is_enabled());
    assert_eq!(
        mgr.plist_path(),
        dir.join(format!("{LAUNCH_AGENT_LABEL}.plist"))
    );

    let xml = fs::read_to_string(mgr.plist_path()).unwrap();
    assert_eq!(xml, render_launch_agent_plist(LAUNCH_AGENT_LABEL, &exe));
    assert!(xml.contains("&amp;"));
    assert!(xml.contains("&apos;"));
    assert!(xml.contains("&lt;Monitor&gt;"));
    assert!(xml.contains("<key>RunAtLoad</key>"));
    assert!(xml.contains("<true/>"));

    // Idempotent second install does not fail or leave .tmp files.
    mgr.set_enabled(true, &exe).unwrap();
    let has_tmp = fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .any(|e| e.file_name().to_string_lossy().ends_with(".tmp"));
    assert!(!has_tmp);

    // Disable removes plist cleanly and is idempotent.
    mgr.set_enabled(false, &exe).unwrap();
    assert!(!mgr.is_enabled());
    mgr.set_enabled(false, &exe).unwrap();

    let _ = fs::remove_dir_all(&dir);
}
