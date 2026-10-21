mod evidence;
mod solver;

use std::{cmp::Ordering, collections::HashMap, sync::Arc};

use rustc_hash::FxHashMap;

use glua_parser::{
    BinaryOperator, LuaAssignStat, LuaAstNode, LuaBinaryExpr, LuaCallExpr, LuaClosureExpr,
    LuaElseIfClauseStat, LuaExpr, LuaFuncStat, LuaIfStat, LuaIndexExpr, LuaLocalStat, LuaNameExpr,
    LuaParenExpr, LuaRepeatStat, LuaReturnStat, LuaSyntaxNode, LuaVarExpr, LuaWhileStat,
};
use rustc_hash::FxHashSet;

use crate::{
    FlowNodeKind, InFiled, LuaDefinitionId, LuaInferenceConfidence, LuaInferenceEventId,
    LuaInferenceNodeId, LuaInferenceProvenanceKind, LuaInferenceStep, LuaInferredGuardOwner,
    LuaInferredPositiveGuard, LuaMemberKey, LuaMemberOwner, LuaSignatureId, LuaType, LuaTypeDeclId,
    LuaTypeFact, SignatureReturnStatus, TypeOps, check_type_compact,
    compilation::analyzer::AnalyzeContext,
    semantic::{
        LuaInferCache, VarRefId, VarRefRootId, dynamic_field_type_without, get_var_expr_var_ref_id,
        get_var_ref_type, infer_bind_value_type, infer_expr, infer_true_condition_narrowing,
        infer_var_type_at_node, resolve_dynamic_field_member, type_name_test,
    },
};

use self::evidence::ContextualTypeEvidence;

pub(super) fn stabilize_unknown_locals(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
) -> bool {
    let _profile =
        crate::profile::Profile::cond_new("local inference stabilize", context.tree_list.len() > 1);
    let mut candidates = Vec::new();
    for tree in &context.tree_list {
        let file_id = tree.file_id;
        candidates.extend(
            guessed_decls(db, file_id)
                .map(|(decl_id, references)| (file_id, *decl_id, references.clone())),
        );
    }
    candidates.sort_by_key(|(_, decl_id, _)| (decl_id.file_id, decl_id.position));
    rederive_settled_initializers(db, context, &mut candidates);

    let mut evidence_by_node =
        FxHashMap::<LuaInferenceNodeId, Vec<ContextualTypeEvidence>>::default();
    for (file_id, decl_id, references) in candidates {
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let flow_tree = db.get_flow_index().get_flow_tree(&file_id);
        let mut cells = references.cells;
        cells.sort_by_key(|cell| cell.range.start());
        // One read the value could not survive as nil settles it for the whole
        // declaration: reaching that read at all means no assignment left a nil
        // behind. Without this the local keeps a `nil` it only ever picked up
        // from the slots its other reads feed -- `draw.SimpleText`'s `number?`
        // parameter -- and every use then wants a guard against it.
        let proven_non_nil = cells.iter().any(|cell| {
            !cell.is_write
                && root
                    .covering_element(cell.range)
                    .ancestors()
                    .find_map(LuaNameExpr::cast)
                    .filter(|name| name.get_range() == cell.range)
                    .is_some_and(|name| read_would_error_on_nil(&name))
        });
        for cell in cells {
            let Some(name_expr) = root
                .covering_element(cell.range)
                .ancestors()
                .find_map(LuaNameExpr::cast)
                .filter(|name| name.get_range() == cell.range)
            else {
                continue;
            };
            let is_function_definition = cell.is_write
                && name_expr
                    .ancestors::<glua_parser::LuaFuncStat>()
                    .next()
                    .is_some_and(|function| {
                        function
                            .get_func_name()
                            .is_some_and(|name| name.syntax() == name_expr.syntax())
                    });
            if cell.is_write && !is_function_definition {
                continue;
            }
            let expr: LuaExpr = LuaVarExpr::NameExpr(name_expr.clone()).into();
            let Some(candidate) =
                infer_bind_value_type(db, context.infer_manager.get_infer_cache(file_id), expr)
            else {
                continue;
            };
            let cache = context.infer_manager.get_infer_cache(file_id);
            let candidate = match guessed_slot_evidence(db, cache, file_id, &name_expr) {
                Some(Some(evidence)) => evidence,
                Some(None) => continue,
                None => candidate,
            };
            let candidate = if proven_non_nil {
                TypeOps::Remove.apply(db, &candidate, &LuaType::Nil)
            } else {
                candidate
            };
            if super::type_is_uninformative(&candidate) {
                continue;
            }
            let definitions = if is_function_definition {
                Arc::from([crate::LuaDefinitionId::Declaration(decl_id)])
            } else {
                flow_tree
                    .and_then(|tree| {
                        tree.get_flow_id(name_expr.get_syntax_id())
                            .map(|flow| (tree, flow))
                    })
                    .map(|(tree, flow)| tree.reaching_definitions(decl_id, flow))
                    .unwrap_or_else(|| Arc::from([crate::LuaDefinitionId::Declaration(decl_id)]))
            };
            for definition in definitions.iter().cloned() {
                let target = LuaInferenceNodeId::Definition(definition);
                evidence_by_node.entry(target.clone()).or_default().push(
                    ContextualTypeEvidence::anchored(
                        target,
                        candidate.clone(),
                        InFiled::new(file_id, name_expr.get_syntax_id()),
                        contextual_type_support(db, &candidate),
                    ),
                );
            }
        }
    }

    let evidence_count = evidence_by_node.values().map(Vec::len).sum::<usize>();
    let solved = solver::solve_local_inference_graph(&evidence_by_node);
    log::debug!(
        "local inference: candidates={} sccs={} resolved={} unresolved={}",
        solved.stats.nodes,
        solved.stats.sccs,
        solved.stats.resolved,
        solved.stats.unresolved
    );
    let changed = db.publish_inference_facts(solved.facts);
    let changed_any = !changed.is_empty();
    if crate::profile::phase_enabled() {
        eprintln!(
            "[profile] local_inference candidates={} evidence={} sccs={} resolved={} unresolved={} changed_files={}",
            solved.stats.nodes,
            evidence_count,
            solved.stats.sccs,
            solved.stats.resolved,
            solved.stats.unresolved,
            changed.len()
        );
    }
    if changed_any {
        context.infer_manager.clear();
    }
    changed_any
}

/// The declarations of `file_id` this pass guesses, with their references.
fn guessed_decls(
    db: &crate::DbIndex,
    file_id: crate::FileId,
) -> impl Iterator<Item = (&crate::LuaDeclId, &crate::DeclReference)> {
    db.get_reference_index()
        .get_decl_references_map(&file_id)
        .into_iter()
        .flatten()
        .filter(|(decl_id, _)| guesses(db, decl_id))
}

/// Whether this pass guesses `decl_id`'s type from its uses: a local the
/// statements left without a type, or one holding a guess it published before.
fn guesses(db: &crate::DbIndex, decl_id: &crate::LuaDeclId) -> bool {
    let is_local = db
        .get_decl_index()
        .get_decl(decl_id)
        .is_some_and(|decl| matches!(decl.extra, crate::LuaDeclExtra::Local { .. }));
    if !is_local {
        return false;
    }
    // `never` is the bottom of the same uninformative band as `unknown` (see
    // `LuaTypeCache::supersedes`), and it is what an initialiser resolves to
    // when the member it reads is not in the index *yet*.
    let uninformative = db
        .get_type_index()
        .get_type_cache(&(*decl_id).into())
        .is_none_or(|cache| {
            cache.is_infer() && matches!(cache.as_type(), LuaType::Unknown | LuaType::Never)
        });
    // A fact this pass published earlier was read off whatever the slots its
    // uses feed held at the time; a later run re-derives it against what they
    // hold now.
    uninformative
        || db
            .get_type_index()
            .holds_contextual_fallback(&(*decl_id).into())
}

/// What a store `t.k = local` says of the local when some writer of `t.k` in
/// the same file stores a local this pass guesses: the type the slot's other
/// writers give it, or nothing when there are none. The slot holds those
/// guesses, the stored local's own included, so reading it back would derive
/// each guess from the guesses. A writer in another file is taken as it
/// stands: telling what it stores would read that file's text, so every edit
/// of it would reach this file. A store through an instance files no member
/// of the slot: its writers are the field's dynamic ones. `None` when the
/// read is no such store.
fn guessed_slot_evidence(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    file_id: crate::FileId,
    name_expr: &LuaNameExpr,
) -> Option<Option<LuaType>> {
    let assign = name_expr.get_parent::<LuaAssignStat>()?;
    let (vars, exprs) = assign.get_var_and_expr_list();
    let at = exprs
        .iter()
        .position(|expr| expr.syntax() == name_expr.syntax())?;
    let LuaVarExpr::IndexExpr(target) = vars.get(at)? else {
        return None;
    };
    let member_index = db.get_member_index();
    let member_id = crate::LuaMemberId::new(target.get_syntax_id(), file_id);
    let key = member_index.get_member(&member_id)?.get_key().clone();
    let owner = member_index.get_current_owner(&member_id)?.clone();
    let Some(item) = member_index
        .get_member_item(&owner, &key)
        .filter(|item| item.get_member_ids().contains(&member_id))
    else {
        let prefix_type = infer_expr(db, cache, target.get_prefix_expr()?).ok()?;
        return dynamic_field_type_without(
            db,
            cache,
            &prefix_type,
            key.get_name()?,
            name_expr.get_position(),
            |writer| writer.file_id == file_id && stores_guessed_local(db, writer),
        );
    };
    let (guessed, others): (Vec<_>, Vec<_>) = item
        .get_member_ids()
        .into_iter()
        .partition(|writer| writer.file_id == file_id && stores_guessed_local(db, *writer));
    if guessed.is_empty() {
        return None;
    }
    let others = match others.as_slice() {
        [] => return Some(None),
        [writer] => crate::LuaMemberIndexItem::One(*writer),
        _ => crate::LuaMemberIndexItem::Many(others),
    };
    Some(
        others
            .resolve_type_with_realm_at_offset(db, &file_id, name_expr.get_position())
            .ok(),
    )
}

/// Whether member `writer` is a store `t.k = local` of the value a local this
/// pass guesses was declared with: only its declaration reaches the store.
fn stores_guessed_local(db: &crate::DbIndex, writer: crate::LuaMemberId) -> bool {
    let syntax_id = writer.get_syntax_id();
    let Some(index_expr) = db
        .get_vfs()
        .syntax_tree_at_statement(&writer.file_id, syntax_id.get_range())
        .and_then(|tree| syntax_id.to_node_from_root(&tree.get_red_root()))
        .and_then(LuaIndexExpr::cast)
    else {
        return false;
    };
    let Some(assign) = index_expr.get_parent::<LuaAssignStat>() else {
        return false;
    };
    let (vars, exprs) = assign.get_var_and_expr_list();
    let value = vars
        .iter()
        .position(|var| var.syntax() == index_expr.syntax())
        .and_then(|at| exprs.get(at));
    let Some(LuaExpr::NameExpr(value)) = value else {
        return false;
    };
    let Some(decl_id) = db
        .get_reference_index()
        .get_local_reference(&writer.file_id)
        .and_then(|references| references.get_decl_id(&value.get_range()))
    else {
        return false;
    };
    guesses(db, &decl_id)
        && db
            .get_flow_index()
            .get_flow_tree(&writer.file_id)
            .and_then(|tree| {
                let flow = tree.get_flow_id(value.get_syntax_id())?;
                Some(tree.reaching_definitions(decl_id, flow))
            })
            .is_some_and(|definitions| {
                definitions.as_ref() == [crate::LuaDefinitionId::Declaration(decl_id)]
            })
}

/// Whether reaching this read with a nil value would be a runtime error.
///
/// Arithmetic, concatenation and length take a value apart; indexing and calling
/// dereference it. Each errors on nil, so the read stands as proof the value is
/// not nil. Every other position -- an argument, a return, the right side of an
/// assignment -- passes the value along and proves nothing.
fn read_would_error_on_nil(name_expr: &LuaNameExpr) -> bool {
    let Some(parent) = name_expr.syntax().parent() else {
        return false;
    };
    if let Some(binary) = LuaBinaryExpr::cast(parent.clone()) {
        return binary.get_op_token().is_some_and(|token| {
            matches!(
                token.get_op(),
                BinaryOperator::OpAdd
                    | BinaryOperator::OpSub
                    | BinaryOperator::OpMul
                    | BinaryOperator::OpDiv
                    | BinaryOperator::OpIDiv
                    | BinaryOperator::OpMod
                    | BinaryOperator::OpPow
                    | BinaryOperator::OpConcat
            )
        });
    }
    if let Some(unary) = glua_parser::LuaUnaryExpr::cast(parent.clone()) {
        return unary.get_op_token().is_some_and(|token| {
            matches!(
                token.get_op(),
                glua_parser::UnaryOperator::OpUnm | glua_parser::UnaryOperator::OpLen
            )
        });
    }
    // The prefix of an index or a call is dereferenced; an argument is not.
    if let Some(index) = LuaIndexExpr::cast(parent.clone()) {
        return index
            .get_prefix_expr()
            .is_some_and(|prefix| prefix.syntax() == name_expr.syntax());
    }
    LuaCallExpr::cast(parent).is_some_and(|call| {
        call.get_prefix_expr()
            .is_some_and(|prefix| prefix.syntax() == name_expr.syntax())
    })
}

/// Re-derives what each candidate's own initializer says before anything is
/// guessed from how it is used.
///
/// Inferring from usage context is the fallback, so it only applies to a value
/// the analyzer genuinely cannot derive. The unresolve pass reaches its answer
/// in waves and retires an item after a fixed number of them, so a chain like
/// `local w = frame:GetWide()` / `local x = w - 1` can leave `x` parked at the
/// placeholder `w` had when `x` was last retried, even though `w` settled
/// afterwards. Asking the initializer again here costs one inference per
/// candidate and removes the guess entirely where the value was derivable.
///
/// Candidates arrive in source order, and this binds as it walks, so a chain
/// settles front to back in a single pass.
fn rederive_settled_initializers(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
    candidates: &mut Vec<(crate::FileId, crate::LuaDeclId, crate::DeclReference)>,
) {
    let mut roots = FxHashMap::<crate::FileId, glua_parser::LuaSyntaxNode>::default();
    candidates.retain(|(file_id, decl_id, _)| {
        let root = match roots.entry(*file_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                let Some(root) = db
                    .get_vfs()
                    .get_syntax_tree(file_id)
                    .map(|tree| tree.get_red_root())
                else {
                    return true;
                };
                entry.insert(root)
            }
        };
        let Some((ret_idx, expr)) =
            crate::compilation::analyzer::local_initializer_expr(db, root, *decl_id)
        else {
            return true;
        };
        // Initializers that read through a call or index — including the
        // `x = y or {}` guard — have their own reconciliation pass, which
        // carries policy this cannot see. Only the operator shape is re-asked
        // here, because nothing else re-derives it.
        if !crate::compilation::analyzer::initializer_is_operator_expr(&expr)
            || crate::compilation::analyzer::initializer_reads_through_call_or_index(&expr)
        {
            return true;
        }
        let cache = context.infer_manager.get_infer_cache(*file_id);
        let Ok(typ) = infer_expr(db, cache, expr) else {
            return true;
        };
        let typ = match &typ {
            LuaType::Variadic(multi) => match multi.get_type(ret_idx) {
                Some(typ) => typ.clone(),
                None => return true,
            },
            _ => typ,
        };
        if !crate::db_index::is_informative_type(&typ) {
            return true;
        }
        crate::compilation::analyzer::common::bind_resolved_type(
            db,
            (*decl_id).into(),
            crate::LuaTypeCache::InferType(typ),
        );
        false
    });
}

