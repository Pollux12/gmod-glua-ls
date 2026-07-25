use log::info;
use std::sync::OnceLock;
use std::time::Instant;

pub struct Profile<'a> {
    name: &'a str,
    start: Instant,
}

/// When `GLUALS_PROFILE` is set, phase-level `Profile` timers print to stderr
/// even without Info-level logging. This gives clean per-phase numbers without
/// the per-node instrumentation overhead that distorts Info-level runs.
fn phase_profile_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GLUALS_PROFILE").is_some())
}

#[allow(unused)]
impl<'a> Profile<'a> {
    pub fn new(name: &'a str) -> Self {
        Self {
            name,
            start: Instant::now(),
        }
    }

    pub fn cond_new(name: &'a str, cond: bool) -> Option<Self> {
        if (cond && log::log_enabled!(log::Level::Info)) || phase_profile_enabled() {
            Some(Self::new(name))
        } else {
            None
        }
    }
}

impl<'a> Drop for Profile<'a> {
    fn drop(&mut self) {
        let duration = self.start.elapsed();
        if log::log_enabled!(log::Level::Info) {
            info!("{}: cost {:?}", self.name, duration);
        }
        if phase_profile_enabled() {
            eprintln!("[profile] {}: cost {:?}", self.name, duration);
        }
    }
}
