//! The process heap of the GLuaLS binaries.
//!
//! The heap is mimalloc. On Linux it lives in an unlinked file mapped shared,
//! so its pages are page cache: the kernel writes them back and drops them
//! when memory runs short, and the memory the process cannot give back stays
//! the same however large the workspace is.
//!
//! `GLUALS_HEAP_DIR` names the directory the file is made in; `off` keeps the
//! heap in anonymous memory. Unset, the file goes to whichever of
//! `$XDG_CACHE_HOME`, `~/.cache`, `/var/tmp` and `/tmp` is on disk and has
//! the most room.

pub use mimalloc::MiMalloc;

#[cfg(target_os = "linux")]
mod file;

/// Where the heap lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backing {
    /// Anonymous memory.
    Memory,
    /// A file on disk.
    File,
}

/// Where the heap lives.
pub fn backing() -> Backing {
    #[cfg(target_os = "linux")]
    if file::installed() {
        return Backing::File;
    }
    Backing::Memory
}

unsafe extern "C" {
    fn mi_collect(force: bool);
}

/// Returns the calling thread's free heap pages to the process.
pub fn collect() {
    // SAFETY: `mi_collect` may be called from any thread at any time.
    unsafe { mi_collect(true) }
}
