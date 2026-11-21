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

struct SplitCountFetcher {
    claude_calls: Arc<AtomicUsize>,
    codex_calls: Arc<AtomicUsize>,
    use_real_claude_cache: bool,
}

impl QuotaFetcher for SplitCountFetcher {
    fn fetch_claude(&self, cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>> {
        self.claude_calls.fetch_add(1, Ordering::SeqCst);
        if self.use_real_claude_cache {
            ebb::providers::claude::ClaudeAdapter::new(cfg.claude_cache_path.clone())
                .read_snapshot()
        } else {
            Ok(Some(QuotaSnapshot {
                provider: Provider::Claude,
                five_hour: Some(QuotaWindow {
                    remaining_percent: 82,
                    resets_at: None,
                }),
                weekly: Some(QuotaWindow {
                    remaining_percent: 97,
                    resets_at: None,
                }),
                fetched_at: 10_000,
            }))
        }
    }

    fn fetch_codex(&self, _cfg: &RefreshConfig) -> Result<QuotaSnapshot> {
        self.codex_calls.fetch_add(1, Ordering::SeqCst);
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

#[test]
fn scheduler_claude_cache_changed_reads_only_claude_and_never_starves_codex() {
    let claude_calls = Arc::new(AtomicUsize::new(0));
    let codex_calls = Arc::new(AtomicUsize::new(0));
    let fetcher = SplitCountFetcher {
        claude_calls: Arc::clone(&claude_calls),
        codex_calls: Arc::clone(&codex_calls),
        use_real_claude_cache: false,
    };
    let (tx, rx) = mpsc::channel();

    let sched = RefreshScheduler::start(
        test_refresh_config(Duration::from_millis(120)),
        fetcher,
        move |ev| {
            if let RefreshEvent::Completed(outcome) = ev {
                let _ = tx.send((
                    outcome.reason,
                    outcome.claude.is_some(),
                    outcome.codex.is_some(),
                ));
            }
        },
    );

    // Fire ClaudeCacheChanged twice quickly: must NOT spawn/call Codex!
    sched.request_refresh(RefreshReason::ClaudeCacheChanged);
    let (r1, has_cl1, has_cx1) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(r1, RefreshReason::ClaudeCacheChanged);
    assert!(has_cl1);
    assert!(!has_cx1);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 0);

    thread::sleep(Duration::from_millis(40));
    sched.request_refresh(RefreshReason::ClaudeCacheChanged);
    let (r2, _, has_cx2) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(r2, RefreshReason::ClaudeCacheChanged);
    assert!(!has_cx2);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 0);

    // Periodic Codex refresh MUST still fire on schedule (~120ms from start), not starved!
    let (r3, has_cl3, has_cx3) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(r3, RefreshReason::Periodic);
    assert!(has_cl3);
    assert!(has_cx3);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 1);
    assert!(claude_calls.load(Ordering::SeqCst) >= 3);

    sched.shutdown();
}

#[cfg(target_os = "macos")]
#[test]
fn kqueue_watcher_detects_atomic_renames_ignores_unrelated_and_triggers_claude_only() {
    use ebb::providers::claude::write_claude_cache_atomic;
    use ebb::watcher::ClaudeCacheWatcher;
    use std::time::Instant;

    let dir = temp_dir("kqueue");
    let cache_path = dir.join("claude-statusline-cache.json");
    let unrelated_path = dir.join("usage-cache.json");

    let claude_calls = Arc::new(AtomicUsize::new(0));
    let codex_calls = Arc::new(AtomicUsize::new(0));
    let fetcher = SplitCountFetcher {
        claude_calls: Arc::clone(&claude_calls),
        codex_calls: Arc::clone(&codex_calls),
        use_real_claude_cache: true,
    };

    let (tx, rx) = mpsc::channel();
    let cfg = RefreshConfig {
        interval: Duration::from_secs(60),
        show_claude: true,
        show_codex: true,
        claude_cache_path: cache_path.clone(),
        codex_path: None,
    };
    let sched = Arc::new(RefreshScheduler::start(cfg, fetcher, move |ev| {
        if let RefreshEvent::Completed(outcome) = ev {
            let rem = outcome
                .claude
                .and_then(|r| r.ok())
                .flatten()
                .and_then(|s| s.five_hour)
                .map(|w| w.remaining_percent);
            let _ = tx.send((outcome.reason, rem, outcome.codex.is_some()));
        }
    }));

    let watch_sched = Arc::clone(&sched);
    let watcher = ClaudeCacheWatcher::start(cache_path.clone(), move || {
        watch_sched.request_refresh(RefreshReason::ClaudeCacheChanged);
    })
    .unwrap();

    // 1. Unrelated file write in same directory must NOT trigger watcher.
    fs::write(&unrelated_path, r#"{"version":1}"#).unwrap();
    assert!(
        rx.recv_timeout(Duration::from_millis(120)).is_err(),
        "unrelated file write must not trigger Claude refresh"
    );

    // 2. First atomic Claude cache write triggers instant ClaudeOnly refresh (<500ms).
    let snap1 = QuotaSnapshot {
        provider: Provider::Claude,
        five_hour: Some(QuotaWindow {
            remaining_percent: 85,
            resets_at: Some(1_795_239_600),
        }),
        weekly: Some(QuotaWindow {
            remaining_percent: 97,
            resets_at: Some(1_795_788_000),
        }),
        fetched_at: 20_000,
    };
    let t_write1 = Instant::now();
    write_claude_cache_atomic(&cache_path, &snap1, 0).unwrap();
    let (r1, rem1, has_cx1) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(
        t_write1.elapsed() < Duration::from_millis(500),
        "instant watch took too long: {:?}",
        t_write1.elapsed()
    );
    assert_eq!(r1, RefreshReason::ClaudeCacheChanged);
    assert_eq!(rem1, Some(85));
    assert!(!has_cx1);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 0);

    // 3. Second atomic rename (identical byte length: 85 -> 84) MUST also fire via inode change.
    let snap2 = QuotaSnapshot {
        five_hour: Some(QuotaWindow {
            remaining_percent: 84,
            resets_at: Some(1_795_239_600),
        }),
        fetched_at: 20_010,
        ..snap1
    };
    write_claude_cache_atomic(&cache_path, &snap2, 0).unwrap();
    let (r2, rem2, has_cx2) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(r2, RefreshReason::ClaudeCacheChanged);
    assert_eq!(rem2, Some(84));
    assert!(!has_cx2);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 0);

    // 4. Clean, fast shutdown of watcher + scheduler (<200ms).
    let t_stop = Instant::now();
    watcher.shutdown();
    sched.shutdown();
    assert!(t_stop.elapsed() < Duration::from_millis(500));

    let _ = fs::remove_dir_all(&dir);
}
