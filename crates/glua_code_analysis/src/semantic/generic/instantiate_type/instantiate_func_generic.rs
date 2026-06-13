use std::{collections::HashSet, ops::Deref, sync::Arc};

use glua_parser::{
    LuaAstNode, LuaCallExpr, LuaChunk, LuaDocTypeList, LuaExpr, LuaFuncStat, LuaNameExpr,
    LuaVarExpr,
};
use internment::ArcIntern;
use rowan::TextSize;
use smol_str::SmolStr;

use crate::db_index::GmodClassCallLiteral;
use crate::db_index::find_call_arg_role_from_type;
use crate::{
    DocTypeInferContext, FileId, GenericTpl, GenericTplId, LuaDocDefaultValue, LuaFunctionType,
    LuaGenericType, LuaSemanticDeclId, LuaSignatureId, TypeVisitTrait,
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

use super::TypeSubstitutor;

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
    let previous_realm = cache.flow_query_realm.replace(use_site_realm);

    let result = resolve_str_default_from_arg_inner(db, cache, param_type, arg_expr, decl_id);

    cache.flow_query_realm = previous_realm;
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
/// expression type and using `find_call_arg_role_from_type`.
fn check_vgui_panel_ref_role(
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
                        if role.domain == "gmod.vgui_panel" && role.role == "reference" {
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
        find_call_arg_role_from_type(
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
/// When a call argument annotated with `call_arg("gmod.vgui_panel", "reference")`
/// is a function-parameter whose enclosing colon-method belongs to a registered
/// VGUI panel table, resolve the argument to that panel's class name.
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
    if !check_vgui_panel_ref_role(db, cache, &prefix_expr, param_idx, call_expr.is_colon_call()) {
        return None;
    }

    // The argument must be a name expression referencing a function parameter.
    let name_expr = LuaNameExpr::cast(arg_expr.syntax().clone())?;
    let file_id = cache.get_file_id();
    let range = name_expr.get_range();
    let local_ref = db.get_reference_index().get_local_reference(&file_id)?;
    let arg_decl_id = local_ref.get_decl_id(&range)?;
    let decl = db.get_decl_index().get_decl(&arg_decl_id)?;
    let crate::LuaDeclExtra::Param { signature_id, .. } = &decl.extra else {
        return None;
    };

    // The enclosing function must be a colon-method (i.e. defined on a PANEL
    // table).  We don't use the returned self type directly — after
    // derma.DefineControl the PANEL local is still a plain table, not a
    // Ref(DCategoryList).  Instead, presence of an enclosing colon method is
    // the gate that distinguishes PANEL callbacks from arbitrary functions.
    infer_enclosing_self_type(db, cache, &name_expr)?;

    // Find the enclosing colon-method's receiver PANEL declaration.
    // This allows us to match the correct registration even when a file
    // has multiple `local PANEL = {}` blocks.
    let (receiver_decl_id, receiver_position, receiver_signature_id) =
        find_enclosing_panel_receiver_context(db, cache, &name_expr)?;

    if *signature_id != receiver_signature_id {
        return None;
    }

    // Look up the panel class name from the file's GmodClassMetadataIndex.
    // Match by declaration: the registration's table argument must resolve
    // to the same local declaration as the enclosing method's receiver.
    let gmod_metadata = db.get_gmod_class_metadata_index();
    let file_metadata = gmod_metadata.get_file_metadata(&file_id)?;

    for call in file_metadata
        .derma_define_control_calls
        .iter()
        .chain(file_metadata.vgui_register_calls.iter())
    {
        let Some(panel_name) = get_panel_name_from_call(call) else {
            continue;
        };
        let Some((table_decl_id, region_start, register_position)) =
            resolve_call_table_registration_region(db, file_id, call)
        else {
            continue;
        };
        if table_decl_id == receiver_decl_id
            && receiver_position >= region_start
            && receiver_position < register_position
            && gmod_metadata.get_vgui_panel_base(panel_name).is_some()
        {
            return Some(LuaType::StringConst(SmolStr::new(panel_name).into()));
        }
    }

    None
}

/// Walk up from `name_expr` to the enclosing colon-method and return the
/// `LuaDeclId` of its receiver variable (the `PANEL` in `PANEL:Method()`).
fn find_enclosing_panel_receiver_context(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    name_expr: &LuaNameExpr,
) -> Option<(crate::LuaDeclId, TextSize, LuaSignatureId)> {
    for func_stat in name_expr.ancestors::<LuaFuncStat>() {
        let Some(LuaVarExpr::IndexExpr(index_expr)) = func_stat.get_func_name() else {
            continue;
        };
        if !index_expr
            .get_index_token()
            .is_some_and(|token| token.is_colon())
        {
            continue;
        }
        let Some(LuaExpr::NameExpr(prefix_name)) = index_expr.get_prefix_expr() else {
            continue;
        };
        let file_id = cache.get_file_id();
        let range = prefix_name.get_range();
        let local_ref = db.get_reference_index().get_local_reference(&file_id)?;
        let decl_id = local_ref.get_decl_id(&range)?;
        let closure = func_stat.get_closure()?;
        let signature_id = LuaSignatureId::from_closure(file_id, &closure);
        return Some((decl_id, range.start(), signature_id));
    }
    None
}

/// Return the registered panel class name from a VGUI registration call's
/// literal arguments (the define / class name arg).
fn get_panel_name_from_call(call: &crate::db_index::GmodScriptedClassCallMetadata) -> Option<&str> {
    let define_idx = call.vgui_panel_define_arg_idx();
    match call.literal_args.get(define_idx)? {
        Some(GmodClassCallLiteral::String(s)) if !s.is_empty() => Some(s.as_str()),
        _ => None,
    }
}

/// Resolve the registration call's table argument to its local declaration and
/// active registration region.
///
/// Returns `(decl_id, region_start, register_position)` when the table
/// argument is a simple name reference; otherwise `None`.
fn resolve_call_table_registration_region(
    db: &DbIndex,
    file_id: crate::FileId,
    call: &crate::db_index::GmodScriptedClassCallMetadata,
) -> Option<(crate::LuaDeclId, TextSize, TextSize)> {
    let table_source = call.vgui_panel_roles.as_ref()?.table.as_ref()?;
    let arg = call.args.get(table_source.arg_idx)?;
    let range = arg.syntax_id.get_range();
    let register_position = call.syntax_id.get_range().start();
    let local_ref = db.get_reference_index().get_local_reference(&file_id)?;
    let decl_id = local_ref.get_decl_id(&range)?;
    let region_start =
        find_latest_decl_write_before_position(db, file_id, decl_id, register_position)
            .unwrap_or(decl_id.position);
    Some((decl_id, region_start, register_position))
}

fn find_latest_decl_write_before_position(
    db: &DbIndex,
    file_id: crate::FileId,
    decl_id: crate::LuaDeclId,
    position: TextSize,
) -> Option<TextSize> {
    db.get_reference_index()
        .get_decl_references(&file_id, &decl_id)
        .and_then(|decl_references| {
            decl_references
                .cells
                .iter()
                .filter(|cell| cell.is_write && cell.range.start() < position)
                .max_by_key(|cell| cell.range.start())
                .map(|cell| cell.range.start())
        })
}

pub fn instantiate_func_generic(
    db: &DbIndex,
    cache: &mut LuaInferCache,
    func: &LuaFunctionType,
    call_expr: LuaCallExpr,
) -> Result<LuaFunctionType, InferFailReason> {
    let file_id = cache.get_file_id().clone();
    let mut generic_tpls = HashSet::new();
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
    };
    if !generic_tpls.is_empty() {
        context.substitutor.add_need_infer_tpls(generic_tpls);

        if let Some(type_list) = call_expr.get_call_generic_type_list() {
            // 如果使用了`obj:abc--[[@<string>]]("abc")`强制指定了泛型, 那么我们只需要直接应用
            apply_call_generic_type_list(db, file_id, &mut context, &type_list);
        } else {
            // 如果没有指定泛型, 则需要从调用参数中推断
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

    if contain_self && let Some(self_type) = infer_self_type(db, cache, &call_expr) {
        substitutor.add_self_type(self_type);
    }

    if let LuaType::DocFunction(f) = instantiate_doc_function(db, func, &substitutor) {
        Ok(f.deref().clone())
    } else {
        Ok(func.clone())
    }
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

        if !func_param_type.is_variadic()
            && check_expr_can_later_infer(context, func_param_type, call_arg_expr)?
        {
            // 如果参数不能被后续推断, 那么我们先不处理
            unresolve_tpls.push((func_param_type.clone(), call_arg_expr.clone()));
            continue;
        }

        let arg_type = match infer_expr(db, context.cache, call_arg_expr.clone()) {
            Ok(t) => t,
            Err(InferFailReason::FieldNotFound) => LuaType::Nil, // 对于未找到的字段, 我们认为是 nil 以执行后续推断
            Err(e) => return Err(e),
        };
        match (func_param_type, &arg_type) {
            (LuaType::Variadic(variadic), _) => {
                let mut arg_types = vec![];
                for arg_expr in &arg_exprs[i..] {
                    let arg_type = infer_expr(db, context.cache, arg_expr.clone())?;
                    arg_types.push(arg_type);
                }
                variadic_tpl_pattern_match(context, variadic, &arg_types)?;
                break;
            }
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
                // Try to bind a StrTplRef from context when the arg is not a
                // literal string constant.  Two resolution paths exist:
                //
                // 1. VGUI panel reference: when the param has a StrTplRef and
                //    the call is annotated with call_arg("gmod.vgui_panel",
                //    "reference"), resolve from the enclosing PANEL method
                //    context.  This works for any arg type (String, Unknown,
                //    Any) because the resolution is purely contextual.
                //
                // 2. Inferred string default: when arg_type is plain `String`
                //    and the variable has a flow-valid `x = x or "literal"`
                //    default.
                //
                // Path 1 is tried first (it is more specific).  Path 2 only
                // applies to `String` args.
                let effective_type = resolve_vgui_panel_ref_from_arg(
                    db,
                    context.cache,
                    func_param_type,
                    call_arg_expr,
                    call_expr,
                    i,
                )
                .or_else(|| {
                    if matches!(arg_type, LuaType::String) {
                        resolve_str_default_from_arg(
                            db,
                            context.cache,
                            func_param_type,
                            call_arg_expr,
                        )
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| arg_type.clone());
                tpl_pattern_match(context, func_param_type, &effective_type)?;
            }
        }
    }

    if !context.substitutor.is_infer_all_tpl() {
        for (func_param_type, call_arg_expr) in unresolve_tpls {
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
                            ArcIntern::new(generic_param.name.clone()),
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
                if let Some(first_arg) = call_expr.get_args_list()?.get_args().next()
                    && let Some(arg_type) = infer_alias_self_arg_type(db, cache, first_arg)
                {
                    let self_type = build_self_type(db, &arg_type);
                    return Some(self_type);
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
