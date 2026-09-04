//! Propagating inferred positive guards across an incremental re-analysis.
//!
//! A guard one file infers is evidence other files read, and an edit can add,
//! move or withdraw one. The snapshot taken before re-analysis is what the pass
//! afterwards diffs against to decide which readers have to run again.

use glua_parser::{LuaAstNode, LuaCallExpr, LuaExpr, LuaIndexKey, LuaNameExpr, LuaParenExpr};
use rustc_hash::{FxHashMap, FxHashSet};

use crate::*;

/// The cross-file facts an edit can invalidate, captured before
/// re-analysis.
#[derive(Clone, Debug, Default)]
pub(crate) struct InferredGuardSnapshot {
    facts: FxHashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
    consumers: FxHashMap<LuaInferredGuardOwner, FxHashSet<FileId>>,
    /// Parameter types inferred from the snapshotted files' call sites, keyed by
    /// the callee signature they belong to.
    inferred_params: FxHashMap<(LuaSignatureId, usize), LuaType>,
    /// The files the snapshot was taken for, needed to recompute the same set.
    snapshot_file_ids: FxHashSet<FileId>,
}

impl InferredGuardSnapshot {
    /// Whether the snapshot recorded anything an edit could invalidate. An empty
    /// one means the propagation pass has no work to do.
    pub(crate) fn is_empty(&self) -> bool {
        self.facts.is_empty() && self.inferred_params.is_empty()
    }

    /// The files this snapshot was taken for, so the same set can be recomputed
    /// after re-analysis and diffed against it.
    pub(crate) fn snapshot_file_ids(&self) -> &FxHashSet<FileId> {
        &self.snapshot_file_ids
    }

    /// Folds a later snapshot in without overwriting anything already held:
    /// the oldest facts are the ones propagation has to diff against.
    pub(crate) fn merge(&mut self, other: Self) {
        for (owner, guard) in other.facts {
            self.facts.entry(owner).or_insert(guard);
        }
        for (owner, consumers) in other.consumers {
            self.consumers.entry(owner).or_insert(consumers);
        }
        for (key, typ) in other.inferred_params {
            self.inferred_params.entry(key).or_insert(typ);
        }
        self.snapshot_file_ids.extend(other.snapshot_file_ids);
    }
}

#[derive(Default)]
struct InferredGuardReferenceFiles {
    files: FxHashSet<FileId>,
    alias_calls: FxHashSet<FileId>,
}

#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct InferredGuardPropagationStats {
    pub changed_facts: usize,
    pub reference_edges: usize,
    pub frontiers: usize,
    pub reindexed_files: usize,
}

fn sort_inferred_guard_owners(owners: &mut [LuaInferredGuardOwner]) {
    owners.sort_by(|left, right| {
        (left.source_file_id(), left.source_position(), left.path()).cmp(&(
            right.source_file_id(),
            right.source_position(),
            right.path(),
        ))
    });
}

fn global_path_for_expr(expr: &LuaExpr) -> Option<Vec<smol_str::SmolStr>> {
    let mut path = match expr {
        LuaExpr::NameExpr(name_expr) => {
            Some(vec![name_expr.get_name_token()?.get_name_text().into()])
        }
        LuaExpr::IndexExpr(index_expr) => {
            if index_expr.get_index_token()?.is_colon() {
                return None;
            }
            let mut path = global_path_for_expr(&index_expr.get_prefix_expr()?)?;
            let member = match index_expr.get_index_key()? {
                LuaIndexKey::Name(name) => name.get_name_text().into(),
                LuaIndexKey::String(string) => string.get_value().into(),
                _ => return None,
            };
            path.push(member);
            Some(path)
        }
        _ => None,
    }?;
    canonicalize_global_root_path(&mut path);
    Some(path)
}

fn immutable_local_alias_decl(
    db: &DbIndex,
    file_id: FileId,
    alias_value: &LuaExpr,
) -> Option<LuaDeclId> {
    let alias_value = enclosing_parenthesized_expr(alias_value);
    let local_stat = alias_value.get_parent::<LuaLocalStat>()?;
    let local_name = local_stat.get_local_name_by_value(alias_value.clone())?;
    let decl_id = LuaDeclId::new(file_id, local_name.get_position());
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if !matches!(decl.extra, LuaDeclExtra::Local { .. })
        || decl.get_value_syntax_id() != Some(alias_value.get_syntax_id())
        || db
            .get_reference_index()
            .get_decl_references(&file_id, &decl_id)
            .is_none_or(|references| references.mutable)
    {
        return None;
    }
    Some(decl_id)
}

