//! Re-derives index-expression member owners once the batch has settled.
//!
//! The Lua walk homes `t[k] = v` onto whatever the prefix `t` inferred to at
//! the moment the walk reached the write
//! ([`set_index_expr_owner`](super::lua::stats)). The narrowing that picks the
//! owner draws candidates from the live member map, which is only partly
//! built mid-walk: a cold build homes onto the fallback owner while a warm
//! re-index homes onto the real one, and the attach gate treats the first
//! answer as final. Deferred resolution has the same shape:
//! [`try_resolve_member`](super::unresolve::try_resolve_member) places a
//! provisional owner from a mid-fixpoint prefix type and never revisits it.
//!
//! This pass re-runs the same owner decision against the settled indexes and
//! *replaces* a contradicted owner (detach + home + merge), so a cold build
//! and a re-analysis converge on the same placement. Scoping follows the other
//! settled passes: first round re-derives every candidate file, later rounds
//! only files whose recorded reads went stale.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr};

use crate::{
    DbIndex, FileId, GlobalId, InFiled, LuaInferCache, LuaMemberId, LuaMemberKey, LuaType,
    db_index::LuaMemberOwner,
};

use super::{AnalyzeContext, SettledReadPass, parallel};

fn owner_settle_candidates_for_file(
    db: &DbIndex,
    file_id: FileId,
    unresolve_deferred: &HashSet<LuaMemberId>,
) -> Vec<LuaMemberId> {
    db.get_member_index()
        .get_file_members(file_id)
        .into_iter()
        .filter(|member| {
            // Only index-expression writes have a prefix expression to
            // re-infer; doc fields, table fields, and static names name
            // their owner syntactically and must never move here.
            if !glua_parser::LuaIndexExpr::can_cast(member.get_id().get_syntax_id().get_kind()) {
                return false;
            }
            let id = member.get_id();
            let index = db.get_member_index();
            // Deferred members are always retried: their first home was
            // explicitly provisional, so they always retry.
            if index.is_deferred_index_expr_member(&id) {
                return true;
            }
            if index.has_synthesized_owner(&id) {
                return false;
            }
            // A member that already carries path provenance is owned by the
            // class-flip machinery, not by the prefix read the settle
            // re-derives: `set_global_path_class` placed it, and re-homing
            // it here would detach it from the path it must follow. The
            // exemption only holds while the path can actually flip — a
            // plain global variable's provenance is inert, so its members
            // stay settle-able.
            if index
                .member_path_provenance(&id)
                .is_some_and(|path| index.global_path_has_class_candidates(path))
            {
                return false;
            }
            // A field on a factory local consumed by `setmetatable(local,
            // Meta)` is owned by the factory transfer, not by the write's
            // prefix read: re-homing it onto the source literal undoes the
            // transfer the synthesis just performed.
            if factory_local_member(db, file_id, &id).is_some() {
                return false;
            }
            // A write the walk's value-flow gates skipped (`t[k] = nil`
            // deletes, unknown keys onto multi-owner prefixes) carries no
            // walk-time owner decision to re-derive: the walk deliberately
            // refused to place it, so the settle must not mint one either.
            if walk_skipped_value_flow_write(db, file_id, id) {
                return false;
            }
            // A member that already carries an owner keeps the old rule: only
            // a real (non-`LocalUnresolve`) placement can be contradicted.
            match index.get_current_owner(&id) {
                Some(current) => !current.is_unknown(),
                // An ownerless member is a candidate only when its write was
                // a *failed prefix resolution*. Those are exactly the writes
                // the walk queued for the settled attach retry — the
                // no-owner-information fallthrough and the finalized member
                // unresolves in `set_index_expr_owner` both land in
                // `settled_member_attach_candidates`, and the attach passes
                // keep the ones they could not place queued. A deliberately
                // unowned write — a `t.x = nil` delete the walk's value-flow
                // gates refused, a prefix that names no table at all —
                // records no retry, so re-homing it here would mint the
                // placement the walk deliberately refused to make. For the
                // queued ones the re-derivation below keeps the member put
                // while the prefix still says nothing and homes it once the
                // prefix names a table, which is exactly the decision warm
                // makes.
                None => unresolve_deferred.contains(&id),
            }
        })
        .map(|member| member.get_id())
        .collect()
}

