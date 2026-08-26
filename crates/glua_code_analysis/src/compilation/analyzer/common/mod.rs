mod migrate_global_member;
use glua_parser::{LuaAstNode, LuaAstToken, LuaExpr, LuaForRangeStat};
pub(super) use migrate_global_member::{
    migrate_global_members_when_type_resolve, migrate_global_path_members_when_owner_resolved,
    reconcile_directly_attached_candidate_members, reconcile_parked_global_path_members,
};
use rowan::{TextRange, TextSize};

use crate::{
    FileId, InFiled, LuaDeclId, LuaMemberId, LuaTypeCache, LuaTypeOwner,
    compilation::analyzer::lua::iterates_table_member_map,
    db_index::{
        DbIndex, LuaMemberOwner, LuaType, LuaTypeDeclId, is_informative_type, is_undetermined_type,
    },
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
    type_cache: LuaTypeCache,
) -> Option<()> {
    let decl_type_cache = db.get_type_index().get_type_cache(&type_owner);

    if decl_type_cache.is_none() {
        seed_type_slot(db, type_owner, type_cache);
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

/// Seeds a type owner that holds nothing yet, widening a mutable declaration's
/// literal to its base type on the way in.
fn seed_type_slot(db: &mut DbIndex, type_owner: LuaTypeOwner, type_cache: LuaTypeCache) {
    let type_cache = widen_mutable_decl_literal(db, &type_owner, type_cache);
    db.get_type_index_mut()
        .force_bind_type(type_owner.clone(), type_cache);
    migrate_global_members_when_type_resolve(db, type_owner);
}

/// A declaration written more than once holds a primitive over its lifetime,
/// not whichever literal one write happened to carry.
pub(crate) fn widen_mutable_decl_literal(
    db: &DbIndex,
    type_owner: &LuaTypeOwner,
    type_cache: LuaTypeCache,
) -> LuaTypeCache {
    if !type_cache.is_infer() {
        return type_cache;
    }
    let LuaTypeOwner::Decl(decl_id) = type_owner else {
        return type_cache;
    };
    if !db
        .get_reference_index()
        .get_decl_references(&decl_id.file_id, decl_id)
        .is_some_and(|decl_ref| decl_ref.mutable)
    {
        return type_cache;
    }
    match type_cache.as_type() {
        LuaType::IntegerConst(_) => LuaTypeCache::InferType(LuaType::Integer),
        LuaType::StringConst(_) => LuaTypeCache::InferType(LuaType::String),
        LuaType::BooleanConst(_) => LuaTypeCache::InferType(LuaType::Boolean),
        LuaType::FloatConst(_) => LuaTypeCache::InferType(LuaType::Number),
        _ => type_cache,
    }
}

/// Where a write to a declaration came from, for [`bind_decl_write`].
#[derive(Clone, Copy)]
pub struct DeclWrite {
    /// Source position of the writing statement.
    pub position: TextSize,
    /// Whether the right-hand side is one whose answer can still improve — a
    /// call or index read, or an operator over one. Only those may fill in a
    /// declaration that nothing has determined yet.
    pub may_improve_after_resolve: bool,
    /// Whether the right-hand side reads out of the declaration it writes to
    /// (`width = bit.bor(width:byte(1), ...)`). Such a write derives its type
    /// from the slot it is about to fill, so it must not fill it.
    pub reads_out_of_decl: bool,
    /// Whether the right-hand side is `<this decl> or <default>`, the one body
    /// write that refines a parameter instead of replacing it.
    pub fills_own_default: bool,
    /// Whether this write is one the file walk would let replace an
    /// uninformative cache: an initializer whose answer can still improve, or
    /// an assignment that reads through a call or index — the boundary
    /// `should_retry_narrowing_decl_assignment` enforces. Used to replay the
    /// acceptance rule a competing write would have faced, not to route this
    /// one.
    pub may_narrow_uninformative: bool,
    /// Whether this write is the declaration's own initializer arriving from
    /// the unresolve pass, which is the only route allowed to displace an
    /// uninformative cache through [`bind_resolved_type`].
    pub resolved_initializer: bool,
}

/// Binds a decl type written by the statement at `write.position`.
///
/// An empty decl slot otherwise goes to whichever write reaches it first, and a
/// write whose right-hand side could not be inferred during the file walk
/// reaches it late — so the decl's type depended on which callees the batch had
/// already resolved rather than on the source. Ordering the claim by source
/// position makes both arrival orders agree on the same answer: the earliest
/// writer owns the decl, except that a write which determined nothing never
/// takes the slot back from a later one that did.
pub fn bind_decl_write(
    db: &mut DbIndex,
    decl_id: LuaDeclId,
    type_cache: LuaTypeCache,
    write: DeclWrite,
) -> Option<()> {
    let DeclWrite {
        position,
        may_improve_after_resolve,
        reads_out_of_decl,
        may_narrow_uninformative,
        resolved_initializer,
        fills_own_default,
    } = write;
    let type_owner = LuaTypeOwner::Decl(decl_id);
    let fallback = |db: &mut DbIndex, type_cache| {
        if resolved_initializer {
            bind_resolved_type(db, type_owner.clone(), type_cache)
        } else {
            bind_type(db, type_owner.clone(), type_cache)
        }
    };
    // A parameter's type is its declared or call-site-inferred type; the writes
    // in the body narrow it for flow analysis, they do not own it. Only a local
    // has a "first writer" to order.
    if db
        .get_decl_index()
        .get_decl(&decl_id)
        .is_none_or(|decl| decl.is_param())
    {
        // A default fill goes through the resolved path, so
        // `gender = gender or GENDER_MALE` gives the same answer whether it was
        // inferred during the walk — seeding the slot outright — or deferred
        // until after the unresolve pass parked `unknown` there. Which of those
        // happens depends on whether the file defining `GENDER_MALE` had been
        // walked yet, which is a property of the batch, not of the source.
        //
        // Only a default fill: a reassignment to something else — splitting a
        // string parameter into a list, say — states what the parameter becomes
        // further down one branch, not what it was passed.
        if fills_own_default {
            let widened = widen_mutable_decl_literal(db, &type_owner, type_cache);
            return bind_resolved_type(db, type_owner, widened);
        }
        return fallback(db, type_cache);
    }
    let seeded = widen_mutable_decl_literal(db, &type_owner, type_cache.clone());
    let seeds = match db.get_type_index().get_type_cache(&type_owner) {
        None => true,
        Some(existing) => {
            let both_inferred = existing.is_infer() && type_cache.is_infer();
            let comparable = both_inferred && !reads_out_of_decl;
            // `any` is the one answer neither `bind_type` nor
            // `bind_resolved_type` will trade in either direction, so between
            // two ordered writes it is ranked rather than positioned: whichever
            // determined something takes the slot, and the winner is then a
            // function of the write set instead of which one resolved first. The
            // other bottoms are left alone — `unknown` on a declaration is what
            // lets a use narrow it, not a give-up to be overwritten.
            let outranks_any =
                comparable && is_informative_type(seeded.as_type()) && existing.as_type().is_any();
            let outranked_by_any =
                comparable && seeded.as_type().is_any() && is_informative_type(existing.as_type());
            if outranks_any {
                true
            } else if outranked_by_any {
                false
            } else if comparable
                && may_improve_after_resolve
                && is_informative_type(seeded.as_type())
                && is_undetermined_type(existing.as_type())
            {
                // The slot holds an inferred give-up answer and this write
                // determined something from a right-hand side the walk already
                // treats as improvable (`should_retry_uninformative_initializer`).
                // Applying that here too keeps the answer the same whether the
                // write was committed during the walk or deferred to this pass.
                true
            } else {
                match db.get_type_index().decl_write_claim(&decl_id) {
                    // Nothing else has taken the slot by source position, so
                    // whatever is in it was not put there by an ordered write.
                    None => false,
                    Some((claimed, claim_may_narrow)) => {
                        // Source position arbitrates between two answers, not
                        // between an answer and none. An earlier write that came
                        // back undetermined -- an unresolve retry that still
                        // cannot see through its initializer -- must not take the
                        // slot from a later one that resolved, or the
                        // declaration is left needing its type guessed from how
                        // it is used.
                        let displaces_an_answer = is_undetermined_type(seeded.as_type())
                            && is_informative_type(existing.as_type());
                        position < claimed
                            && !displaces_an_answer
                            && !claiming_write_would_have_won(
                                &type_owner,
                                &seeded,
                                existing,
                                claim_may_narrow,
                            )
                    }
                }
            }
        }
    };
    if !seeds {
        return fallback(db, type_cache);
    }
    db.get_type_index_mut()
        .record_decl_write_claim(decl_id, position, may_narrow_uninformative);
    seed_type_slot(db, type_owner, type_cache);
    Some(())
}

/// Replays the acceptance rule the slot's current holder would have faced had
/// this write reached the slot first, and reports whether it would still have
/// taken it.
///
/// The rule depends on how that write was committed: a call or index read whose
/// target is uninformative goes through the unresolve pass and
/// [`bind_resolved_type`], which displaces it; anything else goes through
/// [`bind_type`], which keeps a decl's whole-lifetime type unless the incoming
/// one supersedes it.
fn claiming_write_would_have_won(
    type_owner: &LuaTypeOwner,
    seeded: &LuaTypeCache,
    existing: &LuaTypeCache,
    claim_may_narrow: bool,
) -> bool {
    if claim_may_narrow && should_replace_uninformative_resolved_cache(seeded, existing) {
        return true;
    }
    should_replace_uninformative_inferred_cache(type_owner, seeded, existing)
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
