use emmylua_code_analysis::{
    DbIndex, LuaDeclId, LuaDeclTypeKind, LuaSemanticDeclId, LuaSignatureId, LuaType, LuaTypeDeclId,
    RenderLevel, humanize_type,
};
use tokio_util::sync::CancellationToken;

use super::doc_search_request::GluaDocItem;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum MatchRank {
    Exact,
    Prefix,
    Contains,
}

#[derive(Debug, Clone)]
enum DocSymbol {
    Global(LuaDeclId),
    Type(LuaTypeDeclId),
}

#[derive(Debug, Clone)]
struct DocCandidate {
    name: String,
    full_name: String,
    rank: MatchRank,
    symbol: DocSymbol,
}

pub fn build_doc_search(
    db: &DbIndex,
    query: &str,
    limit: usize,
    cancel_token: &CancellationToken,
) -> Option<Vec<GluaDocItem>> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let capped_limit = limit.min(20);
    let query = query.trim();
    if capped_limit == 0 || query.is_empty() {
        return Some(Vec::new());
    }

    let case_sensitive = query.chars().any(|c| c.is_uppercase());
    let mut candidates = Vec::new();
    collect_global_candidates(db, query, case_sensitive, cancel_token, &mut candidates)?;
    collect_type_candidates(db, query, case_sensitive, cancel_token, &mut candidates)?;

    candidates.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.full_name.cmp(&right.full_name))
    });
    candidates.truncate(capped_limit);

    let mut items = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if cancel_token.is_cancelled() {
            return None;
        }

        if let Some(item) = build_doc_item(db, candidate) {
            items.push(item);
        }
    }

    Some(items)
}

fn collect_global_candidates(
    db: &DbIndex,
    query: &str,
    case_sensitive: bool,
    cancel_token: &CancellationToken,
    candidates: &mut Vec<DocCandidate>,
) -> Option<()> {
    for decl_id in db.get_global_index().get_all_global_decl_ids() {
        if cancel_token.is_cancelled() {
            return None;
        }

        let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
            continue;
        };

        let Some(rank) = match_rank(decl.get_name(), query, case_sensitive) else {
            continue;
        };

        candidates.push(DocCandidate {
            name: decl.get_name().to_string(),
            full_name: decl.get_name().to_string(),
            rank,
            symbol: DocSymbol::Global(decl_id),
        });
    }

    Some(())
}

fn collect_type_candidates(
    db: &DbIndex,
    query: &str,
    case_sensitive: bool,
    cancel_token: &CancellationToken,
    candidates: &mut Vec<DocCandidate>,
) -> Option<()> {
    for type_decl in db.get_type_index().get_all_types() {
        if cancel_token.is_cancelled() {
            return None;
        }

        let full_name = type_decl.get_full_name();
        let Some(rank) = match_rank(full_name, query, case_sensitive) else {
            continue;
        };

        candidates.push(DocCandidate {
            name: type_decl.get_id().get_simple_name().to_string(),
            full_name: full_name.to_string(),
            rank,
            symbol: DocSymbol::Type(type_decl.get_id()),
        });
    }

    Some(())
}

fn build_doc_item(db: &DbIndex, candidate: DocCandidate) -> Option<GluaDocItem> {
    match candidate.symbol {
        DocSymbol::Global(decl_id) => {
            db.get_decl_index().get_decl(&decl_id)?;
            let typ = db
                .get_type_index()
                .get_type_cache(&decl_id.into())
                .map(|cache| cache.as_type())
                .unwrap_or(&LuaType::Unknown);
            let owner = LuaSemanticDeclId::LuaDecl(decl_id);
            let fallback = humanize_type(db, typ, RenderLevel::Detailed);
            let documentation = build_documentation_markdown(db, &owner, Some(typ), &fallback);

            Some(GluaDocItem {
                name: candidate.name,
                full_name: candidate.full_name,
                kind: global_kind(typ).to_string(),
                documentation,
                deprecated: is_deprecated(db, &owner),
            })
        }
        DocSymbol::Type(type_decl_id) => {
            let type_decl = db.get_type_index().get_type_decl(&type_decl_id)?;
            let owner = LuaSemanticDeclId::TypeDecl(type_decl_id.clone());
            let alias_origin = type_decl.get_alias_origin(db, None);
            let fallback_type = alias_origin
                .as_ref()
                .cloned()
                .unwrap_or_else(|| LuaType::Ref(type_decl_id.clone()));
            let fallback = humanize_type(db, &fallback_type, RenderLevel::Detailed);
            let documentation =
                build_documentation_markdown(db, &owner, alias_origin.as_ref(), &fallback);

            Some(GluaDocItem {
                name: candidate.name,
                full_name: candidate.full_name,
                kind: type_decl_kind(type_decl_kind_value(type_decl)).to_string(),
                documentation,
                deprecated: is_deprecated(db, &owner),
            })
        }
    }
}

fn type_decl_kind_value(type_decl: &emmylua_code_analysis::LuaTypeDecl) -> LuaDeclTypeKind {
    if type_decl.is_class() {
        LuaDeclTypeKind::Class
    } else if type_decl.is_enum() {
        LuaDeclTypeKind::Enum
    } else if type_decl.is_alias() {
        LuaDeclTypeKind::Alias
    } else {
        LuaDeclTypeKind::Attribute
    }
}

