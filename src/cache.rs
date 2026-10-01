// two-tier cache for /v1/ip lookups, fully invisible to API consumers:
//   L1: in-process TTL map (instant, per-process)
//   L2: sqlite table when the `db` feature is on (survives restarts)
//
// stale-while-revalidate: a stale entry is served INSTANTLY while a background
// thread refreshes it for the next request — users never wait on a refresh and
// never see cache management. Invalidation is automatic: TTL expiry, background
// revalidation, lazy purge, and stale-if-error fallback when upstream breaks.
//
// stampede protection: a singleflight map lets only one fetch per key run at a
// time; concurrent callers wait and share the leader's result (coalesced).
use crate::server::json;
use std::collections::HashMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

// real sqlite handle under the `db` feature, no-op stub otherwise; everything
// above the storage layer talks to DbHandle and never cares which is active
#[cfg(feature = "db")]
pub type DbHandle = crate::db::Db;
#[cfg(not(feature = "db"))]
pub type DbHandle = db_stub::Db;

#[cfg(not(feature = "db"))]
mod db_stub {
    // mirrors the real Db API so callers compile unchanged without the feature
    pub struct Db;
    impl Db {
        pub fn open(_path: &std::path::Path) -> Result<Self, String> {
            Err("db feature not enabled (rebuild with --features db)".to_string())
        }
        pub fn log_request(&self, _: &str, _: &str, _: &str, _: &str, _: u64, _: &str) {}
        pub fn cache_get(&self, _: &str) -> Option<String> {
            None
        }
        pub fn cache_put(&self, _: &str, _: &str, _: i64) {}
        pub fn cache_invalidate(&self, _: &str) -> bool {
            false
        }
        pub fn cache_purge(&self) {}
        pub fn cache_purge_expired(&self) {}
        pub fn cache_stats(&self) -> (u64, u64) {
            (0, 0)
        }
        pub fn request_count(&self) -> u64 {
            0
        }
    }
}

// open the persistent store; returns None when the feature is off or open fails
pub fn open_db(path: &str) -> Option<Arc<DbHandle>> {
    DbHandle::open(std::path::Path::new(path))
        .ok()
        .map(Arc::new)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    MemoryHit,  // fresh in L1
    DbHit,      // fresh in L2
    Miss,       // had to fetch upstream (caller waited)
    Coalesced,  // shared another concurrent caller's fetch
    StaleReval, // served stale instantly, refreshed in background
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::MemoryHit => "memory_hit",
            Source::DbHit => "db_hit",
            Source::Miss => "miss",
            Source::Coalesced => "coalesced",
            Source::StaleReval => "stale_revalidated",
        }
    }
}

struct Entry {
    payload: Arc<String>,
    created: Instant,
    expires: Instant,     // fresh until
    stale_until: Instant, // serve-while-revalidating until
}

struct Shared {
    result: Option<Result<Arc<String>, String>>,
    done: bool,
}

struct Flight {
    shared: Arc<(Mutex<Shared>, Condvar)>,
}

pub struct CacheConfig {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub max_entries: usize,
    pub stale_grace_secs: u64, // how long a stale entry stays servable
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_secs: 900,          // IP geo data moves slowly; 15 min is plenty
            max_entries: 2048,      // small-RAM friendly (~each entry < 1 KB)
            stale_grace_secs: 3600, // up to 1h stale while refreshing
        }
    }
}

pub struct Cache {
    enabled: bool,
    config: CacheConfig,
    mem: Mutex<HashMap<String, Entry>>,
    // singleflight: key -> in-flight fetch; joiners wait on the condvar
    flights: Mutex<HashMap<String, Flight>>,
    db: Option<Arc<DbHandle>>,
    stats: Stats,
}

#[derive(Default)]
struct Stats {
    hits: Mutex<u64>,
    misses: Mutex<u64>,
    db_hits: Mutex<u64>,
    coalesced: Mutex<u64>,
    revalidations: Mutex<u64>,
    stale_served: Mutex<u64>,
    fetch_errors: Mutex<u64>,
}

impl Stats {
    fn bump(counter: &Mutex<u64>) {
        if let Ok(mut value) = counter.lock() {
            *value = value.saturating_add(1);
        }
    }

    fn to_json(&self) -> String {
        let get = |counter: &Mutex<u64>| counter.lock().map(|v| *v).unwrap_or(0);
        json::object(&[
            ("hits", get(&self.hits).to_string()),
            ("misses", get(&self.misses).to_string()),
            ("db_hits", get(&self.db_hits).to_string()),
            ("coalesced", get(&self.coalesced).to_string()),
            ("background_revalidations", get(&self.revalidations).to_string()),
            ("stale_served", get(&self.stale_served).to_string()),
            ("fetch_errors", get(&self.fetch_errors).to_string()),
        ])
    }
}

