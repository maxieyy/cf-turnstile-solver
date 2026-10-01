// server configuration, all of it tweakable via env or /etc/solver/config.json.
// defaults auto-size for small VPS boxes (tested on 881 MiB RAM, 1 vCPU).
use crate::server::json;
use std::fs;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct RateLimitConfig {
    pub enabled: bool,
    pub capacity: f64,        // bucket size (burst)
    pub refill_per_sec: f64,  // tokens per second
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capacity: 10.0,
            refill_per_sec: 0.5, // ~30 req/min sustained
        }
    }
}

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub enabled: bool,
    pub ttl_secs: u64,
    pub max_entries: usize,
    pub stale_grace_secs: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            ttl_secs: 900,
            max_entries: 2048,
            stale_grace_secs: 3600,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub host: String,         // default 127.0.0.1; caddy terminates TLS on top
    pub port: u16,            // internal port caddy reverse-proxies to
    pub browsers: usize,      // chrome processes
    pub tabs: usize,          // contexts per browser
    pub solve_timeout_ms: u64,
    pub headless: bool,
    pub prewarm: bool,
    pub rate_limit: RateLimitConfig,
    pub api_token: Option<String>, // if set, endpoints require `Authorization: Bearer <token>`
    pub proxies: Vec<String>,      // round-robin; empty = direct egress
    pub db_path: Option<String>,   // sqlite file; requires the `db` feature
    pub cache: CacheConfig,
    pub config_path: Option<PathBuf>,
}

impl Default for Config {
    fn default() -> Self {
        let (browsers, tabs) = auto_size();
        Self {
            host: "127.0.0.1".to_string(),
            port: 8907,
            browsers,
            tabs,
            solve_timeout_ms: 29_000,
            headless: true,
            prewarm: true,
            rate_limit: RateLimitConfig::default(),
            api_token: None,
            proxies: Vec::new(),
            db_path: None,
            cache: CacheConfig::default(),
            config_path: None,
        }
    }
}

// pick sane defaults from the machine: RAM decides browser count, cores decide
// tabs. tuned so an 881 MiB / 1 vCPU box boots 1 browser with 4 tabs and still
// has room for caddy + the OS.
fn auto_size() -> (usize, usize) {
    let total_mib = total_ram_mib().unwrap_or(1024);
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    let browsers: u64 = if total_mib < 1500 {
        1
    } else if total_mib < 3500 {
        2
    } else {
        2 + (total_mib - 3500) / 2500
    }
    .clamp(1, 8);

    let tabs: u64 = if cores <= 1 {
        4
    } else if cores <= 2 {
        6
    } else {
        10
    }
    .clamp(2, 30);

    (browsers as usize, tabs as usize)
}

fn total_ram_mib() -> Option<u64> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|line| line.starts_with("MemTotal:"))?;
    let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kib / 1024)
}

impl Config {
    // precedence: defaults < config file < env
    pub fn load() -> Self {
        let mut config = Config::default();

        let path = Config::resolve_config_path();
        if let Some(path) = &path {
            match fs::read_to_string(path) {
                Ok(text) => config.apply_json(&text),
                Err(_) => {}
            }
        }
        config.config_path = path;

        config.apply_env();
        config
    }

