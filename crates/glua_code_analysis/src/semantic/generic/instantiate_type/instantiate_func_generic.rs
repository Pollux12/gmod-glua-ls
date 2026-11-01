use rustc_hash::FxHashSet as HashSet;
use std::{ops::Deref, sync::Arc};

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaChunk, LuaDocTypeList, LuaExpr, LuaLiteralToken, LuaNameExpr,
    LuaSyntaxId,
};
use smol_str::SmolStr;

use crate::db_index::find_best_direct_call_arg_role_from_type;
use crate::{
    DocTypeInferContext, FileId, GenericTpl, GenericTplId, LuaDefinitionId, LuaDocDefaultValue,
    LuaFunctionType, LuaGenericType, LuaSemanticDeclId, TypeVisitTrait,
    db_index::{DbIndex, LuaType},
    infer_doc_type,
    semantic::{
        LuaInferCache,
        generic::{
            instantiate_type::instantiate_doc_function,
            tpl_context::TplContext,
            tpl_pattern::{
                multi_param_tpl_pattern_match_multi_return, tpl_pattern_match,
                variadic_tpl_pattern_match,
            },
        },
        infer::{
            InferFailReason,
            narrow::get_type_at_flow::{
                explicit_param_string_default_reaches_flow, inferred_string_default_reaches_flow,
            },
        },
        infer_enclosing_self_type, infer_expr,
    },
};
use crate::{LuaMemberOwner, SemanticDeclLevel, infer_node_semantic_decl};

use super::{SubstitutorValue, TypeSubstitutor};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericFunctionInstantiation {
    pub function: LuaFunctionType,
    pub bindings: Vec<FuncGenericBinding>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncGenericBinding {
    pub id: GenericTplId,
    pub state: FuncGenericBindingState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FuncGenericBindingState {
    Bound,
    Unbound { fallback: LuaType },
}

fn find_generic_constraint(func: &LuaFunctionType, id: GenericTplId) -> Option<LuaType> {
    let mut constraint = None;
    func.visit_type(&mut |t| match t {
        LuaType::TplRef(generic_tpl) | LuaType::ConstTplRef(generic_tpl)
            if generic_tpl.get_tpl_id() == id =>
        {
            if constraint.is_none() {
                constraint = generic_tpl.get_constraint().cloned();
            }
        }
        LuaType::StrTplRef(str_tpl) if str_tpl.get_tpl_id() == id => {
            if constraint.is_none() {
                constraint = str_tpl.get_constraint().cloned();
            }
        }
        _ => {}
    });
    constraint
}

/// The type a string-template parameter binds from `arg_expr`, whose own type
/// is `arg_type`: the panel class its enclosing PANEL method names when the
/// call marks the argument a `gmod.vgui_panel` reference, else a plain
/// `string` argument's flow-valid `x = x or "literal"` default, else
/// `arg_type` itself.
pub(crate) fn str_tpl_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    param_type: &LuaType,
    arg_expr: &LuaExpr,
    call_expr: &LuaCallExpr,
    param_idx: usize,
    arg_type: LuaType,
) -> LuaType {
    if matches!(
        arg_type,
        LuaType::StringConst(_) | LuaType::DocStringConst(_)
    ) {
        return arg_type;
    }
    resolve_vgui_panel_ref_from_arg(
        db, cache, param_type, arg_expr, &arg_type, call_expr, param_idx,
    )
    .or_else(|| {
        matches!(arg_type, LuaType::String)
            .then(|| resolve_str_default_from_arg(db, cache, param_type, arg_expr))
            .flatten()
    })
    .unwrap_or(arg_type)
}

/// Resolve a flow-valid inferred string default for a call argument expression.
///
/// When the arg is a local variable with an inferred string default (from
/// `x = x or "literal"`), and the self-coalescing assignment is the last
/// write to that variable that dominates the call site, returns
/// `Some(LuaType::StringConst(value))`.
///
/// Only returns a value when:
/// - `arg_type` is exactly `LuaType::String`
/// - `param_type` actually contains a `StrTplRef`
/// - exactly one candidate default is flow-valid at the use site
/// - no explicit `---@param` default takes precedence
fn resolve_str_default_from_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    param_type: &LuaType,
    arg_expr: &LuaExpr,
) -> Option<LuaType> {
    // Quick gate: only when param contains a StrTplRef.
    if !param_type.contain_tpl() {
        return None;
    }
    let mut has_str_tpl = false;
    param_type.visit_type(&mut |t| {
        if matches!(t, LuaType::StrTplRef(_)) {
            has_str_tpl = true;
        }
    });
    if !has_str_tpl {
        return None;
    }

    // Resolve the argument's declaration.
    let name_expr = LuaNameExpr::cast(arg_expr.syntax().clone())?;
    let file_id = cache.get_file_id();
    let range = name_expr.get_range();
    let local_ref = db.get_reference_index().get_local_reference(&file_id)?;
    let decl_id = local_ref.get_decl_id(&range)?;

    // Seed the cache with the use-site realm so that flow-reachability
    // checks evaluate from the call argument's realm context (not the
    // declaration position fallback).
    let use_site_realm = db
        .get_gmod_infer_index()
        .get_realm_at_offset(&file_id, range.start());
    let previous_realm = cache.set_query_realm(Some(use_site_realm));

    let result = resolve_str_default_from_arg_inner(db, cache, param_type, arg_expr, decl_id);

    cache.restore_query_realm(previous_realm);
    result
}

