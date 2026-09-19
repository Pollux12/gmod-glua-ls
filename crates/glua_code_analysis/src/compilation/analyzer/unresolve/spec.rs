//! Speculative parallel compute for the unresolve wave drain.
//!
//! A drained reason group is attempted in sorted order because each item's
//! writes are visible to the items that follow it. Most items never read a
//! fact an earlier item writes, so the attempt is run on workers against the
//! shared index with its writes deferred into [`ResolveWrites`]; the caller
//! replays them in drain order. An item whose recorded reads were moved by an
//! earlier-applied write — or that read a slot after queueing its own write
//! to it — is re-run live, which is exactly what the serial drain would have
//! done.
//!
//! Semantics are preserved by construction: an applied item ran on identical
//! inputs to the live attempt (per-key write versions prove nothing it read
//! moved), and a stale item falls back to the live attempt. The only behavior
//! the compute half changes is which *thread* ran it — per-file infer caches
//! are worker-local and discarded, so no memoised result can leak between the
//! speculative and live paths.
//!
//! `UnResolve` items hold red syntax nodes, which are `!Send`, so every kind
//! crosses the boundary as a descriptor of ids (`LuaSyntaxId`, decl/member/
//! signature ids, token positions). The worker rebuilds the nodes against its
//! file's own red root and runs the real `try_resolve_*` — the read set it
//! records is exactly the one the live attempt would have produced.

use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaBlock, LuaCallExpr, LuaExpr, LuaFuncStat,
    LuaSyntaxId, LuaTableField,
};
use rayon::prelude::*;
use rowan::TextSize;
use rustc_hash::FxHashMap;

use crate::{
    FileId, LuaDeclId, LuaMemberId, LuaSignatureId, LuaType,
    compilation::analyzer::{
        infer_cache_manager::InferCacheManager, lua::LuaReturnPoint, parallel,
    },
    db_index::{
        DbIndex,
        read_set::{self, InferenceReadSet},
    },
};

use super::{
    CallSiteContributionKind, ResolveResult, UnResolve, UnResolveCallClosureParams,
    UnResolveCallSiteContribution, UnResolveClosureReturn, UnResolveDecl, UnResolveIterVar,
    UnResolveMember, UnResolveModule, UnResolveParentAst, UnResolveParentClosureParams,
    UnResolveReturn, UnResolveSpecialCall, UnResolveTableField,
    resolve::{
        try_resolve_decl, try_resolve_iter_var_updates, try_resolve_member,
        try_resolve_return_point,
    },
    resolve_closure::{
        try_resolve_call_closure_params, try_resolve_closure_parent_params,
        try_resolve_closure_return,
    },
    writes::ResolveWrites,
};

/// A `Send` description of one speculatable `UnResolve` item. The item
/// itself holds red syntax nodes and cannot cross a thread boundary, so the
/// worker rebuilds it against the file's own red root.
#[derive(Clone)]
pub(crate) enum ItemDesc {
    Decl {
        file_id: FileId,
        decl_id: LuaDeclId,
        expr: LuaSyntaxId,
        ret_idx: usize,
    },
    Member {
        file_id: FileId,
        member_id: LuaMemberId,
        expr: Option<LuaSyntaxId>,
        prefix: Option<LuaSyntaxId>,
        ret_idx: usize,
    },
    IterDecl {
        file_id: FileId,
        iter_exprs: Vec<LuaSyntaxId>,
        var_positions: Vec<TextSize>,
    },
    Return {
        file_id: FileId,
        signature_id: LuaSignatureId,
        body: Option<LuaSyntaxId>,
        return_points: Vec<ReturnPointDesc>,
    },
    ClosureParams {
        file_id: FileId,
        signature_id: LuaSignatureId,
        call_expr: LuaSyntaxId,
        param_idx: usize,
    },
    ClosureReturn {
        file_id: FileId,
        signature_id: LuaSignatureId,
        call_expr: LuaSyntaxId,
        param_idx: usize,
        body: Option<LuaSyntaxId>,
        return_points: Vec<ReturnPointDesc>,
    },
    ClosureParentParams {
        file_id: FileId,
        signature_id: LuaSignatureId,
        parent_ast: ParentAstDesc,
        /// Carried for the parallel walk's delta move: a queued retry keeps
        /// the flags it was queued with. The wave's own speculative items
        /// always rebuild these as `false`/`None`.
        needs_settled_retry: bool,
        source_signature_id: Option<LuaSignatureId>,
        last_source_return: Option<crate::LuaType>,
    },
    Module {
        file_id: FileId,
        expr: LuaSyntaxId,
    },
    TableField {
        file_id: FileId,
        table_expr: LuaSyntaxId,
        field: LuaSyntaxId,
        decl_feature: crate::LuaMemberFeature,
    },
    SpecialCall {
        file_id: FileId,
        call_expr: LuaSyntaxId,
    },
    CallSiteContribution {
        file_id: FileId,
        expr_file_id: FileId,
        expr_syntax_id: LuaSyntaxId,
        signature_id: LuaSignatureId,
        param_idx: usize,
        kind: CallSiteContributionKind,
    },
}

