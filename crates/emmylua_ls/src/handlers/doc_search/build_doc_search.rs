use emmylua_code_analysis::{
    DbIndex, GlobalId, LuaDeclId, LuaDeclTypeKind, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaMemberOwner, LuaSemanticDeclId, LuaSignatureId, LuaType, LuaTypeDeclId, RenderLevel,
    humanize_type,
};
use std::collections::HashSet;
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
    Member(LuaMemberId, LuaTypeDeclId),
    GlobalPathMember(LuaMemberId, String),
}

#[derive(Debug, Clone)]
struct DocCandidate {
    name: String,
    full_name: String,
    rank: MatchRank,
    symbol: DocSymbol,
}

#[derive(Debug, Clone, Copy)]
struct QualifiedMemberQuery<'a> {
    owner_query: &'a str,
    member_query: &'a str,
    separator: char,
}

#[derive(Debug, Clone, Copy)]
enum SearchQueryMode<'a> {
    Qualified(QualifiedMemberQuery<'a>),
    Unqualified(&'a str),
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
    match resolve_query_mode(query) {
        SearchQueryMode::Qualified(qualified_query) => {
            collect_qualified_member_candidates(
                db,
                qualified_query,
                case_sensitive,
                cancel_token,
                &mut candidates,
            )?;
        }
        SearchQueryMode::Unqualified(unqualified_query) => {
            collect_global_candidates(
                db,
                unqualified_query,
                case_sensitive,
                cancel_token,
                &mut candidates,
            )?;
            collect_type_candidates(
                db,
                unqualified_query,
                case_sensitive,
                cancel_token,
                &mut candidates,
            )?;
            collect_unqualified_member_candidates(
                db,
                unqualified_query,
                case_sensitive,
                cancel_token,
                &mut candidates,
            )?;
        }
    }

    candidates.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.full_name.cmp(&right.full_name))
    });

    let mut seen_full_names = HashSet::new();
    candidates.retain(|candidate| seen_full_names.insert(candidate.full_name.clone()));

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

fn resolve_query_mode(query: &str) -> SearchQueryMode<'_> {
    let Some((separator_idx, separator)) = find_member_separator(query) else {
        return SearchQueryMode::Unqualified(query);
    };

    let owner_query = query[..separator_idx].trim();
    let member_query = query[separator_idx + separator.len_utf8()..].trim();
    if owner_query.is_empty() || member_query.is_empty() {
        let fallback_query = if !member_query.is_empty() {
            member_query
        } else if !owner_query.is_empty() {
            owner_query
        } else {
            query
        };
        return SearchQueryMode::Unqualified(fallback_query);
    }

    SearchQueryMode::Qualified(QualifiedMemberQuery {
        owner_query,
        member_query,
        separator,
    })
}

fn find_member_separator(query: &str) -> Option<(usize, char)> {
    match (query.find(':'), query.find('.')) {
        (Some(colon_idx), Some(dot_idx)) => {
            if colon_idx <= dot_idx {
                Some((colon_idx, ':'))
            } else {
                Some((dot_idx, '.'))
            }
        }
        (Some(colon_idx), None) => Some((colon_idx, ':')),
        (None, Some(dot_idx)) => Some((dot_idx, '.')),
        (None, None) => None,
    }
}

fn collect_qualified_member_candidates(
    db: &DbIndex,
    query: QualifiedMemberQuery,
    case_sensitive: bool,
    cancel_token: &CancellationToken,
    candidates: &mut Vec<DocCandidate>,
) -> Option<()> {
    collect_qualified_type_member_candidates(db, query, case_sensitive, cancel_token, candidates)?;

    if query.separator == '.' {
        collect_qualified_global_decl_member_candidates(
            db,
            query,
            case_sensitive,
            cancel_token,
            candidates,
        )?;

        collect_qualified_global_path_member_candidates(
            db,
            query,
            case_sensitive,
            cancel_token,
            candidates,
        )?;
    }

    Some(())
}

fn collect_qualified_global_decl_member_candidates(
    db: &DbIndex,
    query: QualifiedMemberQuery,
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

        let Some(owner_rank) = match_rank(decl.get_name(), query.owner_query, case_sensitive)
        else {
            continue;
        };

        let Some(owner) = resolve_decl_member_owner(db, decl_id) else {
            continue;
        };

        let Some(members) = db.get_member_index().get_members(&owner) else {
            continue;
        };

        for member in members {
            let LuaMemberKey::Name(member_name) = member.get_key() else {
                continue;
            };

            let Some(member_rank) = match_rank(member_name, query.member_query, case_sensitive)
            else {
                continue;
            };

            let rank = owner_rank.max(member_rank);
            candidates.push(DocCandidate {
                name: member_name.to_string(),
                full_name: format!("{}{}{}", decl.get_name(), query.separator, member_name),
                rank,
                symbol: DocSymbol::GlobalPathMember(member.get_id(), decl.get_name().to_string()),
            });
        }
    }

    Some(())
}