/// Inner helper for `resolve_str_default_from_arg` — separated so the
/// caller can seed/restore `flow_query_realm` around the call.
fn resolve_str_default_from_arg_inner(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    _param_type: &LuaType,
    arg_expr: &LuaExpr,
    decl_id: crate::LuaDeclId,
) -> Option<LuaType> {
    let file_id = cache.get_file_id();

    // ── Explicit default precedence ───────────────────────────────────
    // If the variable is a function parameter with an explicit `---@param`
    // default (e.g. `---@param x string = "foo"`), prefer that over any
    // inferred `x = x or "foo"` default — but ONLY when:
    // 1. the default is a `String` (this resolver is for string-template binding),
    // 2. the explicit default is still flow-valid at the use site.
    if let Some(decl) = db.get_decl_index().get_decl(&decl_id) {
        if let crate::LuaDeclExtra::Param {
            idx: param_idx_in_sig,
            signature_id,
            ..
        } = &decl.extra
        {
            if let Some(default_val) = db
                .get_signature_index()
                .get(signature_id)
                .and_then(|sig| sig.get_param_info_by_id(*param_idx_in_sig))
                .and_then(|info| info.default_value.as_ref())
            {
                // Only String defaults participate in string-template binding.
                if let LuaDocDefaultValue::String(s) = default_val {
                    // Flow-validity: the explicit default must still reach
                    // the use site (not killed by a non-coalescing reassignment).
                    let flow_tree = db.get_flow_index().get_flow_tree(&file_id);
                    let root = LuaChunk::cast(arg_expr.get_root());
                    let flow_valid = match (&flow_tree, &root) {
                        (Some(tree), Some(root)) => tree
                            .get_flow_id(arg_expr.get_syntax_id())
                            .is_some_and(|use_flow_id| {
                                explicit_param_string_default_reaches_flow(
                                    db,
                                    tree,
                                    cache,
                                    root,
                                    decl_id,
                                    use_flow_id,
                                )
                            }),
                        _ => false,
                    };

                    if flow_valid {
                        return Some(LuaType::StringConst(SmolStr::new(s.as_str()).into()));
                    }
                }
                // Non-String explicit defaults (Boolean, Number, Nil) are
                // ignored by this resolver — they don't bind string templates.
            }
        }
    }

    // Get inferred string default candidates from the side-map.
    let candidates = db
        .get_property_index()
        .get_inferred_string_defaults(&decl_id)?;
    if candidates.is_empty() {
        return None;
    }

    // Get the flow tree and the flow ID for the argument expression (use site).
    // Using the arg expression's syntax ID (not the call expression's) is
    // consistent with how existing narrow-flow querying works in
    // `semantic/infer/narrow/mod.rs`.
    let flow_tree = db.get_flow_index().get_flow_tree(&file_id)?;
    let use_flow_id = flow_tree.get_flow_id(arg_expr.get_syntax_id())?;
    let root = LuaChunk::cast(arg_expr.get_root())?;

    // Check each candidate for flow-validity.  Exactly one must be valid.
    let mut valid_value: Option<SmolStr> = None;
    for candidate in candidates {
        let reaches = inferred_string_default_reaches_flow(
            db,
            flow_tree,
            cache,
            &root,
            decl_id,
            use_flow_id,
            candidate.source_range,
        );
        if reaches {
            if valid_value.is_some() {
                // Multiple valid candidates — ambiguous, don't bind.
                return None;
            }
            valid_value = Some(candidate.value.clone());
        }
    }

    valid_value.map(|v| LuaType::StringConst(v.into()))
}

