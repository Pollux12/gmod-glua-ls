#[cfg(feature = "dhat-heap")]
#[global_allocator]
static GLOBAL: dhat::Alloc = dhat::Alloc;

#[cfg(not(feature = "dhat-heap"))]
use mimalloc::MiMalloc;

#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use glua_code_analysis::{
    EmmyLuaAnalysis, Emmyrc, FileId, WorkspaceFolder, collect_workspace_files, load_configs,
    update_code_style,
};
use tokio_util::sync::CancellationToken;

fn setup_logger() {
    let log_file =
        std::env::var("BENCH_LOG").unwrap_or_else(|_| "benchmark_profile.log".to_string());
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_file)
        .expect("Failed to open log file");

    let log_level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|s| s.parse::<log::LevelFilter>().ok())
        .unwrap_or(log::LevelFilter::Warn);

    let logger = fern::Dispatch::new()
        .format(move |out, message, record| {
            out.finish(format_args!(
                "[{}] [{}] {}",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
                record.level(),
                message
            ))
        })
        .level(log_level)
        .chain(file);

    if let Err(e) = logger.apply() {
        eprintln!("Failed to apply logger: {:?}", e);
    }
    eprintln!("Profiling logs → {}", log_file);
}

struct BenchmarkResult {
    phase: String,
    duration: std::time::Duration,
}

fn discover_config_files(root: &Path) -> Vec<PathBuf> {
    let gluarc = root.join(".gluarc.json");
    if gluarc.exists() {
        return vec![gluarc];
    }
    [
        root.join(".luarc.json"),
        root.join(".emmyrc.json"),
        root.join(".emmyrc.lua"),
    ]
    .into_iter()
    .filter(|path| path.exists())
    .collect()
}