fn resolve_decl_member_owner(db: &DbIndex, decl_id: LuaDeclId) -> Option<LuaMemberOwner> {
    let typ = db.get_type_index().get_type_cache(&decl_id.into())?.as_type();
    match typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => Some(LuaMemberOwner::Type(type_id.clone())),
        LuaType::TableConst(id) => Some(LuaMemberOwner::Element(id.clone())),
        LuaType::Instance(inst) => Some(LuaMemberOwner::Element(inst.get_range().clone())),
        _ => None,
    }
}

fn collect_qualified_type_member_candidates(
    db: &DbIndex,
    query: QualifiedMemberQuery,
    case_sensitive: bool,
    cancel_token: &CancellationToken,
    candidates: &mut Vec<DocCandidate>,
) -> Option<()> {
    for type_decl in db.get_type_index().get_all_types() {
        if cancel_token.is_cancelled() {
            return None;
        }

        let Some(owner_rank) = match_type_decl_rank(type_decl, query.owner_query, case_sensitive)
        else {
            continue;
        };

        let owner = LuaMemberOwner::Type(type_decl.get_id());
        let Some(members) = db.get_member_index().get_members(&owner) else {
            continue;
        };

        for member in members {
            let LuaMemberKey::Name(member_name) = member.get_key() else {
                continue;
            };

            let Some(member_rank) = match_rank(member_name, query.member_query, case_sensitive)
            else {
                continue;
            };

            let rank = owner_rank.max(member_rank);
            candidates.push(DocCandidate {
                name: member_name.to_string(),
                full_name: format!(
                    "{}{}{}",
                    type_decl.get_full_name(),
                    query.separator,
                    member_name
                ),
                rank,
                symbol: DocSymbol::Member(member.get_id(), type_decl.get_id()),
            });
        }
    }

    Some(())
}

fn collect_qualified_global_path_member_candidates(
    db: &DbIndex,
    query: QualifiedMemberQuery,
    case_sensitive: bool,
    cancel_token: &CancellationToken,
    candidates: &mut Vec<DocCandidate>,
) -> Option<()> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let owner = LuaMemberOwner::GlobalPath(GlobalId::new(query.owner_query));
    let Some(members) = db.get_member_index().get_members(&owner) else {
        return Some(());
    };

    for member in members {
        let LuaMemberKey::Name(member_name) = member.get_key() else {
            continue;
        };

        let Some(rank) = match_rank(member_name, query.member_query, case_sensitive) else {
            continue;
        };

        candidates.push(DocCandidate {
            name: member_name.to_string(),
            full_name: format!("{}{}{}", query.owner_query, query.separator, member_name),
            rank,
            symbol: DocSymbol::GlobalPathMember(member.get_id(), query.owner_query.to_string()),
        });
    }

    Some(())
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

fn collect_unqualified_member_candidates(
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

        if !type_decl.is_class() {
            continue;
        }

        let owner = LuaMemberOwner::Type(type_decl.get_id());
        let Some(members) = db.get_member_index().get_members(&owner) else {
            continue;
        };

        for member in members {
            let LuaMemberKey::Name(member_name) = member.get_key() else {
                continue;
            };

            let Some(rank) = match_rank(member_name, query, case_sensitive) else {
                continue;
            };

            let separator = member_separator(member.get_feature());
            candidates.push(DocCandidate {
                name: member_name.to_string(),
                full_name: format!("{}{}{}", type_decl.get_full_name(), separator, member_name),
                rank,
                symbol: DocSymbol::Member(member.get_id(), type_decl.get_id()),
            });
        }
    }

    Some(())
}

fn match_type_decl_rank(
    type_decl: &emmylua_code_analysis::LuaTypeDecl,
    query: &str,
    case_sensitive: bool,
) -> Option<MatchRank> {
    let full_rank = match_rank(type_decl.get_full_name(), query, case_sensitive);
    let simple_rank = match_rank(type_decl.get_id().get_simple_name(), query, case_sensitive);
    match (full_rank, simple_rank) {
        (Some(full_rank), Some(simple_rank)) => Some(full_rank.min(simple_rank)),
        (Some(rank), None) | (None, Some(rank)) => Some(rank),
        (None, None) => None,
    }
}