/// Check whether the callable expression has a `call_arg("gmod.vgui_panel",
/// "reference")` annotation on the parameter at `param_idx`.
///
/// First tries semantic-declaration resolution (which yields a `Signature` id
/// carrying the `call_arg` metadata), then falls back to inferring the
/// expression type and using direct call-arg role metadata.
pub(crate) fn check_vgui_panel_ref_role(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    prefix_expr: &LuaExpr,
    param_idx: usize,
    colon_call: bool,
) -> bool {
    let mut candidate_param_indices = vec![param_idx];
    if colon_call && !candidate_param_indices.contains(&(param_idx + 1)) {
        candidate_param_indices.push(param_idx + 1);
    }

    // Try semantic declaration first — this yields Signature ids that carry
    // call_arg attributes directly.
    if let Some(sem_decl) = infer_node_semantic_decl(
        db,
        cache,
        prefix_expr.syntax().clone(),
        SemanticDeclLevel::NoTrace,
    ) {
        if let LuaSemanticDeclId::Signature(sig_id) = &sem_decl {
            if let Some(sig) = db.get_signature_index().get(sig_id) {
                for candidate_idx in &candidate_param_indices {
                    let mut found = false;
                    let mut visitor = |role: &crate::LuaCallArgRole| {
                        if role.is_direct_arg()
                            && role.domain == "gmod.vgui_panel"
                            && role.role == "reference"
                        {
                            found = true;
                        }
                    };
                    sig.visit_call_arg_roles_for_param(*candidate_idx, &mut visitor);
                    if found {
                        return true;
                    }
                }
            }
        }
    }

    // Fall back to inferring the expression type.
    let Ok(callable_type) = infer_expr(db, cache, prefix_expr.clone()) else {
        return false;
    };
    candidate_param_indices.into_iter().any(|candidate_idx| {
        find_best_direct_call_arg_role_from_type(
            db,
            &callable_type,
            candidate_idx,
            "gmod.vgui_panel",
            &["reference"],
        )
        .is_some()
    })
}

