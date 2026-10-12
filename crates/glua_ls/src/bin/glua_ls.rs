use clap::Parser;
use glua_ls::cmd_args::CmdArgs;
use mimalloc::MiMalloc;
use std::error::Error;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// Ends the process when the session does, as `exit` asks, whatever work is
/// still running: 0 after a clean `shutdown` and `exit`, 1 otherwise.
#[tokio::main]
async fn main() {
    let cmd_args = CmdArgs::parse();
    let result: Result<(), Box<dyn Error + Sync + Send>> = glua_ls::run_ls(cmd_args).await;
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
