use glua_doc_cli::{CmdArgs, Parser, run_doc_cli};
use mimalloc::MiMalloc;
use std::{process::ExitCode, thread};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> ExitCode {
    let cmd_args = CmdArgs::parse();
    let worker = thread::Builder::new()
        .name("glua-doc-cli".to_string())
        .stack_size(glua_code_analysis::ANALYSIS_STACK_SIZE)
        .spawn(move || match run_doc_cli(cmd_args) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("ERROR: {error}");
                ExitCode::FAILURE
            }
        });

    match worker {
        Ok(worker) => worker.join().unwrap_or_else(|_| {
            eprintln!("ERROR: documentation worker panicked");
            ExitCode::FAILURE
        }),
        Err(error) => {
            eprintln!("ERROR: failed to start documentation worker: {error}");
            ExitCode::FAILURE
        }
    }
}
