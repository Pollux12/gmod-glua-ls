mod attribute_tags;
mod diagnostic_tags;
mod field_or_operator_def_tags;
mod file_generic_index;
mod infer_type;
mod property_tags;
mod tags;
mod type_def_tags;
mod type_ref_tags;

use super::AnalyzeContext;
use super::common::estimate_file_index_entries;
use crate::{
    FileId, LuaDocDefaultValue, LuaSemanticDeclId, LuaType, TypeOps, WorkspaceId,
    compilation::analyzer::AnalysisPipeline,
    db_index::{DbIndex, LuaTypeDeclId},
    profile::Profile,
};
use file_generic_index::FileGenericIndex;
use glua_parser::{
    LuaAstNode, LuaComment, LuaDocTagDefaultValue as ParsedDocDefaultValue, LuaSyntaxNode,
};
use tags::get_owner_id;

pub(super) fn convert_doc_default_value(default: ParsedDocDefaultValue) -> LuaDocDefaultValue {
    match default {
        ParsedDocDefaultValue::Nil => LuaDocDefaultValue::Nil,
        ParsedDocDefaultValue::Boolean(value) => LuaDocDefaultValue::Boolean(value),
        ParsedDocDefaultValue::Number(value) => LuaDocDefaultValue::Number(value),
        ParsedDocDefaultValue::String(value) => LuaDocDefaultValue::String(value),
        ParsedDocDefaultValue::Expression(value) => LuaDocDefaultValue::Expression(value),
    }
}

pub(super) fn apply_nullable_doc_default(
    analyzer: &mut DocAnalyzer,
    type_ref: LuaType,
    nullable: bool,
    default_value: Option<&LuaDocDefaultValue>,
) -> LuaType {
    if (nullable || matches!(default_value, Some(LuaDocDefaultValue::Nil)))
        && !type_ref.is_nullable()
    {
        TypeOps::Union.apply(analyzer.db, &type_ref, &LuaType::Nil)
    } else {
        type_ref
    }
}

pub struct DocAnalysisPipeline;

impl AnalysisPipeline for DocAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("doc analyze", context.tree_list.len() > 1);
        let tree_list = context.tree_list.clone();
        let workspace_id = context.workspace_id.unwrap_or(WorkspaceId::MAIN);
        for in_filed_tree in tree_list.iter() {
            let root = &in_filed_tree.value;
            register_property_merge_order(db, in_filed_tree.file_id);
            // Pre-size the member and reference tables for this file's writes.
            // The decl phase already ran for this batch, so its decl count is
            // a known per-file fact; the text length bounds the comment/tag
            // walk without walking it. Either may over- or under-shoot; both
            // only change rehashing, never indexed facts.
            let decl_count = db
                .get_decl_index()
                .get_decl_tree(&in_filed_tree.file_id)
                .map(|decl_tree| decl_tree.get_decls().len())
                .unwrap_or(0);
            let estimate =
                estimate_file_index_entries(usize::from(root.syntax().text_range().len()))
                    .max(decl_count.saturating_add(8).min(4096));
            db.get_member_index_mut()
                .reserve_for_file(in_filed_tree.file_id, estimate);
            db.get_reference_index_mut()
                .reserve_for_file(in_filed_tree.file_id, estimate);
            let mut generic_index = FileGenericIndex::new();
            for comment in root.descendants::<LuaComment>() {
                let mut analyzer = DocAnalyzer::new(
                    db,
                    in_filed_tree.file_id,
                    &mut generic_index,
                    comment,
                    root.syntax().clone(),
                    workspace_id,
                );
                analyze_comment(&mut analyzer);
            }
        }
    }
}

/// A class documented in several files shares one property. Tell the property index how those
/// files order against each other. A file in no known workspace sorts after every ranked one.
fn register_property_merge_order(db: &mut DbIndex, file_id: FileId) {
    let module_index = db.get_module_index();
    let workspace_rank = module_index
        .get_workspace_id(file_id)
        .map(|workspace_id| module_index.get_workspace_kind(workspace_id).merge_rank())
        .unwrap_or(crate::WorkspaceKind::MERGE_RANK_NONE);
    let path = db.get_vfs().file_order_key(&file_id);
    db.get_property_index_mut()
        .set_file_merge_order(file_id, workspace_rank, path);
}

fn analyze_comment(analyzer: &mut DocAnalyzer) -> Option<()> {
    let comment = analyzer.comment.clone();
    for tag in comment.get_doc_tags() {
        tags::analyze_tag(analyzer, tag);
    }

    let owenr = get_owner_id(analyzer, None, false)?;
    let comment_description = preprocess_description(
        &comment.get_description()?.get_description_text(),
        Some(&owenr),
    );
    analyzer.db.get_property_index_mut().add_description(
        analyzer.file_id,
        owenr,
        comment_description,
    );

    Some(())
}

#[derive(Debug)]
pub struct DocAnalyzer<'a> {
    file_id: FileId,
    db: &'a mut DbIndex,
    generic_index: &'a mut FileGenericIndex,
    current_type_id: Option<LuaTypeDeclId>,
    comment: LuaComment,
    root: LuaSyntaxNode,
    is_meta: bool,
    workspace_id: WorkspaceId,
}

impl<'a> DocAnalyzer<'a> {
    pub fn new(
        db: &'a mut DbIndex,
        file_id: FileId,
        generic_index: &'a mut FileGenericIndex,
        comment: LuaComment,
        root: LuaSyntaxNode,
        workspace_id: WorkspaceId,
    ) -> DocAnalyzer<'a> {
        let is_meta = db.get_module_index().is_meta_file(&file_id);
        DocAnalyzer {
            file_id,
            db,
            generic_index,
            current_type_id: None,
            comment,
            root,
            is_meta,
            workspace_id,
        }
    }
}

pub fn preprocess_description(mut description: &str, owner: Option<&LuaSemanticDeclId>) -> String {
    let need_remove_start_char = if let Some(owner) = owner {
        !matches!(owner, LuaSemanticDeclId::Signature(_))
    } else {
        true
    };
    if need_remove_start_char && description.starts_with(['#', '@']) {
        description = description.trim_start_matches(['#', '@']);
    }

    let mut result = String::new();
    let lines = description.lines();
    let mut start_with_one_space = None;
    for mut line in lines {
        let indent_count = line.chars().take_while(|c| c.is_whitespace()).count();
        if indent_count == line.len() {
            // empty line
            result.push('\n');
            continue;
        }

        if start_with_one_space.is_none() {
            start_with_one_space = Some(indent_count == 1);
        }

        if let Some(true) = start_with_one_space {
            let mut chars = line.chars();
            let first_char = chars.next();
            if let Some(c) = first_char
                && c.is_whitespace()
            {
                line = chars.as_str();
            }
        }

        result.push_str(line);
        result.push('\n');
    }

    // trim end
    result.trim_end().to_string()
}

pub(super) fn normalize_doc_string_const(value: String) -> String {
    if let Some(inner) = value.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')) {
        return inner.to_string();
    }
    if let Some(inner) = value.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        return inner.to_string();
    }

    value
}
