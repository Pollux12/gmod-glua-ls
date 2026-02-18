use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use emmylua_code_analysis::{LuaType, file_path_to_uri};
use emmylua_parser::{
    LuaAstNode, LuaAstToken, LuaCallArgList, LuaCallExpr, LuaLiteralExpr, LuaStringToken, PathTrait,
};
use lsp_types::{CompletionItem, TextEdit};

use crate::handlers::completion::completion_builder::CompletionBuilder;

use super::get_text_edit_range_in_string;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathCompletionKind {
    File,
    Folder,
    Any,
}

pub fn add_completion(builder: &mut CompletionBuilder) -> Option<()> {
    if builder.is_cancelled() {
        return None;
    }

    let string_token = LuaStringToken::cast(builder.trigger_token.clone())?;
    let text_edit_range = get_text_edit_range_in_string(builder, string_token.clone())?;
    let typed_path = string_token.get_value().replace('\\', "/");
    let has_separator = typed_path.contains('/');
    let (prefix, typed_name_prefix) = split_path_prefix(&typed_path);

    let context = detect_path_context(builder, &string_token);
    if context.is_none() && !has_separator {
        return None;
    }

    let roots = context
        .as_ref()
        .map(|(roots, _)| roots.clone())
        .unwrap_or_else(|| collect_resource_roots(builder));
    let completion_kind = context
        .as_ref()
        .map(|(_, completion_kind)| *completion_kind)
        .unwrap_or(PathCompletionKind::Any);

    let mut seen_insert_text = HashSet::new();
    let mut added_any = false;

    for root in roots {
        let folder = root.join(Path::new(&prefix));
        if !folder.is_dir() {
            continue;
        }

        let Ok(entries) = std::fs::read_dir(folder) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

            if !name
                .to_ascii_lowercase()
                .starts_with(&typed_name_prefix.to_ascii_lowercase())
            {
                continue;
            }

            if completion_kind == PathCompletionKind::Folder && !path.is_dir() {
                continue;
            }

            if add_file_path_completion(
                builder,
                &path,
                name,
                &prefix,
                text_edit_range,
                &mut seen_insert_text,
            )
            .is_some()
            {
                added_any = true;
            }
        }
    }

    if added_any || context.is_some() {
        builder.stop_here();
    }

    Some(())
}

fn detect_path_context(
    builder: &CompletionBuilder,
    string_token: &LuaStringToken,
) -> Option<(Vec<PathBuf>, PathCompletionKind)> {
    let literal_expr = string_token.get_parent::<LuaLiteralExpr>()?;
    let args_list = literal_expr.get_parent::<LuaCallArgList>()?;
    let call_expr = args_list.get_parent::<LuaCallExpr>()?;
    let arg_idx = args_list
        .get_args()
        .position(|arg| arg.get_position() == literal_expr.get_position())?;

    if is_include_loader_context(&call_expr, arg_idx) {
        return Some((collect_contextual_roots(builder), PathCompletionKind::File));
    }

    let completion_kind = infer_param_path_completion_kind(builder, &call_expr, arg_idx)?;
    Some((collect_contextual_roots(builder), completion_kind))
}

fn is_include_loader_context(call_expr: &LuaCallExpr, arg_idx: usize) -> bool {
    if arg_idx != 0 {
        return false;
    }

    let Some(call_path) = call_expr.get_access_path() else {
        return false;
    };

    matches_call_path(&call_path, "include") || matches_call_path(&call_path, "AddCSLuaFile")
}

fn matches_call_path(path: &str, target: &str) -> bool {
    path == target || path.ends_with(&format!(".{target}")) || path.ends_with(&format!(":{target}"))
}

fn infer_param_path_completion_kind(
    builder: &CompletionBuilder,
    call_expr: &LuaCallExpr,
    arg_idx: usize,
) -> Option<PathCompletionKind> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    let call_type = builder.semantic_model.infer_expr(prefix_expr).ok()?;
    let call_is_colon = call_expr.is_colon_call();

    infer_path_kind_from_call_type(builder, &call_type, call_is_colon, arg_idx)
}