/// Resolve a VGUI panel class name from the enclosing method context.
///
/// Some engine entry points hand a panel method its own registered class
/// name as a parameter — e.g. `PANEL:GenerateExample(class, ...)`, whose
/// `class` parameter is annotated `call_arg("gmod.vgui_panel", "own_class")`.
/// When a call argument annotated `call_arg("gmod.vgui_panel", "reference")`
/// is such a parameter of the enclosing colon-method on a registered VGUI
/// panel table, resolve the argument to that panel's class name. Any other
/// method parameter names an arbitrary class and keeps the generic
/// constraint.
///
/// The resolution is **declaration-precise**: the enclosing colon-method's
/// receiver variable must resolve to the same `LuaDeclId` as the registration
/// call's table argument.  This correctly handles files with multiple
/// sequential `local PANEL = {}` blocks.
fn resolve_vgui_panel_ref_from_arg(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    param_type: &LuaType,
    arg_expr: &LuaExpr,
    arg_type: &LuaType,
    call_expr: &LuaCallExpr,
    param_idx: usize,
) -> Option<LuaType> {
    // Gate: param must contain a StrTplRef.
    if !param_type.contain_tpl() {
        return None;
    }
    let mut has_str_tpl = false;
    param_type.visit_type(&mut |t| {
        if matches!(t, LuaType::StrTplRef(_)) {
            has_str_tpl = true;
        }
    });
    if !has_str_tpl {
        return None;
    }

    // Check that the call parameter has call_arg("gmod.vgui_panel", "reference").
    let prefix_expr = call_expr.get_prefix_expr()?;
    if !check_vgui_panel_ref_role(
        db,
        cache,
        &prefix_expr,
        param_idx,
        call_expr.is_colon_call(),
    ) {
        return None;
    }

    // The argument must be a name expression referencing a function parameter.
    let name_expr = LuaNameExpr::cast(arg_expr.syntax().clone())?;
    let file_id = cache.get_file_id();
    let range = name_expr.get_range();
    let local_ref = db.get_reference_index().get_local_reference(&file_id)?;
    let arg_decl_id = local_ref.get_decl_id(&range)?;
    let decl = db.get_decl_index().get_decl(&arg_decl_id)?;
    let crate::LuaDeclExtra::Param {
        idx, signature_id, ..
    } = &decl.extra
    else {
        return None;
    };

    let context = crate::semantic::resolve_registered_vgui_method_context(db, cache, &name_expr)?;
    if *signature_id != context.receiver_signature_id {
        return None;
    }
    // The parameter binds the registered class name only when the method it
    // belongs to overrides a signature marking that parameter index with
    // `call_arg("gmod.vgui_panel", "own_class")` — the annotation contract
    // that says the engine fills it with the panel's own class name.
    if !crate::semantic::registered_vgui_method_param_is_own_class(db, &context, *idx) {
        return None;
    }
    if !incoming_param_reaches_read(db, cache, arg_expr, decl, arg_type) {
        return None;
    }

    Some(LuaType::StringConst(
        SmolStr::new(&context.panel_name).into(),
    ))
}

/// Whether the read `arg_expr` sees the parameter `decl` as it entered: its
/// declaration is the only definition reaching the read and no cast or
/// narrowing in effect there changes `arg_type`, its type at the read.
fn incoming_param_reaches_read(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg_expr: &LuaExpr,
    decl: &crate::LuaDecl,
    arg_type: &LuaType,
) -> bool {
    let decl_id = decl.get_id();
    let Some(tree) = db.get_flow_index().get_flow_tree(&cache.get_file_id()) else {
        return false;
    };
    let Some(flow_id) = tree.get_flow_id(arg_expr.get_syntax_id()) else {
        return false;
    };
    tree.reaching_definitions(decl_id, flow_id).as_ref() == [LuaDefinitionId::Declaration(decl_id)]
        && crate::infer_param_with_cache(db, cache, decl)
            .is_ok_and(|incoming| &incoming == arg_type)
}