impl Cache {
    pub fn new(config: CacheConfig, db: Option<Arc<DbHandle>>) -> Self {
        let enabled = config.enabled;
        Cache {
            enabled,
            config,
            mem: Mutex::new(HashMap::new()),
            flights: Mutex::new(HashMap::new()),
            db,
            stats: Stats::default(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn key_for_ip(ip: &str) -> String {
        format!("ip:{}", ip)
    }

    // The single entry point used by /v1/ip. Returns the payload plus where it
    // came from (for the request log only — callers never surface this).
    // Never blocks on a background refresh; only a cold miss waits on upstream.
    // Takes `&Arc<Self>` so background revalidation can hold a real handle.
    pub fn get_or_fetch<F>(self: &Arc<Self>, key: &str, fetch: F) -> (Arc<String>, Source)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        if !self.enabled {
            // pass-through: fetch, log the miss, store nothing
            return match fetch() {
                Ok(text) => {
                    Stats::bump(&self.stats.misses);
                    (Arc::new(text), Source::Miss)
                }
                Err(_) => {
                    Stats::bump(&self.stats.fetch_errors);
                    (Arc::new(String::new()), Source::Miss)
                }
            };
        }
        // L1 fresh?
        if let Some(entry) = self.mem_get(key) {
            if Instant::now() < entry.expires {
                Stats::bump(&self.stats.hits);
                return (entry.payload, Source::MemoryHit);
            }
            // stale but servable? -> serve instantly + revalidate in background
            if Instant::now() < entry.stale_until {
                Stats::bump(&self.stats.stale_served);
                self.spawn_revalidation(key.to_string(), fetch);
                return (entry.payload, Source::StaleReval);
            }
            // too old to serve: fall through to a blocking fetch
        }

        // join an in-flight fetch for the same key (stampede coalescing)
        if let Some(payload) = self.join_flight(key) {
            return payload;
        }

        // L2 (sqlite) before doing the expensive fetch
        if let Some(db) = &self.db {
            if let Some(payload) = db.cache_get(key) {
                Stats::bump(&self.stats.db_hits);
                let now = Instant::now();
                self.mem_insert(key.to_string(), Entry {
                    payload: Arc::new(payload.clone()),
                    created: now,
                    expires: now + Duration::from_secs(self.config.ttl_secs),
                    stale_until: now + Duration::from_secs(self.config.ttl_secs + self.config.stale_grace_secs),
                });
                return (Arc::new(payload), Source::DbHit);
            }
        }

        // cold miss: lead the fetch ourselves (caller waits — this is the miss path)
        self.lead_flight(key, fetch, true)
    }

    // stale-while-revalidate: serve old value now, refresh on a worker thread
    fn spawn_revalidation<F>(self: &Arc<Self>, key: String, fetch: F)
    where
        F: FnOnce() -> Result<String, String> + Send + 'static,
    {
        // don't stack revalidations: skip if one is already running
        {
            let flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            if flights.contains_key(&key) {
                return;
            }
        }

        Stats::bump(&self.stats.revalidations);
        let cache = Arc::clone(self);
        let _ = thread::Builder::new()
            .name("cache-revalidate".to_string())
            .spawn(move || {
                cache.lead_flight(&key, fetch, false);
            });
    }

    // singleflight core. when `blocking` is true the current thread does the
    // fetch and the CALLER waits for it; when false (background revalidation)
    // the current thread fetches and nobody waits on us.
    fn lead_flight<F>(&self, key: &str, fetch: F, blocking: bool) -> (Arc<String>, Source)
    where
        F: FnOnce() -> Result<String, String>,
    {
        let shared = Arc::new((
            Mutex::new(Shared {
                result: None,
                done: false,
            }),
            Condvar::new(),
        ));

        {
            let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            // another leader may have raced us between check and registration
            if let Some(existing) = flights.get(key) {
                let joined = Arc::clone(&existing.shared);
                drop(flights);
                if blocking {
                    if let Some(result) = wait_on_flight(&joined) {
                        Stats::bump(&self.stats.coalesced);
                        return result;
                    }
                    // leader failed: do it ourselves
                    return self.fetch_and_store(key, fetch);
                }
                return self.fetch_and_store(key, fetch);
            }
            flights.insert(
                key.to_string(),
                Flight {
                    shared: Arc::clone(&shared),
                },
            );
        }

        let result = match fetch() {
            Ok(text) => {
                let payload = Arc::new(text);
                self.store(key, &payload);
                Stats::bump(&self.stats.misses);
                Some((payload, Source::Miss))
            }
            Err(_) => {
                Stats::bump(&self.stats.fetch_errors);
                // stale-if-error: better an old answer than no answer
                match self.mem_get(key).filter(|e| Instant::now() < e.stale_until) {
                    Some(entry) => Some((entry.payload, Source::StaleReval)),
                    None => None,
                }
            }
        };

        // publish to joiners + deregister
        {
            let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            flights.remove(key);
        }
        {
            let (lock, cvar) = (&shared.0, &shared.1);
            let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            guard.result = result.clone().map(|(p, _)| Ok(p)).or(Some(Err("fetch failed".to_string())));
            guard.done = true;
            cvar.notify_all();
        }

        match result {
            Some((payload, source)) => (payload, source),
            None => (Arc::new(String::new()), Source::Miss),
        }
    }

    // try to join an existing flight; None when none exists (or it failed)
    fn join_flight(&self, key: &str) -> Option<(Arc<String>, Source)> {
        let shared = {
            let flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
            let flight = flights.get(key)?;
            Arc::clone(&flight.shared)
        };
        Stats::bump(&self.stats.coalesced);
        wait_on_flight(&shared)
    }

    fn fetch_and_store<F>(&self, key: &str, fetch: F) -> (Arc<String>, Source)
    where
        F: FnOnce() -> Result<String, String>,
    {
        match fetch() {
            Ok(text) => {
                let payload = Arc::new(text);
                self.store(key, &payload);
                Stats::bump(&self.stats.misses);
                (payload, Source::Miss)
            }
            Err(_) => {
                Stats::bump(&self.stats.fetch_errors);
                match self.mem_get(key).filter(|e| Instant::now() < e.stale_until) {
                    Some(entry) => (entry.payload, Source::StaleReval),
                    None => (Arc::new(String::new()), Source::Miss),
                }
            }
        }
    }

    fn wait_result(shared: &Arc<(Mutex<Shared>, Condvar)>) -> Option<(Arc<String>, Source)> {
        let (lock, cvar) = (&shared.0, &shared.1);
        let mut guard = lock.lock().unwrap_or_else(|e| e.into_inner());
        while !guard.done {
            guard = cvar.wait(guard).unwrap_or_else(|e| e.into_inner());
        }
        match guard.result.as_ref() {
            Some(Ok(payload)) => Some((Arc::clone(payload), Source::Coalesced)),
            _ => None,
        }
    }

    fn store(&self, key: &str, payload: &str) {
        let now = Instant::now();
        self.mem_insert(key.to_string(), Entry {
            payload: Arc::new(payload.to_string()),
            created: now,
            expires: now + Duration::from_secs(self.config.ttl_secs),
            stale_until: now + Duration::from_secs(self.config.ttl_secs + self.config.stale_grace_secs),
        });
        if let Some(db) = &self.db {
            db.cache_put(key, payload, self.config.ttl_secs as i64);
        }
    }

    fn mem_get(&self, key: &str) -> Option<Entry> {
        let mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        mem.get(key).map(|e| Entry {
            payload: Arc::clone(&e.payload),
            created: e.created,
            expires: e.expires,
            stale_until: e.stale_until,
        })
    }

    fn mem_insert(&self, key: String, entry: Entry) {
        let mut mem = self.mem.lock().unwrap_or_else(|e| e.into_inner());
        // cheap size cap: drop expired first, then oldest
        if mem.len() >= self.config.max_entries {
            let now = Instant::now();
            mem.retain(|_, e| now < e.stale_until);
            while mem.len() >= self.config.max_entries {
                let oldest = mem
                    .iter()
                    .min_by_key(|(_, e)| e.created)
                    .map(|(k, _)| k.clone());
                match oldest {
                    Some(k) => {
                        mem.remove(&k);
                    }
                    None => break,
                }
            }
        }
        mem.insert(key, entry);
    }

    pub fn has_db(&self) -> bool {
        self.db.is_some()
    }

    // drop expired entries from L1 and the persistent layer (janitor thread)
    pub fn purge_expired(&self) {
        let now = Instant::now();
        self.mem
            .lock()
            .map(|mut m| m.retain(|_, e| now < e.stale_until))
            .ok();
        if let Some(db) = &self.db {
            db.cache_purge_expired();
        }
    }

    pub fn log_request(&self, endpoint: &str, params: &str, status: &str, source: &str, elapsed_ms: u64, client: &str) {
        if let Some(db) = &self.db {
            db.log_request(endpoint, params, status, source, elapsed_ms, client);
        }
    }

    // (db cache entries, db cache hits, logged requests)
    pub fn db_stats(&self) -> (u64, u64, u64) {
        match &self.db {
            Some(db) => {
                let (entries, hits) = db.cache_stats();
                (entries, hits, db.request_count())
            }
            None => (0, 0, 0),
        }
    }

    pub fn to_json(&self) -> String {
        let (db_entries, db_hits, requests) = self.db_stats();
        json::object(&[
            ("ttl_secs", self.config.ttl_secs.to_string()),
            ("max_entries", self.config.max_entries.to_string()),
            ("memory_entries", {
                self.mem.lock().map(|m| m.len()).unwrap_or(0).to_string()
            }),
            ("persistent", if self.db.is_some() { "true" } else { "false" }.to_string()),
            ("db_cache_entries", db_entries.to_string()),
            ("db_cache_hits", db_hits.to_string()),
            ("logged_requests", requests.to_string()),
            ("stats", self.stats.to_json()),
        ])
    }
}

fn wait_on_flight(shared: &Arc<(Mutex<Shared>, Condvar)>) -> Option<(Arc<String>, Source)> {
    Cache::wait_result(shared)
}

use std::thread;