fn contextual_type_support(
    db: &crate::DbIndex,
    candidate: &crate::LuaType,
) -> Arc<[LuaInferenceNodeId]> {
    let type_decl_id = match candidate {
        crate::LuaType::Ref(id) | crate::LuaType::Def(id) => id,
        _ => return Arc::from([]),
    };
    let Some(type_decl) = db.get_type_index().get_type_decl(type_decl_id) else {
        return Arc::from([]);
    };
    let mut support = type_decl
        .get_locations()
        .iter()
        .map(|location| {
            LuaInferenceNodeId::TypeOwner(crate::LuaTypeOwner::SyntaxId(InFiled::new(
                location.file_id,
                glua_parser::LuaSyntaxId::new(
                    glua_parser::LuaSyntaxKind::DocTagClass.into(),
                    location.range,
                ),
            )))
        })
        .collect::<Vec<_>>();
    support.sort_by(LuaInferenceNodeId::stable_cmp);
    support.dedup();
    support.into()
}

/// Infers a known direct child from several otherwise-invalid member uses of the
/// same reaching definition. This is deliberately distinct from generic unknown
/// stabilization: it only starts from a concrete parent reference type.
#[derive(Default)]
struct UnguardedChildProfile {
    subtype_index: std::time::Duration,
    reference_scan: std::time::Duration,
    publish: std::time::Duration,
    references_scanned: usize,
    assignment_targets_skipped: usize,
    conditions_skipped: usize,
    short_circuit_guards_skipped: usize,
    evidence_sites: usize,
}

impl UnguardedChildProfile {
    fn absorb(&mut self, other: Self) {
        self.references_scanned += other.references_scanned;
        self.assignment_targets_skipped += other.assignment_targets_skipped;
        self.conditions_skipped += other.conditions_skipped;
        self.short_circuit_guards_skipped += other.short_circuit_guards_skipped;
        self.evidence_sites += other.evidence_sites;
    }
}

struct UnguardedChildCandidates {
    parent_type: LuaType,
    children: HashMap<crate::LuaTypeDeclId, FxHashSet<LuaMemberKey>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct NestedUnguardedChildTarget {
    root_decl_id: crate::LuaDeclId,
    path: Vec<LuaMemberKey>,
}

struct NestedUnguardedChildCandidates {
    parent_type: LuaType,
    children: HashMap<crate::LuaTypeDeclId, FxHashSet<LuaMemberKey>>,
    receivers: FxHashSet<InFiled<glua_parser::LuaSyntaxId>>,
    sources: HashMap<crate::LuaTypeDeclId, ChildSource>,
}

/// The read reported for `children`: the one their own evidence prefers.
fn winning_source<'a>(
    sources: &'a HashMap<crate::LuaTypeDeclId, ChildSource>,
    children: &[crate::LuaTypeDeclId],
) -> Option<&'a ChildSource> {
    children
        .iter()
        .filter_map(|child| sources.get(child))
        .min_by_key(|source| (source.guarded, source.site.value.get_range().start()))
}

impl NestedUnguardedChildCandidates {
    fn earliest_site(&self) -> Option<&InFiled<glua_parser::LuaSyntaxId>> {
        self.sources
            .values()
            .map(|source| &source.site)
            .min_by_key(|site| in_filed_order(site))
    }
}

fn record_child_source(
    sources: &mut HashMap<crate::LuaTypeDeclId, ChildSource>,
    child: crate::LuaTypeDeclId,
    source: ChildSource,
) {
    sources
        .entry(child)
        .and_modify(|existing| {
            if source.precedes(existing) {
                *existing = source.clone();
            }
        })
        .or_insert(source);
}

fn compare_unguarded_child_candidates(
    left: &LuaTypeDeclId,
    left_display_name: &str,
    right: &LuaTypeDeclId,
    right_display_name: &str,
) -> Ordering {
    left_display_name
        .cmp(right_display_name)
        .then_with(|| left.get_name().cmp(right.get_name()))
        .then_with(|| left.stable_cmp(right))
}

/// A candidate also has the members its candidate ancestors declare, which a
/// nearest-declarer lookup leaves out of its own set.
fn credit_inherited_members(
    db: &crate::DbIndex,
    children: &mut HashMap<crate::LuaTypeDeclId, FxHashSet<LuaMemberKey>>,
) {
    let mut inherited = Vec::new();
    for child_id in children.keys() {
        let mut supers = Vec::new();
        child_id.collect_super_types(db, &mut supers);
        for super_type in supers {
            if let LuaType::Ref(super_id) = super_type
                && let Some(keys) = children.get(&super_id)
            {
                inherited.push((child_id.clone(), keys.clone()));
            }
        }
    }
    for (child_id, keys) in inherited {
        if let Some(own) = children.get_mut(&child_id) {
            own.extend(keys);
        }
    }
}

/// `children` and the supers whose evidence they are credited with, whose reads are
/// evidence for them too.
fn with_credited_supers(
    db: &crate::DbIndex,
    children: &[crate::LuaTypeDeclId],
) -> Vec<crate::LuaTypeDeclId> {
    let mut lineage = children.to_vec();
    for child in children {
        let mut supers = Vec::new();
        child.collect_super_types(db, &mut supers);
        lineage.extend(
            supers
                .into_iter()
                .filter_map(|super_type| match super_type {
                    LuaType::Ref(super_id) => Some(super_id),
                    _ => None,
                }),
        );
    }
    lineage
}

fn in_filed_order(value: &InFiled<glua_parser::LuaSyntaxId>) -> (u32, u32, u32) {
    let range = value.value.get_range();
    (
        value.file_id.id,
        u32::from(range.start()),
        u32::from(range.end()),
    )
}

/// Declared member types already looked up in this pass. `infer_raw_member_type`
/// reads the member index, so the answer only depends on the pair, and a member
/// path repeats at every use of the declaration it hangs off.
type DeclaredPathBases = FxHashMap<(LuaType, LuaMemberKey), Option<(LuaType, LuaTypeDeclId)>>;

/// What every member path rooted at one declaration shares, plus the
/// file-local accumulators used while collecting it.
struct MemberPathEvidence<'a> {
    file_id: crate::FileId,
    root_decl_id: crate::LuaDeclId,
    declared_root_type: Option<&'a LuaType>,
    declared_path_bases: &'a mut DeclaredPathBases,
    subtypes_with_member: &'a mut SubtypesWithMember,
    scores: &'a mut HashMap<NestedUnguardedChildTarget, NestedUnguardedChildCandidates>,
}

/// Evidence for a declaration whose type is a union of classes: a read of a
/// member only some of them have names those classes as what it holds.
struct UnionArmEvidence<'a> {
    file_id: crate::FileId,
    flow_tree: Option<&'a crate::FlowTree>,
    decl_id: crate::LuaDeclId,
    base_type: &'a LuaType,
    arms: &'a [LuaTypeDeclId],
    subtypes_with_member: &'a mut SubtypesWithMember,
    scores: &'a mut HashMap<LuaDefinitionId, UnguardedChildCandidates>,
    sources: &'a mut HashMap<(LuaDefinitionId, crate::LuaTypeDeclId), ChildSource>,
    found_types: &'a mut HashMap<InFiled<glua_parser::LuaSyntaxId>, LuaType>,
}

impl UnionArmEvidence<'_> {
    fn collect(
        &mut self,
        db: &crate::DbIndex,
        cache: &mut crate::LuaInferCache,
        name_expr: &LuaNameExpr,
        index_expr: &LuaIndexExpr,
    ) {
        if is_assignment_target(index_expr)
            || is_condition_evidence(index_expr)
            || is_matching_short_circuit_guard(db, cache, index_expr)
        {
            return;
        }
        let Some(index_key) = index_expr.get_index_key() else {
            return;
        };
        if LuaMemberKey::index_key_is_dynamic(db, cache, &index_key) {
            return;
        }
        let Ok(member_key) = LuaMemberKey::from_index_key(db, cache, &index_key) else {
            return;
        };
        let Some(with_member) = class_union_partial_arms(
            db,
            cache,
            self.subtypes_with_member,
            self.arms,
            &member_key,
            self.file_id,
            index_expr,
        ) else {
            return;
        };
        if member_path_assigned_before(db, cache, self.flow_tree, index_expr) {
            return;
        }
        // A real flow guard wins, as it does for a single declared class.
        let current =
            infer_expr(db, cache, LuaExpr::NameExpr(name_expr.clone())).unwrap_or(LuaType::Unknown);
        if &current != self.base_type {
            return;
        }
        let definitions = self
            .flow_tree
            .and_then(|tree| {
                tree.get_flow_id(name_expr.get_syntax_id())
                    .map(|flow| tree.reaching_definitions(self.decl_id, flow))
            })
            .unwrap_or_else(|| Arc::from([LuaDefinitionId::Declaration(self.decl_id)]));
        let site = InFiled::new(self.file_id, index_expr.get_syntax_id());
        self.found_types.insert(site.clone(), current);
        let source = ChildSource {
            site,
            guarded: is_presence_covered(db, cache, index_expr),
        };
        for definition in definitions.iter() {
            for arm in &with_member {
                self.scores
                    .entry(definition.clone())
                    .or_insert_with(|| UnguardedChildCandidates {
                        parent_type: self.base_type.clone(),
                        children: HashMap::default(),
                    })
                    .children
                    .entry(arm.clone())
                    .or_default()
                    .insert(member_key.clone());
                self.sources
                    .entry((definition.clone(), arm.clone()))
                    .and_modify(|existing| {
                        if source.precedes(existing) {
                            *existing = source.clone();
                        }
                    })
                    .or_insert_with(|| source.clone());
            }
        }
    }
}

/// A read a child inference is reported at, with whether a guard on a member of
/// the same table encloses it.
#[derive(Clone)]
struct ChildSource {
    site: InFiled<glua_parser::LuaSyntaxId>,
    guarded: bool,
}

impl ChildSource {
    /// An unguarded read is reported before a guarded one, then the earliest.
    fn precedes(&self, other: &Self) -> bool {
        (self.guarded, self.site.value.get_range().start())
            < (other.guarded, other.site.value.get_range().start())
    }
}

pub(crate) struct UnguardedChildFileResult {
    scores: HashMap<LuaDefinitionId, UnguardedChildCandidates>,
    deferred_definitions: FxHashSet<LuaDefinitionId>,
    nested_scores: HashMap<NestedUnguardedChildTarget, NestedUnguardedChildCandidates>,
    sources: HashMap<(LuaDefinitionId, crate::LuaTypeDeclId), ChildSource>,
    found_types: HashMap<InFiled<glua_parser::LuaSyntaxId>, LuaType>,
    initializer_refinements: HashMap<crate::LuaDeclId, LuaType>,
    profile: Option<UnguardedChildProfile>,
}

/// What one file's local-fallback derivation settles.
pub(crate) struct FileScan {
    evidence: Option<UnguardedChildFileResult>,
    idle: bool,
}

impl FileScan {
    /// A scan of a run that does not guess locals yet.
    pub(crate) fn skipped() -> Self {
        Self {
            evidence: None,
            idle: false,
        }
    }

    /// Whether the file has no evidence to publish and no local to guess, so
    /// settling it writes nothing.
    pub(crate) fn is_idle(&self) -> bool {
        self.idle
    }

    pub(crate) fn into_evidence(self) -> Option<UnguardedChildFileResult> {
        self.evidence
    }
}

/// Scans one file, noting whether settling it would write nothing.
pub(crate) fn scan_file(db: &crate::DbIndex, file_id: crate::FileId) -> FileScan {
    let evidence = scan_unguarded_children(db, file_id);
    let idle = evidence
        .as_ref()
        .is_some_and(UnguardedChildFileResult::is_empty)
        && guessed_decls(db, file_id).next().is_none();
    FileScan { evidence, idle }
}

/// One file's unguarded-child evidence, read off the index under the view a
/// file derivation infers with; [`publish_unguarded_children`] settles it.
fn scan_unguarded_children(
    db: &crate::DbIndex,
    file_id: crate::FileId,
) -> Option<UnguardedChildFileResult> {
    if !db.get_emmyrc().gmod.enabled {
        return None;
    }
    let infer_cache = crate::LuaInferCache::new(
        file_id,
        crate::CacheOptions {
            analysis_phase: crate::LuaAnalysisPhase::Force,
            dynamic_fields_visible: true,
            call_site_params_visible: true,
        },
    );
    Some(collect_unguarded_child_file(
        db,
        file_id,
        infer_cache,
        db.get_module_index().get_workspace_id(file_id),
        crate::profile::phase_enabled(),
    ))
}

impl UnguardedChildFileResult {
    fn is_empty(&self) -> bool {
        self.scores.is_empty()
            && self.deferred_definitions.is_empty()
            && self.nested_scores.is_empty()
            && self.initializer_refinements.is_empty()
    }
}