/// Whether `member_id`'s write was skipped by the walk's value-flow gates, so
/// there is no walk-time owner decision to re-derive here.
///
/// The `None`-returning fallthroughs in `set_index_expr_owner` leave the
/// member homed where the legacy decl pass (or the deferred creator) first
/// put it; only the skip gates leave *no decision at all* behind. Those are
/// the `nil`-into-table-shape deletes
/// (`should_skip_nil_table_shape_assignment`) and the unknown-key writes onto
/// multi-owner prefixes
/// (`should_skip_ambiguous_unknown_key_table_owner`): both return `Some(())`
/// before any `apply_index_expr_member_owner` runs, so re-deriving a settled
/// owner for them here would mint a placement the walk deliberately refused
/// to make. The async predicate mirrors the walk's — a statically known
/// member key plus the syntactic prefix shape — without re-running inference,
/// so candidacy stays a file-local sweep.
fn walk_skipped_value_flow_write(db: &DbIndex, file_id: FileId, member_id: LuaMemberId) -> bool {
    use glua_parser::{LuaAstNode, LuaIndexKey};
    let Some(member) = db.get_member_index().get_member(&member_id) else {
        return false;
    };
    // Only a static member key can be a walk skip: an `ExprType` key means the
    // write was filed dynamically and the gates below never fired for it.
    if member.get_key().is_expr() {
        return false;
    }
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_red_root())
    else {
        return false;
    };
    let Some(node) = member_id.get_syntax_id().to_node_from_root(&root) else {
        return false;
    };
    let Some(index_expr) = LuaIndexExpr::cast(node) else {
        return false;
    };
    // The unknown-key/multi-owner skip fires only for an `ExprType(Unknown)`
    // index key; the nil-shape skip only files static keys. Either way the
    // gate needs a key expression to have skipped on.
    if !matches!(index_expr.get_index_key(), Some(LuaIndexKey::Expr(_))) {
        return false;
    }
    let Some(prefix_expr) = index_expr.get_prefix_expr() else {
        return false;
    };
    // A computed path never reaches either gate's owner read: the walk files
    // it under the placeholder segment first.
    if prefix_path_has_computed_segment(&prefix_expr) {
        return false;
    }
    // The nil-shape gate only skips deletes into table-shaped prefixes; the
    // unknown-key gate only skips writes whose prefix could name several
    // tables. Both need at least an index-expression prefix to ask about —
    // a bare `NameExpr` prefix (the `MHandler.set` shape) reaches neither
    // gate and must stay a candidate.
    matches!(prefix_expr, LuaExpr::IndexExpr(_))
}

/// Re-derives the owner decision for one member against the settled indexes.
///
/// Returns the settled owner when it names a real placement that differs from
/// the member's current one. `None` means keep: the prefix still says nothing,
fn settled_owner_for_member(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    file_id: FileId,
    member_id: LuaMemberId,
) -> Option<(LuaMemberOwner, bool)> {
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_red_root())?;
    let node = member_id.get_syntax_id().to_node_from_root(&root)?;
    let index_expr = LuaIndexExpr::cast(node)?;
    let prefix_expr = index_expr.get_prefix_expr()?;
    // Same guard as the walk's head: a `GlobalPath`-homed write through a
    // computed index keeps its placeholder filing. Anything else falls
    // through to the same decision below.
    if prefix_path_has_computed_segment(&prefix_expr) {
        let keep = db
            .get_member_index()
            .get_current_owner(&member_id)
            .is_some_and(|owner| matches!(owner, LuaMemberOwner::GlobalPath(_)));
        if keep {
            return None;
        }
    }

    // Syntactic fast paths first, in the same order as the Lua walk: a
    // scoped-class or direct-local-table prefix names its owner without any
    // inference, and those answers do not move between rounds.
    if let Some(owner) = scoped_class_prefix_owner(db, file_id, &prefix_expr) {
        return Some((owner, false));
    }
    if let Some(owner) = direct_local_table_prefix_owner(db, file_id, &prefix_expr) {
        return Some((owner, false));
    }
    if let Some(path) = crate::semantic::pairs_iter_value_registry_path(db, file_id, &prefix_expr) {
        return Some((LuaMemberOwner::GlobalPath(GlobalId::new(&path)), false));
    }

    let prefix_type = crate::semantic::infer_expr(db, cache, prefix_expr.clone()).ok()?;
    if prefix_carries_no_owner_information(&prefix_type) {
        return None;
    }
    // An unknown key cannot pick between candidate tables; same skip as the
    // walk and the deferred creator.
    if index_expr
        .get_index_key()
        .and_then(|key| LuaMemberKey::from_index_key_or_unknown(db, cache, &key).ok())
        .is_some_and(|key| matches!(key, LuaMemberKey::ExprType(ref typ) if typ.is_unknown()))
        && super::lua::has_multiple_distinct_index_expr_member_owners(&prefix_type)
    {
        return None;
    }
    super::lua::resolve_index_expr_member_owner_for_file(db, &prefix_type, Some(file_id))
}
/// Whether the write goes through a computed index (`t[k].field = v`).
///
/// Local copy of the walk's gate: which entry `t[k]` names is not knowable
/// statically, so the declaration walk files such a write under the path's
/// placeholder segment. Kept in sync with it by construction — same shape,
/// same key kinds.
fn prefix_path_has_computed_segment(prefix_expr: &LuaExpr) -> bool {
    use glua_parser::LuaIndexKey;
    let mut current = prefix_expr.clone();
    loop {
        let LuaExpr::IndexExpr(index_expr) = current else {
            return false;
        };
        if matches!(
            index_expr.get_index_key(),
            Some(LuaIndexKey::Expr(_) | LuaIndexKey::Idx(_))
        ) {
            return true;
        }
        match index_expr.get_prefix_expr() {
            Some(next) => current = next,
            None => return false,
        }
    }
}

