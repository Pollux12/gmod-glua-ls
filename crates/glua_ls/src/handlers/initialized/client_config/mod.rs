mod default_config;
mod vscode_config;

use default_config::get_client_config_default;
use serde_json::Value;
use vscode_config::get_client_config_vscode;

use crate::context::{ClientId, ServerContextSnapshot};

#[allow(unused)]
#[derive(Clone, Default, PartialEq)]
pub struct ClientConfig {
    pub client_id: ClientId,
    pub exclude: Vec<String>,
    pub extensions: Vec<String>,
    pub encoding: String,
    pub partial_emmyrcs: Option<Vec<Value>>,
    pub gmod_annotations_path: Option<String>,
    pub gamemode_base_libraries: Vec<String>,
}

/// Leaves out the client's settings, which can hold secrets such as
/// `gluals.gmod.mcp.authToken`.
impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("client_id", &self.client_id)
            .field("exclude", &self.exclude)
            .field("extensions", &self.extensions)
            .field("encoding", &self.encoding)
            .field(
                "partial_emmyrcs",
                &self.partial_emmyrcs.as_ref().map(Vec::len),
            )
            .field("gmod_annotations_path", &self.gmod_annotations_path)
            .field("gamemode_base_libraries", &self.gamemode_base_libraries)
            .finish()
    }
}

impl ClientConfig {
    /// Takes the options `current` got from the command line and the
    /// initialization options, which the client sends once: settings read
    /// again on a change do not carry them.
    pub fn keep_session_options(&mut self, current: &ClientConfig) {
        self.gmod_annotations_path = current.gmod_annotations_path.clone();
        self.gamemode_base_libraries = current.gamemode_base_libraries.clone();
    }
}

pub async fn get_client_config(
    context: &ServerContextSnapshot,
    client_id: ClientId,
    supports_config_request: bool,
) -> ClientConfig {
    let mut config = ClientConfig {
        client_id,
        exclude: Vec::new(),
        extensions: Vec::new(),
        encoding: "utf-8".to_string(),
        partial_emmyrcs: None,
        gmod_annotations_path: None,
        gamemode_base_libraries: Vec::new(),
    };
    if !supports_config_request {
        return config;
    }
    match client_id {
        ClientId::VSCode => {
            get_client_config_vscode(context, &mut config).await;
        }
        ClientId::Neovim => {
            get_client_config_default(context, &mut config, Some(&["Lua", "gluals"])).await;
        }
        _ => {
            get_client_config_default(context, &mut config, None).await;
        }
    };

    config
}

/// The client settings sections `get_client_config` reads for `client_id`.
pub fn client_config_sections(client_id: ClientId) -> &'static [&'static str] {
    match client_id {
        ClientId::VSCode => &["gluals", "files"],
        ClientId::Neovim => &["Lua", "gluals"],
        _ => &["gluals"],
    }
}

#[cfg(test)]
mod tests {
    use super::ClientConfig;

    #[test]
    fn debug_output_leaves_out_client_settings() {
        let config = ClientConfig {
            partial_emmyrcs: Some(vec![serde_json::json!({
                "gmod": { "mcp": { "authToken": "secret-token" } }
            })]),
            ..Default::default()
        };

        assert!(!format!("{config:?}").contains("secret-token"));
    }
}
