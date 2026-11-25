use std::{collections::HashMap, time::Duration};

use glua_code_analysis::FileId;
use glua_parser::{LuaAstNode, LuaStat};
use lsp_types::{ApplyWorkspaceEditParams, Command, Position, TextEdit, WorkspaceEdit};
use serde_json::Value;

use crate::{
    context::ServerContextSnapshot, handlers::fold_range::is_require_stat, util::time_cancel_token,
};

use super::CommandSpec;

pub struct AutoRequireCommand;

impl CommandSpec for AutoRequireCommand {
    const COMMAND: &str = "gluals.auto.require";

    async fn handle(context: ServerContextSnapshot, args: Vec<Value>) -> Option<()> {
        if !context.lsp_features().supports_apply_edit() {
            log::warn!("auto-require skipped: client does not support workspace/applyEdit");
            return None;
        }

        let add_to: FileId = serde_json::from_value(args.first()?.clone()).ok()?;
        let need_require_file_id: FileId = serde_json::from_value(args.get(1)?.clone()).ok()?;
        let position: Position = serde_json::from_value(args.get(2)?.clone()).ok()?;
        let local_name: String = serde_json::from_value(args.get(3)?.clone()).ok()?;
        let member_name: String = serde_json::from_value(args.get(4)?.clone()).ok()?;

        let analysis = context.analysis().read().await;
        let semantic_model = analysis.compilation.get_semantic_model(add_to)?;
        let module_info = semantic_model
            .get_db()
            .get_module_index()
            .get_module(need_require_file_id)?;
        let emmyrc = semantic_model.get_emmyrc();
        let auto_require_func = emmyrc.completion.auto_require_function.clone();
        let require_separator = emmyrc.completion.auto_require_separator.clone();
        let full_module_path = match require_separator.as_str() {
            "." | "" => module_info.full_module_name.clone(),
            _ => module_info
                .full_module_name
                .replace(".", &require_separator),
        };

        let require_str = format!(
            "local {} = {}(\"{}\"){}",
            if member_name.is_empty() {
                local_name
            } else {
                member_name.clone()
            },
            auto_require_func,
            full_module_path,
            if !member_name.is_empty() {
                format!(".{}", member_name)
            } else {
                "".to_string()
            }
        );
        let document = semantic_model.get_document();
        let offset = document.to_offset(position);
        let root_block = semantic_model.get_root().get_block()?;
        let mut last_require_stat: Option<LuaStat> = None;
        for stat in root_block.get_stats() {
            if stat.get_position() > offset {
                break;
            }

            if is_require_stat(&semantic_model, &stat) {
                last_require_stat = Some(stat);
            }
        }

        let line = if let Some(last_require_stat) = last_require_stat {
            let last_require_stat_end = last_require_stat.get_range().end();
            document.get_line(last_require_stat_end)? + 1
        } else {
            0
        };

        let text_edit = TextEdit {
            range: lsp_types::Range {
                start: Position {
                    line: line as u32,
                    character: 0,
                },
                end: Position {
                    line: line as u32,
                    character: 0,
                },
            },
            new_text: format!("{}\n", require_str),
        };

        let uri = document.get_uri();
        #[allow(clippy::mutable_key_type)]
        let mut changes = HashMap::new();
        changes.insert(uri.clone(), vec![text_edit.clone()]);

        let cancel_token = time_cancel_token(Duration::from_secs(5));
        let apply_edit_params = ApplyWorkspaceEditParams {
            label: None,
            edit: WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            },
        };

        let context_clone = context.clone();
        tokio::spawn(async move {
            let res = context_clone
                .client()
                .apply_edit(apply_edit_params, cancel_token)
                .await;
            if let Some(res) = res
                && !res.applied
            {
                log::error!("Failed to apply edit: {:?}", res.failure_reason);
            }
        });

        Some(())
    }
}

pub fn make_auto_require(
    title: &str,
    add_to: FileId,
    need_require_file_id: FileId,
    position: Position,
    local_name: String,          // Name used on import
    member_name: Option<String>, // Imported member name, without the leading `.`; it is appended after `require`. For example require("a").member
) -> Command {
    let args = vec![
        serde_json::to_value(add_to).unwrap(),
        serde_json::to_value(need_require_file_id).unwrap(),
        serde_json::to_value(position).unwrap(),
        serde_json::to_value(local_name).unwrap(),
        serde_json::to_value(member_name.unwrap_or_default()).unwrap(),
    ];

    Command {
        title: title.to_string(),
        command: AutoRequireCommand::COMMAND.to_string(),
        arguments: Some(args),
    }
}
