mod find_decl_function;
mod resolve;
mod resolve_closure;
mod writes;

use std::cmp::Ordering;

use crate::{
    FileId, InferFailReason, LuaInferCache, LuaType,
    compilation::analyzer::unresolve::resolve::try_resolve_special_call,
    db_index::{DbIndex, LuaDeclId, LuaMemberId, LuaSignatureId},
};
use glua_parser::{LuaAssignStat, LuaAstNode, LuaCallExpr, LuaExpr, LuaFuncStat, LuaTableField};
use resolve::{try_resolve_decl, try_resolve_module};
use resolve_closure::{try_resolve_call_closure_params, try_resolve_closure_return};

pub(crate) use resolve::get_wrapped_callable_target_expr;
pub(crate) use resolve::is_unsettled_inferred_return;
pub(crate) use resolve::iter_var_updates;
pub(crate) use resolve::{try_resolve_member, try_resolve_return_point};
pub(crate) use resolve_closure::callback_contract_for_arg;
pub use resolve_closure::extract_hook_name;
pub(crate) use resolve_closure::try_resolve_closure_parent_params;
pub use resolve_closure::{
    resolve_gmod_hook_add_callback_doc_function, resolve_gmod_hook_callback_doc_function,
};

use super::{AnalyzeContext, infer_cache_manager::InferCacheManager, lua::LuaReturnPoint};

type ResolveResult = Result<(), InferFailReason>;

/// Runs a buffered attempt serially: the resolver computes against the live
/// index and its deferred writes land immediately after, which is exactly the
/// inline mutation the unconverted kinds still do.
fn buffered_attempt(
    db: &mut DbIndex,
    infer_manager: &mut InferCacheManager,
    file_id: FileId,
    attempt: impl FnOnce(&DbIndex, &mut writes::ResolveWrites, &mut LuaInferCache) -> ResolveResult,
) -> ResolveResult {
    let cache = infer_manager.get_infer_cache(file_id);
    let mut writes = writes::ResolveWrites::new();
    let result = attempt(db, &mut writes, cache);
    writes.apply(db, infer_manager);
    result
}

/// Resolves every item the current evaluation deferred, against the index as
/// it stands. An item that still cannot be answered stays unanswered: the
/// engine re-evaluates its site once a fact it read changes.
pub(in crate::compilation::analyzer) fn resolve_inline(
    db: &mut DbIndex,
    context: &mut AnalyzeContext,
) {
    while !context.unresolves.is_empty() {
        let mut items = std::mem::take(&mut context.unresolves);
        items.sort_unstable_by(|(a, _), (b, _)| unresolve_stable_cmp(a, b));
        for (mut item, _) in items {
            let Some(file_id) = item.get_file_id() else {
                continue;
            };
            let _ = attempt_resolve(db, &mut context.infer_manager, file_id, &mut item);
        }
    }
}

fn attempt_resolve(
    db: &mut DbIndex,
    infer_manager: &mut InferCacheManager,
    file_id: FileId,
    unresolve: &mut UnResolve,
) -> ResolveResult {
    match unresolve {
        UnResolve::Decl(un_resolve_decl) => {
            buffered_attempt(db, infer_manager, file_id, |db, writes, cache| {
                try_resolve_decl(db, writes, cache, un_resolve_decl)
            })
        }
        UnResolve::Member(un_resolve_member) => {
            // The prefix's writes land before the value is inferred, so a
            // value that reads the member (`self.x = self.x or {}`) sees it
            // attached, as it would through a separate attach item.
            if un_resolve_member.prefix.is_some()
                && let Some(expr) = un_resolve_member.expr.take()
            {
                let attached = buffered_attempt(db, infer_manager, file_id, |db, writes, cache| {
                    try_resolve_member(db, writes, cache, un_resolve_member)
                });
                un_resolve_member.expr = Some(expr);
                attached?;
            }
            buffered_attempt(db, infer_manager, file_id, |db, writes, cache| {
                try_resolve_member(db, writes, cache, un_resolve_member)
            })
        }
        UnResolve::Module(un_resolve_module) => {
            let cache = infer_manager.get_infer_cache(file_id);
            try_resolve_module(db, cache, un_resolve_module)
        }
        UnResolve::Return(un_resolve_return) => {
            buffered_attempt(db, infer_manager, file_id, |db, writes, cache| {
                try_resolve_return_point(db, writes, cache, un_resolve_return)
            })
        }
        UnResolve::ClosureParams(un_resolve_closure_params) => {
            buffered_attempt(db, infer_manager, file_id, |db, writes, cache| {
                try_resolve_call_closure_params(db, writes, cache, un_resolve_closure_params)
            })
        }
        UnResolve::ClosureReturn(un_resolve_closure_return) => {
            buffered_attempt(db, infer_manager, file_id, |db, writes, cache| {
                try_resolve_closure_return(db, writes, cache, un_resolve_closure_return)
            })
        }
        UnResolve::ClosureParentParams(un_resolve_closure_params) => {
            buffered_attempt(db, infer_manager, file_id, |db, writes, cache| {
                try_resolve_closure_parent_params(db, writes, cache, un_resolve_closure_params)
            })
        }
        UnResolve::SpecialCall(un_resolve_special_call) => {
            let cache = infer_manager.get_infer_cache(file_id);
            try_resolve_special_call(db, cache, un_resolve_special_call)
        }
    }
}