/// `LuaReturnPoint` minus its red nodes.
#[derive(Clone)]
pub(crate) enum ReturnPointDesc {
    Expr(LuaSyntaxId),
    MuliExpr(Vec<LuaSyntaxId>),
    Nil,
    Error,
}

/// `UnResolveParentAst` minus its red nodes.
#[derive(Clone)]
pub(crate) enum ParentAstDesc {
    FuncStat(LuaSyntaxId),
    TableField(LuaSyntaxId),
    AssignStat(LuaSyntaxId),
}

/// The buffered outcome of one worker-side attempt: the verdict the serial
/// drain would record plus the writes to replay, the reads to validate
/// before replaying them, and the item-field mutation the attempt made.
pub(super) struct SpecOutcome {
    pub result: ResolveResult,
    pub writes: ResolveWrites,
    pub reads: InferenceReadSet,
    /// Item fields the attempt mutated (a cleared member prefix, a queued
    /// settled-retry flag). The worker's item is discarded, so the caller
    /// replays the same mutation onto the real one.
    pub post_item: Option<Box<dyn FnOnce(&mut UnResolve) + Send>>,
}

fn syntax_id(node: &impl LuaAstNode) -> LuaSyntaxId {
    LuaSyntaxId::from_node(node.syntax())
}

fn return_point_desc(point: &LuaReturnPoint) -> ReturnPointDesc {
    match point {
        LuaReturnPoint::Expr(expr) => ReturnPointDesc::Expr(syntax_id(expr)),
        LuaReturnPoint::MuliExpr(exprs) => {
            ReturnPointDesc::MuliExpr(exprs.iter().map(syntax_id).collect())
        }
        LuaReturnPoint::Nil => ReturnPointDesc::Nil,
        LuaReturnPoint::Error => ReturnPointDesc::Error,
    }
}

/// Runs every speculatable item in `items` on workers and returns its
/// outcome keyed by the item's position in `items`. Unsupported kinds are
/// absent from the map; the caller attempts them live in drain order.
pub(super) fn speculate(
    db: &DbIndex,
    infer_manager: &InferCacheManager,
    items: &[UnResolve],
) -> FxHashMap<usize, SpecOutcome> {
    // Descriptors are extracted here because `items` holds red syntax nodes:
    // neither the items nor a shared slice of them may cross a thread
    // boundary.
    let mut by_file: FxHashMap<FileId, Vec<(usize, ItemDesc)>> = FxHashMap::default();
    for (idx, item) in items.iter().enumerate() {
        let Some(desc) = describe(item) else {
            continue;
        };
        by_file.entry(desc.file_id()).or_default().push((idx, desc));
    }
    if by_file.is_empty() {
        return FxHashMap::default();
    }

    let groups: Vec<(FileId, Vec<(usize, ItemDesc)>)> = by_file.into_iter().collect();
    let compute =
        |(file_id, descs): (FileId, Vec<(usize, ItemDesc)>)| -> Vec<(usize, SpecOutcome)> {
            compute_file(db, infer_manager, file_id, descs)
        };

    if parallel::runs_inline(groups.len()) {
        return groups.into_iter().flat_map(compute).collect();
    }

    parallel::init_pool();
    let mut computed = Vec::new();
    rayon::in_place_scope(|_| {
        computed = groups.into_par_iter().flat_map_iter(compute).collect();
    });
    computed.into_iter().collect()
}