fn build_doc_item(db: &DbIndex, candidate: DocCandidate) -> Option<GluaDocItem> {
    let DocCandidate {
        name,
        full_name,
        symbol,
        ..
    } = candidate;

    match symbol {
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
                name,
                full_name,
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
                name,
                full_name,
                kind: type_decl_kind(type_decl_kind_value(type_decl)).to_string(),
                documentation,
                deprecated: is_deprecated(db, &owner),
            })
        }
        DocSymbol::Member(member_id, type_decl_id) => {
            db.get_type_index().get_type_decl(&type_decl_id)?;
            let member = db.get_member_index().get_member(&member_id)?;
            let typ = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .map(|cache| cache.as_type())
                .unwrap_or(&LuaType::Unknown);
            let owner = LuaSemanticDeclId::Member(member_id);
            let fallback = humanize_type(db, typ, RenderLevel::Detailed);
            let documentation = build_documentation_markdown(db, &owner, Some(typ), &fallback);
            let separator = display_member_separator(&full_name, member_separator(member.get_feature()));

            Some(GluaDocItem {
                name,
                full_name,
                kind: member_kind(typ, separator).to_string(),
                documentation,
                deprecated: is_deprecated(db, &owner),
            })
        }
        DocSymbol::GlobalPathMember(member_id, owner_path) => {
            let member = db.get_member_index().get_member(&member_id)?;
            let typ = db
                .get_type_index()
                .get_type_cache(&member_id.into())
                .map(|cache| cache.as_type())
                .unwrap_or(&LuaType::Unknown);
            let owner = LuaSemanticDeclId::Member(member_id);
            let fallback = if full_name.is_empty() {
                format!("{}{}{}", owner_path, member_separator(member.get_feature()), name)
            } else {
                humanize_type(db, typ, RenderLevel::Detailed)
            };
            let documentation = build_documentation_markdown(db, &owner, Some(typ), &fallback);
            let separator = display_member_separator(&full_name, member_separator(member.get_feature()));

            Some(GluaDocItem {
                name,
                full_name,
                kind: member_kind(typ, separator).to_string(),
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

fn member_kind(typ: &LuaType, separator: &str) -> &'static str {
    if typ.is_function() {
        if separator == ":" {
            "method"
        } else {
            "function"
        }
    } else if typ.is_const() {
        "constant"
    } else {
        "field"
    }
}

fn member_separator(feature: LuaMemberFeature) -> &'static str {
    match feature {
        LuaMemberFeature::FileMethodDecl | LuaMemberFeature::MetaMethodDecl => ":",
        _ => ".",
    }
}

fn display_member_separator<'a>(full_name: &str, fallback: &'a str) -> &'a str {
    match (full_name.rfind(':'), full_name.rfind('.')) {
        (Some(colon_idx), Some(dot_idx)) => {
            if colon_idx > dot_idx {
                ":"
            } else {
                "."
            }
        }
        (Some(_), None) => ":",
        (None, Some(_)) => ".",
        (None, None) => fallback,
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

    fn create_member_workspace() -> VirtualWorkspace {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/member_doc_search.lua",
            r#"
            ---@class Entity
            local Entity = {}

            ---@return number
            function Entity:GetPos()
                return 1
            end

            hook = hook or {}

            ---@param event string
            ---@param identifier string
            ---@param callback function
            function hook.Add(event, identifier, callback)
            end
        "#,
        );

        ws
    }

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

    #[gtest]
    fn build_doc_search_supports_qualified_class_member_query() -> Result<()> {
        let mut ws = create_member_workspace();

        let items =
            build_doc_search(ws.get_db_mut(), "Entity:GetPos", 20, &CancellationToken::new())
                .or_fail()?;

        let item = items
            .iter()
            .find(|item| item.full_name == "Entity:GetPos")
            .or_fail()?;
        verify_that!(item.kind.as_str(), eq("method"))
    }

    #[gtest]
    fn build_doc_search_supports_unqualified_class_member_query() -> Result<()> {
        let mut ws = create_member_workspace();

        let items = build_doc_search(ws.get_db_mut(), "GetPos", 20, &CancellationToken::new())
            .or_fail()?;

        verify_that!(
            items.iter().any(|item| item.full_name == "Entity:GetPos"),
            eq(true)
        )
    }

    #[gtest]
    fn build_doc_search_supports_global_path_member_query() -> Result<()> {
        let mut ws = create_member_workspace();

        let items = build_doc_search(ws.get_db_mut(), "hook.Add", 20, &CancellationToken::new())
            .or_fail()?;

        let item = items
            .iter()
            .find(|item| item.full_name == "hook.Add")
            .or_fail()?;
        verify_that!(item.kind.as_str(), eq("function"))
    }

    #[gtest]
    fn build_doc_search_falls_back_to_unqualified_when_qualified_parts_are_empty() -> Result<()> {
        let mut ws = create_member_workspace();

        let right_only =
            build_doc_search(ws.get_db_mut(), ":GetPos", 20, &CancellationToken::new())
                .or_fail()?;
        verify_that!(
            right_only
                .iter()
                .any(|item| item.full_name == "Entity:GetPos"),
            eq(true)
        )?;

        let left_only =
            build_doc_search(ws.get_db_mut(), "Entity:", 20, &CancellationToken::new())
                .or_fail()?;
        verify_that!(left_only.iter().any(|item| item.full_name == "Entity"), eq(true))
    }

    #[gtest]
    fn build_doc_search_returns_no_results_for_unknown_qualified_member_query() -> Result<()> {
        let mut ws = create_member_workspace();

        let items = build_doc_search(
            ws.get_db_mut(),
            "UnknownType:GetPos",
            20,
            &CancellationToken::new(),
        )
        .or_fail()?;
        verify_that!(items, is_empty())
    }
}
