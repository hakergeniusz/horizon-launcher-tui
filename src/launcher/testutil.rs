//! Test helpers: a private temp root per test.

/// A fresh temp directory for one test, under the system temp dir (never the
/// launcher's own `tmp/`).
pub fn temp_root(name: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "horizon-launcher-tui-test-{}-{}-{n}",
        name.replace('/', "-"),
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
