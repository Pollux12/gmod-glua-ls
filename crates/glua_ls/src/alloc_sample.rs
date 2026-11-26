//! Sampled heap profiler for `glua_ls --features alloc-sample`.
//!
//! Wraps `MiMalloc` with live/peak byte counters and 1-in-128 KiB stack
//! sampling. When `GLUALS_SAMPLE_TRIGGER_FILE` is set, a lightweight watcher
//! thread checks for that file every 100 ms, reads its contents as `<label>`,
//! removes it, and writes `<GLUALS_SAMPLE_OUT>.<label>.folded` plus live/peak
//! stats to stderr.

use std::alloc::{GlobalAlloc, Layout};
use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

use mimalloc::MiMalloc;

const INTERVAL: usize = 128 << 10;
const FRAMES: usize = 64;
const FILTER_BITS: usize = 1 << 26;

pub static ALLOCS: AtomicU64 = AtomicU64::new(0);
pub static BYTES: AtomicU64 = AtomicU64::new(0);
pub static LIVE: AtomicUsize = AtomicUsize::new(0);
pub static PEAK: AtomicUsize = AtomicUsize::new(0);
pub static PEAK_SINCE: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    static UNTIL_SAMPLE: Cell<isize> = const { Cell::new(INTERVAL as isize) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

struct Sample {
    bytes: usize,
    frames: [usize; FRAMES],
}

static SAMPLES: Mutex<Option<HashMap<usize, Sample>>> = Mutex::new(None);
static FILTER: [AtomicU64; FILTER_BITS / 64] = [const { AtomicU64::new(0) }; FILTER_BITS / 64];

fn filter_bit(ptr: usize) -> (usize, u64) {
    let hash = (ptr >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - 26);
    (hash / 64, 1 << (hash % 64))
}

pub struct SamplingAlloc(pub MiMalloc);

unsafe impl GlobalAlloc for SamplingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(layout.size() as u64, Relaxed);
        let live = LIVE.fetch_add(layout.size(), Relaxed) + layout.size();
        PEAK.fetch_max(live, Relaxed);
        PEAK_SINCE.fetch_max(live, Relaxed);
        let ptr = unsafe { self.0.alloc(layout) };
        allocated(ptr, layout.size());
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE.fetch_sub(layout.size(), Relaxed);
        freed(ptr);
        unsafe { self.0.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Relaxed);
        BYTES.fetch_add(new_size as u64, Relaxed);
        LIVE.fetch_sub(layout.size(), Relaxed);
        let live = LIVE.fetch_add(new_size, Relaxed) + new_size;
        PEAK.fetch_max(live, Relaxed);
        PEAK_SINCE.fetch_max(live, Relaxed);
        freed(ptr);
        let ptr = unsafe { self.0.realloc(ptr, layout, new_size) };
        allocated(ptr, new_size);
        ptr
    }
}

fn allocated(ptr: *mut u8, size: usize) {
    let due = UNTIL_SAMPLE.with(|until| {
        let left = until.get() - size as isize;
        if left > 0 {
            until.set(left);
            false
        } else {
            until.set(INTERVAL as isize);
            true
        }
    });
    if !due || ptr.is_null() || BUSY.with(|busy| busy.replace(true)) {
        return;
    }
    let mut frames = [0usize; FRAMES];
    let mut depth = 0;
    backtrace::trace(|frame| {
        frames[depth] = frame.ip() as usize;
        depth += 1;
        depth < FRAMES
    });
    let (word, bit) = filter_bit(ptr as usize);
    FILTER[word].fetch_or(bit, Relaxed);
    SAMPLES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get_or_insert_with(HashMap::new)
        .insert(
            ptr as usize,
            Sample {
                bytes: size.max(INTERVAL),
                frames,
            },
        );
    BUSY.with(|busy| busy.set(false));
}

fn freed(ptr: *mut u8) {
    let (word, bit) = filter_bit(ptr as usize);
    if FILTER[word].load(Relaxed) & bit == 0 || BUSY.with(|busy| busy.replace(true)) {
        return;
    }
    if let Some(samples) = SAMPLES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_mut()
    {
        samples.remove(&(ptr as usize));
    }
    BUSY.with(|busy| busy.set(false));
}

pub fn init() {
    glua_code_analysis::profile::memory::set_alloc_counters(
        glua_code_analysis::profile::memory::AllocCounters {
            live: || LIVE.load(Relaxed),
            take_peak: || PEAK_SINCE.swap(LIVE.load(Relaxed), Relaxed),
            allocated: || (ALLOCS.load(Relaxed), BYTES.load(Relaxed)),
        },
    );
    let Ok(trigger_path) = std::env::var("GLUALS_SAMPLE_TRIGGER_FILE") else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("alloc-sample-watcher".into())
        .spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(100));
                let Ok(raw) = std::fs::read_to_string(&trigger_path) else {
                    continue;
                };
                let label = raw.trim().to_string();
                let _ = std::fs::remove_file(&trigger_path);
                if !label.is_empty() {
                    dump(&label);
                }
            }
        });
}

pub fn dump(label: &str) {
    let was_busy = BUSY.with(|busy| busy.replace(true));
    let live_mb = LIVE.load(Relaxed) as f64 / 1_048_576.0;
    let peak_mb = PEAK.load(Relaxed) as f64 / 1_048_576.0;
    let mut by_stack = HashMap::<[usize; FRAMES], usize>::new();
    if let Some(samples) = SAMPLES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        for sample in samples.values() {
            *by_stack.entry(sample.frames).or_default() += sample.bytes;
        }
    }
    let mut names = HashMap::<usize, String>::new();
    for &ip in by_stack.keys().flatten().filter(|ip| **ip != 0) {
        names.entry(ip).or_insert_with(|| {
            let mut found = Vec::new();
            backtrace::resolve(ip as *mut std::ffi::c_void, |symbol| {
                if let Some(name) = symbol.name() {
                    found.push(format!("{name:#}"));
                }
            });
            found.reverse();
            if found.is_empty() {
                String::from("?")
            } else {
                found.join(";")
            }
        });
    }
    let mut out = String::new();
    let total: usize = by_stack.values().sum();
    for (frames, bytes) in &by_stack {
        let line = frames
            .iter()
            .rev()
            .filter_map(|ip| names.get(ip))
            .flat_map(|names| names.split(';'))
            .filter(|name| !name.starts_with("backtrace::") && !name.contains("alloc_sample"))
            .collect::<Vec<_>>();
        out.push_str(&line.join(";"));
        out.push_str(&format!(" {bytes}\n"));
    }
    let prefix = std::env::var("GLUALS_SAMPLE_OUT").unwrap_or_else(|_| "glua_ls_heap".to_string());
    let path = format!("{prefix}.{label}.folded");
    let _ = std::fs::write(&path, out);
    eprintln!(
        "[ls-sample] {label}: exact_live={live_mb:.1} MiB peak_live={peak_mb:.1} MiB sampled={} MiB ({} stacks) -> {path}",
        total >> 20,
        by_stack.len()
    );
    BUSY.with(|busy| busy.set(was_busy));
}