impl ItemDesc {
    pub(crate) fn file_id(&self) -> FileId {
        match self {
            ItemDesc::Decl { file_id, .. }
            | ItemDesc::Member { file_id, .. }
            | ItemDesc::IterDecl { file_id, .. }
            | ItemDesc::Return { file_id, .. }
            | ItemDesc::ClosureParams { file_id, .. }
            | ItemDesc::ClosureReturn { file_id, .. }
            | ItemDesc::ClosureParentParams { file_id, .. }
            | ItemDesc::Module { file_id, .. }
            | ItemDesc::TableField { file_id, .. }
            | ItemDesc::SpecialCall { file_id, .. }
            | ItemDesc::CallSiteContribution { file_id, .. } => *file_id,
        }
    }
}

/// Extracts the `Send` descriptor for one item, or `None` for kinds the
/// speculative path does not cover (they stay on the live drain).
fn describe(item: &UnResolve) -> Option<ItemDesc> {
    Some(match item {
        UnResolve::Decl(decl) => ItemDesc::Decl {
            file_id: decl.file_id,
            decl_id: decl.decl_id,
            expr: syntax_id(&decl.expr),
            ret_idx: decl.ret_idx,
        },
        UnResolve::Member(member) => ItemDesc::Member {
            file_id: member.file_id,
            member_id: member.member_id,
            expr: member.expr.as_ref().map(syntax_id),
            prefix: member.prefix.as_ref().map(syntax_id),
            ret_idx: member.ret_idx,
        },
        UnResolve::IterDecl(iter) => ItemDesc::IterDecl {
            file_id: iter.file_id,
            iter_exprs: iter.iter_exprs.iter().map(syntax_id).collect(),
            var_positions: iter
                .iter_vars
                .iter()
                .map(glua_parser::LuaAstToken::get_position)
                .collect(),
        },
        UnResolve::Return(un_resolve_return) => ItemDesc::Return {
            file_id: un_resolve_return.file_id,
            signature_id: un_resolve_return.signature_id,
            body: un_resolve_return.body.as_ref().map(syntax_id),
            return_points: un_resolve_return
                .return_points
                .iter()
                .map(return_point_desc)
                .collect(),
        },
        UnResolve::ClosureParams(params) => ItemDesc::ClosureParams {
            file_id: params.file_id,
            signature_id: params.signature_id,
            call_expr: syntax_id(&params.call_expr),
            param_idx: params.param_idx,
        },
        UnResolve::ClosureReturn(closure_return) => ItemDesc::ClosureReturn {
            file_id: closure_return.file_id,
            signature_id: closure_return.signature_id,
            call_expr: syntax_id(&closure_return.call_expr),
            param_idx: closure_return.param_idx,
            body: closure_return.body.as_ref().map(syntax_id),
            return_points: closure_return
                .return_points
                .iter()
                .map(return_point_desc)
                .collect(),
        },
        UnResolve::ClosureParentParams(params) => ItemDesc::ClosureParentParams {
            file_id: params.file_id,
            signature_id: params.signature_id,
            parent_ast: parent_ast_desc(&params.parent_ast),
            needs_settled_retry: false,
            source_signature_id: None,
            last_source_return: None,
        },
        _ => return None,
    })
}

fn parent_ast_desc(parent_ast: &UnResolveParentAst) -> ParentAstDesc {
    match parent_ast {
        UnResolveParentAst::LuaFuncStat(stat) => ParentAstDesc::FuncStat(syntax_id(stat)),
        UnResolveParentAst::LuaTableField(field) => ParentAstDesc::TableField(syntax_id(field)),
        UnResolveParentAst::LuaAssignStat(assign) => ParentAstDesc::AssignStat(syntax_id(assign)),
    }
}