fn enclosing_parenthesized_expr(expr: &LuaExpr) -> LuaExpr {
    let mut expr = expr.clone();
    while let Some(paren_expr) = expr.get_parent::<LuaParenExpr>() {
        if paren_expr
            .get_expr()
            .is_none_or(|inner| inner.get_syntax_id() != expr.get_syntax_id())
        {
            break;
        }
        expr = LuaExpr::ParenExpr(paren_expr);
    }
    expr
}

fn is_call_prefix(expr: &LuaExpr) -> bool {
    let expr = enclosing_parenthesized_expr(expr);
    expr.get_parent::<LuaCallExpr>()
        .and_then(|call| call.get_prefix_expr())
        .is_some_and(|prefix| prefix.get_syntax_id() == expr.get_syntax_id())
}

fn expr_resolves_to_inferred_guard_owner(
    db: &DbIndex,
    caches: &mut FxHashMap<FileId, LuaInferCache>,
    owner: &LuaInferredGuardOwner,
    file_id: FileId,
    expr: &LuaExpr,
) -> bool {
    let cache = caches
        .entry(file_id)
        .or_insert_with(|| LuaInferCache::new(file_id, Default::default()));
    semantic::infer_expr(db, cache, expr.clone()).ok()
        == Some(LuaType::Signature(owner.signature_id()))
}

fn call_resolves_to_inferred_guard_owner(
    db: &DbIndex,
    caches: &mut FxHashMap<FileId, LuaInferCache>,
    owner: &LuaInferredGuardOwner,
    file_id: FileId,
    prefix_expr: &LuaExpr,
) -> bool {
    let prefix_expr = enclosing_parenthesized_expr(prefix_expr);
    let Some(call) = prefix_expr.get_parent::<LuaCallExpr>() else {
        return false;
    };
    if call
        .get_prefix_expr()
        .is_none_or(|prefix| prefix.get_syntax_id() != prefix_expr.get_syntax_id())
    {
        return false;
    }
    let cache = caches
        .entry(file_id)
        .or_insert_with(|| LuaInferCache::new(file_id, Default::default()));
    semantic::get_prefix_expr_signature_id(db, cache, &call) == Some(owner.signature_id())
}