/// Settles a file's unguarded-child evidence into published facts.
pub(crate) fn publish_unguarded_children(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
    result: UnguardedChildFileResult,
) {
    let mut profile = crate::profile::phase_enabled().then(UnguardedChildProfile::default);
    let UnguardedChildFileResult {
        mut scores,
        deferred_definitions,
        nested_scores,
        mut sources,
        found_types,
        initializer_refinements,
        profile: file_profile,
    } = result;
    if let (Some(profile), Some(file_profile)) = (&mut profile, file_profile) {
        profile.absorb(file_profile);
    }
    if !deferred_definitions.is_empty() {
        scores.retain(|definition, _| !deferred_definitions.contains(definition));
        sources.retain(|(definition, _), _| !deferred_definitions.contains(definition));
    }

    let mut updates = Vec::new();
    // Both score maps are hashed, and their entries decide the order facts are
    // published and reported in.
    let mut scores = scores.into_iter().collect::<Vec<_>>();
    scores.sort_by(|(left, _), (right, _)| left.stable_cmp(right));
    for (definition, candidates) in scores {
        let parent_type = candidates.parent_type;
        let mut candidates = candidates.children;
        credit_inherited_members(db, &mut candidates);
        let Some(max_score) = candidates.values().map(FxHashSet::len).max() else {
            continue;
        };
        let mut winners = candidates
            .into_iter()
            .filter_map(|(child_id, keys)| (keys.len() == max_score).then_some(child_id))
            .collect::<Vec<_>>();
        winners.sort_by(|left, right| {
            let left_display_name = db
                .get_type_index()
                .get_type_decl(left)
                .map(|decl| decl.get_name())
                .unwrap_or_else(|| left.get_name());
            let right_display_name = db
                .get_type_index()
                .get_type_decl(right)
                .map(|decl| decl.get_name())
                .unwrap_or_else(|| right.get_name());
            compare_unguarded_child_candidates(left, left_display_name, right, right_display_name)
        });
        let Some(source) = with_credited_supers(db, &winners)
            .into_iter()
            .filter_map(|child| sources.get(&(definition.clone(), child)))
            .min_by_key(|source| (source.guarded, source.site.value.get_range().start()))
            .map(|source| source.site.clone())
        else {
            continue;
        };
        let found_type = found_types.get(&source).cloned().unwrap_or(parent_type);
        let node = LuaInferenceNodeId::Definition(definition);
        // Every definition that reaches the same use files its fact under one
        // event there, so the use is reported once.
        let event = LuaInferenceEventId {
            node: LuaInferenceNodeId::TypeOwner(crate::LuaTypeOwner::SyntaxId(source.clone())),
            kind: LuaInferenceProvenanceKind::UnguardedChild,
            source: source.clone(),
        };
        let typ = LuaType::from_inferred_vec(winners.iter().cloned().map(LuaType::Ref).collect());
        let mut support = Vec::new();
        for child in &winners {
            support.extend(
                contextual_type_support(db, &LuaType::Ref(child.clone()))
                    .iter()
                    .cloned(),
            );
        }
        support.sort_by(LuaInferenceNodeId::stable_cmp);
        support.dedup();
        let mut provenance = db
            .get_inference_fact(&node)
            .map(|fact| {
                fact.provenance()
                    .iter()
                    .filter(|step| step.event.kind == LuaInferenceProvenanceKind::ContextualUnknown)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        provenance.push(LuaInferenceStep {
            event,
            support: support.into(),
            inferred_type: Some(Arc::new(typ.clone())),
            found_type: Some(Arc::new(found_type)),
        });
        updates.push((
            node,
            LuaTypeFact::new(typ, LuaInferenceConfidence::Heuristic, provenance.into()),
        ));
    }

    let mut nested_scores = nested_scores.into_values().collect::<Vec<_>>();
    nested_scores.sort_by_key(|candidates| candidates.earliest_site().map(in_filed_order));
    for candidates in nested_scores {
        let found_type = candidates.parent_type;
        let mut children = candidates.children;
        credit_inherited_members(db, &mut children);
        let Some(max_score) = children.values().map(FxHashSet::len).max() else {
            continue;
        };
        let mut winners = children
            .into_iter()
            .filter_map(|(child_id, keys)| (keys.len() == max_score).then_some(child_id))
            .collect::<Vec<_>>();
        winners.sort_by(|left, right| {
            let left_display_name = db
                .get_type_index()
                .get_type_decl(left)
                .map(|decl| decl.get_name())
                .unwrap_or_else(|| left.get_name());
            let right_display_name = db
                .get_type_index()
                .get_type_decl(right)
                .map(|decl| decl.get_name())
                .unwrap_or_else(|| right.get_name());
            compare_unguarded_child_candidates(left, left_display_name, right, right_display_name)
        });
        let typ = LuaType::from_inferred_vec(winners.iter().cloned().map(LuaType::Ref).collect());
        let mut support = Vec::new();
        for child in &winners {
            support.extend(
                contextual_type_support(db, &LuaType::Ref(child.clone()))
                    .iter()
                    .cloned(),
            );
        }
        support.sort_by(LuaInferenceNodeId::stable_cmp);
        support.dedup();
        let Some(source) = winning_source(&candidates.sources, &with_credited_supers(db, &winners))
            .map(|source| source.site.clone())
        else {
            continue;
        };
        let mut receivers = candidates.receivers.into_iter().collect::<Vec<_>>();
        receivers.sort_by_key(in_filed_order);
        let nodes = receivers
            .into_iter()
            .map(|receiver| LuaInferenceNodeId::TypeOwner(crate::LuaTypeOwner::SyntaxId(receiver)));
        let event_node =
            LuaInferenceNodeId::TypeOwner(crate::LuaTypeOwner::SyntaxId(source.clone()));
        for node in nodes {
            let event = LuaInferenceEventId {
                node: event_node.clone(),
                kind: LuaInferenceProvenanceKind::UnguardedChild,
                source: source.clone(),
            };
            updates.push((
                node,
                LuaTypeFact::new(
                    typ.clone(),
                    LuaInferenceConfidence::Heuristic,
                    Arc::from([LuaInferenceStep {
                        event,
                        support: support.clone().into(),
                        inferred_type: Some(Arc::new(typ.clone())),
                        found_type: Some(Arc::new(found_type.clone())),
                    }]),
                ),
            ));
        }
    }

    let mut refinement_changed_files = FxHashSet::default();
    for (decl_id, initializer_type) in initializer_refinements {
        if db.get_decl_index().get_decl(&decl_id).is_none() {
            continue;
        }
        super::common::write_type_cache(
            db,
            decl_id.into(),
            crate::LuaTypeCache::InferType(initializer_type),
            super::common::TypeCacheWriteMode::ForceOverwrite,
        );
        refinement_changed_files.insert(decl_id.file_id);
    }

    let updates_len = updates.len();
    let publish_start = profile.as_ref().map(|_| std::time::Instant::now());
    let mut changed = db.publish_inference_facts(updates);
    changed.extend(refinement_changed_files.iter().copied());
    if let (Some(profile), Some(start)) = (&mut profile, publish_start) {
        profile.publish = start.elapsed();
    }
    let changed_any = !changed.is_empty();
    if changed_any {
        context.infer_manager.clear();
    }
    if let Some(profile) = profile {
        eprintln!(
            "[profile] unguarded_child subtype_index={:?} reference_scan={:?} publish={:?} references={} assignment_targets_skipped={} conditions_skipped={} short_circuit_guards_skipped={} evidence={} facts={} changed_files={}",
            profile.subtype_index,
            profile.reference_scan,
            profile.publish,
            profile.references_scanned,
            profile.assignment_targets_skipped,
            profile.conditions_skipped,
            profile.short_circuit_guards_skipped,
            profile.evidence_sites,
            updates_len,
            changed.len(),
        );
    }
}

fn collect_unguarded_child_file(
    db: &crate::DbIndex,
    file_id: crate::FileId,
    mut infer_cache: crate::LuaInferCache,
    workspace_id: Option<crate::WorkspaceId>,
    collect_profile: bool,
) -> UnguardedChildFileResult {
    let mut scores = HashMap::<LuaDefinitionId, UnguardedChildCandidates>::default();
    let mut deferred_definitions = FxHashSet::<LuaDefinitionId>::default();
    let mut nested_scores =
        HashMap::<NestedUnguardedChildTarget, NestedUnguardedChildCandidates>::default();
    let mut sources = HashMap::<(LuaDefinitionId, crate::LuaTypeDeclId), ChildSource>::new();
    let mut found_types = HashMap::<InFiled<glua_parser::LuaSyntaxId>, LuaType>::default();
    let mut initializer_refinements = HashMap::<crate::LuaDeclId, LuaType>::default();
    let mut declared_path_bases = DeclaredPathBases::default();
    let mut subtypes_with_member = SubtypesWithMember::default();
    let mut profile = collect_profile.then(UnguardedChildProfile::default);

    let references = db.get_reference_index().get_decl_references_map(&file_id);
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_red_root());
    let (Some(references), Some(root)) = (references, root) else {
        return UnguardedChildFileResult {
            scores,
            deferred_definitions,
            nested_scores,
            sources,
            found_types,
            initializer_refinements,
            profile,
        };
    };
    let flow_tree = db.get_flow_index().get_flow_tree(&file_id);

    let mut decl_ids = references.keys().copied().collect::<Vec<_>>();
    decl_ids.sort_by_key(|decl_id| decl_id.position);
    for decl_id in decl_ids {
        let all_sites = references[&decl_id]
            .cells
            .iter()
            .filter(|cell| !cell.is_write)
            .filter_map(|cell| {
                let name_expr = root
                    .covering_element(cell.range)
                    .ancestors()
                    .find_map(LuaNameExpr::cast)
                    .filter(|name| name.get_range() == cell.range)?;
                let index_expr = name_expr
                    .syntax()
                    .ancestors()
                    .find_map(LuaIndexExpr::cast)
                    .filter(|index| {
                        index
                            .get_prefix_expr()
                            .is_some_and(|prefix| prefix.syntax() == name_expr.syntax())
                    })?;
                Some((name_expr, index_expr))
            })
            .collect::<Vec<_>>();
        if all_sites.is_empty() {
            continue;
        }

        let base_type = declaration_base_type(db, &mut infer_cache, decl_id);

        // A member path reached from this declaration, e.g. `self.Owner` in
        // `self.Owner:ConCommand()`. Climbed from the sites already scanned
        // rather than from a walk of the file, so each level of the path costs
        // one parent hop.
        let mut evidence = MemberPathEvidence {
            file_id,
            root_decl_id: decl_id,
            declared_root_type: base_type.as_ref(),
            declared_path_bases: &mut declared_path_bases,
            subtypes_with_member: &mut subtypes_with_member,
            scores: &mut nested_scores,
        };
        for (_, receiver) in &all_sites {
            let mut receiver = receiver.clone();
            while let Some(index_expr) = member_path_parent(&receiver) {
                collect_member_path_unguarded_child_evidence(
                    db,
                    &mut infer_cache,
                    workspace_id,
                    &mut evidence,
                    &receiver,
                    &index_expr,
                );
                receiver = index_expr;
            }
        }

        // A `DEFINE_BASECLASS` local holds the named class's own table, never a subtype of it.
        let Some(base_type) = base_type.filter(|_| {
            !db.get_decl_index()
                .get_decl(&decl_id)
                .is_some_and(|decl| decl.is_macro_local())
        }) else {
            continue;
        };
        let Some(base_id) = unguarded_child_base_id(&base_type) else {
            if let Some(arms) = class_union_arms(db, &base_type) {
                let mut union_evidence = UnionArmEvidence {
                    file_id,
                    flow_tree,
                    decl_id,
                    base_type: &base_type,
                    arms: &arms,
                    subtypes_with_member: &mut subtypes_with_member,
                    scores: &mut scores,
                    sources: &mut sources,
                    found_types: &mut found_types,
                };
                for (name_expr, index_expr) in &all_sites {
                    union_evidence.collect(db, &mut infer_cache, name_expr, index_expr);
                }
            }
            continue;
        };
        for (name_expr, index_expr) in &all_sites {
            if let Some(profile) = &mut profile {
                profile.references_scanned += 1;
            }
            if is_assignment_target(index_expr) {
                if let Some(profile) = &mut profile {
                    profile.assignment_targets_skipped += 1;
                }
                continue;
            }
            let Some(index_key) = index_expr.get_index_key() else {
                continue;
            };
            if is_condition_evidence(index_expr) {
                if let Some(profile) = &mut profile {
                    profile.conditions_skipped += 1;
                }
                continue;
            }
            if is_matching_short_circuit_guard(db, &mut infer_cache, index_expr) {
                if let Some(profile) = &mut profile {
                    profile.short_circuit_guards_skipped += 1;
                }
                continue;
            }
            if LuaMemberKey::index_key_is_dynamic(db, &mut infer_cache, &index_key) {
                continue;
            }
            let Ok(member_key) = LuaMemberKey::from_index_key(db, &mut infer_cache, &index_key)
            else {
                continue;
            };
            if type_has_visible_member_at_use(
                db,
                workspace_id,
                &LuaType::Ref(base_id.clone()),
                &member_key,
                file_id,
                index_expr.get_position(),
            ) {
                continue;
            }
            if member_path_assigned_before(db, &mut infer_cache, flow_tree, index_expr) {
                continue;
            }
            let receiver =
                get_var_expr_var_ref_id(db, &mut infer_cache, LuaExpr::NameExpr(name_expr.clone()));
            let Some(children) = nearest_subtypes_with_member(
                db,
                &mut infer_cache,
                &mut subtypes_with_member,
                &base_id,
                &member_key,
                member_path_parent(index_expr).is_some(),
                receiver.as_ref(),
            ) else {
                continue;
            };

            // A real flow guard wins. Its narrowed use is neither heuristic
            // evidence nor an unguarded-child diagnostic site.
            let current = infer_expr(db, &mut infer_cache, LuaExpr::NameExpr(name_expr.clone()))
                .unwrap_or(LuaType::Unknown);
            if !unguarded_child_current_matches_base(&current, &base_type, &base_id) {
                continue;
            }
            // Prefer a stabilized initializer type (e.g. a later same-file
            // field assignment) over heuristic child-union refinement when
            // that initializer already owns the used member.
            if let Some(initializer_type) =
                refined_initializer_type_for_decl(db, &mut infer_cache, decl_id, file_id)
            {
                if type_has_visible_static_member_at_use(
                    db,
                    &initializer_type,
                    &member_key,
                    file_id,
                    index_expr.get_position(),
                ) {
                    if is_strict_nominal_refinement(db, &initializer_type, &current) {
                        initializer_refinements
                            .entry(decl_id)
                            .or_insert(initializer_type);
                    }
                    continue;
                }
            }

            let definitions = flow_tree
                .and_then(|tree| {
                    tree.get_flow_id(name_expr.get_syntax_id())
                        .map(|flow| tree.reaching_definitions(decl_id, flow))
                })
                .unwrap_or_else(|| Arc::from([LuaDefinitionId::Declaration(decl_id)]));
            let source = InFiled::new(file_id, index_expr.get_syntax_id());
            found_types.insert(source.clone(), current.clone());
            let source = ChildSource {
                site: source,
                guarded: is_presence_covered(db, &mut infer_cache, index_expr),
            };
            let mut evidence_recorded = false;
            for child_id in children.iter() {
                let visible = match child_member_visible_at(
                    db,
                    &mut infer_cache,
                    child_id,
                    &member_key,
                    file_id,
                    index_expr.get_position(),
                ) {
                    Ok(visible) => visible,
                    Err(reason) if reason.is_need_resolve() => {
                        deferred_definitions.extend(definitions.iter().cloned());
                        break;
                    }
                    Err(_) => false,
                };
                if !visible {
                    continue;
                }
                if !evidence_recorded {
                    if let Some(profile) = &mut profile {
                        profile.evidence_sites += 1;
                    }
                    evidence_recorded = true;
                }
                for definition in definitions.iter().cloned() {
                    scores
                        .entry(definition.clone())
                        .or_insert_with(|| UnguardedChildCandidates {
                            parent_type: base_type.clone(),
                            children: HashMap::default(),
                        })
                        .children
                        .entry(child_id.clone())
                        .or_default()
                        .insert(member_key.clone());
                    let source_key = (definition, child_id.clone());
                    sources
                        .entry(source_key)
                        .and_modify(|existing| {
                            if source.precedes(existing) {
                                *existing = source.clone();
                            }
                        })
                        .or_insert_with(|| source.clone());
                }
            }
        }
    }

    UnguardedChildFileResult {
        scores,
        deferred_definitions,
        nested_scores,
        sources,
        found_types,
        initializer_refinements,
        profile,
    }
}

/// Whether every path reaching `path` assigns that same access path first, so
/// its value there is the assigned one rather than a lookup on the declared type.
/// A closure assigned to a member of `recv` receives `recv` as its first
/// parameter, so a path on that parameter continues as the same path on `recv`
/// from where the closure is created.
fn member_path_assigned_before(
    db: &crate::DbIndex,
    cache: &mut crate::LuaInferCache,
    flow_tree: Option<&crate::FlowTree>,
    path: &LuaIndexExpr,
) -> bool {
    let Some(flow_tree) = flow_tree else {
        return false;
    };
    let (Some(flow_id), Some(root)) = (
        flow_tree.get_flow_id(path.get_syntax_id()),
        glua_parser::LuaChunk::cast(path.get_root()),
    ) else {
        return false;
    };
    let Some(path_ref) = get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(path.clone()))
    else {
        return false;
    };
    path_assigned_on_every_path(db, cache, flow_tree, &root, path, flow_id, path_ref)
}

