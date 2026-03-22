use mimalloc::MiMalloc;

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

/// Default paths — override with env vars
const DEFAULT_LARGE_CODEBASE: &str =
    r"A:\Misc\FearlessSRCDS\steamapps\common\GarrysModDS\garrysmod\gamemodes\cityrp";
const DEFAULT_ANNOTATIONS: &str =
    r"C:\Users\Pollux\Documents\glualangserver\emmylua-rust\glua-api-snippets\output";

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

    let large_codebase =
        std::env::var("BENCH_CODEBASE").unwrap_or_else(|_| DEFAULT_LARGE_CODEBASE.to_string());
    let annotations =
        std::env::var("BENCH_ANNOTATIONS").unwrap_or_else(|_| DEFAULT_ANNOTATIONS.to_string());

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
    analysis.init_std_lib(None);

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
    analysis.update_files_by_path(files);
    let indexing_duration = t.elapsed();
    results.push(BenchmarkResult {
        phase: "indexing (total)".into(),
        duration: indexing_duration,
    });

    // Phase 5: Diagnostics (parallel, matching real LS behavior)
    let t = Instant::now();
    let db = analysis.compilation.get_db();
    let main_file_ids = db.get_module_index().get_main_workspace_file_ids();
    let diag_file_count = main_file_ids.len();

    // Log file paths for debugging slow files
    for &fid in &[
        FileId { id: 1747 },
        FileId { id: 1857 },
        FileId { id: 2017 },
        FileId { id: 2044 },
    ] {
        if let Some(path) = db.get_vfs().get_file_path(&fid) {
            log::info!("FileId {} → {}", fid.id, path.display());
        }
    }

    // Precompute shared diagnostic data once (avoids per-file workspace-wide scans)
    let shared_data = analysis.precompute_diagnostic_shared_data();

    let parallelism = std::env::var("BENCH_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
                .min(16)
        });
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

    let mut total = std::time::Duration::ZERO;
    for result in &results {
        total += result.duration;
        let status = if result.duration.as_secs() >= 10 {
            "❌"
        } else if result.duration.as_secs() >= 2 {
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
    let target_status = if index_diag_total.as_secs() <= 10 {
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
    eprintln!("========================================");
}