impl EmmyLuaAnalysis {
    /// Returns every file this propagation re-analysed, so an edit can report
    /// the full set it settled rather than only the files whose text changed.
    pub(crate) fn reindex_changed_inferred_guard_references(
        &mut self,
        source_file_ids: &FxHashSet<FileId>,
        old_snapshot: &InferredGuardSnapshot,
        already_reindexed: &[FileId],
        incremental_source_file_ids: &FxHashSet<FileId>,
    ) -> FxHashSet<FileId> {
        let profile_enabled = std::env::var_os("GLUALS_PROFILE").is_some();
        let mut profile_changed_facts = 0usize;
        let mut profile_reference_edges = 0usize;
        let mut profile_waves = 0usize;
        let mut profile_reindexed_files = 0usize;
        let mut propagation_reindexed_files = source_file_ids
            .iter()
            .copied()
            .chain(already_reindexed.iter().copied())
            .collect::<FxHashSet<_>>();
        let mut new_facts = self
            .compilation
            .get_db()
            .get_signature_index()
            .inferred_guard_facts_for_files(source_file_ids);
        let equivalent_owners = self.reconcile_equivalent_inferred_guard_owners(
            old_snapshot,
            &new_facts,
            &propagation_reindexed_files,
        );
        let old_facts = &old_snapshot.facts;
        let mut changed_owners = old_facts
            .keys()
            .chain(new_facts.keys())
            .filter(|owner| {
                !equivalent_owners.contains(*owner)
                    && old_facts.get(*owner) != new_facts.get(*owner)
            })
            .cloned()
            .collect::<FxHashSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        if changed_owners.is_empty() {
            #[cfg(test)]
            {
                self.inferred_guard_propagation_stats = InferredGuardPropagationStats::default();
            }
            if profile_enabled {
                eprintln!(
                    "[profile] inferred_guard_incremental changed_facts=0 reference_edges=0 waves=0 reindexed_files=0"
                );
            }
            return propagation_reindexed_files;
        }
        profile_changed_facts += changed_owners.len();
        sort_inferred_guard_owners(&mut changed_owners);
        let mut frontier_old_facts = old_snapshot.facts.clone();
        let mut frontier_old_consumers = old_snapshot.consumers.clone();

        let mut fuse = crate::compilation::analyzer::FixpointFuse::new("guard_frontier");
        while !changed_owners.is_empty() {
            if fuse.trip() {
                break;
            }
            let mut reference_files = FxHashSet::default();
            for owner in &changed_owners {
                let newly_added =
                    !frontier_old_facts.contains_key(owner) && new_facts.contains_key(owner);
                let old_consumers = frontier_old_consumers
                    .get(owner)
                    .into_iter()
                    .flatten()
                    .copied();
                let current_consumers = self
                    .compilation
                    .get_db()
                    .get_signature_index()
                    .inferred_guard_consumers(owner);
                for file_id in old_consumers.chain(current_consumers) {
                    if !propagation_reindexed_files.contains(&file_id) {
                        profile_reference_edges += 1;
                        reference_files.insert(file_id);
                    }
                }
                if newly_added {
                    let allow_alias_retry =
                        incremental_source_file_ids.contains(&owner.source_file_id());
                    let discovered = self.resolve_inferred_guard_reference_files(owner, true);
                    for file_id in discovered.files {
                        // Cold batches resolve aliases in the main pipeline. Only edits need a
                        // post-publication retry for alias calls analyzed with the old guard fact.
                        let alias_retry = allow_alias_retry
                            && discovered.alias_calls.contains(&file_id)
                            && file_id != owner.source_file_id();
                        if !propagation_reindexed_files.contains(&file_id) || alias_retry {
                            profile_reference_edges += 1;
                            reference_files.insert(file_id);
                        }
                    }
                }
            }
            if reference_files.is_empty() {
                break;
            }

            let mut reindex_file_ids = reference_files.into_iter().collect::<Vec<_>>();
            reindex_file_ids.sort_unstable();
            let wave_file_ids = reindex_file_ids.iter().copied().collect::<FxHashSet<_>>();
            let old_wave_snapshot = self.inferred_guard_snapshot(&wave_file_ids);
            self.compilation.remove_index(reindex_file_ids.clone());
            let update_file_ids = reindex_file_ids
                .into_iter()
                .filter(|file_id| {
                    self.compilation
                        .get_db()
                        .get_vfs()
                        .get_syntax_tree(file_id)
                        .is_some()
                })
                .collect::<Vec<_>>();
            if update_file_ids.is_empty() {
                break;
            }
            profile_waves += 1;
            profile_reindexed_files += update_file_ids.len();
            propagation_reindexed_files.extend(wave_file_ids.iter().copied());
            self.compilation.update_index(update_file_ids.clone());

            new_facts = self
                .compilation
                .get_db()
                .get_signature_index()
                .inferred_guard_facts_for_files(&wave_file_ids);
            let equivalent_owners = self.reconcile_equivalent_inferred_guard_owners(
                &old_wave_snapshot,
                &new_facts,
                &propagation_reindexed_files,
            );
            changed_owners = old_wave_snapshot
                .facts
                .keys()
                .chain(new_facts.keys())
                .filter(|owner| {
                    !equivalent_owners.contains(*owner)
                        && old_wave_snapshot.facts.get(*owner) != new_facts.get(*owner)
                })
                .cloned()
                .collect::<FxHashSet<_>>()
                .into_iter()
                .collect();
            frontier_old_facts = old_wave_snapshot.facts;
            frontier_old_consumers = old_wave_snapshot.consumers;
            profile_changed_facts += changed_owners.len();
            sort_inferred_guard_owners(&mut changed_owners);
        }
        if profile_enabled {
            eprintln!(
                "[profile] inferred_guard_incremental changed_facts={} reference_edges={} waves={} reindexed_files={}",
                profile_changed_facts,
                profile_reference_edges,
                profile_waves,
                profile_reindexed_files
            );
        }
        #[cfg(test)]
        {
            self.inferred_guard_propagation_stats = InferredGuardPropagationStats {
                changed_facts: profile_changed_facts,
                reference_edges: profile_reference_edges,
                frontiers: profile_waves,
                reindexed_files: profile_reindexed_files,
            };
        }
        propagation_reindexed_files
    }

