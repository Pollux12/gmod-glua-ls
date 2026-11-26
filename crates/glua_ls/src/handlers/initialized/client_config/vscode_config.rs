use super::ClientConfig;
use crate::context::ServerContextSnapshot;
use crate::handlers::initialized::client_config::default_config::get_client_config_default;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, time::Duration};

#[derive(Debug, Deserialize, Serialize)]
struct VscodeFilesConfig {
    exclude: Option<HashMap<String, bool>>,
    associations: Option<HashMap<String, String>>,
    encoding: Option<String>,
}

pub async fn get_client_config_vscode(
    context: &ServerContextSnapshot,
    config: &mut ClientConfig,
) -> Option<()> {
    get_client_config_default(context, config, None).await;

    let client = context.client();
    let params = lsp_types::ConfigurationParams {
        items: vec![lsp_types::ConfigurationItem {
            scope_uri: None,
            section: Some("files".to_string()),
        }],
    };
    let files_configs = client
        .get_configuration::<VscodeFilesConfig>(params, Duration::from_secs(5))
        .await?;
    for files_config in files_configs {
        if let Some(exclude) = files_config.exclude {
            for (pattern, _) in exclude {
                config.exclude.push(pattern);
            }
        }
        if let Some(associations) = files_config.associations {
            for (pattern, extension) in associations {
                if extension == "lua" || extension == "Lua" {
                    config.extensions.push(pattern);
                }
            }
        }
        config.encoding = files_config.encoding.unwrap_or("utf-8".to_string());
    }

    Some(())
}
