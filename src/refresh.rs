use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{AppConfig, AppPaths};
use crate::error::Result;
use crate::model::{Provider, ProviderState, QuotaSnapshot, UsageCache};
use crate::providers::agy::AgyAdapter;
use crate::providers::claude::ClaudeAdapter;
use crate::providers::codex::CodexAdapter;
use crate::ui::view_model::UiSnapshot;

/// Why a refresh cycle was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshReason {
    Startup,
    Periodic,
    Manual,
    Wake,
    /// Dedicated fast-path triggered by `AppDirWatcher` when local status-line cache files
    /// (`claude-statusline-cache.json` or `agy-statusline-cache.json`) change on disk.
    /// Reads only local cache files without spawning `codex app-server`.
    LocalCacheChanged,
}

impl RefreshReason {
    /// Whether this refresh cycle should also poll Codex (`false` for local cache watch fast-path).
    pub const fn fetches_codex(self) -> bool {
        !matches!(self, RefreshReason::LocalCacheChanged)
    }
}

/// Worker-visible subset of configuration that affects refresh cadence and adapter execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshConfig {
    pub interval: Duration,
    pub show_claude: bool,
    pub show_codex: bool,
    pub show_agy: bool,
    pub claude_cache_path: PathBuf,
    pub agy_cache_path: PathBuf,
    pub codex_path: Option<String>,
}

impl RefreshConfig {
    pub fn from_app_config(config: &AppConfig, paths: &AppPaths) -> Self {
        Self {
            interval: Duration::from_secs(config.refresh_interval_seconds),
            show_claude: config.show_claude,
            show_codex: config.show_codex,
            show_agy: config.show_agy,
            claude_cache_path: paths.claude_cache_path.clone(),
            agy_cache_path: paths.agy_cache_path.clone(),
            codex_path: config.codex_path.clone(),
        }
    }
}

/// Result produced by one background refresh cycle.
#[derive(Debug)]
pub struct RefreshOutcome {
    pub reason: RefreshReason,
    pub claude: Option<Result<Option<QuotaSnapshot>>>,
    pub codex: Option<Result<QuotaSnapshot>>,
    pub agy: Option<Result<Option<QuotaSnapshot>>>,
    pub completed_at: u64,
}

/// Events delivered from the single background worker to the UI layer.
#[derive(Debug)]
pub enum RefreshEvent {
    Started(RefreshReason),
    Completed(Box<RefreshOutcome>),
}

/// Platform-independent application state owned by the UI/main thread.
#[derive(Debug, Clone)]
pub struct AppState {
    pub config: AppConfig,
    pub claude: ProviderState,
    pub codex: ProviderState,
    pub agy: ProviderState,
    pub is_refreshing: bool,
}

impl AppState {
    /// Initialize state from persisted `usage-cache.json` and optional dedicated Claude/Agy caches.
    pub fn from_persisted(
        config: AppConfig,
        persisted_cache: UsageCache,
        claude_statusline_snapshot: Option<QuotaSnapshot>,
        agy_statusline_snapshot: Option<QuotaSnapshot>,
        now_secs: u64,
    ) -> Self {
        let mut claude = ProviderState::new(Provider::Claude);
        let mut codex = ProviderState::new(Provider::Codex);
        let mut agy = ProviderState::new(Provider::Agy);

        if let Some(s) = persisted_cache.claude {
            claude.seed_from_cache(s);
        }
        if let Some(s) = persisted_cache.codex {
            codex.seed_from_cache(s);
        }
        if let Some(s) = persisted_cache.agy {
            agy.seed_from_cache(s);
        }

        if let Some(live_claude) = claude_statusline_snapshot {
            let is_newer = claude
                .snapshot
                .as_ref()
                .is_none_or(|prev| live_claude.fetched_at >= prev.fetched_at);
            if is_newer {
                claude.apply_success(live_claude, now_secs);
            }
        }
        if let Some(live_agy) = agy_statusline_snapshot {
            let is_newer = agy
                .snapshot
                .as_ref()
                .is_none_or(|prev| live_agy.fetched_at >= prev.fetched_at);
            if is_newer {
                agy.apply_success(live_agy, now_secs);
            }
        }

        Self {
            config,
            claude,
            codex,
            agy,
            is_refreshing: false,
        }
    }

    pub fn mark_refresh_started(&mut self) {
        self.is_refreshing = true;
    }