pub fn instantiate_func_generic_with_bindings(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func: &LuaFunctionType,
    call_expr: LuaCallExpr,
) -> Result<GenericFunctionInstantiation, InferFailReason> {
    let file_id = cache.get_file_id().clone();
    let mut generic_tpls = HashSet::default();
    let mut contain_self = false;
    func.visit_type(&mut |t| match t {
        LuaType::TplRef(generic_tpl) | LuaType::ConstTplRef(generic_tpl) => {
            let tpl_id = generic_tpl.get_tpl_id();
            if tpl_id.is_func() {
                generic_tpls.insert(tpl_id);
            }
        }
        LuaType::StrTplRef(str_tpl) => {
            generic_tpls.insert(str_tpl.get_tpl_id());
        }
        LuaType::SelfInfer => {
            contain_self = true;
        }
        _ => {}
    });

    let origin_params = func.get_params();
    let mut func_params: Vec<_> = origin_params
        .iter()
        .map(|(name, t)| (name.clone(), t.clone().unwrap_or(LuaType::Unknown)))
        .collect();

    let arg_exprs = call_expr
        .get_args_list()
        .ok_or(InferFailReason::None)?
        .get_args()
        .collect::<Vec<_>>();
    let mut substitutor = TypeSubstitutor::new();
    let mut context = TplContext {
        db,
        cache,
        substitutor: &mut substitutor,
        call_expr: Some(call_expr.clone()),
        source_range: call_expr.get_range(),
    };

    let callee_generic_ids: Vec<GenericTplId> = {
        let mut ids: Vec<_> = generic_tpls.iter().copied().collect();
        ids.sort_by_key(|id| {
            (
                match id {
                    GenericTplId::Func(_) => 0,
                    GenericTplId::Type(_) => 1,
                },
                id.get_idx(),
            )
        });
        ids
    };

    if !generic_tpls.is_empty() {
        context.substitutor.add_need_infer_tpls(generic_tpls);

        if let Some(type_list) = call_expr.get_call_generic_type_list() {
            // If the generic was forced with `obj:abc--[[@<string>]]("abc")`, apply it directly
            apply_call_generic_type_list(db, file_id, &mut context, &type_list);
        } else {
            // If no generic is specified, infer it from the call arguments
            infer_generic_types_from_call(
                db,
                &mut context,
                func,
                &call_expr,
                &mut func_params,
                &arg_exprs,
            )?;
        }
    }

    if contain_self && let Some(self_type) = infer_call_self_type(db, cache, &call_expr, func) {
        substitutor.add_self_type(self_type);
    }

    let mut bindings = Vec::with_capacity(callee_generic_ids.len());
    for id in callee_generic_ids {
        let state = match substitutor.get(id) {
            Some(SubstitutorValue::None) => {
                let fallback = find_generic_constraint(func, id).unwrap_or(LuaType::Any);
                FuncGenericBindingState::Unbound { fallback }
            }
            Some(
                SubstitutorValue::Type(_)
                | SubstitutorValue::Params(_)
                | SubstitutorValue::MultiTypes(_)
                | SubstitutorValue::MultiBase(_),
            ) => FuncGenericBindingState::Bound,
            None => {
                let fallback = find_generic_constraint(func, id).unwrap_or(LuaType::Any);
                FuncGenericBindingState::Unbound { fallback }
            }
        };
        bindings.push(FuncGenericBinding { id, state });
    }

    let function = if let LuaType::DocFunction(f) = instantiate_doc_function(db, func, &substitutor)
    {
        f.deref().clone()
    } else {
        func.clone()
    };

    Ok(GenericFunctionInstantiation { function, bindings })
}

pub fn instantiate_func_generic(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func: &LuaFunctionType,
    call_expr: LuaCallExpr,
) -> Result<LuaFunctionType, InferFailReason> {
    instantiate_func_generic_with_bindings(db, cache, func, call_expr).map(|res| res.function)
}

fn apply_call_generic_type_list(
    db: &DbIndex,
    file_id: FileId,
    context: &mut TplContext,
    type_list: &LuaDocTypeList,
) {
    let doc_ctx = DocTypeInferContext::new(db, file_id);
    for (i, doc_type) in type_list.get_types().enumerate() {
        let typ = infer_doc_type(doc_ctx, &doc_type);
        context
            .substitutor
            .insert_type(GenericTplId::Func(i as u32), typ, true);
    }
}