fn infer_path_kind_from_call_type(
    builder: &CompletionBuilder,
    call_type: &LuaType,
    call_is_colon: bool,
    arg_idx: usize,
) -> Option<PathCompletionKind> {
    match call_type {
        LuaType::DocFunction(func) => {
            let param_idx =
                map_call_param_to_decl_param_idx(arg_idx, func.is_colon_define(), call_is_colon)?;
            let (_, param_type) = func.get_params().get(param_idx)?;
            classify_path_type(param_type.as_ref()?)
        }
        LuaType::Signature(signature_id) => {
            let signature = builder
                .semantic_model
                .get_db()
                .get_signature_index()
                .get(signature_id)?;
            let param_idx = map_call_param_to_decl_param_idx(
                arg_idx,
                signature.is_colon_define,
                call_is_colon,
            )?;
            let param_type = &signature.get_param_info_by_id(param_idx)?.type_ref;
            classify_path_type(param_type)
        }
        LuaType::Union(union_type) => {
            let mut kind = None;
            for member_type in union_type.into_vec() {
                let member_kind =
                    infer_path_kind_from_call_type(builder, &member_type, call_is_colon, arg_idx)?;
                kind = Some(merge_path_completion_kind(kind, member_kind));
            }
            kind
        }
        LuaType::TypeGuard(inner) => {
            infer_path_kind_from_call_type(builder, inner, call_is_colon, arg_idx)
        }
        _ => None,
    }
}

fn map_call_param_to_decl_param_idx(
    arg_idx: usize,
    decl_is_colon_define: bool,
    call_is_colon: bool,
) -> Option<usize> {
    match (decl_is_colon_define, call_is_colon) {
        (true, false) => arg_idx.checked_sub(1),
        (false, true) => Some(arg_idx + 1),
        _ => Some(arg_idx),
    }
}

fn classify_path_type(typ: &LuaType) -> Option<PathCompletionKind> {
    let type_name = match typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => type_id.get_simple_name().to_string(),
        LuaType::Language(name) => name.to_string(),
        LuaType::Union(union_type) => {
            let mut kind = None;
            for member_type in union_type.into_vec() {
                let member_kind = classify_path_type(&member_type)?;
                kind = Some(merge_path_completion_kind(kind, member_kind));
            }
            return kind;
        }
        LuaType::TypeGuard(inner) => return classify_path_type(inner),
        _ => return None,
    };

    classify_path_type_name(&type_name)
}

fn classify_path_type_name(type_name: &str) -> Option<PathCompletionKind> {
    let lower_name = type_name.to_ascii_lowercase();
    match lower_name.as_str() {
        "folder" | "directory" | "dir" => Some(PathCompletionKind::Folder),
        "file" | "filepath" | "filename" => Some(PathCompletionKind::File),
        "path" => Some(PathCompletionKind::Any),
        _ => None,
    }
}

fn merge_path_completion_kind(
    current: Option<PathCompletionKind>,
    next: PathCompletionKind,
) -> PathCompletionKind {
    match current {
        None => next,
        Some(existing) if existing == next => existing,
        _ => PathCompletionKind::Any,
    }
}

fn collect_contextual_roots(builder: &CompletionBuilder) -> Vec<PathBuf> {
    let mut roots = collect_resource_roots(builder);
    let file_id = builder.semantic_model.get_file_id();
    let file_path = builder
        .semantic_model
        .get_db()
        .get_vfs()
        .get_file_path(&file_id)
        .cloned();

    if let Some(file_path) = file_path {
        if let Some(parent_dir) = file_path.parent() {
            roots.push(parent_dir.to_path_buf());
        }
        if let Some(lua_root) = find_lua_root(&file_path) {
            roots.push(lua_root);
        }
    }

    dedup_existing_dirs(roots)
}