fn path_assigned_on_every_path(
    db: &crate::DbIndex,
    cache: &mut crate::LuaInferCache,
    flow_tree: &crate::FlowTree,
    root: &glua_parser::LuaChunk,
    path: &LuaIndexExpr,
    flow_id: crate::FlowId,
    path_ref: VarRefId,
) -> bool {
    let root_ref = match &path_ref {
        VarRefId::IndexRef(path_root, _) => path_root.as_decl_id().map(VarRefId::VarRef),
        _ => None,
    };
    flow_tree.every_path_settles(flow_id, |node| {
        if let FlowNodeKind::ClosureEntry(position) = node.kind {
            let Some(crate::FlowAntecedent::Single(antecedent)) = node.antecedent else {
                return None;
            };
            let receiver_path = closure_receiver_path_ref(db, cache, path, position, &path_ref)?;
            return Some(path_assigned_on_every_path(
                db,
                cache,
                flow_tree,
                root,
                path,
                antecedent,
                receiver_path,
            ));
        }
        let FlowNodeKind::Assignment(assignment, _) = &node.kind else {
            return None;
        };
        let (vars, _) = assignment.to_node(root)?.get_var_and_expr_list();
        let mut assigns_path = false;
        for var in vars {
            let var_ref = get_var_expr_var_ref_id(db, cache, var.to_expr())?;
            if var_ref == path_ref {
                assigns_path = true;
            } else if Some(&var_ref) == root_ref.as_ref() {
                return Some(false);
            }
        }
        assigns_path.then_some(true)
    })
}

/// `path_ref` rewritten onto the receiver when the closure created at `position`
/// is the direct value of `recv.field = function(self_param, ...)` and the path
/// is rooted at that first parameter.
fn closure_receiver_path_ref(
    db: &crate::DbIndex,
    cache: &mut crate::LuaInferCache,
    path: &LuaIndexExpr,
    position: rowan::TextSize,
    path_ref: &VarRefId,
) -> Option<VarRefId> {
    let VarRefId::IndexRef(path_root, key) = path_ref else {
        return None;
    };
    let closure = path
        .ancestors::<LuaClosureExpr>()
        .find(|closure| closure.get_position() == position)?;
    let first_param = closure.get_params_list()?.get_params().next()?;
    let param_name = first_param.get_name_token()?.get_name_text().to_string();
    let param_decl = crate::LuaDeclId::new(
        cache.get_file_id(),
        glua_parser::LuaAstToken::get_range(&first_param.get_name_token()?).start(),
    );
    if path_root.as_decl_id()? != param_decl {
        return None;
    }
    let assign = LuaAssignStat::cast(closure.syntax().parent()?)?;
    let (vars, exprs) = assign.get_var_and_expr_list();
    let index = exprs
        .iter()
        .position(|expr| expr.syntax() == closure.syntax())?;
    let LuaVarExpr::IndexExpr(target) = vars.get(index)? else {
        return None;
    };
    let LuaExpr::NameExpr(receiver) = target.get_prefix_expr()? else {
        return None;
    };
    let VarRefId::VarRef(receiver_decl) =
        get_var_expr_var_ref_id(db, cache, LuaExpr::NameExpr(receiver.clone()))?
    else {
        return None;
    };
    let rest = key.strip_prefix(param_name.as_str())?;
    Some(VarRefId::IndexRef(
        VarRefRootId::Decl(receiver_decl),
        internment::ArcIntern::new(smol_str::SmolStr::new(format!(
            "{}{rest}",
            receiver.get_name_text()?
        ))),
    ))
}

/// The next level of a member path, e.g. `self.Owner:ConCommand` from
/// `self.Owner`.
fn member_path_parent(receiver: &LuaIndexExpr) -> Option<LuaIndexExpr> {
    receiver
        .syntax()
        .parent()
        .and_then(LuaIndexExpr::cast)
        .filter(|parent| {
            parent
                .get_prefix_expr()
                .is_some_and(|prefix| prefix.syntax() == receiver.syntax())
        })
}

/// Records evidence for one level of a member path, e.g. `receiver` =
/// `self.Owner` and `index_expr` = `self.Owner:ConCommand`. A deeper path calls
/// this once per level.
fn collect_member_path_unguarded_child_evidence(
    db: &crate::DbIndex,
    cache: &mut crate::LuaInferCache,
    workspace_id: Option<crate::WorkspaceId>,
    evidence: &mut MemberPathEvidence<'_>,
    receiver: &LuaIndexExpr,
    index_expr: &LuaIndexExpr,
) {
    let file_id = evidence.file_id;
    let Some(index_key) = index_expr.get_index_key() else {
        return;
    };
    if LuaMemberKey::index_key_is_dynamic(db, cache, &index_key) {
        return;
    }
    let Ok(member_key) = LuaMemberKey::from_index_key(db, cache, &index_key) else {
        return;
    };
    if is_assignment_target(index_expr) {
        return;
    }
    if is_condition_evidence(index_expr) {
        return;
    }
    let Some(receiver_key) = receiver
        .get_index_key()
        .and_then(|key| LuaMemberKey::from_index_key(db, cache, &key).ok())
    else {
        return;
    };
    let Some(prefix) = receiver.get_prefix_expr() else {
        return;
    };
    let receiver_assigned = member_path_assigned_before(
        db,
        cache,
        db.get_flow_index().get_flow_tree(&file_id),
        receiver,
    );
    // At the first level the prefix is the root declaration, whose type is
    // already resolved for its whole reference set; reading it off the name
    // expression would run a flow walk per use, and the child lookup below rules
    // most uses out before narrowing matters. Deeper levels have no such
    // shortcut and infer their prefix, which the narrowed type below then reads
    // back from the same cache.
    let prefix_type = match evidence.declared_root_type {
        Some(typ) if matches!(prefix, LuaExpr::NameExpr(_)) => typ.clone(),
        _ => match infer_expr(db, cache, prefix) {
            Ok(typ) => typ,
            Err(_) => return,
        },
    };
    // The path is only worth narrowing if the thing it is read from is itself
    // settled: a structural object, or a declared class.
    if !prefix_type.contains_object_type()
        && unguarded_child_base_id(&prefix_type).is_none()
        && !prefix_type.is_unknown()
    {
        return;
    }
    // `infer_raw_member_type` answers from the member and type indexes alone: it
    // takes no file and no offset, and the cache it is handed only memoises a
    // lookup keyed by the same type and member. The pair is therefore the whole
    // key, and the indexes it reads do not change until this pass publishes.
    let declared_base = match evidence
        .declared_path_bases
        .get(&(prefix_type.clone(), receiver_key.clone()))
    {
        _ if receiver_assigned => None,
        Some(hit) => hit.clone(),
        None => {
            let resolved = crate::semantic::infer_raw_member_type_with_cache(
                db,
                cache,
                &prefix_type,
                &receiver_key,
            )
            .ok()
            .and_then(|declared| {
                unguarded_child_base_id(&declared).map(|base_id| (declared, base_id))
            });
            evidence
                .declared_path_bases
                .insert((prefix_type, receiver_key), resolved.clone());
            resolved
        }
    };
    let indexed_into = member_path_parent(index_expr).is_some();
    let receiver_ref = get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(receiver.clone()));
    let base_has_visible_member = |base_id: &LuaTypeDeclId| {
        type_has_visible_member_at_use(
            db,
            workspace_id,
            &LuaType::Ref(base_id.clone()),
            &member_key,
            file_id,
            index_expr.get_position(),
        )
    };
    let (base_id, current) = match declared_base {
        // The declaration already names the base, so the child lookup can rule
        // the use out before its narrowed type is worth computing.
        Some((declared, base_id)) => {
            if base_has_visible_member(&base_id)
                || nearest_subtypes_with_member(
                    db,
                    cache,
                    evidence.subtypes_with_member,
                    &base_id,
                    &member_key,
                    indexed_into,
                    receiver_ref.as_ref(),
                )
                .is_none()
            {
                return;
            }
            let current = infer_expr(db, cache, LuaExpr::IndexExpr(receiver.clone()))
                .unwrap_or(LuaType::Unknown);
            // A real flow guard wins, exactly as it does for a plain declaration.
            if !unguarded_child_current_matches_base(&current, &declared, &base_id) {
                return;
            }
            (base_id, current)
        }
        // Nothing is declared for this path, or an assignment replaced it, so a
        // guard or the assigned value is the only thing that could have given it
        // a base. A base read back from this pass's own
        // published narrowing is not one: scoring against it would descend one
        // more level of the class tree per round, so the receiver's pre-pass
        // type is used instead.
        None => {
            let current =
                published_unguarded_child_base(db, file_id, receiver).unwrap_or_else(|| {
                    infer_expr(db, cache, LuaExpr::IndexExpr(receiver.clone()))
                        .unwrap_or(LuaType::Unknown)
                });
            let Some(base_id) = unguarded_child_base_id(&current) else {
                return;
            };
            if base_has_visible_member(&base_id) {
                return;
            }
            (base_id, current)
        }
    };
    let Some(children) = nearest_subtypes_with_member(
        db,
        cache,
        evidence.subtypes_with_member,
        &base_id,
        &member_key,
        indexed_into,
        receiver_ref.as_ref(),
    ) else {
        return;
    };
    if is_matching_short_circuit_guard(db, cache, index_expr) {
        return;
    }
    let Some(target) = nested_unguarded_child_target(db, cache, receiver, evidence.root_decl_id)
    else {
        return;
    };
    let source = ChildSource {
        site: InFiled::new(file_id, index_expr.get_syntax_id()),
        guarded: is_presence_covered(db, cache, index_expr),
    };
    let receiver = InFiled::new(file_id, receiver.get_syntax_id());
    let candidates =
        evidence
            .scores
            .entry(target)
            .or_insert_with(|| NestedUnguardedChildCandidates {
                parent_type: current.clone(),
                children: HashMap::default(),
                receivers: FxHashSet::default(),
                sources: HashMap::default(),
            });
    if candidates.parent_type != current {
        return;
    }
    candidates.receivers.insert(receiver);
    for child_id in children.iter() {
        record_child_source(&mut candidates.sources, child_id.clone(), source.clone());
        candidates
            .children
            .entry(child_id.clone())
            .or_default()
            .insert(member_key.clone());
    }
}

/// What a receiver was before this pass narrowed it, when it did. Recorded on
/// the published step, so it survives the fact that replaced it.
fn published_unguarded_child_base(
    db: &crate::DbIndex,
    file_id: crate::FileId,
    receiver: &LuaIndexExpr,
) -> Option<LuaType> {
    let node = LuaInferenceNodeId::TypeOwner(crate::LuaTypeOwner::SyntaxId(InFiled::new(
        file_id,
        receiver.get_syntax_id(),
    )));
    let fact = db.get_inference_fact(&node)?;
    let step = fact
        .provenance()
        .iter()
        .find(|step| step.event.kind == LuaInferenceProvenanceKind::UnguardedChild)?;
    step.found_type.as_deref().cloned()
}

fn nested_unguarded_child_target(
    db: &crate::DbIndex,
    cache: &mut crate::LuaInferCache,
    receiver: &LuaIndexExpr,
    root_decl_id: crate::LuaDeclId,
) -> Option<NestedUnguardedChildTarget> {
    let mut path = Vec::new();
    let mut current = receiver.clone();
    loop {
        let key = current.get_index_key()?;
        path.push(LuaMemberKey::from_index_key(db, cache, &key).ok()?);
        match current.get_prefix_expr()? {
            LuaExpr::IndexExpr(parent) => current = parent,
            LuaExpr::NameExpr(_) => {
                path.reverse();
                return Some(NestedUnguardedChildTarget { root_decl_id, path });
            }
            _ => return None,
        }
    }
}

pub(super) fn prepare_inferred_positive_guards(db: &mut crate::DbIndex, context: &AnalyzeContext) {
    let file_ids = context
        .tree_list
        .iter()
        .map(|tree| tree.file_id)
        .collect::<Vec<_>>();
    for file_id in &file_ids {
        db.get_signature_index_mut()
            .clear_inferred_positive_guards_for_file(*file_id);
    }
}

pub(super) fn publish_inferred_positive_guards(
    db: &mut crate::DbIndex,
    context: &mut AnalyzeContext,
) -> usize {
    let mut published = 0;
    let mut pending = Vec::new();
    for candidate in std::mem::take(&mut context.inferred_guard_candidates) {
        let file_id = candidate.file_id;
        let Some(root) = db
            .get_vfs()
            .get_syntax_tree(&file_id)
            .map(|tree| tree.get_red_root())
        else {
            continue;
        };
        let Some(node) = candidate.value.to_node_from_root(&root) else {
            continue;
        };
        let Some(closure) = LuaClosureExpr::cast(node) else {
            continue;
        };
        let signature_id = LuaSignatureId::from_closure(file_id, &closure);
        let owner = inferred_guard_owner(db, signature_id, &closure);
        // A guard is published under the name its definition assigns; a
        // definition whose target has no static name has none to publish.
        if owner.is_none()
            && (closure.get_parent::<LuaFuncStat>().is_some()
                || closure.get_parent::<LuaAssignStat>().is_some())
        {
            continue;
        }
        let Some((guard, returns)) =
            inferred_positive_guard_for_closure(db, context, file_id, &closure)
        else {
            pending.push(candidate);
            continue;
        };
        let mut return_changed = false;
        if let Some(signature) = db.get_signature_index_mut().get_mut(&signature_id)
            && signature.resolve_return == SignatureReturnStatus::UnResolve
        {
            signature.return_docs = returns;
            signature.resolve_return = SignatureReturnStatus::InferResolve;
            return_changed = true;
        }
        if return_changed {
            db.get_signature_index_mut().note_return_write(signature_id);
        }
        if let Some(owner) = owner {
            db.get_signature_index_mut()
                .set_owned_inferred_positive_guard(signature_id, owner, guard);
        } else {
            db.get_signature_index_mut()
                .set_inferred_positive_guard(signature_id, guard);
        }
        published += 1;
    }
    context.inferred_guard_candidates = pending;
    published
}