    pub(crate) fn inferred_guard_snapshot(
        &self,
        file_ids: &FxHashSet<FileId>,
    ) -> InferredGuardSnapshot {
        let signature_index = self.compilation.get_db().get_signature_index();
        let facts = signature_index.inferred_guard_facts_for_files(file_ids);
        let consumers = facts
            .keys()
            .map(|owner| {
                (
                    owner.clone(),
                    signature_index.inferred_guard_consumers(owner).collect(),
                )
            })
            .collect();
        let inferred_params = self
            .compilation
            .get_db()
            .get_call_site_param_index()
            .inferred_params_for_contributor_files(file_ids);
        InferredGuardSnapshot {
            facts,
            consumers,
            inferred_params,
            snapshot_file_ids: file_ids.clone(),
        }
    }

    /// Re-analyses callee files whose call-site-inferred parameter types
    /// changed.
    /// Returns every file this pass re-analysed, including whatever its own
    /// ripple reached.
    pub(crate) fn reindex_changed_inferred_param_consumers(
        &mut self,
        old_snapshot: &InferredGuardSnapshot,
        already_reindexed: &[FileId],
    ) -> Vec<FileId> {
        let new_params = self
            .compilation
            .get_db()
            .get_call_site_param_index()
            .inferred_params_for_contributor_files(&old_snapshot.snapshot_file_ids);
        let old_params = &old_snapshot.inferred_params;
        if old_params.is_empty() && new_params.is_empty() {
            return Vec::new();
        }

        let already_reindexed = already_reindexed
            .iter()
            .copied()
            .chain(old_snapshot.snapshot_file_ids.iter().copied())
            .collect::<FxHashSet<_>>();
        let mut changed_files = old_params
            .keys()
            .chain(new_params.keys())
            .filter(|key| old_params.get(*key) != new_params.get(*key))
            .map(|(signature_id, _)| signature_id.get_file_id())
            .filter(|file_id| !already_reindexed.contains(file_id))
            .collect::<Vec<_>>();
        changed_files.sort_unstable();
        changed_files.dedup();
        if changed_files.is_empty() {
            return Vec::new();
        }

        let changed_files = changed_files
            .into_iter()
            .filter(|file_id| {
                self.compilation
                    .get_db()
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .is_some()
            })
            .collect::<Vec<_>>();
        if changed_files.is_empty() {
            return Vec::new();
        }
        // The parameter's consumers are the bodies that read it, which are the
        // files owning these signatures. Whatever those bodies then derive
        // differently is found by diffing their exports, the same way any
        // other edit ripples; expanding to every dependency re-analysed the
        // workspace for one parameter.
        let mut reanalyzed = changed_files.clone();
        let dirty = self.self_index_and_diff(changed_files);
        reanalyzed.extend(self.ripple(dirty));
        reanalyzed
    }

    pub(crate) fn reconcile_equivalent_inferred_guard_owners(
        &mut self,
        old_snapshot: &InferredGuardSnapshot,
        new_facts: &FxHashMap<LuaInferredGuardOwner, LuaInferredPositiveGuard>,
        reindexed_file_ids: &FxHashSet<FileId>,
    ) -> FxHashSet<LuaInferredGuardOwner> {
        let mut reconciled = FxHashSet::default();
        for owner in old_snapshot
            .facts
            .keys()
            .filter(|owner| old_snapshot.facts.get(*owner) == new_facts.get(*owner))
        {
            if let Some(consumers) = old_snapshot.consumers.get(owner) {
                self.compilation
                    .get_db_mut()
                    .get_signature_index_mut()
                    .migrate_inferred_guard_consumers(owner.clone(), consumers, reindexed_file_ids);
            }
            reconciled.insert(owner.clone());
        }

        let mut old_owners = old_snapshot
            .facts
            .keys()
            .filter(|owner| !new_facts.contains_key(*owner))
            .cloned()
            .collect::<Vec<_>>();
        let mut new_owners = new_facts
            .keys()
            .filter(|owner| !old_snapshot.facts.contains_key(*owner))
            .cloned()
            .collect::<Vec<_>>();
        sort_inferred_guard_owners(&mut old_owners);
        sort_inferred_guard_owners(&mut new_owners);

        for old_owner in old_owners {
            let Some(new_idx) = new_owners.iter().position(|new_owner| {
                old_owner.source_file_id() == new_owner.source_file_id()
                    && old_owner.path() == new_owner.path()
                    && old_owner.state_mask() == new_owner.state_mask()
                    && old_snapshot.facts.get(&old_owner) == new_facts.get(new_owner)
            }) else {
                continue;
            };
            let new_owner = new_owners.remove(new_idx);
            if let Some(consumers) = old_snapshot.consumers.get(&old_owner) {
                self.compilation
                    .get_db_mut()
                    .get_signature_index_mut()
                    .migrate_inferred_guard_consumers(
                        new_owner.clone(),
                        consumers,
                        reindexed_file_ids,
                    );
            }
            reconciled.insert(old_owner);
            reconciled.insert(new_owner);
        }
        reconciled
    }

