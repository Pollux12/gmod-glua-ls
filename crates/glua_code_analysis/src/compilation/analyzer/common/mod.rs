mod migrate_global_member;
use glua_parser::{LuaAstNode, LuaAstToken, LuaExpr, LuaForRangeStat};
pub(super) use migrate_global_member::{
    migrate_global_members_when_type_resolve, migrate_global_path_members_when_owner_resolved,
    reconcile_parked_global_path_members,
};
use rowan::TextRange;

use crate::{
    FileId, InFiled, LuaDeclId, LuaMemberId, LuaTypeCache, LuaTypeOwner,
    compilation::analyzer::lua::iterates_table_member_map,
    db_index::{DbIndex, LuaMemberOwner, LuaType, LuaTypeDeclId, is_informative_type},
};

/// Whether `typ` is a raw template placeholder inherited from a generic-for
/// variable that nothing has bound yet.
pub fn holds_unbound_iter_template(
    db: &DbIndex,
    file_id: FileId,
    expr: &LuaExpr,
    typ: &LuaType,
) -> bool {
    if !typ.contain_tpl() {
        return false;
    }

    expr.ancestors::<LuaForRangeStat>().any(|for_range_stat| {
        for_range_stat.get_var_name_list().any(|var_name| {
            let decl_id = LuaDeclId::new(file_id, var_name.get_position());
            db.get_type_index()
                .get_type_cache(&decl_id.into())
                .is_some_and(|cache| cache.as_type().contain_tpl())
        })
    })
}