/// Runs one file's speculative items against a worker-local infer cache.
///
/// The cache is discarded when the group returns: it exists to keep repeated
/// inference within the file cheap, and dropping it means a stale item's
/// recompute can never observe a memoised answer the live drain would not
/// have produced.
fn compute_file(
    db: &DbIndex,
    infer_manager: &InferCacheManager,
    file_id: FileId,
    descs: Vec<(usize, ItemDesc)>,
) -> Vec<(usize, SpecOutcome)> {
    let Some(tree) = db.get_vfs().get_syntax_tree(&file_id) else {
        return Vec::new();
    };
    let root = tree.get_red_root();
    let mut cache = infer_manager.fresh_cache(file_id);
    descs
        .into_iter()
        .filter_map(|(idx, desc)| {
            read_set::arm_with_member_maps();
            let mut writes = ResolveWrites::new();
            let outcome = compute_item(db, &mut cache, &root, desc, &mut writes);
            let reads = read_set::disarm();
            let (result, post_item) = outcome?;
            Some((
                idx,
                SpecOutcome {
                    result,
                    writes,
                    reads,
                    post_item,
                },
            ))
        })
        .collect()
}

/// Hydrates one descriptor against the file's red root and runs the real
/// `try_resolve_*`. `None` skips the item — a node the syntax id cannot
/// rebuild means the live drain has to look at it instead.
fn compute_item(
    db: &DbIndex,
    cache: &mut crate::LuaInferCache,
    root: &glua_parser::LuaSyntaxNode,
    desc: ItemDesc,
    writes: &mut ResolveWrites,
) -> Option<(
    ResolveResult,
    Option<Box<dyn FnOnce(&mut UnResolve) + Send>>,
)> {
    let file_id = desc.file_id();
    let outcome = match desc {
        ItemDesc::Decl {
            decl_id,
            expr,
            ret_idx,
            ..
        } => {
            let expr = expr.to_node_from_root(root).and_then(LuaExpr::cast)?;
            let mut item = UnResolveDecl {
                file_id,
                decl_id,
                expr,
                ret_idx,
            };
            (try_resolve_decl(db, writes, cache, &mut item), None)
        }
        ItemDesc::Member {
            member_id,
            expr,
            prefix,
            ret_idx,
            ..
        } => {
            let expr = hydrate_opt(root, expr)?;
            let prefix = hydrate_opt(root, prefix)?;
            let mut item = UnResolveMember {
                file_id,
                member_id,
                expr,
                prefix,
                ret_idx,
            };
            let result = try_resolve_member(db, writes, cache, &mut item);
            // The serial attempt clears `prefix` once ownership resolves;
            // replay that onto the real item only if the worker's run did.
            let post_item = item
                .prefix
                .is_none()
                .then_some(Box::new(|unresolve: &mut UnResolve| {
                    if let UnResolve::Member(member) = unresolve {
                        member.prefix = None;
                    }
                }) as Box<dyn FnOnce(&mut UnResolve) + Send>);
            (result, post_item)
        }
        ItemDesc::IterDecl {
            iter_exprs,
            var_positions,
            ..
        } => {
            let iter_exprs = iter_exprs
                .iter()
                .map(|id| id.to_node_from_root(root).and_then(LuaExpr::cast))
                .collect::<Option<Vec<_>>>()?;
            (
                try_resolve_iter_var_updates(
                    db,
                    writes,
                    cache,
                    file_id,
                    &iter_exprs,
                    &var_positions,
                ),
                None,
            )
        }
        ItemDesc::Return {
            signature_id,
            body,
            return_points,
            ..
        } => {
            let mut item = UnResolveReturn {
                file_id,
                signature_id,
                body: hydrate_block(root, body)?,
                return_points: hydrate_return_points(root, return_points)?,
            };
            (try_resolve_return_point(db, writes, cache, &mut item), None)
        }
        ItemDesc::ClosureParams {
            signature_id,
            call_expr,
            param_idx,
            ..
        } => {
            let call_expr = call_expr
                .to_node_from_root(root)
                .and_then(LuaCallExpr::cast)?;
            let mut item = UnResolveCallClosureParams {
                file_id,
                signature_id,
                call_expr,
                param_idx,
            };
            (
                try_resolve_call_closure_params(db, writes, cache, &mut item),
                None,
            )
        }
        ItemDesc::ClosureReturn {
            signature_id,
            call_expr,
            param_idx,
            body,
            return_points,
            ..
        } => {
            let call_expr = call_expr
                .to_node_from_root(root)
                .and_then(LuaCallExpr::cast)?;
            let mut item = UnResolveClosureReturn {
                file_id,
                signature_id,
                call_expr,
                param_idx,
                body: hydrate_block(root, body)?,
                return_points: hydrate_return_points(root, return_points)?,
            };
            (
                try_resolve_closure_return(db, writes, cache, &mut item),
                None,
            )
        }
        ItemDesc::ClosureParentParams {
            signature_id,
            parent_ast,
            ..
        } => {
            let parent_ast = match parent_ast {
                ParentAstDesc::FuncStat(id) => UnResolveParentAst::LuaFuncStat(
                    id.to_node_from_root(root).and_then(LuaFuncStat::cast)?,
                ),
                ParentAstDesc::TableField(id) => UnResolveParentAst::LuaTableField(
                    id.to_node_from_root(root).and_then(LuaTableField::cast)?,
                ),
                ParentAstDesc::AssignStat(id) => UnResolveParentAst::LuaAssignStat(
                    id.to_node_from_root(root).and_then(LuaAssignStat::cast)?,
                ),
            };
            let mut item = UnResolveParentClosureParams {
                file_id,
                signature_id,
                parent_ast,
                needs_settled_retry: false,
                source_signature_id: None,
                last_source_return: None,
            };
            let result = try_resolve_closure_parent_params(db, writes, cache, &mut item);
            // The retry fields are part of the attempt's verdict — replay
            // whatever the worker's run settled them to.
            let needs_settled_retry = item.needs_settled_retry;
            let source_signature_id = item.source_signature_id;
            let last_source_return: Option<LuaType> = item.last_source_return.clone();
            let post_item = Box::new(move |unresolve: &mut UnResolve| {
                if let UnResolve::ClosureParentParams(params) = unresolve {
                    params.needs_settled_retry = needs_settled_retry;
                    params.source_signature_id = source_signature_id;
                    params.last_source_return = last_source_return;
                }
            }) as Box<dyn FnOnce(&mut UnResolve) + Send>;
            (result, Some(post_item))
        }
        // The walk's delta move carries every variant, but the wave only
        // speculates the kinds `describe` produces — these stay on the live
        // drain.
        ItemDesc::Module { .. }
        | ItemDesc::TableField { .. }
        | ItemDesc::SpecialCall { .. }
        | ItemDesc::CallSiteContribution { .. } => return None,
    };
    Some(outcome)
}

