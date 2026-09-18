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
    LuaAssignStat, LuaAstNode, LuaBlock, LuaCallExpr, LuaExpr, LuaFuncStat, LuaSyntaxId,
    LuaTableField,
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
    ResolveResult, UnResolve, UnResolveCallClosureParams, UnResolveClosureReturn, UnResolveDecl,
    UnResolveMember, UnResolveParentAst, UnResolveParentClosureParams, UnResolveReturn,
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
enum ItemDesc {
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
    },
}

/// `LuaReturnPoint` minus its red nodes.
enum ReturnPointDesc {
    Expr(LuaSyntaxId),
    MuliExpr(Vec<LuaSyntaxId>),
    Nil,
    Error,
}

/// `UnResolveParentAst` minus its red nodes.
enum ParentAstDesc {
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
    fn file_id(&self) -> FileId {
        match self {
            ItemDesc::Decl { file_id, .. }
            | ItemDesc::Member { file_id, .. }
            | ItemDesc::IterDecl { file_id, .. }
            | ItemDesc::Return { file_id, .. }
            | ItemDesc::ClosureParams { file_id, .. }
            | ItemDesc::ClosureReturn { file_id, .. }
            | ItemDesc::ClosureParentParams { file_id, .. } => *file_id,
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
            parent_ast: match &params.parent_ast {
                UnResolveParentAst::LuaFuncStat(stat) => ParentAstDesc::FuncStat(syntax_id(stat)),
                UnResolveParentAst::LuaTableField(field) => {
                    ParentAstDesc::TableField(syntax_id(field))
                }
                UnResolveParentAst::LuaAssignStat(assign) => {
                    ParentAstDesc::AssignStat(syntax_id(assign))
                }
            },
        },
        _ => return None,
    })
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
