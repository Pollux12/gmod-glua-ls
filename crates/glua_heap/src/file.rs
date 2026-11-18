//! The heap file: made, mapped and handed to mimalloc before `main`, so the
//! first allocation already lands in it.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

/// The address space the file is mapped over. Past it mimalloc takes
/// anonymous memory again.
const RESERVED: usize = 1 << 38;
/// The largest span mimalloc manages as one arena.
const ARENA: usize = 1 << 34;
const ALIGNMENT: usize = 1 << 26;
const PATH_MAX: usize = 4096;

static INSTALLED: AtomicBool = AtomicBool::new(false);
static BASE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn installed() -> bool {
    INSTALLED.load(Relaxed)
}

type CommitFn = unsafe extern "C" fn(
    commit: bool,
    start: *mut c_void,
    size: usize,
    is_zero: *mut bool,
    arg: *mut c_void,
) -> bool;

unsafe extern "C" {
    fn mi_manage_memory(
        start: *mut c_void,
        size: usize,
        is_committed: bool,
        is_pinned: bool,
        is_zero: bool,
        numa_node: c_int,
        exclusive: bool,
        commit_fun: CommitFn,
        commit_fun_arg: *mut c_void,
        arena_id: *mut *mut c_void,
    ) -> bool;
}

#[used]
#[unsafe(link_section = ".init_array")]
static INSTALL: extern "C" fn() = install;

/// Runs before the Rust runtime starts, so it must not allocate.
extern "C" fn install() {
    // SAFETY: only libc calls on buffers owned here, before any other thread
    // exists.
    unsafe {
        let Some(fd) = open_heap_file() else {
            return;
        };
        if libc::ftruncate(fd, RESERVED as libc::off_t) != 0 {
            libc::close(fd);
            return;
        }
        let Some(base) = map(fd) else {
            libc::close(fd);
            return;
        };
        BASE.store(base as usize, Relaxed);
        let mut managed = 0;
        while managed < RESERVED
            && mi_manage_memory(
                base.byte_add(managed),
                ARENA,
                false,
                false,
                true,
                -1,
                false,
                commit,
                fd as usize as *mut c_void,
                std::ptr::null_mut(),
            )
        {
            managed += ARENA;
        }
        if managed < RESERVED {
            libc::munmap(base.byte_add(managed), RESERVED - managed);
        }
        if managed == 0 {
            libc::close(fd);
        } else {
            INSTALLED.store(true, Relaxed);
        }
    }
}

/// Maps `fd` shared at an address aligned for mimalloc's arenas.
unsafe fn map(fd: c_int) -> Option<*mut c_void> {
    unsafe {
        let span = RESERVED + ALIGNMENT;
        let hole = libc::mmap(
            std::ptr::null_mut(),
            span,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
            -1,
            0,
        );
        if hole == libc::MAP_FAILED {
            return None;
        }
        let start = (hole as usize).next_multiple_of(ALIGNMENT);
        let base = libc::mmap(
            start as *mut c_void,
            RESERVED,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_FIXED | libc::MAP_NORESERVE,
            fd,
            0,
        );
        if base == libc::MAP_FAILED {
            libc::munmap(hole, span);
            return None;
        }
        let head = start - hole as usize;
        if head > 0 {
            libc::munmap(hole, head);
        }
        let tail = ALIGNMENT - head;
        if tail > 0 {
            libc::munmap((start + RESERVED) as *mut c_void, tail);
        }
        Some(base)
    }
}

/// Reserves the file's blocks under a range mimalloc is about to use, or
/// gives them back with the pages over them. A range that cannot be reserved
/// is refused, and mimalloc allocates elsewhere, so a full disk is never
/// found by writing to the mapping.
unsafe extern "C" fn commit(
    commit: bool,
    start: *mut c_void,
    size: usize,
    is_zero: *mut bool,
    arg: *mut c_void,
) -> bool {
    let fd = arg as usize as c_int;
    let offset = (start as usize - BASE.load(Relaxed)) as libc::off_t;
    let mode = if commit {
        0
    } else {
        libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE
    };
    loop {
        // SAFETY: `fd` is the heap file, open for the life of the process.
        if unsafe { libc::fallocate(fd, mode, offset, size as libc::off_t) } == 0 {
            break;
        }
        if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            return false;
        }
    }
    if commit && !is_zero.is_null() {
        // SAFETY: mimalloc passes a valid flag or null.
        unsafe { *is_zero = false };
    }
    true
}