fn inferred_guard_owner(
    db: &crate::DbIndex,
    signature_id: LuaSignatureId,
    closure: &LuaClosureExpr,
) -> Option<LuaInferredGuardOwner> {
    let var = if let Some(func_stat) = closure.get_parent::<LuaFuncStat>() {
        func_stat.get_func_name()?
    } else {
        let assign_stat = closure.get_parent::<LuaAssignStat>()?;
        let (vars, value_exprs) = assign_stat.get_var_and_expr_list();
        let value_idx = value_exprs
            .iter()
            .position(|expr| expr.get_position() == closure.get_position())?;
        vars.get(value_idx)?.clone()
    };
    let mut path = match var {
        LuaVarExpr::NameExpr(name_expr) => {
            vec![name_expr.get_name_token()?.get_name_text().into()]
        }
        LuaVarExpr::IndexExpr(index_expr) => global_path_from_index_expr(index_expr)?,
    };
    crate::canonicalize_global_root_path(&mut path);
    Some(LuaInferredGuardOwner::GlobalPath {
        signature_id,
        state_mask: db
            .get_gmod_infer_index()
            .get_state_mask_at_offset(&signature_id.get_file_id(), closure.get_range().start()),
        path: path.into_boxed_slice(),
    })
}

fn global_path_from_index_expr(
    index_expr: glua_parser::LuaIndexExpr,
) -> Option<Vec<smol_str::SmolStr>> {
    use glua_parser::LuaIndexKey;

    let mut path = match index_expr.get_prefix_expr()? {
        LuaExpr::NameExpr(name_expr) => {
            vec![name_expr.get_name_token()?.get_name_text().into()]
        }
        LuaExpr::IndexExpr(prefix) => global_path_from_index_expr(prefix)?,
        _ => return None,
    };
    let member = match index_expr.get_index_key()? {
        LuaIndexKey::Name(name) => name.get_name_text().into(),
        LuaIndexKey::String(string) => string.get_value().into(),
        _ => return None,
    };
    path.push(member);
    Some(path)
}

fn inferred_positive_guard_for_closure(
    db: &crate::DbIndex,
    context: &mut AnalyzeContext,
    file_id: crate::FileId,
    closure: &LuaClosureExpr,
) -> Option<(LuaInferredPositiveGuard, Vec<crate::LuaDocReturnInfo>)> {
    let signature_id = LuaSignatureId::from_closure(file_id, closure);
    let signature = db.get_signature_index().get(&signature_id)?;
    if signature.is_generic() || signature.is_vararg || !signature.overloads.is_empty() {
        return None;
    }

    let return_points =
        super::lua::func_body::analyze_func_body_returns(Some((db, file_id)), closure.get_block()?);
    let mut truthy_exprs = Vec::new();
    for point in &return_points {
        match point {
            super::lua::LuaReturnPoint::Expr(expr)
                if !super::common::is_falsy_literal_expr(expr) =>
            {
                truthy_exprs.push(expr);
            }
            super::lua::LuaReturnPoint::MuliExpr(_) => return None,
            super::lua::LuaReturnPoint::Error(call) => {
                let cache = context.infer_manager.get_infer_cache(file_id);
                if !infer_expr(db, cache, LuaExpr::CallExpr(call.clone()))
                    .ok()?
                    .is_never()
                {
                    return None;
                }
            }
            _ => {}
        }
    }
    if truthy_exprs.is_empty() {
        return None;
    }
    let returns = super::lua::analyze_return_point(
        db,
        context.infer_manager.get_infer_cache(file_id),
        &return_points,
    )
    .ok()?;
    let return_type = if signature.resolve_return == SignatureReturnStatus::UnResolve {
        returns.first()?.type_ref.clone()
    } else {
        signature.return_docs.first()?.type_ref.clone()
    };
    let return_type = match return_type {
        LuaType::Variadic(values) => values.get_type(0).cloned().unwrap_or(LuaType::Nil),
        typ => typ,
    };
    if !type_is_boolean(&return_type) {
        return None;
    }
    let mut candidates = closure
        .get_params_list()?
        .get_params()
        .enumerate()
        .filter_map(|(param_idx, param)| {
            let decl_id = crate::LuaDeclId::new(file_id, param.get_position());
            if db
                .get_reference_index()
                .get_decl_references(&file_id, &decl_id)
                .is_some_and(|references| references.mutable)
            {
                return None;
            }
            let cache = context.infer_manager.get_infer_cache(file_id);
            let tested_type = match return_points.as_slice() {
                [super::lua::LuaReturnPoint::Expr(expr)] => type_name_test(db, cache, expr)
                    .and_then(|(tested, typ)| {
                        matches!(&tested, LuaExpr::NameExpr(name_expr)
                            if names_param(db, file_id, decl_id, name_expr))
                        .then_some(typ)
                    }),
                _ => None,
            };
            let false_removed_type = tested_type.clone().filter(|typ| match typ {
                LuaType::Ref(id) | LuaType::Def(id) => {
                    db.get_type_index().direct_sub_types(id).next().is_none()
                }
                _ => true,
            });
            let narrowed_type = if let Some(typ) = &tested_type {
                if let Some(param) = signature.param_docs.get(&param_idx)
                    && check_type_compact(db, &param.type_ref, typ).is_err()
                {
                    return None;
                }
                typ.clone()
            } else {
                inferred_guard_param_narrowing(
                    db,
                    cache,
                    closure,
                    decl_id,
                    signature.param_docs.contains_key(&param_idx),
                    &truthy_exprs,
                )?
            };
            Some(LuaInferredPositiveGuard {
                param_idx,
                narrowed_type,
                false_removed_type,
            })
        });
    let guard = candidates.next()?;
    candidates.next().is_none().then_some((guard, returns))
}

fn names_param(
    db: &crate::DbIndex,
    file_id: crate::FileId,
    decl_id: crate::LuaDeclId,
    name_expr: &LuaNameExpr,
) -> bool {
    db.get_reference_index()
        .get_var_reference_decl(&file_id, name_expr.get_range())
        == Some(decl_id)
}

/// The type the parameter `decl_id` holds wherever `truthy_exprs`, the values
/// its function can return truthy, are returned: the union of what each
/// return narrows it to. `None` when any of them leaves it unnarrowed. A
/// documented parameter is measured against its declared type, and the
/// narrowed type must be compatible with it (`check_type_compact`, not strict
/// subtyping).
fn inferred_guard_param_narrowing(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    closure: &LuaClosureExpr,
    decl_id: crate::LuaDeclId,
    param_documented: bool,
    truthy_exprs: &[&LuaExpr],
) -> Option<LuaType> {
    let file_id = cache.get_file_id();
    let references_param = |name_expr: &LuaNameExpr| names_param(db, file_id, decl_id, name_expr);
    let var_ref_id = get_var_expr_var_ref_id(
        db,
        cache,
        closure
            .get_block()?
            .descendants::<LuaNameExpr>()
            .find(references_param)
            .map(LuaExpr::NameExpr)?,
    )?;
    let declared = get_var_ref_type(db, cache, &var_ref_id).ok()?;
    let mut narrowed_types = Vec::new();
    for return_expr in truthy_exprs {
        let return_stat = return_expr.get_parent::<LuaReturnStat>()?;
        let flow_type = infer_var_type_at_node(db, cache, &return_stat, var_ref_id.clone()).ok()?;
        let condition_proof = return_expr
            .descendants::<LuaNameExpr>()
            .find(references_param)
            .map(LuaExpr::NameExpr)
            .and_then(|target_expr| {
                infer_true_condition_narrowing(db, cache, target_expr, (*return_expr).clone())
            });
        let (condition_antecedent, narrowed_type) = match condition_proof {
            Some((antecedent, narrowed)) => (
                antecedent,
                TypeOps::Intersect.apply(db, &flow_type, &narrowed),
            ),
            None => (declared.clone(), flow_type),
        };
        let antecedent = if param_documented {
            declared.clone()
        } else {
            condition_antecedent
        };
        if antecedent == narrowed_type
            || antecedent.is_any()
            || narrowed_type.is_any()
            || narrowed_type.is_unknown()
            || narrowed_type.is_never()
            || narrowed_type.is_nil()
        {
            return None;
        }
        if antecedent.is_unknown() {
            if !is_type_test_result(&narrowed_type) {
                return None;
            }
        } else if param_documented && check_type_compact(db, &antecedent, &narrowed_type).is_err() {
            return None;
        }
        narrowed_types.push(narrowed_type);
    }
    let multiple_returns = narrowed_types.len() > 1;
    let union = LuaType::from_vec(narrowed_types);
    if multiple_returns && union == declared {
        return None;
    }
    Some(union)
}

/// The builtin types a `type(value) == "name"` test narrows an untyped value to.
fn is_type_test_result(typ: &LuaType) -> bool {
    matches!(
        typ,
        LuaType::Table
            | LuaType::String
            | LuaType::Number
            | LuaType::Boolean
            | LuaType::Function
            | LuaType::Userdata
            | LuaType::Thread
    )
}

/// Whether `typ` is a predicate's boolean result: booleans, possibly nil, since nil is falsy.
fn type_is_boolean(typ: &LuaType) -> bool {
    let arms = match typ {
        LuaType::Union(union) => union.types().cloned().collect::<Vec<_>>(),
        LuaType::MultiLineUnion(union) => union
            .get_unions()
            .iter()
            .map(|(arm, _)| arm.clone())
            .collect(),
        _ => return typ.is_boolean(),
    };
    arms.iter().any(type_is_boolean) && arms.iter().all(|arm| arm.is_nil() || type_is_boolean(arm))
}

/// The nearest descendants of the class `receiver_type` names whose
/// `member_key` the read `index_expr` of `file_id` sees: the children a guard
/// narrowing its receiver could name. `None` when no single class types the
/// receiver or none of its children do.
pub(crate) fn unguarded_child_candidates(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    memo: &mut SubtypesWithMember,
    file_id: crate::FileId,
    index_expr: &LuaIndexExpr,
    receiver_type: &LuaType,
    member_key: &LuaMemberKey,
) -> Option<Vec<LuaTypeDeclId>> {
    let mut receiver_type = receiver_type;
    while let LuaType::Instance(instance) = receiver_type {
        receiver_type = instance.get_base();
    }
    let base_id = unguarded_child_base_id(receiver_type)?;
    let children = nearest_subtypes_with_member(
        db,
        cache,
        memo,
        &base_id,
        member_key,
        member_path_parent(index_expr).is_some(),
        None,
    )?;
    let visible = children
        .iter()
        .filter(|child_id| {
            child_member_visible_at(
                db,
                cache,
                child_id,
                member_key,
                file_id,
                index_expr.get_position(),
            )
            .unwrap_or(false)
        })
        .cloned()
        .collect::<Vec<_>>();
    (!visible.is_empty()).then_some(visible)
}

/// Whether a read at `position` of `file_id` sees `child_id`'s `member_key`:
/// a member its realm sees, or a dynamic field.
fn child_member_visible_at(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    child_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    position: rowan::TextSize,
) -> Result<bool, crate::InferFailReason> {
    let owner = LuaMemberOwner::Type(child_id.clone());
    let declared = db
        .get_member_index()
        .get_member_item(&owner, member_key)
        .is_some_and(|item| {
            !item
                .visible_member_ids_with_realm_at_offset(db, &file_id, position)
                .is_empty()
        });
    if declared {
        return Ok(true);
    }
    resolve_dynamic_field_member(db, cache, &LuaType::Ref(child_id.clone()), member_key, None)
        .map(|resolution| resolution.is_some())
}

/// The classes a union of two or more classes can hold, its nil and `NULL`
/// arms aside: a member some of them lack is one a guard must narrow to.
pub(crate) fn class_union_arms(db: &crate::DbIndex, typ: &LuaType) -> Option<Vec<LuaTypeDeclId>> {
    let LuaType::Union(union) = typ else {
        return None;
    };
    let mut arms = Vec::new();
    for component in union.types() {
        match component {
            LuaType::Nil => {}
            LuaType::Ref(type_id) | LuaType::Def(type_id)
                if type_id == &LuaTypeDeclId::global("NULL") => {}
            LuaType::Ref(type_id) | LuaType::Def(type_id)
                if db
                    .get_type_index()
                    .get_type_decl(type_id)
                    .is_some_and(|decl| decl.is_class()) =>
            {
                arms.push(type_id.clone());
            }
            _ => return None,
        }
    }
    arms.sort_by(|left, right| {
        left.get_name()
            .cmp(right.get_name())
            .then_with(|| left.stable_cmp(right))
    });
    arms.dedup();
    (arms.len() >= 2).then_some(arms)
}

/// The arms of the class union `arms` that have `member_key` for a read at
/// `position` of `file_id`: declared where the read sees it, or dynamic.
pub(crate) fn class_union_arms_with_member(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    arms: &[LuaTypeDeclId],
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    position: rowan::TextSize,
) -> Vec<LuaTypeDeclId> {
    let workspace_id = db.get_module_index().get_workspace_id(file_id);
    arms.iter()
        .filter(|arm| {
            let arm_type = LuaType::Ref((*arm).clone());
            type_has_visible_member_at_use(
                db,
                workspace_id,
                &arm_type,
                member_key,
                file_id,
                position,
            ) || resolve_dynamic_field_member(db, cache, &arm_type, member_key, Some(position))
                .is_ok_and(|resolution| resolution.is_some())
        })
        .cloned()
        .collect()
}

/// The classes a read of `member_key` through the class union `arms` needs a
/// guard to narrow to, when some arms lack it: the arms with it, plus the
/// descendants with it of an arm without it that covers one of them, since
/// `Entity|NPC` reading `GetShootPos` can as well be a `Player`.
pub(crate) fn class_union_partial_arms(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    memo: &mut SubtypesWithMember,
    arms: &[LuaTypeDeclId],
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    index_expr: &LuaIndexExpr,
) -> Option<Vec<LuaTypeDeclId>> {
    let position = index_expr.get_position();
    let mut children = class_union_arms_with_member(db, cache, arms, member_key, file_id, position);
    if children.is_empty() || children.len() == arms.len() {
        return None;
    }
    let covering_arms = arms
        .iter()
        .filter(|arm| {
            !children.contains(arm)
                && children
                    .iter()
                    .any(|child| crate::semantic::is_sub_type_of(db, child, arm))
        })
        .cloned()
        .collect::<Vec<_>>();
    let indexed_into = member_path_parent(index_expr).is_some();
    for arm in &covering_arms {
        let Some(descendants) =
            nearest_subtypes_with_member(db, cache, memo, arm, member_key, indexed_into, None)
        else {
            continue;
        };
        for descendant in descendants.iter() {
            if !children.contains(descendant)
                && child_member_visible_at(db, cache, descendant, member_key, file_id, position)
                    .unwrap_or(false)
            {
                children.push(descendant.clone());
            }
        }
    }
    children.sort_by(|left, right| {
        left.get_name()
            .cmp(right.get_name())
            .then_with(|| left.stable_cmp(right))
    });
    Some(children)
}

