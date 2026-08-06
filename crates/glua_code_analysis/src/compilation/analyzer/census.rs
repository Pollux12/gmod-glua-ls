//! Diagnostic instrumentation for determinism investigations.
//!
//! Sizes the "degraded success" surface: places where analysis accepts a worse
//! fact rather than failing, so the result depends on how far the batch had been
//! walked. Enabled only when `GLUALS_DEGRADE_CENSUS` is set; when unset the only
//! cost is a relaxed load of a `OnceLock<bool>`.
//!
//! Aggregate with: `grep '^\[census\]' log | sort | uniq -c | sort -rn`

use std::sync::OnceLock;

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("GLUALS_DEGRADE_CENSUS").is_some())
}

pub(crate) fn record(site: &str, reason: &str) {
    if enabled() {
        eprintln!("[census] site={site} reason={reason}");
    }
}
