use lsp_types::InitializeParams;
use std::error::Error;
use tokio::sync::oneshot;

use crate::cmd_args::CmdArgs;
use crate::handlers::initialized_handler;

use super::connection::AsyncConnection;
use super::lsp_server::LspServer;

pub(super) async fn main_loop(
    connection: AsyncConnection,
    params: InitializeParams,
    cmd_args: CmdArgs,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    // Setup initialization completion signal
    let (init_tx, init_rx) = oneshot::channel::<()>();

    // Create and configure server instance
    let server = LspServer::new(connection, &params, init_rx);

    // Start initialization process
    let server_context_snapshot = server.server_context.snapshot();
    tokio::spawn(async move {
        initialized_handler(server_context_snapshot, params, cmd_args).await;
        let _ = init_tx.send(());
    });

    // Run the server
    server.run().await
}
