mod check_reason;
mod find_decl_function;
mod resolve;
mod resolve_closure;

use std::cmp::Ordering;
use std::collections::HashMap;

use crate::{
    FileId, InferFailReason, LuaDeclTypeKind, LuaMemberFeature, LuaSemanticDeclId, LuaTypeDecl,
    LuaTypeFlag,
    compilation::analyzer::{AnalysisPipeline, unresolve::resolve::try_resolve_special_call},
    db_index::{DbIndex, LuaDeclId, LuaMemberId, LuaSignatureId},
    profile::Profile,
};
use check_reason::{check_reach_reason, resolve_all_reason};
use glua_parser::{
    LuaAssignStat, LuaAstNode, LuaAstToken, LuaCallExpr, LuaExpr, LuaFuncStat, LuaNameToken,
    LuaTableExpr, LuaTableField,
};
use resolve::{
    try_resolve_decl, try_resolve_iter_var, try_resolve_member, try_resolve_module,
    try_resolve_module_ref, try_resolve_return_point, try_resolve_table_field,
};
use resolve_closure::{
    try_resolve_call_closure_params, try_resolve_closure_parent_params, try_resolve_closure_return,
};

pub(crate) use resolve::get_wrapped_callable_target_expr;
pub use resolve_closure::extract_hook_name;
pub use resolve_closure::resolve_gmod_hook_add_callback_doc_function;
use rowan::TextRange;

use super::{AnalyzeContext, infer_cache_manager::InferCacheManager, lua::LuaReturnPoint};

type ResolveResult = Result<(), InferFailReason>;

pub struct UnResolveAnalysisPipeline;

impl AnalysisPipeline for UnResolveAnalysisPipeline {
    fn analyze(db: &mut DbIndex, context: &mut AnalyzeContext) {
        let _p = Profile::cond_new("resolve analyze", context.tree_list.len() > 1);
        let mut infer_manager = std::mem::take(&mut context.infer_manager);

        let mat_start = std::time::Instant::now();
        materialize_pending_str_tpl_type_decls(db, &mut infer_manager);
        log::info!(
            "unresolve: initial materialize_pending cost {:?}",
            mat_start.elapsed()
        );

        infer_manager.clear();

        // Use HashMap for O(1) reason grouping (matching upstream)
        let mut reason_resolve: HashMap<InferFailReason, Vec<UnResolve>> = HashMap::new();
        for (unresolve, reason) in context.unresolves.drain(..) {
            reason_resolve.entry(reason).or_default().push(unresolve);
        }

        let total_unresolves: usize = reason_resolve.values().map(|v| v.len()).sum();
        log::info!(
            "unresolve: starting with {} unresolves in {} reason groups",
            total_unresolves,
            reason_resolve.len()
        );

        let mut loop_count = 0;
        while !reason_resolve.is_empty() {
            let iter_start = std::time::Instant::now();

            let resolve_start = std::time::Instant::now();
            try_resolve(db, &mut infer_manager, &mut reason_resolve);
            log::info!(
                "unresolve: loop {} try_resolve cost {:?}",
                loop_count,
                resolve_start.elapsed()
            );

            let mat_start = std::time::Instant::now();
            materialize_pending_str_tpl_type_decls(db, &mut infer_manager);
            log::info!(
                "unresolve: loop {} materialize_pending cost {:?}",
                loop_count,
                mat_start.elapsed()
            );

            if reason_resolve.is_empty() {
                log::info!(
                    "unresolve: loop {} total cost {:?} (resolved all)",
                    loop_count,
                    iter_start.elapsed()
                );
                break;
            }

            let remaining: usize = reason_resolve.values().map(|v| v.len()).sum();
            log::info!(
                "unresolve: loop {} remaining {} unresolves",
                loop_count,
                remaining
            );

            if loop_count == 0 {
                infer_manager.set_force();
            }

            let reason_start = std::time::Instant::now();
            resolve_all_reason(db, &mut reason_resolve, loop_count);
            log::info!(
                "unresolve: loop {} resolve_all_reason cost {:?}",
                loop_count,
                reason_start.elapsed()
            );

            log::info!(
                "unresolve: loop {} total cost {:?}",
                loop_count,
                iter_start.elapsed()
            );

            if loop_count >= 5 {
                break;
            }
            loop_count += 1;
        }

        // Return the infer_manager so later phases (e.g. dynamic field) can
        // reuse cached inference results rather than recomputing from scratch.
        context.infer_manager = infer_manager;
    }
}

