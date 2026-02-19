use std::{cmp, collections::HashSet};

use emmylua_code_analysis::{
    DbIndex, GlobalId, LuaDeclId, LuaDeclTypeKind, LuaMemberFeature, LuaMemberId, LuaMemberKey,
    LuaMemberOwner, LuaSemanticDeclId, LuaSignatureId, LuaType, LuaTypeDeclId, RenderLevel,
    humanize_type,
};
use tokio_util::sync::CancellationToken;

use super::doc_search_request::GluaDocItem;

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
    rank: u32,
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
            .reverse()
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.full_name.cmp(&right.full_name))
    });

    let mut seen_full_names = HashSet::new();
    candidates.retain(|candidate| seen_full_names.insert(candidate.full_name.clone()));

    let mut items = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        if cancel_token.is_cancelled() {
            return None;
        }

        if let Some(item) = build_doc_item(db, candidate) {
            items.push(item);
        }
    }

    let max_constants = cmp::min(5, capped_limit / 3);
    apply_diversity_filter(&mut items, max_constants);
    items.truncate(capped_limit);

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

            let typ = db
                .get_type_index()
                .get_type_cache(&member.get_id().into())
                .map(|cache| cache.as_type())
                .unwrap_or(&LuaType::Unknown);
            let kind = member_kind(typ, ".");
            let semantic_id = LuaSemanticDeclId::Member(member.get_id());
            let description = get_description_text(db, &semantic_id);
            let rank = score_candidate(
                owner_rank.max(member_rank),
                kind,
                description.as_deref(),
                query.member_query,
                case_sensitive,
            );

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

            let typ = db
                .get_type_index()
                .get_type_cache(&member.get_id().into())
                .map(|cache| cache.as_type())
                .unwrap_or(&LuaType::Unknown);
            let separator = if query.separator == ':' { ":" } else { "." };
            let kind = member_kind(typ, separator);
            let semantic_id = LuaSemanticDeclId::Member(member.get_id());
            let description = get_description_text(db, &semantic_id);
            let rank = score_candidate(
                owner_rank.max(member_rank),
                kind,
                description.as_deref(),
                query.member_query,
                case_sensitive,
            );

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

        let typ = db
            .get_type_index()
            .get_type_cache(&member.get_id().into())
            .map(|cache| cache.as_type())
            .unwrap_or(&LuaType::Unknown);
        let kind = member_kind(typ, ".");
        let semantic_id = LuaSemanticDeclId::Member(member.get_id());
        let description = get_description_text(db, &semantic_id);
        let rank = score_candidate(
            rank,
            kind,
            description.as_deref(),
            query.member_query,
            case_sensitive,
        );

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

        let typ = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|cache| cache.as_type())
            .unwrap_or(&LuaType::Unknown);
        let kind = global_kind(typ);
        let semantic_id = LuaSemanticDeclId::LuaDecl(decl_id);
        let description = get_description_text(db, &semantic_id);
        let rank = score_candidate(rank, kind, description.as_deref(), query, case_sensitive);

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

        let kind = type_decl_kind(type_decl_kind_value(type_decl));
        let semantic_id = LuaSemanticDeclId::TypeDecl(type_decl.get_id());
        let description = get_description_text(db, &semantic_id);
        let rank = score_candidate(rank, kind, description.as_deref(), query, case_sensitive);

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
            let typ = db
                .get_type_index()
                .get_type_cache(&member.get_id().into())
                .map(|cache| cache.as_type())
                .unwrap_or(&LuaType::Unknown);
            let kind = member_kind(typ, separator);
            let semantic_id = LuaSemanticDeclId::Member(member.get_id());
            let description = get_description_text(db, &semantic_id);
            let rank = score_candidate(rank, kind, description.as_deref(), query, case_sensitive);

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
) -> Option<u32> {
    let full_rank = match_rank(type_decl.get_full_name(), query, case_sensitive);
    let simple_rank = match_rank(type_decl.get_id().get_simple_name(), query, case_sensitive);
    match (full_rank, simple_rank) {
        (Some(full_rank), Some(simple_rank)) => Some(full_rank.max(simple_rank)),
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

const MATCH_SCORE_EXACT: u32 = 1000;
const MATCH_SCORE_PREFIX: u32 = 600;
const MATCH_SCORE_WORD_BOUNDARY: u32 = 400;
const MATCH_SCORE_CONTAINS: u32 = 200;

const DESCRIPTION_BONUS_CONTAINS: u32 = 150;
const DESCRIPTION_BONUS_FIRST_SENTENCE_START: u32 = 250;

fn score_candidate(
    base_name_score: u32,
    kind: &str,
    description: Option<&str>,
    query: &str,
    case_sensitive: bool,
) -> u32 {
    let name_score = (base_name_score as f32 * kind_score_multiplier(kind)) as u32;
    name_score + description_bonus(description, query, case_sensitive)
}

fn kind_score_multiplier(kind: &str) -> f32 {
    match kind {
        "function" | "method" => 1.0,
        "class" => 0.95,
        "alias" => 0.85,
        "constant" | "field" => 0.65,
        "variable" => 0.80,
        _ => 0.75,
    }
}

fn description_bonus(description: Option<&str>, query: &str, case_sensitive: bool) -> u32 {
    let Some(description) = description else {
        return 0;
    };

    let trimmed = description.trim();
    if trimmed.is_empty() {
        return 0;
    }

    if case_sensitive {
        if first_sentence(trimmed).trim_start().starts_with(query) {
            DESCRIPTION_BONUS_FIRST_SENTENCE_START
        } else if trimmed.contains(query) {
            DESCRIPTION_BONUS_CONTAINS
        } else {
            0
        }
    } else {
        let lower_description = trimmed.to_ascii_lowercase();
        let lower_query = query.to_ascii_lowercase();
        if first_sentence(&lower_description)
            .trim_start()
            .starts_with(&lower_query)
        {
            DESCRIPTION_BONUS_FIRST_SENTENCE_START
        } else if lower_description.contains(&lower_query) {
            DESCRIPTION_BONUS_CONTAINS
        } else {
            0
        }
    }
}

fn first_sentence(text: &str) -> &str {
    text.split_terminator(['.', '!', '?', '\n'])
        .next()
        .unwrap_or(text)
}

fn get_description_text(db: &DbIndex, semantic_id: &LuaSemanticDeclId) -> Option<String> {
    db.get_property_index()
        .get_property(semantic_id)
        .and_then(|property| property.description())
        .map(|description| description.trim().to_string())
        .filter(|description| !description.is_empty())
}

fn apply_diversity_filter(items: &mut Vec<GluaDocItem>, max_constants: usize) {
    let mut constant_count = 0usize;
    items.retain(|item| {
        if item.kind == "constant" || item.kind == "field" {
            constant_count += 1;
            constant_count <= max_constants
        } else {
            true
        }
    });
}

fn match_rank(text: &str, query: &str, case_sensitive: bool) -> Option<u32> {
    if case_sensitive {
        if text == query {
            return Some(MATCH_SCORE_EXACT);
        }

        if text.starts_with(query) {
            return Some(MATCH_SCORE_PREFIX);
        }

        if has_word_boundary_match(text, text, query) {
            return Some(MATCH_SCORE_WORD_BOUNDARY);
        }

        if text.contains(query) {
            return Some(MATCH_SCORE_CONTAINS);
        }

        None
    } else {
        let lower_text = text.to_ascii_lowercase();
        let lower_query = query.to_ascii_lowercase();

        if lower_text == lower_query {
            return Some(MATCH_SCORE_EXACT);
        }

        if lower_text.starts_with(&lower_query) {
            return Some(MATCH_SCORE_PREFIX);
        }

        if has_word_boundary_match(text, &lower_text, &lower_query) {
            return Some(MATCH_SCORE_WORD_BOUNDARY);
        }

        if lower_text.contains(&lower_query) {
            return Some(MATCH_SCORE_CONTAINS);
        }

        None
    }
}

fn has_word_boundary_match(original_text: &str, search_text: &str, query: &str) -> bool {
    search_text
        .match_indices(query)
        .any(|(idx, _)| idx > 0 && is_word_boundary(original_text, idx))
}

fn is_word_boundary(text: &str, idx: usize) -> bool {
    if idx == 0 || !text.is_char_boundary(idx) {
        return idx == 0;
    }

    let prev_char = text[..idx].chars().next_back();
    let current_char = text[idx..].chars().next();

    match (prev_char, current_char) {
        (Some('_' | ':' | '.'), _) => true,
        (Some(prev), Some(current)) => prev.is_ascii_lowercase() && current.is_ascii_uppercase(),
        _ => false,
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

    #[gtest]
    fn build_doc_search_ranks_exact_prefix_boundary_and_contains() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/doc_search_rank.lua",
            r#"
            physics = 1
            physicsEngine = 1
            entity_physics = 1
            myphysicsvalue = 1
        "#,
        );

        let items =
            build_doc_search(ws.get_db_mut(), "physics", 20, &CancellationToken::new()).or_fail()?;

        let exact_idx = items.iter().position(|item| item.name == "physics").or_fail()?;
        let prefix_idx = items
            .iter()
            .position(|item| item.name == "physicsEngine")
            .or_fail()?;
        let boundary_idx = items
            .iter()
            .position(|item| item.name == "entity_physics")
            .or_fail()?;
        let contains_idx = items
            .iter()
            .position(|item| item.name == "myphysicsvalue")
            .or_fail()?;

        verify_that!(exact_idx < prefix_idx, eq(true))?;
        verify_that!(prefix_idx < boundary_idx, eq(true))?;
        verify_that!(boundary_idx < contains_idx, eq(true))
    }

    #[gtest]
    fn build_doc_search_prioritizes_description_bonus_for_matching_names() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/doc_search_description_bonus.lua",
            r#"
            function physicsAlpha()
            end

            ---physics helpers for simulation.
            function physicsBeta()
            end
        "#,
        );

        let items =
            build_doc_search(ws.get_db_mut(), "physics", 20, &CancellationToken::new()).or_fail()?;

        let alpha_idx = items
            .iter()
            .position(|item| item.name == "physicsAlpha")
            .or_fail()?;
        let beta_idx = items
            .iter()
            .position(|item| item.name == "physicsBeta")
            .or_fail()?;

        verify_that!(beta_idx < alpha_idx, eq(true))
    }

    #[gtest]
    fn build_doc_search_limits_constant_and_field_results_for_diversity() -> Result<()> {
        let mut ws = VirtualWorkspace::new();
        ws.def_file(
            "lua/autorun/doc_search_diversity.lua",
            r#"
            ---@class PhysicsContainer
            local PhysicsContainer = {}

            PhysicsContainer.physicsField01 = UNKNOWN_ONE
            PhysicsContainer.physicsField02 = UNKNOWN_TWO
            PhysicsContainer.physicsField03 = UNKNOWN_THREE
            PhysicsContainer.physicsField04 = UNKNOWN_FOUR
            PhysicsContainer.physicsField05 = UNKNOWN_FIVE
            PhysicsContainer.physicsField06 = UNKNOWN_SIX
            PhysicsContainer.physicsField07 = UNKNOWN_SEVEN
            PhysicsContainer.physicsField08 = UNKNOWN_EIGHT

            function PhysicsContainer:physicsRun()
            end

            function physicsGlobal()
            end
        "#,
        );

        let items =
            build_doc_search(ws.get_db_mut(), "physics", 20, &CancellationToken::new()).or_fail()?;

        let constant_or_field_count = items
            .iter()
            .filter(|item| item.kind == "constant" || item.kind == "field")
            .count();
        verify_that!(constant_or_field_count <= 5, eq(true))?;
        verify_that!(items.iter().any(|item| item.kind == "method"), eq(true))?;
        verify_that!(items.iter().any(|item| item.kind == "class"), eq(true))
    }
}