/// `Option<sid>` hydration: `None` in the descriptor stays `None` on the
/// item, while an id that fails to rebuild skips the whole item.
fn hydrate_opt<T: LuaAstNode>(
    root: &glua_parser::LuaSyntaxNode,
    id: Option<LuaSyntaxId>,
) -> Option<Option<T>> {
    id.map(|id| id.to_node_from_root(root).and_then(T::cast))
        .or(Some(None))
}

fn hydrate_block(
    root: &glua_parser::LuaSyntaxNode,
    body: Option<LuaSyntaxId>,
) -> Option<Option<LuaBlock>> {
    hydrate_opt(root, body)
}

fn hydrate_return_points(
    root: &glua_parser::LuaSyntaxNode,
    points: Vec<ReturnPointDesc>,
) -> Option<Vec<LuaReturnPoint>> {
    points
        .into_iter()
        .map(|point| {
            Some(match point {
                ReturnPointDesc::Expr(id) => {
                    LuaReturnPoint::Expr(id.to_node_from_root(root).and_then(LuaExpr::cast)?)
                }
                ReturnPointDesc::MuliExpr(ids) => LuaReturnPoint::MuliExpr(
                    ids.iter()
                        .map(|id| id.to_node_from_root(root).and_then(LuaExpr::cast))
                        .collect::<Option<Vec<_>>>()?,
                ),
                ReturnPointDesc::Nil => LuaReturnPoint::Nil,
                ReturnPointDesc::Error => LuaReturnPoint::Error,
            })
        })
        .collect()
}