fn unguarded_child_base_id(typ: &LuaType) -> Option<LuaTypeDeclId> {
    match typ {
        LuaType::Ref(type_id) | LuaType::Def(type_id) => Some(type_id.clone()),
        LuaType::Union(union) => {
            let mut base_id = None;
            let mut saw_nullable_arm = false;
            for component in union.types() {
                match component {
                    LuaType::Nil => saw_nullable_arm = true,
                    LuaType::Ref(type_id) | LuaType::Def(type_id)
                        if type_id == &LuaTypeDeclId::global("NULL") =>
                    {
                        saw_nullable_arm = true;
                    }
                    LuaType::Ref(type_id) | LuaType::Def(type_id) if base_id.is_none() => {
                        base_id = Some(type_id.clone());
                    }
                    _ => return None,
                }
            }
            saw_nullable_arm.then_some(base_id).flatten()
        }
        _ => None,
    }
}

fn unguarded_child_current_matches_base(
    current: &LuaType,
    declared: &LuaType,
    base_id: &LuaTypeDeclId,
) -> bool {
    current == declared
        || matches!(current, LuaType::Ref(current_id) | LuaType::Def(current_id) if current_id == base_id)
}

fn type_has_visible_member_at_use(
    db: &crate::DbIndex,
    workspace_id: Option<crate::WorkspaceId>,
    typ: &LuaType,
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    position: rowan::TextSize,
) -> bool {
    let visible_in_workspace = workspace_id
        .and_then(|workspace_id| {
            crate::semantic::find_members_with_key_in_workspace_for_file_at_offset(
                db,
                typ,
                member_key.clone(),
                false,
                workspace_id,
                file_id,
                position,
            )
        })
        .is_some_and(|members| !members.is_empty());
    visible_in_workspace
        && type_has_visible_static_member_at_use(db, typ, member_key, file_id, position)
}

fn refined_initializer_type_for_decl(
    db: &crate::DbIndex,
    cache: &mut crate::LuaInferCache,
    decl_id: crate::LuaDeclId,
    file_id: crate::FileId,
) -> Option<LuaType> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    let initializer = decl.get_initializer()?;
    let root = db
        .get_vfs()
        .get_syntax_tree(&file_id)
        .map(|tree| tree.get_red_root())?;
    let node = initializer.get_expr_syntax_id().to_node_from_root(&root)?;
    let expr = LuaExpr::cast(node)?;
    if !matches!(expr, LuaExpr::IndexExpr(_) | LuaExpr::CallExpr(_)) {
        return None;
    }

    let mut initializer_type = infer_expr(db, cache, expr).ok()?;
    if let LuaType::Variadic(variadic) = initializer_type {
        initializer_type = variadic
            .get_type(initializer.get_ret_idx())
            .cloned()
            .unwrap_or(LuaType::Unknown);
    } else if initializer.get_ret_idx() != 0 {
        return None;
    }
    if type_is_uninformative(&initializer_type) {
        return None;
    }
    Some(initializer_type)
}

fn type_is_uninformative(typ: &LuaType) -> bool {
    match typ {
        LuaType::Any | LuaType::Unknown | LuaType::Nil | LuaType::Never => true,
        LuaType::Union(union) => union.types().all(type_is_uninformative),
        _ => false,
    }
}

fn is_strict_nominal_refinement(
    db: &crate::DbIndex,
    candidate: &LuaType,
    current: &LuaType,
) -> bool {
    let Some(candidate_id) = single_nominal_type_id(candidate) else {
        return false;
    };
    let Some(current_id) = single_nominal_type_id(current) else {
        return false;
    };
    candidate_id != current_id && crate::semantic::is_sub_type_of(db, &candidate_id, &current_id)
}

fn single_nominal_type_id(typ: &LuaType) -> Option<LuaTypeDeclId> {
    match typ {
        LuaType::Def(type_id) | LuaType::Ref(type_id) => Some(type_id.clone()),
        LuaType::Instance(instance) => single_nominal_type_id(instance.get_base()),
        LuaType::Union(union) => {
            let mut nominal = None;
            for component in union.types().filter(|component| !component.is_nil()) {
                let type_id = single_nominal_type_id(component)?;
                if nominal
                    .as_ref()
                    .is_some_and(|existing| existing != &type_id)
                {
                    return None;
                }
                nominal = Some(type_id);
            }
            nominal
        }
        _ => None,
    }
}

fn type_has_visible_static_member_at_use(
    db: &crate::DbIndex,
    typ: &LuaType,
    member_key: &LuaMemberKey,
    file_id: crate::FileId,
    position: rowan::TextSize,
) -> bool {
    let (LuaType::Ref(type_id) | LuaType::Def(type_id)) = typ else {
        return false;
    };
    let mut owners = vec![type_id.clone()];
    let mut super_types = Vec::new();
    type_id.collect_super_types(db, &mut super_types);
    owners.extend(
        super_types
            .into_iter()
            .filter_map(|super_type| match super_type {
                LuaType::Ref(super_id) | LuaType::Def(super_id) => Some(super_id),
                _ => None,
            }),
    );
    owners.into_iter().any(|owner_id| {
        // Member lookup resolves `Type` and `GlobalPath` owners for the same
        // name (see `find_custom_type_members`): scoped-class and annotation
        // members are filed under the global path, not the type decl. Checking
        // only `Type` misses them, and the receiver then gets narrowed to a
        // subtype that "has" the member the base already resolves.
        [
            &LuaMemberOwner::Type(owner_id.clone()),
            &LuaMemberOwner::GlobalPath(crate::GlobalId::new(owner_id.get_name())),
        ]
        .into_iter()
        .any(|owner| {
            db.get_member_index()
                .get_member_item(owner, member_key)
                .is_some_and(|item| {
                    !item
                        .visible_member_ids_with_realm_at_offset(db, &file_id, position)
                        .is_empty()
                })
        })
    })
}

/// Whether a guard can name `id`: a class synthesized for an anonymous
/// registration, such as a `vgui.RegisterTable` table, has no name in source.
fn is_nameable(db: &crate::DbIndex, id: &LuaTypeDeclId) -> bool {
    id.is_global()
        || !db
            .get_type_index()
            .get_type_decl(id)
            .is_some_and(|decl| decl.is_auto_generated())
}

/// Descendants already matched against a member key and access shape in this
/// evaluation.
pub(crate) type SubtypesWithMember =
    FxHashMap<(LuaTypeDeclId, LuaMemberKey, bool, Option<VarRefId>), Option<Arc<[LuaTypeDeclId]>>>;