    fn resolve_config_path() -> Option<PathBuf> {
        if let Ok(path) = std::env::var("CONFIG") {
            if !path.trim().is_empty() {
                return Some(PathBuf::from(path));
            }
        }
        for candidate in ["/etc/solver/config.json", "config.json"] {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
        None
    }

    fn apply_json(&mut self, text: &str) {
        let Ok(value) = json::parse(text) else {
            eprintln!("[Config] config file is not valid JSON, ignoring");
            return;
        };

        if let Some(v) = value.get("host").and_then(|v| v.as_str()) {
            self.host = v.to_string();
        }
        if let Some(v) = value.get("port").and_then(|v| v.as_u64()) {
            self.port = (v as u16).max(1);
        }
        if let Some(v) = value.get("browsers").and_then(|v| v.as_u64()) {
            self.browsers = (v as usize).clamp(1, 16);
        }
        if let Some(v) = value.get("tabs").and_then(|v| v.as_u64()) {
            self.tabs = (v as usize).clamp(1, 50);
        }
        if let Some(v) = value.get("timeout_ms").and_then(|v| v.as_u64()) {
            self.solve_timeout_ms = v;
        }
        if let Some(v) = value.get("headless") {
            self.headless = !is_false(v);
        }
        if let Some(v) = value.get("prewarm") {
            self.prewarm = !is_false(v);
        }
        if let Some(v) = value.get("api_token").and_then(|v| v.as_str()) {
            self.api_token = Some(v.to_string()).filter(|s| !s.is_empty());
        }
        if let Some(v) = value.get("proxies").and_then(|v| v.as_array()) {
            self.proxies = v
                .iter()
                .filter_map(|p| p.as_str().map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(rl) = value.get("rate_limit") {
            if let Some(v) = rl.get("enabled") {
                self.rate_limit.enabled = !is_false(v);
            }
            if let Some(v) = rl.get("capacity").and_then(|v| v.as_f64()) {
                self.rate_limit.capacity = v.max(1.0);
            }
            if let Some(v) = rl.get("refill_per_sec").and_then(|v| v.as_f64()) {
                self.rate_limit.refill_per_sec = v.max(0.0);
            }
        }
        if let Some(v) = value.get("db_path").and_then(|v| v.as_str()) {
            self.db_path = Some(v.to_string()).filter(|s| !s.is_empty());
        }
        if let Some(c) = value.get("cache") {
            if let Some(v) = c.get("enabled") {
                self.cache.enabled = !is_false(v);
            }
            if let Some(v) = c.get("ttl_secs").and_then(|v| v.as_u64()) {
                self.cache.ttl_secs = v.max(1);
            }
            if let Some(v) = c.get("max_entries").and_then(|v| v.as_u64()) {
                self.cache.max_entries = (v as usize).max(8);
            }
            if let Some(v) = c.get("stale_grace_secs").and_then(|v| v.as_u64()) {
                self.cache.stale_grace_secs = v;
            }
        }
    }

    fn apply_env(&mut self) {
        if let Some(v) = env_string("HOST") {
            self.host = v;
        }
        if let Some(v) = env_usize("PORT") {
            self.port = (v as u16).max(1);
        }
        if let Some(v) = env_usize("BROWSERS") {
            self.browsers = v.clamp(1, 16);
        }
        if let Some(v) = env_usize("TABS") {
            self.tabs = v.clamp(1, 50);
        }
        if let Some(v) = env_usize("timeOut").or_else(|| env_usize("TIMEOUT_MS")) {
            self.solve_timeout_ms = v as u64;
        }
        if let Some(v) = env_bool("HEADLESS").or_else(|| env_bool("headless")) {
            self.headless = v;
        }
        if let Some(v) = env_bool("PREWARM_BROWSER") {
            self.prewarm = v;
        }
        if let Some(v) = env_string("API_TOKEN") {
            self.api_token = Some(v).filter(|s| !s.is_empty());
        }
        if let Some(v) = env_string("PROXIES") {
            self.proxies = v
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect();
        }
        if let Some(v) = env_bool("RATE_LIMIT_ENABLED") {
            self.rate_limit.enabled = v;
        }
        if let Some(v) = env_f64("RATE_LIMIT_CAPACITY") {
            self.rate_limit.capacity = v.max(1.0);
        }
        if let Some(v) = env_f64("RATE_LIMIT_REFILL_PER_SEC") {
            self.rate_limit.refill_per_sec = v.max(0.0);
        }
        if let Some(v) = env_string("DB_PATH") {
            self.db_path = Some(v);
        }
        if let Some(v) = env_bool("CACHE_ENABLED") {
            self.cache.enabled = v;
        }
        if let Some(v) = env_usize("CACHE_TTL_SECS") {
            self.cache.ttl_secs = v.max(1) as u64;
        }
        if let Some(v) = env_usize("CACHE_MAX_ENTRIES") {
            self.cache.max_entries = v.max(8);
        }
    }

    pub fn to_json(&self) -> String {
        let (capacity, refill) = (
            format_float(self.rate_limit.capacity),
            format_float(self.rate_limit.refill_per_sec),
        );
        let rl = json::object(&[
            ("enabled", bool_string(self.rate_limit.enabled)),
            ("capacity", capacity),
            ("refill_per_sec", refill),
        ]);
        let proxies = format!(
            "[{}]",
            self.proxies
                .iter()
                .map(|p| json::string(p))
                .collect::<Vec<_>>()
                .join(",")
        );
        let cache = json::object(&[
            ("enabled", bool_string(self.cache.enabled)),
            ("ttl_secs", self.cache.ttl_secs.to_string()),
            ("max_entries", self.cache.max_entries.to_string()),
            ("stale_grace_secs", self.cache.stale_grace_secs.to_string()),
        ]);
        json::object(&[
            ("host", json::string(&self.host)),
            ("port", self.port.to_string()),
            ("browsers", self.browsers.to_string()),
            ("tabs", self.tabs.to_string()),
            ("timeout_ms", self.solve_timeout_ms.to_string()),
            ("headless", bool_string(self.headless)),
            ("prewarm", bool_string(self.prewarm)),
            (
                "capacity_solves",
                (self.browsers * self.tabs).to_string(),
            ),
            ("api_token_set", bool_string(self.api_token.is_some())),
            ("proxies", proxies),
            ("rate_limit", rl),
            (
                "db_path",
                match &self.db_path {
                    Some(path) => json::string(path),
                    None => "null".to_string(),
                },
            ),
            ("cache", cache),
            (
                "config_path",
                match &self.config_path {
                    Some(path) => json::string(&path.to_string_lossy()),
                    None => "null".to_string(),
                },
            ),
        ])
    }
}

fn is_false(value: &json::Value) -> bool {
    matches!(value, json::Value::Bool(false))
}

fn format_float(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{:.0}", value)
    } else {
        value.to_string()
    }
}

fn bool_string(value: bool) -> String {
    if value {
        "true".to_string()
    } else {
        "false".to_string()
    }
}

fn env_string(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn env_usize(name: &str) -> Option<usize> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn env_bool(name: &str) -> Option<bool> {
    match std::env::var(name) {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        Err(_) => None,
    }
}
