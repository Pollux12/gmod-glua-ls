use std::collections::HashMap;

use glua_code_analysis::{LuaTypeDeclId, SemanticModel};
use lsp_types::Uri;

#[allow(clippy::mutable_key_type)]
pub fn rename_type_references(
    semantic_model: &SemanticModel,
    type_decl_id: LuaTypeDeclId,
    new_name: String,
    result: &mut HashMap<Uri, HashMap<lsp_types::Range, String>>,
) -> Option<()> {
    let type_index = semantic_model.get_db().get_type_index();
    let type_decl = type_index.get_type_decl(&type_decl_id)?;
    let full_name = type_decl.get_full_name();
    let mut reserved_namespace = String::new();

    // Take the namespace `full_name` uses in the current file
    if let Some(file_namespace) = type_index.get_file_namespace(&semantic_model.get_file_id())
        && full_name.starts_with(&format!("{}.", file_namespace))
    {
        reserved_namespace = file_namespace.clone();
    }
    if reserved_namespace.is_empty()
        && let Some(using_namespaces) =
            type_index.get_file_using_namespace(&semantic_model.get_file_id())
    {
        for using_namespace in using_namespaces {
            if full_name.starts_with(&format!("{}.", using_namespace)) {
                reserved_namespace = using_namespace.clone();
                break;
            }
        }
    }

    let locations = type_decl.get_locations();
    for decl_location in locations {
        let document = semantic_model.get_document_by_file_id(decl_location.file_id)?;
        let range = document.to_lsp_range(decl_location.range)?;
        result
            .entry(document.get_uri())
            .or_default()
            .insert(range, new_name.clone());
    }

    let refs = semantic_model
        .get_db()
        .get_reference_index()
        .get_type_references(&type_decl_id)?;
    let mut document_cache = HashMap::new();
    for in_filed_reference_range in refs {
        let document = if let Some(document) = document_cache.get(&in_filed_reference_range.file_id)
        {
            document
        } else {
            let document =
                semantic_model.get_document_by_file_id(in_filed_reference_range.file_id)?;
            document_cache.insert(in_filed_reference_range.file_id, document);
            document_cache.get(&in_filed_reference_range.file_id)?
        };

        // Use the short or full name depending on the namespace context of the referencing file
        let actual_new_name = determine_type_name_for_file(
            semantic_model,
            in_filed_reference_range.file_id,
            &reserved_namespace,
            &new_name,
        );

        let location = document.to_lsp_location(in_filed_reference_range.value)?;
        result
            .entry(location.uri)
            .or_default()
            .insert(location.range, actual_new_name);
    }

    Some(())
}

/// Uses the short or full name depending on the namespace context of the referencing file
fn determine_type_name_for_file(
    semantic_model: &SemanticModel,
    reference_file_id: glua_code_analysis::FileId,
    reserved_namespace: &str,
    new_simple_name: &str,
) -> String {
    let type_index = semantic_model.get_db().get_type_index();

    // Check whether the referencing file declares the same namespace
    if let Some(file_namespace) = type_index.get_file_namespace(&reference_file_id) {
        if file_namespace == reserved_namespace {
            return new_simple_name.to_string();
        }
        if reserved_namespace.starts_with(&format!("{}.", file_namespace)) {
            return new_simple_name.to_string();
        }
    }

    // Check whether the referencing file uses the corresponding namespace
    if let Some(using_namespaces) = type_index.get_file_using_namespace(&reference_file_id) {
        for using_namespace in using_namespaces {
            if using_namespace == reserved_namespace {
                return new_simple_name.to_string();
            }
            if reserved_namespace.starts_with(&format!("{}.", using_namespace)) {
                return new_simple_name.to_string();
            }
        }
    }

    // If `reserved_namespace` is not empty, combine it with the new name
    if !reserved_namespace.is_empty() {
        return format!("{}.{}", reserved_namespace, new_simple_name);
    }

    new_simple_name.to_string()
}
