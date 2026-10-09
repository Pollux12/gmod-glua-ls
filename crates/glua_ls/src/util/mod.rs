mod analysis_progress;
mod desc;
mod long_running_watchdog;
pub mod ls_profile;
mod module_name_convert;
mod time_cancel_token;

pub use analysis_progress::AnalysisProgressReporter;
pub use desc::*;
pub use long_running_watchdog::*;
pub use module_name_convert::{
    file_name_convert, module_name_convert, to_camel_case, to_pascal_case, to_snake_case,
};
pub use time_cancel_token::time_cancel_token;

#[cfg(test)]
pub(crate) fn unique_temp_dir(prefix: &str) -> std::path::PathBuf {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    std::env::temp_dir().join(format!("{prefix}-{}-{id}", std::process::id()))
}