#[cfg(test)]
thread_local! {
    static DECLARER_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The nearest descendants of `base_id` that declare `member_key` as a member
/// or a direct dynamic field, ordered by name: a declaring class hides its own descendants, which inherit
/// the member from it. Found from the classes that declare the key, walking
/// up, so the cost does not depend on how many classes derive from the base.
/// When the use indexes into the member's value (`indexed_into`), a class that
/// declares the member only as a function cannot be the receiver. `None` when
/// there are none.
fn nearest_subtypes_with_member(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    memo: &mut SubtypesWithMember,
    base_id: &LuaTypeDeclId,
    member_key: &LuaMemberKey,
    indexed_into: bool,
    receiver: Option<&VarRefId>,
) -> Option<Arc<[LuaTypeDeclId]>> {
    memo.entry((
        base_id.clone(),
        member_key.clone(),
        indexed_into,
        receiver.cloned(),
    ))
    .or_insert_with(|| {
        #[cfg(test)]
        DECLARER_LOOKUPS.with(|scans| scans.set(scans.get() + 1));
        let member_index = db.get_member_index();
        // A field the receiver is given by a write that, as this pass sees
        // it, cannot reach `declarer` is no evidence of what the receiver is:
        // only a type the receiver was taken to be before put it there.
        let mut given_by_receiver = |site: &InFiled<rowan::TextRange>, declarer: &LuaTypeDeclId| {
            receiver.is_some_and(|receiver| {
                written_through_without_reaching(db, cache, site, receiver, declarer)
            })
        };
        let dynamic_declarers = match member_key {
            LuaMemberKey::Name(name) => db
                .get_dynamic_field_index()
                .types_declaring_field(name)
                .into_iter()
                .filter(|type_id| {
                    db.get_dynamic_field_index()
                        .field_definitions(&crate::DynamicFieldOwner::Type(type_id.clone()), name)
                        .iter()
                        .any(|site| !given_by_receiver(site, type_id))
                })
                .collect(),
            _ => Vec::new(),
        };
        let declarers = member_index
            .get_current_members_for_key(member_key)
            .into_iter()
            .filter(|member| {
                !indexed_into
                    || !db
                        .get_type_index()
                        .get_type_cache(&crate::LuaTypeOwner::Member(member.get_id()))
                        .is_some_and(|cache| cache.as_type().is_function())
            })
            .filter_map(
                |member| match member_index.get_member_owner(&member.get_id()) {
                    Some(LuaMemberOwner::Type(owner_id))
                        if !member.is_assignment_define()
                            || !given_by_receiver(
                                &InFiled::new(member.get_file_id(), member.get_range()),
                                owner_id,
                            ) =>
                    {
                        Some(owner_id.clone())
                    }
                    _ => None,
                },
            )
            .chain(dynamic_declarers)
            .collect::<FxHashSet<_>>();
        let mut candidates = Vec::new();
        for declarer in declarers {
            let mut supers = Vec::new();
            declarer.collect_super_types(db, &mut supers);
            let ancestors = supers
                .into_iter()
                .filter_map(|super_type| match super_type {
                    LuaType::Ref(super_id) => Some(super_id),
                    _ => None,
                })
                .collect::<FxHashSet<_>>();
            if ancestors.contains(base_id) {
                candidates.push((declarer, ancestors));
            }
        }
        let mut children = candidates
            .iter()
            .filter(|(_, ancestors)| {
                !candidates
                    .iter()
                    .any(|(other, _)| ancestors.contains(other))
            })
            .map(|(declarer, _)| declarer.clone())
            .collect::<Vec<_>>();
        // A guard cannot name a declarer the use may reach, so no other
        // child is worth suggesting.
        if children.is_empty() || children.iter().any(|child| !is_nameable(db, child)) {
            return None;
        }
        children.sort_by(|left, right| {
            left.get_name()
                .cmp(right.get_name())
                .then_with(|| left.stable_cmp(right))
        });
        Some(children.into())
    })
    .clone()
}

/// Whether the field written at `site`, an index key or the index expression
/// holding it, is written through `receiver` while the receiver, as this
/// pass sees it there, is not `declarer` or one of its children: a cast or a
/// guard at the write makes it reach `declarer`.
fn written_through_without_reaching(
    db: &crate::DbIndex,
    cache: &mut LuaInferCache,
    site: &InFiled<rowan::TextRange>,
    receiver: &VarRefId,
    declarer: &LuaTypeDeclId,
) -> bool {
    if site.file_id != cache.get_file_id() {
        return false;
    }
    let Some(root) = db
        .get_vfs()
        .get_syntax_tree(&site.file_id)
        .map(|tree| tree.get_red_root())
    else {
        return false;
    };
    root.covering_element(site.value)
        .ancestors()
        .find_map(LuaIndexExpr::cast)
        .and_then(|index_expr| index_expr.get_prefix_expr())
        .is_some_and(|prefix| {
            get_var_expr_var_ref_id(db, cache, prefix.clone())
                .is_some_and(|written| &written == receiver)
                && !infer_expr(db, cache, prefix).is_ok_and(|typ| type_reaches(db, &typ, declarer))
        })
}

/// Whether `typ` names `declarer` or one of its children.
fn type_reaches(db: &crate::DbIndex, typ: &LuaType, declarer: &LuaTypeDeclId) -> bool {
    match typ {
        LuaType::Ref(id) | LuaType::Def(id) => {
            if id == declarer {
                return true;
            }
            let mut supers = Vec::new();
            id.collect_super_types(db, &mut supers);
            supers.iter().any(
                |super_type| matches!(super_type, LuaType::Ref(super_id) if super_id == declarer),
            )
        }
        LuaType::Union(union) => union
            .types()
            .any(|member| type_reaches(db, member, declarer)),
        LuaType::Instance(instance) => type_reaches(db, instance.get_base(), declarer),
        _ => false,
    }
}

fn is_condition_evidence(index_expr: &LuaIndexExpr) -> bool {
    if is_call_prefix(index_expr) {
        return false;
    }

    let index_range = index_expr.syntax().text_range();
    for node in index_expr.syntax().ancestors() {
        if LuaClosureExpr::cast(node.clone()).is_some() {
            break;
        }

        if let Some(binary) = LuaBinaryExpr::cast(node.clone())
            && binary.get_op_token().is_some_and(|token| {
                matches!(token.get_op(), BinaryOperator::OpAnd | BinaryOperator::OpOr)
            })
            && binary
                .get_exprs()
                .is_some_and(|(left, _)| logical_condition_contains_index(&left, index_expr))
        {
            return true;
        }

        let condition = LuaIfStat::cast(node.clone())
            .and_then(|stat| stat.get_condition_expr())
            .or_else(|| {
                LuaElseIfClauseStat::cast(node.clone()).and_then(|stat| stat.get_condition_expr())
            })
            .or_else(|| LuaWhileStat::cast(node.clone()).and_then(|stat| stat.get_condition_expr()))
            .or_else(|| LuaRepeatStat::cast(node).and_then(|stat| stat.get_condition_expr()));
        if condition
            .is_some_and(|condition| condition.syntax().text_range().contains_range(index_range))
        {
            return true;
        }
    }

    false
}

fn logical_condition_contains_index(expr: &LuaExpr, index_expr: &LuaIndexExpr) -> bool {
    match expr {
        LuaExpr::IndexExpr(index) => index.syntax() == index_expr.syntax(),
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .is_some_and(|inner| logical_condition_contains_index(&inner, index_expr)),
        LuaExpr::BinaryExpr(binary)
            if binary.get_op_token().is_some_and(|token| {
                matches!(token.get_op(), BinaryOperator::OpAnd | BinaryOperator::OpOr)
            }) =>
        {
            binary.get_exprs().is_some_and(|(left, right)| {
                logical_condition_contains_index(&left, index_expr)
                    || logical_condition_contains_index(&right, index_expr)
            })
        }
        _ => false,
    }
}

fn is_assignment_target(index_expr: &LuaIndexExpr) -> bool {
    if let Some(func_name) = index_expr
        .syntax()
        .ancestors()
        .find_map(glua_parser::LuaFuncStat::cast)
        .and_then(|func_stat| func_stat.get_func_name())
        && is_prefix_chain_to(index_expr, func_name.syntax())
    {
        return true;
    }
    let Some(assign_stat) = index_expr
        .syntax()
        .ancestors()
        .find_map(LuaAssignStat::cast)
    else {
        return false;
    };
    let index_range = index_expr.syntax().text_range();
    let (vars, _) = assign_stat.get_var_and_expr_list();
    vars.into_iter().any(|var| {
        var.syntax().text_range().contains_range(index_range)
            && is_prefix_chain_to(index_expr, var.syntax())
    })
}

/// Whether `index_expr` reaches `top` only as the prefix of each index expression between:
/// a read in a key or a call argument of the written path is not part of the path.
fn is_prefix_chain_to(index_expr: &LuaIndexExpr, top: &LuaSyntaxNode) -> bool {
    let mut current = index_expr.syntax().clone();
    while current != *top {
        let Some(parent) = current.parent().and_then(LuaIndexExpr::cast) else {
            return false;
        };
        if parent
            .get_prefix_expr()
            .is_none_or(|prefix| *prefix.syntax() != current)
        {
            return false;
        }
        current = parent.syntax().clone();
    }
    true
}

fn is_call_prefix(index_expr: &LuaIndexExpr) -> bool {
    index_expr
        .syntax()
        .ancestors()
        .find_map(LuaCallExpr::cast)
        .and_then(|call| call.get_prefix_expr())
        .is_some_and(|prefix| prefix.syntax() == index_expr.syntax())
}

fn is_matching_short_circuit_guard(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    let index_range = index_expr.syntax().text_range();
    for ancestor in index_expr.syntax().ancestors() {
        if LuaClosureExpr::cast(ancestor.clone()).is_some() {
            break;
        }
        let Some(binary) = LuaBinaryExpr::cast(ancestor) else {
            continue;
        };
        let Some(op) = binary
            .get_op_token()
            .map(|token| token.get_op())
            .filter(|op| matches!(op, BinaryOperator::OpAnd | BinaryOperator::OpOr))
        else {
            continue;
        };
        let Some((left, right)) = binary.get_exprs() else {
            continue;
        };
        if left.syntax().text_range().contains_range(index_range) {
            if op == BinaryOperator::OpAnd
                && positive_guard_indexes(db, cache, &left)
                    .iter()
                    .any(|guard| guard.syntax() == index_expr.syntax())
            {
                return true;
            }
            continue;
        }
        if !right.syntax().text_range().contains_range(index_range) {
            continue;
        }
        // `a and b` runs `b` when `a` holds; `a or b` runs it when `a` fails.
        let guards = if op == BinaryOperator::OpAnd {
            positive_guard_indexes(db, cache, &left)
        } else {
            present_after_exit_indexes(db, cache, &left)
        };
        if guards_cover(db, cache, guards, index_expr, GuardScope::Path) {
            return true;
        }
    }
    false
}

/// What a guard has to prove present to cover a read.
#[derive(Clone, Copy)]
enum GuardScope {
    /// The read's own member path.
    Path,
    /// Some member of the table the read indexes: a guard on `ent.a` shows `ent`
    /// is a type that defines `a`, which covers `ent.b` too.
    Parent,
}

/// Whether an `if`/`elseif` around `index_expr` only runs its block when a member of
/// the table it indexes holds a value, as in `if IsValid(ent.Child) then ent = ent.Child end`,
/// with the path's root not reassigned earlier in that block.
pub(crate) fn is_guarded_by_enclosing_condition(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    enclosing_guard_covers(db, cache, index_expr, GuardScope::Parent)
        || is_guarded_by_absence_exit(db, cache, index_expr, GuardScope::Parent)
}

/// Whether the read of `index_expr` is covered for presence in any way: by an enclosing guard
/// on a member of the table it indexes, or by `is_member_path_presence_guarded`.
fn is_presence_covered(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    is_guarded_by_enclosing_condition(db, cache, index_expr)
        || is_member_path_presence_guarded(db, cache, index_expr)
}

/// Whether the read of `index_expr` probes its member for presence or the member path is proven
/// present there: by an enclosing test, an earlier exit, a short circuit or a tested local.
pub(crate) fn is_member_path_presence_guarded(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    is_presence_probe(db, cache, index_expr)
        || enclosing_guard_covers(db, cache, index_expr, GuardScope::Path)
        || is_matching_short_circuit_guard(db, cache, index_expr)
        || is_guarded_by_absence_exit(db, cache, index_expr, GuardScope::Path)
        || is_bound_to_presence_tested_local(db, cache, index_expr)
}

/// Whether the read only tests its member: compared for (in)equality, or tested for presence
/// directly or as the argument of a presence-guard call such as `IsValid`.
fn is_presence_probe(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    let value = LuaExpr::IndexExpr(index_expr.clone());
    is_equality_operand(index_expr)
        || super::common::is_presence_tested(index_expr.syntax(), &mut |call| {
            is_presence_guard_target(db, cache, call, &value)
        })
}

/// Whether `value` is the expression the presence-guard `call` narrows: its first argument for
/// a plain call, its receiver for a colon call.
fn is_presence_guard_target(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    call: &LuaCallExpr,
    value: &LuaExpr,
) -> bool {
    crate::semantic::presence_guard_target(db, cache, call).is_some_and(|target| {
        without_parens(target).syntax() == without_parens(value.clone()).syntax()
    })
}

fn without_parens(mut expr: LuaExpr) -> LuaExpr {
    loop {
        let LuaExpr::ParenExpr(paren) = &expr else {
            return expr;
        };
        let Some(inner) = paren.get_expr() else {
            return expr;
        };
        expr = inner;
    }
}

fn is_equality_operand(index_expr: &LuaIndexExpr) -> bool {
    let mut node = index_expr.syntax().clone();
    while let Some(parent) = node.parent() {
        if LuaParenExpr::can_cast(parent.kind().into()) {
            node = parent;
            continue;
        }
        return LuaBinaryExpr::cast(parent)
            .and_then(|binary| binary.get_op_token())
            .is_some_and(|token| {
                matches!(token.get_op(), BinaryOperator::OpEq | BinaryOperator::OpNe)
            });
    }
    false
}

/// Whether the read is the whole value of a `local` that is tested for presence before anything
/// writes the local again.
fn is_bound_to_presence_tested_local(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
) -> bool {
    let file_id = cache.get_file_id();
    let mut value = LuaExpr::IndexExpr(index_expr.clone());
    while let Some(paren) = value.get_parent::<LuaParenExpr>() {
        value = LuaExpr::ParenExpr(paren);
    }
    let Some(local_stat) = value.get_parent::<LuaLocalStat>() else {
        return false;
    };
    let Some(local_name) = local_stat.get_local_name_by_value(value) else {
        return false;
    };
    let decl_position = local_name.get_position();
    let Some(references) = db
        .get_reference_index()
        .get_decl_references(&file_id, &crate::LuaDeclId::new(file_id, decl_position))
    else {
        return false;
    };
    let Some(root) = index_expr.syntax().ancestors().last() else {
        return false;
    };
    let bound_closure = index_expr
        .syntax()
        .ancestors()
        .find_map(LuaClosureExpr::cast)
        .map(|closure| closure.get_syntax_id());
    let bound_end = local_stat.syntax().text_range().end();
    let first_rewrite = references
        .cells
        .iter()
        .filter(|cell| cell.is_write && cell.range.start() != decl_position)
        .map(|cell| cell.range.start())
        .min();
    references.cells.iter().any(|cell| {
        !cell.is_write
            && cell.range.start() >= bound_end
            && first_rewrite.is_none_or(|rewrite| cell.range.start() < rewrite)
            && root
                .covering_element(cell.range)
                .ancestors()
                .find_map(LuaNameExpr::cast)
                .is_some_and(|name_expr| {
                    if name_expr
                        .syntax()
                        .ancestors()
                        .find_map(LuaClosureExpr::cast)
                        .map(|closure| closure.get_syntax_id())
                        != bound_closure
                    {
                        return false;
                    }
                    let value = LuaExpr::NameExpr(name_expr.clone());
                    super::common::is_presence_tested(name_expr.syntax(), &mut |call| {
                        is_presence_guard_target(db, cache, call, &value)
                    })
                })
    })
}

/// Whether an earlier `if not path then return end` in an enclosing block covers `index_expr`.
fn is_guarded_by_absence_exit(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
    scope: GuardScope,
) -> bool {
    for block in index_expr.syntax().ancestors() {
        if LuaClosureExpr::can_cast(block.kind().into()) {
            break;
        }
        let Some(block) = glua_parser::LuaBlock::cast(block) else {
            continue;
        };
        let read_start = index_expr.get_position();
        let stats = block
            .get_stats()
            .take_while(|stat| stat.get_position() < read_start)
            .collect::<Vec<_>>();
        for (idx, stat) in stats.iter().enumerate() {
            let glua_parser::LuaStat::IfStat(if_stat) = stat else {
                continue;
            };
            // A label after the exit lets a `goto` from before it skip the test.
            if stats[idx + 1..]
                .iter()
                .any(|stat| matches!(stat, glua_parser::LuaStat::LabelStat(_)))
            {
                continue;
            }
            let (Some(condition), Some(exit_block)) =
                (if_stat.get_condition_expr(), if_stat.get_block())
            else {
                continue;
            };
            if if_stat
                .syntax()
                .text_range()
                .contains_range(index_expr.syntax().text_range())
                || !always_leaves(&exit_block)
            {
                continue;
            }
            let guards = present_after_exit_indexes(db, cache, &condition);
            if guards_cover(db, cache, guards, index_expr, scope)
                && !root_written_in_block_before(
                    db,
                    cache,
                    &block,
                    condition.syntax().text_range().end(),
                    index_expr,
                )
            {
                return true;
            }
        }
    }
    false
}

/// Whether `block` ends in `return` or `break`/`continue` with no `goto` that may land elsewhere.
fn always_leaves(block: &glua_parser::LuaBlock) -> bool {
    let ends_in_exit = block
        .get_stats()
        .filter(|stat| !matches!(stat, glua_parser::LuaStat::EmptyStat(_)))
        .last()
        .is_some_and(|stat| {
            matches!(
                stat,
                glua_parser::LuaStat::ReturnStat(_) | glua_parser::LuaStat::BreakStat(_)
            )
        });
    ends_in_exit
        && block
            .syntax()
            .descendants()
            .all(|node| glua_parser::LuaGotoStat::cast(node).is_none())
}

/// The member paths a false `condition` proves present: `not path`, `path == nil` and `or`s of them.
fn present_after_exit_indexes(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    condition: &LuaExpr,
) -> Vec<LuaIndexExpr> {
    match condition {
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .map(|inner| present_after_exit_indexes(db, cache, &inner))
            .unwrap_or_default(),
        LuaExpr::UnaryExpr(unary)
            if unary.get_op_token().map(|token| token.get_op())
                == Some(glua_parser::UnaryOperator::OpNot) =>
        {
            unary
                .get_expr()
                .map(|inner| positive_guard_indexes(db, cache, &inner))
                .unwrap_or_default()
        }
        LuaExpr::BinaryExpr(binary) => {
            let (Some(op), Some((left, right))) = (
                binary.get_op_token().map(|token| token.get_op()),
                binary.get_exprs(),
            ) else {
                return Vec::new();
            };
            match op {
                BinaryOperator::OpOr => {
                    let mut indexes = present_after_exit_indexes(db, cache, &left);
                    indexes.extend(present_after_exit_indexes(db, cache, &right));
                    indexes
                }
                BinaryOperator::OpEq => nil_compared_index(&left, &right).into_iter().collect(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    }
}

/// The member path one side of a comparison reads when the other side is `nil`.
fn nil_compared_index(left: &LuaExpr, right: &LuaExpr) -> Option<LuaIndexExpr> {
    [(left, right), (right, left)]
        .into_iter()
        .find(|(_, other)| crate::compilation::analyzer::common::is_nil_literal_expr(other))
        .and_then(|(tested, _)| unparenthesized_index(tested))
}

fn unparenthesized_index(expr: &LuaExpr) -> Option<LuaIndexExpr> {
    match expr {
        LuaExpr::IndexExpr(index) => Some(index.clone()),
        LuaExpr::ParenExpr(paren) => unparenthesized_index(&paren.get_expr()?),
        _ => None,
    }
}

fn enclosing_guard_covers(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    index_expr: &LuaIndexExpr,
    scope: GuardScope,
) -> bool {
    let index_range = index_expr.syntax().text_range();
    for ancestor in index_expr.syntax().ancestors() {
        if LuaClosureExpr::cast(ancestor.clone()).is_some() {
            break;
        }
        let guarded_clause = LuaIfStat::cast(ancestor.clone())
            .and_then(|stat| Some((stat.get_condition_expr()?, stat.get_block()?)))
            .or_else(|| {
                LuaElseIfClauseStat::cast(ancestor)
                    .and_then(|clause| Some((clause.get_condition_expr()?, clause.get_block()?)))
            });
        let Some((condition, block)) = guarded_clause else {
            continue;
        };
        if !block.syntax().text_range().contains_range(index_range) {
            continue;
        }
        if matching_guard_in_expr(db, cache, &condition, index_expr, scope)
            && !root_written_in_block_before(db, cache, &block, block.get_position(), index_expr)
        {
            return true;
        }
    }
    false
}

/// Whether the variable `index_expr`'s path starts from is written inside `block`
/// from `from` up to the read, outside closures the block only creates.
fn root_written_in_block_before(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    block: &glua_parser::LuaBlock,
    from: rowan::TextSize,
    index_expr: &LuaIndexExpr,
) -> bool {
    let Some(decl_id) =
        crate::semantic::get_var_expr_var_ref_id(db, cache, LuaExpr::IndexExpr(index_expr.clone()))
            .and_then(|reference| match reference {
                crate::semantic::VarRefId::IndexRef(root, _) => written_decl(&root),
                _ => None,
            })
    else {
        return false;
    };
    let Some(references) = db
        .get_reference_index()
        .get_decl_references(&decl_id.file_id, &decl_id)
        .filter(|references| references.mutable)
    else {
        return false;
    };
    // A statement evaluates its reads before its own writes.
    let read_start = index_expr
        .ancestors::<glua_parser::LuaStat>()
        .next()
        .map_or(index_expr.get_position(), |stat| stat.get_position());
    let root = block.syntax().ancestors().last();
    references.cells.iter().any(|cell| {
        cell.is_write
            && cell.range.start() >= from
            && cell.range.start() < read_start
            && root.as_ref().is_some_and(|root| {
                !root
                    .covering_element(cell.range)
                    .ancestors()
                    .take_while(|node| node != block.syntax())
                    .any(|node| LuaClosureExpr::can_cast(node.kind().into()))
            })
    })
}

/// The declaration whose writes replace what `root` names: an implicit `self` is its
/// own declaration, not its receiver's.
fn written_decl(root: &crate::semantic::VarRefRootId) -> Option<crate::LuaDeclId> {
    match root {
        crate::semantic::VarRefRootId::SelfRef(self_ref) => Some(self_ref.self_decl_id),
        _ => root.as_decl_id(),
    }
}

fn matching_guard_in_expr(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    guard_expr: &LuaExpr,
    guarded: &LuaIndexExpr,
    scope: GuardScope,
) -> bool {
    let guards = positive_guard_indexes(db, cache, guard_expr);
    guards_cover(db, cache, guards, guarded, scope)
}

/// Whether one of `guards`, member paths proven present, covers the read `guarded`.
fn guards_cover(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    guards: Vec<LuaIndexExpr>,
    guarded: &LuaIndexExpr,
    scope: GuardScope,
) -> bool {
    let mut path = |index: &LuaIndexExpr| {
        let table = index
            .get_prefix_expr()
            .and_then(|prefix| crate::semantic::get_var_expr_var_ref_id(db, cache, prefix))?;
        let key = LuaMemberKey::from_index_key(db, cache, &index.get_index_key()?).ok();
        Some((table, key))
    };
    let Some((guarded_table, guarded_key)) = path(guarded) else {
        return false;
    };
    let candidates = guards
        .into_iter()
        .filter_map(|guard| path(&guard).map(|guard_path| (guard, guard_path)))
        .filter(|(_, (table, _))| *table == guarded_table)
        .collect::<Vec<_>>();
    candidates.into_iter().any(|(guard, (_, key))| {
        (key.is_some() && key == guarded_key)
            || (matches!(scope, GuardScope::Parent)
                && proves_child_type(db, cache, &guard, guarded))
    })
}

fn proves_child_type(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    guard: &LuaIndexExpr,
    read: &LuaIndexExpr,
) -> bool {
    let (Some(prefix), Some(key)) = (guard.get_prefix_expr(), guard.get_index_key()) else {
        return false;
    };
    if LuaMemberKey::index_key_is_dynamic(db, cache, &key) {
        return false;
    }
    let (Some(declared), Ok(key), Some(read_key)) = (
        declared_type(db, cache, &prefix),
        LuaMemberKey::from_index_key(db, cache, &key),
        read.get_index_key(),
    ) else {
        return false;
    };
    let (Some(base_id), Ok(read_key)) = (
        unguarded_child_base_id(&declared),
        LuaMemberKey::from_index_key(db, cache, &read_key),
    ) else {
        return false;
    };
    let file_id = cache.get_file_id();
    if type_has_visible_static_member_at_use(
        db,
        &LuaType::Ref(base_id.clone()),
        &key,
        file_id,
        guard.get_position(),
    ) {
        return false;
    }
    let receiver = get_var_expr_var_ref_id(db, cache, prefix);
    let Some(children) = nearest_subtypes_with_member(
        db,
        cache,
        &mut SubtypesWithMember::default(),
        &base_id,
        &key,
        false,
        receiver.as_ref(),
    ) else {
        return false;
    };
    children.iter().all(|child| {
        let typ = LuaType::Ref(child.clone());
        (type_has_visible_static_member_at_use(db, &typ, &key, file_id, guard.get_position())
            || child_member_visible_at(db, cache, child, &key, file_id, guard.get_position())
                == Ok(true))
            && (type_has_visible_static_member_at_use(
                db,
                &typ,
                &read_key,
                file_id,
                read.get_position(),
            ) || child_member_visible_at(
                db,
                cache,
                child,
                &read_key,
                file_id,
                read.get_position(),
            ) == Ok(true))
            && crate::semantic::find_members_with_key(db, &typ, key.clone(), false).is_some_and(
                |members| {
                    members.iter().any(|member| {
                        !member.typ.is_always_falsy()
                            && !member.typ.is_never()
                            && !member.typ.is_unknown()
                            && !member.typ.is_any()
                    })
                },
            )
    })
}

/// The type `expr`, a variable or a member path off one, is declared with, before any
/// child inference refines it.
fn declared_type(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    expr: &LuaExpr,
) -> Option<LuaType> {
    match expr {
        LuaExpr::NameExpr(name) => {
            let decl_id = crate::semantic::get_var_expr_var_ref_id(
                db,
                cache,
                LuaExpr::NameExpr(name.clone()),
            )?
            .get_decl_id_ref()?;
            db.get_type_index()
                .get_type_fact(&decl_id.into())
                .and_then(|fact| {
                    fact.provenance()
                        .iter()
                        .find(|step| step.event.kind == LuaInferenceProvenanceKind::UnguardedChild)
                        .and_then(|step| step.found_type.as_deref().cloned())
                })
                .or_else(|| declaration_base_type(db, cache, decl_id))
        }
        LuaExpr::IndexExpr(index) => {
            let table = declared_type(db, cache, &index.get_prefix_expr()?)?;
            let key = LuaMemberKey::from_index_key(db, cache, &index.get_index_key()?).ok()?;
            let members = crate::semantic::find_members_with_key(db, &table, key, false)?;
            (!members.is_empty())
                .then(|| LuaType::from_vec(members.into_iter().map(|member| member.typ).collect()))
        }
        _ => None,
    }
}

/// The member paths a true `expr` proves present: tested, `~= nil`, `and`-joined or presence-guarded.
fn positive_guard_indexes(
    db: &crate::DbIndex,
    cache: &mut crate::semantic::LuaInferCache,
    expr: &LuaExpr,
) -> Vec<LuaIndexExpr> {
    match expr {
        LuaExpr::IndexExpr(index) => vec![index.clone()],
        LuaExpr::ParenExpr(paren) => paren
            .get_expr()
            .map(|inner| positive_guard_indexes(db, cache, &inner))
            .unwrap_or_default(),
        LuaExpr::BinaryExpr(binary)
            if binary.get_op_token().map(|token| token.get_op()) == Some(BinaryOperator::OpAnd) =>
        {
            binary
                .get_exprs()
                .map(|(left, right)| {
                    let mut indexes = positive_guard_indexes(db, cache, &left);
                    indexes.extend(positive_guard_indexes(db, cache, &right));
                    indexes
                })
                .unwrap_or_default()
        }
        LuaExpr::BinaryExpr(binary)
            if binary.get_op_token().map(|token| token.get_op()) == Some(BinaryOperator::OpNe) =>
        {
            binary
                .get_exprs()
                .and_then(|(left, right)| nil_compared_index(&left, &right))
                .into_iter()
                .collect()
        }
        LuaExpr::CallExpr(call) => crate::semantic::presence_guard_target(db, cache, call)
            .map(|target| match target {
                LuaExpr::IndexExpr(index) => vec![index],
                LuaExpr::ParenExpr(_) => positive_guard_indexes(db, cache, &target),
                _ => Vec::new(),
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

fn declaration_base_type(
    db: &crate::DbIndex,
    cache: &mut crate::LuaInferCache,
    decl_id: crate::LuaDeclId,
) -> Option<LuaType> {
    let decl = db.get_decl_index().get_decl(&decl_id)?;
    if decl.is_param() {
        return crate::infer_param_with_cache(db, cache, decl).ok();
    }
    db.get_type_index()
        .get_type_cache(&decl_id.into())
        .map(|cache| cache.as_type().clone())
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, LuaNameExpr, LuaParser, ParserConfig};

    use crate::{Emmyrc, FileId, LuaTypeDeclId, VirtualWorkspace};

    use super::{compare_unguarded_child_candidates, is_assignment_target};

    fn parse_index_expr(code: &str) -> LuaIndexExpr {
        LuaParser::parse(code, ParserConfig::default())
            .get_chunk_node()
            .descendants::<LuaIndexExpr>()
            .next()
            .expect("expected index expression")
    }

    /// A write through a receiver that, as the pass sees it there, is not the
    /// declaring class gives no evidence for that class; under a cast to it,
    /// the same write does.
    #[test]
    fn a_receiver_write_is_evidence_only_where_it_reaches_the_class() {
        fn excluded(source: &str) -> bool {
            let mut ws = VirtualWorkspace::new();
            let file_id = ws.def_file("lua/vgui/clicks.lua", source);
            let db = ws.analysis.compilation.get_db();
            let write = db
                .get_vfs()
                .get_syntax_tree(&file_id)
                .expect("syntax tree")
                .get_chunk_node()
                .descendants::<LuaIndexExpr>()
                .last()
                .expect("the write");
            let mut cache = crate::LuaInferCache::new(file_id, Default::default());
            let receiver = crate::semantic::get_var_expr_var_ref_id(
                db,
                &mut cache,
                write.get_prefix_expr().expect("the receiver"),
            )
            .expect("the receiver's var ref");
            let site = crate::InFiled::new(
                file_id,
                write
                    .get_index_key()
                    .expect("the key")
                    .get_range()
                    .expect("its range"),
            );
            super::written_through_without_reaching(
                db,
                &mut cache,
                &site,
                &receiver,
                &LuaTypeDeclId::global("BarB"),
            )
        }

        let classes = "---@class Panel
---@class BarB: Panel
";
        assert_eq!(
            (
                excluded(&format!(
                    "{classes}local function stamp(line)
    line.m_fClickTime = 1
end
"
                )),
                excluded(&format!(
                    "{classes}local function stamp(line)
    ---@cast line BarB
    line.m_fClickTime = 1
end
"
                )),
            ),
            (true, false)
        );
    }

    #[test]
    fn assignment_target_classifier_accepts_index_on_left_hand_side() {
        let index_expr = parse_index_expr("value.ChildOnly = function() end");

        assert!(is_assignment_target(&index_expr));
    }

    #[test]
    fn assignment_target_classifier_rejects_index_on_right_hand_side() {
        let index_expr = parse_index_expr("result = value.ChildOnly");

        assert!(!is_assignment_target(&index_expr));
    }

    #[test]
    fn assignment_target_classifier_rejects_index_read_in_a_key() {
        let chunk = LuaParser::parse("targets[value.ChildOnly] = 1", ParserConfig::default())
            .get_chunk_node();
        let key_read = chunk
            .descendants::<LuaIndexExpr>()
            .find(|index| index.syntax().text() == "value.ChildOnly")
            .expect("the key read");

        assert!(!is_assignment_target(&key_read));
    }

    #[test]
    fn unguarded_child_candidate_order_breaks_global_local_name_ties() {
        let global = LuaTypeDeclId::global("SharedName");
        let local = LuaTypeDeclId::local(FileId::new(1), "SharedName");

        assert_eq!(
            compare_unguarded_child_candidates(&global, "SharedName", &local, "SharedName"),
            Ordering::Less
        );
    }

    #[test]
    fn unguarded_child_candidate_order_breaks_local_file_name_ties() {
        let first = LuaTypeDeclId::local(FileId::new(1), "SharedName");
        let second = LuaTypeDeclId::local(FileId::new(2), "SharedName");

        assert_eq!(
            compare_unguarded_child_candidates(&first, "SharedName", &second, "SharedName"),
            Ordering::Less
        );
    }

    #[test]
    fn unguarded_child_looks_up_a_members_declarers_once_per_evaluation() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let threads = crate::compilation::analyzer::parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::compilation::analyzer::parallel::set_analysis_threads(1);
        super::DECLARER_LOOKUPS.with(|scans| scans.set(0));

        ws.def_files(vec![
            (
                "addons/scan_fixture/lua/autorun/sh_types.lua",
                r#"
                ---@class Vector
                ---@class Entity
                ---@class Player: Entity
                ---@field GetShootPos fun(self: Player): Vector
                ---@class NPC: Entity
                ---@class Holder
                ---@field proc Entity
                "#,
            ),
            (
                "addons/scan_fixture/lua/autorun/server/consumer.lua",
                r#"
                ---@type Entity
                local owner
                local a = owner:GetShootPos()
                local b = owner:GetShootPos()
                local c = owner:GetShootPos()

                ---@type Holder
                local holder
                local d = holder.proc:GetShootPos()
                local e = holder.proc:GetShootPos()
                local f = holder.proc:GetShootPos()
                "#,
            ),
        ]);

        crate::compilation::analyzer::parallel::set_analysis_threads(0);
        drop(threads);
        let evaluations = ws
            .analysis
            .compilation
            .last_stats()
            .evaluations(crate::db_index::derivation::DerivationKind::LocalFallbacks);
        let scans = super::DECLARER_LOOKUPS.with(std::cell::Cell::get);
        assert!(
            (1..=evaluations).contains(&scans),
            "{scans} declarer lookups over {evaluations} evaluations"
        );
    }

    #[test]
    fn unguarded_child_skips_the_declarer_lookup_for_members_the_base_has() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);
        let threads = crate::compilation::analyzer::parallel::THREADS
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::compilation::analyzer::parallel::set_analysis_threads(1);
        super::DECLARER_LOOKUPS.with(|scans| scans.set(0));

        ws.def_files(vec![
            (
                "addons/scan_fixture/lua/autorun/sh_types.lua",
                r#"
                ---@class Vector
                ---@class Entity
                ---@field GetPos fun(self: Entity): Vector
                ---@class Player: Entity
                ---@field GetPos fun(self: Player): Vector
                ---@class NPC: Entity
                ---@class Holder
                ---@field proc Entity
                "#,
            ),
            (
                "addons/scan_fixture/lua/autorun/server/consumer.lua",
                r#"
                ---@type Entity
                local owner
                local a = owner:GetPos()

                ---@type Holder
                local holder
                local b = holder.proc:GetPos()
                "#,
            ),
        ]);

        crate::compilation::analyzer::parallel::set_analysis_threads(0);
        drop(threads);
        assert_eq!(super::DECLARER_LOOKUPS.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn unguarded_child_parallel_files_keep_direct_and_nested_evidence() {
        let mut ws = VirtualWorkspace::new();
        let mut emmyrc = Emmyrc::default();
        emmyrc.gmod.enabled = true;
        ws.update_emmyrc(emmyrc);

        const TYPES: &str = r#"
            ---@class Vector
            ---@class Entity
            ---@class Player: Entity
            ---@field GetShootPos fun(self: Player): Vector
            ---@class Holder
            ---@field proc Entity
        "#;
        const CONSUMER: &str = r#"
            ---@type Entity
            local owner
            local position = owner:GetShootPos()

            ---@type Holder
            local holder
            local nested_position = holder.proc:GetShootPos()
        "#;
        let consumer_names = [
            "addons/parallel_fixture/lua/autorun/server/consumer_01.lua",
            "addons/parallel_fixture/lua/autorun/server/consumer_02.lua",
            "addons/parallel_fixture/lua/autorun/server/consumer_03.lua",
            "addons/parallel_fixture/lua/autorun/server/consumer_04.lua",
            "addons/parallel_fixture/lua/autorun/server/consumer_05.lua",
            "addons/parallel_fixture/lua/autorun/server/consumer_06.lua",
            "addons/parallel_fixture/lua/autorun/server/consumer_07.lua",
            "addons/parallel_fixture/lua/autorun/server/consumer_08.lua",
        ];
        let mut files = vec![("addons/parallel_fixture/lua/autorun/sh_types.lua", TYPES)];
        files.extend(consumer_names.iter().copied().map(|name| (name, CONSUMER)));
        ws.def_files(files);

        for file_name in consumer_names {
            let file_id = ws
                .analysis
                .get_file_id(&ws.virtual_url_generator.new_uri(file_name))
                .expect("consumer file");
            let owner_type = {
                let model = ws
                    .analysis
                    .compilation
                    .get_semantic_model(file_id)
                    .expect("consumer semantic model");
                let owner = model
                    .get_root()
                    .descendants::<LuaNameExpr>()
                    .filter(|expr| expr.get_name_text().as_deref() == Some("owner"))
                    .last()
                    .expect("owner expression");
                model
                    .get_semantic_info(owner.syntax().clone().into())
                    .expect("owner semantic info")
                    .display_typ()
                    .clone()
            };
            assert_eq!(ws.humanize_type(owner_type), "Player");

            let nested_type = {
                let model = ws
                    .analysis
                    .compilation
                    .get_semantic_model(file_id)
                    .expect("consumer semantic model");
                let receiver = model
                    .get_root()
                    .descendants::<LuaIndexExpr>()
                    .find(|expr| expr.syntax().text().to_string().trim() == "holder.proc")
                    .expect("nested receiver");
                model
                    .infer_expr(LuaExpr::IndexExpr(receiver))
                    .expect("nested receiver type")
            };
            assert_eq!(ws.humanize_type(nested_type), "Player");
        }
    }
}