fn unresolve_kind_rank(unresolve: &UnResolve) -> u8 {
    match unresolve {
        UnResolve::Decl(_) => 1,
        UnResolve::Member(_) => 2,
        UnResolve::Module(_) => 3,
        UnResolve::Return(_) => 4,
        UnResolve::ClosureParams(_) => 5,
        UnResolve::ClosureReturn(_) => 6,
        UnResolve::ClosureParentParams(_) => 7,
        UnResolve::SpecialCall(_) => 8,
    }
}

fn unresolve_stable_cmp(a: &UnResolve, b: &UnResolve) -> Ordering {
    unresolve_kind_rank(a)
        .cmp(&unresolve_kind_rank(b))
        .then_with(|| a.sort_key().cmp(&b.sort_key()))
}

#[derive(Debug)]
pub enum UnResolve {
    Decl(Box<UnResolveDecl>),
    Member(Box<UnResolveMember>),
    Module(Box<UnResolveModule>),
    Return(Box<UnResolveReturn>),
    ClosureParams(Box<UnResolveCallClosureParams>),
    ClosureReturn(Box<UnResolveClosureReturn>),
    ClosureParentParams(Box<UnResolveParentClosureParams>),
    SpecialCall(Box<UnResolveSpecialCall>),
}

#[allow(dead_code)]
impl UnResolve {
    pub fn get_file_id(&self) -> Option<FileId> {
        match self {
            UnResolve::Decl(un_resolve_decl) => Some(un_resolve_decl.file_id),
            UnResolve::Member(un_resolve_member) => Some(un_resolve_member.file_id),
            UnResolve::Module(un_resolve_module) => Some(un_resolve_module.file_id),
            UnResolve::Return(un_resolve_return) => Some(un_resolve_return.file_id),
            UnResolve::ClosureParams(un_resolve_closure_params) => {
                Some(un_resolve_closure_params.file_id)
            }
            UnResolve::ClosureReturn(un_resolve_closure_return) => {
                Some(un_resolve_closure_return.file_id)
            }
            UnResolve::ClosureParentParams(un_resolve_closure_params) => {
                Some(un_resolve_closure_params.file_id)
            }
            UnResolve::SpecialCall(un_resolve_special_call) => {
                Some(un_resolve_special_call.file_id)
            }
        }
    }

    /// The declaration or member this item writes to, when narrowing can read
    /// it. Used to tell whether a resolution settled a value the flow walk
    /// could still have been reading as undetermined.
    pub fn narrowing_root(&self) -> Option<crate::semantic::VarRefCacheRootKey> {
        match self {
            UnResolve::Decl(decl) => Some(crate::semantic::VarRefCacheRootKey::Decl(decl.decl_id)),
            UnResolve::Member(member) => Some(crate::semantic::VarRefCacheRootKey::Member(
                member.member_id,
            )),
            _ => None,
        }
    }