fn type_decl_kind(kind: LuaDeclTypeKind) -> &'static str {
    match kind {
        LuaDeclTypeKind::Class => "class",
        LuaDeclTypeKind::Enum => "enum",
        LuaDeclTypeKind::Alias => "alias",
        LuaDeclTypeKind::Attribute => "variable",
    }
}

fn global_kind(typ: &LuaType) -> &'static str {
    if typ.is_function() {
        "function"
    } else if typ.is_const() {
        "constant"
    } else {
        "variable"
    }
}

fn is_deprecated(db: &DbIndex, owner: &LuaSemanticDeclId) -> bool {
    db.get_property_index()
        .get_property(owner)
        .is_some_and(|property| property.deprecated().is_some())
}

fn build_documentation_markdown(
    db: &DbIndex,
    owner: &LuaSemanticDeclId,
    signature_source: Option<&LuaType>,
    fallback: &str,
) -> String {
    let mut sections = Vec::new();

    if let Some(property) = db.get_property_index().get_property(owner) {
        if let Some(description) = property.description()
            && !description.trim().is_empty()
        {
            sections.push(description.trim().to_string());
        }

        if let Some(tag_content) = property.tag_content() {
            let tags = tag_content
                .get_all_tags()
                .iter()
                .map(|(tag_name, value)| format!("@*{}* {}", tag_name, value))
                .collect::<Vec<_>>()
                .join("\n\n");
            if !tags.trim().is_empty() {
                sections.push(tags);
            }
        }
    }

    if let Some(source_type) = signature_source {
        let mut signature_ids = Vec::new();
        collect_signature_ids(source_type, &mut signature_ids);
        for signature_id in signature_ids {
            if let Some(signature_markdown) = build_signature_markdown(db, &signature_id)
                && !signature_markdown.trim().is_empty()
            {
                sections.push(signature_markdown);
            }
        }
    }

    let documentation = sections.join("\n\n").trim().to_string();
    if documentation.is_empty() {
        fallback.to_string()
    } else {
        documentation
    }
}

fn build_signature_markdown(db: &DbIndex, signature_id: &LuaSignatureId) -> Option<String> {
    let signature = db.get_signature_index().get(signature_id)?;
    let mut markdown = String::new();

    for idx in 0..signature.params.len() {
        let Some(param_info) = signature.get_param_info_by_id(idx) else {
            continue;
        };

        let Some(description) = &param_info.description else {
            continue;
        };

        markdown.push_str(&format!(
            "@*param* `{}` — {}\n\n",
            param_info.name, description
        ));
    }

    for return_info in &signature.return_docs {
        let Some(description) = &return_info.description else {
            continue;
        };

        let name_prefix = return_info
            .name
            .as_ref()
            .filter(|name| !name.is_empty())
            .map(|name| format!("`{}` ", name))
            .unwrap_or_default();

        markdown.push_str(&format!("@*return* {}— {}\n\n", name_prefix, description));
    }

    let markdown = markdown.trim().to_string();
    if markdown.is_empty() {
        None
    } else {
        Some(markdown)
    }
}

fn collect_signature_ids(typ: &LuaType, signature_ids: &mut Vec<LuaSignatureId>) {
    match typ {
        LuaType::Signature(signature_id) => {
            if !signature_ids.contains(signature_id) {
                signature_ids.push(*signature_id);
            }
        }
        LuaType::Union(union) => {
            for inner in union.into_vec() {
                collect_signature_ids(&inner, signature_ids);
            }
        }
        _ => {}
    }
}

fn match_rank(text: &str, query: &str, case_sensitive: bool) -> Option<MatchRank> {
    if case_sensitive {
        if text == query {
            Some(MatchRank::Exact)
        } else if text.starts_with(query) {
            Some(MatchRank::Prefix)
        } else if text.contains(query) {
            Some(MatchRank::Contains)
        } else {
            None
        }
    } else {
        let lower_text = text.to_ascii_lowercase();
        let lower_query = query.to_ascii_lowercase();
        if lower_text == lower_query {
            Some(MatchRank::Exact)
        } else if lower_text.starts_with(&lower_query) {
            Some(MatchRank::Prefix)
        } else if lower_text.contains(&lower_query) {
            Some(MatchRank::Contains)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use emmylua_code_analysis::VirtualWorkspace;
    use googletest::prelude::*;
    use tokio_util::sync::CancellationToken;

    use super::build_doc_search;

    #[gtest]
    fn build_doc_search_returns_markdown_for_function_docs() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/doc_search.lua",
            r#"
            ---Legacy API.
            ---@param value string User facing value.
            ---@return number total The computed total.
            ---@deprecated Use NewApi instead.
            function OldApi(value)
                return 1
            end
        "#,
        );

        let items =
            build_doc_search(ws.get_db_mut(), "OldApi", 20, &CancellationToken::new()).or_fail()?;
        let old_api_item = items.iter().find(|item| item.name == "OldApi").or_fail()?;

        verify_that!(old_api_item.kind.as_str(), eq("function"))?;
        verify_that!(old_api_item.deprecated, eq(true))?;
        verify_that!(
            old_api_item
                .documentation
                .contains("@*param* `value` — User facing value."),
            eq(true)
        )?;
        verify_that!(
            old_api_item
                .documentation
                .contains("@*return* `total` — The computed total."),
            eq(true)
        )
    }
}