/// Whether `expr` is a plain read of a variable of an enclosing `pairs` loop.
///
/// Those loops take their variable types from the iterated table's member map,
/// so [`analyze_for_range_stat`] queues a retry that re-derives them once the
/// map settles. A fact that copies the variable holds the same pre-settlement
/// snapshot, but nothing retries it, so committing one here would freeze
/// whichever members happened to be indexed first. Queue it behind the retry
/// instead.
///
/// Only a direct read qualifies. An expression that merely mentions the variable
/// — a concatenation, a call argument — has a type its own operator decides, so
/// deferring it buys nothing and costs a retry.
///
/// [`analyze_for_range_stat`]: super::lua::analyze_for_range_stat
pub fn reads_settling_iter_var(db: &DbIndex, file_id: FileId, expr: &LuaExpr) -> bool {
    let LuaExpr::NameExpr(name_expr) = expr else {
        return false;
    };
    let Some(name) = name_expr.get_name_text() else {
        return false;
    };
    let Some(decl) = db
        .get_decl_index()
        .get_decl_tree(&file_id)
        .and_then(|decl_tree| decl_tree.find_local_decl(&name, name_expr.get_position()))
    else {
        return false;
    };

    expr.ancestors::<LuaForRangeStat>()
        .filter(|for_range_stat| iterates_table_member_map(db, file_id, for_range_stat))
        .flat_map(|for_range_stat| for_range_stat.get_var_name_list())
        .any(|var_name| LuaDeclId::new(file_id, var_name.get_position()) == decl.get_id())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeCacheWriteMode {
    /// Preserve an existing cache and only write when the owner is currently
    /// uncached. This is the default for doc/discovery surfaces that should not
    /// overwrite an earlier authority decision.
    InsertOnly,
    /// Replace the cache unconditionally. This is reserved for policy decisions
    /// that have already computed the final widened or synthesized type.
    ForceOverwrite,
}

/// Writes a type cache using the requested low-level write mode.
///
/// Call sites fall into doc-annotation, assignment-inferred, and
/// resolved-synthesized families. Authority-based precedence was evaluated and
/// rejected in Phase C2 for lack of evidence; see
/// `.slim/deepwork/indexing-type-source-refactor.md`.
pub fn write_type_cache(
    db: &mut DbIndex,
    owner: LuaTypeOwner,
    cache: LuaTypeCache,
    mode: TypeCacheWriteMode,
) {
    match mode {
        TypeCacheWriteMode::InsertOnly => db.get_type_index_mut().bind_type(owner, cache),
        TypeCacheWriteMode::ForceOverwrite => db.get_type_index_mut().force_bind_type(owner, cache),
    }
}

/// Binds an inferred/declared type and preserves the legacy declaration merge
/// behavior.
///
/// This is intentionally broader than `write_type_cache`: when a cache already
/// exists it may merge table members into a resolved definition, or replace only
/// the narrow uninformative inferred cases documented by
/// `should_replace_uninformative_inferred_cache`.
pub fn bind_type(
    db: &mut DbIndex,
    type_owner: LuaTypeOwner,
    mut type_cache: LuaTypeCache,
) -> Option<()> {
    let decl_type_cache = db.get_type_index().get_type_cache(&type_owner);

    if decl_type_cache.is_none() {
        // type backward
        if type_cache.is_infer()
            && let LuaTypeOwner::Decl(decl_id) = &type_owner
            && let Some(decl_ref) = db
                .get_reference_index()
                .get_decl_references(&decl_id.file_id, decl_id)
            && decl_ref.mutable
        {
            match &type_cache.as_type() {
                LuaType::IntegerConst(_) => type_cache = LuaTypeCache::InferType(LuaType::Integer),
                LuaType::StringConst(_) => type_cache = LuaTypeCache::InferType(LuaType::String),
                LuaType::BooleanConst(_) => type_cache = LuaTypeCache::InferType(LuaType::Boolean),
                LuaType::FloatConst(_) => type_cache = LuaTypeCache::InferType(LuaType::Number),
                _ => {}
            }
        }

        db.get_type_index_mut()
            .bind_type(type_owner.clone(), type_cache);
        migrate_global_members_when_type_resolve(db, type_owner);
    } else {
        let decl_type_cache = decl_type_cache?;
        let decl_type = decl_type_cache.as_type();
        if should_replace_uninformative_inferred_cache(&type_owner, decl_type_cache, &type_cache) {
            db.get_type_index_mut()
                .force_bind_type(type_owner.clone(), type_cache);
            migrate_global_members_when_type_resolve(db, type_owner);
        } else {
            merge_def_type(db, decl_type.clone(), type_cache.as_type().clone(), 0);
        }
    }

    Some(())
}

/// Binds a type produced by the unresolve/resolution pass.
///
/// Resolved caches share the same uninformative inferred-type replacement test
/// as member inference, but do not inherit member-only or signature-specific
/// inferred write policy. If no resolved replacement applies, this falls back to
/// `bind_type` for the normal declaration merge behavior.
pub fn bind_resolved_type(
    db: &mut DbIndex,
    type_owner: LuaTypeOwner,
    type_cache: LuaTypeCache,
) -> Option<()> {
    if let Some(current_cache) = db.get_type_index().get_type_cache(&type_owner)
        && should_replace_uninformative_resolved_cache(current_cache, &type_cache)
    {
        db.get_type_index_mut()
            .force_bind_type(type_owner.clone(), type_cache);
        migrate_global_members_when_type_resolve(db, type_owner);
        return Some(());
    }

    bind_type(db, type_owner, type_cache)
}

fn should_replace_uninformative_resolved_cache(
    current_cache: &LuaTypeCache,
    new_cache: &LuaTypeCache,
) -> bool {
    should_replace_uninformative_infer_type_cache(current_cache, new_cache)
}

/// Whether a freshly inferred cache should displace what is already stored.
fn should_replace_uninformative_inferred_cache(
    type_owner: &LuaTypeOwner,
    current_cache: &LuaTypeCache,
    new_cache: &LuaTypeCache,
) -> bool {
    if should_inferred_signature_replace_uninformative_cache(current_cache, new_cache) {
        return true;
    }

    // `nil` and `never` sit at the bottom of the type lattice: they record
    // that no value was found, not that any value is allowed. A concrete
    // inferred type is strictly more precise, so it has to win regardless of
    // which round produced it — a local that an early round saw only as `nil`
    // must not stay `nil` once the assignment that gives it a table has been
    // analysed.
    if new_cache.supersedes(current_cache) {
        return true;
    }

    match type_owner {
        LuaTypeOwner::Member(_) => {}
        // A decl's cache is its whole-lifetime type; narrowing it to one
        // assignment's type is flow analysis' job, not the cache's. So only
        // a *placeholder* may be displaced here, and a bare `any`/`unknown`
        // is not treated as one: several inference paths deliberately park
        // a decl at `any`/`unknown` and expect it to survive a later,
        // narrower assignment (pinned by
        // `test_flow_merge_keeps_inferred_any_over_specific_non_table_assignment`,
        // `stabilized_local_respects_assignment_regions` and
        // `bind_type_keeps_uninformative_decl_cache_for_non_signature_inference`).
        LuaTypeOwner::Decl(_) => {
            if matches!(current_cache.as_type(), LuaType::Any | LuaType::Unknown) {
                return false;
            }
        }
        LuaTypeOwner::SyntaxId(_) => return false,
    }

    should_replace_uninformative_infer_type_cache(current_cache, new_cache)
}

fn should_replace_uninformative_infer_type_cache(
    current_cache: &LuaTypeCache,
    new_cache: &LuaTypeCache,
) -> bool {
    let LuaTypeCache::InferType(current_type) = current_cache else {
        return false;
    };
    let LuaTypeCache::InferType(new_type) = new_cache else {
        return false;
    };

    !is_informative_type(current_type) && is_informative_type(new_type)
}

fn should_inferred_signature_replace_uninformative_cache(
    current_cache: &LuaTypeCache,
    new_cache: &LuaTypeCache,
) -> bool {
    matches!(new_cache, LuaTypeCache::InferType(LuaType::Signature(_)))
        && match current_cache {
            LuaTypeCache::DocType(LuaType::DocFunction(func)) => {
                matches!(func.get_ret(), LuaType::Any | LuaType::Unknown)
            }
            LuaTypeCache::InferType(typ) => typ.is_any() || typ.is_unknown(),
            _ => false,
        }
}

fn merge_def_type(db: &mut DbIndex, decl_type: LuaType, expr_type: LuaType, merge_level: i32) {
    if merge_level > 1 {
        return;
    }

    if let LuaType::Def(def) = &decl_type {
        match &expr_type {
            LuaType::TableConst(in_filed_range) => {
                merge_def_type_with_table(db, def.clone(), in_filed_range.clone());
            }
            LuaType::Instance(instance) => {
                let base_ref = instance.get_base();
                merge_def_type(db, base_ref.clone(), expr_type, merge_level + 1);
            }
            _ => {}
        }
    }
}

fn merge_def_type_with_table(
    db: &mut DbIndex,
    def_id: LuaTypeDeclId,
    table_range: InFiled<TextRange>,
) -> Option<()> {
    let expr_member_owner = LuaMemberOwner::Element(table_range);
    let member_index = db.get_member_index_mut();
    let expr_member_ids = member_index
        .get_members(&expr_member_owner)?
        .iter()
        .map(|member| member.get_id())
        .collect::<Vec<_>>();
    let def_owner = LuaMemberOwner::Type(def_id);
    for table_member_id in expr_member_ids {
        add_member(db, def_owner.clone(), table_member_id);
    }

    Some(())
}

pub fn add_member(db: &mut DbIndex, owner: LuaMemberOwner, member_id: LuaMemberId) -> Option<()> {
    db.get_member_index_mut()
        .set_member_owner(owner.clone(), member_id.file_id, member_id);
    db.get_member_index_mut()
        .add_member_to_owner(owner.clone(), member_id);

    Some(())
}

fn get_owner_id(db: &DbIndex, type_owner: &LuaTypeOwner) -> Option<LuaMemberOwner> {
    let type_cache = db.get_type_index().get_type_cache(type_owner)?;
    member_owner_from_type(type_cache.as_type())
}

fn member_owner_from_type(typ: &LuaType) -> Option<LuaMemberOwner> {
    match typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => {
            Some(LuaMemberOwner::Type(type_id.clone()))
        }
        LuaType::TableConst(id) => Some(LuaMemberOwner::Element(id.clone())),
        LuaType::Instance(inst) => member_owner_from_type(inst.get_base())
            .or_else(|| Some(LuaMemberOwner::Element(inst.get_range().clone()))),
        LuaType::TypeGuard(inner) => member_owner_from_type(inner),
        LuaType::Union(union) => preferred_owner_from_types(union.types()),
        LuaType::Intersection(intersection) => {
            preferred_owner_from_types(intersection.get_types().iter())
        }
        LuaType::MultiLineUnion(union) => {
            preferred_owner_from_types(union.get_unions().iter().map(|(typ, _)| typ))
        }
        _ => None,
    }
}

