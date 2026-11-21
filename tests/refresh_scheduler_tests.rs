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

    fn fetch_agy(&self, _cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>> {
        Ok(None)
    }
}

fn test_refresh_config(interval: Duration) -> RefreshConfig {
    RefreshConfig {
        interval,
        show_claude: true,
        show_codex: true,
        show_agy: false,
        claude_cache_path: PathBuf::from("/tmp/unused-claude-cache.json"),
        agy_cache_path: PathBuf::from("/tmp/unused-agy-cache.json"),
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

    let mut state = AppState::from_persisted(cfg, cache, Some(live_claude.clone()), None, 1_060);
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
        agy: None,
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
        agy: None,
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
    agy_calls: Arc<AtomicUsize>,
    use_real_caches: bool,
}

impl QuotaFetcher for SplitCountFetcher {
    fn fetch_claude(&self, cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>> {
        self.claude_calls.fetch_add(1, Ordering::SeqCst);
        if self.use_real_caches {
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

    fn fetch_agy(&self, cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>> {
        self.agy_calls.fetch_add(1, Ordering::SeqCst);
        if self.use_real_caches {
            ebb::providers::agy::AgyAdapter::new(cfg.agy_cache_path.clone()).read_snapshot()
        } else {
            Ok(Some(QuotaSnapshot {
                provider: Provider::Agy,
                five_hour: None,
                weekly: Some(QuotaWindow {
                    remaining_percent: 94,
                    resets_at: None,
                }),
                fetched_at: 10_000,
            }))
        }
    }
}

#[test]
fn scheduler_local_cache_changed_reads_local_only_and_never_starves_codex() {
    let claude_calls = Arc::new(AtomicUsize::new(0));
    let codex_calls = Arc::new(AtomicUsize::new(0));
    let agy_calls = Arc::new(AtomicUsize::new(0));
    let fetcher = SplitCountFetcher {
        claude_calls: Arc::clone(&claude_calls),
        codex_calls: Arc::clone(&codex_calls),
        agy_calls: Arc::clone(&agy_calls),
        use_real_caches: false,
    };
    let (tx, rx) = mpsc::channel();

    let mut cfg = test_refresh_config(Duration::from_secs(60));
    cfg.show_agy = true;
    let sched = RefreshScheduler::start(cfg, fetcher, move |ev| {
        if let RefreshEvent::Completed(outcome) = ev {
            let _ = tx.send((
                outcome.reason,
                outcome.claude.is_some(),
                outcome.codex.is_some(),
                outcome.agy.is_some(),
            ));
        }
    });

    // Fire LocalCacheChanged twice: must read Claude + Agy ONLY and never spawn/call Codex.
    for _ in 0..2 {
        sched.request_refresh(RefreshReason::LocalCacheChanged);
        let (reason, has_cl, has_cx, has_ag) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(reason, RefreshReason::LocalCacheChanged);
        assert!(has_cl);
        assert!(!has_cx);
        assert!(has_ag);
        assert_eq!(codex_calls.load(Ordering::SeqCst), 0);
    }

    // Switch to a short periodic interval and stream LocalCacheChanged faster than interval;
    // Periodic Codex refresh MUST still fire on schedule instead of being starved.
    let mut short_cfg = test_refresh_config(Duration::from_millis(120));
    short_cfg.show_agy = true;
    sched.update_config(short_cfg);
    let mut saw_periodic = false;
    for _ in 0..25 {
        sched.request_refresh(RefreshReason::LocalCacheChanged);
        thread::sleep(Duration::from_millis(25));
        while let Ok((reason, has_cl, has_cx, has_ag)) = rx.try_recv() {
            assert!(has_cl);
            assert!(has_ag);
            if reason == RefreshReason::Periodic {
                assert!(has_cx);
                saw_periodic = true;
            } else {
                assert_eq!(reason, RefreshReason::LocalCacheChanged);
                assert!(!has_cx);
            }
        }
        if saw_periodic {
            break;
        }
    }
    if !saw_periodic {
        let (reason, has_cl, has_cx, has_ag) = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(reason, RefreshReason::Periodic);
        assert!(has_cl);
        assert!(has_cx);
        assert!(has_ag);
    }
    assert!(codex_calls.load(Ordering::SeqCst) >= 1);
    assert!(claude_calls.load(Ordering::SeqCst) >= 3);
    assert!(agy_calls.load(Ordering::SeqCst) >= 3);

    sched.shutdown();
}

#[cfg(target_os = "macos")]
#[test]
fn kqueue_app_dir_watcher_detects_claude_agy_and_config_independently() {
    use ebb::providers::agy::write_agy_cache_atomic;
    use ebb::providers::claude::write_claude_cache_atomic;
    use ebb::watcher::{AppDirWatcher, DirWatchEvent};
    use std::time::Instant;

    let dir = temp_dir("kqueue-multi");
    let claude_path = dir.join("claude-statusline-cache.json");
    let agy_path = dir.join("agy-statusline-cache.json");
    let config_path = dir.join("config.json");
    let unrelated_path = dir.join("usage-cache.json");

    let claude_calls = Arc::new(AtomicUsize::new(0));
    let codex_calls = Arc::new(AtomicUsize::new(0));
    let agy_calls = Arc::new(AtomicUsize::new(0));
    let fetcher = SplitCountFetcher {
        claude_calls: Arc::clone(&claude_calls),
        codex_calls: Arc::clone(&codex_calls),
        agy_calls: Arc::clone(&agy_calls),
        use_real_caches: true,
    };

    let (sched_tx, sched_rx) = mpsc::channel();
    let cfg = RefreshConfig {
        interval: Duration::from_secs(60),
        show_claude: true,
        show_codex: true,
        show_agy: true,
        claude_cache_path: claude_path.clone(),
        agy_cache_path: agy_path.clone(),
        codex_path: None,
    };
    let sched = Arc::new(RefreshScheduler::start(cfg, fetcher, move |ev| {
        if let RefreshEvent::Completed(outcome) = ev {
            let cl_rem = outcome
                .claude
                .and_then(|r| r.ok())
                .flatten()
                .and_then(|s| s.five_hour)
                .map(|w| w.remaining_percent);
            let ag_rem = outcome
                .agy
                .and_then(|r| r.ok())
                .flatten()
                .and_then(|s| s.weekly)
                .map(|w| w.remaining_percent);
            let _ = sched_tx.send((outcome.reason, cl_rem, ag_rem, outcome.codex.is_some()));
        }
    }));

    let (ev_tx, ev_rx) = mpsc::channel::<DirWatchEvent>();
    let watch_sched = Arc::clone(&sched);
    let watcher = AppDirWatcher::start(
        dir.clone(),
        claude_path.clone(),
        agy_path.clone(),
        config_path.clone(),
        move |ev| {
            let _ = ev_tx.send(ev);
            if ev.any_cache_changed() {
                watch_sched.request_refresh(RefreshReason::LocalCacheChanged);
            }
        },
    )
    .unwrap();

    // 1. Unrelated file write in same directory must NOT trigger watcher.
    fs::write(&unrelated_path, r#"{"version":1}"#).unwrap();
    assert!(
        ev_rx.recv_timeout(Duration::from_millis(120)).is_err(),
        "unrelated file write must not trigger AppDirWatcher"
    );

    // 2. Atomic Claude cache write triggers instant LocalCacheChanged (<500ms, 0 Codex calls).
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
    write_claude_cache_atomic(&claude_path, &snap1, 0).unwrap();
    let ev1 = ev_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(ev1.claude_changed && !ev1.agy_changed && !ev1.config_changed);
    let (r1, cl1, _, has_cx1) = sched_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(t_write1.elapsed() < Duration::from_millis(500));
    assert_eq!(r1, RefreshReason::LocalCacheChanged);
    assert_eq!(cl1, Some(85));
    assert!(!has_cx1);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 0);

    // 3. Atomic Agy cache write triggers agy_changed + LocalCacheChanged with 0 Codex calls.
    let agy_snap = QuotaSnapshot {
        provider: Provider::Agy,
        five_hour: None,
        weekly: Some(QuotaWindow {
            remaining_percent: 91,
            resets_at: Some(1_795_800_000),
        }),
        fetched_at: 20_010,
    };
    write_agy_cache_atomic(&agy_path, &agy_snap, 0).unwrap();
    let ev2 = ev_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!ev2.claude_changed && ev2.agy_changed && !ev2.config_changed);
    let (r2, _, ag2, has_cx2) = sched_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert_eq!(r2, RefreshReason::LocalCacheChanged);
    assert_eq!(ag2, Some(91));
    assert!(!has_cx2);
    assert_eq!(codex_calls.load(Ordering::SeqCst), 0);

    // 4. External `config.json` edit fires `config_changed` WITHOUT triggering a quota refresh.
    AppConfig::default().save_atomic(&config_path).unwrap();
    let ev3 = ev_rx.recv_timeout(Duration::from_secs(1)).unwrap();
    assert!(!ev3.claude_changed && !ev3.agy_changed && ev3.config_changed);
    assert!(sched_rx.recv_timeout(Duration::from_millis(120)).is_err());

    // 5. Clean, fast shutdown of watcher + scheduler (<500ms).
    let t_stop = Instant::now();
    watcher.shutdown();
    sched.shutdown();
    assert!(t_stop.elapsed() < Duration::from_millis(500));

    let _ = fs::remove_dir_all(&dir);
}