/// Like [`describe`] but covering every `UnResolve` variant: the parallel
/// file walk moves its produced unresolves across threads as descriptors,
/// and an undescribable variant would force the file's serial re-run.
pub(crate) fn describe_walk(item: &UnResolve) -> ItemDesc {
    match item {
        UnResolve::Module(module) => ItemDesc::Module {
            file_id: module.file_id,
            expr: syntax_id(&module.expr),
        },
        UnResolve::TableField(field) => ItemDesc::TableField {
            file_id: field.file_id,
            table_expr: syntax_id(&field.table_expr),
            field: syntax_id(&field.field),
            decl_feature: field.decl_feature,
        },
        UnResolve::SpecialCall(call) => ItemDesc::SpecialCall {
            file_id: call.file_id,
            call_expr: syntax_id(&call.call_expr),
        },
        UnResolve::CallSiteContribution(contribution) => ItemDesc::CallSiteContribution {
            file_id: contribution.file_id,
            expr_file_id: contribution.expr_file_id,
            expr_syntax_id: contribution.expr_syntax_id,
            signature_id: contribution.signature_id,
            param_idx: contribution.param_idx,
            kind: contribution.kind,
        },
        UnResolve::ClosureParentParams(params) => ItemDesc::ClosureParentParams {
            file_id: params.file_id,
            signature_id: params.signature_id,
            parent_ast: parent_ast_desc(&params.parent_ast),
            needs_settled_retry: params.needs_settled_retry,
            source_signature_id: params.source_signature_id,
            last_source_return: params.last_source_return.clone(),
        },
        _ => describe(item).expect("describe covers the variants describe_walk delegates"),
    }
}

