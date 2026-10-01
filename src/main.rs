mod browser;
mod cache;
mod config;
mod fingerprint;
mod ratelimit;
mod server;
mod solver;
mod proxy;
mod ws;
mod tui;

#[cfg(feature = "db")]
mod db;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

const TURNSTILE_UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

fn main() {
    let config = Arc::new(config::Config::load());
    let headless = config.headless;

    tui::banner(config.port, config.browsers, config.tabs);
    println!(
        "[System] auto-sized: {} browser(s) x {} tab(s) = {} parallel solves (timeout {} ms, headless: {})",
        config.browsers,
        config.tabs,
        config.browsers * config.tabs,
        config.solve_timeout_ms,
        headless
    );
    if let Some(path) = &config.config_path {
        println!("[System] config file: {}", path.display());
    }
    if !config.proxies.is_empty() {
        println!("[System] proxies configured: {}", config.proxies.len());
    }
    if config.api_token.is_some() {
        println!("[System] api auth: enabled (Authorization: Bearer <token>)");
    } else {
        println!("[System] api auth: disabled (set api_token in config to enable)");
    }
    if config.rate_limit.enabled {
        println!(
            "[System] rate limit: burst {}, refill {}/s per client",
            config.rate_limit.capacity, config.rate_limit.refill_per_sec
        );
    } else {
        println!("[System] rate limit: disabled");
    }

    // optional persistent layer (sqlite via the `db` feature)
    let db = match &config.db_path {
        Some(path) => match cache::open_db(path) {
            Some(db) => {
                println!("[System] sqlite db: {} (request log + persistent cache)", path);
                Some(db)
            }
            None => {
                eprintln!(
                    "[Warning] failed to open sqlite db at {} (was the binary built with --features db?); continuing without persistence",
                    path
                );
                None
            }
        },
        None => None,
    };

    let cache = Arc::new(cache::Cache::new(
        cache::CacheConfig {
            enabled: config.cache.enabled,
            ttl_secs: config.cache.ttl_secs,
            max_entries: config.cache.max_entries,
            stale_grace_secs: config.cache.stale_grace_secs,
        },
        db,
    ));
    if config.cache.enabled {
        println!(
            "[System] cache: {}s ttl, max {} entries (stale-while-revalidate, singleflight)",
            config.cache.ttl_secs, config.cache.max_entries
        );
    } else {
        println!("[System] cache: disabled");
    }

    // empty fallback lets the service still boot if the fetch fails
    let turnstile_script = solver::fetch_turnstile_script().unwrap_or_else(|err| {
        eprintln!(
            "[Warning] Failed to dynamically fetch Turnstile script: {}. Falling back to empty script.",
            err
        );
        String::new()
    });
    let turnstile_script = Arc::<str>::from(turnstile_script.as_str());

    let service = Arc::new(solver::SolverPool::new(
        Duration::from_millis(config.solve_timeout_ms),
        config.browsers,
        config.tabs,
        headless,
        turnstile_script,
    ));

    let limiter = Arc::new(ratelimit::RateLimiter::new(
        config.rate_limit.enabled,
        config.rate_limit.capacity,
        config.rate_limit.refill_per_sec,
    ));

    // ctrl-c / sigterm -> set the flag so worker loops tear down cleanly
    if let Err(err) = shutdown::install() {
        eprintln!("[Warning] Failed to install shutdown handler: {}", err);
    }

    if config.prewarm {
        start_browser_prewarm(Arc::clone(&service), headless);
    }
    start_turnstile_updater(Arc::clone(&service));

    if let Err(err) = server::serve(Arc::clone(&config), Arc::clone(&service), limiter, Arc::clone(&cache)) {
        panic!("HTTP server failed: {}", err);
    }

    service.shutdown();
}

fn start_browser_prewarm(service: Arc<solver::SolverPool>, headless: bool) {
    thread::spawn(move || {
        if let Err(err) = service.prewarm(headless) {
            eprintln!("[Warning] Browser prewarm failed: {}", err);
        }
    });
}