fn materialize_pending_str_tpl_type_decls(db: &mut DbIndex, infer_manager: &mut InferCacheManager) {
    let pending_type_decls = infer_manager.drain_pending_str_tpl_type_decls();

    for pending in pending_type_decls {
        if db
            .get_type_index()
            .get_type_decl(&pending.type_decl_id)
            .is_none()
        {
            db.get_type_index_mut().add_type_decl(
                pending.file_id,
                LuaTypeDecl::new(
                    pending.file_id,
                    TextRange::default(),
                    pending.type_decl_id.get_simple_name().to_string(),
                    LuaDeclTypeKind::Class,
                    LuaTypeFlag::AutoGenerated.into(),
                    pending.type_decl_id.clone(),
                ),
            );
        }

        let has_super = db
            .get_type_index()
            .get_super_types_iter(&pending.type_decl_id)
            .map(|mut supers| supers.any(|existing_super| existing_super == &pending.super_type))
            .unwrap_or(false);
        if !has_super {
            db.get_type_index_mut().add_super_type(
                pending.type_decl_id.clone(),
                pending.file_id,
                pending.super_type,
            );
        }
    }
}

#[allow(unused)]
fn record_unresolve_info(
    time_hash_map: HashMap<usize, (u128, usize)>,
    reason_unresolves: &HashMap<InferFailReason, Vec<UnResolve>>,
) {
    let mut unresolve_info: HashMap<String, usize> = HashMap::new();
    for (check_reason, unresolves) in reason_unresolves.iter() {
        for unresolve in unresolves {
            match unresolve {
                UnResolve::Return(_) => {
                    unresolve_info
                        .entry("UnResolveReturn".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::Decl(_) => {
                    unresolve_info
                        .entry("UnResolveDecl".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::Member(_) => {
                    unresolve_info
                        .entry("UnResolveMember".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::Module(_) => {
                    unresolve_info
                        .entry("UnResolveModule".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::ClosureParams(_) => {
                    unresolve_info
                        .entry("UnResolveClosureParams".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::ClosureReturn(_) => {
                    unresolve_info
                        .entry("UnResolveClosureReturn".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::ClosureParentParams(_) => {
                    unresolve_info
                        .entry("UnResolveClosureParentParams".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::IterDecl(_) => {
                    unresolve_info
                        .entry("UnResolveIterDecl".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::ModuleRef(_) => {
                    unresolve_info
                        .entry("UnResolveModuleRef".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::TableField(_) => {
                    unresolve_info
                        .entry("UnResolveTableField".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
                UnResolve::SpecialCall(_) => {
                    unresolve_info
                        .entry("UnResolveSpecialCall".to_string())
                        .and_modify(|count| *count += 1)
                        .or_insert(1);
                }
            }
        }
    }

    log::info!("unresolve reason count {}", reason_unresolves.len());
    let mut s = String::new();
    let mut unresolve_info_vec = unresolve_info
        .iter()
        .map(|(k, v)| (k.clone(), *v))
        .collect::<Vec<_>>();
    unresolve_info_vec.sort_by(|a, b| a.1.cmp(&b.1).reverse());
    s.clear();
    s.push_str("unresolve info:\n");
    for (name, count) in unresolve_info_vec {
        s.push_str(&format!("{}: {}\n", name, count));
    }
    log::info!("{}", s);
}

fn try_resolve(
    db: &mut DbIndex,
    infer_manager: &mut InferCacheManager,
    reason_resolve: &mut HashMap<InferFailReason, Vec<UnResolve>>,
) {
    loop {
        let mut changed = false;
        let mut to_be_remove = Vec::new();
        let mut retain_unresolve = Vec::new();

        for check_reason in sorted_reason_keys(reason_resolve) {
            let Some(unresolves) = reason_resolve.get_mut(&check_reason) else {
                continue;
            };

            let reached = check_reach_reason(db, infer_manager, &check_reason).unwrap_or(false);
            if !reached {
                continue;
            }

            unresolves.sort_unstable_by(unresolve_stable_cmp);
            for mut unresolve in unresolves.drain(..) {
                let file_id = unresolve.get_file_id().unwrap_or(FileId { id: 0 });
                let cache = infer_manager.get_infer_cache(file_id);
                let resolve_result = match &mut unresolve {
                    UnResolve::Decl(un_resolve_decl) => {
                        try_resolve_decl(db, cache, un_resolve_decl)
                    }
                    UnResolve::Member(un_resolve_member) => {
                        try_resolve_member(db, cache, un_resolve_member)
                    }
                    UnResolve::Module(un_resolve_module) => {
                        try_resolve_module(db, cache, un_resolve_module)
                    }
                    UnResolve::Return(un_resolve_return) => {
                        try_resolve_return_point(db, cache, un_resolve_return)
                    }
                    UnResolve::ClosureParams(un_resolve_closure_params) => {
                        try_resolve_call_closure_params(db, cache, un_resolve_closure_params)
                    }
                    UnResolve::ClosureReturn(un_resolve_closure_return) => {
                        try_resolve_closure_return(db, cache, un_resolve_closure_return)
                    }
                    UnResolve::IterDecl(un_resolve_iter_var) => {
                        try_resolve_iter_var(db, cache, un_resolve_iter_var)
                    }
                    UnResolve::ModuleRef(module_ref) => {
                        try_resolve_module_ref(db, cache, module_ref)
                    }
                    UnResolve::ClosureParentParams(un_resolve_closure_params) => {
                        try_resolve_closure_parent_params(db, cache, un_resolve_closure_params)
                    }
                    UnResolve::TableField(un_resolve_table_field) => {
                        try_resolve_table_field(db, cache, un_resolve_table_field)
                    }
                    UnResolve::SpecialCall(un_resolve_special_call) => {
                        try_resolve_special_call(db, cache, un_resolve_special_call)
                    }
                };

                match resolve_result {
                    Ok(_) => {
                        changed = true;
                    }
                    Err(InferFailReason::None | InferFailReason::RecursiveInfer) => {}
                    Err(InferFailReason::FieldNotFound) => {
                        if !cache.get_config().analysis_phase.is_force() {
                            retain_unresolve.push((unresolve, InferFailReason::FieldNotFound));
                        }
                    }
                    Err(InferFailReason::UnResolveOperatorCall) => {
                        if !cache.get_config().analysis_phase.is_force() {
                            retain_unresolve
                                .push((unresolve, InferFailReason::UnResolveOperatorCall));
                        }
                    }
                    Err(reason) => {
                        if reason != check_reason {
                            changed = true;
                            retain_unresolve.push((unresolve, reason));
                        }
                    }
                }
            }

            to_be_remove.push(check_reason);
        }

        for reason in to_be_remove {
            reason_resolve.remove(&reason);
        }

        for (unresolve, reason) in retain_unresolve {
            reason_resolve.entry(reason).or_default().push(unresolve);
        }

        if !changed || reason_resolve.is_empty() {
            break;
        }
    }
}

fn sorted_reason_keys(
    reason_resolve: &HashMap<InferFailReason, Vec<UnResolve>>,
) -> Vec<InferFailReason> {
    let mut keys: Vec<InferFailReason> = reason_resolve.keys().cloned().collect();
    keys.sort_unstable_by(infer_fail_reason_stable_cmp);
    keys
}

fn infer_fail_reason_kind_rank(reason: &InferFailReason) -> u8 {
    match reason {
        InferFailReason::None => 0,
        InferFailReason::RecursiveInfer => 1,
        InferFailReason::FieldNotFound => 2,
        InferFailReason::UnResolveOperatorCall => 3,
        InferFailReason::UnResolveDeclType(_) => 4,
        InferFailReason::UnResolveMemberType(_) => 5,
        InferFailReason::UnResolveExpr(_) => 6,
        InferFailReason::UnResolveSignatureReturn(_) => 7,
        InferFailReason::UnResolveTypeDecl(_) => 8,
        InferFailReason::UnResolveModuleExport(_) => 9,
    }
}

fn infer_fail_reason_stable_cmp(a: &InferFailReason, b: &InferFailReason) -> Ordering {
    let rank_cmp = infer_fail_reason_kind_rank(a).cmp(&infer_fail_reason_kind_rank(b));
    if rank_cmp != Ordering::Equal {
        return rank_cmp;
    }

    match (a, b) {
        (
            InferFailReason::UnResolveDeclType(a_decl),
            InferFailReason::UnResolveDeclType(b_decl),
        ) => a_decl
            .file_id
            .id
            .cmp(&b_decl.file_id.id)
            .then_with(|| u32::from(a_decl.position).cmp(&u32::from(b_decl.position))),
        (
            InferFailReason::UnResolveMemberType(a_member),
            InferFailReason::UnResolveMemberType(b_member),
        ) => a_member.file_id.id.cmp(&b_member.file_id.id).then_with(|| {
            u32::from(a_member.get_position()).cmp(&u32::from(b_member.get_position()))
        }),
        (InferFailReason::UnResolveExpr(a_expr), InferFailReason::UnResolveExpr(b_expr)) => {
            a_expr.file_id.id.cmp(&b_expr.file_id.id).then_with(|| {
                u32::from(a_expr.value.syntax().text_range().start())
                    .cmp(&u32::from(b_expr.value.syntax().text_range().start()))
            })
        }
        (
            InferFailReason::UnResolveSignatureReturn(a_signature),
            InferFailReason::UnResolveSignatureReturn(b_signature),
        ) => a_signature
            .get_file_id()
            .id
            .cmp(&b_signature.get_file_id().id)
            .then_with(|| {
                u32::from(a_signature.get_position()).cmp(&u32::from(b_signature.get_position()))
            }),
        (
            InferFailReason::UnResolveTypeDecl(a_type),
            InferFailReason::UnResolveTypeDecl(b_type),
        ) => a_type.get_name().cmp(b_type.get_name()),
        (
            InferFailReason::UnResolveModuleExport(a_file_id),
            InferFailReason::UnResolveModuleExport(b_file_id),
        ) => a_file_id.id.cmp(&b_file_id.id),
        _ => Ordering::Equal,
    }
}

fn unresolve_kind_rank(unresolve: &UnResolve) -> u8 {
    match unresolve {
        UnResolve::Decl(_) => 0,
        UnResolve::IterDecl(_) => 1,
        UnResolve::Member(_) => 2,
        UnResolve::Module(_) => 3,
        UnResolve::Return(_) => 4,
        UnResolve::ClosureParams(_) => 5,
        UnResolve::ClosureReturn(_) => 6,
        UnResolve::ClosureParentParams(_) => 7,
        UnResolve::ModuleRef(_) => 8,
        UnResolve::TableField(_) => 9,
        UnResolve::SpecialCall(_) => 10,
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
    IterDecl(Box<UnResolveIterVar>),
    Member(Box<UnResolveMember>),
    Module(Box<UnResolveModule>),
    Return(Box<UnResolveReturn>),
    ClosureParams(Box<UnResolveCallClosureParams>),
    ClosureReturn(Box<UnResolveClosureReturn>),
    ClosureParentParams(Box<UnResolveParentClosureParams>),
    ModuleRef(Box<UnResolveModuleRef>),
    TableField(Box<UnResolveTableField>),
    SpecialCall(Box<UnResolveSpecialCall>),
}

#[allow(dead_code)]
impl UnResolve {
    pub fn get_file_id(&self) -> Option<FileId> {
        match self {
            UnResolve::Decl(un_resolve_decl) => Some(un_resolve_decl.file_id),
            UnResolve::IterDecl(un_resolve_iter_var) => Some(un_resolve_iter_var.file_id),
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
            UnResolve::TableField(un_resolve_table_field) => Some(un_resolve_table_field.file_id),
            UnResolve::ModuleRef(_) => None,
            UnResolve::SpecialCall(un_resolve_special_call) => {
                Some(un_resolve_special_call.file_id)
            }
        }
    }

    /// Returns a deterministic sort key (file_id, text_position) for stable ordering.
    /// This ensures unresolves are processed in a consistent order regardless of
    /// HashMap iteration order or other non-deterministic sources during collection.
    fn sort_key(&self) -> (u32, u32) {
        match self {
            UnResolve::Decl(d) => (d.file_id.id, u32::from(d.decl_id.position)),
            UnResolve::IterDecl(d) => (
                d.file_id.id,
                d.iter_vars
                    .first()
                    .map(|v| u32::from(v.syntax().text_range().start()))
                    .unwrap_or(0),
            ),
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
            UnResolve::ModuleRef(d) => (0, d.module_file_id.id),
            UnResolve::TableField(d) => (
                d.file_id.id,
                u32::from(d.field.syntax().text_range().start()),
            ),
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

#[derive(Debug)]
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
pub struct UnResolveIterVar {
    pub file_id: FileId,
    pub iter_exprs: Vec<LuaExpr>,
    pub iter_vars: Vec<LuaNameToken>,
}

impl From<UnResolveIterVar> for UnResolve {
    fn from(un_resolve_iter_var: UnResolveIterVar) -> Self {
        UnResolve::IterDecl(Box::new(un_resolve_iter_var))
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

#[derive(Debug)]
pub struct UnResolveModuleRef {
    pub owner_id: LuaSemanticDeclId,
    pub module_file_id: FileId,
}

impl From<UnResolveModuleRef> for UnResolve {
    fn from(un_resolve_module_ref: UnResolveModuleRef) -> Self {
        UnResolve::ModuleRef(Box::new(un_resolve_module_ref))
    }
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum UnResolveParentAst {
    LuaFuncStat(LuaFuncStat),
    LuaTableField(LuaTableField),
    LuaAssignStat(LuaAssignStat),
}

#[derive(Debug)]
pub struct UnResolveParentClosureParams {
    pub file_id: FileId,
    pub signature_id: LuaSignatureId,
    pub parent_ast: UnResolveParentAst,
}

impl From<UnResolveParentClosureParams> for UnResolve {
    fn from(un_resolve_closure_params: UnResolveParentClosureParams) -> Self {
        UnResolve::ClosureParentParams(Box::new(un_resolve_closure_params))
    }
}

#[derive(Debug)]
pub struct UnResolveTableField {
    pub file_id: FileId,
    pub table_expr: LuaTableExpr,
    pub field: LuaTableField,
    pub decl_feature: LuaMemberFeature,
}

impl From<UnResolveTableField> for UnResolve {
    fn from(un_resolve_table_field: UnResolveTableField) -> Self {
        UnResolve::TableField(Box::new(un_resolve_table_field))
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
    use std::collections::HashMap;

    use rowan::TextSize;

    use crate::{FileId, InferFailReason, LuaDeclId, LuaTypeDeclId};

    use super::sorted_reason_keys;

    #[test]
    fn reason_group_order_is_stable_across_hashmap_insertion_order() {
        let reasons = [
            InferFailReason::FieldNotFound,
            InferFailReason::UnResolveDeclType(LuaDeclId::new(FileId::new(2), TextSize::new(20))),
            InferFailReason::UnResolveDeclType(LuaDeclId::new(FileId::new(2), TextSize::new(8))),
            InferFailReason::UnResolveTypeDecl(LuaTypeDeclId::local(FileId::new(3), "Local.Zed")),
            InferFailReason::UnResolveTypeDecl(LuaTypeDeclId::global("Global.A")),
            InferFailReason::UnResolveModuleExport(FileId::new(9)),
        ];

        let mut forward: HashMap<InferFailReason, Vec<super::UnResolve>> = HashMap::new();
        for reason in reasons.iter().cloned() {
            forward.insert(reason, Vec::new());
        }

        let mut reverse: HashMap<InferFailReason, Vec<super::UnResolve>> = HashMap::new();
        for reason in reasons.iter().rev().cloned() {
            reverse.insert(reason, Vec::new());
        }

        assert_eq!(sorted_reason_keys(&forward), sorted_reason_keys(&reverse));
    }
}
