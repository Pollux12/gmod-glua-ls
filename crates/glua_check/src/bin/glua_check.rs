use clap::Parser;
use glua_check::{cmd_args::CmdArgs, run_check};
use mimalloc::MiMalloc;
use std::error::Error;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Analysis recurses over deeply nested syntax and runs on every runtime
/// thread, so each one, like the thread driving the runtime, gets
/// [`glua_code_analysis::ANALYSIS_STACK_SIZE`].
fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    std::thread::Builder::new()
        .stack_size(glua_code_analysis::ANALYSIS_STACK_SIZE)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_stack_size(glua_code_analysis::ANALYSIS_STACK_SIZE)
                .build()
                .expect("tokio runtime should build")
                .block_on(run())
        })
        .expect("glua_check worker thread should spawn")
        .join()
        .expect("glua_check worker thread should not panic")
}

async fn run() -> Result<(), Box<dyn Error + Sync + Send>> {
    let cmd_args = CmdArgs::parse();
    run_check(cmd_args).await
}
