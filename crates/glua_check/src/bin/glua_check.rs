use clap::Parser;
use glua_check::{cmd_args::CmdArgs, run_check};
use mimalloc::MiMalloc;
use std::error::Error;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Sync + Send>> {
    let cmd_args = CmdArgs::parse();
    run_check(cmd_args).await
}