#[tokio::main]
async fn main() {
    setup_logger();

    let large_codebase = std::env::var("BENCH_CODEBASE").unwrap_or_else(|_| {
        eprintln!("ERROR: BENCH_CODEBASE env var is required");
        std::process::exit(1);
    });
    let annotations = std::env::var("BENCH_ANNOTATIONS").unwrap_or_else(|_| {
        eprintln!("ERROR: BENCH_ANNOTATIONS env var is required");
        std::process::exit(1);
    });

    let large_path = PathBuf::from(&large_codebase);
    let annotations_path = PathBuf::from(&annotations);

    if !large_path.exists() {
        eprintln!(
            "ERROR: Large codebase path does not exist: {}",
            large_codebase
        );
        std::process::exit(1);
    }
    if !annotations_path.exists() {
        eprintln!("ERROR: Annotations path does not exist: {}", annotations);
        std::process::exit(1);
    }

    eprintln!("=== GLuaLS Benchmark ===");
    eprintln!("Codebase: {}", large_codebase);
    eprintln!("Annotations: {}", annotations);
    eprintln!();

    let mut results = Vec::new();

    // Phase 1: Config loading
    let t = Instant::now();
    let config_files = discover_config_files(&large_path);
    let mut emmyrc = if config_files.is_empty() {
        Emmyrc::default()
    } else {
        load_configs(config_files, None)
    };

    // Ensure GMod is enabled and annotations are loaded
    emmyrc.gmod.enabled = true;
    emmyrc.pre_process_emmyrc(&large_path);
    results.push(BenchmarkResult {
        phase: "config loading".into(),
        duration: t.elapsed(),
    });

    // Phase 2: Create analysis + add workspaces
    let t = Instant::now();
    let mut analysis = EmmyLuaAnalysis::new();
    analysis.update_config(Arc::new(emmyrc.clone()));
    analysis.init_std_lib();

    // Add annotations as library workspace
    analysis.add_library_workspace(annotations_path.clone());

    // Add main workspace
    analysis.add_main_workspace(large_path.clone());
    results.push(BenchmarkResult {
        phase: "workspace setup".into(),
        duration: t.elapsed(),
    });

    // Phase 3: Collect files
    let t = Instant::now();
    let mut workspace_folders = vec![
        WorkspaceFolder::new(annotations_path.clone(), true),
        WorkspaceFolder::new(large_path.clone(), false),
    ];

    // Add library paths from config
    for lib in &emmyrc.workspace.library {
        let path = PathBuf::from(lib.get_path().clone());
        if path != annotations_path {
            analysis.add_library_workspace(path.clone());
            workspace_folders.push(WorkspaceFolder::new(path, true));
        }
    }

    let file_infos = collect_workspace_files(&workspace_folders, &analysis.emmyrc, None, None);
    let file_count = file_infos.len();
    let files: Vec<_> = file_infos
        .into_iter()
        .filter_map(|file| {
            if file.path.ends_with(".editorconfig") {
                let file_path = PathBuf::from(&file.path);
                let parent_dir = file_path
                    .parent()
                    .unwrap()
                    .to_path_buf()
                    .to_string_lossy()
                    .to_string()
                    .replace("\\", "/");
                let file_normalized = file_path.to_string_lossy().to_string().replace("\\", "/");
                update_code_style(&parent_dir, &file_normalized);
                None
            } else {
                Some(file.into_tuple())
            }
        })
        .collect();
    results.push(BenchmarkResult {
        phase: format!("file collection ({} files)", file_count),
        duration: t.elapsed(),
    });

    // Phase 4: Indexing (update_files_by_path runs parsing + full analysis pipeline)
    let t = Instant::now();
    #[cfg(feature = "dhat-heap")]
    let dhat_profiler = dhat::Profiler::new_heap();
    analysis.update_files_by_path(files);
    #[cfg(feature = "dhat-heap")]
    drop(dhat_profiler);
    let indexing_duration = t.elapsed();
    results.push(BenchmarkResult {
        phase: "indexing (total)".into(),
        duration: indexing_duration,
    });

    // Phase 4b: Incremental edit latency — the full cost a keystroke pays
    // once it lands: reindex of the edited file plus its whole dependency
    // expansion, then the post-edit diagnostics pass (shared-data recompute
    // + the edited file), matching the production LS flow. Worst-case
    // biased: files are ranked by reindex-expansion size and the top hubs
    // are edited.
    let mut incremental_worst: Option<std::time::Duration> = None;
    if std::env::var("BENCH_INCREMENTAL").is_ok() {
        let main_ids = analysis
            .compilation
            .get_db()
            .get_module_index()
            .get_main_workspace_file_ids();
        let path_of = |id: &FileId| {
            analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_path(id)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        };
        // Ranking every file costs ~30s on a large workspace, but the worst
        // hubs are stable properties of the codebase — cache them per codebase
        // and only re-rank when the cache is missing or stale.
        let cache_path = PathBuf::from("target/bench_incremental_targets.txt");
        let mut sample: Vec<(FileId, usize)> = Vec::new();
        if let Ok(cached) = std::fs::read_to_string(&cache_path) {
            let mut lines = cached.lines();
            if lines.next() == Some(large_codebase.as_str()) {
                let by_path: std::collections::HashMap<String, FileId> = main_ids
                    .iter()
                    .filter_map(|id| path_of(id).map(|p| (p, *id)))
                    .collect();
                let entries: Vec<Option<(FileId, usize)>> = lines
                    .map(|line| {
                        let (n, path) = line.split_once('\t')?;
                        Some((*by_path.get(path)?, n.parse::<usize>().ok()?))
                    })
                    .collect();
                if !entries.is_empty() && entries.iter().all(Option::is_some) {
                    sample = entries.into_iter().flatten().collect();
                    eprintln!(
                        "  [incremental] using {} cached edit targets from {}",
                        sample.len(),
                        cache_path.display()
                    );
                }
            }
        }
        if sample.is_empty() {
            let t_rank = Instant::now();
            let mut ranked: Vec<(FileId, usize)> = main_ids
                .iter()
                .map(|id| (*id, analysis.expand_reindex_file_ids(vec![*id]).len()))
                .collect();
            ranked.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
            eprintln!(
                "  [incremental] ranked {} files by reindex expansion in {:.3}s",
                ranked.len(),
                t_rank.elapsed().as_secs_f64()
            );

            // Top-3 hubs (worst ripple), the median file, and the largest file
            // by bytes (worst parse).
            sample = ranked.iter().take(3).copied().collect();
            for extra in [
                ranked.get(ranked.len() / 2).copied(),
                main_ids
                    .iter()
                    .max_by_key(|id| {
                        analysis
                            .compilation
                            .get_db()
                            .get_vfs()
                            .get_file_content(id)
                            .map_or(0, |text| text.len())
                    })
                    .and_then(|id| ranked.iter().find(|(f, _)| f == id).copied()),
            ]
            .into_iter()
            .flatten()
            {
                if !sample.iter().any(|(id, _)| *id == extra.0) {
                    sample.push(extra);
                }
            }

            let mut out = format!("{large_codebase}\n");
            for (id, n) in &sample {
                if let Some(path) = path_of(id) {
                    out.push_str(&format!("{n}\t{path}\n"));
                }
            }
            if let Err(err) = std::fs::write(&cache_path, out) {
                eprintln!("  [incremental] failed to write target cache: {err}");
            }
        }

        let mut total = std::time::Duration::ZERO;
        let mut worst = std::time::Duration::ZERO;
        let mut edited = 0usize;
        for (file_id, expansion) in sample {
            let Some(uri) = analysis.compilation.get_db().get_vfs().get_uri(&file_id) else {
                continue;
            };
            let Some(text) = analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_content(&file_id)
                .cloned()
            else {
                continue;
            };
            let name = analysis
                .compilation
                .get_db()
                .get_vfs()
                .get_file_path(&file_id)
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_else(|| format!("{file_id:?}"));
            // Append a comment: defeats the token-identity no-op gate, so the
            // full production edit path (expansion + remove + rebuild) runs.
            let edited_text = format!("{text}\n-- bench incremental edit\n");
            let t = Instant::now();
            analysis.update_file_by_uri(&uri, Some(edited_text));
            let reindex = t.elapsed();
            let t = Instant::now();
            let shared = analysis.precompute_diagnostic_shared_data();
            analysis.diagnose_file_with_shared(file_id, CancellationToken::new(), shared);
            let diagnostics = t.elapsed();
            let elapsed = reindex + diagnostics;
            total += elapsed;
            worst = worst.max(elapsed);
            edited += 1;
            eprintln!(
                "  [incremental] {name} (reindexes {expansion} files): {:.3}s ({:.3}s reindex + {:.3}s diagnostics)",
                elapsed.as_secs_f64(),
                reindex.as_secs_f64(),
                diagnostics.as_secs_f64()
            );
            analysis.update_file_by_uri(&uri, Some(text));
        }
        if edited > 0 {
            eprintln!(
                "  [incremental] {} edits, avg {:.3}s, worst {:.3}s",
                edited,
                total.as_secs_f64() / edited as f64,
                worst.as_secs_f64()
            );
            incremental_worst = Some(worst);
        }
    }

    // Phase 5: Diagnostics (parallel, matching real LS behavior)
    let t = Instant::now();
    let main_file_ids = analysis.get_main_workspace_file_ids_for_diagnostics();
    let diag_file_count = main_file_ids.len();

    // Precompute shared diagnostic data once (avoids per-file workspace-wide scans)
    let precompute_t = Instant::now();
    let shared_data = analysis.precompute_diagnostic_shared_data();
    eprintln!(
        "  [diag] precompute_shared_data: {:.3}s",
        precompute_t.elapsed().as_secs_f64()
    );

    let parallelism = match std::env::var("BENCH_THREADS") {
        Ok(val) => match val.parse::<usize>() {
            Ok(n) if n > 0 => n,
            Ok(_) => {
                eprintln!(
                    "ERROR: BENCH_THREADS must be a positive integer, got: {}",
                    val
                );
                std::process::exit(1);
            }
            Err(_) => {
                eprintln!("ERROR: BENCH_THREADS is not a valid integer: {}", val);
                std::process::exit(1);
            }
        },
        Err(_) => std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(16),
    };
    eprintln!(
        "Diagnostics: {} files, {} threads",
        diag_file_count, parallelism
    );

    let total_diagnostics = std::sync::atomic::AtomicUsize::new(0);
    let error_count = std::sync::atomic::AtomicUsize::new(0);
    let warning_count = std::sync::atomic::AtomicUsize::new(0);
    let info_count = std::sync::atomic::AtomicUsize::new(0);
    let hint_count = std::sync::atomic::AtomicUsize::new(0);
    let next_file = std::sync::atomic::AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..parallelism {
            let analysis = &analysis;
            let counter = &total_diagnostics;
            let errors = &error_count;
            let warnings = &warning_count;
            let infos = &info_count;
            let hints = &hint_count;
            let next = &next_file;
            let file_ids = &main_file_ids;
            let shared = shared_data.clone();
            s.spawn(move || {
                loop {
                    let idx = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if idx >= file_ids.len() {
                        break;
                    }
                    let cancel_token = CancellationToken::new();
                    if let Some(diagnostics) = analysis.diagnose_file_with_shared(
                        file_ids[idx],
                        cancel_token,
                        shared.clone(),
                    ) {
                        counter.fetch_add(diagnostics.len(), std::sync::atomic::Ordering::Relaxed);
                        for d in &diagnostics {
                            match d.severity {
                                Some(lsp_types::DiagnosticSeverity::ERROR) => {
                                    errors.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                Some(lsp_types::DiagnosticSeverity::WARNING) => {
                                    warnings.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                Some(lsp_types::DiagnosticSeverity::INFORMATION) => {
                                    infos.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                Some(lsp_types::DiagnosticSeverity::HINT) => {
                                    hints.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                                }
                                _ => {}
                            }
                        }
                    }
                }
            });
        }
    });
    let total_diagnostics = total_diagnostics.load(std::sync::atomic::Ordering::Relaxed);
    let errors = error_count.load(std::sync::atomic::Ordering::Relaxed);
    let warnings = warning_count.load(std::sync::atomic::Ordering::Relaxed);
    let infos = info_count.load(std::sync::atomic::Ordering::Relaxed);
    let hints = hint_count.load(std::sync::atomic::Ordering::Relaxed);
    let diagnostics_duration = t.elapsed();
    results.push(BenchmarkResult {
        phase: format!(
            "diagnostics ({} files, {} issues)",
            diag_file_count, total_diagnostics
        ),
        duration: diagnostics_duration,
    });

    eprintln!();
    eprintln!(
        "Diagnostic breakdown: {} errors, {} warnings, {} info, {} hints",
        errors, warnings, infos, hints
    );

    // Output results
    eprintln!();
    eprintln!("========================================");
    eprintln!("BENCHMARK RESULTS");
    eprintln!("========================================");
    eprintln!("{:<45} {:>12}", "Phase", "Duration");
    eprintln!("{}", "-".repeat(60));

    let warning_threshold = std::time::Duration::from_secs(2);
    let error_threshold = std::time::Duration::from_secs(10);
    let mut total = std::time::Duration::ZERO;
    for result in &results {
        total += result.duration;
        let status = if result.duration >= error_threshold {
            "❌"
        } else if result.duration >= warning_threshold {
            "⚠️"
        } else {
            "✅"
        };
        eprintln!(
            "{:<45} {:>10.3}s  {}",
            result.phase,
            result.duration.as_secs_f64(),
            status
        );
    }
    eprintln!("{}", "-".repeat(60));

    let index_diag_total = indexing_duration + diagnostics_duration;
    let target_status = if index_diag_total <= error_threshold {
        "✅ TARGET MET"
    } else {
        "❌ TARGET NOT MET"
    };
    eprintln!(
        "{:<45} {:>10.3}s",
        "TOTAL (all phases)",
        total.as_secs_f64(),
    );
    eprintln!(
        "{:<45} {:>10.3}s  {}",
        "INDEX + DIAGNOSTICS",
        index_diag_total.as_secs_f64(),
        target_status
    );
    eprintln!("Target: ≤10s");

    // A single-file edit is the interactive hot path: the user is typing, and
    // every keystroke that lands pays reindex + diagnostics. Budget it
    // separately from the cold index — a workspace that indexes in 10s is
    // useless if each edit costs a second.
    if let Some(worst) = incremental_worst {
        let incremental_target = std::time::Duration::from_secs(1);
        let status = if worst <= incremental_target {
            "✅ TARGET MET"
        } else {
            "❌ TARGET NOT MET"
        };
        eprintln!(
            "{:<45} {:>10.3}s  {}",
            "INCREMENTAL EDIT (worst)",
            worst.as_secs_f64(),
            status
        );
        eprintln!("Target: ≤1s per edit (reindex ripple + edited-file diagnostics)");
    }
    eprintln!("========================================");
}