struct Path {
    bytes: [u8; PATH_MAX],
    len: usize,
}

impl Path {
    /// `parts` joined, or `None` when they do not fit.
    unsafe fn join(parts: &[*const c_char]) -> Option<Self> {
        let mut path = Self {
            bytes: [0; PATH_MAX],
            len: 0,
        };
        for part in parts {
            // SAFETY: each part is a NUL-terminated string.
            let part = unsafe { CStr::from_ptr(*part) }.to_bytes();
            if path.len + part.len() >= PATH_MAX {
                return None;
            }
            path.bytes[path.len..path.len + part.len()].copy_from_slice(part);
            path.len += part.len();
        }
        Some(path)
    }

    fn as_ptr(&self) -> *const c_char {
        self.bytes.as_ptr().cast()
    }
}

/// The bytes free in `dir` for this user, or `None` when it is missing or
/// kept in memory.
unsafe fn disk_room(dir: &Path) -> Option<u64> {
    const TMPFS_MAGIC: i64 = 0x0102_1994;
    const RAMFS_MAGIC: i64 = 0x8584_58f6_u32 as i64;
    // SAFETY: `dir` is NUL-terminated and `stat` is writable.
    unsafe {
        let mut stat: libc::statfs = std::mem::zeroed();
        if libc::statfs(dir.as_ptr(), &mut stat) != 0 {
            return None;
        }
        let kind = stat.f_type as i64 & 0xffff_ffff;
        if kind == TMPFS_MAGIC || kind == RAMFS_MAGIC {
            return None;
        }
        Some(stat.f_bavail as u64 * stat.f_bsize as u64)
    }
}

/// An unlinked file in `dir`.
unsafe fn open_in(dir: &Path) -> Option<c_int> {
    // SAFETY: both paths are NUL-terminated buffers owned here.
    unsafe {
        let fd = libc::open(
            dir.as_ptr(),
            libc::O_TMPFILE | libc::O_RDWR | libc::O_CLOEXEC,
            0o600,
        );
        if fd >= 0 {
            return Some(fd);
        }
        let mut template = Path::join(&[dir.as_ptr(), c"/gluals-heap-XXXXXX".as_ptr()])?;
        let fd = libc::mkostemp(template.bytes.as_mut_ptr().cast(), libc::O_CLOEXEC);
        if fd < 0 {
            return None;
        }
        libc::unlink(template.as_ptr());
        Some(fd)
    }
}

unsafe fn open_heap_file() -> Option<c_int> {
    // SAFETY: `getenv` results are read before anything can change the
    // environment.
    unsafe {
        let chosen = libc::getenv(c"GLUALS_HEAP_DIR".as_ptr());
        if !chosen.is_null() {
            let name = CStr::from_ptr(chosen).to_bytes();
            if name.is_empty() || name == b"off" {
                return None;
            }
            return open_in(&Path::join(&[chosen])?);
        }
        let cache = libc::getenv(c"XDG_CACHE_HOME".as_ptr());
        let home = libc::getenv(c"HOME".as_ptr());
        let candidates = [
            (!cache.is_null()).then(|| Path::join(&[cache])).flatten(),
            (!home.is_null())
                .then(|| Path::join(&[home, c"/.cache".as_ptr()]))
                .flatten(),
            Path::join(&[c"/var/tmp".as_ptr()]),
            Path::join(&[c"/tmp".as_ptr()]),
        ];
        let mut roomiest: Option<(u64, c_int)> = None;
        for dir in candidates.iter().flatten() {
            let Some(room) = disk_room(dir) else {
                continue;
            };
            if roomiest.is_some_and(|(most, _)| most >= room) {
                continue;
            }
            if let Some(fd) = open_in(dir) {
                if let Some((_, previous)) = roomiest.replace((room, fd)) {
                    libc::close(previous);
                }
            }
        }
        roomiest.map(|(_, fd)| fd)
    }
}
