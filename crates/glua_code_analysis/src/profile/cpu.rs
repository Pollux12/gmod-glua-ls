//! The calling thread's CPU time, for per-file profiles that must hold on a
//! busy machine, where wall time includes time other processes ran.

use std::time::Duration;

#[cfg(windows)]
mod platform {
    use std::time::Duration;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    impl FileTime {
        fn duration(&self) -> Duration {
            Duration::from_nanos(((u64::from(self.high) << 32) | u64::from(self.low)) * 100)
        }
    }

    unsafe extern "system" {
        fn GetCurrentThread() -> isize;
        fn GetThreadTimes(
            thread: isize,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
    }

    pub(super) fn thread_cpu_time() -> Option<Duration> {
        let mut creation = FileTime::default();
        let mut exit = FileTime::default();
        let mut kernel = FileTime::default();
        let mut user = FileTime::default();
        // SAFETY: every pointer is a writable FILETIME, and the pseudo-handle
        // needs no closing.
        let ok = unsafe {
            GetThreadTimes(
                GetCurrentThread(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        (ok != 0).then(|| kernel.duration() + user.duration())
    }
}

#[cfg(not(windows))]
mod platform {
    pub(super) fn thread_cpu_time() -> Option<std::time::Duration> {
        None
    }
}

/// The CPU time the calling thread has used, where the platform reports it.
pub fn thread_cpu_time() -> Option<Duration> {
    platform::thread_cpu_time()
}
