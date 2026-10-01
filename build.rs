// compiles the bundled sqlite amalgamation only when the `db` feature is on,
// so the default build stays a tiny binary with zero C compilation.
// uses the system compiler directly (cc is present on any linux vps with
// build-essential) -- no external crates needed.
use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let db_enabled = env::var("CARGO_FEATURE_DB").is_ok();
    if !db_enabled {
        return;
    }

    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let amalg = manifest.join("sqlite").join("sqlite3.c");
    if !amalg.exists() {
        panic!(
            "db feature enabled but {} is missing; see sqlite/README.md",
            amalg.display()
        );
    }

    println!("cargo:rerun-if-changed=sqlite/sqlite3.c");
    println!("cargo:rerun-if-changed=sqlite/sqlite3.h");
    println!("cargo:rerun-if-env-changed=CC");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let obj = out_dir.join("sqlite3.o");
    let lib = out_dir.join("libsqlite3.a");

    let cc = env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let status = Command::new(&cc)
        .args([
            "-c",
            "-O2",
            "-DSQLITE_THREADSAFE=1",
            // trim the fat: single binary, no extras needed
            "-DSQLITE_OMIT_LOAD_EXTENSION",
            "-DSQLITE_OMIT_DEPRECATED",
            "-DSQLITE_OMIT_PROGRESS_CALLBACK",
            "-DSQLITE_OMIT_SHARED_CACHE",
            "-DSQLITE_DEFAULT_MEMSTATUS=0",
            "-DSQLITE_MAX_EXPR_DEPTH=0",
            "-DSQLITE_LIKE_DOESNT_MATCH_BLOBS",
            "-DSQLITE_DQS=0",
            "-fPIC",
            "-o",
        ])
        .arg(&obj)
        .arg(&amalg)
        .status()
        .unwrap_or_else(|err| panic!("failed to run {} (install build-essential): {}", cc, err));

    if !status.success() {
        panic!("{} failed to compile sqlite3.c", cc);
    }

    let ar = env::var("AR").unwrap_or_else(|_| "ar".to_string());
    let status = Command::new(&ar)
        .args(["rcs"])
        .arg(&lib)
        .arg(&obj)
        .status()
        .unwrap_or_else(|err| panic!("failed to run {}: {}", ar, err));
    if !status.success() {
        panic!("{} failed to archive libsqlite3.a", ar);
    }

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=sqlite3");
}
