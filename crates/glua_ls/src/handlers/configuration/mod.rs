use std::time::Duration;

use lsp_types::{
    ClientCapabilities, DidChangeConfigurationParams, Registration, RegistrationParams,
    ServerCapabilities,
};

use crate::{
    context::{ClientId, ClientProxy, LspFeatures, ServerContextSnapshot},
    handlers::initialized::{client_config_sections, get_client_config},
};

use super::RegisterCapabilities;

/// Pulls the client's settings again, since the notification's payload is
/// client-specific, and applies whatever the effective config changed.
pub async fn on_did_change_configuration(
    context: ServerContextSnapshot,
    _params: DidChangeConfigurationParams,
) -> Option<()> {
    let client_id = context
        .workspace_manager()
        .read()
        .await
        .client_config
        .client_id;
    let supports_config_request = context.lsp_features().supports_config_request();
    log::info!("change config client_id: {:?}", client_id);

    let mut new_client_config =
        get_client_config(&context, client_id, supports_config_request).await;

    let workspace_manager = context.workspace_manager_arc();
    let mut manager = workspace_manager.write().await;
    new_client_config.keep_session_options(&manager.client_config);
    if manager.client_config == new_client_config {
        log::info!("skip config reload; client config unchanged");
        return Some(());
    }
    manager.client_config = new_client_config;
    manager
        .schedule_config_reload(Duration::ZERO, workspace_manager.clone())
        .await;
    Some(())
}

/// Asks the client to send `workspace/didChangeConfiguration` when a section
/// `client_id`'s settings are read from changes.
pub fn register_did_change_configuration(
    client: &ClientProxy,
    lsp_features: &LspFeatures,
    client_id: ClientId,
) {
    if !lsp_features.supports_did_change_configuration_registration() {
        return;
    }
    client.dynamic_register_capability(RegistrationParams {
        registrations: vec![Registration {
            id: "gluals_did_change_configuration".to_string(),
            method: "workspace/didChangeConfiguration".to_string(),
            register_options: Some(serde_json::json!({
                "section": client_config_sections(client_id),
            })),
        }],
    });
}

pub struct ConfigurationCapabilities;

impl RegisterCapabilities for ConfigurationCapabilities {
    fn register_capabilities(_: &mut ServerCapabilities, _: &ClientCapabilities) {}
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use googletest::prelude::*;
    use lsp_server::{Connection, Message};
    use lsp_types::ClientCapabilities;

    use super::register_did_change_configuration;
    use crate::context::{ClientId, ClientProxy, LspFeatures};

    fn registrations(capabilities: serde_json::Value) -> Vec<serde_json::Value> {
        let (connection, peer) = Connection::memory();
        let client = ClientProxy::new(connection);
        let features = LspFeatures::new(
            serde_json::from_value::<ClientCapabilities>(capabilities)
                .expect("capabilities should deserialize"),
        );
        register_did_change_configuration(&client, &features, ClientId::VSCode);
        std::iter::from_fn(|| peer.receiver.recv_timeout(Duration::from_millis(50)).ok())
            .filter_map(|message| match message {
                Message::Request(request) if request.method == "client/registerCapability" => {
                    Some(request.params)
                }
                _ => None,
            })
            .collect()
    }

    #[gtest]
    fn registers_the_sections_it_reads_when_the_client_allows_it() -> Result<()> {
        let sent = registrations(serde_json::json!({
            "workspace": { "didChangeConfiguration": { "dynamicRegistration": true } }
        }));

        verify_that!(
            sent,
            eq(&vec![serde_json::json!({
                "registrations": [{
                    "id": "gluals_did_change_configuration",
                    "method": "workspace/didChangeConfiguration",
                    "registerOptions": { "section": ["gluals", "files"] },
                }]
            })])
        )
    }

    #[gtest]
    fn registers_nothing_without_dynamic_registration() -> Result<()> {
        verify_that!(registrations(serde_json::json!({})), is_empty())
    }

    /// Settings read again on a change do not carry what the command line and
    /// the initialization options set: the annotations path and gamemode bases
    /// stay.
    #[gtest]
    fn a_settings_change_keeps_the_options_set_at_initialization() -> Result<()> {
        let runtime = tokio::runtime::Runtime::new().expect("tokio runtime should build");
        let kept = runtime.block_on(async {
            let (connection, _peer) = Connection::memory();
            let context =
                crate::context::ServerContext::new(connection, ClientCapabilities::default());
            let snapshot = context.snapshot();
            {
                let mut manager = snapshot.workspace_manager().write().await;
                manager.client_config.gmod_annotations_path = Some("annotations".to_string());
                manager.client_config.gamemode_base_libraries = vec!["sandbox".to_string()];
            }
            super::on_did_change_configuration(
                snapshot.clone(),
                lsp_types::DidChangeConfigurationParams {
                    settings: serde_json::Value::Null,
                },
            )
            .await;
            let manager = snapshot.workspace_manager().read().await;
            (
                manager.client_config.gmod_annotations_path.clone(),
                manager.client_config.gamemode_base_libraries.clone(),
            )
        });
        verify_that!(
            kept,
            eq(&(Some("annotations".to_string()), vec!["sandbox".to_string()]))
        )
    }
}
