// sqlite persistence, compiled only with the `db` feature. stores two things:
//   1. request log  - every /v1/ip lookup with timing and cache status
//   2. result cache - persistent layer under the memory cache, survives restarts
// raw FFI against the vendored amalgamation; no external crates.
#![allow(non_camel_case_types)]

use std::ffi::{c_char, c_double, c_int, CStr, CString};
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

extern "C" {
    fn sqlite3_open_v2(
        filename: *const c_char,
        db: *mut *mut sqlite3,
        flags: c_int,
        vfs: *const c_char,
    ) -> c_int;
    fn sqlite3_close_v2(db: *mut sqlite3) -> c_int;
    fn sqlite3_prepare_v2(
        db: *mut sqlite3,
        sql: *const c_char,
        n_byte: c_int,
        stmt: *mut *mut sqlite3_stmt,
        tail: *mut *const c_char,
    ) -> c_int;
    fn sqlite3_step(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_finalize(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_bind_text(stmt: *mut sqlite3_stmt, index: c_int, value: *const c_char, n: c_int, destructor: isize) -> c_int;
    fn sqlite3_bind_int64(stmt: *mut sqlite3_stmt, index: c_int, value: i64) -> c_int;
    fn sqlite3_bind_double(stmt: *mut sqlite3_stmt, index: c_int, value: c_double) -> c_int;
    fn sqlite3_column_text(stmt: *mut sqlite3_stmt, col: c_int) -> *const u8;
    fn sqlite3_column_int64(stmt: *mut sqlite3_stmt, col: c_int) -> i64;
    fn sqlite3_column_double(stmt: *mut sqlite3_stmt, col: c_int) -> c_double;
    fn sqlite3_column_type(stmt: *mut sqlite3_stmt, col: c_int) -> c_int;
    fn sqlite3_column_count(stmt: *mut sqlite3_stmt) -> c_int;
    fn sqlite3_errmsg(db: *mut sqlite3) -> *const c_char;
    fn sqlite3_exec(db: *mut sqlite3, sql: *const c_char, cb: isize, ctx: *mut u8, err: *mut *const c_char) -> c_int;
    fn sqlite3_busy_timeout(db: *mut sqlite3, ms: c_int) -> c_int;
}

#[repr(C)]
struct sqlite3 {
    _private: [u8; 0],
}
#[repr(C)]
struct sqlite3_stmt {
    _private: [u8; 0],
}

const SQLITE_OK: c_int = 0;
const SQLITE_ROW: c_int = 100;
const SQLITE_DONE: c_int = 101;
const SQLITE_OPEN_READWRITE: c_int = 0x00000002;
const SQLITE_OPEN_CREATE: c_int = 0x00000004;
const SQLITE_FULLMUTEX: c_int = 0x00010000;
const SQLITE_NULL: c_int = 5;
// SQLITE_TRANSIENT
const TRANSIENT: isize = -1;

pub struct Db {
    conn: Mutex<*mut sqlite3>,
}

unsafe impl Send for Db {}
unsafe impl Sync for Db {}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Db {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let cpath = CString::new(path.to_string_lossy().as_bytes())
            .map_err(|_| "bad db path".to_string())?;
        let mut conn: *mut sqlite3 = std::ptr::null_mut();
        let flags = SQLITE_OPEN_READWRITE | SQLITE_OPEN_CREATE | SQLITE_FULLMUTEX;
        let rc = unsafe { sqlite3_open_v2(cpath.as_ptr(), &mut conn, flags, std::ptr::null()) };
        if rc != SQLITE_OK {
            let msg = if conn.is_null() {
                format!("sqlite open failed rc={}", rc)
            } else {
                let raw = unsafe { sqlite3_errmsg(conn) };
                unsafe { CStr::from_ptr(raw) }.to_string_lossy().to_string()
            };
            return Err(msg);
        }
        unsafe { sqlite3_busy_timeout(conn, 2000) };

        let db = Db {
            conn: Mutex::new(conn),
        };
        db.exec_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS requests (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 created_at INTEGER NOT NULL,
                 endpoint TEXT NOT NULL,
                 params TEXT NOT NULL,
                 status TEXT NOT NULL,
                 source TEXT NOT NULL,
                 elapsed_ms INTEGER NOT NULL,
                 client TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_requests_created ON requests(created_at DESC);
             CREATE TABLE IF NOT EXISTS cache (
                 key TEXT PRIMARY KEY,
                 payload TEXT NOT NULL,
                 created_at INTEGER NOT NULL,
                 expires_at INTEGER NOT NULL,
                 hits INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS idx_cache_expires ON cache(expires_at);",
        )?;
        Ok(db)
    }

    fn exec_batch(&self, sql: &str) -> Result<(), String> {
        let csql = CString::new(sql).map_err(|_| "bad sql".to_string())?;
        let mut err: *const c_char = std::ptr::null();
        let rc = unsafe {
            sqlite3_exec(*self.conn.lock().unwrap(), csql.as_ptr(), 0, std::ptr::null_mut(), &mut err)
        };
        if rc != SQLITE_OK {
            let msg = if err.is_null() {
                format!("sqlite exec failed rc={}", rc)
            } else {
                unsafe { CStr::from_ptr(err) }.to_string_lossy().to_string()
            };
            return Err(msg);
        }
        Ok(())
    }

    fn query(
        &self,
        sql: &str,
        binds: &[Bind],
    ) -> Result<Vec<Vec<Value>>, String> {
        let csql = CString::new(sql).map_err(|_| "bad sql".to_string())?;
        let conn = *self.conn.lock().unwrap();
        let mut stmt: *mut sqlite3_stmt = std::ptr::null_mut();
        let rc = unsafe {
            sqlite3_prepare_v2(conn, csql.as_ptr(), -1, &mut stmt, std::ptr::null_mut())
        };
        if rc != SQLITE_OK {
            let raw = unsafe { sqlite3_errmsg(conn) };
            return Err(unsafe { CStr::from_ptr(raw) }.to_string_lossy().to_string());
        }
        let _guard = Finalize(stmt);

        for (index, bind) in binds.iter().enumerate() {
            let idx = (index + 1) as c_int;
            let rc = match bind {
                Bind::Text(s) => {
                    let cval = CString::new(s.as_bytes()).map_err(|_| "bad bind text")?;
                    unsafe { sqlite3_bind_text(stmt, idx, cval.as_ptr(), s.len() as c_int, TRANSIENT) }
                }
                Bind::Int(v) => unsafe { sqlite3_bind_int64(stmt, idx, *v) },
                Bind::Float(v) => unsafe { sqlite3_bind_double(stmt, idx, *v) },
            };
            if rc != SQLITE_OK {
                let raw = unsafe { sqlite3_errmsg(conn) };
                return Err(unsafe { CStr::from_ptr(raw) }.to_string_lossy().to_string());
            }
        }

        let mut rows = Vec::new();
        loop {
            let rc = unsafe { sqlite3_step(stmt) };
            if rc == SQLITE_ROW {
                let mut row = Vec::new();
                let columns = unsafe { sqlite3_column_count(stmt) as usize };
                for col in 0..columns {
                    row.push(read_column(stmt, col));
                }
                rows.push(row);
            } else if rc == SQLITE_DONE {
                break;
            } else {
                let raw = unsafe { sqlite3_errmsg(conn) };
                return Err(unsafe { CStr::from_ptr(raw) }.to_string_lossy().to_string());
            }
        }
        Ok(rows)
    }

    fn execute(&self, sql: &str, binds: &[Bind]) -> Result<(), String> {
        self.query(sql, binds).map(|_| ())
    }

    // ---- request log ----

    pub fn log_request(
        &self,
        endpoint: &str,
        params: &str,
        status: &str,
        source: &str,
        elapsed_ms: u64,
        client: &str,
    ) {
        let _ = self.execute(
            "INSERT INTO requests (created_at, endpoint, params, status, source, elapsed_ms, client)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[
                Bind::Int(now_epoch()),
                Bind::Text(endpoint.to_string()),
                Bind::Text(params.chars().take(200).collect()),
                Bind::Text(status.to_string()),
                Bind::Text(source.to_string()),
                Bind::Int(elapsed_ms as i64),
                Bind::Text(client.chars().take(64).collect()),
            ],
        );
    }

    // ---- persistent cache ----

    // Some(payload) on a fresh row; bumps its hit counter
    pub fn cache_get(&self, key: &str) -> Option<String> {
        let rows = self
            .query(
                "SELECT payload, expires_at FROM cache WHERE key = ?1",
                &[Bind::Text(key.to_string())],
            )
            .ok()?;
        let row = rows.first()?;
        let payload = row.first()?.as_text()?.clone();
        let expires_at = row.get(1).and_then(|v| v.as_i64())?;
        if now_epoch() >= expires_at {
            // expired: drop it lazily
            let _ = self.execute("DELETE FROM cache WHERE key = ?1", &[Bind::Text(key.to_string())]);
            return None;
        }
        let _ = self.execute(
            "UPDATE cache SET hits = hits + 1 WHERE key = ?1",
            &[Bind::Text(key.to_string())],
        );
        Some(payload)
    }

    pub fn cache_put(&self, key: &str, payload: &str, ttl_secs: i64) {
        let now = now_epoch();
        let _ = self.execute(
            "INSERT INTO cache (key, payload, created_at, expires_at, hits)
             VALUES (?1, ?2, ?3, ?4, 0)
             ON CONFLICT(key) DO UPDATE SET
               payload = excluded.payload,
               created_at = excluded.created_at,
               expires_at = excluded.expires_at,
               hits = 0",
            &[
                Bind::Text(key.to_string()),
                Bind::Text(payload.to_string()),
                Bind::Int(now),
                Bind::Int(now + ttl_secs.max(1)),
            ],
        );
    }

    pub fn cache_invalidate(&self, key: &str) -> bool {
        self.execute("DELETE FROM cache WHERE key = ?1", &[Bind::Text(key.to_string())])
            .is_ok()
    }

    pub fn cache_purge(&self) {
        let _ = self.execute("DELETE FROM cache", &[]);
    }

    pub fn cache_purge_expired(&self) {
        let _ = self.execute(
            "DELETE FROM cache WHERE expires_at <= ?1",
            &[Bind::Int(now_epoch())],
        );
    }

    pub fn cache_stats(&self) -> (u64, u64) {
        let rows = self
            .query(
                "SELECT COUNT(*), COALESCE(SUM(hits), 0) FROM cache WHERE expires_at > ?1",
                &[Bind::Int(now_epoch())],
            )
            .unwrap_or_default();
        match rows.first() {
            Some(row) => {
                let entries = row.first().and_then(|v| v.as_i64()).unwrap_or(0) as u64;
                let hits = row.get(1).and_then(|v| v.as_i64()).unwrap_or(0) as u64;
                (entries, hits)
            }
            None => (0, 0),
        }
    }

    pub fn request_count(&self) -> u64 {
        self.query("SELECT COUNT(*) FROM requests", &[])
            .ok()
            .and_then(|rows| rows.first().and_then(|row| row.first().and_then(|v| v.as_i64())))
            .unwrap_or(0) as u64
    }
}

struct Finalize(*mut sqlite3_stmt);
impl Drop for Finalize {
    fn drop(&mut self) {
        unsafe { sqlite3_finalize(self.0) };
    }
}

// sqlite3_column_count lives with the other externs
enum Bind {
    Text(String),
    Int(i64),
    Float(f64),
}

#[derive(Debug, Clone)]
pub enum Value {
    Text(String),
    Int(i64),
    Float(f64),
    Null,
}

impl Value {
    fn as_text(&self) -> Option<&String> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
    fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(v) => Some(*v),
            Value::Float(v) => Some(*v as i64),
            _ => None,
        }
    }
}

const SQLITE_INTEGER: c_int = 1;
const SQLITE_FLOAT: c_int = 2;
const SQLITE_TEXT: c_int = 3;

fn read_column(stmt: *mut sqlite3_stmt, col: usize) -> Value {
    unsafe {
        match sqlite3_column_type(stmt, col as c_int) {
            SQLITE_INTEGER => Value::Int(sqlite3_column_int64(stmt, col as c_int)),
            SQLITE_FLOAT => Value::Float(sqlite3_column_double(stmt, col as c_int)),
            SQLITE_TEXT => {
                let ptr = sqlite3_column_text(stmt, col as c_int);
                if ptr.is_null() {
                    return Value::Null;
                }
                Value::Text(
                    CStr::from_ptr(ptr as *const c_char)
                        .to_string_lossy()
                        .to_string(),
                )
            }
            _ => Value::Null,
        }
    }
}

impl Drop for Db {
    fn drop(&mut self) {
        if let Ok(conn) = self.conn.lock() {
            unsafe { sqlite3_close_v2(*conn) };
        }
    }
}