fn start_turnstile_updater(service: Arc<solver::SolverPool>) {
    thread::spawn(move || {
        while !shutdown::is_requested() {
            sleep_until_update_or_shutdown(TURNSTILE_UPDATE_INTERVAL);
            if shutdown::is_requested() {
                break;
            }

            match solver::fetch_turnstile_script() {
                Ok(script) => service.set_turnstile_script(Arc::<str>::from(script.as_str())),
                Err(err) => eprintln!("[Updater] Update failed: {}", err),
            }
        }
    });
}

// sleep in 1s chunks so a shutdown request doesn't wait out the full interval
fn sleep_until_update_or_shutdown(duration: Duration) {
    let mut slept = Duration::ZERO;
    while slept < duration && !shutdown::is_requested() {
        let remaining = duration.saturating_sub(slept);
        let step = remaining.min(Duration::from_secs(1));
        thread::sleep(step);
        slept += step;
    }
}

mod base64 {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &[u8]) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        let mut i = 0;

        while i < input.len() {
            let b0 = input[i];
            let b1 = if i + 1 < input.len() { input[i + 1] } else { 0 };
            let b2 = if i + 2 < input.len() { input[i + 2] } else { 0 };

            out.push(TABLE[(b0 >> 2) as usize] as char);
            out.push(TABLE[(((b0 & 0b0000_0011) << 4) | (b1 >> 4)) as usize] as char);

            if i + 1 < input.len() {
                out.push(TABLE[(((b1 & 0b0000_1111) << 2) | (b2 >> 6)) as usize] as char);
            } else {
                out.push('=');
            }

            if i + 2 < input.len() {
                out.push(TABLE[(b2 & 0b0011_1111) as usize] as char);
            } else {
                out.push('=');
            }

            i += 3;
        }

        out
    }
}

mod shutdown {
    use std::sync::atomic::{AtomicBool, Ordering};

    static SHUTDOWN: AtomicBool = AtomicBool::new(false);

    pub fn install() -> Result<(), String> {
        platform::install()
    }

    pub fn is_requested() -> bool {
        SHUTDOWN.load(Ordering::SeqCst)
    }

    fn request_shutdown() {
        SHUTDOWN.store(true, Ordering::SeqCst);
    }

    #[cfg(windows)]
    mod platform {
        use super::request_shutdown;

        const CTRL_C_EVENT: u32 = 0;
        const CTRL_BREAK_EVENT: u32 = 1;
        const CTRL_CLOSE_EVENT: u32 = 2;
        const CTRL_LOGOFF_EVENT: u32 = 5;
        const CTRL_SHUTDOWN_EVENT: u32 = 6;

        type HandlerRoutine = unsafe extern "system" fn(u32) -> i32;

        #[link(name = "Kernel32")]
        extern "system" {
            fn SetConsoleCtrlHandler(handler: Option<HandlerRoutine>, add: i32) -> i32;
        }

        pub fn install() -> Result<(), String> {
            let ok = unsafe { SetConsoleCtrlHandler(Some(handler), 1) };
            if ok == 0 {
                return Err("SetConsoleCtrlHandler failed".to_string());
            }
            Ok(())
        }

        unsafe extern "system" fn handler(event: u32) -> i32 {
            match event {
                CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
                | CTRL_SHUTDOWN_EVENT => {
                    request_shutdown();
                    1
                }
                _ => 0,
            }
        }
    }

    #[cfg(unix)]
    mod platform {
        use super::request_shutdown;

        const SIGINT: i32 = 2;
        const SIGTERM: i32 = 15;

        type SignalHandler = extern "C" fn(i32);

        extern "C" {
            fn signal(signum: i32, handler: SignalHandler) -> SignalHandler;
        }

        pub fn install() -> Result<(), String> {
            unsafe {
                signal(SIGINT, handler);
                signal(SIGTERM, handler);
            }
            Ok(())
        }

        extern "C" fn handler(_: i32) {
            request_shutdown();
        }
    }

    #[cfg(not(any(unix, windows)))]
    mod platform {
        pub fn install() -> Result<(), String> {
            Ok(())
        }
    }
}