/// Reuse an argument type already inferred for the same call.
///
/// `infer_call_arg_expr_list_types` memoises the whole argument list per call
/// under `(call_syntax_id, None)`. When that entry exists and aligns 1:1 with
/// `arg_exprs`, a concrete entry is exactly what `infer_expr` returned for the
/// same expression, so it is reused without re-inference.
///
/// `Unknown`/`Nil` entries are never reused: the shared entry stores `Unknown`
/// when the underlying inference failed, while this path maps `FieldNotFound`
/// to `Nil` and propagates other errors. Falling back to `infer_expr` for
/// those keeps the error paths identical.
fn shared_call_arg_type(
    cache: &mut LuaInferCache,
    call_syntax_id: LuaSyntaxId,
    arg_exprs: &[LuaExpr],
    arg_idx: usize,
) -> Option<LuaType> {
    let shared = cache.tracked_get(
        |cache| &mut cache.call_arg_types_cache,
        &(call_syntax_id, None),
    )?;
    if shared.len() != arg_exprs.len() {
        return None;
    }
    let arg_expr = &arg_exprs[arg_idx];
    let (cached_type, cached_range) = &shared[arg_idx];
    if *cached_range != arg_expr.get_range() {
        return None;
    }
    match cached_type {
        LuaType::Unknown | LuaType::Nil => None,
        _ => {
            if arg_may_unwrap_variadic(arg_expr, arg_idx, arg_exprs.len()) {
                return None;
            }
            Some(cached_type.clone())
        }
    }
}

/// Whether `infer_expr` on this argument may yield a `Variadic` that the
/// shared call-arg list stores unwrapped (a `Base` variadic behind a trailing
/// call or a `...` literal collapses to one entry, so reusing it would lose
/// the `Variadic` wrapper the `(_, Variadic)` arm dispatches on).
fn arg_may_unwrap_variadic(arg_expr: &LuaExpr, arg_idx: usize, arg_count: usize) -> bool {
    match arg_expr {
        LuaExpr::LiteralExpr(literal_expr) => {
            matches!(literal_expr.get_literal(), Some(LuaLiteralToken::Dots(_)))
        }
        LuaExpr::CallExpr(_) => arg_idx + 1 == arg_count,
        _ => false,
    }
}

fn infer_generic_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_syntax_id: LuaSyntaxId,
    arg_exprs: &[LuaExpr],
    arg_idx: usize,
) -> Result<LuaType, InferFailReason> {
    if let Some(shared) = shared_call_arg_type(cache, call_syntax_id, arg_exprs, arg_idx) {
        return Ok(shared);
    }
    match infer_expr(db, cache, arg_exprs[arg_idx].clone()) {
        Ok(t) => Ok(t),
        // Unresolved fields infer as nil so follow-up deduction can proceed.
        Err(InferFailReason::FieldNotFound) => Ok(LuaType::Nil),
        Err(e) => Err(e),
    }
}

fn infer_variadic_tail_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_syntax_id: LuaSyntaxId,
    arg_exprs: &[LuaExpr],
    arg_idx: usize,
) -> Result<LuaType, InferFailReason> {
    if let Some(shared) = shared_call_arg_type(cache, call_syntax_id, arg_exprs, arg_idx) {
        return Ok(shared);
    }
    infer_expr(db, cache, arg_exprs[arg_idx].clone())
}

