use glua_doc_cli::{CmdArgs, Parser, run_doc_cli};
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cmd_args = CmdArgs::parse();
    run_doc_cli(cmd_args)
}