    /// Returns a deterministic sort key (file_id, text_position, order,
    /// write_position) for stable ordering. This ensures unresolves are
    /// processed in a consistent order regardless of FxHashMap iteration order
    /// or other non-deterministic sources during collection. A local with
    /// several deferred writes carries one item per write, and which of them
    /// binds last decides the cache, so the write's own position breaks the
    /// tie in source order.
    ///
    /// A member can likewise carry two items for the same site: the
    /// owner-attach item `set_index_expr_owner` queues while the prefix cannot
    /// be inferred, and the value item the assignment queues while the
    /// right-hand side cannot. Both sort on the member's position, so without
    /// a further key their relative order is whatever `sort_unstable` makes of
    /// the push order — which retained index state perturbs. The attach must
    /// drain first, matching the serial walk: once the member exists, a value
    /// read back through the same index path sees a pending member rather than
    /// an absent one, and does not commit an absence-derived answer.
    fn sort_key(&self) -> (u32, u32, u32, u32) {
        let (file_id, position) = self.sort_key_site();
        let (order, write_position) = match self {
            UnResolve::Decl(d) => (0, u32::from(d.expr.syntax().text_range().start())),
            UnResolve::Member(m) => (u32::from(m.prefix.is_none()), m.ret_idx as u32),
            _ => (0, 0),
        };
        (file_id, position, order, write_position)
    }

    fn sort_key_site(&self) -> (u32, u32) {
        match self {
            UnResolve::Decl(d) => (d.file_id.id, u32::from(d.decl_id.position)),
            UnResolve::Member(d) => (d.file_id.id, u32::from(d.member_id.get_position())),
            UnResolve::Module(d) => (
                d.file_id.id,
                u32::from(d.expr.syntax().text_range().start()),
            ),
            UnResolve::Return(d) => (d.file_id.id, u32::from(d.signature_id.get_position())),
            UnResolve::ClosureParams(d) => (
                d.file_id.id,
                u32::from(d.call_expr.syntax().text_range().start()),
            ),
            UnResolve::ClosureReturn(d) => (
                d.file_id.id,
                u32::from(d.call_expr.syntax().text_range().start()),
            ),
            UnResolve::ClosureParentParams(d) => {
                (d.file_id.id, u32::from(d.signature_id.get_position()))
            }
            UnResolve::SpecialCall(d) => (
                d.file_id.id,
                u32::from(d.call_expr.syntax().text_range().start()),
            ),
        }
    }
}

#[derive(Debug)]
pub struct UnResolveDecl {
    pub file_id: FileId,
    pub decl_id: LuaDeclId,
    pub expr: LuaExpr,
    pub ret_idx: usize,
}

impl From<UnResolveDecl> for UnResolve {
    fn from(un_resolve_decl: UnResolveDecl) -> Self {
        UnResolve::Decl(Box::new(un_resolve_decl))
    }
}

#[derive(Debug)]
pub struct UnResolveMember {
    pub file_id: FileId,
    pub member_id: LuaMemberId,
    pub expr: Option<LuaExpr>,
    pub prefix: Option<LuaExpr>,
    pub ret_idx: usize,
}

impl From<UnResolveMember> for UnResolve {
    fn from(un_resolve_member: UnResolveMember) -> Self {
        UnResolve::Member(Box::new(un_resolve_member))
    }
}

#[derive(Debug)]
pub struct UnResolveModule {
    pub file_id: FileId,
    pub expr: LuaExpr,
}

impl From<UnResolveModule> for UnResolve {
    fn from(un_resolve_module: UnResolveModule) -> Self {
        UnResolve::Module(Box::new(un_resolve_module))
    }
}

#[derive(Debug, Clone)]
pub struct UnResolveReturn {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub return_points: Vec<LuaReturnPoint>,
}

impl From<UnResolveReturn> for UnResolve {
    fn from(un_resolve_return: UnResolveReturn) -> Self {
        UnResolve::Return(Box::new(un_resolve_return))
    }
}

#[derive(Debug)]
pub struct UnResolveCallClosureParams {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub call_expr: LuaCallExpr,
    pub param_idx: usize,
}