fn infer_generic_types_from_call(
    db: &DbIndex,
    context: &mut TplContext,
    func: &LuaFunctionType,
    call_expr: &LuaCallExpr,
    func_params: &mut Vec<(String, LuaType)>,
    arg_exprs: &[LuaExpr],
) -> Result<(), InferFailReason> {
    let colon_call = call_expr.is_colon_call();
    let colon_define = func.is_colon_define();
    match (colon_define, colon_call) {
        (true, false) => {
            func_params.insert(0, ("self".to_string(), LuaType::Any));
        }
        (false, true) => {
            if !func_params.is_empty() {
                func_params.remove(0);
            }
        }
        (true, true) => {
            // For colon-define + colon-call: the call args exclude the
            // implicit self, but `func_params` may still carry an explicit
            // "self" entry (e.g. from an overload annotation like
            // `fun(self: Panel, className: \`T\`): T`).  Remove it so that
            // func_params[i] aligns with arg_exprs[i].
            if !func_params.is_empty() && func_params[0].0 == "self" {
                func_params.remove(0);
            }
        }
        _ => {}
    }

    let call_syntax_id = call_expr.get_syntax_id();
    let mut unresolve_tpls = vec![];
    for i in 0..func_params.len() {
        if i >= arg_exprs.len() {
            break;
        }

        if context.substitutor.is_infer_all_tpl() {
            break;
        }

        let (_, func_param_type) = &func_params[i];
        let call_arg_expr = &arg_exprs[i];
        if !func_param_type.contain_tpl() {
            continue;
        }
        context.source_range = call_arg_expr.get_range();

        if !func_param_type.is_variadic()
            && check_expr_can_later_infer(context, func_param_type, call_arg_expr)?
        {
            // If the parameter cannot be inferred later, do not handle it for now
            unresolve_tpls.push((func_param_type.clone(), call_arg_expr.clone()));
            continue;
        }

        // A variadic parameter discards the single pre-inferred argument type
        // below (the tail re-infers with `?` error semantics), so inferring it
        // first would infer `arg_exprs[i]` twice for the same call. Go straight
        // to the tail loop instead; the first tail inference returns the same
        // `Err` the skipped pre-inference would have returned.
        if let LuaType::Variadic(variadic) = &func_params[i].1 {
            let mut arg_types = Vec::with_capacity(arg_exprs.len() - i);
            for tail_idx in i..arg_exprs.len() {
                let arg_type = infer_variadic_tail_arg_type(
                    db,
                    context.cache,
                    call_syntax_id,
                    arg_exprs,
                    tail_idx,
                )?;
                arg_types.push(arg_type);
            }
            variadic_tpl_pattern_match(context, variadic, &arg_types)?;
            break;
        }

        let arg_type = infer_generic_arg_type(db, context.cache, call_syntax_id, arg_exprs, i)?;
        match (func_param_type, &arg_type) {
            (_, LuaType::Variadic(variadic)) => {
                let func_param_types = func_params[i..]
                    .iter()
                    .map(|(_, t)| t)
                    .cloned()
                    .collect::<Vec<_>>();
                multi_param_tpl_pattern_match_multi_return(context, &func_param_types, variadic)?;
                break;
            }
            _ => {
                let effective_type = str_tpl_arg_type(
                    db,
                    context.cache,
                    func_param_type,
                    call_arg_expr,
                    call_expr,
                    i,
                    arg_type,
                );
                tpl_pattern_match(context, func_param_type, &effective_type)?;
            }
        }
    }

    if !context.substitutor.is_infer_all_tpl() {
        for (func_param_type, call_arg_expr) in unresolve_tpls {
            context.source_range = call_arg_expr.get_range();
            let closure_type = infer_expr(db, context.cache, call_arg_expr)?;

            tpl_pattern_match(context, &func_param_type, &closure_type)?;
        }
    }

    Ok(())
}

pub fn build_self_type(db: &DbIndex, self_type: &LuaType) -> LuaType {
    match self_type {
        LuaType::Def(id) | LuaType::Ref(id) => {
            if let Some(generic) = db.get_type_index().get_generic_params(id) {
                let mut params = Vec::new();
                for (i, generic_param) in generic.iter().enumerate() {
                    if let Some(t) = &generic_param.type_constraint {
                        params.push(t.clone());
                    } else {
                        params.push(LuaType::TplRef(Arc::new(GenericTpl::new(
                            GenericTplId::Type(i as u32),
                            crate::db_index::intern_smol_str(&generic_param.name),
                            None,
                        ))));
                    }
                }
                let generic = LuaGenericType::new(id.clone(), params);
                return LuaType::Generic(Arc::new(generic));
            }
        }
        _ => {}
    };
    self_type.clone()
}

pub fn infer_call_self_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
    func: &LuaFunctionType,
) -> Option<LuaType> {
    if !call_expr.is_colon_call()
        && (func.is_colon_define()
            || func
                .get_params()
                .first()
                .is_some_and(|(name, _)| name == "self"))
    {
        let first_arg = call_expr.get_args_list()?.get_args().next()?;
        let self_type = infer_alias_self_arg_type(db, cache, first_arg)?;
        return Some(build_self_type(db, &self_type));
    }
    infer_self_type(db, cache, call_expr)
}