/// Whether a prefix type says nothing about which table a member lands on.
///
/// Mirrors the walk's `prefix_carries_no_owner_information`: a number or a
/// string is a real answer (no owner), while unknown/any/table is an
/// unsettled read worth retrying later.
fn prefix_carries_no_owner_information(prefix_type: &LuaType) -> bool {
    match prefix_type {
        LuaType::Unknown | LuaType::Any | LuaType::Table => true,
        LuaType::Union(union) => union.types().all(|arm| {
            matches!(
                arm,
                LuaType::Nil | LuaType::Unknown | LuaType::Any | LuaType::Table
            )
        }),
        _ => false,
    }
}

/// The walk's scoped-class fast path, read without an analyzer handle.
fn scoped_class_prefix_owner(
    db: &DbIndex,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> Option<LuaMemberOwner> {
    super::lua::scoped_class_prefix_member_owner(db, file_id, prefix_expr).map(|(owner, _)| owner)
}

/// The factory local whose table a member sits on, when `setmetatable(local,
/// Meta)` consumed it: the factory transfer owns that field, so the owner
/// settle must leave it alone. Matches both ends of the binding — the source
/// literal (which the transfer detaches from) and the metatable it lands on
/// (whose later re-derivation would detach it back).
fn factory_local_member(db: &DbIndex, file_id: FileId, id: &LuaMemberId) -> Option<()> {
    let current = db.get_member_index().get_current_owner(id)?.clone();
    let LuaMemberOwner::Element(table) = &current else {
        return None;
    };
    let bindings = db
        .get_metatable_index()
        .factory_bindings_for_file(file_id)?;
    bindings
        .iter()
        .any(|binding| binding.table_range == *table || binding.metatable_range == *table)
        .then_some(())
}

/// The walk's direct-local-table fast path, read without an analyzer handle:
fn direct_local_table_prefix_owner(
    db: &DbIndex,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> Option<LuaMemberOwner> {
    let LuaExpr::NameExpr(name_expr) = prefix_expr else {
        return None;
    };
    let decl_id = db
        .get_reference_index()
        .get_local_reference(&file_id)
        .and_then(|file_ref| file_ref.get_decl_id(&name_expr.get_range()))?;
    if db
        .get_type_index()
        .get_type_cache(&decl_id.into())
        .is_some_and(crate::LuaTypeCache::is_doc)
    {
        return None;
    }
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if decl.get_initializer()?.get_ret_idx() != 0 {
        return None;
    }
    let root = db
        .get_vfs()
        .get_syntax_tree(&decl_id.file_id)?
        .get_red_root();
    let node = decl
        .get_initializer()?
        .get_expr_syntax_id()
        .to_node_from_root(&root)?;
    let table_expr = glua_parser::LuaTableExpr::cast(node)?;
    // A mutable local may be reassigned; the walk declines those too.
    let mutable = db
        .get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .map(|decl_ref| decl_ref.mutable)
        .unwrap_or(false);
    (!mutable)
        .then(|| LuaMemberOwner::Element(InFiled::new(decl_id.file_id, table_expr.get_range())))
}

/// Re-derives index-expression member owners against the settled indexes.
///
/// Returns whether any member moved. A moved member-map reader may answer
/// differently now, so the tail continues the loop — and re-derives every
/// candidate unfiltered next round — when this returns true.
pub(crate) fn rederive_settled_index_expr_owners(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
    moved_owners: Option<&HashSet<crate::LuaTypeOwner>>,
) -> bool {
    let _p = crate::profile::Profile::new("settled_owner_pass");
    let read_pass = SettledReadPass::Owner;
    let candidate_count = db.get_member_index().get_members_count();
    let read_version = crate::db_index::read_set::InferenceReadVersion::capture(db);

    // Candidate files: every analyzed file holding a candidate member. Built
    // once per candidate-count epoch like the member-key pass.
    if context
        .settled_owner_candidates
        .as_ref()
        .is_none_or(|(count, _)| *count != candidate_count)
    {
        // Provenance for ownerless members: the walk queues a write whose
        // prefix read failed for the settled attach retry, and the attach
        // passes keep every write they could not place queued. At this point
        // that queue is exactly the set of failed resolutions — a write the
        // walk deliberately left unowned never enters it.
        let unresolve_deferred: HashSet<LuaMemberId> = context
            .settled_member_attach_candidates
            .iter()
            .map(|candidate| LuaMemberId::new(candidate.value, candidate.file_id))
            .collect();
        let mut candidates = HashMap::default();
        for file_id in context.analyzed_file_ids() {
            let mut members = owner_settle_candidates_for_file(db, file_id, &unresolve_deferred);
            members.sort_unstable_by_key(|id| id.get_syntax_id().get_range().start());
            if !members.is_empty() {
                candidates.insert(file_id, members);
            }
        }
        context.settled_owner_candidates = Some((candidate_count, candidates));
    }
    let Some((_, candidates)) = context.settled_owner_candidates.as_ref() else {
        return false;
    };
    let mut file_ids = candidates.keys().copied().collect::<Vec<_>>();
    if file_ids.is_empty() {
        return false;
    }
    file_ids.sort_unstable();
    file_ids.retain(|file_id| {
        context.settled_pass_reads_stale(db, read_pass, candidate_count, *file_id, moved_owners)
    });
    if file_ids.is_empty() {
        return false;
    }

    let analysis_phase = context.infer_manager.current_phase();
    let dynamic_fields_visible = context.infer_manager.dynamic_fields_visible();
    let call_site_params_visible = context.infer_manager.call_site_params_visible();

    // Read-only per file, applied on the caller thread in file order like
    let results = parallel::map_files_collect(db, &file_ids, |db, file_id| {
        let mut infer_cache = LuaInferCache::new(
            file_id,
            crate::CacheOptions {
                analysis_phase,
                dynamic_fields_visible,
                building_dynamic_field_index: false,
                call_site_params_visible,
            },
        );
        let mut updates = Vec::new();
        crate::db_index::read_set::arm_with_member_maps();
        for &member_id in &candidates[&file_id] {
            let current = db.get_member_index().get_current_owner(&member_id).cloned();
            let Some((settled_owner, set_owner_only)) =
                settled_owner_for_member(db, &mut infer_cache, file_id, member_id)
            else {
                continue;
            };
            // Never displace a synthesized owner, and never move onto the
            // same owner: both are append-only no-ops.
            if db.get_member_index().has_synthesized_owner(&member_id) {
                continue;
            }
            let canonical_settled = db.get_member_index().canonical_owner(settled_owner.clone());
            if current.as_ref() == Some(&canonical_settled) {
                continue;
            }
            // `set_owner_only` keeps the walk's meaning: a `Ref` prefix
            // names a declared class without becoming one of its members.
            updates.push((member_id, settled_owner, set_owner_only, current));
        }
        (file_id, updates, crate::db_index::read_set::disarm())
    });

    let mut moved = false;
    for (file_id, updates, reads) in results {
        context.record_settled_pass_reads(read_pass, file_id, candidate_count, read_version, reads);
        for (member_id, settled_owner, set_owner_only, previous) in updates {
            let index = db.get_member_index_mut();
            if index.has_synthesized_owner(&member_id) {
                continue;
            }
            // Replace, never accrete: detach from the contradicted owner
            // first so the old slot does not keep a writer it lost.
            if let Some(previous) = previous
                && previous != index.canonical_owner(settled_owner.clone())
            {
                index.detach_member_from_owner(&previous, member_id);
            }
            if set_owner_only {
                index.set_member_owner_only(settled_owner, file_id, member_id);
            } else {
                index.set_member_owner(settled_owner.clone(), file_id, member_id);
                index.add_member_to_owner(settled_owner, member_id);
            }
            moved = true;
        }
    }
    // The candidate set names members by count epoch; a move re-homes them,
    // so drop the snapshot and rebuild next round.
    if moved {
        context.settled_owner_candidates = None;
    }
    moved
}