impl From<UnResolveCallClosureParams> for UnResolve {
    fn from(un_resolve_closure_params: UnResolveCallClosureParams) -> Self {
        UnResolve::ClosureParams(Box::new(un_resolve_closure_params))
    }
}

#[derive(Debug)]
pub struct UnResolveClosureReturn {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub call_expr: LuaCallExpr,
    pub param_idx: usize,
    pub return_points: Vec<LuaReturnPoint>,
}

impl From<UnResolveClosureReturn> for UnResolve {
    fn from(un_resolve_closure_return: UnResolveClosureReturn) -> Self {
        UnResolve::ClosureReturn(Box::new(un_resolve_closure_return))
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone)]
pub enum UnResolveParentAst {
    LuaFuncStat(LuaFuncStat),
    LuaTableField(LuaTableField),
    LuaAssignStat(LuaAssignStat),
}

#[derive(Debug, Clone)]
pub struct UnResolveParentClosureParams {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub parent_ast: UnResolveParentAst,
    /// Set when the inherited contract was installed while the source
    /// signature's return still held unsettled slots; the item retired with a
    /// partial answer and joins the settled-tail retry queue.
    pub needs_settled_retry: bool,
    /// The signature the contract was copied from, and the source return as
    /// seen when the candidate was queued or last retried. The source's own
    /// inferred return can keep moving through slot values that read as
    /// settled (a transient `nil`), so the retry stays queued until the
    /// source is stable and the copy mirrors it.
    pub source_signature_id: Option<LuaSignatureId>,
    pub last_source_return: Option<LuaType>,
}

impl From<UnResolveParentClosureParams> for UnResolve {
    fn from(un_resolve_closure_params: UnResolveParentClosureParams) -> Self {
        UnResolve::ClosureParentParams(Box::new(un_resolve_closure_params))
    }
}

#[derive(Debug)]
pub struct UnResolveSpecialCall {
    pub file_id: FileId,
    pub call_expr: LuaCallExpr,
}

impl From<UnResolveSpecialCall> for UnResolve {
    fn from(un_resolve_special_call: UnResolveSpecialCall) -> Self {
        UnResolve::SpecialCall(Box::new(un_resolve_special_call))
    }
}

#[cfg(test)]
mod tests {
    use glua_parser::{LuaAstNode, LuaExpr, LuaIndexExpr, LuaParser, ParserConfig};

    use crate::{FileId, LuaMemberId};

    use super::{UnResolve, UnResolveMember, unresolve_stable_cmp};

    /// A member whose owner attach and value write both deferred carries two
    /// items at the same sort position: `prefix`-only (attach) and
    /// `expr`-only (value). The attach must drain first — it is the order the
    /// serial walk uses, and a value read back through the member's own index
    /// path (`row[y] = (row[y] or {})`-shaped loops) must see the member as
    /// pending rather than absent. With equal keys `sort_unstable` leaves the
    /// order to the push order, which retained index state perturbs; the sort
    /// key itself has to distinguish them.
    #[test]
    fn member_attach_item_sorts_before_value_item() {
        let tree = LuaParser::parse("t[k] = v", ParserConfig::default());
        let index_expr = tree
            .get_chunk_node()
            .descendants::<LuaIndexExpr>()
            .next()
            .expect("index expr");
        let file_id = FileId::new(1);
        let member_id = LuaMemberId::new(index_expr.get_syntax_id(), file_id);
        let attach = || {
            UnResolveMember {
                file_id,
                member_id,
                expr: None,
                prefix: index_expr.get_prefix_expr(),
                ret_idx: 0,
            }
            .into()
        };
        let value = || {
            UnResolveMember {
                file_id,
                member_id,
                expr: Some(LuaExpr::IndexExpr(index_expr.clone())),
                prefix: None,
                ret_idx: 0,
            }
            .into()
        };

        for items in [vec![attach(), value()], vec![value(), attach()]] {
            let mut sorted = items;
            sorted.sort_unstable_by(unresolve_stable_cmp);
            let UnResolve::Member(first) = &sorted[0] else {
                panic!("expected member item");
            };
            assert!(
                first.prefix.is_some() && first.expr.is_none(),
                "owner attach must drain before the value write"
            );
        }
    }
}