pub fn infer_self_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    call_expr: &LuaCallExpr,
) -> Option<LuaType> {
    let prefix_expr = call_expr.get_prefix_expr()?;
    match prefix_expr {
        LuaExpr::IndexExpr(index) => {
            let self_expr = index.get_prefix_expr()?;
            let self_type = infer_expr(db, cache, self_expr).ok()?;
            let self_type = build_self_type(db, &self_type);
            return Some(self_type);
        }
        LuaExpr::NameExpr(name) => {
            let semantic_decl_id = infer_node_semantic_decl(
                db,
                cache,
                name.syntax().clone(),
                SemanticDeclLevel::default(),
            )?;
            if let LuaSemanticDeclId::Member(member_id) = semantic_decl_id {
                if member_takes_self(db, member_id)
                    && let Some(first_arg) = call_expr.get_args_list()?.get_args().next()
                    && let Some(arg_type) = infer_alias_self_arg_type(db, cache, first_arg)
                {
                    return Some(build_self_type(db, &arg_type));
                }
                let owner = db.get_member_index().get_current_owner(&member_id)?;
                if let LuaMemberOwner::Type(id) = owner {
                    let typ = LuaType::Ref(id.clone());
                    let self_type = build_self_type(db, &typ);
                    return Some(self_type);
                }
                return None;
            }
        }
        _ => {}
    }

    None
}

/// Whether the function `member_id` holds takes its receiver as its first argument: it is
/// colon-defined or names its first parameter `self`.
fn member_takes_self(db: &DbIndex, member_id: crate::LuaMemberId) -> bool {
    match db
        .get_type_index()
        .get_type_cache(&member_id.into())
        .map(|cache| cache.as_type().clone())
    {
        Some(LuaType::Signature(signature_id)) => db
            .get_signature_index()
            .get_declared(&signature_id)
            .is_some_and(|signature| {
                signature.is_colon_define
                    || signature.params.first().is_some_and(|name| name == "self")
            }),
        Some(LuaType::DocFunction(func)) => {
            func.is_colon_define()
                || func
                    .get_params()
                    .first()
                    .is_some_and(|(name, _)| name == "self")
        }
        _ => false,
    }
}

fn infer_alias_self_arg_type(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    arg_expr: LuaExpr,
) -> Option<LuaType> {
    if let LuaExpr::NameExpr(name_expr) = &arg_expr
        && name_expr.get_name_text()? == "self"
        && is_implicit_self_name(db, cache, name_expr)
        && let Some(self_type) = infer_enclosing_self_type(db, cache, name_expr)
    {
        return Some(self_type);
    }

    infer_expr(db, cache, arg_expr).ok()
}

fn is_implicit_self_name(db: &DbIndex, cache: &LuaInferCache, name_expr: &LuaNameExpr) -> bool {
    db.get_decl_index()
        .get_decl_tree(&cache.get_file_id())
        .and_then(|tree| tree.find_local_decl("self", name_expr.get_position()))
        .is_some_and(|decl| decl.is_implicit_self())
}

fn check_expr_can_later_infer(
    context: &mut TplContext,
    func_param_type: &LuaType,
    call_arg_expr: &LuaExpr,
) -> Result<bool, InferFailReason> {
    let doc_function = match func_param_type {
        LuaType::DocFunction(doc_func) => doc_func.clone(),
        LuaType::Signature(sig_id) => {
            let sig = context
                .db
                .get_signature_index()
                .get(sig_id)
                .ok_or(InferFailReason::None)?;

            sig.to_doc_func_type()
        }
        _ => return Ok(false),
    };

    if let LuaExpr::ClosureExpr(_) = call_arg_expr {
        return Ok(true);
    }

    let doc_params = doc_function.get_params();
    let variadic_count = doc_params
        .iter()
        .filter_map(|(_, t)| {
            if let Some(LuaType::Variadic(_)) = t {
                Some(())
            } else {
                None
            }
        })
        .count();

    Ok(variadic_count > 1)
}
