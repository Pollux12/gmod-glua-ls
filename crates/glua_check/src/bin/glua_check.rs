use clap::Parser;
use glua_check::{cmd_args::CmdArgs, run_check};
use mimalloc::MiMalloc;
use std::error::Error;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Analysis recurses over deeply nested syntax. The server does that work on
/// spawned threads, which get a far larger stack than a process main thread
/// does on Windows, so the CLI has to ask for one explicitly. Without it a
/// large workspace overflows the stack before it reports anything.
fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    std::thread::Builder::new()
        .stack_size(256 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
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
