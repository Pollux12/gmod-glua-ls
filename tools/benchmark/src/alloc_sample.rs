//! Sampled heap profile for `--features alloc-sample`: one allocation per
//! [`INTERVAL`] bytes is kept with its call stack while it lives, and
//! `BENCH_SAMPLE_AT_MB` writes the stacks of what is live once the live heap
//! first passes that size, and again after indexing, as folded stacks
//! (`frame;frame;... bytes`) to `BENCH_SAMPLE_OUT` (default `heap`).

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

/// The bytes allocated between two samples on a thread.
const INTERVAL: usize = 128 << 10;
const FRAMES: usize = 64;
const FILTER_BITS: usize = 1 << 26;

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
static THRESHOLD: AtomicUsize = AtomicUsize::new(usize::MAX);
static DUMPING: AtomicBool = AtomicBool::new(false);

fn filter_bit(ptr: usize) -> (usize, u64) {
    let hash = (ptr >> 3).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - 26);
    (hash / 64, 1 << (hash % 64))
}

pub fn init() {
    if let Some(megabytes) = std::env::var("BENCH_SAMPLE_AT_MB")
        .ok()
        .and_then(|raw| raw.parse::<usize>().ok())
    {
        THRESHOLD.store(megabytes << 20, Relaxed);
    }
}

/// Notes an allocation of `size` bytes at `ptr`, with `live` bytes now live.
pub fn allocated(ptr: *mut u8, size: usize, live: usize) {
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
    if live > THRESHOLD.load(Relaxed) && !DUMPING.swap(true, Relaxed) {
        THRESHOLD.store(usize::MAX, Relaxed);
        dump("peak");
        DUMPING.store(false, Relaxed);
    }
    BUSY.with(|busy| busy.set(false));
}

/// Notes that the allocation at `ptr` is gone.
pub fn freed(ptr: *mut u8) {
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

/// Writes the stacks of the live samples to `<BENCH_SAMPLE_OUT>.<label>.folded`.
pub fn dump(label: &str) {
    let was_busy = BUSY.with(|busy| busy.replace(true));
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
            // Outermost first, so inlined callees follow their callers.
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
    let path = format!(
        "{}.{label}.folded",
        std::env::var("BENCH_SAMPLE_OUT").unwrap_or_else(|_| "heap".to_string())
    );
    let _ = std::fs::write(&path, out);
    eprintln!(
        "[sample] {label}: {} MB live in {} stacks -> {path}",
        total >> 20,
        by_stack.len()
    );
    BUSY.with(|busy| busy.set(was_busy));
}
