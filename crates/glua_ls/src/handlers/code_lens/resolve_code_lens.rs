use glua_code_analysis::LuaCompilation;
use lsp_types::{CodeLens, Command, Location, Range, Uri};
use tokio_util::sync::CancellationToken;

use crate::{
    context::ClientId,
    handlers::references::{search_decl_references, search_member_references},
};

use super::{CodeLensData, CodeLensResolveData};

// VSCode does not support calling editor.action.showReferences directly through LSP,
// it can only be converted through the VSCode plugin
const VSCODE_COMMAND_NAME: &str = "gluals.showReferences";
// In fact, VSCode ultimately uses this command
const OTHER_COMMAND_NAME: &str = "editor.action.showReferences";

pub fn resolve_code_lens(
    compilation: &LuaCompilation,
    code_lens: CodeLens,
    client_id: ClientId,
    cancel_token: &CancellationToken,
) -> Option<CodeLens> {
    let data = decode_code_lens_data(code_lens.data.as_ref()?)?;
    match data.payload {
        CodeLensData::Member(member_id) => {
            let file_id = member_id.file_id;
            let semantic_model = compilation.get_semantic_model(file_id)?;
            let mut results = Vec::new();
            search_member_references(
                &semantic_model,
                compilation,
                member_id,
                &mut results,
                cancel_token,
            );
            let mut ref_count = results.len();
            ref_count = ref_count.saturating_sub(1);
            let uri = semantic_model.get_document().get_uri();
            let command = make_usage_command(uri, code_lens.range, ref_count, client_id, results);

            Some(CodeLens {
                range: code_lens.range,
                command: Some(command),
                data: None,
            })
        }
        CodeLensData::DeclId(decl_id) => {
            let file_id = decl_id.file_id;
            let semantic_model = compilation.get_semantic_model(file_id)?;
            let mut results = Vec::new();
            search_decl_references(
                &semantic_model,
                compilation,
                decl_id,
                &mut results,
                cancel_token,
            );
            let ref_count = results.len();
            let uri = semantic_model.get_document().get_uri();
            let command = make_usage_command(uri, code_lens.range, ref_count, client_id, results);
            Some(CodeLens {
                range: code_lens.range,
                command: Some(command),
                data: None,
            })
        }
        CodeLensData::NetMessage(message_name) => {
            let db = compilation.get_db();
            let network_index = db.get_gmod_network_index();
            let send_flows = network_index.get_send_flows_for_message(&message_name);
            let receive_flows = network_index.get_receive_flows_for_message(&message_name);
            let vfs = db.get_vfs();
            let mut locations: Vec<Location> = Vec::new();
            let mut sender_count = 0usize;
            for (file_id, flow) in &send_flows {
                let Some(uri) = vfs.get_uri(file_id) else {
                    continue;
                };
                let Some(document) = vfs.get_document(file_id) else {
                    continue;
                };
                let Some(range) = document.to_lsp_range(flow.start_range) else {
                    continue;
                };
                locations.push(Location::new(uri, range));
                sender_count += 1;
            }
            let mut receiver_count = 0usize;
            for (file_id, flow) in &receive_flows {
                let Some(uri) = vfs.get_uri(file_id) else {
                    continue;
                };
                let Some(document) = vfs.get_document(file_id) else {
                    continue;
                };
                let Some(range) = document.to_lsp_range(flow.receive_range) else {
                    continue;
                };
                locations.push(Location::new(uri, range));
                receiver_count += 1;
            }
            let total = sender_count + receiver_count;
            let usage_count = total.saturating_sub(1);
            let lens_uri = data
                .uri
                .clone()
                .or_else(|| locations.first().map(|loc| loc.uri.clone()))?;
            let title = format_net_usages_title(usage_count, sender_count, receiver_count);
            let command = make_command_with_title(
                lens_uri,
                code_lens.range,
                title,
                client_id,
                locations,
            );
            Some(CodeLens {
                range: code_lens.range,
                command: Some(command),
                data: None,
            })
        }
    }
}

fn decode_code_lens_data(value: &serde_json::Value) -> Option<CodeLensResolveData> {
    serde_json::from_value::<CodeLensResolveData>(value.clone())
        .ok()
        .or_else(|| {
            serde_json::from_value::<CodeLensData>(value.clone())
                .ok()
                .map(|payload| CodeLensResolveData { uri: None, payload })
        })
}

fn get_command_name(client_id: ClientId) -> &'static str {
    match client_id {
        ClientId::VSCode => VSCODE_COMMAND_NAME,
        _ => OTHER_COMMAND_NAME,
    }
}

fn make_usage_command(
    uri: Uri,
    range: Range,
    ref_count: usize,
    client_id: ClientId,
    refs: Vec<Location>,
) -> Command {
    let title = format!(
        "{} usage{}",
        ref_count,
        if ref_count == 1 { "" } else { "s" }
    );
    make_command_with_title(uri, range, title, client_id, refs)
}

fn make_command_with_title(
    uri: Uri,
    range: Range,
    title: String,
    client_id: ClientId,
    refs: Vec<Location>,
) -> Command {
    let args = vec![
        serde_json::to_value(uri).unwrap(),
        serde_json::to_value(range.start).unwrap(),
        serde_json::to_value(refs).unwrap(),
    ];

    Command {
        title,
        command: get_command_name(client_id).to_string(),
        arguments: Some(args),
    }
}

fn format_net_usages_title(
    usage_count: usize,
    sender_count: usize,
    receiver_count: usize,
) -> String {
    let usage_word = if usage_count == 1 { "usage" } else { "usages" };
    let sender_word = if sender_count == 1 { "sender" } else { "senders" };
    let receiver_word = if receiver_count == 1 {
        "receiver"
    } else {
        "receivers"
    };
    format!(
        "{usage_count} {usage_word} ({sender_count} {sender_word}, {receiver_count} {receiver_word})"
    )
}