fn collect_resource_roots(builder: &CompletionBuilder) -> Vec<PathBuf> {
    let roots = builder
        .semantic_model
        .get_db()
        .get_effective_resource_paths();

    dedup_existing_dirs(roots)
}

fn dedup_existing_dirs(roots: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut dedup = HashSet::new();
    let mut result = Vec::new();
    for root in roots {
        if !root.is_dir() {
            continue;
        }

        if dedup.insert(root.clone()) {
            result.push(root);
        }
    }

    result
}

fn find_lua_root(file_path: &Path) -> Option<PathBuf> {
    for ancestor in file_path.ancestors() {
        let Some(name) = ancestor.file_name().and_then(|name| name.to_str()) else {
            continue;
        };

        if name.eq_ignore_ascii_case("lua") {
            return Some(ancestor.to_path_buf());
        }
    }

    None
}

fn split_path_prefix(path: &str) -> (String, String) {
    if let Some(last_sep) = path.rfind('/') {
        let prefix = path[..last_sep + 1].to_string();
        let file_name_prefix = path[last_sep + 1..].to_string();
        (prefix, file_name_prefix)
    } else {
        (String::new(), path.to_string())
    }
}

fn add_file_path_completion(
    builder: &mut CompletionBuilder,
    path: &PathBuf,
    name: &str,
    prefix: &str,
    text_edit_range: lsp_types::Range,
    seen_insert_text: &mut HashSet<String>,
) -> Option<()> {
    let kind: lsp_types::CompletionItemKind = if path.is_dir() {
        lsp_types::CompletionItemKind::FOLDER
    } else {
        lsp_types::CompletionItemKind::FILE
    };

    let detail = file_path_to_uri(path).map(|uri| uri.to_string());

    let filter_text = format!("{}{}", prefix, name);
    if !seen_insert_text.insert(filter_text.clone()) {
        return None;
    }

    let text_edit = TextEdit {
        range: text_edit_range,
        new_text: filter_text.clone(),
    };
    let completion_item = CompletionItem {
        label: name.to_string(),
        kind: Some(kind),
        filter_text: Some(filter_text),
        text_edit: Some(lsp_types::CompletionTextEdit::Edit(text_edit)),
        detail,
        ..Default::default()
    };

    builder.add_completion_item(completion_item)?;

    Some(())
}

#[cfg(test)]
mod tests {
    use super::{
        PathCompletionKind, classify_path_type_name, map_call_param_to_decl_param_idx,
        merge_path_completion_kind,
    };

    #[test]
    fn test_classify_path_type_name_file_folder_and_path() {
        assert_eq!(
            classify_path_type_name("file"),
            Some(PathCompletionKind::File)
        );
        assert_eq!(
            classify_path_type_name("directory"),
            Some(PathCompletionKind::Folder)
        );
        assert_eq!(
            classify_path_type_name("path"),
            Some(PathCompletionKind::Any)
        );
        assert_eq!(classify_path_type_name("string"), None);
    }

    #[test]
    fn test_map_call_param_idx_for_colon_rules() {
        assert_eq!(map_call_param_to_decl_param_idx(0, false, false), Some(0));
        assert_eq!(map_call_param_to_decl_param_idx(0, false, true), Some(1));
        assert_eq!(map_call_param_to_decl_param_idx(1, true, false), Some(0));
        assert_eq!(map_call_param_to_decl_param_idx(0, true, false), None);
    }

    #[test]
    fn test_merge_path_completion_kind() {
        assert_eq!(
            merge_path_completion_kind(Some(PathCompletionKind::File), PathCompletionKind::File),
            PathCompletionKind::File
        );
        assert_eq!(
            merge_path_completion_kind(Some(PathCompletionKind::File), PathCompletionKind::Folder),
            PathCompletionKind::Any
        );
    }
}
