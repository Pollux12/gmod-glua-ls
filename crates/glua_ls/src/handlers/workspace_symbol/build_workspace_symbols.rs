use glua_code_analysis::{DbIndex, LuaCompilation, LuaSemanticDeclId, LuaType};
use lsp_types::{OneOf, SymbolKind, SymbolTag, WorkspaceSymbol, WorkspaceSymbolResponse};
use tokio_util::sync::CancellationToken;

/// A workspace symbol query: matched case-sensitively when it holds an
/// uppercase letter, otherwise ignoring case.
struct SymbolQuery {
    text: String,
    case_sensitive: bool,
}

impl SymbolQuery {
    fn new(query: &str) -> Self {
        let case_sensitive = query.chars().any(char::is_uppercase);
        Self {
            text: if case_sensitive {
                query.to_owned()
            } else {
                query.to_lowercase()
            },
            case_sensitive,
        }
    }

    fn matches(&self, name: &str) -> bool {
        if self.case_sensitive {
            return name.contains(&self.text);
        }
        if name.is_ascii() && self.text.is_ascii() {
            let text = self.text.as_bytes();
            return text.is_empty()
                || name
                    .as_bytes()
                    .windows(text.len())
                    .any(|window| window.eq_ignore_ascii_case(text));
        }
        name.to_lowercase().contains(&self.text)
    }
}

pub fn build_workspace_symbols(
    compilation: &LuaCompilation,
    query: String,
    cancel_token: CancellationToken,
) -> Option<WorkspaceSymbolResponse> {
    let query = SymbolQuery::new(&query);
    let mut symbols = Vec::new();
    add_global_variable_symbols(&mut symbols, compilation, &query, &cancel_token)?;
    add_type_symbols(&mut symbols, compilation, &query, &cancel_token)?;
    Some(WorkspaceSymbolResponse::Nested(symbols))
}

fn add_global_variable_symbols(
    symbols: &mut Vec<WorkspaceSymbol>,
    compilation: &LuaCompilation,
    query: &SymbolQuery,
    cancel_token: &CancellationToken,
) -> Option<()> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let db = compilation.get_db();
    let global_decl_ids = db
        .get_global_index()
        .get_global_decl_ids_matching(|name| query.matches(name));
    for decl_id in global_decl_ids {
        if cancel_token.is_cancelled() {
            return None;
        }
        let Some(decl) = db.get_decl_index().get_decl(&decl_id) else {
            continue;
        };
        let Some(location) = db
            .get_vfs()
            .get_document(&decl.get_file_id())
            .and_then(|document| document.to_lsp_location(decl.get_range()))
        else {
            continue;
        };
        let typ = db
            .get_type_index()
            .get_type_cache(&decl_id.into())
            .map(|cache| cache.as_type())
            .unwrap_or(&LuaType::Unknown);
        symbols.push(WorkspaceSymbol {
            name: decl.get_name().to_string(),
            kind: get_symbol_kind(typ),
            tags: is_deprecated(db, LuaSemanticDeclId::LuaDecl(decl_id))
                .then(|| vec![SymbolTag::DEPRECATED]),
            container_name: None,
            location: OneOf::Left(location),
            data: None,
        });
    }

    Some(())
}

fn add_type_symbols(
    symbols: &mut Vec<WorkspaceSymbol>,
    compilation: &LuaCompilation,
    query: &SymbolQuery,
    cancel_token: &CancellationToken,
) -> Option<()> {
    if cancel_token.is_cancelled() {
        return None;
    }

    let db = compilation.get_db();
    let mut types = db
        .get_type_index()
        .iter_type_decls()
        .filter(|typ| query.matches(typ.get_full_name()))
        .collect::<Vec<_>>();
    types.sort_unstable_by(|left, right| left.get_id().stable_cmp(&right.get_id()));
    for typ in types {
        if cancel_token.is_cancelled() {
            return None;
        }
        let Some(location) = typ.get_locations().first().and_then(|location| {
            db.get_vfs()
                .get_document(&location.file_id)?
                .to_lsp_location(location.range)
        }) else {
            continue;
        };
        symbols.push(WorkspaceSymbol {
            name: typ.get_full_name().to_string(),
            kind: SymbolKind::CLASS,
            tags: is_deprecated(db, LuaSemanticDeclId::TypeDecl(typ.get_id()))
                .then(|| vec![SymbolTag::DEPRECATED]),
            container_name: None,
            location: OneOf::Left(location),
            data: None,
        });
    }

    Some(())
}

fn get_symbol_kind(typ: &LuaType) -> SymbolKind {
    if typ.is_function() {
        return SymbolKind::FUNCTION;
    } else if typ.is_const() {
        return SymbolKind::CONSTANT;
    } else if typ.is_def() {
        return SymbolKind::CLASS;
    }

    SymbolKind::VARIABLE
}

fn is_deprecated(db: &DbIndex, id: LuaSemanticDeclId) -> bool {
    let property = db.get_property_index().get_property(&id);
    property.is_some_and(|prop| prop.deprecated().is_some())
}

#[cfg(test)]
mod tests {
    use super::SymbolQuery;

    /// A lowercase query ignores case; one with an uppercase letter keeps it.
    #[test]
    fn a_query_ignores_case_unless_it_holds_an_uppercase_letter() {
        let lower = SymbolQuery::new("getpos");
        let upper = SymbolQuery::new("GetPos");
        assert_eq!(
            (
                lower.matches("Entity.GetPos"),
                lower.matches("Ã‰ntityGetPos"),
                upper.matches("Entity.getpos"),
                upper.matches("Entity.GetPos"),
                SymbolQuery::new("").matches("anything"),
            ),
            (true, true, false, true, true)
        );
    }
}