fn preferred_owner_from_types<'a>(
    types: impl Iterator<Item = &'a LuaType>,
) -> Option<LuaMemberOwner> {
    let mut fallback_owner = None;
    for typ in types {
        let Some(owner) = member_owner_from_type(typ) else {
            continue;
        };

        if matches!(owner, LuaMemberOwner::Type(_)) {
            return Some(owner);
        }

        if fallback_owner.is_none() {
            fallback_owner = Some(owner);
        }
    }

    fallback_owner
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileId, LuaDecl, LuaDeclExtra, LuaMemberId, LuaSignatureId, VirtualWorkspace};
    use glua_parser::{LuaAstNode, LuaClosureExpr, LuaSyntaxId, LuaSyntaxKind};

    fn owner() -> LuaTypeOwner {
        let decl = LuaDecl::new(
            "cache_owner",
            FileId::new(1),
            TextRange::new(0.into(), 11.into()),
            LuaDeclExtra::Global {
                kind: LuaSyntaxKind::NameExpr.into(),
            },
            None,
        );
        LuaTypeOwner::Decl(decl.get_id())
    }

    fn member_owner() -> LuaTypeOwner {
        let range = TextRange::new(0.into(), 1.into());
        LuaTypeOwner::Member(LuaMemberId::new(
            LuaSyntaxId::new(LuaSyntaxKind::IndexExpr.into(), range),
            FileId::new(1),
        ))
    }

    fn signature_type() -> LuaType {
        let mut ws = VirtualWorkspace::new();
        let file_id = ws.def("local function f() end");
        let semantic_model = ws
            .analysis
            .compilation
            .get_semantic_model(file_id)
            .expect("expected semantic model");
        let closure = semantic_model
            .get_root()
            .descendants::<LuaClosureExpr>()
            .next()
            .expect("expected closure");

        LuaType::Signature(LuaSignatureId::from_closure(file_id, &closure))
    }

    #[test]
    fn write_type_cache_respects_insert_only_and_force_overwrite_modes() {
        let mut db = DbIndex::new();
        let owner = owner();

        write_type_cache(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::String),
            TypeCacheWriteMode::InsertOnly,
        );
        write_type_cache(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Integer),
            TypeCacheWriteMode::InsertOnly,
        );
        assert!(matches!(
            db.get_type_index().get_type_cache(&owner),
            Some(LuaTypeCache::InferType(LuaType::String))
        ));

        write_type_cache(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Integer),
            TypeCacheWriteMode::ForceOverwrite,
        );
        assert!(matches!(
            db.get_type_index().get_type_cache(&owner),
            Some(LuaTypeCache::InferType(LuaType::Integer))
        ));
    }

    #[test]
    fn bind_resolved_type_replaces_uninformative_resolved_cache() {
        let mut db = DbIndex::new();
        let owner = owner();

        write_type_cache(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Unknown),
            TypeCacheWriteMode::InsertOnly,
        );
        bind_resolved_type(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::String),
        );

        assert!(matches!(
            db.get_type_index().get_type_cache(&owner),
            Some(LuaTypeCache::InferType(LuaType::String))
        ));
    }

    #[test]
    fn bind_type_replaces_uninformative_member_cache() {
        let mut db = DbIndex::new();
        let owner = member_owner();

        write_type_cache(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Nil),
            TypeCacheWriteMode::InsertOnly,
        );
        bind_type(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Integer),
        );

        assert!(matches!(
            db.get_type_index().get_type_cache(&owner),
            Some(LuaTypeCache::InferType(LuaType::Integer))
        ));
    }

    #[test]
    fn bind_type_keeps_uninformative_decl_cache_for_non_signature_inference() {
        let mut db = DbIndex::new();
        let owner = owner();

        write_type_cache(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Unknown),
            TypeCacheWriteMode::InsertOnly,
        );
        bind_type(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Integer),
        );

        assert!(matches!(
            db.get_type_index().get_type_cache(&owner),
            Some(LuaTypeCache::InferType(LuaType::Unknown))
        ));
    }

    #[test]
    fn bind_type_replaces_uninformative_decl_cache_for_signature_inference() {
        let mut db = DbIndex::new();
        let owner = owner();
        let signature_type = signature_type();

        write_type_cache(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(LuaType::Unknown),
            TypeCacheWriteMode::InsertOnly,
        );
        bind_type(
            &mut db,
            owner.clone(),
            LuaTypeCache::InferType(signature_type),
        );

        assert!(matches!(
            db.get_type_index().get_type_cache(&owner),
            Some(LuaTypeCache::InferType(LuaType::Signature(_)))
        ));
    }
}
