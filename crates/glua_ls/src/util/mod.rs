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

/// A folder [`unique_temp_dir`] names, created with the guard and removed when
/// it drops.
#[cfg(test)]
pub(crate) struct TempDir(std::path::PathBuf);

#[cfg(test)]
impl TempDir {
    pub(crate) fn new(prefix: &str) -> Self {
        let dir = Self(unique_temp_dir(prefix));
        std::fs::create_dir_all(&dir.0).expect("test folder should be created");
        dir
    }
}

#[cfg(test)]
impl std::ops::Deref for TempDir {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