    /// Merge a completed background refresh cycle into state.
    /// Returns `true` if any provider's persisted snapshot changed (and should be saved to disk).
    pub fn apply_refresh_outcome(&mut self, outcome: RefreshOutcome) -> bool {
        let prev_claude = self.claude.snapshot.clone();
        let prev_codex = self.codex.snapshot.clone();
        let prev_agy = self.agy.snapshot.clone();

        if let Some(res) = outcome.claude {
            merge_passive_outcome(&mut self.claude, res, outcome.completed_at);
        }

        if let Some(res) = outcome.codex {
            match res {
                Ok(snap) => self.codex.apply_success(snap, outcome.completed_at),
                Err(err) => self.codex.apply_failure(&err, outcome.completed_at),
            }
        }

        if let Some(res) = outcome.agy {
            merge_passive_outcome(&mut self.agy, res, outcome.completed_at);
        }

        self.is_refreshing = false;
        self.claude.snapshot != prev_claude
            || self.codex.snapshot != prev_codex
            || self.agy.snapshot != prev_agy
    }

    pub fn ui_snapshot(&self, now_secs: u64) -> UiSnapshot {
        UiSnapshot::build(
            &self.config,
            &self.claude,
            &self.codex,
            &self.agy,
            self.is_refreshing,
            now_secs,
        )
    }

    pub fn to_usage_cache(&self) -> UsageCache {
        let mut cache = UsageCache::empty();
        if let Some(ref s) = self.claude.snapshot {
            cache.set_snapshot(s.clone());
        }
        if let Some(ref s) = self.codex.snapshot {
            cache.set_snapshot(s.clone());
        }
        if let Some(ref s) = self.agy.snapshot {
            cache.set_snapshot(s.clone());
        }
        cache
    }
}

