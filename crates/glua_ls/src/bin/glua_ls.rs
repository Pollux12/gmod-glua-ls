use clap::Parser;
use glua_code_analysis::ANALYSIS_STACK_SIZE;
use glua_ls::cmd_args::CmdArgs;
use mimalloc::MiMalloc;
use std::error::Error;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Analysis runs on every runtime thread, so each one, like the thread
/// driving the runtime, gets [`ANALYSIS_STACK_SIZE`].
fn main() {
    std::thread::Builder::new()
        .stack_size(ANALYSIS_STACK_SIZE)
        .spawn(serve)
        .expect("glua_ls thread should spawn")
        .join()
        .expect("glua_ls thread should not panic");
}

/// Ends the process when the session does, as `exit` asks, whatever work is
/// still running: 0 after a clean `shutdown` and `exit`, 1 otherwise.
fn serve() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(ANALYSIS_STACK_SIZE)
        .build()
        .expect("tokio runtime should build");
    let result: Result<(), Box<dyn Error + Sync + Send>> =
        runtime.block_on(glua_ls::run_ls(CmdArgs::parse()));
    log::logger().flush();
    glua_ls::drain_stderr_log(std::time::Duration::from_secs(1));
    let code = match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("Error: {error}");
            1
        }
    };
    std::process::exit(code);
}
