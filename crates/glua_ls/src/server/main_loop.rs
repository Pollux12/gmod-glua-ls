use lsp_types::InitializeParams;
use std::error::Error;
use tokio::sync::oneshot;

use crate::cmd_args::CmdArgs;
use crate::handlers::initialized_handler;

use super::connection::AsyncConnection;
use super::lsp_server::LspServer;

/// Carries the outcome of the spawned initialization task back to the server
/// loop. `Ok(())` means the language server may proceed with normal message
/// processing; `Err(reason)` means startup failed (currently: GMod mode is
/// enabled but required annotations could not be resolved) and the server
/// must abort after surfacing the reason to the client.
pub(super) type InitResult = Result<(), String>;

pub(super) async fn main_loop(
    connection: AsyncConnection,
    params: InitializeParams,
    cmd_args: CmdArgs,
) -> Result<(), Box<dyn Error + Sync + Send>> {
    // Setup initialization completion signal. Carries the init task outcome.
    let (init_tx, init_rx) = oneshot::channel::<InitResult>();

    // Create and configure server instance
    let server = LspServer::new(connection, &params, init_rx);

    // Start initialization process
    let server_context_snapshot = server.server_context.snapshot();
    tokio::spawn(async move {
        let result = initialized_handler(server_context_snapshot, params, cmd_args).await;
        // On `Err`, the handler already sent a `window/showMessage` to the
        // client with the user-facing reason; here we just propagate the
        // outcome so the server loop can abort cleanly.
        let _ = init_tx.send(result);
    });

    // Run the server
    server.run().await
}