    fn resolve_inferred_guard_reference_files(
        &self,
        owner: &LuaInferredGuardOwner,
        discover_aliases: bool,
    ) -> InferredGuardReferenceFiles {
        let Some(member_name) = owner.path().last() else {
            return InferredGuardReferenceFiles::default();
        };
        let references = if owner.path().len() == 1 {
            self.compilation
                .get_db()
                .get_reference_index()
                .get_global_references(member_name)
        } else {
            self.compilation
                .get_db()
                .get_reference_index()
                .get_index_references(&LuaMemberKey::Name(member_name.clone()))
        };
        let Some(references) = references else {
            return InferredGuardReferenceFiles::default();
        };

        let db = self.compilation.get_db();
        let mut caches = FxHashMap::<FileId, LuaInferCache>::default();
        let mut matching_references = references
            .into_iter()
            .filter_map(|reference| {
                let root = db
                    .get_vfs()
                    .get_syntax_tree(&reference.file_id)?
                    .get_red_root();
                let expr = LuaExpr::cast(reference.value.to_node_from_root(&root)?)?;
                (global_path_for_expr(&expr).as_deref() == Some(owner.path())
                    && db.get_gmod_infer_index().are_offsets_compatible(
                        &reference.file_id,
                        expr.get_range().start(),
                        &owner.source_file_id(),
                        owner.signature_id().get_position(),
                    ))
                .then_some((reference.file_id, expr))
            })
            .collect::<Vec<_>>();
        matching_references.sort_by_key(|(file_id, expr)| (*file_id, expr.get_range().start()));

        let mut result = InferredGuardReferenceFiles::default();
        let mut alias_queue = VecDeque::new();
        let mut visited_aliases = FxHashSet::default();
        for (file_id, expr) in matching_references {
            if call_resolves_to_inferred_guard_owner(db, &mut caches, owner, file_id, &expr) {
                result.files.insert(file_id);
            }
            if discover_aliases
                && expr_resolves_to_inferred_guard_owner(db, &mut caches, owner, file_id, &expr)
                && let Some(decl_id) = immutable_local_alias_decl(db, file_id, &expr)
            {
                alias_queue.push_back(decl_id);
            }
        }

        while let Some(decl_id) = alias_queue.pop_front() {
            if !visited_aliases.insert(decl_id) {
                continue;
            }
            let Some(root) = db
                .get_vfs()
                .get_syntax_tree(&decl_id.file_id)
                .map(|tree| tree.get_red_root())
            else {
                continue;
            };
            let Some(decl_references) = db
                .get_reference_index()
                .get_decl_references(&decl_id.file_id, &decl_id)
            else {
                continue;
            };
            let mut cells = decl_references.cells.clone();
            cells.sort_by_key(|cell| cell.range.start());
            for cell in cells {
                if cell.is_write {
                    continue;
                }
                let Some(name_expr) = root
                    .covering_element(cell.range)
                    .ancestors()
                    .find_map(LuaNameExpr::cast)
                    .filter(|name_expr| name_expr.get_range() == cell.range)
                else {
                    continue;
                };
                let expr = LuaExpr::NameExpr(name_expr);
                if !db.get_gmod_infer_index().are_offsets_compatible(
                    &decl_id.file_id,
                    expr.get_range().start(),
                    &owner.source_file_id(),
                    owner.signature_id().get_position(),
                ) {
                    continue;
                }
                if is_call_prefix(&expr) {
                    result.files.insert(decl_id.file_id);
                    result.alias_calls.insert(decl_id.file_id);
                }
                if let Some(next_decl_id) = immutable_local_alias_decl(db, decl_id.file_id, &expr) {
                    alias_queue.push_back(next_decl_id);
                }
            }
        }

        result
    }
}