impl ItemDesc {
    /// Rebuilds the `UnResolve` this descriptor names against `root`.
    /// `None` only when a syntax id cannot be rebuilt — impossible for a
    /// descriptor produced against the same tree, so the caller treats it
    /// as a bug guard rather than a fallback.
    pub(crate) fn hydrate(&self, root: &glua_parser::LuaSyntaxNode) -> Option<UnResolve> {
        let item = match self {
            ItemDesc::Decl {
                file_id,
                decl_id,
                expr,
                ret_idx,
            } => UnResolve::Decl(Box::new(UnResolveDecl {
                file_id: *file_id,
                decl_id: *decl_id,
                expr: expr.to_node_from_root(root).and_then(LuaExpr::cast)?,
                ret_idx: *ret_idx,
            })),
            ItemDesc::Member {
                file_id,
                member_id,
                expr,
                prefix,
                ret_idx,
            } => UnResolve::Member(Box::new(UnResolveMember {
                file_id: *file_id,
                member_id: *member_id,
                expr: hydrate_opt(root, *expr)?,
                prefix: hydrate_opt(root, *prefix)?,
                ret_idx: *ret_idx,
            })),
            ItemDesc::IterDecl {
                file_id,
                iter_exprs,
                var_positions,
            } => UnResolve::IterDecl(Box::new(UnResolveIterVar {
                file_id: *file_id,
                iter_exprs: iter_exprs
                    .iter()
                    .map(|id| id.to_node_from_root(root).and_then(LuaExpr::cast))
                    .collect::<Option<Vec<_>>>()?,
                iter_vars: var_positions
                    .iter()
                    .map(|position| {
                        let token = match root.token_at_offset(*position) {
                            rowan::TokenAtOffset::Single(token) => token,
                            rowan::TokenAtOffset::Between(_, right) => right,
                            rowan::TokenAtOffset::None => return None,
                        };
                        glua_parser::LuaNameToken::cast(token)
                    })
                    .collect::<Option<Vec<_>>>()?,
            })),
            ItemDesc::Return {
                file_id,
                signature_id,
                body,
                return_points,
            } => UnResolve::Return(Box::new(UnResolveReturn {
                file_id: *file_id,
                signature_id: *signature_id,
                body: hydrate_block(root, *body)?,
                return_points: hydrate_return_points(root, return_points.clone())?,
            })),
            ItemDesc::ClosureParams {
                file_id,
                signature_id,
                call_expr,
                param_idx,
            } => UnResolve::ClosureParams(Box::new(UnResolveCallClosureParams {
                file_id: *file_id,
                signature_id: *signature_id,
                call_expr: call_expr
                    .to_node_from_root(root)
                    .and_then(LuaCallExpr::cast)?,
                param_idx: *param_idx,
            })),
            ItemDesc::ClosureReturn {
                file_id,
                signature_id,
                call_expr,
                param_idx,
                body,
                return_points,
            } => UnResolve::ClosureReturn(Box::new(UnResolveClosureReturn {
                file_id: *file_id,
                signature_id: *signature_id,
                call_expr: call_expr
                    .to_node_from_root(root)
                    .and_then(LuaCallExpr::cast)?,
                param_idx: *param_idx,
                body: hydrate_block(root, *body)?,
                return_points: hydrate_return_points(root, return_points.clone())?,
            })),
            ItemDesc::ClosureParentParams {
                file_id,
                signature_id,
                parent_ast,
                needs_settled_retry,
                source_signature_id,
                last_source_return,
            } => UnResolve::ClosureParentParams(Box::new(UnResolveParentClosureParams {
                file_id: *file_id,
                signature_id: *signature_id,
                parent_ast: match parent_ast {
                    ParentAstDesc::FuncStat(id) => UnResolveParentAst::LuaFuncStat(
                        id.to_node_from_root(root).and_then(LuaFuncStat::cast)?,
                    ),
                    ParentAstDesc::TableField(id) => UnResolveParentAst::LuaTableField(
                        id.to_node_from_root(root).and_then(LuaTableField::cast)?,
                    ),
                    ParentAstDesc::AssignStat(id) => UnResolveParentAst::LuaAssignStat(
                        id.to_node_from_root(root).and_then(LuaAssignStat::cast)?,
                    ),
                },
                needs_settled_retry: *needs_settled_retry,
                source_signature_id: *source_signature_id,
                last_source_return: last_source_return.clone(),
            })),
            ItemDesc::Module { file_id, expr } => UnResolve::Module(Box::new(UnResolveModule {
                file_id: *file_id,
                expr: expr.to_node_from_root(root).and_then(LuaExpr::cast)?,
            })),
            ItemDesc::TableField {
                file_id,
                table_expr,
                field,
                decl_feature,
            } => UnResolve::TableField(Box::new(UnResolveTableField {
                file_id: *file_id,
                table_expr: table_expr
                    .to_node_from_root(root)
                    .and_then(glua_parser::LuaTableExpr::cast)?,
                field: field
                    .to_node_from_root(root)
                    .and_then(LuaTableField::cast)?,
                decl_feature: *decl_feature,
            })),
            ItemDesc::SpecialCall { file_id, call_expr } => {
                UnResolve::SpecialCall(Box::new(UnResolveSpecialCall {
                    file_id: *file_id,
                    call_expr: call_expr
                        .to_node_from_root(root)
                        .and_then(LuaCallExpr::cast)?,
                }))
            }
            ItemDesc::CallSiteContribution {
                file_id,
                expr_file_id,
                expr_syntax_id,
                signature_id,
                param_idx,
                kind,
            } => UnResolve::CallSiteContribution(Box::new(UnResolveCallSiteContribution {
                file_id: *file_id,
                expr_file_id: *expr_file_id,
                expr_syntax_id: *expr_syntax_id,
                signature_id: *signature_id,
                param_idx: *param_idx,
                kind: *kind,
            })),
        };
        Some(item)
    }
}
