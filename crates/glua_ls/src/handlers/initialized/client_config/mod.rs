mod default_config;
mod vscode_config;

use default_config::get_client_config_default;
use serde_json::Value;
use vscode_config::get_client_config_vscode;

use crate::context::{ClientId, ServerContextSnapshot};

#[allow(unused)]
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClientConfig {
    pub client_id: ClientId,
    pub exclude: Vec<String>,
    pub extensions: Vec<String>,
    pub encoding: String,
    pub partial_emmyrcs: Option<Vec<Value>>,
    pub gmod_annotations_path: Option<String>,
    pub gamemode_base_libraries: Vec<String>,
    pub gmod_plugin_library_paths: Vec<String>,
}

impl ClientConfig {
    pub fn preserve_initialization_options_from(mut self, previous: &Self) -> Self {
        if self.gmod_annotations_path.is_none() {
            self.gmod_annotations_path = previous.gmod_annotations_path.clone();
        }
        if self.gamemode_base_libraries.is_empty() {
            self.gamemode_base_libraries = previous.gamemode_base_libraries.clone();
        }
        if self.gmod_plugin_library_paths.is_empty() {
            self.gmod_plugin_library_paths = previous.gmod_plugin_library_paths.clone();
        }

        self
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
        gmod_plugin_library_paths: Vec::new(),
    };
    match client_id {
        ClientId::VSCode => {
            get_client_config_vscode(context, &mut config).await;
        }
        ClientId::Neovim => {
            get_client_config_default(context, &mut config, Some(&["Lua", "gluals"])).await;
        }
        _ if supports_config_request => {
            get_client_config_default(context, &mut config, None).await;
        }
        _ => {}
    };

    config
}

#[cfg(test)]
mod tests {
    use super::ClientConfig;

    #[test]
    fn preserve_initialization_options_from_keeps_init_only_paths() {
        let previous = ClientConfig {
            gmod_annotations_path: Some("/annotations".to_string()),
            gamemode_base_libraries: vec!["/gamemodes/base".to_string()],
            gmod_plugin_library_paths: vec!["/plugins/darkrp".to_string()],
            ..Default::default()
        };

        let next = ClientConfig {
            extensions: vec!["*.lua".to_string()],
            ..Default::default()
        }
        .preserve_initialization_options_from(&previous);

        assert_eq!(next.gmod_annotations_path, previous.gmod_annotations_path);
        assert_eq!(
            next.gamemode_base_libraries,
            previous.gamemode_base_libraries
        );
        assert_eq!(
            next.gmod_plugin_library_paths,
            previous.gmod_plugin_library_paths
        );
        assert_eq!(next.extensions, vec!["*.lua".to_string()]);
    }

    #[test]
    fn preserve_initialization_options_from_keeps_new_values_when_present() {
        let previous = ClientConfig {
            gmod_annotations_path: Some("/old-annotations".to_string()),
            gamemode_base_libraries: vec!["/old-base".to_string()],
            gmod_plugin_library_paths: vec!["/old-plugin".to_string()],
            ..Default::default()
        };

        let next = ClientConfig {
            gmod_annotations_path: Some("/new-annotations".to_string()),
            gamemode_base_libraries: vec!["/new-base".to_string()],
            gmod_plugin_library_paths: vec!["/new-plugin".to_string()],
            ..Default::default()
        }
        .preserve_initialization_options_from(&previous);

        assert_eq!(
            next.gmod_annotations_path,
            Some("/new-annotations".to_string())
        );
        assert_eq!(next.gamemode_base_libraries, vec!["/new-base".to_string()]);
        assert_eq!(
            next.gmod_plugin_library_paths,
            vec!["/new-plugin".to_string()]
        );
    }
}