/// Abstraction over provider I/O so the scheduler can be tested deterministically without subprocesses.
pub trait QuotaFetcher: Send + 'static {
    fn fetch_claude(&self, cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>>;
    fn fetch_codex(&self, cfg: &RefreshConfig) -> Result<QuotaSnapshot>;
    fn fetch_agy(&self, cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>>;
}

/// Production fetcher delegating to `ClaudeAdapter`, `AgyAdapter`, and `CodexAdapter`.
#[derive(Debug, Default, Clone, Copy)]
pub struct LiveQuotaFetcher;

impl QuotaFetcher for LiveQuotaFetcher {
    fn fetch_claude(&self, cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>> {
        ClaudeAdapter::new(cfg.claude_cache_path.clone()).read_snapshot()
    }

    fn fetch_codex(&self, cfg: &RefreshConfig) -> Result<QuotaSnapshot> {
        CodexAdapter::new(cfg.codex_path.clone()).fetch_snapshot()
    }

    fn fetch_agy(&self, cfg: &RefreshConfig) -> Result<Option<QuotaSnapshot>> {
        AgyAdapter::new(cfg.agy_cache_path.clone()).read_snapshot()
    }
}

fn merge_passive_outcome(
    state: &mut ProviderState,
    res: Result<Option<QuotaSnapshot>>,
    completed_at: u64,
) {
    match res {
        Ok(Some(snap)) => {
            let is_newer = state
                .snapshot
                .as_ref()
                .is_none_or(|prev| snap.fetched_at >= prev.fetched_at);
            if is_newer {
                state.apply_success(snap, completed_at);
            }
        }
        Ok(None) => {
            state.last_attempt_at = Some(completed_at);
        }
        Err(err) => state.apply_failure(&err, completed_at),
    }
}

struct WorkerState {
    config: RefreshConfig,
    /// Coalesced pending manual/startup/wake refresh reason (`None` if no immediate refresh queued).
    pending_reason: Option<RefreshReason>,
    /// True while a refresh cycle is actively running on the single worker thread.
    in_flight: bool,
    /// True while macOS is sleeping (pauses periodic polls until wake).
    sleeping: bool,
    /// Generation counter incremented on config changes to reschedule `Condvar::wait_timeout`.
    config_generation: u64,
    shutdown: bool,
}

/// Single-thread background refresh coordinator with:
/// - periodic scheduling (configurable interval, zero busy-looping via `Condvar::wait_timeout`)
/// - strict request coalescing (never runs two refreshes concurrently)
/// - sleep/wake pause & immediate wake refresh
/// - clean, bounded shutdown
pub struct RefreshScheduler {
    shared: Arc<(Mutex<WorkerState>, Condvar)>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl RefreshScheduler {
    /// Start the single background worker thread.
    /// Does not trigger an immediate refresh until `request_refresh(RefreshReason::Startup)` is called
    /// (or the first periodic interval elapses).
    pub fn start<F, C>(initial_config: RefreshConfig, fetcher: F, on_event: C) -> Self
    where
        F: QuotaFetcher,
        C: Fn(RefreshEvent) + Send + 'static,
    {
        let shared = Arc::new((
            Mutex::new(WorkerState {
                config: initial_config,
                pending_reason: None,
                in_flight: false,
                sleeping: false,
                config_generation: 0,
                shutdown: false,
            }),
            Condvar::new(),
        ));

        let worker_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("ebb-refresh-worker".into())
            .spawn(move || worker_loop(worker_shared, fetcher, on_event))
            .expect("spawn refresh worker thread");

        Self {
            shared,
            worker: Mutex::new(Some(handle)),
        }
    }

    /// Request a refresh. Coalesces automatically if one is already pending or in-flight.
    pub fn request_refresh(&self, reason: RefreshReason) {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.shutdown {
            return;
        }
        // Coalesce: keep at most one queued follow-up refresh; prioritize Manual/Wake over Periodic.
        state.pending_reason = Some(merge_reason(state.pending_reason, reason));
        cvar.notify_one();
    }

    /// Update polling interval and provider visibility; immediately wakes worker to apply new schedule.
    pub fn update_config(&self, config: RefreshConfig) {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.config != config {
            state.config = config;
            state.config_generation = state.config_generation.wrapping_add(1);
            cvar.notify_one();
        }
    }

    /// Pause periodic polling on system sleep; resume and trigger a coalesced refresh on wake.
    pub fn set_sleeping(&self, sleeping: bool) {
        let (lock, cvar) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        if state.sleeping == sleeping {
            return;
        }
        state.sleeping = sleeping;
        if !sleeping && !state.shutdown {
            state.pending_reason = Some(merge_reason(state.pending_reason, RefreshReason::Wake));
        }
        cvar.notify_one();
    }

    /// Stop the background worker thread cleanly.
    pub fn shutdown(&self) {
        {
            let (lock, cvar) = &*self.shared;
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.shutdown = true;
            state.pending_reason = None;
            cvar.notify_all();
        }
        if let Some(handle) = self.worker.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = handle.join();
        }
    }
}

impl Drop for RefreshScheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn merge_reason(existing: Option<RefreshReason>, incoming: RefreshReason) -> RefreshReason {
    match existing {
        None => incoming,
        Some(prev) => {
            if reason_priority(incoming) >= reason_priority(prev) {
                incoming
            } else {
                prev
            }
        }
    }
}

fn reason_priority(reason: RefreshReason) -> u8 {
    match reason {
        RefreshReason::LocalCacheChanged => 0,
        RefreshReason::Periodic => 1,
        RefreshReason::Startup => 2,
        RefreshReason::Wake => 3,
        RefreshReason::Manual => 4,
    }
}

fn worker_loop<F, C>(shared: Arc<(Mutex<WorkerState>, Condvar)>, fetcher: F, on_event: C)
where
    F: QuotaFetcher,
    C: Fn(RefreshEvent) + Send + 'static,
{
    let (lock, cvar) = &*shared;
    let mut last_generation = {
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        guard.config_generation
    };
    let mut next_periodic = {
        let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        Instant::now() + guard.config.interval
    };

    loop {
        let (reason, cfg) = {
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());

            loop {
                if state.shutdown {
                    return;
                }

                // If config changed (e.g. interval shortened from 5m to 30s), recompute next deadline.
                if state.config_generation != last_generation {
                    last_generation = state.config_generation;
                    next_periodic = Instant::now() + state.config.interval;
                }

                // While macOS is asleep, park indefinitely until wake or shutdown.
                if state.sleeping {
                    state = cvar.wait(state).unwrap_or_else(|e| e.into_inner());
                    continue;
                }

                if let Some(pending) = state.pending_reason.take() {
                    state.in_flight = true;
                    break (pending, state.config.clone());
                }

                let now = Instant::now();
                if now >= next_periodic {
                    state.in_flight = true;
                    break (RefreshReason::Periodic, state.config.clone());
                }

                let wait_dur = next_periodic.saturating_duration_since(now);
                let (new_state, _) = cvar
                    .wait_timeout(state, wait_dur)
                    .unwrap_or_else(|e| e.into_inner());
                state = new_state;
            }
        };

        on_event(RefreshEvent::Started(reason));

        // Execute provider reads outside the mutex so incoming requests coalesce without blocking UI.
        let claude = if cfg.show_claude {
            Some(fetcher.fetch_claude(&cfg))
        } else {
            None
        };
        let codex = if cfg.show_codex && reason.fetches_codex() {
            Some(fetcher.fetch_codex(&cfg))
        } else {
            None
        };
        let agy = if cfg.show_agy {
            Some(fetcher.fetch_agy(&cfg))
        } else {
            None
        };

        let outcome = RefreshOutcome {
            reason,
            claude,
            codex,
            agy,
            completed_at: unix_now_secs(),
        };

        let should_emit = {
            let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
            state.in_flight = false;
            last_generation = state.config_generation;
            // Only reset the periodic Codex schedule on full refresh cycles so active Claude
            // status-line bursts never starve Codex polling.
            if reason.fetches_codex() {
                next_periodic = Instant::now() + state.config.interval;
            }
            !state.shutdown
        };

        if should_emit {
            on_event(RefreshEvent::Completed(Box::new(outcome)));
        }
    }
}

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
